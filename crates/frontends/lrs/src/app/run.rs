//! What is running right now, as values that cannot be half there.
//!
//! A turn used to be three fields that had to agree (the job, its event channel,
//! "was it asked to stop"), and a command a fourth. Each is one `Option` now:
//! there is either a whole turn or none.

use lrs::protocol::Event;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

/// A turn on the backend: its job, the events it streams, and whether it has been
/// asked to stop and has not ended yet.
pub struct Turn {
    pub job_id: String,
    pub events: Receiver<Event>,
    pub stopping: bool,
}

/// A `!` or user command in this process, and the flag that stops it.
#[derive(Default)]
pub struct ShellRun {
    pub stop: Arc<AtomicBool>,
    pub stopping: bool,
}
