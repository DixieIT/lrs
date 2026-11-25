//! HTTP calls against `lele serve`. Starting a server is [`super::spawn`]'s job.

use anyhow::{anyhow, Context, Result};
use std::io::{BufRead, BufReader};
use std::sync::mpsc::Sender;
use std::time::Duration;

use lrs::protocol::{
    parse_sse_line, ApiError, CommandList, CommandResult, CommandSpec, Event, Health, ModelList,
    ModelSet, Provider, ProviderList, Registry, RegistrySet, RunRequest, RunResponse,
    SessionMessages, SessionRow, WidgetList, WidgetRow,
};


/// What answered — or didn't — on the port.
#[derive(Debug)]
pub enum Probe {
    Ok(Health),
    /// Nothing is listening. Spawning a server is the right move.
    Unreachable,
    /// Something is listening but doesn't speak `/health`. Spawning another one
    /// would just lose the race for the port; the user has to fix the server.
    Unsupported,
}

#[derive(Debug, Clone)]
pub struct Client {
    base: String,
}

impl Client {
    pub fn new(port: u16) -> Self {
        Client { base: format!("http://127.0.0.1:{port}") }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Probe `/health`. Deliberately short-timeout: this runs on the startup path
    /// and a dead port must fail fast, not stall the UI.
    pub fn probe(&self) -> Probe {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(1500)))
            .build()
            .into();
        match agent.get(format!("{}/health", self.base)).call() {
            Ok(resp) => match resp.into_body().read_json::<Health>() {
                Ok(h) => Probe::Ok(h),
                // Listening and answering 200, but not with what we asked for.
                Err(_) => Probe::Unsupported,
            },
            // ureq surfaces a non-2xx as an error carrying the status. A 404 here
            // is the signal that matters: something *is* serving this port, it
            // just predates /health — almost always a `lele` installed before
            // lrs existed. Waiting for it to start answering is futile.
            Err(ureq::Error::StatusCode(_)) => Probe::Unsupported,
            Err(_) => Probe::Unreachable,
        }
    }

    /// Convenience for the places that only care whether we can talk to it.
    pub fn health(&self) -> Option<Health> {
        match self.probe() {
            Probe::Ok(h) => Some(h),
            _ => None,
        }
    }

    pub fn start_run(&self, req: &RunRequest) -> Result<RunResponse> {
        let resp = ureq::post(format!("{}/run", self.base))
            .send_json(req)
            .context("POST /run")?
            .into_body()
            .read_json::<RunResponse>()
            .context("decoding /run response")?;
        Ok(resp)
    }

    /// The model catalog. Slow on a cold server cache (it resolves live provider
    /// catalogs), so callers fetch it off the UI thread.
    pub fn models(&self) -> Result<ModelList> {
        self.get_json("/models")
    }

    pub fn providers(&self) -> Result<Vec<Provider>> {
        Ok(self.get_json::<ProviderList>("/providers")?.providers)
    }

    /// Persist `ref_` as the default model. The answer says whether the provider
    /// still needs a key, which is what sends the UI into the connect flow.
    pub fn set_model(&self, ref_: &str) -> Result<ModelSet> {
        self.post_json("/config/model", serde_json::json!({ "model": ref_ }))
    }

    /// Hand a provider key to the server, which validates it against the provider
    /// before saving. Blocking for as long as that probe takes — never call it on
    /// the UI thread.
    pub fn connect(&self, provider: &str, key: &str) -> Result<()> {
        let body = serde_json::json!({ "provider": provider, "key": key });
        self.post_json::<serde_json::Value>("/providers/connect", body)?;
        Ok(())
    }

    /// Plugin panels for one session as they stand right now. Live changes arrive
    /// on the job stream instead; this only seeds the first frame.
    ///
    /// The session matters: one server hosts many conversations, and unscoped this
    /// would show another project's panels.
    pub fn widgets(&self, session: &str) -> Result<Vec<WidgetRow>> {
        let path = format!("/widgets?session={}", query_escape(session));
        Ok(self.get_json::<WidgetList>(&path)?.widgets)
    }

    /// Slash commands registered by plugins, for the completion menu.
    pub fn commands(&self) -> Result<Vec<CommandSpec>> {
        Ok(self.get_json::<CommandList>("/commands")?.commands)
    }

    /// Run a plugin command. The handler lives in the server's process — a
    /// frontend has no way to run it itself.
    ///
    /// An empty `session` asks the server to mint one; the answer says which, and
    /// the caller adopts it. Otherwise state set before the first turn would be
    /// filed under a session nobody goes on to use.
    pub fn run_command(&self, name: &str, arg: &str, session: &str) -> Result<CommandResult> {
        self.post_json(
            "/commands",
            serde_json::json!({ "name": name, "arg": arg, "session_id": session }),
        )
    }

    /// Plugins and extensions discovered for `cwd`, with their configured and
    /// live state.
    pub fn registry(&self, cwd: &str) -> Result<Registry> {
        self.get_json(&format!("/registry?cwd={}", query_escape(cwd)))
    }

    /// Enable or disable one. Writes config; the change lands on the next reload.
    pub fn set_registry(
        &self,
        kind: &str,
        name: &str,
        state: &str,
        project: bool,
        cwd: &str,
    ) -> Result<RegistrySet> {
        self.post_json(
            "/registry",
            serde_json::json!({
                "kind": kind, "name": name, "state": state,
                "project": project, "cwd": cwd,
            }),
        )
    }

    /// Re-register plugins and extensions, and report the state afterwards.
    pub fn reload(&self, cwd: &str) -> Result<Registry> {
        self.post_json("/reload", serde_json::json!({ "cwd": cwd }))
    }

    /// Sessions previously run in `dir`, most recently updated first.
    pub fn sessions(&self, dir: &str) -> Result<Vec<SessionRow>> {
        self.get_json(&format!("/sessions?directory={}", query_escape(dir)))
    }

    pub fn session(&self, id: &str) -> Result<SessionMessages> {
        self.get_json(&format!("/sessions/{id}"))
    }

    /// For calls that legitimately take seconds — a catalog fetch, or a key probe
    /// against a remote provider — but must still not hang forever.
    ///
    /// `http_status_as_error(false)` is the load-bearing part: ureq's default turns
    /// a 4xx into an error that carries only the status code, discarding the body.
    /// Every endpoint here answers `{"error": "..."}` with the reason a human needs
    /// ("key rejected by acme (HTTP 401)"), so the body must survive.
    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build()
            .into()
    }

    fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .agent()
            .get(format!("{}{path}", self.base))
            .call()
            .with_context(|| format!("GET {path}"))?;
        decode(resp, path)
    }

    fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T> {
        let resp = self
            .agent()
            .post(format!("{}{path}", self.base))
            .send_json(body)
            .with_context(|| format!("POST {path}"))?;
        decode(resp, path)
    }

    pub fn cancel(&self, job_id: &str) -> Result<()> {
        // Through the shared agent, for its timeout: a bare `ureq::post` has none,
        // and a wedged server would hang the one request you make when things are
        // already going wrong.
        self.agent()
            .post(format!("{}/jobs/{job_id}/cancel", self.base))
            .send_empty()
            .context("POST cancel")?;
        Ok(())
    }

    /// Redirect a turn that is already running. The engine injects it before its
    /// next model call, so it can take a tool call to land.
    pub fn steer(&self, job_id: &str, text: &str) -> Result<()> {
        let res = self
            .agent()
            .post(format!("{}/jobs/{job_id}/steer", self.base))
            .send_json(serde_json::json!({ "text": text }))
            .context("POST steer")?;
        if res.status() != 200 {
            let status = res.status();
            let body: serde_json::Value =
                res.into_body().read_json().unwrap_or(serde_json::Value::Null);
            let why = body.get("error").and_then(|e| e.as_str()).unwrap_or("").to_string();
            anyhow::bail!(if why.is_empty() { format!("HTTP {status}") } else { why });
        }
        Ok(())
    }

    /// Blocking: read the job's SSE stream to completion, forwarding each event.
    /// Meant to run on its own thread. Returns when the stream ends or the
    /// receiver is gone (the UI moved on).
    pub fn stream_events(&self, job_id: &str, tx: Sender<Event>) -> Result<()> {
        // No global timeout here, unlike `health`: a turn legitimately runs for
        // minutes between frames, and serve.py sends a heartbeat comment every 15s
        // to keep the socket warm.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(None)
            .build()
            .into();
        let resp = agent
            .get(format!("{}/jobs/{job_id}/stream", self.base))
            .call()
            .context("GET stream")?;
        let reader = BufReader::new(resp.into_body().into_reader());
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break, // server closed mid-turn
            };
            if let Some(ev) = parse_sse_line(&line) {
                let finished = matches!(ev, Event::JobFinished { .. } | Event::JobFailed { .. });
                if tx.send(ev).is_err() {
                    break; // UI gone
                }
                if finished {
                    break;
                }
            }
        }
        Ok(())
    }
}

/// Turn a response into `T`, or into the server's own error message. Reading the
/// body once, up front, is what lets a failure say *why* rather than "HTTP 400".
fn decode<T: serde::de::DeserializeOwned>(
    resp: ureq::http::Response<ureq::Body>,
    path: &str,
) -> Result<T> {
    let status = resp.status().as_u16();
    let raw = resp
        .into_body()
        .read_to_string()
        .with_context(|| format!("reading {path}"))?;
    if !(200..300).contains(&status) {
        let msg = serde_json::from_str::<ApiError>(&raw)
            .map(|e| e.error)
            .unwrap_or_default();
        let msg = if msg.is_empty() { format!("HTTP {status}") } else { msg };
        return Err(anyhow!("{msg}"));
    }
    serde_json::from_str(&raw).with_context(|| format!("decoding {path}"))
}

/// Percent-escape a query-string value. Deliberately tiny: the only thing that
/// ever goes through here is a filesystem path, and pulling in a URL crate for
/// Windows drive letters and backslashes would be the larger risk.
fn query_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_path_survives_the_query_string() {
        // Backslashes, a drive colon and spaces all have to come back intact, or
        // the server filters sessions by a directory that doesn't exist.
        assert_eq!(
            query_escape(r"D:\work\lele harness"),
            "D%3A%5Cwork%5Clele%20harness"
        );
    }

    #[test]
    fn escaping_leaves_unreserved_characters_alone() {
        assert_eq!(query_escape("a-z_0.9~"), "a-z_0.9~");
    }

    #[test]
    fn health_on_a_dead_port_is_none_not_a_panic() {
        // Port 1 is never a lele server; this must fail fast and quietly.
        assert!(Client::new(1).health().is_none());
    }

    #[test]
    fn base_url_is_loopback_only() {
        // Never dial anything but localhost: the server has no auth.
        assert_eq!(Client::new(8765).base(), "http://127.0.0.1:8765");
    }
}
