//! The steering queue: messages typed into a turn that is already running.
//!
//! They are already echoed in the transcript, so this strip is not about *what*
//! was said — it is about the wait. A steer sits in the server's queue until the
//! loop reaches its next model call, which can be a whole tool call away, and
//! without a visible marker that gap reads as the message having been dropped.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Theme;

/// Rows the strip may take. Past this the rest are summarised — a queue this deep
/// is unusual, and the prompt matters more than listing all of it.
pub const MAX_ROWS: u16 = 3;

/// Rows needed for `n` pending messages.
pub fn height(n: usize) -> u16 {
    (n as u16).min(MAX_ROWS)
}

pub fn draw(f: &mut Frame, area: Rect, queued: &[String], theme: &Theme) {
    if queued.is_empty() {
        return;
    }
    let rows = area.height as usize;
    // The last row becomes a summary when there is more than it can show, so only
    // `rows - 1` messages are listed in that case.
    let (shown, extra) = if queued.len() > rows { (rows - 1, queued.len() - (rows - 1)) } else { (queued.len(), 0) };

    let mark = Style::default().fg(theme.accent).add_modifier(Modifier::BOLD);
    let body = Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC);
    let width = area.width as usize;

    let mut lines: Vec<Line> = queued[..shown]
        .iter()
        .map(|text| {
            Line::from(vec![
                Span::styled("⤷ ", mark),
                Span::styled(one_line(text, width.saturating_sub(2)), body),
            ])
        })
        .collect();
    if extra > 0 {
        lines.push(Line::from(Span::styled(format!("⤷ +{extra} in queue"), body)));
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// A queued message on one row: newlines collapsed, then cut to fit.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max || max == 0 {
        return flat;
    }
    let keep = max.saturating_sub(1);
    format!("{}…", flat.chars().take(keep).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(queued: &[String], w: u16, h: u16) -> Vec<String> {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| draw(f, f.area(), queued, &Theme::tokyonight())).unwrap();
        term.backend()
            .buffer()
            .content()
            .chunks(w as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>().trim_end().to_string())
            .collect()
    }

    #[test]
    fn nothing_queued_takes_no_rows() {
        assert_eq!(height(0), 0);
    }

    #[test]
    fn each_message_gets_a_row_up_to_the_cap() {
        assert_eq!(height(1), 1);
        assert_eq!(height(3), 3);
        assert_eq!(height(9), MAX_ROWS);
    }

    #[test]
    fn a_queued_message_is_shown_with_its_marker() {
        let rows = render(&["aspetta, usa il file vecchio".to_string()], 40, 1);
        assert_eq!(rows[0], "⤷ aspetta, usa il file vecchio");
    }

    #[test]
    fn a_multi_line_message_is_flattened_onto_one_row() {
        let rows = render(&["prima\nseconda".to_string()], 40, 1);
        assert_eq!(rows[0], "⤷ prima seconda");
    }

    #[test]
    fn a_long_message_is_cut_to_the_width() {
        let rows = render(&["x".repeat(100)], 20, 1);
        assert_eq!(rows[0].chars().count(), 20);
        assert!(rows[0].ends_with('…'));
    }

    #[test]
    fn a_deep_queue_is_summarised_rather_than_truncated_silently() {
        let msgs: Vec<String> = (1..=5).map(|i| format!("msg{i}")).collect();
        let rows = render(&msgs, 30, MAX_ROWS);
        assert_eq!(rows[0], "⤷ msg1");
        assert_eq!(rows[1], "⤷ msg2");
        assert_eq!(rows[2], "⤷ +3 in queue", "the rest must be accounted for");
    }
}
