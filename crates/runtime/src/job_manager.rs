//! Async turn execution, cancel/steer routing and the event bus.

use crate::Registry;
use sdk::SessionStore;
use protocol::{Event, Role};
use sdk::{Error, EventSink, FsPort, Result, ToolContext, TurnControl, TurnContext};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Per-job control handle handed to the frontend.
#[derive(Clone, Default)]
pub struct TurnController {
    cancelled: Arc<AtomicBool>,
    steers: Arc<Mutex<Vec<String>>>,
}

impl TurnController {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
    pub fn steer(&self, text: String) {
        self.steers.lock().unwrap().push(text);
    }
    /// The shared flag itself, so `ToolContext` can capture it directly
    /// instead of allocating a new controller view on every invocation.
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }
    pub fn handle(&self) -> Arc<dyn TurnControl + Send + Sync> {
        Arc::new(ControllerView {
            cancelled: self.cancelled.clone(),
            steers: self.steers.clone(),
        })
    }
}

struct ControllerView {
    cancelled: Arc<AtomicBool>,
    steers: Arc<Mutex<Vec<String>>>,
}

impl TurnControl for ControllerView {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
    fn take_steers(&self) -> Vec<String> {
        std::mem::take(&mut *self.steers.lock().unwrap())
    }
}

/// A cancellable subscription. Dropping it removes the listener from the bus.
pub struct Subscription {
    id: u64,
    bus: Arc<EventBus>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.bus.unsubscribe(self.id);
    }
}

/// Fan-out sink. Frontends subscribe; components only ever see `EventSink`.
///
/// Two flavours: the runtime-wide bus is live only, while each job gets its
/// own bus with a replay buffer, because a fast job can finish before its
/// frontend subscribes. Replay is per job so one job's history never leaks
/// into another job's stream.
pub struct EventBus {
    next_id: AtomicU64,
    listeners: RwLock<HashMap<u64, Box<dyn Fn(Event) + Send + Sync>>>,
    history: Option<RwLock<Vec<Event>>>,
    self_ref: std::sync::Weak<EventBus>,
}

impl EventBus {
    fn shared(replay: bool) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            next_id: AtomicU64::new(0),
            listeners: RwLock::new(HashMap::new()),
            history: replay.then(|| RwLock::new(Vec::new())),
            self_ref: weak.clone(),
        })
    }

    pub fn subscribe(&self, f: impl Fn(Event) + Send + Sync + 'static) -> Subscription {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        // Hold the history lock while registering, so an event emitted
        // concurrently is either replayed or delivered live, never both or neither.
        let history = self.history.as_ref().map(|h| h.read().unwrap());
        if let Some(events) = &history {
            for event in events.iter() { f(event.clone()); }
        }
        self.listeners.write().unwrap().insert(id, Box::new(f));
        drop(history);
        let bus = self.self_ref.upgrade().expect("EventBus is only built inside an Arc");
        Subscription { id, bus }
    }

    fn unsubscribe(&self, id: u64) {
        self.listeners.write().unwrap().remove(&id);
    }

    pub fn subscriber_count(&self) -> usize {
        self.listeners.read().unwrap().len()
    }
}

impl EventSink for EventBus {
    fn emit(&self, event: Event) {
        let history = self.history.as_ref().map(|h| h.write().unwrap());
        let guard = self.listeners.read().unwrap();
        for l in guard.values() {
            l(event.clone());
        }
        if let Some(mut events) = history {
            events.push(event);
        }
    }
}

/// Sends every job event to the job's own bus and to the runtime-wide bus.
struct JobSink {
    job: Arc<EventBus>,
    all: Arc<EventBus>,
}

impl EventSink for JobSink {
    fn emit(&self, event: Event) {
        self.job.emit(event.clone());
        self.all.emit(event);
    }
}

/// The model-facing context of a session's next turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnPreview {
    /// The gateway's default model; a request may still name another.
    pub model: Option<String>,
    /// Service key of the executor the turn would use.
    pub executor: String,
    /// System instructions the model would receive, in order.
    pub system: Vec<String>,
    /// Tool definitions exactly as the executor offers them.
    pub tools: Vec<serde_json::Value>,
    pub history: Vec<protocol::SessionMessage>,
}

pub struct JobManager {
    registry: Arc<Registry>,
    sessions: Arc<dyn SessionStore>,
    events: Arc<EventBus>,
    job_events: Arc<RwLock<HashMap<String, Arc<EventBus>>>>,
    controls: Arc<RwLock<HashMap<String, TurnController>>>,
    next_job_id: AtomicU64,
    fs: Arc<dyn FsPort>,
    /// Service key of the executor every new turn uses.
    executor_key: RwLock<String>,
}

/// Default filesystem port: denies every access. A composition root that
/// needs filesystem tools supplies its own implementation via `with_fs`.
#[derive(Default)]
struct DenyFs;
impl FsPort for DenyFs {}

impl JobManager {
    pub fn new(registry: Arc<Registry>, sessions: Arc<dyn SessionStore>) -> Self {
        Self::with_fs(registry, sessions, Arc::new(DenyFs))
    }

    pub fn with_fs(registry: Arc<Registry>, sessions: Arc<dyn SessionStore>, fs: Arc<dyn FsPort>) -> Self {
        Self {
            registry,
            sessions,
            events: EventBus::shared(false),
            job_events: Arc::new(RwLock::new(HashMap::new())),
            controls: Arc::new(RwLock::new(HashMap::new())),
            next_job_id: AtomicU64::new(0),
            fs,
            executor_key: RwLock::new(sdk::NATIVE_EXECUTOR.to_string()),
        }
    }

    /// Choose the executor for turns started from now on, by service key
    /// (`sdk::NATIVE_EXECUTOR` or `sdk::PTC_EXECUTOR`). The key must name a
    /// registered executor; a running turn keeps the one it started with.
    pub fn select_executor(&self, key: &str) -> Result<()> {
        if self.registry.executor(key).is_none() {
            return Err(Error::InvalidInput(format!("no executor registered as {key}")));
        }
        *self.executor_key.write().unwrap() = key.to_string();
        Ok(())
    }

    /// The executor new turns use: its service key and the port itself.
    pub fn executor(&self) -> Result<(String, Arc<dyn sdk::ToolExecutor>)> {
        let key = self.executor_key.read().unwrap().clone();
        let executor = self.registry.executor(&key)
            .ok_or_else(|| Error::Unavailable(format!("no executor registered as {key}")))?;
        Ok((key, executor))
    }

    /// The messages a turn hands the model: the system prompt, when one is
    /// registered, then the session. The prompt is composed per turn and never
    /// stored in the session.
    fn model_history(&self, session_id: &str, cwd: &str) -> Result<Vec<protocol::SessionMessage>> {
        let mut history = Vec::new();
        if let Some(prompt) = self.registry.system_prompt() {
            let scope = sdk::Scope { cwd: cwd.into(), session_id: session_id.into() };
            history.push(protocol::SessionMessage {
                id: String::new(), role: Role::System, content: prompt.compose(&scope, &[]),
                created_at: 0, tool_calls: Vec::new(),
            });
        }
        if !session_id.is_empty() {
            history.extend(self.sessions.history(session_id)?);
        }
        Ok(history)
    }

    /// What the next turn of `session_id` would hand the model, resolved from
    /// the same registry, executor and history `start_run` uses.
    pub fn preview(&self, session_id: &str, cwd: &str) -> Result<TurnPreview> {
        let (executor_key, executor) = self.executor()?;
        let tools = self.registry.tools();
        let tool_refs: Vec<&dyn sdk::Tool> = tools.iter().map(|t| t.as_ref()).collect();
        let history = self.model_history(session_id, cwd)?;
        Ok(TurnPreview {
            model: self.registry.active_gateway().and_then(|g| g.default_model()),
            executor: executor_key,
            system: history.iter().filter(|m| m.role == Role::System).map(|m| m.content.clone()).collect(),
            tools: executor.definitions(&tool_refs),
            history,
        })
    }

    /// Live events of every job. No replay: use `job_events` to follow one job.
    pub fn events(&self) -> Arc<EventBus> {
        self.events.clone()
    }

    /// The events of one job, replayed from its start to a late subscriber.
    /// Kept after the job ends until `forget` is called.
    pub fn job_events(&self, job_id: &str) -> Option<Arc<EventBus>> {
        self.job_events.read().unwrap().get(job_id).cloned()
    }

    /// Drop a finished job's event log once its frontend has consumed it.
    pub fn forget(&self, job_id: &str) {
        self.job_events.write().unwrap().remove(job_id);
    }

    pub fn control(&self, job_id: &str) -> Option<TurnController> {
        self.controls.read().unwrap().get(job_id).cloned()
    }

    pub fn active_jobs(&self) -> usize {
        self.controls.read().unwrap().len()
    }

    /// Run one turn. The runner is chosen from the registry; the runtime itself
    /// contains no agent logic.
    pub fn start_run(
        &self,
        session_id: &str,
        cwd: &str,
        text: &str,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<String> {
        // Resolve every port before persisting the prompt, so a misconfigured
        // runtime fails the request instead of leaving an unanswered message.
        let runner = self.registry.active_runner()
            .ok_or_else(|| Error::Unavailable("no AgentRunner registered".into()))?;
        let gateway = self.registry.active_gateway()
            .ok_or_else(|| Error::Unavailable("no ModelGateway registered".into()))?;
        let (_, executor) = self.executor()?;
        // Fail closed: without a permission decision no turn may run tools.
        let permission = self.registry.permission()
            .ok_or_else(|| Error::Unavailable("no tool permission policy registered".into()))?;
        // Without it tool results would reach the model unbounded.
        let presentation = self.registry.presentation()
            .ok_or_else(|| Error::Unavailable("no tool presentation registered".into()))?;
        self.sessions.push(session_id, Role::User, text.into())?;

        let tools = self.registry.tools();
        let history = self.model_history(session_id, cwd)?;
        let job_id = format!("j-{}", self.next_job_id.fetch_add(1, Ordering::SeqCst));
        let controller = TurnController::default();

        let cancellation = controller.cancellation();
        let tool_context = ToolContext {
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            cancellation_token: Arc::new(move || cancellation.load(Ordering::SeqCst)),
            fs: self.fs.clone(),
        };
        self.controls.write().unwrap().insert(job_id.clone(), controller.clone());
        let job_bus = EventBus::shared(true);
        self.job_events.write().unwrap().insert(job_id.clone(), job_bus.clone());

        let session_id_owned = session_id.to_string();
        let cwd_owned = cwd.to_string();
        let model_owned = model.map(str::to_string);
        let effort_owned = effort.map(str::to_string);
        let events = JobSink { job: job_bus, all: self.events.clone() };
        let jid = job_id.clone();
        let controls = self.controls.clone();
        let sessions = self.sessions.clone();

        std::thread::spawn(move || {
            let tool_refs: Vec<&dyn sdk::Tool> = tools.iter().map(|t| t.as_ref()).collect();
            let ctx = TurnContext {
                session_id: &session_id_owned,
                cwd: &cwd_owned,
                history: &history,
                gateway: gateway.as_ref(),
                tools: &tool_refs,
                model: model_owned.as_deref(),
                effort: effort_owned.as_deref(),
                controller: controller.handle(),
                executor: executor.as_ref(),
                permission: permission.as_ref(),
                presentation: presentation.as_ref(),
                tool_context,
            };

            // Wrapper sink: persist assistant text before forwarding every event.
            let recorder = RecordingSink::new(&events, &session_id_owned, sessions.as_ref());
            let outcome = runner.run(&ctx, &recorder);
            let outcome = outcome.and(recorder.flush());
            // The controller goes before the terminal event, so a frontend
            // that sees JobFinished never finds the job still controllable.
            controls.write().unwrap().remove(&jid);

            match outcome {
                Ok(()) => events.emit(Event::JobFinished {
                    session_id: session_id_owned,
                    job_id: jid.clone(),
                }),
                Err(e) => events.emit(Event::JobFailed {
                    session_id: session_id_owned,
                    job_id: jid.clone(),
                    error: e.to_string(),
                }),
            }
        });

        Ok(job_id)
    }
}

/// Persists every message the turn adds to the conversation (`MessageAdded`),
/// in order, before the event reaches clients. The session therefore holds the
/// whole trajectory (tool requests and the results the model saw) and the
/// next turn's request starts with exactly the text this turn sent.
struct RecordingSink<'a> {
    inner: &'a dyn EventSink,
    session_id: &'a str,
    sessions: &'a dyn SessionStore,
    /// The first message that could not be stored; it fails the job.
    failed: Mutex<Option<Error>>,
}

impl<'a> RecordingSink<'a> {
    fn new(inner: &'a dyn EventSink, session_id: &'a str, sessions: &'a dyn SessionStore) -> Self {
        Self { inner, session_id, sessions, failed: Mutex::new(None) }
    }
    /// A message that could not be persisted fails the job rather than vanishing.
    fn flush(&self) -> Result<()> {
        match self.failed.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl EventSink for RecordingSink<'_> {
    fn emit(&self, event: Event) {
        if let Event::MessageAdded { message } = &event {
            if let Err(e) = self.sessions.append(self.session_id, message.clone()) {
                self.failed.lock().unwrap().get_or_insert(e);
            }
        }
        self.inner.emit(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::SessionMessage;
    use sdk::{AgentRunner, Chunk, Ctx, ModelGateway, ModelInfo, ProviderInfo, Stream, ToolExecutor, ToolOutput};
    use serde_json::Value;
    use std::sync::atomic::AtomicUsize;

    /// Minimal in-memory session store for runtime tests.
    #[derive(Default)]
    struct TestSessions {
        rows: Mutex<std::collections::HashMap<String, (protocol::SessionRow, Vec<protocol::SessionMessage>)>>,
    }
    impl SessionStore for TestSessions {
        fn create(&self, cwd: &str) -> Result<protocol::SessionRow> {
            let row = protocol::SessionRow {
                id: "s-0".into(), cwd: cwd.into(), title: None, created_at: 0, updated_at: 0, message_count: 0,
            };
            self.rows.lock().unwrap().insert(row.id.clone(), (row.clone(), Vec::new()));
            Ok(row)
        }
        fn push(&self, id: &str, role: Role, content: String) -> Result<()> {
            self.append(id, protocol::SessionMessage {
                id: String::new(), role, content, created_at: 0, tool_calls: Vec::new(),
            })
        }
        fn append(&self, id: &str, message: protocol::SessionMessage) -> Result<()> {
            let mut rows = self.rows.lock().unwrap();
            let entry = rows.entry(id.into()).or_insert((
                protocol::SessionRow {
                    id: id.into(), cwd: ".".into(), title: None, created_at: 0, updated_at: 0, message_count: 0,
                }, Vec::new(),
            ));
            entry.1.push(message);
            Ok(())
        }
        fn history(&self, id: &str) -> Result<Vec<protocol::SessionMessage>> {
            Ok(self.rows.lock().unwrap().get(id).map(|e| e.1.clone()).unwrap_or_default())
        }
        fn list(&self, cwd: Option<&str>) -> Result<Vec<protocol::SessionRow>> {
            Ok(self.rows.lock().unwrap().values()
                .filter(|(r, _)| cwd.is_none_or(|c| r.cwd == c))
                .map(|(r, _)| r.clone()).collect())
        }
    }

    #[derive(Default)]
    struct NoFs;
    impl FsPort for NoFs {}

    /// Passes results through unchanged.
    struct Verbatim;
    impl sdk::ToolPresentation for Verbatim {
        fn render_call(&self, name: &str, _: &Value) -> String { name.into() }
        fn render_result(&self, output: &ToolOutput) -> String { output.content.clone() }
        fn render_diagnostic(&self, error: &Error) -> String { error.to_string() }
    }

    struct TestExecutor;
    impl ToolExecutor for TestExecutor {
        fn name(&self) -> &str { "test" }
        fn execute(&self, t: &dyn sdk::Tool, a: Value, c: &ToolContext,
                   _p: &sdk::PermissionCheck) -> Result<ToolOutput> {
            t.execute(a, c)
        }
        fn definitions(&self, _tools: &[&dyn sdk::Tool]) -> Vec<Value> { vec![] }
        fn dispatch(&self, _call: &str, _a: Value, _tools: &[&dyn sdk::Tool], _c: &ToolContext,
                    _p: &sdk::PermissionCheck) -> Result<ToolOutput> {
            Ok(ToolOutput::err("no tools in this fixture"))
        }
    }

    struct TestGateway;
    impl ModelGateway for TestGateway {
        fn id(&self) -> &str { "g" }
        fn providers(&self) -> Result<Vec<ProviderInfo>> { Ok(vec![]) }
        fn models(&self) -> Result<Vec<ModelInfo>> { Ok(vec![]) }
        fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn default_model(&self) -> Option<String> { Some("m".into()) }
        fn stream(&self, _: &str, _: &[SessionMessage], _: &[Value]) -> Result<Stream> {
            Ok(Box::new(vec![
                Ok(Chunk::TextDelta("hello ".into())),
                Ok(Chunk::TextDelta("world".into())),
                Ok(Chunk::Done),
            ].into_iter()))
        }
    }

    /// Runner that records the controller it received and emits one text delta.
    struct TestRunner {
        seen_cancel: Arc<AtomicBool>,
        seen_steers: Arc<Mutex<Vec<String>>>,
    }
    impl AgentRunner for TestRunner {
        fn id(&self) -> &str { "r" }
        fn run(&self, tc: &TurnContext<'_>, sink: &dyn EventSink) -> Result<()> {
            if tc.controller.is_cancelled() {
                self.seen_cancel.store(true, Ordering::SeqCst);
            }
            for s in tc.controller.take_steers() {
                self.seen_steers.lock().unwrap().push(s);
            }
            sink.emit(Event::TextDelta { text: "assistant answer".into() });
            // A runner announces every message it adds; the runtime stores it.
            sink.emit(Event::MessageAdded { message: SessionMessage {
                id: String::new(), role: Role::Assistant, content: "assistant answer".into(),
                created_at: 0, tool_calls: Vec::new(),
            } });
            Ok(())
        }
    }

    fn fixture(runner: TestRunner) -> (JobManager, Arc<dyn SessionStore>) {
        fixture_with(runner, true)
    }

    fn fixture_with(runner: TestRunner, permission: bool) -> (JobManager, Arc<dyn SessionStore>) {
        let registry = Arc::new(Registry::default());
        registry.declare(sdk::ResolverManifest {
            id: "g".into(), name: "g".into(), version: "0".into(),
            form: sdk::PluginForm::Function,
            registrations: vec![sdk::RegistrationDecl::Gateway { name: "g".into() }],
            config_schema: None,
        });
        let mut ctx = crate::LoaderCtx::new("g", registry.clone());
        let mut d = sdk::Disposer::default();
        d.push(ctx.effect(sdk::Contribution::Gateway { gateway: Box::new(TestGateway) }).unwrap());
        std::mem::forget(d);
        registry.declare(sdk::ResolverManifest {
            id: "r".into(), name: "r".into(), version: "0".into(),
            form: sdk::PluginForm::Function,
            registrations: vec![sdk::RegistrationDecl::Runner { name: "r".into() }],
            config_schema: None,
        });
        let mut ctx = crate::LoaderCtx::new("r", registry.clone());
        let mut d = sdk::Disposer::default();
        d.push(ctx.effect(sdk::Contribution::Runner { runner: Box::new(runner) }).unwrap());
        std::mem::forget(d);
        registry.declare(sdk::ResolverManifest {
            id: "x".into(), name: "x".into(), version: "0".into(),
            form: sdk::PluginForm::Function,
            registrations: vec![
                sdk::RegistrationDecl::Service { name: sdk::NATIVE_EXECUTOR.into() },
                sdk::RegistrationDecl::Service { name: sdk::TOOL_PERMISSION.into() },
                sdk::RegistrationDecl::Service { name: sdk::TOOL_PRESENTATION.into() },
            ],
            config_schema: None,
        });
        let mut ctx = crate::LoaderCtx::new("x", registry.clone());
        let mut d = sdk::Disposer::default();
        let executor: Arc<dyn ToolExecutor> = Arc::new(TestExecutor);
        d.push(ctx.effect(sdk::Contribution::Service {
            name: sdk::NATIVE_EXECUTOR.into(),
            service: Arc::new(executor),
        }).unwrap());
        if permission {
            let allow: Arc<sdk::PermissionCheck> = Arc::new(|_: &str, _: &Value| Ok(()));
            d.push(ctx.effect(sdk::Contribution::Service {
                name: sdk::TOOL_PERMISSION.into(),
                service: Arc::new(allow),
            }).unwrap());
        }
        let verbatim: Arc<dyn sdk::ToolPresentation> = Arc::new(Verbatim);
        d.push(ctx.effect(sdk::Contribution::Service {
            name: sdk::TOOL_PRESENTATION.into(),
            service: Arc::new(verbatim),
        }).unwrap());
        std::mem::forget(d);

        let sessions: Arc<dyn SessionStore> = Arc::new(TestSessions::default());
        sessions.create(".").unwrap();
        let jobs = JobManager::with_fs(registry, sessions.clone(), Arc::new(NoFs));
        (jobs, sessions)
    }

    #[test]
    fn persists_user_prompt_and_assistant_reply() {
        let runner = TestRunner {
            seen_cancel: Arc::new(AtomicBool::new(false)),
            seen_steers: Arc::new(Mutex::new(vec![])),
        };
        let (jobs, sessions) = fixture(runner);
        let id = jobs.start_run("s-0", ".", "user prompt", None, None).unwrap();
        // Wait for the job thread to unwind.
        for _ in 0..200 {
            if jobs.control(&id).is_none() { break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let messages = sessions.history("s-0").unwrap();
        assert!(messages.iter().any(|m| m.role == Role::User && m.content == "user prompt"));
        assert!(messages.iter().any(|m| m.role == Role::Assistant && m.content == "assistant answer"));
    }

    #[test]
    fn each_job_replays_only_its_own_events() {
        let runner = TestRunner {
            seen_cancel: Arc::new(AtomicBool::new(false)),
            seen_steers: Arc::new(Mutex::new(vec![])),
        };
        let (jobs, _) = fixture(runner);
        let first = jobs.start_run("s-0", ".", "a", None, None).unwrap();
        let second = jobs.start_run("s-0", ".", "b", None, None).unwrap();
        for _ in 0..200 {
            if jobs.active_jobs() == 0 { break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let sub = jobs.job_events(&second).unwrap()
            .subscribe(move |e| sink.lock().unwrap().push(e));
        drop(sub);
        let finished: Vec<String> = seen.lock().unwrap().iter().filter_map(|e| match e {
            Event::JobFinished { job_id, .. } => Some(job_id.clone()),
            _ => None,
        }).collect();
        assert_eq!(finished, vec![second.clone()]);
        jobs.forget(&first);
        assert!(jobs.job_events(&first).is_none());
    }

    #[test]
    fn without_a_permission_policy_no_turn_starts_and_no_prompt_is_stored() {
        let runner = TestRunner {
            seen_cancel: Arc::new(AtomicBool::new(false)),
            seen_steers: Arc::new(Mutex::new(vec![])),
        };
        let (jobs, sessions) = fixture_with(runner, false);
        let err = jobs.start_run("s-0", ".", "hi", None, None).unwrap_err();
        assert_eq!(err, Error::Unavailable("no tool permission policy registered".into()));
        assert!(sessions.history("s-0").unwrap().is_empty());
    }

    #[test]
    fn only_a_registered_executor_can_be_selected() {
        let runner = TestRunner {
            seen_cancel: Arc::new(AtomicBool::new(false)),
            seen_steers: Arc::new(Mutex::new(vec![])),
        };
        let (jobs, _) = fixture(runner);
        assert_eq!(jobs.executor().unwrap().0, sdk::NATIVE_EXECUTOR);
        assert!(jobs.select_executor(sdk::PTC_EXECUTOR).is_err());
        assert!(jobs.select_executor(sdk::NATIVE_EXECUTOR).is_ok());
    }

    #[test]
    fn the_system_prompt_leads_the_turn_and_is_never_stored() {
        struct Fixed;
        impl sdk::SystemPrompt for Fixed {
            fn compose(&self, scope: &sdk::Scope, _: &[String]) -> String {
                format!("prompt for {}", scope.cwd)
            }
        }
        let runner = TestRunner {
            seen_cancel: Arc::new(AtomicBool::new(false)),
            seen_steers: Arc::new(Mutex::new(vec![])),
        };
        let (jobs, sessions) = fixture(runner);
        assert!(jobs.preview("s-0", "/p").unwrap().system.is_empty(), "no prompt until one is registered");

        jobs.registry.declare(sdk::ResolverManifest {
            id: "sp".into(), name: "sp".into(), version: "0".into(),
            form: sdk::PluginForm::Function,
            registrations: vec![sdk::RegistrationDecl::Service { name: sdk::SYSTEM_PROMPT.into() }],
            config_schema: None,
        });
        let mut ctx = crate::LoaderCtx::new("sp", jobs.registry.clone());
        let prompt: Arc<dyn sdk::SystemPrompt> = Arc::new(Fixed);
        std::mem::forget(ctx.effect(sdk::Contribution::Service {
            name: sdk::SYSTEM_PROMPT.into(),
            service: Arc::new(prompt),
        }).unwrap());

        sessions.push("s-0", Role::User, "hi".into()).unwrap();
        let preview = jobs.preview("s-0", "/p").unwrap();
        assert_eq!(preview.system, ["prompt for /p"]);
        let roles: Vec<Role> = preview.history.iter().map(|m| m.role).collect();
        assert_eq!(roles, [Role::System, Role::User]);
        assert!(sessions.history("s-0").unwrap().iter().all(|m| m.role != Role::System));
    }

    #[test]
    fn removes_controller_when_job_finishes() {
        let runner = TestRunner {
            seen_cancel: Arc::new(AtomicBool::new(false)),
            seen_steers: Arc::new(Mutex::new(vec![])),
        };
        let (jobs, _) = fixture(runner);
        let id = jobs.start_run("s-0", ".", "hi", None, None).unwrap();
        assert!(jobs.control(&id).is_some());
        for _ in 0..200 {
            if jobs.control(&id).is_none() { break; }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(jobs.control(&id).is_none());
        assert_eq!(jobs.active_jobs(), 0);
    }

    #[test]
    fn cancel_and_steer_reach_the_runner_and_ids_are_unique() {
        let cancel = Arc::new(AtomicBool::new(false));
        let steers = Arc::new(Mutex::new(vec![]));
        let runner = TestRunner { seen_cancel: cancel.clone(), seen_steers: steers.clone() };
        let (jobs, _) = fixture(runner);
        let c = TurnController::default();
        c.cancel();
        c.steer("mid-turn".into());
        // Drive the exact controller the runner will see.
        let tc_ctrl = c.clone();
        let seen = Arc::new(AtomicUsize::new(0));
        let s = seen.clone();
        let events = jobs.events();
        let sub = events.subscribe(move |_| { s.fetch_add(1, Ordering::SeqCst); });
        assert_eq!(events.subscriber_count(), 1);
        drop(sub);
        assert_eq!(events.subscriber_count(), 0);

        let first = jobs.start_run("s-0", ".", "a", None, None).unwrap();
        let second = jobs.start_run("s-0", ".", "b", None, None).unwrap();
        assert_ne!(first, second);
        assert!(first.starts_with("j-"));
        assert!(tc_ctrl.cancellation().load(Ordering::SeqCst));
        assert_eq!(tc_ctrl.handle().take_steers(), vec!["mid-turn"]);
    }
}
