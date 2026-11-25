//! Permission policy for tool execution.
//!
//! Exports only a closure factory. There is no public `check()` service a
//! caller could invoke to bypass the executor: enforcement happens inside the
//! operation that decides to run the tool.

use sdk::{Config, Contribution, Ctx, Disposer, Error, FunctionPlugin, PluginForm,
          PluginManifest, RegistrationDecl, ResolverManifest, Result};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, Default)]
pub struct PermissionPolicy {
    pub deny: Vec<String>,
    pub allow: Vec<String>,
}

impl PermissionPolicy {
    /// The only decision point. Deny wins; a non-empty allow-list is restrictive.
    pub fn decide(&self, name: &str, _args: &Value) -> Result<()> {
        if self.deny.iter().any(|d| d == name) {
            return Err(Error::PermissionDenied(name.into()));
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|a| a == name) {
            return Err(Error::PermissionDenied(name.into()));
        }
        Ok(())
    }
}

/// Build the capability closure handed to `ToolExecutor::execute`.
pub fn permission_closure(
    policy: PermissionPolicy,
) -> impl Fn(&str, &Value) -> Result<()> + Send + Sync {
    move |name, args| policy.decide(name, args)
}

impl PluginManifest for PermissionPolicy {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "tool-permission".into(), name: "tool-permission".into(),
            version: env!("CARGO_PKG_VERSION").into(), form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Service { name: sdk::TOOL_PERMISSION.into() }],
            config_schema: None,
        }
    }
}

/// Registers the policy's closure as an `Arc<PermissionCheck>`, the shape the
/// runtime hands to every turn. The policy itself stays private.
#[derive(Default)]
pub struct PermissionPlugin {
    policy: PermissionPolicy,
}

impl PermissionPlugin {
    pub fn new(policy: PermissionPolicy) -> Self { Self { policy } }
}

impl FunctionPlugin for PermissionPlugin {
    fn name(&self) -> &str { "tool-permission" }
    fn registrations(&self) -> Vec<RegistrationDecl> { PermissionPolicy::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        let check: Arc<sdk::PermissionCheck> = Arc::new(permission_closure(self.policy.clone()));
        let h = ctx.effect(Contribution::Service {
            name: sdk::TOOL_PERMISSION.into(),
            service: Arc::new(check),
        })?;
        let mut d = Disposer::default();
        d.push(h);
        Ok(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_wins_and_allow_list_is_restrictive() {
        let p = PermissionPolicy { deny: vec!["bash".into()], allow: vec!["read".into()] };
        assert!(p.decide("bash", &Value::Null).is_err());
        assert!(p.decide("other", &Value::Null).is_err());
        assert!(p.decide("read", &Value::Null).is_ok());
    }
}
