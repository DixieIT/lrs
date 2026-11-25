//! Default-model resolution for an agent turn.
use sdk::{
    Config, Contribution, Ctx, DefaultModel as DefaultModelPort, Disposer, Error,
    FunctionPlugin, PluginForm, PluginManifest, RegistrationDecl, ResolverManifest, Result, Scope,
};

pub struct AgentDefaultModel;

impl DefaultModelPort for AgentDefaultModel {
    fn resolve(&self, _scope: &Scope, override_model: Option<&str>) -> Result<String> {
        override_model.map(str::to_string).ok_or(Error::Unavailable("no default model".into()))
    }
    fn context_limit(&self, _model: &str) -> Result<u64> {
        Err(Error::Unavailable("no model catalog".into()))
    }
}

impl PluginManifest for AgentDefaultModel {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "agent-default-model".into(),
            name: "agent-default-model".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Service { name: "agent-default-model".into() }],
            config_schema: None,
        }
    }
}

pub struct AgentDefaultModelPlugin;
impl FunctionPlugin for AgentDefaultModelPlugin {
    fn name(&self) -> &str { "agent-default-model" }
    fn registrations(&self) -> Vec<RegistrationDecl> { AgentDefaultModel::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        let h = ctx.effect(Contribution::Service {
            name: "agent-default-model".into(),
            service: std::sync::Arc::new(AgentDefaultModel),
        })?;
        let mut d = Disposer::default();
        d.push(h);
        Ok(d)
    }
}
