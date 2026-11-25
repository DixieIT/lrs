//! Run one command to completion, cancellation or timeout.
//!
//! The command gets its own process group, so stopping it stops everything it
//! started: killing only the shell would leave `node`, `cargo` or `sleep`
//! running as orphans. Output is read on two threads while the caller polls
//! for cancellation and the deadline.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the child, the cancellation flag and the clock are checked.
const POLL: Duration = Duration::from_millis(25);

/// After the command ends, how long to wait for its output pipes to close. A
/// background process it started may hold them open for ever.
const DRAIN: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum Finished {
    /// The command ended on its own; `code` is `None` when a signal ended it.
    Exited { code: Option<i32>, output: Vec<u8> },
    /// Stopped by the deadline.
    TimedOut { output: Vec<u8> },
    /// Stopped by the cancellation flag. The turn ends, so its output is dropped.
    Cancelled,
}

pub fn run(mut cmd: Command, cancelled: &dyn Fn() -> bool, timeout: Duration) -> std::io::Result<Finished> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    isolate(&mut cmd);
    let mut child = cmd.spawn()?;

    // stdout and stderr land in one buffer in arrival order, as a terminal shows them.
    let merged = Arc::new(Mutex::new(Vec::new()));
    let (closed_tx, closed_rx) = mpsc::channel();
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ];
    let mut open = 0;
    for mut pipe in pipes.into_iter().flatten() {
        open += 1;
        let (merged, closed) = (merged.clone(), closed_tx.clone());
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => merged.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(&chunk[..n]),
                }
            }
            if closed.send(()).is_err() {
                // The command already finished and nobody waits for this pipe.
                tracing::debug!("output pipe closed after the command was collected");
            }
        });
    }

    let deadline = Instant::now() + timeout;
    let ending = loop {
        if let Some(status) = child.try_wait()? {
            break Ending::Exited(status.code());
        }
        if cancelled() {
            kill_tree(&mut child);
            break Ending::Cancelled;
        }
        if Instant::now() >= deadline {
            kill_tree(&mut child);
            break Ending::TimedOut;
        }
        std::thread::sleep(POLL);
    };

    let drained_by = Instant::now() + DRAIN;
    for _ in 0..open {
        let left = drained_by.saturating_duration_since(Instant::now());
        if closed_rx.recv_timeout(left).is_err() {
            break;
        }
    }
    let output = std::mem::take(&mut *merged.lock().unwrap_or_else(|e| e.into_inner()));
    Ok(match ending {
        Ending::Exited(code) => Finished::Exited { code, output },
        Ending::TimedOut => Finished::TimedOut { output },
        Ending::Cancelled => Finished::Cancelled,
    })
}

enum Ending {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
}

#[cfg(windows)]
fn isolate(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(unix)]
fn isolate(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // A new group whose id is the child's pid, so `kill -<pid>` reaches all of it.
    cmd.process_group(0);
}

/// Stop the command and everything it started, then reap it.
fn kill_tree(child: &mut Child) {
    let pid = child.id().to_string();
    #[cfg(windows)]
    let tree = Command::new("taskkill").args(["/F", "/T", "/PID", &pid])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
    #[cfg(unix)]
    let tree = Command::new("kill").args(["-KILL", &format!("-{pid}")])
        .stdout(Stdio::null()).stderr(Stdio::null()).status();
    match tree {
        Ok(status) if status.success() => {}
        // The tree may already be gone; the direct kill below still applies.
        Ok(status) => tracing::debug!(%pid, %status, "process tree kill reported failure"),
        Err(e) => tracing::warn!(%pid, error = %e, "could not run the process tree kill"),
    }
    if let Err(e) = child.kill() {
        tracing::debug!(%pid, error = %e, "child already exited");
    }
    if let Err(e) = child.wait() {
        tracing::warn!(%pid, error = %e, "could not reap the stopped command");
    }
}
