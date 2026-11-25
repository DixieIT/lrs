//! Service: the system instructions every turn starts with.
use sdk::{
    Config, Contribution, Ctx, Disposer, FunctionPlugin, PluginForm, PluginManifest,
    RegistrationDecl, ResolverManifest, Result, Scope, SystemPrompt as SystemPromptPort,
};
use std::sync::Arc;

/// The fixed part of the system prompt. Model-facing: pinned by the tests below.
pub const PROMPT: &str = "You are Lele";

pub struct SystemPrompt;

impl SystemPromptPort for SystemPrompt {
    /// The fixed prompt, then the turn's working directory so that "here"
    /// means something. The same for every capability set.
    fn compose(&self, scope: &Scope, _: &[String]) -> String {
        if scope.cwd.trim().is_empty() {
            return PROMPT.into();
        }
        format!("{PROMPT}\nWorking directory: {}", scope.cwd)
    }
}

impl PluginManifest for SystemPrompt {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "system-prompt".into(),
            name: "system-prompt".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Service { name: sdk::SYSTEM_PROMPT.into() }],
            config_schema: None,
        }
    }
}

pub struct SystemPromptPlugin;
impl FunctionPlugin for SystemPromptPlugin {
    fn name(&self) -> &str { "system-prompt" }
    fn registrations(&self) -> Vec<RegistrationDecl> { SystemPrompt::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        // Registered as `Arc<dyn SystemPrompt>`, the shape the runtime resolves.
        let prompt: Arc<dyn SystemPromptPort> = Arc::new(SystemPrompt);
        let h = ctx.effect(Contribution::Service {
            name: sdk::SYSTEM_PROMPT.into(),
            service: Arc::new(prompt),
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
    fn the_prompt_names_the_working_directory() {
        let scope = Scope { cwd: r"d:\work\mini-lele".into(), session_id: "s".into() };
        assert_eq!(SystemPrompt.compose(&scope, &[]), "You are Lele\nWorking directory: d:\\work\\mini-lele");
    }

    #[test]
    fn without_a_working_directory_the_prompt_is_just_the_name() {
        let scope = Scope { cwd: String::new(), session_id: "s".into() };
        assert_eq!(SystemPrompt.compose(&scope, &[]), "You are Lele");
    }
}
