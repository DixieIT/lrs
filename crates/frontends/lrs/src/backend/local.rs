//! In-process composition root. Instantiates the runtime, registers the
//! chosen packages and translates runtime facts into TUI protocol structs.

use super::Backend;
use lrs::protocol::{
    CommandResult, CommandSpec, Health, ModelList, ModelSet, Provider, Registry, RegistrySet,
    RegistryEntry, RunRequest, RunResponse, SessionMessages, SessionRow, WidgetRow,
};
use lrs::protocol::Event;
use anyhow::{anyhow, Context, Result};
use runtime::{Runtime, RuntimeBuilder};
use sdk::{ConfigStore, SecretVault, SessionStore};
use super::local_fs::LocalFs;
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};

struct FileStore {
    path: std::path::PathBuf,
    values: RwLock<HashMap<String, String>>,
}

impl FileStore {
    fn load(path: std::path::PathBuf) -> Self {
        let values = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        Self { path, values: RwLock::new(values) }
    }
    fn read_value(&self, key: &str) -> sdk::Result<Option<String>> {
        Ok(self.values.read().unwrap().get(key).cloned())
    }
    fn write_value(&self, key: &str, value: &str) -> sdk::Result<()> {
        self.values.write().unwrap().insert(key.into(), value.into());
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| sdk::Error::Unavailable(e.to_string()))?;
        }
        std::fs::write(&self.path, toml::to_string(&*self.values.read().unwrap()).map_err(|e| sdk::Error::Internal(e.to_string()))?)
            .map_err(|e| sdk::Error::Unavailable(e.to_string()))?;
        Ok(())
    }
}

impl SecretVault for FileStore {
    fn get(&self, key: &str) -> sdk::Result<Option<String>> { self.read_value(key) }
    fn set(&self, key: &str, value: &str) -> sdk::Result<()> { self.write_value(key, value) }
}

impl ConfigStore for FileStore {
    fn get(&self, key: &str) -> sdk::Result<Option<String>> { self.read_value(key) }
    fn set(&self, key: &str, value: &str) -> sdk::Result<()> { self.write_value(key, value) }
}

pub struct LocalBackend {
    runtime: Runtime,
    sessions: Arc<dyn SessionStore>,
    store: Arc<FileStore>,
    controllers: RwLock<HashMap<String, runtime::TurnController>>,
    /// The project directory as the TUI names it, so commands see the same
    /// working directory a turn does.
    project: String,
}

/// Config key holding the default executor (`native` or `ptc`).
const EXECUTOR_KEY: &str = "runtime.executor";

impl LocalBackend {
    /// `project` is the only directory tools may touch. `executor` overrides
    /// the configured one; with neither, turns use code mode (`ptc`).
    pub fn new(home: &Path, project: &Path, executor: Option<&str>) -> Result<Self> {
        let store = Arc::new(FileStore::load(home.join("lrs-store.toml")));
        let executor = match executor {
            Some(key) => key.to_string(),
            None => store.read_value(EXECUTOR_KEY)?.unwrap_or_else(|| sdk::PTC_EXECUTOR.into()),
        };
        if executor != sdk::NATIVE_EXECUTOR && executor != sdk::PTC_EXECUTOR {
            anyhow::bail!("unknown executor {executor:?}: use {} or {}", sdk::NATIVE_EXECUTOR, sdk::PTC_EXECUTOR);
        }
        let fs = LocalFs::new(project)
            .with_context(|| format!("opening project directory {}", project.display()))?;
        let vault: Arc<dyn SecretVault> = store.clone();
        let config: Arc<dyn ConfigStore> = store.clone();
        // Sessions outlive the process: one SQLite database under the home.
        let db = home.join("lrs").join("sessions.db");
        let durable = Arc::new(session::SqliteSessionStore::open(&db)
            .map_err(|e| anyhow!("opening the session store {}: {e}", db.display()))?);
        let sessions: Arc<dyn SessionStore> = durable.clone();
        let runtime = RuntimeBuilder::new()
            .with_sessions(sessions.clone())
            .with_plugin(Box::new(session::SessionRecallPlugin::new(durable)))
            .with_secret_vault(vault)
            .with_config_store(config)
            .with_fs(Arc::new(fs))
            .with_plugin(Box::new(gateway::GatewayPlugin))
            .with_plugin(Box::new(runner_react::ReactLoopPlugin::new(
                runner_react::ReactLoopAgent::new(sdk::REACT_RUNNER, 0),
            )))
            .with_plugin(Box::new(tool_permission::PermissionPlugin::default()))
            .with_plugin(Box::new(tool_executor_native::NativeExecutorPlugin::default()))
            .with_plugin(Box::new(tool_executor_ptc::PtcExecutorPlugin::default()))
            .with_plugin(Box::new(tools::ToolsPlugin))
            .with_plugin(Box::new(tool_shell::ToolShellPlugin::default()))
            .with_plugin(Box::new(system_prompt::SystemPromptPlugin))
            .with_plugin(Box::new(agent_tool_presentation::AgentToolPresentationPlugin::default()))
            .with_executor(executor)
            .build()
            .context("building in-process runtime")?;
        Ok(Self {
            runtime, sessions, store,
            controllers: RwLock::new(HashMap::new()),
            project: project.display().to_string(),
        })
    }

    #[cfg(test)]
    fn from_runtime(runtime: Runtime, sessions: Arc<dyn SessionStore>, store: Arc<FileStore>) -> Self {
        Self { runtime, sessions, store, controllers: RwLock::new(HashMap::new()), project: ".".into() }
    }

    fn gateway(&self) -> Result<Arc<dyn sdk::ModelGateway>> {
        self.runtime.registry.active_gateway()
            .ok_or_else(|| anyhow!("no ModelGateway registered"))
    }
}

impl Backend for LocalBackend {
    fn base(&self) -> &str { "local" }
    fn health(&self) -> Option<Health> {
        let gw = self.gateway().ok()?;
        Some(Health {
            model: gw.default_model().unwrap_or_default(),
            context_limit: 0,
            tools: self.runtime.registry.tools().iter().map(|t| t.name().into()).collect(),
            plugins: self.runtime.plugin_manifests().iter().map(|m| m.name.clone()).collect(),
            extensions: vec![],
            skills: vec![],
            executor: self.runtime.jobs.executor().map(|(key, _)| key).unwrap_or_default(),
        })
    }

    fn start_run(&self, req: &RunRequest) -> Result<RunResponse> {
        // A first prompt carries no session: create one, and hand its id back
        // so the next prompt continues the same conversation.
        let session_id = match req.session_id.as_deref().filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => self.sessions.create(&req.cwd).map_err(|e| anyhow!(e.to_string()))?.id,
        };
        let task = req.task.as_str()
            .ok_or_else(|| anyhow!("the local runtime accepts text prompts only"))?;
        let id = self.runtime.jobs.start_run(
            &session_id,
            &req.cwd,
            task,
            req.model.as_deref(),
            req.effort.as_deref(),
        )?;
        // A fast job may already be over; then there is nothing left to steer.
        if let Some(controller) = self.runtime.jobs.control(&id) {
            self.controllers.write().unwrap().insert(id.clone(), controller);
        }
        Ok(RunResponse { session_id, job_id: id })
    }

    fn models(&self) -> Result<ModelList> {
        let gw = self.gateway()?;
        // An empty catalog before `/connect` is expected, not a startup error.
        let models = match gw.models() {
            Ok(list) => list.into_iter().map(|m| m.id).collect(),
            Err(sdk::Error::AuthRequired { .. }) | Err(sdk::Error::Unavailable(_)) => vec![],
            Err(e) => return Err(anyhow!(e.to_string())),
        };
        Ok(ModelList {
            current: gw.default_model().unwrap_or_default(),
            models,
        })
    }

    fn providers(&self) -> Result<Vec<Provider>> {
        Ok(self.gateway()?.providers().map_err(|e| anyhow!(e.to_string()))?
            .into_iter().map(|p| Provider {
                name: p.name,
                api_key_env: format!("{}_API_KEY", p.id.to_uppercase()),
                base_url: p.base_url,
                connected: p.configured,
                models: vec![],
            }).collect())
    }

    fn set_model(&self, ref_: &str) -> Result<ModelSet> {
        self.store.write_value("gateway.default_model", ref_)?;
        Ok(ModelSet { model: ref_.into(), missing_key_env: None, context_limit: 0 })
    }

    fn connect(&self, provider: &str, key: &str) -> Result<()> {
        self.gateway()?.connect_shared(provider, key).map_err(|e| anyhow!(e.to_string()))
    }

    fn widgets(&self, session: &str) -> Result<Vec<WidgetRow>> {
        Ok(self.runtime.widgets.for_session(session).into_iter().map(|w| WidgetRow {
            id: w.id,
            markup: w.body,
        }).collect())
    }

    /// The TUI looks commands up by their slash form (`/model`), so every
    /// name is published with the slash.
    fn commands(&self) -> Result<Vec<CommandSpec>> {
        Ok(self.runtime.commands.specs().into_iter().map(|c| CommandSpec {
            name: format!("/{}", c.name),
            desc: c.description,
            sub: c.subcommands.into_iter().map(|s| (s.clone(), s)).collect(),
        }).collect())
    }

    fn run_command(&self, name: &str, arg: &str, session: &str) -> Result<CommandResult> {
        let r = self.runtime.commands.run_command(name.trim_start_matches('/'), arg, session, &self.project)
            .map_err(|e| anyhow!(e.to_string()))?;
        Ok(CommandResult {
            output: r.text.lines().map(str::to_string).collect(),
            unsupported: vec![], error: None, session_id: r.session_id.unwrap_or_default(), widgets: vec![],
        })
    }

    fn registry(&self, _cwd: &str) -> Result<Registry> {
        Ok(Registry {
            plugins: self.runtime.plugin_manifests().into_iter().map(|m| RegistryEntry {
                name: m.name, path: String::new(), state: "active".into(), live: true,
            }).collect(),
            extensions: vec![],
            error: None,
        })
    }

    fn set_registry(&self, _kind: &str, _name: &str, _state: &str, _force: bool, _cwd: &str) -> Result<RegistrySet> {
        Ok(RegistrySet::default())
    }

    fn reload(&self, cwd: &str) -> Result<Registry> { self.registry(cwd) }

    fn sessions(&self, dir: &str) -> Result<Vec<SessionRow>> {
        Ok(self.sessions.list(Some(dir)).map_err(|e| anyhow!(e.to_string()))?
            .into_iter()
            .map(|r| SessionRow { id: r.id, title: r.title.unwrap_or_default(), message_count: r.message_count })
            .collect())
    }

    fn session(&self, id: &str) -> Result<SessionMessages> {
        let messages = self.sessions.history(id).map_err(|e| anyhow!(e.to_string()))?;
        Ok(SessionMessages {
            session_id: id.into(),
            messages: messages.into_iter().map(|m| lrs::protocol::Message {
                role: match m.role {
                    protocol::Role::User => "user",
                    protocol::Role::Assistant => "assistant",
                    protocol::Role::System => "system",
                    protocol::Role::Tool => "tool",
                }.into(),
                content: serde_json::json!(m.content),
                // Resumed tool calls are redrawn as tool blocks, results included.
                tool_calls: m.tool_calls.into_iter().map(|c| lrs::protocol::ToolCall {
                    function: lrs::protocol::ToolFunction { name: c.name, arguments: c.arguments },
                }).collect(),
            }).collect(),
        })
    }

    fn cancel(&self, job_id: &str) -> Result<()> {
        if let Some(c) = self.controllers.read().unwrap().get(job_id) { c.cancel(); }
        Ok(())
    }

    fn steer(&self, job_id: &str, text: &str) -> Result<()> {
        if let Some(c) = self.controllers.read().unwrap().get(job_id) { c.steer(text.into()); }
        Ok(())
    }

    fn stream_events(&self, job_id: &str, tx: Sender<Event>) -> Result<()> {
        let bus = self.runtime.jobs.job_events(job_id)
            .ok_or_else(|| anyhow!("unknown job {job_id}"))?;
        let (raw_tx, raw_rx) = std::sync::mpsc::channel();
        // RAII: unsubscribed when this function returns. Earlier events of
        // this job are replayed into the channel before live ones.
        // `raw_rx` lives until this function returns and `subscription` is
        // dropped before that, so the send cannot fail while subscribed.
        let subscription = bus.subscribe(move |event| {
            raw_tx.send(event).expect("job stream receiver outlives its subscription");
        });
        let mut tools = ToolNames::default();
        for event in raw_rx.iter() {
            let terminal = matches!(
                event,
                protocol::Event::JobFinished { .. } | protocol::Event::JobFailed { .. }
            );
            if let Some(wire) = tools.wire_event(event) {
                if tx.send(wire).is_err() {
                    break; // the UI stopped listening; the job keeps running
                }
            }
            if terminal {
                break;
            }
        }
        drop(subscription);
        self.controllers.write().unwrap().remove(job_id);
        self.runtime.jobs.forget(job_id);
        Ok(())
    }
}

/// Remembers each tool call's name and arguments from its start event, so the
/// end event, which carries only the call id, can be shown under its name.
#[derive(Default)]
struct ToolNames {
    calls: HashMap<String, (String, serde_json::Value)>,
}

impl ToolNames {
    /// Map a runtime event onto the frontend's wire event. Events the TUI has
    /// no view for are skipped.
    fn wire_event(&mut self, event: protocol::Event) -> Option<Event> {
        match event {
            protocol::Event::TextDelta { text } => Some(Event::Token { text, run_id: None }),
            protocol::Event::Thinking { text } => Some(Event::Thinking { text, run_id: None }),
            // What the next request will carry: the prompt just sent and the reply.
            protocol::Event::Usage { input_tokens, output_tokens, context_limit } => Some(Event::Usage {
                context: input_tokens + output_tokens,
                turn_input: input_tokens,
                turn_output: output_tokens,
                context_limit,
            }),
            protocol::Event::ToolCallStart { id, name, args } => {
                self.calls.insert(id, (name.clone(), args.clone()));
                Some(Event::ToolStarted { name, args, run_id: None })
            }
            protocol::Event::ToolCallEnd { id, output, failed } => {
                let (name, args) = self.calls.remove(&id)
                    .unwrap_or_else(|| (id, serde_json::Value::Null));
                Some(Event::ToolFinished { name, args, output, failed, run_id: None })
            }
            protocol::Event::JobFinished { .. } => Some(Event::JobFinished { leftover_steers: vec![] }),
            protocol::Event::JobFailed { error, .. } => Some(Event::JobFailed { error }),
            protocol::Event::Steered { text } => Some(Event::SteerConsumed { text }),
            protocol::Event::WidgetUpdated { widget } =>
                Some(Event::Widget { id: widget.id, markup: Some(widget.body) }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::{Chunk, Contribution, Ctx, FunctionPlugin, ModelGateway, ModelInfo, ProviderInfo, Stream};

    struct TestGateway;
    impl ModelGateway for TestGateway {
        fn id(&self) -> &str { "gateway" }
        fn providers(&self) -> sdk::Result<Vec<ProviderInfo>> { Ok(vec![]) }
        fn models(&self) -> sdk::Result<Vec<ModelInfo>> {
            Ok(vec![ModelInfo { id: "test/model".into(), provider: "test".into(), context_limit: 0 }])
        }
        fn connect(&mut self, _: &str, _: &str) -> sdk::Result<()> { Ok(()) }
        fn default_model(&self) -> Option<String> { Some("test/model".into()) }
        fn stream(&self, _: &str, _: &[protocol::SessionMessage], _: &[serde_json::Value]) -> sdk::Result<Stream> {
            Ok(Box::new(vec![
                Ok(Chunk::TextDelta("hello".into())),
                Ok(Chunk::Done),
            ].into_iter()))
        }
    }
    /// Apply a package the way `RuntimeBuilder` does: declare, then apply.
    fn apply(registry: &Arc<runtime::Registry>, plugin: &dyn FunctionPlugin) {
        registry.declare(sdk::ResolverManifest {
            id: plugin.name().into(), name: plugin.name().into(), version: "0".into(),
            form: sdk::PluginForm::Function,
            registrations: plugin.registrations(),
            config_schema: None,
        });
        let mut ctx = runtime::LoaderCtx::new(plugin.name(), registry.clone());
        std::mem::forget(plugin.apply(&sdk::Config::default(), &mut ctx).unwrap());
    }

    /// A scratch home and project for a full composition.
    fn dirs(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("lrs-local-{}-{name}", std::process::id()));
        // Windows reuses process ids, so a directory left by an earlier run can
        // carry its store — a saved executor made the default-executor test fail.
        if base.exists() {
            std::fs::remove_dir_all(&base).unwrap();
        }
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        (base.join("home"), project)
    }

    #[test]
    fn local_stream_delivers_answer_and_finishes() {
        let registry = Arc::new(runtime::Registry::default());
        registry.declare(sdk::ResolverManifest {
            id: "test".into(), name: "test".into(), version: "0".into(),
            form: sdk::PluginForm::Function,
            registrations: vec![
                sdk::RegistrationDecl::Gateway { name: "gateway".into() },
                sdk::RegistrationDecl::Runner { name: sdk::REACT_RUNNER.into() },
            ],
            config_schema: None,
        });
        let mut ctx = runtime::LoaderCtx::new("test", registry.clone());
        let mut d = sdk::Disposer::default();
        d.push(ctx.effect(Contribution::Gateway { gateway: Box::new(TestGateway) }).unwrap());
        d.push(ctx.effect(Contribution::Runner {
            runner: Box::new(runner_react::ReactLoopAgent::new(sdk::REACT_RUNNER, 0)),
        }).unwrap());
        std::mem::forget(d);
        // The real executor and permission packages.
        apply(&registry, &tool_executor_native::NativeExecutorPlugin::default());
        apply(&registry, &tool_permission::PermissionPlugin::default());
        apply(&registry, &agent_tool_presentation::AgentToolPresentationPlugin::default());

        let sessions: Arc<dyn SessionStore> = Arc::new(session::InMemorySessionStore::default());
        let jobs = Arc::new(runtime::JobManager::new(registry.clone(), sessions.clone()));
        let commands = Arc::new(runtime::CommandBus::new(registry.clone()));
        let runtime = runtime::Runtime::from_parts(
            registry.clone(), sessions.clone(), jobs, commands,
            Arc::new(runtime::WidgetBus::default()),
        );
        let backend = LocalBackend::from_runtime(
            runtime, sessions, Arc::new(FileStore::load(std::path::PathBuf::from("unused.toml"))),
        );
        // First prompt: no session yet, the backend creates one.
        let mut req = RunRequest {
            task: serde_json::json!("ciao"), cwd: ".".into(),
            session_id: None, model: Some("test/model".into()),
            effort: None, pane_id: None,
        };
        let first = backend.start_run(&req).unwrap();
        assert!(!first.session_id.is_empty());
        let (tx, rx) = std::sync::mpsc::channel();
        backend.stream_events(&first.job_id, tx).unwrap();
        let events: Vec<Event> = rx.try_iter().collect();
        assert!(events.iter().any(|e| matches!(e, Event::Token { text, .. } if text == "hello")));
        assert!(matches!(events.last(), Some(Event::JobFinished { .. })));
        assert!(backend.runtime.jobs.control(&first.job_id).is_none());

        // Second prompt in the same session: only its own events, and the
        // conversation now holds both turns.
        req.session_id = Some(first.session_id.clone());
        let second = backend.start_run(&req).unwrap();
        assert_eq!(second.session_id, first.session_id);
        let (tx, rx) = std::sync::mpsc::channel();
        backend.stream_events(&second.job_id, tx).unwrap();
        let events: Vec<Event> = rx.try_iter().collect();
        let tokens = events.iter().filter(|e| matches!(e, Event::Token { .. })).count();
        let finished = events.iter().filter(|e| matches!(e, Event::JobFinished { .. })).count();
        assert_eq!((tokens, finished), (1, 1));
        let history = backend.session(&first.session_id).unwrap().messages;
        assert_eq!(history.len(), 4, "two prompts and two answers");
    }

    #[test]
    fn a_prompt_for_an_unknown_session_fails_instead_of_vanishing() {
        let (home, project) = dirs("unknown");
        let backend = LocalBackend::new(&home, &project, None).unwrap();
        let req = RunRequest {
            task: serde_json::json!("ciao"), cwd: ".".into(),
            session_id: Some("never-created".into()), model: None,
            effort: None, pane_id: None,
        };
        assert!(backend.start_run(&req).is_err());
    }

    #[test]
    fn the_executor_comes_from_the_flag_then_the_config_then_code_mode() {
        let (home, project) = dirs("executor");
        let default = LocalBackend::new(&home, &project, None).unwrap();
        assert_eq!(default.runtime.jobs.executor().unwrap().0, sdk::PTC_EXECUTOR);
        let native = LocalBackend::new(&home, &project, Some("native")).unwrap();
        assert_eq!(native.runtime.jobs.executor().unwrap().0, sdk::NATIVE_EXECUTOR);
        native.store.write_value(EXECUTOR_KEY, "native").unwrap();
        let configured = LocalBackend::new(&home, &project, None).unwrap();
        assert_eq!(configured.runtime.jobs.executor().unwrap().0, sdk::NATIVE_EXECUTOR);
        assert!(LocalBackend::new(&home, &project, Some("v8")).is_err());
    }

    #[test]
    fn commands_carry_the_slash_and_context_reports_what_the_model_gets() {
        let (home, project) = dirs("context");
        let backend = LocalBackend::new(&home, &project, Some("ptc")).unwrap();
        let names: Vec<String> = backend.commands().unwrap().into_iter().map(|c| c.name).collect();
        assert!(names.contains(&"/context".to_string()) && names.contains(&"/model".to_string()), "{names:?}");
        let report = backend.run_command("/context", "", "").unwrap().output;
        assert!(report.contains(&"executor   ptc".to_string()), "{report:?}");
        assert!(report.contains(&"── system prompt".to_string()) && report.contains(&"You are Lele".to_string()), "{report:?}");
        let here = format!("Working directory: {}", project.display());
        assert!(report.contains(&here), "commands see the project directory a turn does: {report:?}");
        assert!(report.contains(&"▸ run_js".to_string()), "{report:?}");
        assert!(report.contains(&"  declare const tools: {".to_string()), "{report:?}");
        assert!(report.iter().any(|l| l == "    read_file(args: {"), "{report:?}");
        // Tools appear sorted by name, so the offer is the same in every process.
        assert!(!report.iter().any(|l| l.contains("list_dir")), "bash covers listing: {report:?}");
        let order: Vec<&str> = ["bash(", "edit_file(", "read_file(", "write_file("].iter()
            .map(|n| report.iter().find(|l| l.trim_start().starts_with(n)).map(String::as_str).unwrap_or(""))
            .collect();
        let positions: Vec<usize> = order.iter()
            .map(|l| report.iter().position(|r| r == l).unwrap_or(usize::MAX)).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{positions:?}");
    }

    #[test]
    fn usage_becomes_the_context_in_use_and_carries_the_window() {
        let mut names = ToolNames::default();
        let event = names.wire_event(protocol::Event::Usage {
            input_tokens: 8_000, output_tokens: 500, context_limit: 32_768,
        });
        match event {
            Some(Event::Usage { context, turn_input, turn_output, context_limit }) => {
                assert_eq!(context, 8_500, "the next request carries the prompt and the reply");
                assert_eq!((turn_input, turn_output, context_limit), (8_000, 500, 32_768));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    /// The two real packages together: a code-mode program calls the shell.
    /// Lives here because only the composition root may depend on both.
    #[test]
    fn a_code_mode_program_runs_shell_commands_through_tools_bash() {
        use sdk::{Tool, ToolExecutor};
        let (_, project) = dirs("ptc-bash");
        std::fs::write(project.join("a.txt"), "x").unwrap();
        let shell = tool_shell::Shell::detect();
        let list = if shell == tool_shell::Shell::PowerShell { "Get-ChildItem -Name" } else { "ls" };
        let bash = tool_shell::Bash::new(shell);
        let tools: [&dyn Tool; 1] = [&bash];
        let ctx = sdk::ToolContext {
            session_id: "s".into(),
            cwd: project.display().to_string(),
            cancellation_token: Arc::new(|| false),
            fs: Arc::new(LocalFs::new(&project).unwrap()),
        };
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let permission = move |name: &str, _: &serde_json::Value| -> sdk::Result<()> {
            log.lock().unwrap().push(name.to_string());
            Ok(())
        };
        let code = format!(
            "const listing = tools.bash({{ command: {list:?} }}); \
             let failed = ''; try {{ tools.bash({{ command: 'exit 4' }}); }} catch (e) {{ failed = e.message; }} \
             return {{ hasFile: listing.includes('a.txt'), failed }};"
        );
        let out = tool_executor_ptc::ToolExecutorPtc::default()
            .dispatch("run_js", serde_json::json!({ "code": code }), &tools, &ctx, &permission)
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        let value: serde_json::Value = serde_json::from_str(&out.content).unwrap();
        assert_eq!(value["hasFile"], true, "{value}");
        assert_eq!(value["failed"], "(no output)\n[exit code 4]");
        assert_eq!(*seen.lock().unwrap(), ["bash", "bash"], "every command went through the permission");
    }

    /// Every request the model gets, recorded; answers from a script: the
    /// first call asks for `read_file`, every later call answers in text.
    struct Scripted(Arc<std::sync::Mutex<Vec<Vec<protocol::SessionMessage>>>>);
    impl ModelGateway for Scripted {
        fn id(&self) -> &str { "gateway" }
        fn providers(&self) -> sdk::Result<Vec<ProviderInfo>> { Ok(vec![]) }
        fn models(&self) -> sdk::Result<Vec<ModelInfo>> { Ok(vec![]) }
        fn connect(&mut self, _: &str, _: &str) -> sdk::Result<()> { Ok(()) }
        fn default_model(&self) -> Option<String> { Some("test/model".into()) }
        fn stream(&self, _: &str, messages: &[protocol::SessionMessage], _: &[serde_json::Value]) -> sdk::Result<Stream> {
            let mut calls = self.0.lock().unwrap();
            calls.push(messages.to_vec());
            let chunks = if calls.len() == 1 {
                vec![
                    Ok(Chunk::ToolCallChunk {
                        id: "call_1".into(), name: Some("read_file".into()),
                        arguments_delta: r#"{"path":"a.txt"}"#.into(),
                    }),
                    Ok(Chunk::Done),
                ]
            } else {
                vec![Ok(Chunk::TextDelta(format!("answer {}", calls.len()))), Ok(Chunk::Done)]
            };
            Ok(Box::new(chunks.into_iter()))
        }
    }

    /// What the provider receives of a message: the stored id only matters for
    /// a tool result, where it names the call.
    fn wire(m: &protocol::SessionMessage) -> (protocol::Role, String, Vec<protocol::ToolCall>, String) {
        let id = if m.role == protocol::Role::Tool { m.id.clone() } else { String::new() };
        (m.role, m.content.clone(), m.tool_calls.clone(), id)
    }

    #[test]
    fn the_next_turn_carries_the_previous_turns_tool_calls_and_results() {
        let (_, project) = dirs("trajectory");
        std::fs::write(project.join("a.txt"), "alpha").unwrap();
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));

        let registry = Arc::new(runtime::Registry::default());
        registry.declare(sdk::ResolverManifest {
            id: "test".into(), name: "test".into(), version: "0".into(),
            form: sdk::PluginForm::Function,
            registrations: vec![
                sdk::RegistrationDecl::Gateway { name: "gateway".into() },
                sdk::RegistrationDecl::Runner { name: sdk::REACT_RUNNER.into() },
            ],
            config_schema: None,
        });
        let mut ctx = runtime::LoaderCtx::new("test", registry.clone());
        let mut d = sdk::Disposer::default();
        d.push(ctx.effect(Contribution::Gateway { gateway: Box::new(Scripted(requests.clone())) }).unwrap());
        d.push(ctx.effect(Contribution::Runner {
            runner: Box::new(runner_react::ReactLoopAgent::new(sdk::REACT_RUNNER, 0)),
        }).unwrap());
        std::mem::forget(d);
        apply(&registry, &tool_executor_native::NativeExecutorPlugin::default());
        apply(&registry, &tool_permission::PermissionPlugin::default());
        apply(&registry, &agent_tool_presentation::AgentToolPresentationPlugin::default());
        apply(&registry, &tools::ToolsPlugin);

        let sessions: Arc<dyn SessionStore> = Arc::new(session::InMemorySessionStore::default());
        let jobs = Arc::new(runtime::JobManager::with_fs(
            registry.clone(), sessions.clone(), Arc::new(LocalFs::new(&project).unwrap()),
        ));
        // As `RuntimeBuilder::build` does: `/context` is the runtime's own command.
        let commands = Arc::new(runtime::CommandBus::new(registry.clone())
            .with_builtin(Arc::new(runtime::ContextCommand::new(jobs.clone()))));
        let runtime = runtime::Runtime::from_parts(
            registry.clone(), sessions.clone(), jobs, commands, Arc::new(runtime::WidgetBus::default()),
        );
        let backend = LocalBackend::from_runtime(
            runtime, sessions, Arc::new(FileStore::load(std::path::PathBuf::from("unused.toml"))),
        );
        let turn = |session: Option<String>, text: &str| -> String {
            let req = RunRequest {
                task: serde_json::json!(text), cwd: project.display().to_string(),
                session_id: session, model: Some("test/model".into()), effort: None, pane_id: None,
            };
            let resp = backend.start_run(&req).unwrap();
            let (tx, rx) = std::sync::mpsc::channel();
            backend.stream_events(&resp.job_id, tx).unwrap();
            let events: Vec<Event> = rx.try_iter().collect();
            assert!(matches!(events.last(), Some(Event::JobFinished { .. })), "{events:?}");
            resp.session_id
        };

        let session = turn(None, "read a.txt");
        turn(Some(session.clone()), "what did you run?");

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3, "turn 1: tool call, then answer; turn 2: answer");
        let (end_of_turn_one, turn_two) = (&requests[1], &requests[2]);

        // The second turn sees the whole first turn, tool request and result included.
        use protocol::Role::{Assistant, Tool, User};
        let shape: Vec<(protocol::Role, &str)> = turn_two.iter().map(|m| (m.role, m.content.as_str())).collect();
        assert_eq!(shape, [
            (User, "read a.txt"),
            (Assistant, ""),
            (Tool, "alpha"),
            (Assistant, "answer 2"),
            (User, "what did you run?"),
        ]);
        assert_eq!(turn_two[1].tool_calls, [protocol::ToolCall {
            id: "call_1".into(), name: "read_file".into(), arguments: r#"{"path":"a.txt"}"#.into(),
        }]);
        assert_eq!(turn_two[2].id, "call_1", "the result still names its call");

        // Prompt cache: turn two's request starts with exactly what the last
        // request of turn one sent.
        let prefix: Vec<_> = turn_two[..end_of_turn_one.len()].iter().map(wire).collect();
        let before: Vec<_> = end_of_turn_one.iter().map(wire).collect();
        assert_eq!(prefix, before);

        // And /context shows the same trajectory.
        let report = backend.run_command("/context", "", &session).unwrap().output;
        assert!(report.iter().any(|l| l == "▸ assistant · calls read_file"), "{report:?}");
        assert!(report.iter().any(|l| l == "▸ tool · answers call_1"), "{report:?}");
    }

    #[test]
    fn sessions_survive_a_restart_and_recall_finds_them() {
        let (home, project) = dirs("persist");
        let db = home.join("lrs").join("sessions.db");
        if db.exists() {
            std::fs::remove_file(&db).unwrap();
        }
        let cwd = project.display().to_string();
        let session_id = {
            let backend = LocalBackend::new(&home, &project, Some("native")).unwrap();
            let req = RunRequest {
                task: serde_json::json!("come configuro il parser?"), cwd: cwd.clone(),
                session_id: None, model: Some("openrouter/none".into()), effort: None, pane_id: None,
            };
            // No provider is configured, so the turn itself fails; the prompt
            // is stored before the model is asked.
            backend.start_run(&req).unwrap().session_id
        };
        let reopened = LocalBackend::new(&home, &project, Some("native")).unwrap();
        let rows = reopened.sessions(&cwd).unwrap();
        assert_eq!(rows[0].id, session_id, "{rows:?}");
        assert_eq!(rows[0].title, "come configuro il parser?");
        assert!(rows[0].message_count >= 1);
        let resumed = reopened.session(&session_id).unwrap();
        assert_eq!(resumed.messages[0].text(), "come configuro il parser?");

        let names: Vec<String> = reopened.commands().unwrap().into_iter().map(|c| c.name).collect();
        assert!(names.contains(&"/recall".to_string()), "{names:?}");
        let found = reopened.run_command("/recall", "parser", "").unwrap().output;
        assert!(found.iter().any(|l| l.contains(&session_id)), "{found:?}");
    }
}
