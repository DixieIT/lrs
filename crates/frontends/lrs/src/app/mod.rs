//! Application state and the event loop.

pub mod clipboard;
pub mod commands;
pub mod config;
pub mod files;
pub mod history;
pub mod imgpath;
pub mod keys;
pub mod log;
pub mod paste;
pub mod resume;
pub mod run;
pub mod shell;
pub mod side;
pub mod user_commands;

use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use lrs::protocol::{ModelSet, Provider};
use lrs::protocol::{Event, RunRequest};
use crate::theme::{self, Theme, DEFAULT_THEME};
use crate::ui::ask::Ask;
use crate::ui::block::{self, Block, Level};
use crate::ui::picker::{self, Picker, Row};
use crate::ui::selection::Selection;
use crate::ui::spinner::Spinner;
use crate::ui::status::Status;
use crate::ui::viewer::Viewer;
use crate::ui::{chat, input::Editor};

use commands::Command;
use history::History;
use side::Side;

/// The one modal that can be open at a time. Every variant draws through
/// `ui::overlay`, and takes every keystroke while it is up.
pub enum Overlay {
    Pick(Picker),
    Ask(Ask),
    /// Read-only text, scrolled: `/context`.
    View(Viewer),
}

/// What the server offers, fetched once in the background at startup. The pickers
/// read from here, which is why opening one is instant.
#[derive(Default)]
pub struct Catalog {
    pub models: Vec<String>,
    /// The server's current default, marked in the picker.
    pub current: String,
    pub providers: Vec<Provider>,
    pub loaded: bool,
}

/// How long to block on terminal input before looping. Also the cadence at which
/// streamed events reach the screen, so it has to stay well under a frame.
const TICK: Duration = Duration::from_millis(30);

/// What the server can do, listed on startup. Sections with nothing in them are
/// dropped rather than shown empty — an "extensions: (none)" line is noise.
fn banner(h: &lrs::protocol::Health) -> Block {
    let sections = [
        ("tools", &h.tools),
        ("plugins", &h.plugins),
        ("extensions", &h.extensions),
        ("skills", &h.skills),
    ]
    .into_iter()
    .filter(|(_, items)| !items.is_empty())
    .map(|(label, items)| (label.to_string(), items.clone()))
    .collect();

    Block::Banner {
        title: "lrs — /help for commands".into(),
        sections,
    }
}

pub struct App {
    pub blocks: Vec<Block>,
    pub ed: Editor,
    pub theme: Theme,
    pub themes: BTreeMap<String, Theme>,
    pub status: Status,
    pub expand_all: bool,
    pub should_quit: bool,

    /// Scroll offset in wrapped lines; `follow` pins the view to the tail.
    pub scroll: u16,
    pub follow: bool,
    pub menu_sel: usize,

    /// The open modal, if any. While it is `Some` it takes every keystroke.
    pub overlay: Option<Overlay>,
    pub catalog: Catalog,
    /// Plugins and extensions discovered for this project.
    pub registry: lrs::protocol::Registry,
    pub registry_loaded: bool,
    /// Shell-style prompt recall on the up arrow, persisted per project.
    pub hist: History,
    /// Content the prompt shows as a chip: big pastes and clipboard images.
    pub pastes: paste::Pastes,
    /// Printable input not yet acted on, and when the last of it arrived.
    ///
    /// Everything typable waits here for one tick before it counts. That frame of
    /// delay is what lets a paste be recognised as a whole: the console delivers
    /// one in several chunks, and the first can be a single character — too
    /// little to tell from typing until the rest of it shows up.
    pub key_buf: Option<(String, std::time::Instant)>,
    /// The project's files, for `@path` completion. Scanned lazily — only once
    /// an `@` is actually being typed.
    pub files: files::Files,

    /// The live drag selection over the transcript, if any.
    pub selection: Option<Selection>,
    /// Where the transcript was drawn last frame, so a mouse position can be
    /// turned into a line and column. Only [`render`] writes it.
    pub transcript_rect: ratatui::layout::Rect,
    /// A short-lived confirmation line ("copied N characters"), and when it
    /// appeared. Deliberately not a Block: it disappears on its own, and a
    /// transcript entry for every copy would be noise.
    pub toast: Option<(String, std::time::Instant)>,

    /// Built-ins plus the commands the server's plugins registered.
    pub commands: commands::Table,
    /// Plugin-drawn panels, keyed by widget id.
    pub widgets: Vec<crate::ui::widgets::Widget>,
    /// Steered messages the running turn has not injected yet. The server says
    /// when each one is queued and when the loop takes it, so this is what is
    /// actually pending rather than a guess.
    pub queued: Vec<String>,
    /// A steer the turn ended before injecting. It becomes the next turn, started
    /// once the current one has fully wound down.
    pending_turn: Option<String>,

    client: std::sync::Arc<dyn crate::backend::Backend>,
    cwd: String,
    session_id: Option<String>,
    /// The turn that is running, if any. Asking it to stop marks it, and a second
    /// `^c` while it is marked leaves instead of asking again, so a stuck cancel
    /// never traps you.
    turn: Option<run::Turn>,
    /// The `!` or user command that is running, if any, with the same marking.
    shell: Option<run::ShellRun>,
    /// Commands the user defined as files; see [`user_commands`].
    user_commands: Vec<user_commands::UserCommand>,
    /// Commands run with `!` since the last turn: the next one tells the model.
    shell_ran: Vec<shell::Ran>,
    /// Herdr pane id, forwarded per-run so the herdr plugin in serve can report
    /// agent state to this pane. `None` outside herdr.
    pane_id: Option<String>,
    /// Results of off-thread work (catalog, key probe, session read). Kept for the
    /// whole lifetime of the app so a background thread always has somewhere to post.
    side_tx: Sender<Side>,
    side_rx: Receiver<Side>,
    model_override: Option<String>,
    effort: Option<String>,
    cfg: config::Config,

    /// Bumped by `touch()` on every content change; the wrap cache keys off it.
    revision: u64,
    cache: Vec<ratatui::text::Line<'static>>,
    cache_key: Option<(u64, u16, bool)>,

    /// The live checklist. One panel redrawn in place, not a block per call —
    /// the tool sends the whole list every time.

    /// The animated "working" line. Deliberately not a Block: it would bust the
    /// wrap cache on every frame, re-wrapping the whole transcript 30 times a
    /// second. It gets its own reserved row instead.
    spinner: Spinner,
}

impl App {
    pub fn new(client: std::sync::Arc<dyn crate::backend::Backend>, spawned: bool, health: lrs::protocol::Health) -> Self {
        let model = health.model.clone();
        let (themes, errors) = theme::load_all();
        let cfg = config::load();
        let wanted = cfg.theme.clone().unwrap_or_else(|| DEFAULT_THEME.to_string());
        let theme = themes
            .get(&wanted)
            .or_else(|| themes.get(DEFAULT_THEME))
            .cloned()
            .unwrap_or_else(Theme::tokyonight);

        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let cwd_short = std::path::Path::new(&cwd)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| cwd.clone());

        let mut blocks = Vec::new();
        if spawned {
            blocks.push(Block::Notice {
                text: "started lele serve in the background".into(),
                level: Level::Info,
            });
        }
        // Surface bad theme files rather than silently dropping them — a user who
        // just wrote one needs to know why it didn't show up.
        for e in errors {
            blocks.push(Block::Notice { text: e, level: Level::Warn });
        }
        blocks.push(banner(&health));
        if health.model.is_empty() {
            blocks.push(Block::Notice {
                text: "no provider connected — use /connect <provider> <key>".into(),
                level: Level::Warn,
            });
        }

        let status = Status {
            cwd: cwd_short,
            model: model.clone(),
            context_limit: health.context_limit,
            executor: health.executor.clone(),
            hide_context: !cfg.status.context,
            ..Default::default()
        };

        let (user_commands, user_errors) =
            user_commands::dir().map(|d| user_commands::load(&d)).unwrap_or_default();
        for e in user_errors {
            blocks.push(Block::Notice { text: e, level: Level::Warn });
        }
        let builtin = commands::Table::builtin();
        for name in builtin.clashes(&user_commands) {
            blocks.push(Block::Notice {
                text: format!("commands: {name} is already a command; its file is ignored"),
                level: Level::Warn,
            });
        }

        let (side_tx, side_rx) = mpsc::channel();
        App {
            blocks,
            ed: Editor::default(),
            theme,
            themes,
            status,
            expand_all: false,
            should_quit: false,
            scroll: 0,
            follow: true,
            menu_sel: 0,
            overlay: None,
            catalog: Catalog::default(),
            registry: Default::default(),
            registry_loaded: false,
            hist: History::load(&cwd),
            pastes: paste::Pastes::default(),
            key_buf: None,
            files: files::Files::default(),
            selection: None,
            transcript_rect: ratatui::layout::Rect::default(),
            toast: None,
            commands: builtin.with_user(&user_commands),
            widgets: Vec::new(),
            queued: Vec::new(),
            pending_turn: None,
            client,
            cwd,
            session_id: None,
            turn: None,
            shell: None,
            user_commands,
            shell_ran: Vec::new(),
            pane_id: std::env::var("HERDR_PANE_ID").ok().filter(|s| !s.is_empty()),
            side_tx,
            side_rx,
            model_override: None,
            effort: None,
            cfg,
            revision: 0,
            cache: Vec::new(),
            cache_key: None,
            spinner: Spinner::new(),
        }
    }

    pub fn busy(&self) -> bool {
        self.turn.is_some()
    }

    pub fn notice(&mut self, text: impl Into<String>, level: Level) {
        let text = text.into();
        // Don't stack a notice on top of itself. Anything driven by a keystroke
        // can fire many times in a row, and a screenful of the same warning
        // buries the conversation it was meant to annotate.
        if let Some(Block::Notice { text: last, .. }) = self.blocks.last() {
            if *last == text {
                self.follow = true;
                return;
            }
        }
        self.blocks.push(Block::Notice { text, level });
        self.follow = true;
        self.touch();
    }

    // ── submitting ────────────────────────────────────────────────────────────

    pub fn submit(&mut self) {
        let shell = self.ed.shell_mode();
        let typed = self.ed.take();
        // Sending always ends the command prompt. The `!` is put back in front of
        // the text, which is how the history keeps the command and how it is parsed.
        self.ed.leave_shell();
        if shell && typed.trim().is_empty() {
            return;
        }
        let raw = if shell { format!("!{typed}") } else { typed };
        if raw.trim().is_empty() {
            return;
        }
        // The expanded form goes in the history, because the blobs behind the
        // chips are released as soon as the turn starts. Storing `[Pasted 52
        // lines]` left the history holding a chip with nothing behind it, and
        // recalling it sent that literal string to the model.
        self.hist.push(&self.pastes.expand(&raw));
        self.touch();
        match commands::parse(&raw, &self.commands) {
            Command::Send(task) => self.start_turn(task),
            Command::Shell(command) => self.run_shell(command),
            Command::User { name, arg } => self.run_user_command(&name, &arg),
            Command::Image => self.paste_image(),
            Command::Plugin { name, arg } => self.run_plugin_command(&name, &arg),
            Command::Registry { plugins } => self.open_registry_picker(plugins),
            Command::Reload => self.reload(),
            Command::Help => self.open_help_picker(),
            Command::Quit => self.should_quit = true,
            Command::Context => self.open_context(),
            Command::Expand => {
                self.expand_all = !self.expand_all;
                let s = if self.expand_all { "expanded" } else { "collapsed" };
                self.notice(format!("tool output {s}"), Level::Info);
            }
            Command::Clear => {
                self.session_id = None;
                self.blocks.clear();
                // Plugin panels belong to the conversation that set them; keeping
                // them would show the old session's goal — or its checklist —
                // above a fresh one.
                self.widgets.clear();
                self.status.context = 0;
                self.notice("new session", Level::Info);
            }
            Command::Theme(arg) => match arg {
                Some(name) => self.set_theme(&name),
                None => self.open_theme_picker(),
            },
            Command::Model(arg) => match arg {
                // A bare name is matched against the catalog the same way the
                // picker's search does, so `/model luna` finds the full ref.
                Some(m) => match self.resolve_model(&m) {
                    Some(full) => self.choose_model(&full),
                    None => self.choose_model(&m),
                },
                None => self.open_model_picker(),
            },
            Command::Connect(arg) => match arg {
                Some(name) => self.open_connect(&name),
                None => self.open_provider_picker(),
            },
            Command::Resume(id) => self.resume(id),
            Command::Effort(arg) => match arg.as_deref().map(commands::normalise_effort) {
                Some(Some(v)) => self.set_effort(&v),
                Some(None) => self.notice("effort: low|medium|high|max|off", Level::Warn),
                None => self.open_effort_picker(),
            },
        }
    }

    fn set_effort(&mut self, v: &str) {
        self.status.effort = v.to_string();
        self.effort = Some(v.to_string());
        let shown = if v.is_empty() { "off" } else { v };
        self.notice(format!("effort → {shown}"), Level::Info);
    }

    fn set_theme(&mut self, name: &str) {
        match self.themes.get(name).cloned() {
            Some(t) => {
                self.theme = t;
                // Styles are baked into the cached lines, so they must be rebuilt.
                self.touch();
                self.cfg.theme = Some(name.to_string());
                if let Err(e) = config::save(&self.cfg) {
                    self.notice(format!("theme applied but not saved: {e}"), Level::Warn);
                } else {
                    self.notice(format!("theme → {name}"), Level::Info);
                }
            }
            None => self.notice(format!("no theme {name:?} — /theme to pick one"), Level::Warn),
        }
    }

    // ── pickers ───────────────────────────────────────────────────────────────

    /// Fetch the model and provider lists in the background. Called once at
    /// startup so `/model` and `/connect` open on data that is already there;
    /// resolving the catalog can take seconds against a cold server.
    pub fn fetch_catalog(&self) {
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        std::thread::spawn(move || {
            let (models, current, err) = match client.models() {
                Ok(m) => (m.models, m.current, None),
                Err(e) => (Vec::new(), String::new(), Some(e.to_string())),
            };
            let providers = client.providers().unwrap_or_default();
            side::deliver(&tx, Side::Catalog { models, current, providers, error: err });
        });
    }

    // ── plugins ───────────────────────────────────────────────────────────────

    /// Pull the plugin command table and the current widget state. Both live in
    /// the server's process — the plugins are loaded there, not here — so this is
    /// the only way a frontend learns that `/goal` exists at all.
    pub fn fetch_plugins(&self) {
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        let session = self.session_id.clone().unwrap_or_default();
        std::thread::spawn(move || {
            let commands = client.commands().unwrap_or_default();
            // Only this session's panels: the server hosts other conversations,
            // and their widgets are not ours to draw.
            let widgets = client.widgets(&session).unwrap_or_default();
            side::deliver(&tx, Side::Plugins { commands, widgets });
        });
    }

    /// Ask the backend what the next turn would hand the model; the answer
    /// opens in a popup. Off the draw thread, like every backend call.
    fn open_context(&mut self) {
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        let session = self.session_id.clone().unwrap_or_default();
        std::thread::spawn(move || {
            let result = client.run_command("/context", "", &session).map_err(|e| e.to_string());
            side::deliver(&tx, Side::Context(result));
        });
    }

    /// Hand a plugin command to the server and show what it printed.
    fn run_plugin_command(&mut self, name: &str, arg: &str) {
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        let session = self.session_id.clone().unwrap_or_default();
        let (name, arg) = (name.to_string(), arg.to_string());
        std::thread::spawn(move || {
            let out = client.run_command(&name, &arg, &session).map_err(|e| e.to_string());
            side::deliver(&tx, Side::Command { name, result: out });
        });
    }

    fn set_widget(&mut self, id: &str, markup: Option<String>) {
        crate::ui::widgets::upsert(&mut self.widgets, id, markup);
    }

    /// Model rows grouped by provider, the way the Textual picker groups them.
    /// openrouter goes last: its catalog is 300+ entries and would otherwise bury
    /// the small configured groups below the fold.
    fn model_rows(&self) -> Vec<Row> {
        let mut groups: Vec<(String, Vec<&String>)> = Vec::new();
        for r in &self.catalog.models {
            let (prov, _) = lrs::protocol::split_ref(r, &self.catalog.providers);
            match groups.iter_mut().find(|(name, _)| *name == prov) {
                Some((_, refs)) => refs.push(r),
                None => groups.push((prov, vec![r])),
            }
        }
        groups.sort_by_key(|(name, _)| name == "openrouter");

        let active = self.active_model();
        let mut rows = Vec::new();
        for (prov, refs) in groups {
            let url = self
                .catalog
                .providers
                .iter()
                .find(|p| p.name == prov)
                .map(|p| p.base_url.clone())
                .unwrap_or_default();
            rows.push(Row::heading(prov.clone()).hay(format!("{prov} {url}")));
            for r in refs {
                let (_, mid) = lrs::protocol::split_ref(r, &self.catalog.providers);
                let mut row = Row::item(r.clone(), format!("  {mid}")).hay(r.clone());
                if *r == active {
                    row = row.note("← current");
                }
                rows.push(row);
            }
        }
        rows
    }

    fn provider_rows(&self) -> Vec<Row> {
        self.catalog
            .providers
            .iter()
            .map(|p| Row::item(p.name.clone(), p.name.clone()).hay(p.base_url.clone()).dot(p.connected))
            .collect()
    }

    /// The model this frontend is actually sending, which is the override if one
    /// was picked here and the server's default otherwise.
    fn active_model(&self) -> String {
        self.model_override.clone().unwrap_or_else(|| {
            // The catalog is fresher than the health probe taken at startup: another
            // client may have moved the server's default since.
            if self.catalog.current.is_empty() {
                self.status.model.clone()
            } else {
                self.catalog.current.clone()
            }
        })
    }

    pub fn open_model_picker(&mut self) {
        let p = if self.catalog.loaded {
            Picker::new(picker::Kind::Model, "Select model", self.model_rows())
        } else {
            Picker::pending(picker::Kind::Model, "Select model")
        };
        self.overlay = Some(Overlay::Pick(p));
    }

    pub fn open_provider_picker(&mut self) {
        let p = if self.catalog.loaded {
            Picker::new(picker::Kind::Provider, "Select provider", self.provider_rows())
        } else {
            Picker::pending(picker::Kind::Provider, "Select provider")
        };
        self.overlay = Some(Overlay::Pick(p));
    }

    pub fn open_effort_picker(&mut self) {
        let current = self.effort.clone().unwrap_or_default();
        let rows = ["low", "medium", "high", "max", "off"]
            .into_iter()
            .map(|v| {
                let is_current = if v == "off" { current.is_empty() } else { v == current };
                Row::item(v, v).dot(is_current)
            })
            .collect();
        self.overlay = Some(Overlay::Pick(Picker::new(
            picker::Kind::Effort,
            "Reasoning effort",
            rows,
        )));
    }

    pub fn open_theme_picker(&mut self) {
        let current = self.theme.name.clone();
        let rows = self
            .themes
            .keys()
            .map(|n| Row::item(n.clone(), n.clone()).dot(*n == current))
            .collect();
        self.overlay = Some(Overlay::Pick(Picker::new(picker::Kind::Theme, "Select theme", rows)));
    }

    /// The plugin/extension list, as picker rows. A filled dot means "loaded right
    /// now", the note carries the configured state — the two differ between an
    /// enable and the reload that applies it, and hiding that would make the
    /// picker look broken.
    fn registry_rows(&self, plugins: bool) -> Vec<Row> {
        let list = if plugins { &self.registry.plugins } else { &self.registry.extensions };
        list.iter()
            .map(|e| {
                let note = match (e.state.as_str(), e.live) {
                    ("disabled", true) => "disabled · /reload to apply".to_string(),
                    ("disabled", false) => "disabled".to_string(),
                    (s, false) if s != "disabled" => format!("{s} · /reload to apply"),
                    ("available", true) => "available".to_string(),
                    _ => String::new(),
                };
                Row::item(e.name.clone(), e.name.clone())
                    .hay(e.path.clone())
                    .note(note)
                    .dot(e.live)
            })
            .collect()
    }

    pub fn open_registry_picker(&mut self, plugins: bool) {
        let kind = picker::Kind::Registry { plugins };
        let title = if plugins { "Plugins — enter toggles" } else { "Extensions — enter toggles" };
        let rows = self.registry_rows(plugins);
        self.overlay = Some(Overlay::Pick(if self.registry_loaded {
            Picker::new(kind, title, rows)
        } else {
            Picker::pending(kind, title)
        }));
        if !self.registry_loaded {
            self.fetch_registry();
        }
    }

    pub fn fetch_registry(&self) {
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        let cwd = self.cwd.clone();
        std::thread::spawn(move || {
            side::deliver(&tx, Side::Registry(client.registry(&cwd).map_err(|e| e.to_string())));
        });
    }

    /// Flip one entry between active and disabled, then refresh the list.
    fn toggle_registry(&mut self, plugins: bool, name: &str) {
        let list = if plugins { &self.registry.plugins } else { &self.registry.extensions };
        let currently_off = list
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.state == "disabled")
            .unwrap_or(false);
        let state = if currently_off { "active" } else { "disabled" };
        let kind = if plugins { "plugins" } else { "extensions" };
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        let cwd = self.cwd.clone();
        let (kind, name, state) = (kind.to_string(), name.to_string(), state.to_string());
        std::thread::spawn(move || {
            // Global config, not the project's: a plugin you turn off is normally
            // off everywhere, and --project is the exception, not the default.
            let out = client
                .set_registry(&kind, &name, &state, false, &cwd)
                .map(|r| r.message)
                .map_err(|e| e.to_string());
            side::deliver(&tx, Side::RegistrySet(out));
        });
    }

    /// Re-register plugins and extensions on the server.
    pub fn reload(&mut self) {
        self.reload_user_commands();
        self.notice("reloading plugins and extensions…", Level::Info);
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        let cwd = self.cwd.clone();
        std::thread::spawn(move || {
            side::deliver(&tx, Side::Reloaded(client.reload(&cwd).map_err(|e| e.to_string())));
        });
    }

    /// Ask for a provider's API key. Unknown names are refused here rather than
    /// after the user has typed a secret into a prompt that was never going to work.
    pub fn open_connect(&mut self, provider: &str) {
        let Some(p) = self.catalog.providers.iter().find(|p| p.name == provider) else {
            if !self.catalog.loaded {
                self.notice("still loading providers — try again in a moment", Level::Warn);
            } else {
                let known: Vec<&str> =
                    self.catalog.providers.iter().map(|p| p.name.as_str()).collect();
                self.notice(
                    format!("unknown provider {provider:?} — known: {}", known.join(", ")),
                    Level::Warn,
                );
            }
            return;
        };
        self.overlay = Some(Overlay::Ask(Ask::connect(&p.name, &p.api_key_env, p.connected)));
    }

    /// Persist a model choice. The write and the context-limit lookup both go
    /// through the server, so they run off-thread; the override applies at once so
    /// the very next turn already uses the new model.
    fn choose_model(&mut self, ref_: &str) {
        self.model_override = Some(ref_.to_string());
        self.status.model = ref_.to_string();
        self.notice(format!("model → {ref_}"), Level::Info);
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        let ref_ = ref_.to_string();
        std::thread::spawn(move || {
            let out = client.set_model(&ref_).map_err(|e| e.to_string());
            side::deliver(&tx, Side::ModelSet(out));
        });
    }

    /// Match a bare argument against the catalog: exact ref first, then substring.
    fn resolve_model(&self, arg: &str) -> Option<String> {
        let q = arg.to_lowercase();
        self.catalog
            .models
            .iter()
            .find(|m| m.eq_ignore_ascii_case(arg))
            .or_else(|| self.catalog.models.iter().find(|m| m.to_lowercase().contains(&q)))
            .cloned()
    }

    /// Hand the key to the server, which validates it against the provider before
    /// saving. That probe is a remote call, hence the thread.
    fn connect(&mut self, provider: &str, key: &str) {
        self.notice(format!("validating key against {provider}…"), Level::Info);
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        let (provider, key) = (provider.to_string(), key.to_string());
        std::thread::spawn(move || {
            let error = client.connect(&provider, &key).err().map(|e| e.to_string());
            side::deliver(&tx, Side::Connected { provider, error });
        });
    }

    // ── resume ────────────────────────────────────────────────────────────────

    /// Reopen this project's most recent session. Both the lookup and the
    /// transcript read hit the server, so the work happens on a thread and the
    /// blocks arrive through [`Side::Resumed`].
    /// Reopen a session: the one named, or the latest of this project.
    pub fn resume(&mut self, id: Option<String>) {
        if self.busy() {
            self.notice("a turn is running — ^c first", Level::Warn);
            return;
        }
        let what = id.as_deref().map_or("the last session".to_string(), |id| format!("session {id}"));
        self.notice(format!("resuming {what}…"), Level::Info);
        let client = self.client.clone();
        let tx = self.side_tx.clone();
        let cwd = self.cwd.clone();
        std::thread::spawn(move || {
            let msg = match id {
                Some(id) => client.session(&id)
                    .map_err(|e| format!("could not read {id}: {e}"))
                    .and_then(|s| if s.messages.is_empty() {
                        Err(format!("no session {id} with messages"))
                    } else {
                        Ok(s)
                    }),
                None => match client.sessions(&cwd) {
                    Err(e) => Err(format!("could not list sessions: {e}")),
                    Ok(rows) => match rows.into_iter().find(|r| r.message_count > 0) {
                        None => Err("no earlier session in this project".to_string()),
                        Some(row) => client
                            .session(&row.id)
                            .map_err(|e| format!("could not read {}: {e}", row.id)),
                    },
                },
            };
            let side = match msg {
                Ok(session) => {
                    let blocks = resume::blocks(&session);
                    Side::Resumed {
                        summary: resume::summary(&session, &blocks),
                        session_id: session.session_id.clone(),
                        blocks,
                        error: None,
                    }
                }
                Err(e) => Side::Resumed {
                    session_id: String::new(),
                    summary: String::new(),
                    blocks: Vec::new(),
                    error: Some(e),
                },
            };
            side::deliver(&tx, side);
        });
    }

    // ── overlay ───────────────────────────────────────────────────────────────

    pub fn overlay_open(&self) -> bool {
        self.overlay.is_some()
    }

    pub fn close_overlay(&mut self) {
        self.overlay = None;
    }

    /// Act on whatever the open modal was asking for, then close it.
    pub fn overlay_submit(&mut self) {
        let Some(ov) = self.overlay.take() else { return };
        match ov {
            Overlay::Pick(p) => {
                let Some(id) = p.selected().map(str::to_string) else {
                    // Nothing matches the query: keep the modal up rather than
                    // silently dismissing it and losing what was typed.
                    self.overlay = Some(Overlay::Pick(p));
                    return;
                };
                match p.kind {
                    picker::Kind::Model => self.choose_model(&id),
                    picker::Kind::Provider => self.open_connect(&id),
                    picker::Kind::Effort => {
                        let v = if id == "off" { String::new() } else { id };
                        self.set_effort(&v);
                    }
                    picker::Kind::Theme => self.set_theme(&id),
                    picker::Kind::Registry { plugins } => {
                        self.toggle_registry(plugins, &id);
                        // Toggling several in a row is the normal case, so the
                        // picker stays up; esc closes it.
                        self.overlay = Some(Overlay::Pick(p));
                    }
                    // Into the prompt, not run: the list holds /clear and /quit.
                    // `complete` also marks any `<placeholder>` in it.
                    picker::Kind::Help => self.ed.complete(&format!("{id} ")),
                }
            }
            // Enter closes a read-only view, like esc.
            Overlay::View(_) => {}
            Overlay::Ask(a) => {
                if a.value().is_empty() {
                    self.notice("connect cancelled", Level::Info);
                    return;
                }
                let crate::ui::ask::Kind::ConnectKey { provider, .. } = &a.kind;
                let provider = provider.clone();
                self.connect(&provider, a.value());
            }
        }
    }

    /// Read the clipboard image and drop a chip for it into the prompt. The read
    /// and the PNG encode both take long enough to stutter the draw loop, so they
    /// happen on a thread.
    /// A paste of image paths becomes attached pictures. True if it was one, in
    /// which case nothing goes into the prompt as text.
    ///
    /// This is the route that works everywhere: copying a file, or dropping it on
    /// the window, makes the terminal paste its *path*, and that text arrives no
    /// matter which keys the terminal has claimed for itself.
    ///
    /// Only the parsing happens here. Whether the files exist is decided on the
    /// thread that reads them — `is_file()` against a UNC path to a host that is
    /// down blocks for tens of seconds, and this is the thread that draws.
    pub fn attach_pasted_images(&mut self, text: &str) -> bool {
        let paths = imgpath::candidates(text);
        if paths.is_empty() {
            return false;
        }
        log::trace!("paste looks like {} image path(s)", paths.len());
        let tx = self.side_tx.clone();
        std::thread::spawn(move || {
            let _s = log::risky("reading pasted images");
            for p in paths {
                let out = imgpath::read_as_data_url(&p).map_err(|e| format!("{e:#}"));
                if tx.send(Side::Image(out)).is_err() {
                    return;
                }
            }
        });
        true
    }

    pub fn paste_image(&mut self) {
        let tx = self.side_tx.clone();
        std::thread::spawn(move || {
            // Named before the call: "the clipboard had no image" and "the read
            // never returned" look identical from the outside.
            let _s = log::risky("clipboard image read");
            let out = clipboard::image_data_url().map_err(|e| e.to_string());
            log::trace!("clipboard image: {}", match &out {
                Ok(u) => format!("ok, {} bytes of data url", u.len()),
                Err(e) => format!("failed: {e}"),
            });
            side::deliver(&tx, Side::Image(out));
        });
    }

    fn start_turn(&mut self, shown: String) {
        self.send_turn(shown, true);
    }

    /// `echo` is false for a leftover steer: it was already put in the transcript
    /// when the user sent it, and showing it twice would suggest they said it twice.
    fn send_turn(&mut self, shown: String, echo: bool) {
        if self.busy() {
            self.steer(shown);
            return;
        }
        // What the transcript shows keeps the chips; what the model receives has
        // the pastes expanded and the images attached as their own content parts.
        let images = self.pastes.images_in(&shown);
        // What the user ran with `!` goes in front of what they say, and is kept
        // until the turn has really started.
        let ran = std::mem::take(&mut self.shell_ran);
        let text = format!("{}{}", shell::preface(&ran), self.pastes.expand(&shown)).trim_end().to_string();
        let task = if images.is_empty() {
            serde_json::Value::String(text)
        } else {
            let mut parts = Vec::new();
            if !text.trim().is_empty() {
                parts.push(serde_json::json!({ "type": "text", "text": text }));
            }
            for url in images {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": url },
                }));
            }
            serde_json::Value::Array(parts)
        };
        // Before `retain_used` drops the blobs: the transcript shows the opening
        // of each paste, not its chip. `[Pasted 52 lines]` above the answer can't
        // tell you whether you sent the right 52 lines.
        let echoed = self.pastes.preview(&shown);
        self.pastes.retain_used("");
        if echo {
            self.blocks.push(Block::User(echoed));
        }
        self.follow = true;

        let req = RunRequest {
            task,
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            model: self.model_override.clone(),
            effort: self.effort.clone(),
            pane_id: self.pane_id.clone(),
        };
        match self.client.start_run(&req) {
            Ok(resp) => {
                self.session_id = Some(resp.session_id.clone());
                self.status.session = resp.session_id;
                self.status.busy = true;

                let (tx, rx) = mpsc::channel();
                self.turn = Some(run::Turn { job_id: resp.job_id.clone(), events: rx, stopping: false });
                self.spinner.restart();
                let client = self.client.clone();
                std::thread::spawn(move || {
                    let failed = tx.clone();
                    if let Err(e) = client.stream_events(&resp.job_id, tx) {
                        // The stream broke before the job ended: end the turn
                        // with the reason instead of leaving it spinning.
                        let error = format!("lost the event stream: {e}");
                        if failed.send(lrs::protocol::Event::JobFailed { error }).is_err() {
                            log::line(format_args!("event stream lost after the turn was closed"));
                        }
                    }
                });
            }
            Err(e) => {
                self.shell_ran = ran;
                let base = self.client.base().to_string();
                self.notice(format!("could not start the turn via {base}: {e}"), Level::Error)
            }
        }
    }

    /// Send a message into the turn that is already running.
    ///
    /// The old behaviour — refusing with "a turn is already running" — was wrong
    /// twice over: a turn heading the wrong way is exactly when you most want to
    /// say so, and every refused enter left another warning in the transcript.
    ///
    /// Images can't steer: the engine injects a steer as a plain user message, so
    /// a picture needs a turn of its own. It stays in the prompt.
    fn steer(&mut self, shown: String) {
        let images = self.pastes.images_in(&shown);
        if !images.is_empty() {
            self.ed.set(&shown);
            self.notice("an image needs its own turn — ^c to cancel this one", Level::Warn);
            return;
        }
        let Some(job) = self.turn.as_ref().map(|t| t.job_id.clone()) else {
            self.notice("no running turn to steer", Level::Warn);
            return;
        };
        let text = self.pastes.expand(&shown);
        self.blocks.push(Block::User(self.pastes.preview(&shown)));
        self.pastes.retain_used("");
        self.follow = true;
        self.touch();

        // Off-thread like every other call: the UI must keep streaming the turn
        // this is steering.
        let (client, tx) = (self.client.clone(), self.side_tx.clone());
        std::thread::spawn(move || {
            side::deliver(&tx, Side::Steered(client.steer(&job, &text).map_err(|e| e.to_string())));
        });
    }

    /// `/help` as a list you can search rather than a wall of text.
    ///
    /// The commands are pickable and go into the prompt rather than running — the
    /// list holds `/clear` and `/quit`, and browsing help is no place to lose a
    /// session to a stray enter. The key bindings are there to be read.
    fn open_help_picker(&mut self) {
        let width = self.commands.entries.iter().map(|e| e.name.chars().count()).max().unwrap_or(8);
        let mut rows = vec![Row::heading("commands")];
        rows.extend(self.commands.entries.iter().map(|e| {
            Row::item(e.name.clone(), format!("{:<width$}", e.name))
                .note(e.desc.clone())
                .hay(e.desc.clone())
        }));
        rows.push(Row::heading("keys"));
        let kw = commands::KEYS.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(8);
        rows.extend(
            commands::KEYS.iter().map(|(k, d)| Row::info(format!("{k:<kw$}")).note(*d).hay(*d)),
        );
        self.overlay = Some(Overlay::Pick(Picker::new(picker::Kind::Help, "Help", rows)));
    }

    /// Put a recalled prompt back where it can be edited and sent again.
    ///
    /// The history holds the real text, so a big one is re-collapsed on the way
    /// in — otherwise the up arrow would drop four hundred lines into the prompt,
    /// which is the thing chips exist to prevent.
    pub fn recall(&mut self, entry: &str) {
        // A picture is released with the turn it was sent in, so an image chip in
        // a recalled prompt is an empty label. Sending it would put the literal
        // text "[Image #1]" in front of the model — a turn that looks like it
        // carries a screenshot and doesn't.
        let (shell, entry) = match entry.strip_prefix('!') {
            Some(command) => (true, command),
            None => (false, entry),
        };
        let (entry, dropped) = self.pastes.without_dead_images(entry);
        if dropped > 0 {
            let s = if dropped == 1 { "image is" } else { "images are" };
            self.notice(format!("{dropped} {s} not kept in history — ^v to attach again"), Level::Warn);
        }
        if paste::Pastes::should_collapse(&entry) {
            let chip = self.pastes.add_text(&entry);
            self.ed.set(&chip);
        } else {
            self.ed.set(&entry);
        }
        if shell {
            self.ed.enter_shell();
        } else {
            self.ed.leave_shell();
        }
    }

    /// `!cmd`: run it, and tell the model what it printed.
    fn run_shell(&mut self, command: String) {
        self.start_shell(shell::Job { title: shell_title(&command), command, to_model: true });
    }

    /// `/name args` for a command the user defined.
    fn run_user_command(&mut self, name: &str, arg: &str) {
        let Some(command) = self.user_commands.iter().find(|c| c.name == name).cloned() else {
            self.notice(format!("{name} is no longer defined — /reload re-reads the files"), Level::Warn);
            return;
        };
        self.start_shell(shell::Job {
            title: format!("{name} {}", arg.trim()).trim_end().to_string(),
            command: command.command_line(arg),
            to_model: command.to_model,
        });
    }

    /// Re-read `~/.lele/lrs/commands`, so an edited or new file counts without
    /// restarting lrs.
    fn reload_user_commands(&mut self) {
        let (found, errors) = user_commands::dir()
            .map(|d| user_commands::load(&d))
            .unwrap_or_default();
        for e in errors {
            self.notice(e, Level::Warn);
        }
        let table = self.commands.clone().with_user(&[]);
        for name in table.clashes(&found) {
            self.notice(format!("commands: {name} is already a command; its file is ignored"), Level::Warn);
        }
        self.commands = table.with_user(&found);
        self.user_commands = found;
    }

    /// Run a command off the UI thread, in the project directory. One at a time:
    /// its block is how the result finds its place in the transcript.
    fn start_shell(&mut self, job: shell::Job) {
        if self.shell.is_some() {
            self.notice("a command is already running — esc to stop it", Level::Warn);
            return;
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.shell = Some(run::ShellRun { stop: stop.clone(), stopping: false });
        self.blocks.push(Block::Tool {
            title: job.title.clone(),
            body: None,
            kind: block::body_kind("bash"),
            failed: false,
            started: Some(std::time::Instant::now()),
            took: None,
        });
        self.follow = true;
        self.touch();
        let (tx, cwd) = (self.side_tx.clone(), self.cwd.clone());
        std::thread::spawn(move || {
            let result = shell::run(&cwd, &job.command, &stop);
            side::deliver(&tx, Side::Shell { job, result });
        });
    }

    /// Fill the command's block. A finished command that is for the model is sent
    /// to it as a turn of its own; one that was stopped or never started told it
    /// nothing.
    fn shell_finished(&mut self, job: shell::Job, result: Result<shell::ShellResult, String>) {
        use shell::ShellResult;
        self.shell = None;
        let (text, failed) = match &result {
            Ok(ShellResult::Finished { text, failed }) => (text.clone(), *failed),
            Ok(ShellResult::Stopped) => ("(stopped)".to_string(), true),
            Err(e) => (e.clone(), true),
        };
        let slot = self.blocks.iter_mut().rev().find(
            |b| matches!(b, Block::Tool { title: t, body: None, .. } if *t == job.title),
        );
        if let Some(Block::Tool { body, failed: slot_failed, started, took, .. }) = slot {
            *body = Some(text.clone());
            *slot_failed = failed;
            *took = started.map(|at| at.elapsed());
        }
        if let Ok(ShellResult::Finished { .. }) = result {
            if job.to_model {
                self.shell_ran.push(shell::Ran { command: job.command, output: text });
                // The output is the message: send it now instead of waiting for
                // the next one. While a turn runs it is kept for the next turn.
                if !self.busy() {
                    self.send_turn(String::new(), false);
                }
            }
        }
        self.follow = true;
        // The transcript is redrawn only when its revision moves: without this
        // the output stays unseen until the next keystroke happens to do it.
        self.touch();
    }

    /// A turn or a `!` command is running: what `esc` and `^c` act on.
    pub fn working(&self) -> bool {
        self.busy() || self.shell.is_some()
    }

    pub fn cancel(&mut self) {
        let mut asked = false;
        if let Some(run) = &mut self.shell {
            run.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            run.stopping = true;
            asked = true;
        }
        if let Some(turn) = &mut self.turn {
            turn.stopping = true;
            asked = true;
            // ^c is the key you press when something is already wrong, so it is
            // the last one that may block on the server answering.
            let client = self.client.clone();
            let tx = self.side_tx.clone();
            let job = turn.job_id.clone();
            std::thread::spawn(move || {
                if let Err(e) = client.cancel(&job) {
                    side::deliver(&tx, Side::CancelFailed(e.to_string()));
                }
            });
        }
        if asked {
            self.notice("cancelling…", Level::Warn);
        }
    }

    /// A stop was requested for the turn or command that is still running.
    pub fn cancelling(&self) -> bool {
        self.turn.as_ref().is_some_and(|t| t.stopping)
            || self.shell.as_ref().is_some_and(|s| s.stopping)
    }

    // ── streaming ─────────────────────────────────────────────────────────────

    /// Drain everything queued without blocking, so a fast stream doesn't render
    /// one token per frame.
    pub fn pump(&mut self) {
        self.expire_toast();
        self.pump_side();
        self.refresh_files();
        let Some(turn) = &self.turn else { return };
        let mut done = false;
        let events: Vec<Event> = turn.events.try_iter().collect();
        if events.is_empty() {
            return;
        }
        for ev in events {
            if self.apply(ev) {
                done = true;
            }
        }
        self.touch();
        if done {
            self.turn = None;
            self.status.busy = false;
            // Only now, with `busy()` false again, or it would steer itself into
            // the job that just finished.
            if let Some(task) = self.pending_turn.take() {
                self.send_turn(task, false);
            }
        }
    }

    /// Rescan the project only while an `@path` is being typed.
    ///
    /// The scan shells out to git, so it runs off-thread and only when it can
    /// matter: a session that never types an `@` never pays for one, and the
    /// 5s TTL keeps a long editing session from re-scanning per keystroke.
    fn refresh_files(&mut self) {
        if !self.files.stale()
            || crate::ui::input::at_token(self.ed.text(), self.ed.cursor()).is_none()
        {
            return;
        }
        self.files.inflight = true;
        let (root, tx) = (self.cwd.clone(), self.side_tx.clone());
        std::thread::spawn(move || {
            side::deliver(&tx, Side::Files(files::scan(&root)));
        });
    }

    /// Results of background work. Runs every tick, turn or no turn.
    fn pump_side(&mut self) {
        for msg in self.side_rx.try_iter().collect::<Vec<_>>() {
            match msg {
                Side::Catalog { models, current, providers, error } => {
                    self.catalog = Catalog { models, current, providers, loaded: true };
                    if let Some(e) = error {
                        self.notice(format!("could not list models: {e}"), Level::Warn);
                    }
                    // A picker opened before the fetch landed is still on screen
                    // waiting; fill it in place rather than making the user reopen it.
                    let rows = match self.overlay {
                        Some(Overlay::Pick(ref p)) if p.loading => match p.kind {
                            picker::Kind::Model => Some(self.model_rows()),
                            picker::Kind::Provider => Some(self.provider_rows()),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let (Some(rows), Some(Overlay::Pick(p))) = (rows, self.overlay.as_mut()) {
                        p.set_rows(rows);
                    }
                }
                Side::ModelSet(Ok(ModelSet { model, missing_key_env, context_limit })) => {
                    if context_limit > 0 {
                        self.status.context_limit = context_limit;
                    }
                    // The pick is saved but unusable until its provider has a key,
                    // so go straight to asking for one — the alternative is a 401
                    // on the next turn with no explanation.
                    if let Some(env) = missing_key_env {
                        let (prov, _) =
                            lrs::protocol::split_ref(&model, &self.catalog.providers);
                        self.notice(
                            format!("{prov} needs {env} — connecting"),
                            Level::Warn,
                        );
                        self.overlay = Some(Overlay::Ask(Ask::connect(&prov, &env, false)));
                    }
                }
                Side::ModelSet(Err(e)) => {
                    // The override still applies to this session's turns; only the
                    // "remember it" half failed.
                    self.notice(format!("model not saved as the default: {e}"), Level::Warn);
                }
                Side::Connected { provider, error } => match error {
                    Some(e) => {
                        self.notice(format!("{provider}: {e}"), Level::Error);
                        self.open_connect(&provider); // straight back to the prompt
                    }
                    None => {
                        self.notice(format!("connected — {provider}"), Level::Info);
                        // Its catalog and its dot both just changed.
                        self.fetch_catalog();
                    }
                },
                Side::Resumed { blocks, session_id, summary, error } => match error {
                    Some(e) => self.notice(e, Level::Warn),
                    None => {
                        self.blocks = blocks;
                        self.session_id = Some(session_id.clone());
                        self.status.session = session_id;
                        self.notice(summary, Level::Info);
                        self.follow = true;
                        self.touch();
                        // Panels belong to the session we just adopted, not to
                        // the empty one we started with.
                        self.widgets.clear();
                        self.fetch_plugins();
                    }
                },
                Side::Image(Ok(url)) => {
                    let chip = self.pastes.add_image(url);
                    // A space after it, so the next thing typed isn't glued to the
                    // label and swallowed by the chip's own text.
                    self.ed.insert_str(&format!("{chip} "));
                }
                Side::Image(Err(e)) => self.notice(format!("no image pasted: {e}"), Level::Warn),
                Side::Registry(result) => match result {
                    Err(e) => self.notice(format!("could not list plugins: {e}"), Level::Warn),
                    Ok(reg) => {
                        self.registry = reg;
                        self.registry_loaded = true;
                        // Fill a picker that was opened before the list landed.
                        let rows = match self.overlay {
                            Some(Overlay::Pick(ref p)) => match p.kind {
                                picker::Kind::Registry { plugins } => {
                                    Some(self.registry_rows(plugins))
                                }
                                _ => None,
                            },
                            _ => None,
                        };
                        if let (Some(rows), Some(Overlay::Pick(p))) = (rows, self.overlay.as_mut())
                        {
                            p.set_rows(rows);
                        }
                    }
                },
                Side::RegistrySet(result) => match result {
                    Err(e) => self.notice(format!("could not change it: {e}"), Level::Error),
                    Ok(msg) => {
                        self.notice(msg, Level::Info);
                        self.fetch_registry(); // the list just changed under us
                    }
                },
                Side::Reloaded(result) => match result {
                    Err(e) => self.notice(format!("reload failed: {e}"), Level::Error),
                    Ok(reg) => {
                        if let Some(e) = &reg.error {
                            self.notice(format!("reload failed: {e}"), Level::Error);
                        } else {
                            let live = reg.plugins.iter().filter(|p| p.live).count()
                                + reg.extensions.iter().filter(|e| e.live).count();
                            self.notice(format!("reloaded — {live} active"), Level::Info);
                        }
                        self.registry = reg;
                        self.registry_loaded = true;
                        // A reload can add or remove commands and panels.
                        self.fetch_plugins();
                    }
                },
                Side::Files(list) => self.files.set(list),
                // Only the failure is worth a line: a steer that landed shows up
                // in the queue strip on its own.
                Side::Copied(Ok(n)) => {
                    self.set_toast(format!("copied {n} characters to the clipboard"))
                }
                Side::Copied(Err(e)) => self.notice(format!("copy failed: {e}"), Level::Warn),
                Side::Steered(Ok(())) => {}
                Side::Steered(Err(e)) => {
                    self.notice(format!("could not steer the turn: {e}"), Level::Warn)
                }
                Side::Plugins { commands, widgets } => {
                    self.commands = commands::Table::builtin()
                        .with_plugins(&commands)
                        .with_user(&self.user_commands);
                    self.widgets.clear();
                    for w in widgets {
                        self.set_widget(&w.id, Some(w.markup));
                    }
                }
                Side::CancelFailed(e) => self.notice(format!("could not cancel: {e}"), Level::Error),
                Side::Shell { job, result } => self.shell_finished(job, result),
                Side::Context(result) => match result {
                    Ok(res) if res.error.is_none() => {
                        self.overlay = Some(Overlay::View(Viewer::new("Context", crate::ui::context::rows(&res.output))));
                    }
                    Ok(res) => self.notice(format!("/context: {}", res.error.unwrap_or_default()), Level::Error),
                    Err(e) => self.notice(format!("/context: {e}"), Level::Error),
                },
                Side::Command { name, result } => match result {
                    Err(e) => self.notice(format!("{name}: {e}"), Level::Error),
                    Ok(res) => {
                        for line in &res.output {
                            // Handlers print Rich markup — they were written for
                            // Textual. A Notice is plain text, so the tags have to
                            // come off or they show up literally.
                            self.notice(crate::ui::markup::plain(line), Level::Info);
                        }
                        if let Some(err) = &res.error {
                            self.notice(format!("{name}: {err}"), Level::Error);
                        }
                        if !res.unsupported.is_empty() {
                            // Better than a command that silently half-runs.
                            self.notice(
                                format!(
                                    "{name} wanted {} — not available outside the Textual TUI",
                                    res.unsupported.join(", ")
                                ),
                                Level::Warn,
                            );
                        }
                        // A command run before the first turn gets a session
                        // minted for it; adopt it, or the state the handler just
                        // set would belong to a conversation we never continue.
                        if self.session_id.is_none() && !res.session_id.is_empty() {
                            self.session_id = Some(res.session_id.clone());
                            self.status.session = res.session_id;
                        }
                        // The handler may have set or cleared a panel.
                        self.widgets.clear();
                        for w in res.widgets {
                            self.set_widget(&w.id, Some(w.markup));
                        }
                    }
                },
            }
        }
    }

    /// Returns true when the turn has ended.
    fn apply(&mut self, ev: Event) -> bool {
        match ev {
            Event::Token { text, run_id } => match run_id {
                Some(id) => self.push_to_agent(id.as_str(), &text),
                None => self.append_streaming(&text, false),
            },
            Event::Thinking { text, run_id } => match run_id {
                // A child's reasoning belongs inside the child's block, not
                // interleaved with the parent's.
                Some(id) => self.push_to_agent(id.as_str(), &text),
                None => self.append_streaming(&text, true),
            },
            Event::AgentStarted { agent_id, run_id: Some(id), .. } => {
                self.blocks.push(Block::Agent {
                    run_id: id,
                    label: format!("Agent({agent_id})"),
                    text: String::new(),
                    done: false,
                });
                self.follow = true;
            }
            Event::AgentFinished { run_id: Some(id), .. } => {
                if let Some(Block::Agent { done, .. }) = self.agent_block(&id) {
                    *done = true;
                }
            }
            Event::AgentFinished { run_id: None, result } => {
                // The main agent's reply normally arrives as streamed tokens,
                // so this is a no-op. On the failure path (3-attempt API error)
                // nothing ever streamed: serve.py forwards step()'s message in
                // `result` and it is the only channel the turn has. Without it
                // the turn would end in silence.
                if !result.is_empty() && !matches!(self.blocks.last(), Some(Block::Assistant(_))) {
                    self.append_streaming(&result, false);
                }
            }
            // A child's own tool calls are narrated inside its block; rendering
            // them as the parent's would misattribute the work.
            Event::ToolStarted { name, args, run_id: Some(id) } => {
                let line = format!("
{}
", block::tool_title(&name, &args));
                self.push_to_agent(&id, &line);
            }
            Event::ToolFinished { run_id: Some(_), .. } => {}
            Event::ToolStarted { name, args, .. } => {
                // The checklist has its own pinned panel; a transcript block would
                // duplicate it, and leaving a pending slot here would let the next
                // tool's result fill the wrong one.
                if matches!(name.as_str(), "todo_write" | "subagent" | "subagent_fork") {
                    return false;
                }
                self.blocks.push(Block::Tool {
                    title: block::tool_title(&name, &args),
                    body: None,
                    kind: block::body_kind(&name),
                    failed: false,
                    started: Some(std::time::Instant::now()),
                    took: None,
                });
                self.follow = true;
            }
            Event::ToolFinished { name, args, output, failed, .. } => {
                // subagent calls already streamed into their Agent block; the
                // duplicate full text here is only worth showing if it failed.
                if matches!(name.as_str(), "subagent" | "subagent_fork") {
                    let low = output.trim_start().to_lowercase();
                    if low.starts_with("error") || low.starts_with("unknown") {
                        self.notice(format!("{name}: {output}"), Level::Error);
                    }
                    return false;
                }
                // `todo` draws itself: its package publishes the checklist as a
                // plugin panel, which arrives as Event::Widget. Parsing the call's
                // arguments here as well is how this client used to keep a second
                // checklist renderer that could disagree with the first.
                if name == "todo_write" {
                    return false;
                }
                self.finish_tool(&name, &args, output, failed)
            }
            // A plugin moved a panel mid-turn — a tool setting the goal, typically.
            Event::Widget { id, markup } => self.set_widget(&id, markup),
            Event::Usage { context, context_limit, .. } => {
                self.status.context = context;
                if context_limit > 0 {
                    self.status.context_limit = context_limit;
                }
            }
            Event::Phase { title } => self.notice(format!("▸ {title}"), Level::Info),
            Event::Log { message } => self.notice(format!("  {message}"), Level::Info),
            Event::JobFailed { error } => {
                self.notice(format!("error: {error}"), Level::Error);
                return true;
            }
            Event::SteerQueued { text } => self.queued.push(text),
            // Matched by text rather than popped blindly: the loop drains the
            // whole queue in one go, and the events arrive in that order.
            Event::SteerConsumed { text } => {
                if let Some(i) = self.queued.iter().position(|q| *q == text) {
                    self.queued.remove(i);
                }
            }
            Event::JobFinished { leftover_steers } => {
                // The queue belongs to the job that just ended, whatever is left
                // in it — a steer the loop never reached comes back as leftovers.
                self.queued.clear();
                self.pending_turn = (!leftover_steers.is_empty())
                    .then(|| leftover_steers.join("\n\n"));
                return true;
            }
            // agent_started/agent_finished/job_started carry nothing the transcript
            // needs — the tokens already told the story.
            _ => {}
        }
        false
    }

    fn agent_block(&mut self, run_id: &str) -> Option<&mut Block> {
        self.blocks.iter_mut().rev().find(
            |b| matches!(b, Block::Agent { run_id: r, .. } if r == run_id),
        )
    }

    /// Append a child's output to its block, creating one if the `agent_started`
    /// event was missed — a late subscriber must still see the work.
    fn push_to_agent(&mut self, run_id: &str, text: &str) {
        if let Some(b) = self.agent_block(run_id) {
            b.push_text(text);
        } else {
            self.blocks.push(Block::Agent {
                run_id: run_id.to_string(),
                label: "Agent".into(),
                text: text.to_string(),
                done: false,
            });
        }
        self.follow = true;
    }

    fn append_streaming(&mut self, text: &str, thinking: bool) {
        let matches_kind = matches!(
            self.blocks.last(),
            Some(Block::Thinking(_)) if thinking
        ) || matches!(
            self.blocks.last(),
            Some(Block::Assistant(_)) if !thinking
        );
        if matches_kind {
            if let Some(last) = self.blocks.last_mut() {
                last.push_text(text);
            }
        } else if thinking {
            self.blocks.push(Block::Thinking(text.to_string()));
        } else {
            self.blocks.push(Block::Assistant(text.to_string()));
        }
        self.follow = true;
    }

    /// Fill the oldest still-running tool block. Parallel calls emit every start
    /// before any finish, and finishes arrive in call order — so first started is
    /// first resolved, same rule the Textual TUI uses.
    fn finish_tool(&mut self, name: &str, args: &serde_json::Value, output: String, failed: bool) {
        let slot = self.blocks.iter_mut().find(
            |b| matches!(b, Block::Tool { body: None, .. }),
        );
        // apply_patch's argument *is* the diff, and it reads far better than the
        // "ok, patched" the tool returns.
        let shown = if name == "apply_patch" {
            args.get("patch")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or(output)
        } else {
            output
        };
        match slot {
            Some(Block::Tool { body, failed: slot_failed, started, took, .. }) => {
                *body = Some(shown);
                *slot_failed = failed;
                *took = started.map(|at| at.elapsed());
            }
            _ => self.blocks.push(Block::Tool {
                title: block::tool_title(name, args),
                body: Some(shown),
                kind: block::body_kind(name),
                failed,
                // A finish without a start: there is nothing to time it from.
                started: None,
                took: None,
            }),
        }
        self.follow = true;
    }

    // ── scrolling ─────────────────────────────────────────────────────────────

    /// Refresh the wrapped transcript for `width`, if anything it depends on
    /// changed. Re-wrapping thousands of lines on every 30ms tick would burn CPU
    /// for nothing while the user is just reading.
    ///
    /// Separate from reading `self.cache` so the draw closure can hold shared
    /// borrows of the rest of the app at the same time.
    pub fn ensure_wrapped(&mut self, width: u16) {
        let key = (self.revision, width, self.expand_all);
        if self.cache_key != Some(key) {
            let lines = chat::lines(&self.blocks, &self.theme, self.expand_all, width);
            self.cache = chat::wrap(&lines, width);
            self.cache_key = Some(key);
        }
    }

    /// Call after any change to `blocks` or the theme.
    fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    /// Clamp the offset to the content, and honour `follow` by pinning to the tail.
    pub fn resolve_scroll(&mut self, content_height: u16, viewport: u16) {
        let max = content_height.saturating_sub(viewport);
        if self.follow {
            self.scroll = max;
        } else {
            self.scroll = self.scroll.min(max);
        }
    }

    pub fn scroll_by(&mut self, delta: i32) {
        let next = (self.scroll as i32 + delta).max(0) as u16;
        self.scroll = next;
        // Scrolling up detaches from the tail; the caller re-pins on ^l or a new turn.
        if delta < 0 {
            self.follow = false;
        }
    }

    pub fn to_bottom(&mut self) {
        self.follow = true;
    }

    // ── selection ─────────────────────────────────────────────────────────────

    /// Turn a terminal position into a (wrap-cache line, column), or `None` when
    /// the pointer isn't over the transcript.
    pub fn at(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        let r = self.transcript_rect;
        if r.width == 0 || col < r.x || row < r.y || col >= r.x + r.width || row >= r.y + r.height {
            return None;
        }
        Some((
            self.scroll as usize + (row - r.y) as usize,
            (col - r.x) as usize,
        ))
    }

    pub fn begin_selection(&mut self, col: u16, row: u16) {
        self.selection = self.at(col, row).map(Selection::new);
    }

    pub fn drag_selection(&mut self, col: u16, row: u16) {
        // Clamp to the transcript rather than dropping the update: dragging a few
        // pixels past the edge is how anyone selects the last line.
        let r = self.transcript_rect;
        if r.width == 0 {
            return;
        }
        let col = col.clamp(r.x, r.x + r.width - 1);
        let row = row.clamp(r.y, r.y + r.height - 1);
        let Some(to) = self.at(col, row) else { return };
        if let Some(sel) = self.selection.as_mut() {
            sel.extend(to);
        }
    }

    /// Copy whatever is selected. Called on mouse-up: select-to-copy, the way a
    /// terminal does it, so there is no second keystroke to remember.
    pub fn copy_selection(&mut self) {
        let Some(sel) = self.selection else { return };
        if sel.is_empty() {
            self.selection = None; // a plain click just clears the highlight
            return;
        }
        let text = sel.text(&self.cache);
        // Off the UI thread. The Windows clipboard is a machine-wide lock, and
        // another process holding it makes `set_text` wait — a drag ends in a
        // copy, so that wait froze the whole app mid-selection. Both writes are
        // through a locked stdout, so the escape sequence can't land inside a
        // frame the renderer is halfway through.
        let tx = self.side_tx.clone();
        std::thread::spawn(move || {
            side::deliver(&tx, Side::Copied(clipboard::copy(&text).map_err(|e| e.to_string())));
        });
    }

    pub fn set_toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), std::time::Instant::now()));
    }

    /// How long a toast stays up. Long enough to read, short enough not to sit
    /// over the status line while you work.
    const TOAST_TTL: Duration = Duration::from_secs(2);

    fn expire_toast(&mut self) {
        if let Some((_, at)) = self.toast {
            if at.elapsed() > Self::TOAST_TTL {
                self.toast = None;
            }
        }
    }
}

/// Draw one frame. Generic over the backend so it can be rendered into ratatui's
/// `TestBackend` and asserted on, rather than only ever running against a real TTY.
pub fn render<B>(term: &mut ratatui::Terminal<B>, app: &mut App) -> Result<()>
where
    B: ratatui::backend::Backend,
    // anyhow needs the backend's error to be a real std error to wrap it.
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Geometry is settled before drawing so the scroll offset the frame uses is the
    // same one the app keeps — computing it inside the closure would leave the two
    // a frame apart, which shows up as jitter while streaming.
    let area = term.size()?;
    let full = ratatui::layout::Rect::new(0, 0, area.width, area.height);
    let panes = crate::ui::layout(
        full,
        app.ed.height(full.width, 8),
        app.menu_rows().len() as u16,
        crate::ui::widgets::height(&app.widgets),
        crate::ui::steer::height(app.queued.len()),
    );

    // While a turn runs, the bottom row of the transcript pane belongs to the
    // spinner — right where the next output will land, and outside the wrap cache.
    let busy = app.busy();
    let (transcript, working) = if busy && panes.chat.height > 1 {
        let mut t = panes.chat;
        t.height -= 1;
        let mut w = panes.chat;
        w.y += t.height;
        w.height = 1;
        (t, Some(w))
    } else {
        (panes.chat, None)
    };
    if busy {
        app.spinner.tick();
    }

    app.ensure_wrapped(transcript.width);
    app.resolve_scroll(chat::height(&app.cache), transcript.height);
    // The mouse handler maps pointer positions through this, so it has to be the
    // rect this very frame drew into.
    app.transcript_rect = transcript;

    let scroll = app.scroll;
    let menu = app.menu_rows();
    let sel = app.menu_sel;
    // Shared borrow only inside the closure, so the caller keeps `app` mutable.
    let view = &*app;
    term.draw(|f| {
        chat::draw(f, transcript, &view.cache, scroll, &view.theme);
        if let Some(sel) = &view.selection {
            highlight(f, transcript, &view.cache, scroll, sel);
        }
        if let Some(row) = working {
            f.render_widget(
                ratatui::widgets::Paragraph::new(view.spinner.line(&view.theme)),
                row,
            );
        }
        if let Some(w) = panes.widgets {
            crate::ui::widgets::draw(f, w, &view.widgets, &view.theme);
        }
        if let Some(q) = panes.queued {
            crate::ui::steer::draw(f, q, &view.queued, &view.theme);
        }
        if let Some(r) = panes.rule {
            f.render_widget(
                ratatui::widgets::Paragraph::new(ratatui::text::Line::from(
                    ratatui::text::Span::styled(
                        "─".repeat(r.width as usize),
                        ratatui::style::Style::default().fg(view.theme.border),
                    ),
                )),
                r,
            );
        }
        if let Some(m) = panes.menu {
            crate::ui::input::draw_menu(f, m, &menu, sel, &view.theme);
        }
        crate::ui::input::draw(
            f,
            panes.input,
            &view.ed,
            "ask anything…  (/ for commands)",
            &view.theme,
        );
        match &view.toast {
            // The toast borrows the status row while it lives: it is the one line
            // guaranteed to be on screen, and cwd/model come back in two seconds.
            Some((text, _)) => f.render_widget(
                ratatui::widgets::Paragraph::new(ratatui::text::Line::from(
                    ratatui::text::Span::styled(
                        format!(" ✓ {text}"),
                        ratatui::style::Style::default().fg(view.theme.ok),
                    ),
                )),
                panes.status,
            ),
            // The mode is named under the prompt, where the model and cwd usually are.
            None if view.ed.shell_mode() => f.render_widget(
                ratatui::widgets::Paragraph::new(ratatui::text::Line::from(
                    ratatui::text::Span::styled(
                        " shell mode",
                        ratatui::style::Style::default().fg(view.theme.warn),
                    ),
                )),
                panes.status,
            ),
            None => crate::ui::status::draw(f, panes.status, &view.status, &view.theme),
        }
        // Last, and over everything: a modal that drew before the prompt would be
        // painted over by it, and the prompt's caret would sit on top of the box.
        match &view.overlay {
            Some(Overlay::Pick(p)) => p.draw(f, full, &view.theme),
            Some(Overlay::Ask(a)) => a.draw(f, full, &view.theme),
            Some(Overlay::View(v)) => v.draw(f, full, &view.theme),
            None => {}
        }
    })?;
    Ok(())
}

/// Repaint the selected cells in reverse video, on top of the drawn transcript.
///
/// Restyling the buffer rather than re-rendering the lines keeps the selection
/// independent of how any block chose to colour itself, and reverse video is the
/// one highlight that stays legible under every theme.
fn highlight(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    cache: &[ratatui::text::Line<'static>],
    scroll: u16,
    sel: &Selection,
) {
    use ratatui::style::Modifier;
    for y in 0..area.height {
        let li = scroll as usize + y as usize;
        let Some(line) = cache.get(li) else { break };
        // Stop at the text: the padding to the right of a short line is not
        // content, and highlighting it would draw a bar across empty space.
        let len = crate::ui::selection::line_text(line).chars().count();
        for x in 0..area.width.min(len as u16) {
            if sel.contains(li, x as usize) {
                f.buffer_mut()[(area.x + x, area.y + y)]
                    .modifier
                    .insert(Modifier::REVERSED);
            }
        }
    }
}

/// The terminal loop. Owns nothing but the draw/poll rhythm.
pub fn run(term: &mut crate::Terminal, app: &mut App) -> Result<()> {
    // Input is read on its own thread; this one only ever waits on a channel, so
    // nothing on the terminal side can stop it drawing. See `keys::spawn_reader`.
    let input = keys::spawn_reader();
    while !app.should_quit {
        {
            let _s = log::timed("pump", 20);
            app.pump();
        }
        {
            let _s = log::timed("render", 20);
            render(term, app)?;
        }
        keys::poll(app, &input, TICK)?;
    }
    Ok(())
}

impl App {
    /// The completion menu: slash commands on a line that starts with one,
    /// otherwise the files matching the `@path` token under the caret.
    ///
    /// The two can't collide — a slash command occupies the whole prompt while
    /// an `@path` is a token inside one — so the leading character decides.
    pub fn menu_rows(&self) -> Vec<crate::ui::input::MenuRow> {
        if self.ed.text().starts_with('/') && !self.ed.shell_mode() {
            return crate::ui::input::menu_matches(self.ed.text(), &self.commands);
        }
        crate::ui::input::file_matches(self.ed.text(), self.ed.cursor(), self.files.list())
    }

    pub fn menu_open(&self) -> bool {
        // A modal owns the keyboard, so the command menu must not also claim the
        // arrows and enter while one is up.
        !self.overlay_open() && !self.menu_rows().is_empty()
    }

    /// True when what is typed already *is* one of the menu's completions.
    ///
    /// Enter has to send in that case. A subcommand keeps matching its own hint
    /// once fully typed, so completing again would rewrite `/goal clear` as
    /// `/goal clear` for ever — the command could never be submitted at all.
    pub fn menu_exact(&self) -> bool {
        let text = self.ed.text();
        let typed = text.trim();
        !typed.is_empty() && self.menu_rows().iter().any(|r| r.applied(text).trim() == typed)
    }
}

/// A command typed with `!` is titled as the agent's own `bash` calls are.
fn shell_title(command: &str) -> String {
    block::tool_title("bash", &serde_json::json!({ "command": command }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "legacy-http")]
    use crate::net::Client;
    use crate::ui::block::Body;

    /// Environment variables are process-global, so any test that redirects
    /// `LELE_HOME` has to be the only one doing it. Tests that *write* settings
    /// take this lock; tests that merely read are unaffected either way.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Redirect config and theme lookups into a scratch directory for the duration
    /// of the returned guard. Without it the suite would overwrite the real
    /// ~/.lele/lrs.toml — which it did, once, before this existed.
    fn isolated(name: &str) -> std::sync::MutexGuard<'static, ()> {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join("lrs-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        unsafe { std::env::set_var("LELE_HOME", &dir) };
        guard
    }

    pub(super) fn health_fixture() -> lrs::protocol::Health {
        lrs::protocol::Health {
            model: "test-model".into(),
            context_limit: 200_000,
            tools: vec!["read_file".into(), "bash".into()],
            plugins: vec!["goal".into()],
            ..Default::default()
        }
    }

    fn app() -> App {
        App::new(std::sync::Arc::new(crate::backend::HttpBackend(Client::new(1))), false, health_fixture())
    }

    fn last_text(a: &App) -> String {
        match a.blocks.last() {
            Some(Block::Assistant(s)) | Some(Block::Thinking(s)) => s.clone(),
            Some(Block::Notice { text, .. }) => text.clone(),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn streamed_tokens_coalesce_into_one_block() {
        let mut a = app();
        let before = a.blocks.len();
        a.apply(Event::Token { text: "ci".into(), run_id: None });
        a.apply(Event::Token { text: "ao".into(), run_id: None });
        assert_eq!(a.blocks.len(), before + 1, "each token must not spawn a block");
        assert_eq!(last_text(&a), "ciao");
    }

    #[test]
    fn thinking_and_answer_stay_separate_blocks() {
        let mut a = app();
        a.apply(Event::Thinking { text: "hmm".into(), run_id: None });
        a.apply(Event::Token { text: "ecco".into(), run_id: None });
        assert!(matches!(a.blocks[a.blocks.len() - 2], Block::Thinking(_)));
        assert!(matches!(a.blocks[a.blocks.len() - 1], Block::Assistant(_)));
    }

    #[test]
    fn tools_resolve_first_started_first() {
        let mut a = app();
        a.apply(Event::ToolStarted { name: "read_file".into(), args: serde_json::json!({"path":"a"}), run_id: None });
        a.apply(Event::ToolStarted { name: "read_file".into(), args: serde_json::json!({"path":"b"}), run_id: None });
        a.apply(Event::ToolFinished {
            name: "read_file".into(),
            args: serde_json::json!({"path":"a"}),
            output: "primo".into(),
            failed: false,
            run_id: None,
        });
        let tools: Vec<&Block> = a.blocks.iter().filter(|b| matches!(b, Block::Tool { .. })).collect();
        match (&tools[0], &tools[1]) {
            (Block::Tool { body: Some(x), .. }, Block::Tool { body: None, .. }) => {
                assert_eq!(x, "primo")
            }
            other => panic!("wrong pairing: {other:?}"),
        }
    }

    #[test]
    fn a_finish_without_a_start_still_renders() {
        let mut a = app();
        a.apply(Event::ToolFinished {
            name: "bash".into(),
            args: serde_json::json!({"command":"ls"}),
            output: "out".into(),
            failed: false,
            run_id: None,
        });
        assert!(a.blocks.iter().any(|b| matches!(b, Block::Tool { body: Some(_), .. })));
    }

    #[test]
    fn apply_patch_shows_the_patch_not_the_ack() {
        let mut a = app();
        a.apply(Event::ToolStarted { name: "apply_patch".into(), args: serde_json::json!({}), run_id: None });
        a.apply(Event::ToolFinished {
            name: "apply_patch".into(),
            args: serde_json::json!({"patch": "+ new line"}),
            output: "ok".into(),
            failed: false,
            run_id: None,
        });
        let found = a.blocks.iter().any(
            |b| matches!(b, Block::Tool { body: Some(s), kind: Body::Diff, .. } if s == "+ new line"),
        );
        assert!(found, "expected the diff body, got {:?}", a.blocks);
    }

    #[test]
    fn job_end_events_report_completion() {
        let mut a = app();
        assert!(a.apply(Event::JobFinished { leftover_steers: Vec::new() }));
        assert!(a.apply(Event::JobFailed { error: "boom".into() }));
        assert!(!a.apply(Event::Token { text: "x".into(), run_id: None }));
    }

    #[test]
    fn an_unknown_event_changes_nothing() {
        let mut a = app();
        let before = a.blocks.len();
        assert!(!a.apply(Event::Unknown));
        assert_eq!(a.blocks.len(), before);
    }

    #[test]
    fn unknown_theme_warns_and_keeps_the_current_one() {
        let mut a = app();
        let before = a.theme.name.clone();
        a.set_theme("nope");
        assert_eq!(a.theme.name, before);
        assert!(matches!(a.blocks.last(), Some(Block::Notice { level: Level::Warn, .. })));
    }

    #[test]
    fn switching_to_a_builtin_theme_takes_effect() {
        // set_theme persists; without the guard this writes the real
        // ~/.lele/lrs.toml and races the isolated tests for LELE_HOME.
        let _env = isolated("switch");
        let mut a = app();
        a.set_theme("nord");
        assert_eq!(a.theme.name, "nord");
    }

    #[test]
    fn clear_drops_the_session_so_the_next_turn_starts_fresh() {
        let mut a = app();
        a.session_id = Some("abc".into());
        a.status.context = 5000;
        a.ed.set("/clear");
        a.submit();
        assert!(a.session_id.is_none());
        assert_eq!(a.status.context, 0);
    }

    #[test]
    fn submitting_blank_input_does_nothing() {
        let mut a = app();
        let before = a.blocks.len();
        a.ed.set("   ");
        a.submit();
        assert_eq!(a.blocks.len(), before);
    }

    #[test]
    fn scrolling_up_detaches_from_the_tail() {
        let mut a = app();
        assert!(a.follow);
        a.scroll_by(-3);
        assert!(!a.follow);
        a.to_bottom();
        assert!(a.follow);
    }

    #[test]
    fn follow_pins_to_the_bottom_regardless_of_offset() {
        let mut a = app();
        a.scroll = 999;
        a.follow = true;
        a.resolve_scroll(50, 20);
        assert_eq!(a.scroll, 30);
    }

    #[test]
    fn offset_is_clamped_to_the_content() {
        let mut a = app();
        a.follow = false;
        a.scroll = 999;
        a.resolve_scroll(50, 20);
        assert_eq!(a.scroll, 30);
    }

    #[test]
    fn content_shorter_than_the_viewport_never_scrolls() {
        let mut a = app();
        a.follow = true;
        a.resolve_scroll(5, 20);
        assert_eq!(a.scroll, 0);
    }

    #[test]
    fn expand_all_reveals_the_full_tool_body() {
        let mut a = app();
        let body = (0..30).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        a.blocks.push(Block::Tool { title: "t".into(), body: Some(body), kind: Body::Plain, failed: false, started: None, took: None });
        let collapsed = chat::lines(&a.blocks, &a.theme, false, 80).len();
        let expanded = chat::lines(&a.blocks, &a.theme, true, 80).len();
        assert!(expanded > collapsed, "^o must reveal more, got {expanded} vs {collapsed}");
    }

    #[test]
    fn the_wrap_cache_is_invalidated_when_content_changes() {
        let mut a = app();
        a.ensure_wrapped(80);
        let before = a.cache.len();
        a.notice("una riga nuova", Level::Info);
        a.ensure_wrapped(80);
        assert!(a.cache.len() > before, "stale cache would freeze the transcript");
    }

    #[test]
    fn the_wrap_cache_is_invalidated_on_resize() {
        let mut a = app();
        a.notice("parola ".repeat(40), Level::Info);
        a.ensure_wrapped(200);
        let wide = a.cache.len();
        a.ensure_wrapped(20);
        assert!(a.cache.len() > wide, "resize must re-wrap");
    }

    // ── rendering ─────────────────────────────────────────────────────────────
    // Drawn into ratatui's TestBackend, so the whole frame is asserted without a TTY.

    fn screen(a: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        render(&mut term, a).unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_full_frame_renders_transcript_prompt_and_status() {
        let mut a = app();
        a.blocks.clear();
        a.touch();
        a.blocks.push(Block::User("ciao".into()));
        a.blocks.push(Block::Assistant("**eccomi**".into()));
        a.touch();

        let s = screen(&mut a, 40, 10);
        let joined = s.join("\n");
        assert!(joined.contains("❯ ciao"), "user message missing:\n{joined}");
        assert!(joined.contains("eccomi"), "reply missing:\n{joined}");
        // bold markers must not survive into the rendered frame
        assert!(!joined.contains("**"), "raw markdown leaked:\n{joined}");
        // status is the last row
        assert!(s[9].contains("test-model"), "status row was {:?}", s[9]);
    }

    // ── pickers, connect, resume ──────────────────────────────────────────────

    fn with_catalog(a: &mut App) {
        a.catalog = Catalog {
            models: vec![
                "anthropic:claude-opus-5".into(),
                "z-ai/glm-4.6".into(),
                "opencode-go:gpt-5.6-luna".into(),
            ],
            current: "opencode-go:gpt-5.6-luna".into(),
            providers: vec![
                Provider {
                    name: "anthropic".into(),
                    api_key_env: "ANTHROPIC_API_KEY".into(),
                    base_url: "https://api.anthropic.com/v1".into(),
                    connected: true,
                    ..Default::default()
                },
                Provider {
                    name: "opencode-go".into(),
                    api_key_env: "OPENCODE_API_KEY".into(),
                    base_url: "https://opencode.ai/zen/go/v1".into(),
                    connected: false,
                    ..Default::default()
                },
            ],
            loaded: true,
        };
    }

    #[test]
    fn the_model_picker_groups_by_provider_and_buries_openrouter() {
        // openrouter's catalog is 300+ entries; first would hide everything else.
        let mut a = app();
        with_catalog(&mut a);
        let rows = a.model_rows();
        let headings: Vec<&str> = rows
            .iter()
            .filter(|r| r.id.is_none())
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(headings, vec!["anthropic", "opencode-go", "openrouter"]);
        // a bare ref with no known prefix belongs to openrouter
        assert!(rows.iter().any(|r| r.id.as_deref() == Some("z-ai/glm-4.6")));
    }

    #[test]
    fn the_picker_marks_the_model_actually_in_use() {
        let mut a = app();
        with_catalog(&mut a);
        let current = a
            .model_rows()
            .into_iter()
            .find(|r| !r.note.is_empty())
            .expect("nothing marked as current");
        assert_eq!(current.id.as_deref(), Some("opencode-go:gpt-5.6-luna"));

        // and it follows an override rather than the server's stale default
        a.model_override = Some("anthropic:claude-opus-5".into());
        let current = a.model_rows().into_iter().find(|r| !r.note.is_empty()).unwrap();
        assert_eq!(current.id.as_deref(), Some("anthropic:claude-opus-5"));
    }

    #[test]
    fn the_provider_picker_dots_the_connected_ones() {
        let mut a = app();
        with_catalog(&mut a);
        let rows = a.provider_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].dot, Some(true), "anthropic has a key");
        assert_eq!(rows[1].dot, Some(false));
    }

    #[test]
    fn a_picker_opened_before_the_catalog_lands_fills_in_by_itself() {
        // Making the user close and reopen it would be the obvious failure here.
        let mut a = app();
        a.open_model_picker();
        assert!(matches!(a.overlay, Some(Overlay::Pick(ref p)) if p.loading));

        a.side_tx
            .send(Side::Catalog {
                models: vec!["anthropic:claude-opus-5".into()],
                current: "anthropic:claude-opus-5".into(),
                providers: vec![Provider { name: "anthropic".into(), ..Default::default() }],
                error: None,
            })
            .unwrap();
        a.pump();
        match a.overlay {
            Some(Overlay::Pick(ref p)) => {
                assert!(!p.loading);
                assert_eq!(p.selected(), Some("anthropic:claude-opus-5"));
            }
            _ => panic!("the picker closed instead of filling in"),
        }
    }

    #[test]
    fn a_bare_model_argument_is_matched_against_the_catalog() {
        let mut a = app();
        with_catalog(&mut a);
        assert_eq!(a.resolve_model("luna").as_deref(), Some("opencode-go:gpt-5.6-luna"));
        assert_eq!(a.resolve_model("GLM-4.6").as_deref(), Some("z-ai/glm-4.6"));
        assert_eq!(a.resolve_model("banana"), None);
    }

    #[test]
    fn picking_a_model_applies_it_to_the_next_turn_immediately() {
        // The persist call is a round trip; waiting for it before the override
        // takes effect would make the very next turn use the old model.
        let mut a = app();
        with_catalog(&mut a);
        a.choose_model("anthropic:claude-opus-5");
        assert_eq!(a.model_override.as_deref(), Some("anthropic:claude-opus-5"));
        assert_eq!(a.status.model, "anthropic:claude-opus-5");
    }

    #[test]
    fn a_model_whose_provider_has_no_key_opens_the_connect_prompt() {
        let mut a = app();
        with_catalog(&mut a);
        a.side_tx
            .send(Side::ModelSet(Ok(ModelSet {
                model: "opencode-go:gpt-5.6-luna".into(),
                missing_key_env: Some("OPENCODE_API_KEY".into()),
                context_limit: 120_000,
            })))
            .unwrap();
        a.pump();
        assert_eq!(a.status.context_limit, 120_000);
        match a.overlay {
            Some(Overlay::Ask(ref ask)) => {
                assert_eq!(
                    ask.kind,
                    crate::ui::ask::Kind::ConnectKey {
                        provider: "opencode-go".into(),
                        env: "OPENCODE_API_KEY".into(),
                    }
                );
            }
            _ => panic!("a key-less model should lead straight into connect"),
        }
    }

    #[test]
    fn a_model_that_is_ready_to_use_asks_for_nothing() {
        let mut a = app();
        with_catalog(&mut a);
        a.side_tx
            .send(Side::ModelSet(Ok(ModelSet {
                model: "anthropic:claude-opus-5".into(),
                missing_key_env: None,
                context_limit: 200_000,
            })))
            .unwrap();
        a.pump();
        assert!(!a.overlay_open());
    }

    #[test]
    fn a_rejected_key_says_why_and_asks_again() {
        let mut a = app();
        with_catalog(&mut a);
        a.side_tx
            .send(Side::Connected {
                provider: "anthropic".into(),
                error: Some("key rejected by anthropic (HTTP 401)".into()),
            })
            .unwrap();
        a.pump();
        assert!(matches!(a.overlay, Some(Overlay::Ask(_))), "must not drop the user back to the prompt");
        let said = format!("{:?}", a.blocks);
        assert!(said.contains("401"), "the reason was swallowed: {said}");
    }

    #[test]
    fn connect_refuses_a_provider_that_is_not_configured() {
        let mut a = app();
        with_catalog(&mut a);
        a.open_connect("ghost");
        assert!(!a.overlay_open(), "asked for a secret nothing could use");
        assert!(matches!(a.blocks.last(), Some(Block::Notice { level: Level::Warn, .. })));
    }

    #[test]
    fn resuming_replaces_the_transcript_and_continues_the_session() {
        let mut a = app();
        a.side_tx
            .send(Side::Resumed {
                blocks: vec![Block::User("prima".into()), Block::Assistant("dopo".into())],
                session_id: "20250101_120000_abcdef".into(),
                summary: "resumed 20250101_120000_abcdef — 1 turn(s)".into(),
                error: None,
            })
            .unwrap();
        a.pump();
        assert_eq!(a.session_id.as_deref(), Some("20250101_120000_abcdef"));
        assert!(matches!(a.blocks.first(), Some(Block::User(t)) if t == "prima"));
        // the next turn continues that conversation rather than starting a new one
        assert_eq!(a.status.session, "20250101_120000_abcdef");
    }

    #[test]
    fn a_project_with_no_history_says_so_instead_of_failing() {
        let mut a = app();
        a.side_tx
            .send(Side::Resumed {
                blocks: Vec::new(),
                session_id: String::new(),
                summary: String::new(),
                error: Some("no earlier session in this project".into()),
            })
            .unwrap();
        a.pump();
        assert!(a.session_id.is_none());
        assert!(matches!(a.blocks.last(), Some(Block::Notice { level: Level::Warn, .. })));
    }

    #[test]
    fn a_pasted_image_becomes_a_chip_in_the_prompt() {
        let mut a = app();
        a.side_tx
            .send(Side::Image(Ok("data:image/png;base64,AAA".into())))
            .unwrap();
        a.pump();
        assert_eq!(a.ed.text(), "[Image #1] ");
    }

    #[test]
    fn a_clipboard_with_no_image_warns_and_leaves_the_prompt_alone() {
        let mut a = app();
        a.ed.set("scrivendo");
        a.side_tx
            .send(Side::Image(Err("the clipboard holds no image".into())))
            .unwrap();
        a.pump();
        assert_eq!(a.ed.text(), "scrivendo");
        assert!(matches!(a.blocks.last(), Some(Block::Notice { level: Level::Warn, .. })));
    }

    #[test]
    fn a_turn_with_an_image_is_sent_as_content_parts() {
        // The transcript keeps the chip; the wire gets text plus the picture.
        let mut a = app();
        let chip = a.pastes.add_image("data:image/png;base64,AAA".into());
        let shown = format!("che cos'è {chip}");
        let images = a.pastes.images_in(&shown);
        assert_eq!(images, vec!["data:image/png;base64,AAA"]);
        let text = a.pastes.expand(&shown);
        assert_eq!(text, shown, "the label reads fine in place");
    }

    // ── selection ─────────────────────────────────────────────────────────────

    /// A rendered app whose transcript rect is real, so mouse coordinates map.
    fn drawn(w: u16, h: u16) -> (App, Vec<String>) {
        let mut a = app();
        a.blocks.clear();
        a.blocks.push(Block::Assistant("alfa beta gamma".into()));
        a.touch();
        let s = screen(&mut a, w, h);
        (a, s)
    }

    #[test]
    fn a_pointer_position_maps_to_a_line_and_column() {
        let (a, _) = drawn(40, 12);
        let r = a.transcript_rect;
        assert!(r.width > 0, "the transcript rect was never recorded");
        assert_eq!(a.at(r.x + 3, r.y + 1), Some((a.scroll as usize + 1, 3)));
        // outside the transcript there is nothing to select
        assert_eq!(a.at(r.x, r.y + r.height), None);
        assert_eq!(a.at(r.x + r.width, r.y), None);
    }

    #[test]
    fn a_drag_past_the_edge_still_reaches_the_last_line() {
        // Clamping rather than dropping the update: overshooting the bottom is how
        // anyone selects to the end.
        let (mut a, _) = drawn(40, 12);
        let r = a.transcript_rect;
        a.begin_selection(r.x, r.y);
        a.drag_selection(r.x + 500, r.y + 500);
        let sel = a.selection.expect("the drag was dropped");
        assert_eq!(sel.head.0, a.scroll as usize + (r.height - 1) as usize);
        assert_eq!(sel.head.1, (r.width - 1) as usize);
    }

    #[test]
    fn a_click_with_no_drag_clears_instead_of_copying() {
        let (mut a, _) = drawn(40, 12);
        let r = a.transcript_rect;
        a.begin_selection(r.x + 2, r.y);
        a.copy_selection();
        assert!(a.selection.is_none());
        assert!(a.toast.is_none(), "a bare click must not claim it copied anything");
    }

    #[test]
    fn a_selection_is_highlighted_in_the_rendered_frame() {
        let (mut a, _) = drawn(40, 12);
        let r = a.transcript_rect;
        // The banner is cleared, so the assistant text is on the first rows.
        let row = (0..r.height)
            .find(|y| {
                a.cache
                    .get(a.scroll as usize + *y as usize)
                    .map(|l| crate::ui::selection::line_text(l).contains("alfa"))
                    .unwrap_or(false)
            })
            .expect("the reply is not on screen");
        a.begin_selection(r.x, r.y + row);
        a.drag_selection(r.x + 3, r.y + row);

        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        render(&mut term, &mut a).unwrap();
        let buf = term.backend().buffer().clone();
        let rev = |x: u16| {
            buf[(r.x + x, r.y + row)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        };
        assert!(rev(0) && rev(3), "the selected cells are not highlighted");
        assert!(!rev(4), "the highlight ran past the selection");
    }

    #[test]
    fn the_highlight_stops_at_the_end_of_the_text() {
        // A bar drawn across the padding of a short line looks like a bug.
        let mut a = app();
        a.blocks.clear();
        a.blocks.push(Block::Assistant("corta".into()));
        a.touch();
        screen(&mut a, 40, 12);
        let r = a.transcript_rect;
        let row = (0..r.height)
            .find(|y| {
                a.cache
                    .get(a.scroll as usize + *y as usize)
                    .map(|l| crate::ui::selection::line_text(l).contains("corta"))
                    .unwrap_or(false)
            })
            .unwrap();
        a.begin_selection(r.x, r.y + row);
        a.drag_selection(r.x + 39, r.y + row);

        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        render(&mut term, &mut a).unwrap();
        let buf = term.backend().buffer().clone();
        assert!(!buf[(r.x + 30, r.y + row)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED));
    }

    #[test]
    fn typing_clears_a_stale_highlight() {
        let (mut a, _) = drawn(40, 12);
        let r = a.transcript_rect;
        a.begin_selection(r.x, r.y);
        a.drag_selection(r.x + 4, r.y);
        assert!(a.selection.is_some());
        keys::tests::press_key(&mut a, 'x');
        assert!(a.selection.is_none());
    }

    #[test]
    fn a_toast_takes_the_status_row_and_then_gives_it_back() {
        let mut a = app();
        a.set_toast("copied 12 characters to the clipboard");
        let s = screen(&mut a, 60, 12);
        assert!(s[11].contains("copied 12 characters"), "status row was {:?}", s[11]);

        // Rewind it past its lifetime rather than sleeping in a test.
        a.toast = Some((
            "vecchio".into(),
            std::time::Instant::now() - Duration::from_secs(10),
        ));
        a.pump();
        let s = screen(&mut a, 60, 12);
        assert!(s[11].contains("test-model"), "the status line never came back: {:?}", s[11]);
    }

    #[test]
    fn the_wheel_scrolls_now_that_the_mouse_is_captured() {
        // Capturing the mouse also captures the wheel; without this, scrolling
        // silently stops working.
        let (mut a, _) = drawn(40, 6);
        a.follow = true;
        keys::tests::wheel_up(&mut a);
        assert!(!a.follow, "the wheel should detach from the tail");
    }

    // ── plugins ───────────────────────────────────────────────────────────────

    fn goal_spec() -> lrs::protocol::CommandSpec {
        lrs::protocol::CommandSpec {
            name: "/goal".into(),
            desc: "Set, pause, resume, or clear a session goal".into(),
            sub: std::collections::BTreeMap::from([
                ("<objective>".to_string(), "set a new goal".to_string()),
                ("pause".to_string(), "pause the goal".to_string()),
            ]),
        }
    }

    fn with_goal_plugin(a: &mut App) {
        a.side_tx
            .send(Side::Plugins { commands: vec![goal_spec()], widgets: Vec::new() })
            .unwrap();
        a.pump();
    }

    #[test]
    fn the_servers_plugin_commands_join_the_menu() {
        let mut a = app();
        assert!(a.commands.get("/goal").is_none(), "nothing known before the fetch");
        with_goal_plugin(&mut a);
        assert!(a.commands.get("/goal").is_some());

        a.ed.set("/go");
        let rows = a.menu_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "/goal");
        assert!(rows[0].desc.contains("session goal"));
    }

    #[test]
    fn a_plugins_subcommand_hints_show_above_the_prompt() {
        let mut a = app();
        with_goal_plugin(&mut a);
        a.ed.set("/goal ");
        let labels: Vec<String> = a.menu_rows().into_iter().map(|r| r.label).collect();
        assert_eq!(labels, vec!["pause", "<objective>"]);
    }

    #[test]
    fn completing_the_placeholder_leaves_it_ready_to_type_over() {
        let mut a = app();
        with_goal_plugin(&mut a);
        a.ed.set("/goal ");
        a.menu_sel = 1; // the <objective> row
        keys::tests::press_tab(&mut a);
        assert_eq!(a.ed.text(), "/goal <objective>");
        keys::tests::press_key(&mut a, 's');
        assert_eq!(a.ed.text(), "/goal s", "the placeholder should have been replaced");
    }

    #[test]
    fn a_fully_typed_subcommand_can_actually_be_sent() {
        // It keeps matching its own hint, so completing again would rewrite it to
        // itself for ever and enter would never submit.
        let mut a = app();
        with_goal_plugin(&mut a);
        a.ed.set("/goal pause");
        assert!(a.menu_open(), "the hint is still showing, as it should be");
        assert!(a.menu_exact(), "enter would re-complete instead of sending");

        keys::tests::press_enter(&mut a);
        assert!(a.ed.is_empty(), "the prompt was not submitted");
    }

    #[test]
    fn a_half_typed_subcommand_still_completes_on_enter() {
        let mut a = app();
        with_goal_plugin(&mut a);
        a.ed.set("/goal pau");
        keys::tests::press_enter(&mut a);
        assert_eq!(a.ed.text(), "/goal pause");
        // and a second press sends it
        keys::tests::press_enter(&mut a);
        assert!(a.ed.is_empty());
    }

    // ── steering ──────────────────────────────────────────────────────────────

    #[test]
    fn a_queued_steer_shows_until_the_loop_takes_it() {
        let mut a = app();
        a.apply(Event::SteerQueued { text: "usa il file vecchio".into() });
        assert_eq!(a.queued, vec!["usa il file vecchio".to_string()]);

        a.apply(Event::SteerConsumed { text: "usa il file vecchio".into() });
        assert!(a.queued.is_empty(), "the strip would sit there for the rest of the turn");
    }

    #[test]
    fn consuming_one_steer_leaves_the_others_queued() {
        let mut a = app();
        for t in ["uno", "due", "tre"] {
            a.apply(Event::SteerQueued { text: t.into() });
        }
        a.apply(Event::SteerConsumed { text: "due".into() });
        assert_eq!(a.queued, vec!["uno".to_string(), "tre".to_string()]);
    }

    #[test]
    fn an_unknown_steer_consumption_is_harmless() {
        // The stream can be joined late, so a consumed event may name something
        // this client never saw queued.
        let mut a = app();
        a.apply(Event::SteerConsumed { text: "mai visto".into() });
        assert!(a.queued.is_empty());
    }

    #[test]
    fn the_queue_is_emptied_when_the_turn_ends() {
        let mut a = app();
        a.apply(Event::SteerQueued { text: "in coda".into() });
        assert!(a.apply(Event::JobFinished { leftover_steers: Vec::new() }));
        assert!(a.queued.is_empty(), "a dead job's queue is nobody's");
    }

    #[test]
    fn a_leftover_steer_becomes_the_next_turn() {
        let mut a = app();
        a.apply(Event::JobFinished { leftover_steers: vec!["troppo tardi".into()] });
        assert_eq!(a.pending_turn.as_deref(), Some("troppo tardi"));
    }

    #[test]
    fn several_leftovers_are_joined_into_one_turn() {
        let mut a = app();
        a.apply(Event::JobFinished { leftover_steers: vec!["uno".into(), "due".into()] });
        assert_eq!(a.pending_turn.as_deref(), Some("uno\n\ndue"));
    }

    #[test]
    fn steering_without_a_running_job_says_so_once() {
        // `busy()` is false here, so this is the guard for a job id that vanished
        // rather than the normal path.
        let mut a = app();
        a.steer("ciao".into());
        assert_eq!(a.blocks.iter().filter(|b| matches!(b, Block::Notice { .. })).count(), 1);
    }

    #[test]
    fn the_steer_queue_is_drawn_above_the_prompt() {
        let mut a = app();
        a.apply(Event::SteerQueued { text: "aspetta un attimo".into() });
        let s = screen(&mut a, 60, 12).join("\n");
        assert!(s.contains("⤷ aspetta un attimo"), "queue strip not drawn:\n{s}");
    }

    #[test]
    fn a_sent_paste_is_echoed_by_its_opening_lines() {
        let mut a = app();
        let body = (1..=52).map(|i| format!("riga {i}")).collect::<Vec<_>>().join("\n");
        let chip = a.pastes.add_text(&body);
        a.ed.set(&format!("spiega {chip}"));
        keys::tests::press_enter(&mut a);

        let echo = a
            .blocks
            .iter()
            .rev()
            .find_map(|b| match b {
                Block::User(t) => Some(t.clone()),
                _ => None,
            })
            .expect("nothing echoed");
        assert!(echo.starts_with("spiega riga 1\nriga 2\nriga 3"), "got {echo:?}");
        assert!(echo.contains("+49 more lines"));
        assert!(!echo.contains("[Pasted"), "the chip is still what the transcript shows");
    }

    #[test]
    fn recalling_a_sent_paste_gives_back_its_text_not_a_dead_chip() {
        // The blobs are released on send, so a chip recalled from the history has
        // nothing behind it: submitting it again used to send the literal string
        // `[Pasted 52 lines]` to the model.
        let mut a = app();
        let body = (1..=52).map(|i| format!("riga {i}")).collect::<Vec<_>>().join("\n");
        let chip = a.pastes.add_text(&body);
        a.ed.set(&chip);
        keys::tests::press_enter(&mut a);

        let recalled = a.hist.prev("").expect("nothing in the history");
        a.recall(&recalled);
        // Back to a chip in the prompt, but one with the text behind it again.
        assert_eq!(a.ed.text(), "[Pasted 52 lines]");
        assert_eq!(a.pastes.expand(a.ed.text()), body, "the recalled chip is hollow");
    }

    #[test]
    fn the_up_arrow_walks_back_through_the_whole_session() {
        let mut a = app();
        for text in ["primo messaggio", "secondo messaggio", "terzo messaggio"] {
            a.ed.set(text);
            keys::tests::press_enter(&mut a);
        }
        keys::tests::press_up(&mut a);
        assert_eq!(a.ed.text(), "terzo messaggio");
        keys::tests::press_up(&mut a);
        assert_eq!(a.ed.text(), "secondo messaggio", "the up arrow stopped at the newest");
        keys::tests::press_up(&mut a);
        assert_eq!(a.ed.text(), "primo messaggio");
        keys::tests::press_down(&mut a);
        assert_eq!(a.ed.text(), "secondo messaggio");
    }

    /// What a real session costs the render thread.
    ///
    /// The wrap cache is keyed on a revision that every content change bumps, so
    /// a streamed token re-lays and re-wraps the *whole* transcript. That is fine
    /// at ten turns and not fine at five hundred, and the render thread is the
    /// one that also has to keep reading input.
    #[test]
    fn measure_the_cost_of_re_wrapping_a_long_session() {
        for turns in [10, 100, 400] {
            let mut a = app();
            for i in 0..turns {
                a.blocks.push(Block::User(format!("domanda {i}")));
                a.blocks.push(Block::Assistant(
                    "Una risposta di lunghezza plausibile, con abbastanza parole da \
                     andare a capo più volte in ottanta colonne. "
                        .repeat(8),
                ));
            }
            a.touch();
            let t = std::time::Instant::now();
            a.ensure_wrapped(80);
            let cold = t.elapsed();
            let lines = a.cache.len();

            // What a streamed token costs: content changed, so the cache is void.
            let t = std::time::Instant::now();
            for _ in 0..10 {
                a.touch();
                a.ensure_wrapped(80);
            }
            let per_frame = t.elapsed() / 10;

            println!(
                "{turns:4} turns · {lines:6} lines · first wrap {cold:>9.2?} · per frame {per_frame:>9.2?} → {:.0} fps ceiling",
                1.0 / per_frame.as_secs_f64()
            );
            // Deliberately loose — this is here to catch a tenfold regression, not
            // to police milliseconds on a machine of unknown speed. Measured at
            // ~15ms for 400 turns in release, ~10x that unoptimised.
            let budget = Duration::from_millis(if cfg!(debug_assertions) { 2000 } else { 200 });
            assert!(per_frame < budget, "{turns} turns re-wrap in {per_frame:?}");
        }
    }

    #[test]
    fn help_opens_a_picker_listing_commands_and_keys() {
        let mut a = app();
        with_goal_plugin(&mut a); // a plugin's command has to be in there too
        a.ed.set("/help");
        keys::tests::press_enter(&mut a);
        assert!(a.overlay_open());

        // Only a screenful shows at once; the rest is behind the search.
        let s = screen(&mut a, 100, 24).join("\n");
        for want in ["commands", "/clear", "/goal", "start a fresh session"] {
            assert!(s.contains(want), "{want:?} missing from the help picker:\n{s}");
        }

        let Some(Overlay::Pick(p)) = a.overlay.as_mut() else { panic!("no picker") };
        for c in "project file".chars() {
            p.insert(c);
        }
        let s = screen(&mut a, 100, 24).join("\n");
        assert!(s.contains("keys"), "the key section is unreachable:\n{s}");
        assert!(s.contains("complete a project file"), "{s}");
    }

    #[test]
    fn a_key_binding_in_the_help_list_cannot_be_picked() {
        // There is nothing to do with it, and enter on one would look broken.
        let mut a = app();
        a.open_help_picker();
        let Some(Overlay::Pick(p)) = a.overlay.as_mut() else { panic!("no picker") };
        for _ in 0..200 {
            p.down();
        }
        let last = p.selected().expect("nothing selectable").to_string();
        assert!(last.starts_with('/'), "the cursor landed on a key binding: {last:?}");
    }

    #[test]
    fn picking_a_command_from_help_puts_it_in_the_prompt() {
        // Rather than running it: the list holds /clear and /quit.
        let mut a = app();
        a.open_help_picker();
        let Some(Overlay::Pick(p)) = a.overlay.as_mut() else { panic!("no picker") };
        for c in "theme".chars() {
            p.insert(c);
        }
        a.overlay_submit();
        assert_eq!(a.ed.text(), "/theme ");
        assert!(!a.overlay_open(), "the picker should have closed");
    }

    #[test]
    fn searching_the_help_picker_matches_descriptions_too() {
        let mut a = app();
        a.open_help_picker();
        let Some(Overlay::Pick(p)) = a.overlay.as_mut() else { panic!("no picker") };
        for c in "clipboard".chars() {
            p.insert(c);
        }
        assert_eq!(p.selected(), Some("/image"), "found only by what it does");
    }

    #[test]
    fn recalling_a_command_does_not_end_the_walk_back() {
        // The recalled `/help` opens its own menu over the prompt, and the menu
        // used to take the arrows from there on — so the history stopped dead at
        // whatever the newest entry was.
        let mut a = app();
        a.hist.push("primo messaggio");
        a.hist.push("/expand");

        keys::tests::press_up(&mut a);
        assert_eq!(a.ed.text(), "/expand");
        assert!(a.menu_open(), "the command menu is up, as it should be");
        keys::tests::press_up(&mut a);
        assert_eq!(a.ed.text(), "primo messaggio", "the menu swallowed the arrow");
    }

    #[test]
    fn the_menu_gets_the_arrows_back_once_you_type() {
        let mut a = app();
        a.hist.push("/e");
        keys::tests::press_up(&mut a); // recalls "/e" — history owns the arrows
        assert!(a.hist.browsing());

        keys::tests::press_key(&mut a, 'x'); // editing makes it your own draft
        assert!(!a.hist.browsing());
        assert!(a.menu_rows().len() >= 2, "need a menu with somewhere to move");
        keys::tests::press_down(&mut a);
        assert_eq!(a.menu_sel, 1, "the menu should have taken the arrow back");
    }

    #[test]
    fn recalling_an_ordinary_prompt_leaves_it_as_it_was() {
        let mut a = app();
        a.recall("sistema il bug");
        assert_eq!(a.ed.text(), "sistema il bug");
    }

    #[test]
    fn the_same_notice_does_not_stack() {
        // Every refused enter used to leave another copy; a screenful of identical
        // warnings buries the conversation.
        let mut a = app();
        for _ in 0..20 {
            a.notice("uguale", Level::Warn);
        }
        assert_eq!(a.blocks.iter().filter(|b| matches!(b, Block::Notice { .. })).count(), 1);
        a.notice("diverso", Level::Warn);
        a.notice("uguale", Level::Warn);
        assert_eq!(a.blocks.iter().filter(|b| matches!(b, Block::Notice { .. })).count(), 3);
    }

    /// Pretend the project scan already landed.
    fn with_files(a: &mut App) {
        a.files.set(
            ["src/main.rs", "src/app/mod.rs", "README.md"].iter().map(|s| s.to_string()).collect(),
        );
    }

    #[test]
    fn an_at_opens_the_file_menu_and_tab_completes_it() {
        let mut a = app();
        with_files(&mut a);
        a.ed.set("@src/ma");
        assert!(a.menu_open());
        assert_eq!(a.menu_rows()[0].label, "@src/main.rs");

        keys::tests::press_tab(&mut a);
        assert_eq!(a.ed.text(), "@src/main.rs ");
    }

    #[test]
    fn typing_an_at_scans_the_real_project_off_thread() {
        // The whole path: pump spots the token, a thread shells out to git, and
        // the result lands back through the side channel. Tests run in the crate
        // directory, so the project it scans is lrs itself.
        let mut a = app();
        a.ed.set("@");
        a.pump();
        assert!(a.files.inflight, "no scan was started");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while a.files.list().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
            a.pump();
        }
        assert!(a.files.list().iter().any(|p| p == "src/app/files.rs"), "scan came back empty");
        assert!(!a.files.inflight);
    }

    #[test]
    fn an_idle_prompt_never_shells_out() {
        // The scan costs a subprocess; a session that never types an @ must not
        // start one on every tick.
        let mut a = app();
        a.ed.set("nessuna chiocciola");
        a.pump();
        assert!(!a.files.inflight);
    }

    #[test]
    fn the_file_menu_stays_shut_until_the_project_has_been_scanned() {
        let mut a = app();
        a.ed.set("@src");
        assert!(!a.menu_open(), "nothing to offer yet, so nothing should block enter");
    }

    #[test]
    fn a_slash_command_and_an_at_path_do_not_compete() {
        let mut a = app();
        with_files(&mut a);
        // A line starting with '/' is a command, even with an @ later in it.
        a.ed.set("/theme");
        assert_eq!(a.menu_rows()[0].label, "/theme");
    }

    #[test]
    fn a_fully_typed_path_sends_instead_of_completing_again() {
        let mut a = app();
        with_files(&mut a);
        a.ed.set("leggi @README.md");
        assert!(a.menu_open(), "the file is still the only match");
        assert!(a.menu_exact(), "enter would re-complete a path already typed in full");
    }

    #[test]
    fn enter_completes_a_half_typed_path_then_sends() {
        let mut a = app();
        with_files(&mut a);
        a.ed.set("leggi @READ");
        keys::tests::press_enter(&mut a);
        assert_eq!(a.ed.text(), "leggi @README.md ");
        keys::tests::press_enter(&mut a);
        assert!(a.ed.is_empty(), "the prompt was not submitted");
    }

    #[test]
    fn the_at_path_reaches_the_model_verbatim() {
        // Nothing here expands the reference: the system prompt tells the agent
        // that @path is a file to read, so the text is the whole contract.
        let mut a = app();
        with_files(&mut a);
        a.ed.set("spiega @src/main.rs");
        keys::tests::press_enter(&mut a);
        let sent = a.blocks.iter().rev().find_map(|b| match b {
            Block::User(text) => Some(text.clone()),
            _ => None,
        });
        assert_eq!(sent.as_deref(), Some("spiega @src/main.rs"));
    }

    #[test]
    fn a_fully_typed_builtin_sends_on_the_first_enter() {
        let mut a = app();
        a.ed.set("/expand");
        keys::tests::press_enter(&mut a);
        assert!(a.ed.is_empty());
        assert!(a.expand_all, "the command did not run");
    }

    #[test]
    fn free_text_after_a_command_is_not_held_by_the_menu() {
        let mut a = app();
        with_goal_plugin(&mut a);
        a.ed.set("/goal spedire la release");
        assert!(!a.menu_open(), "nothing matches, so nothing should block enter");
        keys::tests::press_enter(&mut a);
        assert!(a.ed.is_empty());
    }

    fn registry_fixture() -> lrs::protocol::Registry {
        use lrs::protocol::RegistryEntry;
        lrs::protocol::Registry {
            plugins: vec![
                RegistryEntry {
                    name: "goal".into(),
                    path: "/home/u/.lele/plugins/goal".into(),
                    state: "active".into(),
                    live: true,
                },
                // Turned off in config but still loaded: the reload hasn't happened.
                RegistryEntry {
                    name: "roast".into(),
                    path: "/home/u/.lele/plugins/roast".into(),
                    state: "disabled".into(),
                    live: true,
                },
            ],
            extensions: vec![RegistryEntry {
                name: "lele-pet".into(),
                path: "/home/u/.lele/extensions/lele-pet".into(),
                state: "active".into(),
                live: false,
            }],
            error: None,
        }
    }

    fn with_registry(a: &mut App) {
        a.side_tx.send(Side::Registry(Ok(registry_fixture()))).unwrap();
        a.pump();
    }

    #[test]
    fn the_plugin_picker_shows_what_is_loaded_and_what_is_configured() {
        let mut a = app();
        with_registry(&mut a);
        let rows = a.registry_rows(true);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].dot, Some(true));
        assert!(rows[0].note.is_empty(), "an active, loaded plugin needs no note");
        // disabled but still running: say so, or the picker looks like it lied
        assert_eq!(rows[1].dot, Some(true));
        assert!(rows[1].note.contains("/reload"), "got {:?}", rows[1].note);
    }

    #[test]
    fn an_enabled_but_not_yet_loaded_extension_says_it_needs_a_reload() {
        let mut a = app();
        with_registry(&mut a);
        let rows = a.registry_rows(false);
        assert_eq!(rows[0].dot, Some(false));
        assert!(rows[0].note.contains("/reload"), "got {:?}", rows[0].note);
    }

    #[test]
    fn the_extension_picker_lists_extensions_not_plugins() {
        let mut a = app();
        with_registry(&mut a);
        a.ed.set("/extension");
        a.submit();
        match a.overlay {
            Some(Overlay::Pick(ref p)) => {
                assert_eq!(p.kind, picker::Kind::Registry { plugins: false });
                assert_eq!(p.selected(), Some("lele-pet"));
            }
            _ => panic!("no picker"),
        }
    }

    #[test]
    fn the_picker_stays_up_after_a_toggle() {
        // Turning several things off in a row is the normal case.
        let mut a = app();
        with_registry(&mut a);
        a.ed.set("/plugin");
        a.submit();
        assert!(a.overlay_open());
        keys::tests::press_enter(&mut a);
        assert!(a.overlay_open(), "one toggle should not dismiss the list");
    }

    #[test]
    fn a_registry_picker_opened_before_the_list_lands_fills_in() {
        let mut a = app();
        a.open_registry_picker(true);
        assert!(matches!(a.overlay, Some(Overlay::Pick(ref p)) if p.loading));
        with_registry(&mut a);
        match a.overlay {
            Some(Overlay::Pick(ref p)) => {
                assert!(!p.loading);
                assert_eq!(p.selected(), Some("goal"));
            }
            _ => panic!("the picker closed instead of filling in"),
        }
    }

    #[test]
    fn a_reload_reports_how_much_came_back_up() {
        let mut a = app();
        a.side_tx.send(Side::Reloaded(Ok(registry_fixture()))).unwrap();
        a.pump();
        let said = format!("{:?}", a.blocks);
        assert!(said.contains("reloaded"), "no confirmation: {said}");
        assert!(a.registry_loaded);
    }

    #[test]
    fn a_failed_reload_says_so_instead_of_claiming_success() {
        let mut a = app();
        a.side_tx
            .send(Side::Reloaded(Ok(lrs::protocol::Registry {
                error: Some("plugin roast: SyntaxError".into()),
                ..Default::default()
            })))
            .unwrap();
        a.pump();
        assert!(matches!(a.blocks.last(), Some(Block::Notice { level: Level::Error, .. })));
    }

    #[test]
    fn plugin_output_reaches_the_transcript_without_its_markup() {
        // Handlers print Rich markup; the tags would otherwise show up literally.
        let mut a = app();
        a.side_tx
            .send(Side::Command {
                name: "/plugins".into(),
                result: Ok(lrs::protocol::CommandResult {
                    output: vec!["[bold green]●[/bold green] goal  attivo".into()],
                    ..Default::default()
                }),
            })
            .unwrap();
        a.pump();
        let said = format!("{:?}", a.blocks);
        assert!(said.contains("● goal  attivo"), "got {said}");
        assert!(!said.contains("[bold"), "raw markup leaked: {said}");
    }

    #[test]
    fn a_widget_set_during_a_turn_appears_at_once() {
        // The goal tool runs mid-turn; waiting for the next poll would show the
        // objective only after the answer had finished streaming.
        let mut a = app();
        a.apply(Event::Widget {
            id: "goal".into(),
            markup: Some("Pursuing goal ([bold cyan]…[/bold cyan] spedire)".into()),
        });
        assert_eq!(a.widgets.len(), 1);
        assert_eq!(a.widgets[0].id, "goal");

        let joined = screen(&mut a, 60, 14).join("\n");
        assert!(joined.contains("Pursuing goal (… spedire)"), "not on screen:\n{joined}");
        assert!(!joined.contains("[bold"), "raw markup leaked:\n{joined}");
    }

    #[test]
    fn clearing_a_widget_takes_its_row_back() {
        let mut a = app();
        a.apply(Event::Widget { id: "goal".into(), markup: Some("x".into()) });
        assert_eq!(a.widgets.len(), 1);
        a.apply(Event::Widget { id: "goal".into(), markup: None });
        assert!(a.widgets.is_empty());
        let joined = screen(&mut a, 60, 14).join("\n");
        assert!(!joined.contains('▎'), "the strip is still drawn:\n{joined}");
    }

    #[test]
    fn a_plugin_commands_output_lands_in_the_transcript_with_its_widget() {
        let mut a = app();
        a.side_tx
            .send(Side::Command {
                name: "/goal".into(),
                result: Ok(lrs::protocol::CommandResult {
                    output: vec!["Goal set: spedire".into()],
                    unsupported: Vec::new(),
                    error: None,
                    widgets: vec![lrs::protocol::WidgetRow {
                        id: "goal".into(),
                        markup: "Pursuing goal (… spedire)".into(),
                    }],
                    session_id: "20250101_120000_abcdef".into(),
                }),
            })
            .unwrap();
        a.pump();
        let said = format!("{:?}", a.blocks);
        assert!(said.contains("Goal set: spedire"), "output missing: {said}");
        assert_eq!(a.widgets.len(), 1, "the widget the handler set is missing");
    }

    #[test]
    fn a_command_run_before_the_first_turn_adopts_the_session_it_was_given() {
        // Otherwise the goal the handler just set belongs to a conversation this
        // frontend never continues, and vanishes on the next turn.
        let mut a = app();
        assert!(a.session_id.is_none());
        a.side_tx
            .send(Side::Command {
                name: "/goal".into(),
                result: Ok(lrs::protocol::CommandResult {
                    output: vec!["Goal set: x".into()],
                    unsupported: Vec::new(),
                    error: None,
                    widgets: Vec::new(),
                    session_id: "20250101_120000_abcdef".into(),
                }),
            })
            .unwrap();
        a.pump();
        assert_eq!(a.session_id.as_deref(), Some("20250101_120000_abcdef"));
    }

    #[test]
    fn an_existing_session_is_not_replaced_by_a_command() {
        let mut a = app();
        a.session_id = Some("mia".into());
        a.side_tx
            .send(Side::Command {
                name: "/goal".into(),
                result: Ok(lrs::protocol::CommandResult {
                    session_id: "altra".into(),
                    ..Default::default()
                }),
            })
            .unwrap();
        a.pump();
        assert_eq!(a.session_id.as_deref(), Some("mia"));
    }

    #[test]
    fn a_new_session_drops_the_plugin_panels() {
        // They belong to the conversation that set them.
        let mut a = app();
        a.apply(Event::Widget { id: "goal".into(), markup: Some("vecchio".into()) });
        a.ed.set("/clear");
        a.submit();
        assert!(a.widgets.is_empty());
    }

    #[test]
    fn a_command_needing_the_textual_tui_says_so_instead_of_half_running() {
        let mut a = app();
        a.side_tx
            .send(Side::Command {
                name: "/diff".into(),
                result: Ok(lrs::protocol::CommandResult {
                    output: Vec::new(),
                    unsupported: vec!["push_screen".into()],
                    error: None,
                    widgets: Vec::new(),
                    session_id: String::new(),
                }),
            })
            .unwrap();
        a.pump();
        let said = format!("{:?}", a.blocks);
        assert!(said.contains("push_screen"), "silently did nothing: {said}");
        assert!(matches!(a.blocks.last(), Some(Block::Notice { level: Level::Warn, .. })));
    }

    #[test]
    fn a_modal_is_drawn_over_the_transcript() {
        let mut a = app();
        a.blocks.clear();
        a.blocks.push(Block::User("una domanda".into()));
        a.touch();
        assert!(screen(&mut a, 60, 16).join("\n").contains("una domanda"));

        a.open_effort_picker();
        let joined = screen(&mut a, 60, 16).join("\n");
        assert!(joined.contains("Reasoning effort"), "no modal drawn:\n{joined}");
        assert!(joined.contains("medium"), "the options are missing:\n{joined}");
        assert!(joined.contains("esc"), "no way out shown:\n{joined}");
    }

    #[test]
    fn bare_theme_opens_the_picker_and_choosing_applies_it() {
        let _env = isolated("themepicker");
        let mut a = app();
        a.ed.set("/theme");
        keys::tests::press_enter(&mut a);
        assert!(a.overlay_open(), "enter on a bare /theme should open the picker");

        let joined = screen(&mut a, 60, 16).join("\n");
        assert!(joined.contains("Select theme"), "no theme picker drawn:\n{joined}");
        assert!(joined.contains("nord"), "the themes are missing:\n{joined}");
        // the theme in use is marked, so you can tell where you are
        assert!(joined.contains('●'), "nothing marks the current theme:\n{joined}");

        // and picking one actually switches
        if let Some(Overlay::Pick(p)) = a.overlay.as_mut() {
            for c in "nord".chars() {
                p.insert(c);
            }
        }
        keys::tests::press_enter(&mut a);
        assert!(!a.overlay_open());
        assert_eq!(a.theme.name, "nord");
    }

    #[test]
    fn a_trailing_space_still_opens_the_theme_picker() {
        // `/theme` completes to "/theme " from the menu, so the very next enter
        // must open the picker rather than being read as an empty argument.
        let _env = isolated("themespace");
        let mut a = app();
        a.ed.set("/theme ");
        keys::tests::press_enter(&mut a);
        assert!(a.overlay_open());
    }

    #[test]
    fn the_key_prompt_never_echoes_the_key() {
        let mut a = app();
        with_catalog(&mut a);
        a.open_connect("anthropic");
        if let Some(Overlay::Ask(ask)) = a.overlay.as_mut() {
            ask.insert_str("sk-super-secret");
        }
        let joined = screen(&mut a, 100, 16).join("\n");
        assert!(!joined.contains("sk-super"), "the key was rendered:\n{joined}");
        assert!(joined.contains("•••"), "no masked echo at all:\n{joined}");
        assert!(joined.contains("ANTHROPIC_API_KEY"), "doesn't say what it wants:\n{joined}");
    }

    #[test]
    fn a_modal_survives_a_terminal_too_small_for_it() {
        // Drawing outside the frame panics ratatui, so this asserts on not crashing.
        let mut a = app();
        with_catalog(&mut a);
        a.open_model_picker();
        screen(&mut a, 20, 6);
        a.open_connect("anthropic");
        screen(&mut a, 12, 4);
    }

    #[test]
    fn the_opening_banner_lists_what_the_server_can_do() {
        let mut a = app();
        let s = screen(&mut a, 70, 20).join("\n");
        assert!(s.contains("tools"), "no tools section:\n{s}");
        assert!(s.contains("read_file"), "tool names missing:\n{s}");
        assert!(s.contains("goal"), "plugins missing:\n{s}");
        // empty sections are dropped, not shown as headings with nothing under them
        assert!(!s.contains("extensions"), "empty section rendered:\n{s}");
    }

    #[test]
    fn the_model_picker_finds_a_group_and_a_fragment_typed_together() {
        let mut a = app();
        a.catalog.providers = vec![
            Provider { name: "llamacpp".into(), connected: true, ..Default::default() },
            Provider { name: "openrouter".into(), connected: true, ..Default::default() },
        ];
        a.catalog.models = vec![
            "llamacpp/hy-local/x.gguf".into(),
            "z-ai/glm-4.6".into(),
            "tencent/hy4-flash".into(),
        ];
        let mut picker = Picker::new(picker::Kind::Model, "Select model", a.model_rows());
        for c in "openrouter hy".chars() {
            picker.insert(c);
        }
        assert_eq!(
            picker.selected(), Some("tencent/hy4-flash"),
            "`hy` alone would also fit the local model; the group word narrows it",
        );
    }

    #[test]
    fn a_local_servers_models_are_listed_under_it_not_under_openrouter() {
        let mut a = app();
        a.catalog.providers = vec![
            Provider { name: "llamacpp".into(), connected: true, ..Default::default() },
            Provider { name: "openrouter".into(), connected: true, ..Default::default() },
        ];
        a.catalog.models = vec![
            "thinkingmachines/inkling-2-mini".into(),
            "llamacpp/inkling/Inkling-2-Mini-UD-Q4_K_M.gguf".into(),
        ];
        let rows = a.model_rows();
        let headings: Vec<&str> = rows.iter().filter(|r| r.id.is_none()).map(|r| r.label.as_str()).collect();
        assert_eq!(headings, vec!["llamacpp", "openrouter"], "the local server has its own group");
        let local = rows.iter()
            .find(|r| r.id.as_deref() == Some("llamacpp/inkling/Inkling-2-Mini-UD-Q4_K_M.gguf"))
            .unwrap();
        assert_eq!(local.label.trim(), "inkling/Inkling-2-Mini-UD-Q4_K_M.gguf");
    }

    #[test]
    fn a_reply_reports_the_window_and_how_much_of_it_is_used() {
        let mut a = app();
        a.status.context_limit = 0;
        a.apply(Event::Usage { context: 9_000, turn_input: 8_000, turn_output: 1_000, context_limit: 32_768 });
        assert_eq!(a.status.context, 9_000);
        assert_eq!(a.status.context_limit, 32_768);

        // A backend that does not know the window must not wipe one already known.
        a.apply(Event::Usage { context: 10_000, turn_input: 0, turn_output: 0, context_limit: 0 });
        assert_eq!(a.status.context_limit, 32_768);
    }

    #[test]
    fn the_context_limit_from_health_reaches_the_gauge() {
        let mut a = app();
        assert_eq!(a.status.context_limit, 200_000);
        a.apply(Event::Usage { context: 20_000, turn_input: 0, turn_output: 0, context_limit: 0 });
        let s = screen(&mut a, 70, 10);
        assert!(s[9].contains("20.0k/200.0k"), "status row was {:?}", s[9]);
    }

    #[test]
    fn a_thinking_event_is_drawn_above_the_answer() {
        // Whether any thinking shows up at runtime is the provider's call — many
        // never stream reasoning deltas at all. This pins down that when one does
        // arrive, it reaches the screen labelled and separate from the reply.
        let mut a = app();
        a.blocks.clear();
        a.touch();
        a.apply(Event::Thinking { text: "17 non ha divisori".into(), run_id: None });
        a.apply(Event::Token { text: "primo".into(), run_id: None });
        a.touch();

        let s = screen(&mut a, 60, 12).join("\n");
        assert!(s.contains("thinking"), "no thinking label:\n{s}");
        assert!(s.contains("17 non ha divisori"), "reasoning text missing:\n{s}");
        let think_at = s.find("17 non ha").unwrap();
        let answer_at = s.find("primo").unwrap();
        assert!(think_at < answer_at, "thinking must precede the answer:\n{s}");
    }

    /// Put the app in the state a live turn produces, without a server.
    fn make_busy(a: &mut App) -> mpsc::Sender<Event> {
        let (tx, rx) = mpsc::channel();
        a.turn = Some(run::Turn { job_id: "job".into(), events: rx, stopping: false });
        tx
    }

    /// What `!cmd` hands to the runner.
    fn bang(command: &str) -> shell::Job {
        shell::Job { title: shell_title(command), command: command.into(), to_model: true }
    }

    fn user_command(name: &str, run: &str, to_model: bool) -> user_commands::UserCommand {
        user_commands::UserCommand {
            name: name.into(), desc: String::new(), run: run.into(), to_model, sub: Default::default(),
        }
    }

    #[test]
    fn a_user_command_runs_its_shell_line_with_the_arguments_and_shows_under_its_own_name() {
        let mut a = app();
        a.blocks.clear();
        a.user_commands = vec![user_command("/inkling", "echo inkling", false)];
        a.commands = a.commands.clone().with_user(&a.user_commands);
        a.ed.set("/inkling start");
        a.submit();
        assert!(a.shell.is_some(), "it runs like a `!` command, so esc stops it");
        assert!(matches!(a.blocks.last(), Some(Block::Tool { title, .. }) if title == "/inkling start"));
    }

    #[test]
    fn a_user_command_is_not_sent_to_the_model_unless_its_file_says_so() {
        use shell::ShellResult;
        let finished = || Ok(ShellResult::Finished { text: "ok".into(), failed: false });
        let job = |to_model| shell::Job { title: "/x".into(), command: "x".into(), to_model };

        let mut quiet = app();
        quiet.blocks.push(Block::Tool {
            title: "/x".into(), body: None, kind: Body::Plain, failed: false,
            started: Some(std::time::Instant::now()), took: None,
        });
        quiet.shell = Some(run::ShellRun::default());
        quiet.shell_finished(job(false), finished());
        assert!(quiet.shell_ran.is_empty(), "nothing queued for the model");
        assert!(matches!(&quiet.blocks.last(), Some(Block::Tool { body: Some(b), .. }) if b == "ok"));

        let mut told = app();
        told.shell = Some(run::ShellRun::default());
        told.shell_finished(job(true), finished());
        // Nothing listens on the test client's port, so the turn it starts fails
        // and the output is kept for the next one.
        assert_eq!(told.shell_ran.len(), 1, "the model is to be told");
    }

    #[test]
    fn a_user_command_never_shadows_a_built_in() {
        let user = [user_command("/quit", "true", false), user_command("/inkling", "true", false)];
        let table = commands::Table::builtin();
        assert_eq!(table.clashes(&user), vec!["/quit"]);
        let merged = table.with_user(&user);
        assert_eq!(merged.get("/quit").unwrap().source, commands::Source::Builtin);
        assert_eq!(merged.get("/inkling").unwrap().source, commands::Source::User);
    }

    #[test]
    fn plugin_commands_arriving_later_do_not_drop_the_users() {
        let mut a = app();
        a.user_commands = vec![user_command("/inkling", "true", false)];
        a.side_tx
            .send(Side::Plugins { commands: vec![goal_spec()], widgets: Vec::new() })
            .unwrap();
        a.pump_side();
        assert!(a.commands.get("/goal").is_some());
        assert_eq!(a.commands.get("/inkling").unwrap().source, commands::Source::User);
    }

    #[test]
    fn a_finished_command_fills_its_block_and_is_kept_for_the_next_turn() {
        use shell::ShellResult;
        let mut a = app();
        a.blocks.clear();
        a.shell = Some(run::ShellRun::default());
        a.blocks.push(Block::Tool {
            title: shell_title("ls"), body: None, kind: Body::Plain, failed: false,
            started: Some(std::time::Instant::now()), took: None,
        });
        a.shell_finished(bang("ls"), Ok(ShellResult::Finished { text: "a.txt".into(), failed: false }));

        assert!(a.shell.is_none(), "the command is over");
        assert!(matches!(&a.blocks[0], Block::Tool { body: Some(b), failed: false, .. } if b == "a.txt"));
        // Nothing listens on the test client's port, so the turn it starts here
        // fails, and the output has to survive that.
        assert!(matches!(a.blocks.last(), Some(Block::Notice { level: Level::Error, .. })),
            "a finished command starts a turn");
        assert_eq!(a.shell_ran.len(), 1);
        assert_eq!(a.shell_ran[0].command, "ls");
    }

    #[test]
    fn a_command_that_ends_during_a_turn_waits_for_the_next_one() {
        use shell::ShellResult;
        let mut a = app();
        let _tx = make_busy(&mut a);
        a.shell = Some(run::ShellRun::default());
        a.shell_finished(bang("ls"), Ok(ShellResult::Finished { text: "a".into(), failed: false }));
        assert!(!a.blocks.iter().any(|b| matches!(b, Block::Notice { level: Level::Error, .. })),
            "starting a second turn would have failed here, and said so");
        assert_eq!(a.shell_ran.len(), 1, "kept for the next turn");
    }

    #[test]
    fn a_bang_on_an_empty_prompt_puts_the_bang_where_the_arrow_is() {
        let mut a = app();
        a.blocks.clear();
        keys::tests::press_key(&mut a, '!');
        for c in "ls -la".chars() {
            keys::tests::press_key(&mut a, c);
        }
        assert_eq!(a.ed.text(), "ls -la", "the bang is the mode, not part of the text");
        let rows = screen(&mut a, 60, 8);
        let prompt = rows.iter().find(|l| l.contains("ls -la")).expect("the prompt is drawn");
        assert!(prompt.starts_with("! ls -la"), "{prompt:?}");
        assert!(rows.last().unwrap().contains("shell mode"), "the mode is named under the prompt: {rows:?}");

        a.ed.leave_shell();
        let rows = screen(&mut a, 60, 8);
        let prompt = rows.iter().find(|l| l.contains("ls -la")).expect("the prompt is drawn");
        assert!(prompt.starts_with("❯ "), "a message keeps the usual prompt: {prompt:?}");
        assert!(!rows.last().unwrap().contains("shell mode"));
    }

    #[test]
    fn a_command_is_run_and_remembered_with_its_bang() {
        let mut a = app();
        a.blocks.clear();
        a.ed.enter_shell();
        a.ed.set("echo hi");
        a.submit();
        assert!(!a.ed.shell_mode(), "sending ends the command prompt");
        assert!(a.shell.is_some());
        assert!(matches!(a.blocks.last(), Some(Block::Tool { title, .. }) if *title == shell_title("echo hi")));

        a.recall("!echo hi");
        assert!(a.ed.shell_mode());
        assert_eq!(a.ed.text(), "echo hi");
        a.recall("plain message");
        assert!(!a.ed.shell_mode());
    }

    #[test]
    fn the_command_menu_stays_closed_while_typing_a_command() {
        let mut a = app();
        a.ed.enter_shell();
        a.ed.set("/usr/bin/env");
        assert!(!a.menu_open(), "a path is not a slash command");
    }

    #[test]
    fn an_empty_command_prompt_is_left_with_backspace_escape_or_ctrl_c() {
        use crossterm::event::{KeyCode, KeyModifiers};
        for leave in [KeyCode::Backspace, KeyCode::Esc] {
            let mut a = app();
            keys::tests::press_key(&mut a, '!');
            assert!(a.ed.shell_mode());
            keys::tests::press_code(&mut a, leave, KeyModifiers::NONE);
            assert!(!a.ed.shell_mode());
            assert!(!a.should_quit);
        }
        let mut a = app();
        keys::tests::press_key(&mut a, '!');
        keys::tests::press_code(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!a.ed.shell_mode() && !a.should_quit, "the first ^c leaves the mode, not the app");
    }

    #[test]
    fn a_command_redraws_the_transcript_when_it_starts_and_when_it_ends() {
        use shell::ShellResult;
        let mut a = app();
        a.shell = Some(run::ShellRun::default());
        a.blocks.push(Block::Tool {
            title: shell_title("ls"), body: None, kind: Body::Plain, failed: false,
            started: Some(std::time::Instant::now()), took: None,
        });
        let before = a.revision;
        a.shell_finished(bang("ls"), Ok(ShellResult::Finished { text: "a".into(), failed: false }));
        assert_ne!(a.revision, before, "the result would stay invisible until the next key");

        let started = a.revision;
        a.run_shell("ls".into());
        assert_ne!(a.revision, started, "the running command must show at once");
    }

    #[test]
    fn a_stopped_or_unstartable_command_tells_the_model_nothing() {
        use shell::ShellResult;
        for result in [Ok(ShellResult::Stopped), Err("no shell".to_string())] {
            let mut a = app();
            a.shell = Some(run::ShellRun::default());
            a.shell_finished(bang("sleep 9"), result);
            assert!(a.shell_ran.is_empty());
            assert!(a.shell.is_none());
        }
    }

    #[test]
    fn only_one_command_runs_at_a_time() {
        let mut a = app();
        let running: std::sync::Arc<std::sync::atomic::AtomicBool> = Default::default();
        a.shell = Some(run::ShellRun { stop: running.clone(), stopping: false });
        let blocks = a.blocks.len();
        a.run_shell("ls".into());
        assert_eq!(a.blocks.len() - blocks, 1, "only the warning, no second block");
        assert!(matches!(a.blocks.last(), Some(Block::Notice { .. })));
    }

    #[test]
    fn a_request_to_stop_ends_with_the_turn_it_was_for() {
        let mut a = app();
        let tx = make_busy(&mut a);
        a.turn.as_mut().unwrap().stopping = true;
        assert!(a.cancelling());
        tx.send(Event::JobFinished { leftover_steers: Vec::new() }).unwrap();
        a.pump();
        assert!(!a.busy(), "the turn is over");
        assert!(!a.cancelling(), "and the next one starts with no stop pending");
    }

    #[test]
    fn escape_stops_a_running_command() {
        let mut a = app();
        let running: std::sync::Arc<std::sync::atomic::AtomicBool> = Default::default();
        a.shell = Some(run::ShellRun { stop: running.clone(), stopping: false });
        assert!(a.working() && !a.busy(), "a command is not a turn");
        keys::tests::press_esc(&mut a);
        assert!(running.load(std::sync::atomic::Ordering::SeqCst));
        assert!(a.cancelling());
    }

    #[test]
    fn a_turn_that_cannot_start_keeps_the_commands_for_the_next_one() {
        let mut a = app();
        a.shell_ran.push(shell::Ran { command: "ls".into(), output: "a.txt".into() });
        // Nothing listens on the test client's port, so the turn never starts.
        a.ed.set("what is in there?");
        a.submit();
        assert_eq!(a.shell_ran.len(), 1, "the output must not be lost with the failed turn");
    }

    #[test]
    fn the_working_line_shows_only_while_a_turn_runs() {
        let mut a = app();
        a.blocks.clear();
        a.touch();
        // Height 8 → transcript rows 0..=4, divider 5, prompt 6, status 7.
        let idle = screen(&mut a, 60, 8);
        assert!(idle[4].trim().is_empty(), "idle row not blank: {:?}", idle[4]);

        let _tx = make_busy(&mut a);
        let busy = screen(&mut a, 60, 8);
        // The spinner owns the bottom transcript row, just above the divider.
        let row = &busy[4];
        assert!(
            row.chars().any(|c| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c)),
            "no spinner on the working row: {row:?}"
        );
    }

    #[test]
    fn the_working_line_animates_between_frames() {
        let mut a = app();
        let _tx = make_busy(&mut a);
        let first = screen(&mut a, 60, 8)[4].clone();
        let second = screen(&mut a, 60, 8)[4].clone();
        assert_ne!(first, second, "the line is frozen; it would read as a hang");
    }

    #[test]
    fn the_working_line_does_not_eat_the_transcript() {
        let mut a = app();
        a.blocks.clear();
        a.blocks.push(Block::User("visibile".into()));
        a.touch();
        let _tx = make_busy(&mut a);
        let s = screen(&mut a, 60, 8).join("\n");
        assert!(s.contains("visibile"), "transcript lost to the spinner:\n{s}");
    }

    #[test]
    fn a_one_row_transcript_pane_keeps_the_transcript() {
        // Squeezed to nothing there is no room to reserve; content wins over chrome.
        let mut a = app();
        let _tx = make_busy(&mut a);
        let _ = screen(&mut a, 40, 4);
    }

    fn todo_event(items: serde_json::Value) -> Event {
        Event::ToolFinished { name: "todo_write".into(), args: serde_json::json!({"items": items}), output: "ok".into(), failed: false, run_id: None }
    }

    /// What the todo package publishes: the rendered checklist, as a plugin panel
    /// keyed `todo`. This client no longer builds that markup itself.
    fn todo_panel(markup: &str) -> Event {
        Event::Widget { id: "todo".into(), markup: Some(markup.into()) }
    }

    #[test]
    fn a_second_todo_call_replaces_the_first_instead_of_stacking() {
        let mut a = app();
        let before = a.blocks.len();
        a.apply(Event::ToolStarted { name: "todo_write".into(), args: serde_json::json!({}), run_id: None });
        a.apply(todo_event(serde_json::json!([{"content": "fare", "status": "pending"}])));
        a.apply(Event::ToolStarted { name: "todo_write".into(), args: serde_json::json!({}), run_id: None });
        a.apply(todo_event(serde_json::json!([{"content": "fare", "status": "completed"}])));

        assert_eq!(a.blocks.len(), before, "todo must not append transcript blocks");
        // The panel is keyed, so a second publish replaces the first rather than
        // stacking — the property this test has always been about.
        a.apply(todo_panel("todo 0/1\n[ ] fare"));
        a.apply(todo_panel("todo 1/1\n[x] fare"));
        assert_eq!(a.widgets.len(), 1);
    }

    #[test]
    fn the_checklist_is_pinned_above_the_prompt() {
        let mut a = app();
        a.blocks.clear();
        a.touch();
        a.apply(todo_panel("todo 1/2\n[x] primo passo\n[~] secondo passo"));
        let s = screen(&mut a, 60, 14);
        let joined = s.join("\n");
        assert!(joined.contains("todo 1/2"), "no header:\n{joined}");
        assert!(joined.contains("secondo passo"), "items missing:\n{joined}");
        // and it sits below the transcript, not inside it
        let todo_row = s.iter().position(|l| l.contains("todo 1/2")).unwrap();
        let prompt_row = s.iter().position(|l| l.contains("ask anything")).unwrap();
        assert!(todo_row < prompt_row, "checklist should be above the prompt");
    }

    #[test]
    fn clearing_it_removes_the_panel() {
        let mut a = app();
        a.apply(todo_panel("todo 0/1\n[ ] x"));
        assert!(!a.widgets.is_empty());
        a.apply(Event::Widget { id: "todo".into(), markup: None });
        assert!(a.widgets.is_empty());
        assert!(!screen(&mut a, 60, 14).join("\n").contains("todo "));
    }

    #[test]
    fn a_todo_between_two_real_tools_does_not_steal_their_slots() {
        // The FIFO pairs each finish with the oldest unfinished tool. A todo that
        // left a pending slot behind would make the next result land in it.
        let mut a = app();
        a.apply(Event::ToolStarted { name: "read_file".into(), args: serde_json::json!({"path": "a"}), run_id: None });
        a.apply(Event::ToolStarted { name: "todo_write".into(), args: serde_json::json!({}), run_id: None });
        a.apply(todo_event(serde_json::json!([{"content": "x", "status": "pending"}])));
        a.apply(Event::ToolFinished {
            name: "read_file".into(),
            args: serde_json::json!({"path": "a"}),
            output: "contenuto".into(),
            failed: false,
            run_id: None,
        });
        let filled = a.blocks.iter().any(
            |b| matches!(b, Block::Tool { title, body: Some(s), .. } if title.contains('a') && s == "contenuto"),
        );
        assert!(filled, "read_file lost its output: {:?}", a.blocks);
    }

    #[test]
    fn a_new_session_drops_the_checklist() {
        let mut a = app();
        a.apply(todo_panel("todo 0/1\n[ ] x"));
        a.ed.set("/clear");
        a.submit();
        assert!(a.widgets.is_empty(), "a stale checklist would carry into the new session");
    }

    fn child_started(a: &mut App, run: &str, agent: &str) {
        a.apply(Event::AgentStarted {
            agent_id: agent.into(),
            parent_id: Some("main".into()),
            run_id: Some(run.into()),
        });
    }

    #[test]
    fn a_sub_agent_gets_its_own_block_not_the_parents_tool_slot() {
        let mut a = app();
        let before = a.blocks.len();
        // The parent's subagent call must not draw a block: the child's own block
        // is the UI, and two would show the same work twice.
        a.apply(Event::ToolStarted {
            name: "subagent".into(),
            args: serde_json::json!({"agent": "explore"}),
            run_id: None,
        });
        assert_eq!(a.blocks.len(), before, "parent drew a duplicate block");

        child_started(&mut a, "r1", "explore");
        let labelled = a.blocks.iter().any(
            |b| matches!(b, Block::Agent { label, .. } if label == "Agent(explore)"),
        );
        assert!(labelled, "expected Agent(explore), got {:?}", a.blocks.last());
    }

    #[test]
    fn child_tokens_stream_into_the_child_block_not_the_answer() {
        let mut a = app();
        child_started(&mut a, "r1", "explore");
        a.apply(Event::Token { text: "riga uno\n".into(), run_id: Some("r1".into()) });
        a.apply(Event::Token { text: "riga due".into(), run_id: Some("r1".into()) });
        // ...while the parent's own tokens stay separate.
        a.apply(Event::Token { text: "conclusione".into(), run_id: None });

        let agent_text = a.blocks.iter().find_map(
            |b| match b { Block::Agent { text, .. } => Some(text.clone()), _ => None },
        );
        assert_eq!(agent_text.as_deref(), Some("riga uno\nriga due"));
        assert!(
            a.blocks.iter().any(|b| matches!(b, Block::Assistant(s) if s == "conclusione")),
            "parent output was swallowed by the child block"
        );
    }

    #[test]
    fn a_long_child_report_is_truncated_to_its_tail() {
        let mut a = app();
        a.blocks.clear();
        child_started(&mut a, "r1", "explore");
        for i in 0..60 {
            a.apply(Event::Token { text: format!("riga {i}\n"), run_id: Some("r1".into()) });
        }
        a.touch();

        let s = screen(&mut a, 60, 20).join("\n");
        assert!(s.contains("riga 59"), "the live edge must be visible:\n{s}");
        assert!(!s.contains("riga 0\n"), "the head should be folded away:\n{s}");
        assert!(s.contains("earlier lines (^o)"), "no expand hint:\n{s}");
    }

    #[test]
    fn ctrl_o_reveals_the_whole_child_report() {
        let mut a = app();
        a.blocks.clear();
        child_started(&mut a, "r1", "explore");
        for i in 0..60 {
            a.apply(Event::Token { text: format!("riga {i}\n"), run_id: Some("r1".into()) });
        }
        a.touch();
        let collapsed = chat::lines(&a.blocks, &a.theme, false, 80).len();
        let expanded = chat::lines(&a.blocks, &a.theme, true, 80).len();
        assert!(expanded > collapsed + 40, "expand did not reveal the body");
    }

    #[test]
    fn a_sub_agents_markdown_is_rendered_not_shown_raw() {
        let mut a = app();
        a.blocks.clear();
        child_started(&mut a, "r1", "explore");
        a.apply(Event::Token {
            text: "**Verificato:**\n- `src/app/` esiste\n".into(),
            run_id: Some("r1".into()),
        });
        a.touch();

        let s = screen(&mut a, 70, 16).join("\n");
        assert!(s.contains("Verificato:"), "text missing:\n{s}");
        assert!(!s.contains("**"), "raw bold markers leaked:\n{s}");
        assert!(!s.contains('`'), "raw backticks leaked:\n{s}");
        assert!(s.contains('•'), "list not rendered:\n{s}");
    }

    #[test]
    fn the_boundary_between_a_sub_agent_and_the_answer_is_visible() {
        let mut a = app();
        a.blocks.clear();
        child_started(&mut a, "r1", "explore");
        a.apply(Event::Token { text: "quel che ho trovato".into(), run_id: Some("r1".into()) });
        a.apply(Event::AgentFinished { result: String::new(), run_id: Some("r1".into()) });
        a.apply(Event::Token { text: "la mia sintesi".into(), run_id: None });
        a.touch();

        let s = screen(&mut a, 70, 16);
        let joined = s.join("\n");
        let child = s.iter().position(|l| l.contains("quel che ho trovato")).unwrap();
        let close = s.iter().position(|l| l.trim_start().starts_with('╰')).expect(
            &format!("no closing edge:\n{joined}"),
        );
        let parent = s.iter().position(|l| l.contains("la mia sintesi")).unwrap();
        assert!(child < close && close < parent, "boundary out of order:\n{joined}");
    }

    #[test]
    fn a_failed_turn_without_streamed_tokens_still_shows_the_error() {
        // A 3-attempt API failure streams nothing: the error text arrives only
        // in agent_finished.result and must not end the turn in silence.
        let mut a = app();
        a.blocks.clear();
        a.blocks.push(Block::User("domanda".into()));
        a.apply(Event::AgentFinished {
            result: "[turn stopped: API call failed after 3 attempts: 429]".into(),
            run_id: None,
        });
        assert_eq!(
            last_text(&a),
            "[turn stopped: API call failed after 3 attempts: 429]",
            "the error must be visible in the transcript"
        );
    }

    #[test]
    fn agent_finished_does_not_duplicate_a_streamed_reply() {
        // A normal turn already showed the reply as streamed tokens; the
        // authoritative result arriving after it must not be appended twice.
        let mut a = app();
        a.blocks.clear();
        a.apply(Event::Token { text: "ciao".into(), run_id: None });
        a.apply(Event::AgentFinished { result: "ciao".into(), run_id: None });
        assert_eq!(a.blocks.len(), 1, "the reply must not be duplicated");
        assert_eq!(last_text(&a), "ciao");
    }

    #[test]
    fn every_wrapped_row_of_a_sub_agent_keeps_its_gutter() {
        // The gutter is a prefix span; if the body were wrapped afterwards the
        // continuation rows would fall outside it and the block would look open.
        let mut a = app();
        a.blocks.clear();
        child_started(&mut a, "r1", "explore");
        a.apply(Event::Token { text: "parola ".repeat(40), run_id: Some("r1".into()) });
        a.touch();

        let s = screen(&mut a, 40, 20);
        let body: Vec<&String> = s.iter().filter(|l| l.contains("parola")).collect();
        assert!(body.len() > 2, "expected several wrapped rows, got {}", body.len());
        for l in body {
            assert!(l.trim_start().starts_with('│'), "row escaped the gutter: {l:?}");
        }
    }

    #[test]
    fn a_child_tool_call_is_narrated_inside_the_child_block() {
        let mut a = app();
        child_started(&mut a, "r1", "explore");
        a.apply(Event::ToolStarted {
            name: "read_file".into(),
            args: serde_json::json!({"path": "src/a.rs"}),
            run_id: Some("r1".into()),
        });
        let text = a.blocks.iter().find_map(
            |b| match b { Block::Agent { text, .. } => Some(text.clone()), _ => None },
        ).unwrap_or_default();
        assert!(text.contains("read_file  src/a.rs"), "got {text:?}");
        // and it must not appear as one of the parent's tools
        assert!(!a.blocks.iter().any(|b| matches!(b, Block::Tool { .. })));
    }

    #[test]
    fn a_failed_subagent_still_surfaces() {
        // The child block is the normal UI, but a failure never got one.
        let mut a = app();
        a.apply(Event::ToolFinished {
            name: "subagent".into(),
            args: serde_json::json!({"agent": "explore"}),
            output: "error: unknown agent".into(),
            failed: false,
            run_id: None,
        });
        assert!(
            a.blocks.iter().any(|b| matches!(b, Block::Notice { level: Level::Error, .. })),
            "a failed subagent vanished"
        );
    }

    #[test]
    fn child_output_arriving_before_its_start_event_is_not_lost() {
        let mut a = app();
        a.apply(Event::Token { text: "orfano".into(), run_id: Some("r9".into()) });
        assert!(
            a.blocks.iter().any(|b| matches!(b, Block::Agent { text, .. } if text == "orfano")),
            "dropped a child's output"
        );
    }

    #[test]
    fn the_prompt_shows_a_placeholder_when_empty_and_the_text_otherwise() {
        let mut a = app();
        assert!(screen(&mut a, 50, 8).join("\n").contains("ask anything"));
        a.ed.set("scrivo");
        let s = screen(&mut a, 50, 8).join("\n");
        assert!(s.contains("❯ scrivo"));
        assert!(!s.contains("ask anything"));
    }

    #[test]
    fn the_command_menu_appears_over_the_prompt() {
        let mut a = app();
        a.ed.set("/th");
        let s = screen(&mut a, 60, 12).join("\n");
        assert!(s.contains("/theme"), "menu not drawn:\n{s}");
    }

    #[test]
    fn the_file_menu_appears_over_the_prompt() {
        let mut a = app();
        with_files(&mut a);
        a.ed.set("leggi @src/");
        let s = screen(&mut a, 60, 12).join("\n");
        assert!(s.contains("@src/main.rs"), "file menu not drawn:\n{s}");
        assert!(s.contains("@src/app/mod.rs"), "file menu not drawn:\n{s}");
    }

    #[test]
    fn a_long_path_is_clipped_to_the_window_rather_than_wrapped() {
        let mut a = app();
        a.files.set(vec![format!("{}/deep/nested/file.rs", "src/very/long".repeat(4))]);
        a.ed.set("@file");
        let rows = screen(&mut a, 40, 12);
        assert!(rows.iter().all(|r| r.chars().count() <= 40));
        assert!(rows.iter().any(|r| r.contains("…") && r.contains("file.rs")), "{rows:#?}");
    }

    #[test]
    fn a_tiny_terminal_still_renders_without_panicking() {
        // QA will try this. Every pane must survive being squeezed.
        let mut a = app();
        a.blocks.push(Block::Assistant("parola ".repeat(50)));
        a.touch();
        for (w, h) in [(20u16, 4u16), (10, 3), (80, 2), (5, 10)] {
            let _ = screen(&mut a, w, h);
        }
    }

    #[test]
    fn long_output_pins_to_the_newest_line() {
        let mut a = app();
        a.blocks.clear();
        for i in 0..60 {
            a.blocks.push(Block::Notice { text: format!("riga {i}"), level: Level::Info });
        }
        a.touch();
        a.follow = true;
        let s = screen(&mut a, 40, 12);
        assert!(s.join("\n").contains("riga 59"), "tail not visible:\n{}", s.join("\n"));
    }

    #[test]
    fn switching_theme_changes_the_rendered_colours() {
        let _env = isolated("colours");
        let mut a = app();
        a.blocks.clear();
        a.blocks.push(Block::User("x".into()));
        // Start from a known theme so the assertion can't be defeated by whatever
        // the previous run happened to save.
        a.set_theme("tokyonight");

        // Tall enough that nothing scrolls, so (0,0) is reliably the ❯ marker
        // rather than whatever a scrolled viewport happens to land on.
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 16)).unwrap();
        render(&mut term, &mut a).unwrap();
        let before = term.backend().buffer()[(0, 0)].style().fg;
        assert_ne!(before, Some(ratatui::style::Color::Reset), "expected the ❯ marker");

        a.set_theme("gruvbox-dark");
        render(&mut term, &mut a).unwrap();
        let after = term.backend().buffer()[(0, 0)].style().fg;
        assert_ne!(before, after, "theme switch did not repaint the transcript");
    }

    #[test]
    fn the_theme_choice_is_persisted_and_read_back() {
        let _env = isolated("persist");
        let mut a = app();
        a.set_theme("nord");
        // A fresh app in the same LELE_HOME must come up on the saved theme.
        let b = App::new(std::sync::Arc::new(crate::backend::HttpBackend(Client::new(1))), false, health_fixture());
        assert_eq!(b.theme.name, "nord");
    }

    #[test]
    fn a_theme_file_in_lele_home_is_picked_up() {
        let _env = isolated("userthemes");
        let dir = theme::themes_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mio.toml"), "accent = \"#ff0000\"\n").unwrap();

        let mut a = app();
        a.set_theme("mio");
        assert_eq!(a.theme.name, "mio");
        assert_eq!(a.theme.accent, ratatui::style::Color::Rgb(0xff, 0, 0));
        // and an unspecified role still inherits the default
        assert_eq!(a.theme.fg, Theme::tokyonight().fg);
    }

    #[test]
    fn a_broken_theme_file_is_reported_not_swallowed() {
        let _env = isolated("badtheme");
        let dir = theme::themes_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("rotto.toml"), "accent = \"banana\"\n").unwrap();

        let a = app();
        let warned = a.blocks.iter().any(|b| {
            matches!(b, Block::Notice { text, level: Level::Warn } if text.contains("rotto"))
        });
        assert!(warned, "a bad theme must surface, got {:?}", a.blocks);
    }

    #[test]
    fn menu_opens_only_while_typing_a_slash_command() {
        let mut a = app();
        assert!(!a.menu_open());
        a.ed.set("/th");
        assert!(a.menu_open());
        a.ed.set("ciao");
        assert!(!a.menu_open());
    }
}
