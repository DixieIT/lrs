//! Native tool executor: enforces the permission closure before invoking the
//! tool, and respects cancellation from the injected `ToolContext`.

use sdk::{
    Config, Contribution, Ctx, Disposer, FunctionPlugin, PluginForm, PluginManifest,
    RegistrationDecl, ResolverManifest, Result, Tool, ToolContext, ToolExecutor, ToolOutput,
};
use serde_json::Value;

#[derive(Clone, Copy)]
pub struct ToolExecutorNative;

impl ToolExecutor for ToolExecutorNative {
    fn name(&self) -> &str {
        "native"
    }

    fn execute(
        &self,
        tool: &dyn Tool,
        args: Value,
        ctx: &ToolContext,
        permission: &sdk::PermissionCheck,
    ) -> Result<ToolOutput> {
        permission(tool.name(), &args)?;
        // After the decision, before the tool: a malformed call is reported to
        // the model with the field and the type it expected, and never runs.
        if let Err(problems) = tool_schema::validate_args(&tool.parameters_schema(), &args) {
            return Ok(ToolOutput::err(format!("invalid arguments for {}: {problems}", tool.name())));
        }
        if ctx.is_cancelled() {
            return Err(sdk::Error::Cancelled);
        }
        tool.execute(args, ctx)
    }

    fn definitions(&self, tools: &[&dyn Tool]) -> Vec<Value> {
        tools.iter().map(|t| serde_json::json!({
            "name": t.name(),
            "description": t.description(),
            "parameters": t.parameters_schema(),
        })).collect()
    }

    fn dispatch(
        &self,
        call: &str,
        args: Value,
        tools: &[&dyn Tool],
        ctx: &ToolContext,
        permission: &sdk::PermissionCheck,
    ) -> Result<ToolOutput> {
        match tools.iter().find(|t| t.name() == call) {
            Some(tool) => self.execute(*tool, args, ctx, permission),
            None => Ok(ToolOutput::err(format!("unknown tool {call}"))),
        }
    }
}

/// Registration adapter. The executor is a service contributed as an effect;
/// its name is part of the resolver manifest, so the Loader can reject an
/// undeclared registration.
pub struct NativeExecutorPlugin {
    executor: ToolExecutorNative,
}

impl Default for NativeExecutorPlugin {
    fn default() -> Self {
        Self { executor: ToolExecutorNative }
    }
}

impl sdk::PluginManifest for NativeExecutorPlugin {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "tool-executor-native".into(),
            name: "tool-executor-native".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Service { name: "native".into() }],
            config_schema: None,
        }
    }
}

impl FunctionPlugin for NativeExecutorPlugin {
    fn name(&self) -> &str {
        "tool-executor-native"
    }

    fn registrations(&self) -> Vec<RegistrationDecl> {
        Self::manifest().registrations
    }

    fn apply(&self, _config: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        // Registered as `Arc<dyn ToolExecutor>` so the runtime can recover the
        // port from the type-erased service without knowing this concrete type.
        let executor: std::sync::Arc<dyn ToolExecutor> = std::sync::Arc::new(self.executor);
        let handle = ctx.effect(Contribution::Service {
            name: sdk::NATIVE_EXECUTOR.to_string(),
            service: std::sync::Arc::new(executor),
        })?;
        let mut disposer = Disposer::default();
        disposer.push(handle);
        Ok(disposer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sdk::{FsPort, ToolOutput};

    struct DenyFs;
    impl FsPort for DenyFs {}

    struct EchoTool;
    impl Tool for EchoTool {
        fn name(&self) -> &str { "echo" }
        fn description(&self) -> &str { "" }
        fn parameters_schema(&self) -> Value { serde_json::json!({}) }
        fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
            Ok(ToolOutput::ok(args["text"].as_str().unwrap_or("")))
        }
    }

    fn context(cancelled: bool) -> ToolContext {
        ToolContext {
            session_id: "s".into(),
            cwd: ".".into(),
            cancellation_token: std::sync::Arc::new(move || cancelled),
            fs: std::sync::Arc::new(DenyFs),
        }
    }

    #[test]
    fn denies_when_permission_rejects() {
        let err = ToolExecutorNative.execute(
            &EchoTool,
            serde_json::json!({"text": "hi"}),
            &context(false),
            &|_, _| Err(sdk::Error::PermissionDenied("echo".into())),
        ).unwrap_err();
        assert!(matches!(err, sdk::Error::PermissionDenied(_)));
    }

    #[test]
    fn respects_cancellation() {
        let err = ToolExecutorNative.execute(
            &EchoTool,
            serde_json::json!({"text": "hi"}),
            &context(true),
            &|_, _| Ok(()),
        ).unwrap_err();
        assert_eq!(err, sdk::Error::Cancelled);
    }

    #[test]
    fn returns_tool_output_when_allowed_and_live() {
        let out = ToolExecutorNative.execute(
            &EchoTool,
            serde_json::json!({"text": "hi"}),
            &context(false),
            &|_, _| Ok(()),
        ).unwrap();
        assert_eq!(out.content, "hi");
        assert!(!out.is_error);
    }

    /// Requires `path` (string); counts how often it really ran.
    struct Strict(std::sync::Arc<std::sync::atomic::AtomicUsize>);
    impl Tool for Strict {
        fn name(&self) -> &str { "strict" }
        fn description(&self) -> &str { "" }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]})
        }
        fn execute(&self, _args: Value, _ctx: &ToolContext) -> Result<ToolOutput> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ToolOutput::ok("ran"))
        }
    }

    #[test]
    fn invalid_arguments_are_a_tool_error_and_the_tool_never_runs() {
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let out = ToolExecutorNative.execute(
            &Strict(runs.clone()), serde_json::json!({"path": 7}), &context(false), &|_, _| Ok(()),
        ).unwrap();
        assert!(out.is_error);
        assert_eq!(out.content, "invalid arguments for strict: `path` must be string, got a number (7)");
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn a_denial_wins_over_a_validation_error() {
        let err = ToolExecutorNative.execute(
            &Strict(Default::default()), serde_json::json!({}), &context(false),
            &|name, _| Err(sdk::Error::PermissionDenied(name.into())),
        ).unwrap_err();
        assert_eq!(err, sdk::Error::PermissionDenied("strict".into()));
    }

    #[test]
    fn offers_every_tool_and_dispatches_by_name() {
        let tools: [&dyn Tool; 1] = [&EchoTool];
        let defs = ToolExecutorNative.definitions(&tools);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["name"], "echo");
        let out = ToolExecutorNative.dispatch(
            "echo", serde_json::json!({"text": "hi"}), &tools, &context(false), &|_, _| Ok(()),
        ).unwrap();
        assert_eq!(out.content, "hi");
        let unknown = ToolExecutorNative.dispatch(
            "nope", Value::Null, &tools, &context(false), &|_, _| Ok(()),
        ).unwrap();
        assert!(unknown.is_error);
    }
}
