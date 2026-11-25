//! Getting a server without the user starting one.
//!
//! The stated requirement: never make someone run `lele serve` in each project
//! before opening lrs. So we probe, and spawn a detached server if nothing
//! answers. One server serves every project — `cwd` travels per request, and the
//! engine scopes model/effort to the job's thread, so two windows in two repos
//! don't collide.

use anyhow::{anyhow, Context, Result};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::client::{Client, Probe};

/// How long to wait for a freshly spawned server. `warmup()` on the Python side
/// does a TCP preconnect and an agentmap build, but `/health` answers as soon as
/// the socket binds, so this window is a generous ceiling rather than a typical wait.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Returns the client plus whether we had to start the server ourselves (the UI
/// says so on the first frame, otherwise a 3-second pause looks like a hang).
pub fn ensure_server(port: u16, spawn_allowed: bool) -> Result<(Client, bool)> {
    let client = Client::new(port);
    match client.probe() {
        Probe::Ok(_) => return Ok((client, false)),
        Probe::Unsupported => return Err(stale_server(port)),
        Probe::Unreachable => {}
    }
    if !spawn_allowed {
        return Err(anyhow!(
            "no lele serve on port {port} and --no-spawn was given \
             — start one with `lele serve --port {port}`"
        ));
    }

    spawn_detached(port).context("spawning `lele serve`")?;

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    while Instant::now() < deadline {
        match client.probe() {
            Probe::Ok(_) => return Ok((client, true)),
            // The server we just started came up without /health, so it is an
            // older lele than this binary expects. Polling for the rest of the
            // minute would only delay the same failure.
            Probe::Unsupported => return Err(stale_server(port)),
            Probe::Unreachable => std::thread::sleep(POLL_INTERVAL),
        }
    }
    Err(anyhow!(
        "spawned `lele serve` but it never answered on port {port} \
         — run `lele serve --port {port}` in a terminal to see why"
    ))
}

/// The failure people actually hit: `lele` on PATH is an installed copy older
/// than lrs, so its `serve` has no `/health`. Say exactly that, and how to
/// fix it — a bare "connection failed" would send anyone hunting the wrong thing.
fn stale_server(port: u16) -> anyhow::Error {
    anyhow!(
        "something is serving port {port} but it has no /health endpoint.\n\
         That is a `lele serve` older than lrs. Reinstall lele from this \
         checkout:\n    uv tool install --reinstall .\n\
         (or point elsewhere with --port, if that server isn't lele at all)"
    )
}

#[cfg(windows)]
fn spawn_detached(port: u16) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW *only*. It gives the child a console that is never shown,
    // which is exactly what we want. Do not add DETACHED_PROCESS alongside it:
    // the two are contradictory (one asks for a hidden console, the other for no
    // console at all), Windows resolves the conflict by ignoring them, and the
    // server pops up a visible black window on top of the TUI.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new("lele")
        .args(["serve", "--port", &port.to_string()])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(port: u16) -> Result<()> {
    Command::new("lele")
        .args(["serve", "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn no_spawn_refuses_instead_of_hanging() {
        // Port 1 never hosts a lele server.
        let err = ensure_server(1, false).unwrap_err().to_string();
        assert!(err.contains("--no-spawn"), "unexpected: {err}");
    }

    /// Serve one request with the given status, then stop. Stands in for a `lele
    /// serve` that predates /health.
    fn one_shot(status: &'static str, body: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        port
    }

    #[test]
    fn an_old_server_fails_fast_with_the_fix_in_the_message() {
        // The real-world case: `lele` on PATH is older than lrs, so its
        // serve answers 404 on /health. This must not spin for the whole startup
        // timeout, and must name the remedy.
        let port = one_shot("404 Not Found", r#"{"error":"not found"}"#);
        let started = Instant::now();
        let err = ensure_server(port, true).unwrap_err().to_string();
        assert!(started.elapsed() < Duration::from_secs(5), "it waited too long");
        assert!(err.contains("no /health"), "unexpected: {err}");
        assert!(err.contains("uv tool install --reinstall"), "no remedy given: {err}");
    }

    #[test]
    fn a_healthy_server_is_used_as_is_and_not_respawned() {
        let port = one_shot("200 OK", r#"{"ok":true,"model":"m"}"#);
        let (client, spawned) = ensure_server(port, true).unwrap();
        assert!(!spawned, "must reuse a server that is already answering");
        assert_eq!(client.health().map(|h| h.model).unwrap_or_default(), "m");
    }

    #[test]
    fn a_200_that_is_not_health_shaped_counts_as_unsupported() {
        // Some other service on the port, answering 200 with unrelated JSON.
        let port = one_shot("200 OK", r#"[1,2,3]"#);
        let err = ensure_server(port, true).unwrap_err().to_string();
        assert!(err.contains("no /health"), "unexpected: {err}");
    }
}
