//! Pure port traits implemented by packages and consumed by the runtime. Model-facing text never contains UI,
//! transport or implementation vocabulary.
//!
//! Enforcement lives in the deciding operation: the permission closure is
//! passed INTO the executor, not applied by a wrapper or a listener.

use crate::{Result, Tool, ToolContext, ToolOutput};
use serde_json::Value;

/// The seam implemented by `tool-executor-native` and `tool-executor-ptc`.
///
/// An executor owns both halves of tool use: what the model is offered
/// (`definitions`) and how a call the model made is carried out (`dispatch`).
/// The native executor offers every tool directly; the PTC executor offers a
/// single code tool whose script calls the tools.
pub trait ToolExecutor: Send + Sync {
    fn name(&self) -> &str;

    /// Run one tool. `permission` is a private capability closure: no public
    /// method with one internal caller.
    fn execute(
        &self,
        tool: &dyn Tool,
        args: Value,
        ctx: &ToolContext,
        permission: &PermissionCheck,
    ) -> Result<ToolOutput>;

    /// The tool definitions the model receives, as
    /// `{"name", "description", "parameters"}` objects.
    fn definitions(&self, tools: &[&dyn Tool]) -> Vec<Value>;

    /// Carry out one call the model made, by the name it used. `tools` is the
    /// turn's tool set; every tool that runs goes through `permission`.
    fn dispatch(
        &self,
        call: &str,
        args: Value,
        tools: &[&dyn Tool],
        ctx: &ToolContext,
        permission: &PermissionCheck,
    ) -> Result<ToolOutput>;
}

/// Capability closure handed to a `ToolExecutor`. It is created by the
/// `tool-permission` package and enforced inside the deciding operation.
pub type PermissionCheck = dyn Fn(&str, &Value) -> Result<()> + Send + Sync;

/// `agent/tool-presentation` — model-facing rendering of calls, results and
/// diagnostics. Stable text is pinned verbatim by snapshots.
///
/// Registered under `sdk::TOOL_PRESENTATION` as an `Arc<dyn ToolPresentation>`;
/// the runner passes every tool result through `render_result` before the
/// model sees it, so this is where model-facing size limits live.
pub trait ToolPresentation: Send + Sync {
    fn render_call(&self, name: &str, args: &Value) -> String;
    fn render_result(&self, output: &ToolOutput) -> String;
    fn render_diagnostic(&self, error: &crate::Error) -> String;
}

/// `scope` — request boundary. Holds cwd and session identity; isolated per job.
#[derive(Debug, Clone, PartialEq)]
pub struct Scope {
    pub cwd: String,
    pub session_id: String,
}

/// `agent/default-model` — resolves the model for a turn.
pub trait DefaultModel: Send + Sync {
    fn resolve(&self, scope: &Scope, override_model: Option<&str>) -> Result<String>;
    fn context_limit(&self, model: &str) -> Result<u64>;
}

/// `system-prompt` — task-relevant instructions only, never UI vocabulary.
pub trait SystemPrompt: Send + Sync {
    fn compose(&self, scope: &Scope, capabilities: &[String]) -> String;
}

/// `tools` — the active tool set for a turn.
pub trait Tools: Send + Sync {
    fn active(&self, scope: &Scope) -> Vec<&dyn Tool>;
}
