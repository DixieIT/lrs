//! The `bash` tool: run one shell command in the project directory.
//!
//! The shell is chosen once, when the plugin starts (see [`shell`]), and the
//! tool's description names it so the model writes commands for the right
//! one. Every command gets its own process group and the whole tree is killed
//! on cancellation or timeout (see [`process`]). Output length is not limited
//! here: that is the model-facing presentation's job.

mod process;
mod shell;

pub use shell::Shell;

use process::Finished;
use sdk::{
    Config, Contribution, Ctx, Disposer, Error, FunctionPlugin, PluginForm, PluginManifest,
    RegistrationDecl, ResolverManifest, Result, Tool, ToolContext, ToolOutput,
};
use serde_json::{json, Value};
use std::time::Duration;

pub const BASH: &str = "bash";

/// Timeout when the call names none, and the most a call may ask for.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;

pub struct Bash {
    shell: Shell,
    description: String,
}

impl Bash {
    pub fn new(shell: Shell) -> Self {
        let description = describe(&shell);
        Self { shell, description }
    }
}

/// Model-facing description. Deterministic for a given shell; pinned by tests.
fn describe(shell: &Shell) -> String {
    let syntax = match shell {
        Shell::GitBash { .. } => {
            "Commands use bash syntax; Unix tools such as ls, grep, find, sed and sort are available."
        }
        Shell::PowerShell => "Commands use PowerShell syntax and cmdlets, not bash.",
        Shell::Sh => "Commands use POSIX sh syntax.",
    };
    format!(
        "Run a shell command in {label}, starting in the project directory, and return its \
         combined output. {syntax} The command is stopped after timeout_ms milliseconds \
         (default {DEFAULT_TIMEOUT_MS}, at most {MAX_TIMEOUT_MS}); a non-zero exit status is \
         reported as a failure with its code. Prefer read_file, write_file and edit_file for \
         working with files.",
        label = shell.label(),
    )
}

impl Tool for Bash {
    fn name(&self) -> &str { BASH }
    fn description(&self) -> &str { &self.description }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The command to run."},
                "timeout_ms": {"type": "integer", "description": "Milliseconds before the command is stopped."},
            },
            "required": ["command"],
        })
    }

    fn execute(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let Some(command) = args.get("command").and_then(Value::as_str).filter(|c| !c.trim().is_empty()) else {
            return Ok(ToolOutput::err("missing string argument `command`"));
        };
        let timeout_ms = args.get("timeout_ms").and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let mut cmd = self.shell.command(command);
        cmd.current_dir(&ctx.cwd);
        let cancelled = ctx.cancellation_token.clone();
        let finished = match process::run(cmd, &|| cancelled(), Duration::from_millis(timeout_ms)) {
            Ok(finished) => finished,
            Err(e) => return Ok(ToolOutput::err(format!("could not start {}: {e}", self.shell.label()))),
        };
        Ok(match finished {
            Finished::Cancelled => return Err(Error::Cancelled),
            Finished::TimedOut { output } => ToolOutput::err(format!(
                "{}\n[stopped: timed out after {timeout_ms} ms]", text(&output),
            )),
            Finished::Exited { code: Some(0), output } => ToolOutput::ok(text(&output)),
            Finished::Exited { code: Some(code), output } => {
                ToolOutput::err(format!("{}\n[exit code {code}]", text(&output)))
            }
            Finished::Exited { code: None, output } => {
                ToolOutput::err(format!("{}\n[terminated by a signal]", text(&output)))
            }
        })
    }
}

/// What the command printed, as text without terminal colour codes.
fn text(output: &[u8]) -> String {
    let cleaned = strip_ansi(&String::from_utf8_lossy(output));
    let trimmed = cleaned.trim_end();
    if trimmed.is_empty() { "(no output)".into() } else { trimmed.into() }
}

/// Remove terminal escape sequences: CSI (`ESC [ … final`), OSC (`ESC ] …`
/// up to BEL or `ESC \`) and two-character escapes. Plain text is untouched.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // Parameters and intermediates, then one final byte in @..~.
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Any other escape is two characters long; a lone ESC at the end is dropped.
            _ => {}
        }
    }
    out
}

impl PluginManifest for Bash {
    fn manifest() -> ResolverManifest {
        ResolverManifest {
            id: "tool-shell".into(),
            name: "tool-shell".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            form: PluginForm::Function,
            registrations: vec![RegistrationDecl::Tool { name: BASH.into() }],
            config_schema: None,
        }
    }
}

/// Registers `bash` with the shell detected when the plugin is built.
pub struct ToolShellPlugin {
    shell: Shell,
}

impl Default for ToolShellPlugin {
    fn default() -> Self {
        Self { shell: Shell::detect() }
    }
}

impl ToolShellPlugin {
    pub fn new(shell: Shell) -> Self { Self { shell } }
}

impl FunctionPlugin for ToolShellPlugin {
    fn name(&self) -> &str { "tool-shell" }
    fn registrations(&self) -> Vec<RegistrationDecl> { Bash::manifest().registrations }
    fn apply(&self, _c: &Config, ctx: &mut dyn Ctx) -> Result<Disposer> {
        let mut d = Disposer::default();
        d.push(ctx.effect(Contribution::Tool { tool: Box::new(Bash::new(self.shell.clone())) })?);
        Ok(d)
    }
}

#[cfg(test)]
mod tests;
