//! Pure orchestration. Knows nothing about TUI, HTTP, or storage backends.

mod command_bus;
mod context;
mod job_manager;
mod registry;
mod widget_bus;

pub use command_bus::CommandBus;
pub use context::{ContextCommand, CONTEXT_COMMAND};
pub use job_manager::{EventBus, JobManager, TurnController, TurnPreview};
pub use registry::{LoaderCtx, Registry};
pub use widget_bus::WidgetBus;

use sdk::{Config, ConfigStore, Disposer, FsPort, FunctionPlugin, PluginForm, PluginManifest, ResolverManifest, Result, SecretVault, SessionStore};
use std::sync::Arc;

pub struct Runtime {
    pub registry: Arc<Registry>,
    pub sessions: Arc<dyn SessionStore>,
    pub jobs: Arc<JobManager>,
    pub commands: Arc<CommandBus>,
    pub widgets: Arc<WidgetBus>,
    /// Unwound in reverse on `/reload` and on shutdown.
    disposers: Vec<Disposer>,
    plugins: Vec<Box<dyn FunctionPlugin>>,
}

impl Runtime {
    pub fn builder() -> RuntimeBuilder {
        RuntimeBuilder::default()
    }

    /// Assemble a runtime from already-registered parts. Used by composition
    /// roots that need to inject a custom registry (tests, embedded hosts).
    #[doc(hidden)]
    pub fn from_parts(
        registry: Arc<Registry>,
        sessions: Arc<dyn SessionStore>,
        jobs: Arc<JobManager>,
        commands: Arc<CommandBus>,
        widgets: Arc<WidgetBus>,
    ) -> Self {
        Self { registry, sessions, jobs, commands, widgets, disposers: vec![], plugins: vec![] }
    }

    pub fn plugin_manifests(&self) -> Vec<ResolverManifest> {
        self.registry.manifests()
    }

    /// Drop every disposer, then re-apply all function plugins with new config.
    pub fn reload(&mut self, config: &Config) -> Result<()> {
        self.disposers.clear();
        for plugin in &self.plugins {
            let mut ctx = LoaderCtx::new(plugin.name(), self.registry.clone());
            let d = plugin.apply(config, &mut ctx)?;
            self.disposers.push(d);
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct RuntimeBuilder {
    plugins: Vec<Box<dyn FunctionPlugin>>,
    service_manifests: Vec<ResolverManifest>,
    sessions: Option<Arc<dyn SessionStore>>,
    dependencies: Vec<(String, Arc<dyn std::any::Any + Send + Sync>)>,
    vault: Option<Arc<dyn SecretVault>>,
    config: Option<Arc<dyn ConfigStore>>,
    fs: Option<Arc<dyn FsPort>>,
    executor: Option<String>,
}

impl RuntimeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Form 2 — function plugin: named exports only, no default service type.
    pub fn with_plugin(mut self, plugin: Box<dyn FunctionPlugin>) -> Self {
        self.plugins.push(plugin);
        self
    }

    /// Form 1 — service package: a single default-exported service class.
    pub fn with_service<M: PluginManifest>(mut self) -> Self {
        self.service_manifests.push(M::manifest());
        self
    }

    /// Session storage is a port: the composition root supplies the concrete
    /// implementation from `crates/core/session`.
    pub fn with_sessions(mut self, sessions: Arc<dyn SessionStore>) -> Self {
        self.sessions = Some(sessions);
        self
    }

    /// Inject a host dependency (e.g. `SecretVault`, `ConfigStore`) into every
    /// package through `ctx.get(name)`.
    pub fn with_secret_vault(mut self, vault: Arc<dyn SecretVault>) -> Self {
        self.vault = Some(vault);
        self
    }

    pub fn with_config_store(mut self, config: Arc<dyn ConfigStore>) -> Self {
        self.config = Some(config);
        self
    }

    /// The filesystem port tools reach through `ToolContext::fs`. Without one,
    /// every filesystem access is denied.
    pub fn with_fs(mut self, fs: Arc<dyn FsPort>) -> Self {
        self.fs = Some(fs);
        self
    }

    /// Service key of the tool executor turns use (`sdk::NATIVE_EXECUTOR` by
    /// default). Checked against the registered executors in `build`.
    pub fn with_executor(mut self, key: impl Into<String>) -> Self {
        self.executor = Some(key.into());
        self
    }

    pub fn with_dependency(
        mut self,
        name: impl Into<String>,
        value: Arc<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        self.dependencies.push((name.into(), value));
        self
    }

    pub fn build(mut self) -> Result<Runtime> {
        let registry = Arc::new(Registry::default());
        for manifest in self.service_manifests {
            registry.declare(manifest);
        }
        // Function plugins must appear in the resolver manifest to be loaded.
        for plugin in &self.plugins {
            registry.declare(ResolverManifest {
                id: plugin.name().to_string(),
                name: plugin.name().to_string(),
                version: "0.1.0".into(),
                form: PluginForm::Function,
                registrations: plugin.registrations(),
                config_schema: None,
            });
        }

        for (name, value) in self.dependencies {
            registry.provide(name, value);
        }
        if let Some(v) = self.vault { registry.set_vault(v); }
        if let Some(c) = self.config { registry.set_config(c); }
        let widgets = Arc::new(WidgetBus::default());
        let sessions = self.sessions.ok_or_else(|| sdk::Error::Unavailable(
            "no SessionStore supplied to RuntimeBuilder".into()
        ))?;
        let jobs = Arc::new(match self.fs.take() {
            Some(fs) => JobManager::with_fs(registry.clone(), sessions.clone(), fs),
            None => JobManager::new(registry.clone(), sessions.clone()),
        });

        let commands = Arc::new(
            CommandBus::new(registry.clone()).with_builtin(Arc::new(ContextCommand::new(jobs.clone()))),
        );

        let mut disposers = Vec::new();
        for plugin in &self.plugins {
            let mut ctx = LoaderCtx::new(plugin.name(), registry.clone());
            let d = plugin.apply(&Config::default(), &mut ctx)?;
            disposers.push(d);
        }
        if let Some(key) = self.executor.take() {
            jobs.select_executor(&key)?;
        }

        Ok(Runtime {
            registry,
            sessions,
            jobs,
            commands,
            widgets,
            disposers,
            plugins: std::mem::take(&mut self.plugins),
        })
    }
}

pub use sdk::Error as RuntimeError;
