//! The "working" line: a spinner, a phrase, and pulsing dots.
//!
//! Purely cosmetic, but it is the only thing on screen between sending a prompt
//! and the first token, which on a reasoning model can be many seconds. Without
//! it the terminal looks hung.
//!
//! The phrase used to type itself in a character at a time. It doesn't any more:
//! next to a braille frame — half of which are a vertical bar of dots — growing
//! text reads as a caret typing, and a second cursor on screen is unsettling when
//! the real one is in the prompt below. The spinner and the dots are motion
//! enough.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Frames a phrase stays up before the next one takes over. At the loop's tick
/// rate that is roughly three seconds.
const HOLD: usize = 90;
/// Frames per step of the trailing dots.
const DOT_PULSE: usize = 6;

/// The bundled list is the Python package's own, compiled in rather than copied:
/// one file, no drift between the two frontends.
const BUNDLED: &str = include_str!("../../assets/phrases.txt");

/// Used only if both the user's file and the bundled one somehow yield nothing.
const FALLBACK: &[&str] = &["Thinking…", "Cooking…", "Crunching…", "Pondering…"];

pub struct Spinner {
    phrases: Vec<String>,
    phrase: usize,
    frame: usize,
    /// Frames the current phrase has been up, so it can hand over to the next.
    held: usize,
}

/// One per line, blanks and `#` comments ignored.
fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

impl Spinner {
    /// `~/.lele/phrases.txt` wins over the bundled set, so an override written for
    /// the Textual TUI is picked up here too.
    pub fn new() -> Self {
        let user = crate::app::config::dir()
            .map(|d| d.join("phrases.txt"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|t| parse(&t))
            .filter(|v| !v.is_empty());

        let phrases = user.unwrap_or_else(|| {
            let bundled = parse(BUNDLED);
            if bundled.is_empty() {
                FALLBACK.iter().map(|s| s.to_string()).collect()
            } else {
                bundled
            }
        });
        Spinner { phrases, phrase: 0, frame: 0, held: 0 }
    }

    /// Advance one frame, moving on to the next phrase once this one has had
    /// its time.
    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        self.held += 1;
        if self.held >= HOLD {
            // Step rather than sample at random: `rand` would be a dependency, and
            // walking the list is just as varied over a wait of any length.
            self.phrase = (self.phrase + 1) % self.phrases.len().max(1);
            self.held = 0;
        }
    }

    /// Restart on a fresh phrase — called when a turn begins, so two turns in a
    /// row don't open on the same words.
    pub fn restart(&mut self) {
        self.phrase = (self.phrase + 1) % self.phrases.len().max(1);
        self.held = 0;
        self.frame = 0;
    }

    fn current(&self) -> &str {
        self.phrases.get(self.phrase).map(String::as_str).unwrap_or("Thinking…")
    }

    pub fn line(&self, theme: &Theme) -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("{} ", FRAMES[self.frame % FRAMES.len()]),
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.current().to_string(), Style::default().fg(theme.muted)),
            // The only thing on the line that grows, and it grows by one dot.
            Span::styled(
                ".".repeat((self.frame / DOT_PULSE) % 4),
                Style::default().fg(theme.accent),
            ),
        ])
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &Spinner) -> String {
        s.line(&Theme::tokyonight())
            .spans
            .iter()
            .map(|x| x.content.as_ref())
            .collect()
    }

    #[test]
    fn the_bundled_list_is_not_empty() {
        // Bundled copy of the phrases file — if the source list ever moves,
        // this fails at test time rather than showing a bare spinner forever.
        assert!(parse(BUNDLED).len() > 10, "bundled phrases missing");
    }

    #[test]
    fn comments_and_blanks_are_skipped() {
        let v = parse("# nota\n\n  Pondering…  \n\n# altra\nCooking…\n");
        assert_eq!(v, vec!["Pondering…", "Cooking…"]);
    }

    #[test]
    fn the_phrase_is_there_whole_from_the_first_frame() {
        // It used to grow a character at a time, which next to a braille frame
        // read as a caret typing — a second cursor, with the real one below.
        let s = Spinner { phrases: vec!["Coalescing".into()], phrase: 0, frame: 0, held: 0 };
        assert!(text(&s).contains("Coalescing"), "got {:?}", text(&s));
    }

    #[test]
    fn the_phrase_never_changes_width_while_it_is_up() {
        let mut s = Spinner { phrases: vec!["Coalescing".into()], phrase: 0, frame: 0, held: 0 };
        for _ in 0..HOLD - 1 {
            s.tick();
            let t = text(&s);
            let body = t.trim_end_matches('.');
            assert!(body.ends_with("Coalescing"), "the phrase moved: {t:?}");
        }
    }

    #[test]
    fn it_moves_on_to_the_next_phrase_after_the_hold() {
        let mut s = Spinner {
            phrases: vec!["uno".into(), "due".into()],
            phrase: 0,
            frame: 0,
            held: 0,
        };
        for _ in 0..HOLD {
            s.tick();
        }
        assert_eq!(s.phrase, 1, "should have advanced");
    }

    #[test]
    fn a_single_phrase_list_does_not_divide_by_zero() {
        let mut s = Spinner { phrases: vec![], phrase: 0, frame: 0, held: 0 };
        for _ in 0..200 {
            s.tick();
        }
        assert!(!text(&s).is_empty());
    }

    #[test]
    fn no_block_caret_is_ever_drawn() {
        // A second caret on screen is confusing: the real one belongs in the prompt.
        let mut s = Spinner { phrases: vec!["abcdef".into()], phrase: 0, frame: 0, held: 0 };
        for _ in 0..200 {
            s.tick();
            assert!(!text(&s).contains('█'), "block caret leaked: {:?}", text(&s));
        }
    }

    #[test]
    fn the_dots_pulse_so_a_long_wait_still_moves() {
        let mut s = Spinner { phrases: vec!["abcdef".into()], phrase: 0, frame: 0, held: 0 };
        let seen: Vec<usize> =
            (0..24).map(|_| { s.tick(); text(&s).matches('.').count() }).collect();
        assert!(seen.iter().any(|&n| n == 0), "the dots never empty: {seen:?}");
        assert!(seen.iter().any(|&n| n == 3), "the dots never fill: {seen:?}");
    }

    #[test]
    fn the_spinner_character_advances() {
        let mut s = Spinner::new();
        let first = text(&s).chars().next().unwrap();
        s.tick();
        assert_ne!(first, text(&s).chars().next().unwrap());
    }

    #[test]
    fn restart_begins_a_different_phrase_from_the_start() {
        let mut s = Spinner {
            phrases: vec!["uno".into(), "due".into()],
            phrase: 0,
            frame: 9,
            held: 3,
        };
        s.restart();
        assert_eq!(s.held, 0);
        assert_eq!(s.phrase, 1);
    }
}
