//! Commands typed with `!`: running them, and what the model is told.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// How a command typed with `!` ended.
pub enum ShellResult {
    /// It ran to the end, or hit the tool's own timeout, and printed `text`.
    Finished { text: String, failed: bool },
    /// The user stopped it.
    Stopped,
}

/// Run `command` in `cwd` until it ends or `stop` is raised. Blocking: the
/// caller keeps it off the UI thread. It is the same shell, output cleaning and
/// process-tree cleanup the agent's `bash` tool has, but it is the user's own
/// command in the user's own directory, so it asks no permission.
pub fn run(cwd: &str, command: &str, stop: &Arc<AtomicBool>) -> Result<ShellResult, String> {
    let bash = tool_shell::Bash::new(tool_shell::Shell::detect());
    let cancelled = || stop.load(Ordering::SeqCst);
    match bash.run(command, cwd, &cancelled, tool_shell::DEFAULT_TIMEOUT_MS) {
        Ok(out) => Ok(ShellResult::Finished { text: out.content, failed: out.is_error }),
        Err(sdk::Error::Cancelled) => Ok(ShellResult::Stopped),
        Err(e) => Err(e.to_string()),
    }
}

/// Most of a command's output the model is given. Errors sit at the end of a
/// long log, so the tail is kept.
const MAX_CHARS: usize = 16_000;

/// A command the user ran, and what it printed.
pub struct Ran {
    pub command: String,
    pub output: String,
}

/// The tail of `output` when it is longer than the model should read.
fn bounded(output: &str) -> String {
    let total = output.chars().count();
    if total <= MAX_CHARS {
        return output.to_string();
    }
    let tail: String = output.chars().skip(total - MAX_CHARS).collect();
    format!("[earlier output cut]\n{tail}")
}

/// Text that goes in front of the user's next message, so the model knows what
/// they ran. Empty when nothing was run.
pub fn preface(ran: &[Ran]) -> String {
    if ran.is_empty() {
        return String::new();
    }
    let mut text = String::from("Commands I ran in the project directory, with their output:\n\n");
    for r in ran {
        text.push_str(&format!("$ {}\n{}\n\n", r.command, bounded(&r.output)));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(command: &str, output: &str) -> Ran {
        Ran { command: command.into(), output: output.into() }
    }

    fn finished(result: Result<ShellResult, String>) -> (String, bool) {
        match result.unwrap() {
            ShellResult::Finished { text, failed } => (text, failed),
            ShellResult::Stopped => panic!("nobody stopped it"),
        }
    }

    #[test]
    fn a_command_reports_its_output_and_its_failure() {
        let here = std::env::current_dir().unwrap().display().to_string();
        let stop = Arc::new(AtomicBool::new(false));

        let (text, failed) = finished(run(&here, "echo hello", &stop));
        assert!(text.contains("hello"), "{text}");
        assert!(!failed);

        let (text, failed) = finished(run(&here, "exit 3", &stop));
        assert!(failed);
        assert!(text.contains("exit code 3"), "{text}");
    }

    #[test]
    fn stopping_a_command_returns_promptly() {
        let here = std::env::current_dir().unwrap().display().to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let raise = stop.clone();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(300));
            raise.store(true, Ordering::SeqCst);
        });
        let sleep = if cfg!(windows) { "Start-Sleep 30" } else { "sleep 30" };
        let started = std::time::Instant::now();
        let result = run(&here, sleep, &stop);
        stopper.join().unwrap();
        assert!(matches!(result, Ok(ShellResult::Stopped)));
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "the command was not stopped");
    }

    #[test]
    fn nothing_run_adds_nothing() {
        assert_eq!(preface(&[]), "");
    }

    #[test]
    fn the_model_is_told_each_command_and_what_it_printed() {
        let text = preface(&[ran("ls", "a.txt"), ran("git status", "clean")]);
        assert_eq!(
            text,
            "Commands I ran in the project directory, with their output:\n\n\
             $ ls\na.txt\n\n$ git status\nclean\n\n",
        );
    }

    #[test]
    fn a_long_output_keeps_its_end() {
        let long = format!("{}END", "x".repeat(MAX_CHARS));
        let text = preface(&[ran("build", &long)]);
        assert!(text.contains("[earlier output cut]"));
        assert!(text.contains("xEND"), "the tail is what is kept");
        assert!(text.chars().count() < MAX_CHARS + 200);
    }
}
