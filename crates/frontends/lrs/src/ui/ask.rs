//! A one-line modal prompt, optionally masked. Currently only `/connect` uses it,
//! but it shares [`super::overlay`]'s frame with the picker so the two look like
//! one family rather than two independently invented dialogs.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::overlay;
use crate::theme::Theme;

/// What the answer is for, so the app can act on it without a second lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    ConnectKey { provider: String, env: String },
}

pub struct Ask {
    pub kind: Kind,
    pub title: String,
    pub message: String,
    /// Warning shown above the field — an already-connected provider is about to
    /// have its key replaced.
    pub warn: Option<String>,
    /// Echo dots instead of characters. On for anything that is a secret.
    pub masked: bool,
    value: String,
}

impl Ask {
    pub fn connect(provider: &str, env: &str, connected: bool) -> Self {
        Ask {
            kind: Kind::ConnectKey { provider: provider.into(), env: env.into() },
            title: format!("Connect — {provider}"),
            message: format!("paste the API key ({env}) — validated, then saved to ~/.lele/.env"),
            warn: connected
                .then(|| "already connected — a new key replaces it, esc keeps the current one".into()),
            masked: true,
            value: String::new(),
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn insert(&mut self, c: char) {
        self.value.push(c);
    }

    pub fn insert_str(&mut self, s: &str) {
        // A pasted key arrives with the newline that ended the copy; keeping it
        // would send a key the provider rejects for a reason nobody can see.
        self.value.extend(s.chars().filter(|c| !c.is_control()));
    }

    pub fn backspace(&mut self) {
        self.value.pop();
    }

    pub fn draw(&self, f: &mut Frame, frame: Rect, theme: &Theme) {
        let rows = 3 + u16::from(self.warn.is_some());
        // The message names the env var being asked for; cutting it off would
        // leave the user guessing which key the box wants.
        let want = self
            .message
            .chars()
            .count()
            .max(self.warn.as_ref().map_or(0, |w| w.chars().count()))
            .min(u16::MAX as usize) as u16;
        let area = overlay::area(frame, 56, want, rows);
        let inner = overlay::draw(f, area, &self.title, "enter confirm · esc cancel", theme);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let w = inner.width as usize;
        let mut lines = Vec::new();
        if let Some(warn) = &self.warn {
            lines.push(overlay::row(
                overlay::truncate(warn, w),
                inner.width,
                Style::default().fg(theme.warn),
            ));
        }
        lines.push(overlay::row(
            overlay::truncate(&self.message, w),
            inner.width,
            Style::default().fg(theme.muted),
        ));
        lines.push(Line::from(""));

        // Length leaks nothing useful and confirms the paste landed.
        let shown = if self.masked {
            "•".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        };
        lines.push(Line::from(vec![
            Span::styled("❯ ", Style::default().fg(theme.accent)),
            Span::styled(
                overlay::truncate(&shown, w.saturating_sub(3)),
                Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled("▏", Style::default().fg(theme.accent)),
        ]));
        overlay::body(f, inner, lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_connect_prompt_names_the_env_var_it_wants() {
        let a = Ask::connect("acme", "ACME_API_KEY", false);
        assert!(a.message.contains("ACME_API_KEY"));
        assert!(a.warn.is_none());
        assert!(a.masked, "an API key must never be echoed");
    }

    #[test]
    fn replacing_an_existing_key_warns_first() {
        let a = Ask::connect("acme", "ACME_API_KEY", true);
        assert!(a.warn.is_some(), "silently overwriting a working key is data loss");
    }

    #[test]
    fn a_pasted_key_loses_its_trailing_newline() {
        // Copying from a dashboard usually brings one along, and a key with a \n
        // is rejected by the provider for a reason the user can't see.
        let mut a = Ask::connect("acme", "E", false);
        a.insert_str("sk-abc123\n");
        assert_eq!(a.value(), "sk-abc123");
    }

    #[test]
    fn typing_and_deleting_behave() {
        let mut a = Ask::connect("acme", "E", false);
        for c in "sk-1".chars() {
            a.insert(c);
        }
        a.backspace();
        assert_eq!(a.value(), "sk-");
    }
}
