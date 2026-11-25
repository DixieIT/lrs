//! Executes one turn. Never talks to the UI: emits `Event` on an `EventSink`.

use crate::{EventSink, ModelGateway, PermissionCheck, Result, Tool, ToolContext, ToolExecutor, ToolPresentation};
use protocol::SessionMessage;
use std::sync::Arc;

/// Everything a turn needs, handed to the active `AgentRunner`.
pub struct TurnContext<'a> {
    pub session_id: &'a str,
    pub cwd: &'a str,
    pub history: &'a [SessionMessage],
    pub gateway: &'a dyn ModelGateway,
    pub tools: &'a [&'a dyn Tool],
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub controller: Arc<dyn TurnControl>,
    /// Executor seam chosen by the composition root (native or PTC).
    pub executor: &'a dyn ToolExecutor,
    /// Permission decision for every tool the executor runs. Supplied by the
    /// runtime; the runner never builds a policy of its own.
    pub permission: &'a PermissionCheck,
    /// Model-facing rendering of tool results (size limits included). Applied
    /// to what the model receives only, never to the events a frontend shows.
    pub presentation: &'a dyn ToolPresentation,
    /// Injected by the composition root: filesystem, cancellation and any
    /// other system port a tool is allowed to use. The runner never builds one.
    pub tool_context: ToolContext,
}

/// Real-time control from the frontend: steer messages and cancellation.
pub trait TurnControl: Send + Sync {
    fn is_cancelled(&self) -> bool;
    /// Messages typed mid-turn, waiting to be injected at a safe point.
    fn take_steers(&self) -> Vec<String>;
}

/// A runner never stores anything itself: it emits `Event::MessageAdded` for
/// every message it adds to the conversation (assistant replies and tool
/// requests, tool results as the model saw them, steers), in order, and the
/// runtime persists them. What it does not announce is lost after the turn.
pub trait AgentRunner: Send + Sync {
    /// Stable registration id, used by undo and by select_runner().
    fn id(&self) -> &str;
    fn run(&self, ctx: &TurnContext<'_>, sink: &dyn EventSink) -> Result<()>;
}
