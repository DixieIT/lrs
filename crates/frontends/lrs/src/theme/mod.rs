//! Colour themes.
//!
//! A theme is a flat set of named roles, never raw colours at the call site — that is
//! what lets a plugin's output re-colour with everything else instead of hardcoding
//! its own palette. Built-ins are compiled in so lrs always starts with a
//! working theme; anything in `~/.lele/themes/*.toml` is loaded on top and can
//! override a built-in of the same name.

mod builtin;

use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;

use builtin::BUILTINS;

/// Every colour the UI can ask for. Adding a role here means adding it to
/// `DEFAULT_TOML` too, otherwise a user theme that omits it falls back to
/// tokyonight's value rather than failing to load.
#[derive(Debug, Clone)]
pub struct Theme {
    pub name: String,
    /// Terminal background. `None` keeps the terminal's own, which is what most
    /// people actually want — a TUI that repaints the background fights the
    /// user's transparency and colour scheme.
    pub bg: Option<Color>,
    pub fg: Color,
    /// De-emphasised text: timestamps, hints, the statusline's labels.
    pub muted: Color,
    pub accent: Color,
    pub border: Color,
    /// The user's own messages.
    pub user: Color,
    /// Tool call titles.
    pub tool: Color,
    /// Reasoning/thinking text.
    pub thinking: Color,
    pub ok: Color,
    pub warn: Color,
    pub err: Color,
    /// Diff bodies.
    pub add: Color,
    pub del: Color,
}

/// Wire format for a theme file. Every field is optional so a user theme can
/// tweak two colours without restating the whole palette.
#[derive(Debug, Deserialize)]
struct ThemeFile {
    name: Option<String>,
    /// `bg = "none"` (or omitted) keeps the terminal background.
    bg: Option<String>,
    fg: Option<String>,
    muted: Option<String>,
    accent: Option<String>,
    border: Option<String>,
    user: Option<String>,
    tool: Option<String>,
    thinking: Option<String>,
    ok: Option<String>,
    warn: Option<String>,
    err: Option<String>,
    add: Option<String>,
    del: Option<String>,
}

/// `#rrggbb`, `rrggbb`, an ANSI colour name, or "none". Anything else is an error
/// rather than a silent fallback — a typo'd colour should tell you, not render as
/// black. ANSI names resolve against the terminal's own 16-colour palette, which is
/// how a theme can defer to whatever the user already configured.
fn parse_color(raw: &str) -> Result<Option<Color>> {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("none") || s.is_empty() {
        return Ok(None);
    }
    let named = match s.to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" | "white" => Some(Color::Gray),
        "bright-black" | "dark-gray" | "dark-grey" => Some(Color::DarkGray),
        "bright-red" => Some(Color::LightRed),
        "bright-green" => Some(Color::LightGreen),
        "bright-yellow" => Some(Color::LightYellow),
        "bright-blue" => Some(Color::LightBlue),
        "bright-magenta" => Some(Color::LightMagenta),
        "bright-cyan" => Some(Color::LightCyan),
        "bright-white" => Some(Color::White),
        "default" => Some(Color::Reset),
        _ => None,
    };
    if let Some(c) = named {
        return Ok(Some(c));
    }
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("bad colour {raw:?} — expected #rrggbb, an ANSI colour name, or \"none\"");
    }
    let n = u32::from_str_radix(hex, 16).context("hex")?;
    Ok(Some(Color::Rgb(
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    )))
}

impl Theme {
    /// Apply a parsed file over `self`, so a partial theme inherits the rest.
    fn overlay(mut self, f: ThemeFile, fallback_name: &str) -> Result<Self> {
        self.name = f.name.unwrap_or_else(|| fallback_name.to_string());
        // bg is the one role where "absent" and "none" mean the same thing
        // (keep the terminal's), so it never inherits from the base theme.
        if let Some(v) = f.bg.as_deref() {
            self.bg = parse_color(v)?;
        }
        macro_rules! set {
            ($($field:ident),*) => {$(
                if let Some(v) = f.$field.as_deref() {
                    if let Some(c) = parse_color(v)? { self.$field = c; }
                }
            )*};
        }
        set!(fg, muted, accent, border, user, tool, thinking, ok, warn, err, add, del);
        Ok(self)
    }

    pub fn from_toml(text: &str, fallback_name: &str) -> Result<Self> {
        let parsed: ThemeFile = toml::from_str(text)?;
        Self::tokyonight().overlay(parsed, fallback_name)
    }

    /// The default, and the inheritance root for every other theme.
    pub fn tokyonight() -> Self {
        builtin::base()
    }
}

/// The name lrs falls back to when no theme is configured.
pub const DEFAULT_THEME: &str = "tokyonight";

pub fn themes_dir() -> Option<PathBuf> {
    crate::app::config::dir().map(|d| d.join("themes"))
}

/// Every available theme by name: built-ins first, then `~/.lele/themes/*.toml`
/// overriding same-named ones. A malformed user theme is skipped with its error
/// collected — one bad file must not take the whole picker down.
pub fn load_all() -> (BTreeMap<String, Theme>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut errors = Vec::new();
    for (name, body) in BUILTINS {
        match Theme::from_toml(body, name) {
            Ok(t) => {
                out.insert((*name).to_string(), t);
            }
            Err(e) => errors.push(format!("built-in theme {name}: {e}")),
        }
    }
    if let Some(dir) = themes_dir() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                match std::fs::read_to_string(&path).map_err(anyhow::Error::from)
                    .and_then(|txt| Theme::from_toml(&txt, &stem))
                {
                    Ok(t) => {
                        out.insert(stem, t);
                    }
                    Err(e) => errors.push(format!("{}: {e}", path.display())),
                }
            }
        }
    }
    (out, errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_parses() {
        let (themes, errors) = load_all();
        assert!(errors.is_empty(), "built-ins must parse cleanly: {errors:?}");
        for (name, _) in BUILTINS {
            assert!(themes.contains_key(*name), "missing built-in {name}");
        }
    }

    #[test]
    fn partial_theme_inherits_the_rest() {
        let t = Theme::from_toml("accent = \"#ff0000\"", "custom").unwrap();
        assert_eq!(t.accent, Color::Rgb(0xff, 0, 0));
        // untouched roles keep tokyonight's values
        assert_eq!(t.fg, Theme::tokyonight().fg);
        assert_eq!(t.name, "custom");
    }

    #[test]
    fn bad_colour_is_an_error_not_a_default() {
        assert!(Theme::from_toml("accent = \"nope\"", "x").is_err());
        assert!(Theme::from_toml("accent = \"#12345\"", "x").is_err());
    }

    #[test]
    fn bg_none_keeps_the_terminal_background() {
        assert!(Theme::from_toml("bg = \"none\"", "x").unwrap().bg.is_none());
        // and an explicit bg is honoured
        assert_eq!(
            Theme::from_toml("bg = \"#101010\"", "x").unwrap().bg,
            Some(Color::Rgb(0x10, 0x10, 0x10))
        );
    }
}
