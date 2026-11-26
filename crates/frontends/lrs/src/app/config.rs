//! `~/.lele/lrs.toml` — the handful of choices that should survive a restart.
//!
//! Separate from lele's own `config.yaml` on purpose: this is frontend state, and
//! writing into the shared config from a second frontend would be a good way to
//! corrupt the first one's settings.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Theme name; absent means the built-in default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Status::is_default")]
    pub status: Status,
}

/// What the strip under the prompt shows. Everything is on until turned off:
/// `[status]` then `context = false` hides the context window gauge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    #[serde(default = "on")]
    pub context: bool,
}

impl Default for Status {
    fn default() -> Self {
        Self { context: true }
    }
}

impl Status {
    fn is_default(&self) -> bool {
        self.context
    }
}

fn on() -> bool {
    true
}

/// Where settings live. `LELE_HOME` overrides the default `~/.lele`, which lets
/// the tests point at a temp directory instead of writing into the real config —
/// and lets anyone relocate their lele directory.
pub fn dir() -> Option<PathBuf> {
    match std::env::var_os("LELE_HOME") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => dirs::home_dir().map(|h| h.join(".lele")),
    }
}

pub fn path() -> Option<PathBuf> {
    dir().map(|d| d.join("lrs.toml"))
}

/// Never fails: a missing or malformed config falls back to defaults. A frontend
/// that refuses to start because of a stray character in a settings file is worse
/// than one that ignores it.
pub fn load() -> Config {
    let Some(p) = path() else { return Config::default() };
    let Ok(text) = std::fs::read_to_string(p) else { return Config::default() };
    toml::from_str(&text).unwrap_or_default()
}

pub fn save(cfg: &Config) -> anyhow::Result<()> {
    let Some(p) = path() else {
        anyhow::bail!("no home directory to save settings in")
    };
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(p, toml::to_string_pretty(cfg)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_config_is_valid() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.theme.is_none());
    }

    #[test]
    fn the_context_gauge_is_on_unless_the_config_turns_it_off() {
        let on: Config = toml::from_str("").unwrap();
        assert!(on.status.context);
        let also_on: Config = toml::from_str("[status]").unwrap();
        assert!(also_on.status.context, "a table without the key keeps the default");
        let off: Config = toml::from_str("[status]\ncontext = false").unwrap();
        assert!(!off.status.context);
    }

    #[test]
    fn the_default_status_is_not_written_back() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!text.contains("status"), "{text}");
        let off = Config { status: Status { context: false }, ..Default::default() };
        let back: Config = toml::from_str(&toml::to_string_pretty(&off).unwrap()).unwrap();
        assert!(!back.status.context, "a choice to hide it survives a save");
    }

    #[test]
    fn unknown_keys_do_not_break_loading() {
        // A newer lrs's key must not stop an older one from starting.
        let c: Config = toml::from_str("theme = \"nord\"\nfuture_key = 3").unwrap();
        assert_eq!(c.theme.as_deref(), Some("nord"));
    }

    #[test]
    fn round_trips() {
        let c = Config { theme: Some("gruvbox-dark".into()), port: Some(9000), ..Default::default() };
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.theme, c.theme);
        assert_eq!(back.port, c.port);
    }

    #[test]
    fn absent_fields_are_omitted_not_written_as_null() {
        let s = toml::to_string_pretty(&Config::default()).unwrap();
        assert!(!s.contains("theme"), "got: {s:?}");
    }
}
