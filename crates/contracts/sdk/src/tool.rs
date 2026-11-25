//! A tool the agent may call. `ToolContext` exposes only what is strictly
//! necessary — never another component.

use crate::{FsPort, Result};
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }

    pub fn err(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

/// The only capabilities a tool receives. Components that need file access use
/// `FsPort`; they never import `tool-fs` or any other sibling.
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: String,
    pub cwd: String,
    pub cancellation_token: Arc<dyn Fn() -> bool + Send + Sync>,
    pub fs: Arc<dyn FsPort>,
}

impl ToolContext {
    pub fn is_cancelled(&self) -> bool {
        (self.cancellation_token)()
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput>;
}
