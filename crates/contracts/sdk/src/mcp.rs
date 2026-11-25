//! MCP connection lifecycle projected into a single `Capability`.

use crate::{Capability, Result};

pub trait McpProvider: Send + Sync {
    /// Cwd-scoped connection name, e.g. `stdio:/usr/bin/rust-analyzer`.
    fn id(&self) -> &str;
    fn connect(&mut self, cwd: &str) -> Result<()>;
    fn disconnect(&mut self) -> Result<()>;
    /// Tools, prompts and resources all arrive as one capability.
    fn capability(&self) -> Box<dyn Capability>;
}
