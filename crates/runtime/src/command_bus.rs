//! Routes slash commands to the handler contributed with their `CommandSpec`.

use crate::Registry;
use protocol::{CommandResult, CommandSpec};
use sdk::{CommandContext, Error, Result};
use std::sync::Arc;

pub struct CommandBus {
    registry: Arc<Registry>,
    /// Commands the runtime answers itself. They come first: a package
    /// cannot shadow them.
    builtins: Vec<Arc<dyn sdk::SlashCommandHandler>>,
}

impl CommandBus {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry, builtins: Vec::new() }
    }

    pub fn with_builtin(mut self, handler: Arc<dyn sdk::SlashCommandHandler>) -> Self {
        self.builtins.push(handler);
        self
    }

    pub fn specs(&self) -> Vec<CommandSpec> {
        let mut specs: Vec<CommandSpec> = self.builtins.iter().map(|h| h.spec()).collect();
        specs.extend(self.registry.command_specs().into_iter()
            .filter(|s| !self.builtins.iter().any(|b| b.spec().name == s.name)));
        specs
    }

    /// Resolve a command name to its handler. An unknown name returns `None`:
    /// the caller sends the text to the agent verbatim, never swallowing a typo.
    pub fn handler(&self, name: &str) -> Option<Arc<dyn sdk::SlashCommandHandler>> {
        self.builtins.iter().find(|h| h.spec().name == name).cloned()
            .or_else(|| self.registry.find_command(name))
    }

    /// Execute a command with the full host context.
    ///
    /// `input` is the raw text after `/`, e.g. `goal refactor the parser`.
    pub fn dispatch(
        &self,
        input: &str,
        session_id: &str,
        cwd: &str,
    ) -> Option<Result<CommandResult>> {
        let body = input.trim().strip_prefix('/')?;
        let (name, arg) = match body.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim_start().to_string()),
            None => (body, String::new()),
        };
        if name.is_empty() {
            return None;
        }
        let handler = self.handler(name)?;
        Some(handler.execute(&CommandContext {
            arg,
            session_id: session_id.into(),
            cwd: cwd.into(),
        }))
    }

    /// Convenience for the legacy host, which already parsed name and arg.
    pub fn run_command(
        &self,
        name: &str,
        arg: &str,
        session: &str,
        cwd: &str,
    ) -> Result<CommandResult> {
        let handler = self
            .handler(name)
            .ok_or_else(|| Error::InvalidInput(format!("unknown command /{name}")))?;
        handler.execute(&CommandContext {
            arg: arg.into(),
            session_id: session.into(),
            cwd: cwd.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::{Config, Contribution, Ctx, Disposer, FunctionPlugin, RegistrationDecl, ResolverManifest, PluginForm};
    use serde_json::json;

    struct EchoCommand;
    impl sdk::SlashCommandHandler for EchoCommand {
        fn spec(&self) -> CommandSpec {
            CommandSpec {
                name: "echo".into(), description: "echoes its argument".into(),
                args_schema: Some(json!({"type":"string"})), subcommands: vec![],
                owner: Some("test".into()),
            }
        }
        fn execute(&self, ctx: &CommandContext) -> Result<CommandResult> {
            Ok(CommandResult {
                text: format!("{}|{}|{}", ctx.arg, ctx.session_id, ctx.cwd),
                session_id: Some(ctx.session_id.clone()),
            })
        }
    }

    struct TestPlugin;
    impl FunctionPlugin for TestPlugin {
        fn name(&self) -> &str { "test" }
        fn registrations(&self) -> Vec<RegistrationDecl> {
            vec![RegistrationDecl::Command { name: "echo".into() }]
        }
        fn apply(&self, _config: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
            let handle = ctx.effect(Contribution::Command {
                handler: Box::new(EchoCommand),
            })?;
            let mut d = Disposer::default();
            d.push(handle);
            Ok(d)
        }
    }

    fn bus() -> CommandBus {
        let registry = Arc::new(Registry::default());
        registry.declare(ResolverManifest {
            id: "test".into(), name: "test".into(), version: "0".into(),
            form: PluginForm::Function, registrations: TestPlugin.registrations(),
            config_schema: None,
        });
        let mut ctx = crate::LoaderCtx::new("test", registry.clone());
        let disposer = TestPlugin.apply(&Config::default(), &mut ctx).unwrap();
        std::mem::forget(disposer); // keep registrations alive for the test
        CommandBus::new(registry)
    }

    #[test]
    fn dispatch_passes_arg_session_and_cwd_to_the_handler() {
        let result = bus().dispatch("/echo hello world", "s7", "/repo").unwrap().unwrap();
        assert_eq!(result.text, "hello world|s7|/repo");
        assert_eq!(result.session_id.as_deref(), Some("s7"));
    }

    #[test]
    fn unknown_command_returns_none() {
        assert!(bus().dispatch("/nope", "s7", "/repo").is_none());
    }

    #[test]
    fn run_command_matches_dispatch() {
        let b = bus();
        let a = b.run_command("echo", "hello", "s7", "/repo").unwrap();
        let c = b.dispatch("/echo hello", "s7", "/repo").unwrap().unwrap();
        assert_eq!(a, c);
    }
}
