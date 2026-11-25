//! Loader and effect store.
//!
//! Invariants:
//! - all state is private; readers go through accessors;
//! - a contribution is accepted only if declared in the manifest of the
//!   plugin currently being loaded;
//! - every effect returns a `RegistrationHandle` whose drop truly undoes it.

use protocol::{CommandSpec, WidgetRow};
use sdk::{
    AgentRunner, Capability, Contribution, Ctx, Error, McpProvider, ModelGateway,
    RegistrationDecl, RegistrationHandle, ResolverManifest, Result, SkillProvider,
    PermissionCheck, SlashCommandHandler, SystemPrompt, Tool, ToolExecutor, ToolPresentation,
};
use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

type Hook = dyn Fn(&dyn Any) + Send + Sync;

#[derive(Default)]
pub struct Registry {
    manifests: RwLock<HashMap<String, ResolverManifest>>,
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    commands: RwLock<HashMap<String, Arc<dyn SlashCommandHandler>>>,
    widgets: RwLock<HashMap<String, WidgetRow>>,
    gateways: RwLock<HashMap<String, Arc<dyn ModelGateway>>>,
    runners: RwLock<HashMap<String, Arc<dyn AgentRunner>>>,
    capabilities: RwLock<HashMap<String, Arc<dyn Capability>>>,
    mcp: RwLock<HashMap<String, Arc<dyn McpProvider>>>,
    skills: RwLock<HashMap<String, Arc<dyn SkillProvider>>>,
    services: RwLock<HashMap<String, Arc<dyn Any + Send + Sync>>>,
    /// Host-provided dependencies injected into packages via `ctx.get(name)`.
    dependencies: RwLock<HashMap<String, Arc<dyn Any + Send + Sync>>>,
    hooks: RwLock<HashMap<u64, (&'static str, Arc<Hook>)>>,
    next_hook_id: AtomicU64,
    active_runner: RwLock<Option<String>>,
    active_gateway: RwLock<Option<String>>,
    vault: RwLock<Option<Arc<dyn sdk::SecretVault>>>,
    config: RwLock<Option<Arc<dyn sdk::ConfigStore>>>,
}

impl Registry {
    // ---- manifest ---------------------------------------------------------

    pub fn declare(&self, manifest: ResolverManifest) {
        self.manifests.write().unwrap().insert(manifest.id.clone(), manifest);
    }

    /// Provide a host dependency visible to every package as `ctx.get(name)`.
    pub fn provide(&self, name: impl Into<String>, value: Arc<dyn Any + Send + Sync>) {
        self.dependencies.write().unwrap().insert(name.into(), value);
    }

    pub fn dependency(&self, name: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        match name {
            sdk::SECRET_VAULT => self.vault.read().unwrap().as_ref().map(|v| Arc::new(v.clone()) as Arc<dyn Any + Send + Sync>),
            sdk::CONFIG_STORE => self.config.read().unwrap().as_ref().map(|c| Arc::new(c.clone()) as Arc<dyn Any + Send + Sync>),
            _ => self.dependencies.read().unwrap().get(name).cloned(),
        }
    }

    pub fn set_vault(&self, vault: Arc<dyn sdk::SecretVault>) {
        *self.vault.write().unwrap() = Some(vault);
    }

    pub fn set_config(&self, config: Arc<dyn sdk::ConfigStore>) {
        *self.config.write().unwrap() = Some(config);
    }

    pub fn manifests(&self) -> Vec<ResolverManifest> {
        self.manifests.read().unwrap().values().cloned().collect()
    }

    /// Whether `contribution` is declared by *this* plugin's manifest.
    fn accepts(&self, plugin_id: &str, contribution: &Contribution) -> bool {
        let Some(manifest) = self.manifests.read().unwrap().get(plugin_id).cloned() else {
            return false;
        };
        manifest.registrations.iter().any(|r| match (r, contribution) {
            (RegistrationDecl::Service { name: a }, Contribution::Service { name: b, .. }) => a == b,
            (RegistrationDecl::Tool { name: a }, Contribution::Tool { tool }) => a == &tool.name(),
            (RegistrationDecl::Command { name: a }, Contribution::Command { handler }) => a == &handler.spec().name,
            (RegistrationDecl::Mcp { name: a }, Contribution::Mcp { provider }) => a == provider.id(),
            (RegistrationDecl::Skill { name: a }, Contribution::Skill { provider }) => a == provider.id(),
            (RegistrationDecl::Widget { id: a }, Contribution::Widget { widget }) => a == &widget.id,
            (RegistrationDecl::Capability { name: a }, Contribution::Capability { capability }) => a == capability.name(),
            (RegistrationDecl::Gateway { name: a }, Contribution::Gateway { gateway }) => a == gateway.id(),
            (RegistrationDecl::Runner { name: a }, Contribution::Runner { runner }) => a == runner.id(),
            (RegistrationDecl::Hook { event: a }, Contribution::Hook { event: b, .. }) => a == b,
            _ => false,
        })
    }

    // ---- readers ----------------------------------------------------------

    /// Sorted by name: the tool list is part of what the model receives, and a
    /// stable order keeps that text identical from one process to the next
    /// (and the provider's prompt cache warm).
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = self.tools.read().unwrap().values().cloned().collect();
        tools.sort_by(|a, b| a.name().cmp(b.name()));
        tools
    }
    pub fn command_specs(&self) -> Vec<CommandSpec> {
        self.commands.read().unwrap().values().map(|h| h.spec()).collect()
    }
    pub fn find_command(&self, name: &str) -> Option<Arc<dyn SlashCommandHandler>> {
        self.commands.read().unwrap().get(name).cloned()
    }
    pub fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        self.capabilities.read().unwrap().values().cloned().collect()
    }
    pub fn mcp_providers(&self) -> Vec<Arc<dyn McpProvider>> {
        self.mcp.read().unwrap().values().cloned().collect()
    }
    pub fn skill_providers(&self) -> Vec<Arc<dyn SkillProvider>> {
        self.skills.read().unwrap().values().cloned().collect()
    }
    pub fn widgets(&self) -> Vec<WidgetRow> {
        self.widgets.read().unwrap().values().cloned().collect()
    }
    /// The `ToolExecutor` registered as a service under `key`
    /// (`sdk::NATIVE_EXECUTOR` or `sdk::PTC_EXECUTOR`). Executor packages
    /// register an `Arc<dyn ToolExecutor>`, the same shape as the vault.
    pub fn executor(&self, key: &str) -> Option<Arc<dyn ToolExecutor>> {
        let service = self.services.read().unwrap().get(key).cloned()?;
        service.downcast_ref::<Arc<dyn ToolExecutor>>().cloned()
    }
    /// The model-facing presentation registered under `sdk::TOOL_PRESENTATION`.
    pub fn presentation(&self) -> Option<Arc<dyn ToolPresentation>> {
        let service = self.services.read().unwrap().get(sdk::TOOL_PRESENTATION).cloned()?;
        service.downcast_ref::<Arc<dyn ToolPresentation>>().cloned()
    }
    /// The system prompt registered under `sdk::SYSTEM_PROMPT`, if any.
    pub fn system_prompt(&self) -> Option<Arc<dyn SystemPrompt>> {
        let service = self.services.read().unwrap().get(sdk::SYSTEM_PROMPT).cloned()?;
        service.downcast_ref::<Arc<dyn SystemPrompt>>().cloned()
    }
    /// The permission closure registered under `sdk::TOOL_PERMISSION`.
    pub fn permission(&self) -> Option<Arc<PermissionCheck>> {
        let service = self.services.read().unwrap().get(sdk::TOOL_PERMISSION).cloned()?;
        service.downcast_ref::<Arc<PermissionCheck>>().cloned()
    }
    pub fn active_runner(&self) -> Option<Arc<dyn AgentRunner>> {
        let id = self.active_runner.read().unwrap().clone()?;
        self.runners.read().unwrap().get(&id).cloned()
    }
    pub fn active_gateway(&self) -> Option<Arc<dyn ModelGateway>> {
        let id = self.active_gateway.read().unwrap().clone()?;
        self.gateways.read().unwrap().get(&id).cloned()
    }
    pub fn select_runner(&self, id: &str) -> Result<()> {
        if self.runners.read().unwrap().contains_key(id) {
            *self.active_runner.write().unwrap() = Some(id.into());
            Ok(())
        } else {
            Err(Error::InvalidInput(format!("unknown runner {id}")))
        }
    }
    pub fn select_gateway(&self, id: &str) -> Result<()> {
        if self.gateways.read().unwrap().contains_key(id) {
            *self.active_gateway.write().unwrap() = Some(id.into());
            Ok(())
        } else {
            Err(Error::InvalidInput(format!("unknown gateway {id}")))
        }
    }

    // ---- mutation and undo ------------------------------------------------

    fn apply_inner(&self, contribution: Contribution) -> Result<UndoKey> {
        match contribution {
            Contribution::Tool { tool } => {
                let key = tool.name().to_string();
                self.tools.write().unwrap().insert(key.clone(), Arc::from(tool));
                Ok(UndoKey::Tool(key))
            }
            Contribution::Command { handler } => {
                let key = handler.spec().name;
                self.commands.write().unwrap().insert(key.clone(), Arc::from(handler));
                Ok(UndoKey::Command(key))
            }
            Contribution::Widget { widget } => {
                let key = format!("{}:{}", widget.session_id, widget.id);
                self.widgets.write().unwrap().insert(key.clone(), widget);
                Ok(UndoKey::Widget(key))
            }
            Contribution::Capability { capability } => {
                let key = capability.name().to_string();
                self.capabilities.write().unwrap().insert(key.clone(), Arc::from(capability));
                Ok(UndoKey::Capability(key))
            }
            Contribution::Mcp { provider } => {
                let key = provider.id().to_string();
                self.mcp.write().unwrap().insert(key.clone(), Arc::from(provider));
                Ok(UndoKey::Mcp(key))
            }
            Contribution::Skill { provider } => {
                let key = provider.id().to_string();
                self.skills.write().unwrap().insert(key.clone(), Arc::from(provider));
                Ok(UndoKey::Skill(key))
            }
            Contribution::Gateway { gateway } => {
                let key = gateway.id().to_string();
                let first = self.gateways.read().unwrap().is_empty();
                self.gateways.write().unwrap().insert(key.clone(), Arc::from(gateway));
                if first {
                    *self.active_gateway.write().unwrap() = Some(key.clone());
                }
                Ok(UndoKey::Gateway(key))
            }
            Contribution::Runner { runner } => {
                let key = runner.id().to_string();
                let first = self.runners.read().unwrap().is_empty();
                self.runners.write().unwrap().insert(key.clone(), Arc::from(runner));
                if first {
                    *self.active_runner.write().unwrap() = Some(key.clone());
                }
                Ok(UndoKey::Runner(key))
            }
            Contribution::Service { name, service } => {
                self.services.write().unwrap().insert(name.clone(), service);
                Ok(UndoKey::Service(name))
            }
            Contribution::Hook { event, handler } => {
                let id = self.next_hook_id.fetch_add(1, Ordering::SeqCst);
                self.hooks.write().unwrap().insert(id, (event, Arc::from(handler)));
                Ok(UndoKey::Hook(id))
            }
        }
    }

    fn undo(&self, key: &UndoKey) {
        match key {
            UndoKey::Tool(k) => { self.tools.write().unwrap().remove(k); }
            UndoKey::Command(k) => { self.commands.write().unwrap().remove(k); }
            UndoKey::Widget(k) => { self.widgets.write().unwrap().remove(k); }
            UndoKey::Capability(k) => { self.capabilities.write().unwrap().remove(k); }
            UndoKey::Mcp(k) => { self.mcp.write().unwrap().remove(k); }
            UndoKey::Skill(k) => { self.skills.write().unwrap().remove(k); }
            UndoKey::Service(k) => { self.services.write().unwrap().remove(k); }
            UndoKey::Gateway(k) => {
                self.gateways.write().unwrap().remove(k);
                if self.active_gateway.read().unwrap().as_deref() == Some(k.as_str()) {
                    *self.active_gateway.write().unwrap() =
                        self.gateways.read().unwrap().keys().next().cloned();
                }
            }
            UndoKey::Runner(k) => {
                self.runners.write().unwrap().remove(k);
                if self.active_runner.read().unwrap().as_deref() == Some(k.as_str()) {
                    *self.active_runner.write().unwrap() =
                        self.runners.read().unwrap().keys().next().cloned();
                }
            }
            UndoKey::Hook(id) => { self.hooks.write().unwrap().remove(id); }
        }
    }

    #[cfg(test)]
    fn hook_count(&self, event: &'static str) -> usize {
        self.hooks.read().unwrap().values().filter(|(e, _)| *e == event).count()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum UndoKey {
    Tool(String), Command(String), Widget(String), Capability(String),
    Service(String), Gateway(String), Runner(String), Hook(u64), Mcp(String), Skill(String),
}

/// The `Ctx` handed to a package. Bound to one plugin id: every effect is
/// validated against that plugin's resolver manifest only.
pub struct LoaderCtx {
    plugin_id: String,
    registry: Arc<Registry>,
}

impl LoaderCtx {
    pub fn new(plugin_id: impl Into<String>, registry: Arc<Registry>) -> Self {
        Self { plugin_id: plugin_id.into(), registry }
    }
}

impl Ctx for LoaderCtx {
    fn effect(&mut self, contribution: Contribution) -> Result<RegistrationHandle> {
        if !self.registry.accepts(&self.plugin_id, &contribution) {
            return Err(Error::InvalidInput(format!(
                "registration not declared by plugin {}", self.plugin_id
            )));
        }
        let r = self.registry.clone();
        let key = r.apply_inner(contribution)?;
        Ok(RegistrationHandle::new(move || r.undo(&key)))
    }

    fn on(&mut self, event: &'static str, handler: Box<Hook>) -> Result<RegistrationHandle> {
        self.effect(Contribution::Hook { event, handler })
    }

    fn get(&self, name: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        self.registry.dependency(name)
    }

    fn declared(&self, name: &str) -> Arc<dyn Any + Send + Sync> {
        self.get(name).unwrap_or_else(|| panic!("injection {name} was not declared"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::{Disposer, ToolContext, ToolOutput};
    use serde_json::{json, Value};

    fn manifest(id: &str, registrations: Vec<RegistrationDecl>) -> ResolverManifest {
        ResolverManifest {
            id: id.into(), name: id.into(), version: "0".into(),
            form: sdk::PluginForm::Function, registrations, config_schema: None,
        }
    }

    struct TestTool;
    impl Tool for TestTool {
        fn name(&self) -> &str { "t" }
        fn description(&self) -> &str { "" }
        fn parameters_schema(&self) -> Value { json!({}) }
        fn execute(&self, _a: Value, _c: &ToolContext) -> Result<ToolOutput> { Ok(ToolOutput::ok("")) }
    }
    struct TestGateway;
    impl ModelGateway for TestGateway {
        fn id(&self) -> &str { "g" }
        fn providers(&self) -> Result<Vec<sdk::ProviderInfo>> { Ok(vec![]) }
        fn models(&self) -> Result<Vec<sdk::ModelInfo>> { Ok(vec![]) }
        fn connect(&mut self, _: &str, _: &str) -> Result<()> { Ok(()) }
        fn default_model(&self) -> Option<String> { None }
        fn stream(&self, _: &str, _: &[protocol::SessionMessage], _: &[Value]) -> Result<sdk::Stream> {
            Ok(Box::new(std::iter::empty()))
        }
    }
    struct TestRunner;
    impl AgentRunner for TestRunner {
        fn id(&self) -> &str { "r" }
        fn run(&self, _: &sdk::TurnContext<'_>, _: &dyn sdk::EventSink) -> Result<()> { Ok(()) }
    }

    #[test]
    fn rejects_contribution_not_declared_by_this_plugin() {
        let r = Arc::new(Registry::default());
        r.declare(manifest("a", vec![RegistrationDecl::Tool { name: "t".into() }]));
        let mut ctx = LoaderCtx::new("a", r.clone());
        // declared by nobody: must be rejected even though the registry knows "a"
        let err = match ctx.effect(Contribution::Widget {
            widget: WidgetRow { session_id: "s".into(), id: "w".into(), title: String::new(), body: String::new() },
        }) { Err(e) => e, Ok(_) => panic!("undeclared contribution was accepted") };
        assert!(err.to_string().contains("plugin a"), "{err}");
        assert!(r.tools().is_empty());
    }

    #[test]
    fn dropping_handle_removes_tool_gateway_runner_and_hook() {
        let r = Arc::new(Registry::default());
        r.declare(manifest("p", vec![
            RegistrationDecl::Tool { name: "t".into() },
            RegistrationDecl::Gateway { name: "g".into() },
            RegistrationDecl::Runner { name: "r".into() },
            RegistrationDecl::Hook { event: "before_model".into() },
        ]));
        let mut ctx = LoaderCtx::new("p", r.clone());
        {
            let mut d = Disposer::default();
            d.push(ctx.effect(Contribution::Tool { tool: Box::new(TestTool) }).unwrap());
            d.push(ctx.effect(Contribution::Gateway { gateway: Box::new(TestGateway) }).unwrap());
            d.push(ctx.effect(Contribution::Runner { runner: Box::new(TestRunner) }).unwrap());
            d.push(ctx.on("before_model", Box::new(|_: &dyn Any| {})).unwrap());

            assert_eq!(r.tools().len(), 1);
            assert!(r.active_gateway().is_some());
            assert!(r.active_runner().is_some());
            assert_eq!(r.hook_count("before_model"), 1);
        }
        assert!(r.tools().is_empty());
        assert!(r.active_gateway().is_none());
        assert!(r.active_runner().is_none());
        assert_eq!(r.hook_count("before_model"), 0);
    }
}
