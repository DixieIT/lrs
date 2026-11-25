//! Transport abstraction. The TUI talks to `dyn Backend`, never to a concrete
//! client. Both the legacy HTTP bridge and the in-process runtime implement it.

use lrs::protocol::{
    CommandResult, CommandSpec, Health, ModelList, ModelSet, Provider, Registry, RegistrySet,
    RunRequest, RunResponse, SessionMessages, SessionRow, WidgetRow,
};
use lrs::protocol::Event;
use std::sync::mpsc::Sender;

#[cfg(feature = "local-runtime")]
mod local;
#[cfg(feature = "local-runtime")]
mod local_fs;
#[cfg(feature = "local-runtime")]
pub use local::LocalBackend;

pub trait Backend: Send + Sync {
    fn base(&self) -> &str;
    fn health(&self) -> Option<Health>;
    fn start_run(&self, req: &RunRequest) -> anyhow::Result<RunResponse>;
    fn models(&self) -> anyhow::Result<ModelList>;
    fn providers(&self) -> anyhow::Result<Vec<Provider>>;
    fn set_model(&self, ref_: &str) -> anyhow::Result<ModelSet>;
    fn connect(&self, provider: &str, key: &str) -> anyhow::Result<()>;
    fn widgets(&self, session: &str) -> anyhow::Result<Vec<WidgetRow>>;
    fn commands(&self) -> anyhow::Result<Vec<CommandSpec>>;
    fn run_command(&self, name: &str, arg: &str, session: &str) -> anyhow::Result<CommandResult>;
    fn registry(&self, cwd: &str) -> anyhow::Result<Registry>;
    fn set_registry(&self, kind: &str, name: &str, state: &str, force: bool, cwd: &str) -> anyhow::Result<RegistrySet>;
    fn reload(&self, cwd: &str) -> anyhow::Result<Registry>;
    fn sessions(&self, dir: &str) -> anyhow::Result<Vec<SessionRow>>;
    fn session(&self, id: &str) -> anyhow::Result<SessionMessages>;
    fn cancel(&self, job_id: &str) -> anyhow::Result<()>;
    fn steer(&self, job_id: &str, text: &str) -> anyhow::Result<()>;
    /// Blocking: forward events until the job ends or the receiver is dropped.
    fn stream_events(&self, job_id: &str, tx: Sender<Event>) -> anyhow::Result<()>;
}

/// Legacy bridge to the Python `serve` process. Enabled by `legacy-http`.
#[cfg(feature = "legacy-http")]
pub struct HttpBackend(pub crate::net::Client);

#[cfg(feature = "legacy-http")]
impl Backend for HttpBackend {
    fn base(&self) -> &str { self.0.base() }
    fn health(&self) -> Option<Health> { self.0.health() }
    fn start_run(&self, req: &RunRequest) -> anyhow::Result<RunResponse> { Ok(self.0.start_run(req)?) }
    fn models(&self) -> anyhow::Result<ModelList> { Ok(self.0.models()?) }
    fn providers(&self) -> anyhow::Result<Vec<Provider>> { Ok(self.0.providers()?) }
    fn set_model(&self, ref_: &str) -> anyhow::Result<ModelSet> { Ok(self.0.set_model(ref_)?) }
    fn connect(&self, provider: &str, key: &str) -> anyhow::Result<()> { Ok(self.0.connect(provider, key)?) }
    fn widgets(&self, session: &str) -> anyhow::Result<Vec<WidgetRow>> { Ok(self.0.widgets(session)?) }
    fn commands(&self) -> anyhow::Result<Vec<CommandSpec>> { Ok(self.0.commands()?) }
    fn run_command(&self, name: &str, arg: &str, session: &str) -> anyhow::Result<CommandResult> {
        Ok(self.0.run_command(name, arg, session)?)
    }
    fn registry(&self, cwd: &str) -> anyhow::Result<Registry> { Ok(self.0.registry(cwd)?) }
    fn set_registry(&self, kind: &str, name: &str, state: &str, force: bool, cwd: &str) -> anyhow::Result<RegistrySet> {
        Ok(self.0.set_registry(kind, name, state, force, cwd)?)
    }
    fn reload(&self, cwd: &str) -> anyhow::Result<Registry> { Ok(self.0.reload(cwd)?) }
    fn sessions(&self, dir: &str) -> anyhow::Result<Vec<SessionRow>> { Ok(self.0.sessions(dir)?) }
    fn session(&self, id: &str) -> anyhow::Result<SessionMessages> { Ok(self.0.session(id)?) }
    fn cancel(&self, job_id: &str) -> anyhow::Result<()> { Ok(self.0.cancel(job_id)?) }
    fn steer(&self, job_id: &str, text: &str) -> anyhow::Result<()> { Ok(self.0.steer(job_id, text)?) }
    fn stream_events(&self, job_id: &str, tx: Sender<Event>) -> anyhow::Result<()> {
        Ok(self.0.stream_events(job_id, tx)?)
    }
}
