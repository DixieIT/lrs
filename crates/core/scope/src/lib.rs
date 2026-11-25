//! Request boundary, isolated per job.
use sdk::{
    Config, Contribution, Ctx, Disposer, FunctionPlugin, PluginForm, PluginManifest,
    RegistrationDecl, ResolverManifest, Result, Scope as ScopeValue,
};

pub use sdk::Scope;

#[derive(Clone)]
pub struct ScopeService(pub ScopeValue);

impl PluginManifest for ScopeService {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "scope".into(),
            name: "scope".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Service { name: "scope".into() }],
            config_schema: None,
        }
    }
}

pub struct ScopePlugin;
impl FunctionPlugin for ScopePlugin {
    fn name(&self) -> &str { "scope" }
    fn registrations(&self) -> Vec<RegistrationDecl> { ScopeService::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        let h = ctx.effect(Contribution::Service {
            name: "scope".into(),
            service: std::sync::Arc::new(ScopeService(ScopeValue {
                cwd: ".".into(),
                session_id: String::new(),
            })),
        })?;
        let mut d = Disposer::default();
        d.push(h);
        Ok(d)
    }
}
