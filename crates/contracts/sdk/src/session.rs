//! Conversation state port. The runtime depends on this abstraction only;
//! the concrete implementation lives in `crates/core/session`.

use crate::Result;
use protocol::{Role, SessionMessage, SessionRow};

pub trait SessionStore: Send + Sync {
    fn create(&self, cwd: &str) -> Result<SessionRow>;
    fn push(&self, session_id: &str, role: Role, content: String) -> Result<()>;
    /// Append a complete message: its `tool_calls`, and for a tool result the
    /// id of the call it answers, are kept as given.
    fn append(&self, session_id: &str, message: SessionMessage) -> Result<()>;
    fn history(&self, session_id: &str) -> Result<Vec<SessionMessage>>;
    fn list(&self, cwd: Option<&str>) -> Result<Vec<SessionRow>>;
}
