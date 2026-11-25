//! Slash command execution port.
//!
//! `CommandSpec` is only metadata for menus and completion. The executable
//! half is `SlashCommandHandler`, contributed through `ctx.effect()` together
//! with its spec. A command with no handler is not dispatchable.

use crate::Result;
use protocol::{CommandResult, CommandSpec};

/// Everything a command needs to run. Mirrors the host API: no metadata-only
/// answers, no hidden globals.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandContext {
    /// Text after the command name, verbatim. Empty string when no argument.
    pub arg: String,
    pub session_id: String,
    pub cwd: String,
}

pub trait SlashCommandHandler: Send + Sync {
    fn spec(&self) -> CommandSpec;
    fn execute(&self, ctx: &CommandContext) -> Result<CommandResult>;
}
