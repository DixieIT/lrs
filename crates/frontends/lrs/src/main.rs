//! lrs — a small, sidebar-free terminal frontend for lele.
//!
//! It is a frontend and nothing else: every decision about the conversation lives
//! behind `lele serve`, which lrs starts for you if it isn't already running.
//! Run it in a project directory; the working directory travels with each request,
//! so one background server handles every repo you have open.

mod app;
mod backend;
mod net;
mod theme;
mod ui;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use std::io::{stdout, Stdout};

pub type Terminal = ratatui::Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BackendKind { Local, Http }

struct Args {
    port: u16,
    spawn: bool,
    resume: bool,
    debug: bool,
    backend: BackendKind,
    /// Tool executor for the local backend; `None` uses the configured one.
    executor: Option<String>,
}

/// `Ok(None)` means we printed help and should exit cleanly.
fn parse_args() -> Result<Option<Args>> {
    let default_port: u16 = 8765;
    let mut port = app::config::load().port.unwrap_or(default_port);
    let mut spawn = true;
    let mut resume = false;
    let mut debug = false;
    let mut backend = if cfg!(feature = "local-runtime") { BackendKind::Local } else { BackendKind::Http };
    let mut executor = None;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!(
                    "lrs — terminal frontend for lele\n\n\
                     usage: lrs [-r] [--port N] [--no-spawn] [--backend http|local] [--executor native|ptc] [--debug]\n\n\
                     \x20 -r, --resume     reopen this project's last session\n\
                     \x20 --port N         port of `lele serve` (default {})\n\
                     \x20 --no-spawn       fail instead of starting a server\n\
                     \x20 --backend NAME   http (legacy) or local (in-process)\n\
                     \x20 --executor NAME  local tool executor: ptc (code mode, default) or native;\n\
                     \x20                  override the default with runtime.executor in ~/.lele/lrs-store.toml\n\
                     \x20 --debug          trace the loop to ~/.lele/lrs.log\n\n\
                     themes live in ~/.lele/themes/<name>.toml — switch with /theme",
                    default_port
                );
                return Ok(None);
            }
            "-r" | "--resume" => resume = true,
            "--no-spawn" => spawn = false,
            "--debug" => debug = true,
            "--backend" => {
                let value = it.next().context("--backend needs http or local")?;
                backend = match value.as_str() {
                    "http" => BackendKind::Http,
                    "local" => BackendKind::Local,
                    other => anyhow::bail!("unknown backend {other:?} — use http or local"),
                };
            }
            "--executor" => {
                let value = it.next().context("--executor needs native or ptc")?;
                if value != "native" && value != "ptc" {
                    anyhow::bail!("unknown executor {value:?} — use native or ptc");
                }
                executor = Some(value);
            }
            "--port" => {
                port = it
                    .next()
                    .context("--port needs a number")?
                    .parse()
                    .context("--port must be a number")?;
            }
            other => anyhow::bail!("unknown argument {other:?} — try --help"),
        }
    }
    Ok(Some(Args { port, spawn, resume, debug, backend, executor }))
}

fn main() -> Result<()> {
    let Some(args) = parse_args()? else { return Ok(()) };

    // Before anything else, so the trace covers startup too. Reported on the
    // normal console: once the alternate screen is up there is nowhere to say it.
    if args.debug {
        match app::log::enable() {
            Ok(path) => eprintln!("lrs: tracing to {}", path.display()),
            Err(e) => eprintln!("lrs: could not open the log: {e}"),
        }
    }

    // Connect before touching the terminal: if this fails the error belongs on a
    // normal console, not smeared across a half-initialised alternate screen.
    if args.backend == BackendKind::Http && args.executor.is_some() {
        anyhow::bail!("--executor applies to the local backend only; the server picks its own");
    }
    match args.backend {
        BackendKind::Local => eprintln!("lrs: starting in-process runtime…"),
        BackendKind::Http => eprintln!("lrs: connecting to lele serve on port {}…", args.port),
    }
    let (backend, spawned): (std::sync::Arc<dyn backend::Backend>, bool) = match args.backend {
        BackendKind::Local => {
            #[cfg(feature = "local-runtime")]
            {
                let home = std::path::PathBuf::from(std::env::var_os("LELE_HOME").unwrap_or_else(|| {
                    dirs::home_dir().unwrap_or_default().join(".lele").into_os_string()
                }));
                let project = std::env::current_dir().context("reading the working directory")?;
                let local = backend::LocalBackend::new(&home, &project, args.executor.as_deref())?;
                (std::sync::Arc::new(local), false)
            }
            #[cfg(not(feature = "local-runtime"))]
            {
                anyhow::bail!("local backend requires --features local-runtime");
            }
        }
        BackendKind::Http => {
            #[cfg(feature = "legacy-http")]
            {
                let (client, spawned) = net::ensure_server(args.port, args.spawn)?;
                (std::sync::Arc::new(backend::HttpBackend(client)), spawned)
            }
            #[cfg(not(feature = "legacy-http"))]
            {
                anyhow::bail!("http backend requires --features legacy-http");
            }
        }
    };
    let health = backend.health().unwrap_or_default();

    let mut term = setup()?;
    let mut application = app::App::new(backend, spawned, health);
    // Both are network reads on their own threads, so they overlap with the first
    // frame instead of delaying it.
    application.fetch_catalog();
    application.fetch_plugins();
    if args.resume {
        application.resume(None);
    }
    let result = app::run(&mut term, &mut application);
    // Restore before propagating: a raw-mode terminal left behind is unusable, so
    // teardown must not be skipped by the `?` on the loop's result.
    restore(&mut term)?;
    result
}

fn setup() -> Result<Terminal> {
    enable_raw_mode()?;
    let mut out = stdout();
    // Mouse capture is what makes drag-to-select work. It costs the terminal's own
    // selection while lrs is up — hold shift while dragging to get it back on
    // terminals that support the override.
    execute!(out, EnterAlternateScreen, EnableBracketedPaste, EnableMouseCapture)?;
    // Lets the terminal tell shift+enter apart from enter. Unsupported on some
    // terminals, so a failure here is not fatal — that binding just won't fire.
    if let Err(e) = execute!(
        out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    ) {
        app::log::line(format_args!("keyboard enhancement unavailable: {e}"));
    }
    restore_on_panic();
    let mut term = ratatui::Terminal::new(CrosstermBackend::new(out))?;
    term.clear()?;
    Ok(term)
}

/// A panic unwinds past `restore`, leaving the shell in raw mode with the mouse
/// captured — unusable until `reset`, and the panic message drawn on a screen
/// that is about to vanish. Undo the setup first, then let the default hook
/// print the message on the normal screen.
///
/// Only the main thread's panic ends the app. A worker that panics (a clipboard
/// read, a stream) leaves the UI running, and tearing the terminal down under it
/// would break the screen of an app that is still in use.
fn restore_on_panic() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            // Nothing to report a failure to but the trace: stdout is the very
            // thing being repaired.
            if let Err(e) = execute!(
                stdout(),
                PopKeyboardEnhancementFlags,
                DisableMouseCapture,
                LeaveAlternateScreen,
                DisableBracketedPaste
            ) {
                app::log::line(format_args!("terminal not restored after panic: {e}"));
            }
            if let Err(e) = disable_raw_mode() {
                app::log::line(format_args!("raw mode not left after panic: {e}"));
            }
        }
        default(info);
    }));
}

fn restore(term: &mut Terminal) -> Result<()> {
    // Mirrors the push in `setup`, which may have been refused: nothing to undo then.
    if let Err(e) = execute!(term.backend_mut(), PopKeyboardEnhancementFlags) {
        app::log::line(format_args!("keyboard enhancement not popped: {e}"));
    }
    // Leaving capture on would make the shell that follows unusable with a mouse.
    execute!(
        term.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        DisableBracketedPaste
    )?;
    disable_raw_mode()?;
    term.show_cursor()?;
    Ok(())
}
