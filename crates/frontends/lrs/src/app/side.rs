//! Results of work that must not run on the UI thread.
//!
//! Three calls here can legitimately take seconds — resolving the model catalog,
//! probing an API key against its provider, and reading back a session — and the
//! event loop redraws every 30ms. Each runs on its own thread and posts one of
//! these back; [`super::App::pump`] drains them between frames.

use lrs::protocol::{CommandResult, CommandSpec, ModelSet, Provider, WidgetRow};
use crate::ui::block::Block;

pub enum Side {
    /// The model list and provider list, fetched together at startup.
    Catalog {
        models: Vec<String>,
        current: String,
        providers: Vec<Provider>,
        /// Set when the fetch failed; the pickers then say why instead of
        /// showing an empty list forever.
        error: Option<String>,
    },
    /// `POST /config/model` came back.
    ModelSet(Result<ModelSet, String>),
    /// A key was validated and saved, or rejected with a reason.
    Connected { provider: String, error: Option<String> },
    /// A past session, replayed into transcript blocks off-thread.
    Resumed {
        blocks: Vec<Block>,
        session_id: String,
        /// Ready-made line for the transcript, so the summary is computed where
        /// the session is still in hand.
        summary: String,
        error: Option<String>,
    },
    /// The clipboard image, as a `data:` URL, or why it couldn't be read.
    Image(Result<String, String>),
    /// The server's plugin command table and current widget state.
    Plugins {
        commands: Vec<CommandSpec>,
        widgets: Vec<WidgetRow>,
    },
    /// The `/context` report, for the popup.
    Context(Result<CommandResult, String>),
    /// What a plugin command printed and did.
    Command {
        name: String,
        result: Result<CommandResult, String>,
    },
    /// The plugin/extension list.
    Registry(Result<lrs::protocol::Registry, String>),
    /// Confirmation that one was enabled or disabled.
    RegistrySet(Result<String, String>),
    /// The state after a reload.
    Reloaded(Result<lrs::protocol::Registry, String>),
    /// The project's file list, for `@path` completion.
    Files(Vec<String>),
    /// A mid-turn message reached the server's steering queue, or didn't.
    Steered(Result<(), String>),
    /// A selection reached the clipboard: how much, or why not.
    Copied(Result<usize, String>),
    /// `^c` reached the backend, but the job could not be cancelled.
    CancelFailed(String),
    /// A `!` command ended, or could not be started.
    Shell { command: String, result: Result<super::shell::ShellResult, String> },
}

/// Post a background result to the UI thread. The receiver goes away only when
/// the app is closing, so a result that cannot be delivered has nobody left to
/// show it to: it is traced, not dropped in silence.
pub fn deliver(tx: &std::sync::mpsc::Sender<Side>, side: Side) {
    if tx.send(side).is_err() {
        super::log::line(format_args!("a background result arrived after the app closed"));
    }
}
