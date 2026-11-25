//! Slash commands.
//!
//! Parsing is separated from execution so the whole table is testable without a
//! terminal or a server. Anything that isn't a known command is sent to the agent
//! verbatim — a typo'd slash should reach the model, not be swallowed with an error.
//!
//! The table is not fixed: plugins register commands on the server, and lrs
//! merges them in so `/goal` completes and runs here exactly as it does in the
//! Textual TUI, without this crate knowing a thing about goal.

use std::collections::BTreeMap;

/// The registry, also used to build the completion menu.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show this list"),
    ("/theme", "switch theme (picker, or /theme <name>)"),
    ("/model", "switch model (picker, or /model <ref>)"),
    ("/connect", "set a provider's API key (picker, or /connect <name>)"),
    ("/effort", "reasoning effort: low|medium|high|max|off"),
    ("/image", "attach the clipboard image (or ^p)"),
    ("/plugin", "enable or disable a plugin (picker)"),
    ("/extension", "enable or disable an extension (picker)"),
    ("/reload", "re-register plugins, extensions and skills"),
    ("/resume", "reopen this project's last session, or /resume <id>"),
    ("/clear", "start a fresh session"),
    ("/expand", "toggle full tool output"),
    ("/context", "show what the model receives, in a popup"),
    ("/quit", "exit"),
];

/// `None` on the picker-backed commands means "no argument given" — open the
/// picker rather than erroring, which is what makes `/model` on its own useful.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Theme(Option<String>),
    Model(Option<String>),
    Connect(Option<String>),
    Effort(Option<String>),
    /// Attach the clipboard image. Exists as a command because most terminals eat
    /// ^v before lrs can see it.
    Image,
    /// A command a plugin registered; the server runs it.
    Plugin { name: String, arg: String },
    /// Toggle a plugin or extension. `true` = plugins.
    Registry { plugins: bool },
    Reload,
    /// The last session of this project, or the one named.
    Resume(Option<String>),
    Clear,
    Expand,
    /// Show the model-facing context of the next turn in a popup.
    Context,
    Quit,
    /// Not a command — send it to the agent as a turn.
    Send(String),
}

/// One entry of the live command table: a built-in, or a command a plugin
/// registered on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub desc: String,
    /// argument → description, as the plugin declared it. A `<placeholder>` key
    /// completes with the placeholder in place, ready to type over.
    pub sub: BTreeMap<String, String>,
    /// Plugin commands are executed by the server; built-ins never leave here.
    pub plugin: bool,
}

/// Built-ins plus whatever the server's plugins registered.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub entries: Vec<Entry>,
}

impl Table {
    /// Built-ins only. What lrs has before it has talked to the server.
    pub fn builtin() -> Self {
        Table {
            entries: COMMANDS
                .iter()
                .map(|(name, desc)| Entry {
                    name: (*name).to_string(),
                    desc: (*desc).to_string(),
                    sub: BTreeMap::new(),
                    plugin: false,
                })
                .collect(),
        }
    }

    /// Merge in the server's plugin commands. Built-ins win on a name clash, the
    /// same rule `merge_plugin_commands` applies on the Python side — otherwise a
    /// plugin could shadow `/quit`.
    pub fn with_plugins(mut self, specs: &[lrs::protocol::CommandSpec]) -> Self {
        self.entries.retain(|e| !e.plugin);
        for spec in specs {
            if self.entries.iter().any(|e| e.name == spec.name) {
                continue;
            }
            self.entries.push(Entry {
                name: spec.name.clone(),
                desc: spec.desc.clone(),
                sub: spec.sub.clone(),
                plugin: true,
            });
        }
        self.entries.sort_by(|a, b| a.name.cmp(&b.name));
        self
    }

    pub fn get(&self, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.name == name)
    }
}

pub fn parse(raw: &str, table: &Table) -> Command {
    let input = raw.trim();
    if !input.starts_with('/') {
        return Command::Send(input.to_string());
    }
    let (head, rest) = match input.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (input, ""),
    };
    let arg = (!rest.is_empty()).then(|| rest.to_string());

    // A plugin command is dispatched by name to the server. Checked before the
    // built-in table only for names the built-ins don't claim, since `with_plugins`
    // already dropped any that collide.
    if let Some(e) = table.get(head) {
        if e.plugin {
            return Command::Plugin { name: head.to_string(), arg: rest.to_string() };
        }
    }

    match head {
        "/help" | "/h" | "/?" => Command::Help,
        "/theme" => Command::Theme(arg),
        "/model" => Command::Model(arg),
        "/connect" | "/login" => Command::Connect(arg),
        "/effort" => Command::Effort(arg),
        "/image" | "/img" | "/paste" => Command::Image,
        // Both spellings: the Textual TUI calls them /plugins and /extensions.
        "/plugin" | "/plugins" => Command::Registry { plugins: true },
        "/extension" | "/extensions" | "/ext" => Command::Registry { plugins: false },
        "/reload" => Command::Reload,
        "/resume" | "/r" => Command::Resume(arg),
        "/clear" | "/new" => Command::Clear,
        "/expand" => Command::Expand,
        "/context" | "/ctx" => Command::Context,
        "/quit" | "/exit" | "/q" => Command::Quit,
        // Unknown slash: the agent sees it. Better than eating the input.
        _ => Command::Send(input.to_string()),
    }
}

/// Normalise an effort argument. Returns `Some("")` for "off" — the wire format
/// distinguishes "explicitly off" from "unset", so the empty string is meaningful.
pub fn normalise_effort(arg: &str) -> Option<String> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "no" | "0" => Some(String::new()),
        v @ ("low" | "medium" | "high" | "max") => Some(v.to_string()),
        _ => None,
    }
}

/// The other half of `/help`. Not commands, so they are shown but never picked.
pub const KEYS: &[(&str, &str)] = &[
    ("@", "complete a project file into the prompt"),
    ("enter", "send, or take the highlighted completion"),
    ("tab", "take the highlighted completion"),
    ("shift+enter · ^j", "newline"),
    ("↑ / ↓", "prompt history, or move in the completion menu"),
    ("^↑ / ^↓", "scroll one line"),
    ("pgup / pgdn", "scroll a screenful"),
    ("^l", "jump to the newest line"),
    ("^o", "expand or collapse all tool output"),
    ("^p · ^v", "attach the clipboard image (^v if your terminal passes it on)"),
    ("^w / ^u", "delete the word before the caret / clear the prompt"),
    ("^c", "cancel the running turn, then quit"),
    ("esc", "cancel the turn, or clear the prompt"),
    ("drag", "select and copy · shift+drag for the terminal's own selection"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use lrs::protocol::CommandSpec;

    /// Built-ins only — what most of these tests care about.
    fn t() -> Table {
        Table::builtin()
    }

    /// A table with the goal plugin merged in, exactly as the server reports it.
    fn with_goal() -> Table {
        Table::builtin().with_plugins(&[CommandSpec {
            name: "/goal".into(),
            desc: "Set, pause, resume, or clear a session goal".into(),
            sub: BTreeMap::from([
                ("<objective>".to_string(), "set a new goal and start pursuing it".to_string()),
                ("pause".to_string(), "pause the goal".to_string()),
                ("resume".to_string(), "resume a paused goal".to_string()),
                ("clear".to_string(), "clear the goal and hide the widget".to_string()),
            ]),
        }])
    }

    #[test]
    fn a_plugin_command_is_dispatched_to_the_server() {
        assert_eq!(
            parse("/goal spedire la release", &with_goal()),
            Command::Plugin { name: "/goal".into(), arg: "spedire la release".into() }
        );
        // and with no argument it still goes there — the plugin decides what that means
        assert_eq!(
            parse("/goal", &with_goal()),
            Command::Plugin { name: "/goal".into(), arg: String::new() }
        );
    }

    #[test]
    fn an_unknown_plugin_command_still_reaches_the_agent() {
        // Without the plugin loaded, /goal is just text — not a swallowed error.
        assert_eq!(parse("/goal x", &t()), Command::Send("/goal x".into()));
    }

    #[test]
    fn a_plugin_cannot_shadow_a_builtin() {
        let table = Table::builtin().with_plugins(&[CommandSpec {
            name: "/quit".into(),
            desc: "hijacked".into(),
            sub: BTreeMap::new(),
        }]);
        assert_eq!(parse("/quit", &table), Command::Quit);
        assert!(!table.get("/quit").unwrap().plugin);
    }

    #[test]
    fn refreshing_the_table_does_not_stack_duplicates() {
        // fetch_plugins can run more than once (after a /connect, say).
        let specs = [CommandSpec { name: "/goal".into(), ..Default::default() }];
        let table = Table::builtin().with_plugins(&specs).with_plugins(&specs);
        assert_eq!(table.entries.iter().filter(|e| e.name == "/goal").count(), 1);
    }

    #[test]
    fn a_plugin_command_that_vanishes_is_dropped() {
        let table = with_goal().with_plugins(&[]);
        assert!(table.get("/goal").is_none());
        assert!(table.get("/quit").is_some(), "built-ins must survive");
    }

    #[test]
    fn plain_text_is_a_turn() {
        assert_eq!(parse("fix the bug", &t()), Command::Send("fix the bug".into()));
    }

    #[test]
    fn commands_parse_with_and_without_arguments() {
        assert_eq!(parse("/theme", &t()), Command::Theme(None));
        assert_eq!(parse("/theme nord", &t()), Command::Theme(Some("nord".into())));
        assert_eq!(parse("  /theme   nord  ", &t()), Command::Theme(Some("nord".into())));
    }

    #[test]
    fn an_unknown_slash_reaches_the_agent() {
        // Swallowing this would lose the user's input for a typo.
        assert_eq!(parse("/nope x", &t()), Command::Send("/nope x".into()));
    }

    #[test]
    fn aliases_work() {
        assert_eq!(parse("/q", &t()), Command::Quit);
        assert_eq!(parse("/new", &t()), Command::Clear);
        assert_eq!(parse("/?", &t()), Command::Help);
    }

    #[test]
    fn effort_off_is_distinct_from_unset() {
        assert_eq!(normalise_effort("off"), Some(String::new()));
        assert_eq!(normalise_effort("HIGH"), Some("high".into()));
        assert_eq!(normalise_effort("banana"), None, "a bad level must be rejected, not sent");
    }

    #[test]
    fn a_model_ref_with_a_slash_survives_parsing() {
        // Provider refs contain '/', which must not confuse the splitter.
        assert_eq!(
            parse("/model openrouter:anthropic/claude", &t()),
            Command::Model(Some("openrouter:anthropic/claude".into()))
        );
    }

    #[test]
    fn multiword_arguments_are_kept_whole() {
        assert_eq!(parse("/model a b", &t()), Command::Model(Some("a b".into())));
    }

    #[test]
    fn the_picker_commands_take_an_optional_argument() {
        assert_eq!(parse("/model", &t()), Command::Model(None));
        assert_eq!(parse("/connect", &t()), Command::Connect(None));
        assert_eq!(parse("/connect anthropic", &t()), Command::Connect(Some("anthropic".into())));
        assert_eq!(parse("/effort", &t()), Command::Effort(None));
        assert_eq!(parse("/resume", &t()), Command::Resume(None));
        assert_eq!(parse("/resume 20261122-1430-abc", &t()), Command::Resume(Some("20261122-1430-abc".into())));
        // `/r` mirrors the `-r` flag rather than being a second thing to remember
        assert_eq!(parse("/r", &t()), Command::Resume(None));
    }

    #[test]
    fn every_registered_command_actually_parses() {
        for (name, _) in COMMANDS {
            assert!(
                !matches!(parse(name, &t()), Command::Send(_)),
                "{name} is listed but falls through to the agent"
            );
        }
    }
}
