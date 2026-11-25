//! The compiled-in theme set.
//!
//! Built-ins ship as TOML and go through exactly the same parser a user theme
//! does, so a broken built-in fails in our tests rather than on someone's machine.
//! `base()` is the one palette defined in Rust: it is the fallback every partial
//! theme inherits from, so it can never itself depend on parsing succeeding.

use ratatui::style::Color;

use super::Theme;

pub const BUILTINS: &[(&str, &str)] = &[
    ("tokyonight", include_str!("../../themes/tokyonight.toml")),
    ("tokyonight-day", include_str!("../../themes/tokyonight-day.toml")),
    ("catppuccin-mocha", include_str!("../../themes/catppuccin-mocha.toml")),
    ("gruvbox-dark", include_str!("../../themes/gruvbox-dark.toml")),
    ("nord", include_str!("../../themes/nord.toml")),
    ("ansi", include_str!("../../themes/ansi.toml")),
];

/// lrs's default palette, and the inheritance root for every other theme.
pub fn base() -> Theme {
    Theme {
        name: "tokyonight".into(),
        bg: None,
        fg: Color::Rgb(0xc0, 0xca, 0xf5),
        muted: Color::Rgb(0x56, 0x5f, 0x89),
        accent: Color::Rgb(0x7a, 0xa2, 0xf7),
        border: Color::Rgb(0x29, 0x2e, 0x42),
        user: Color::Rgb(0x7d, 0xcf, 0xff),
        tool: Color::Rgb(0xbb, 0x9a, 0xf7),
        thinking: Color::Rgb(0x9e, 0xce, 0x6a),
        ok: Color::Rgb(0x9e, 0xce, 0x6a),
        warn: Color::Rgb(0xe0, 0xaf, 0x68),
        err: Color::Rgb(0xf7, 0x76, 0x8e),
        add: Color::Rgb(0x9e, 0xce, 0x6a),
        del: Color::Rgb(0xf7, 0x76, 0x8e),
    }
}
