//! A read-only, scrollable text modal: `/context` and anything else that is
//! too long for the transcript and meant to be read, not picked from.
//!
//! Rows arrive already split into parts (see [`super::context`]); each part
//! takes its colour from the theme. Long lines wrap instead of being cut, so
//! nothing the model receives is hidden. Scrolling counts screen rows; the
//! window size is only known while drawing, so the last layout is kept for
//! the page keys.

use std::cell::Cell;

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::context::{bar_cells, Part, Row};
use super::overlay;
use crate::theme::Theme;

pub struct Viewer {
    title: String,
    rows: Vec<Row>,
    /// First visible screen row.
    top: Cell<usize>,
    /// Screen rows the body showed last time it was drawn.
    page: Cell<usize>,
    /// Screen rows at the last draw, to clamp scrolling.
    total: Cell<usize>,
}

/// One screen row, after wrapping.
enum Screen {
    Text { text: String, part: Part, heading: bool },
    Usage(Vec<(Part, u64)>),
    Spans(Vec<(String, Part)>),
}

impl Viewer {
    pub fn new(title: impl Into<String>, rows: Vec<Row>) -> Self {
        Self { title: title.into(), rows, top: Cell::new(0), page: Cell::new(1), total: Cell::new(usize::MAX) }
    }

    fn scroll(&self, by: isize) {
        let max = self.total.get().saturating_sub(self.page.get());
        let next = self.top.get().saturating_add_signed(by).min(max);
        self.top.set(next);
    }

    pub fn up(&mut self) { self.scroll(-1) }
    pub fn down(&mut self) { self.scroll(1) }
    pub fn page_up(&mut self) { self.scroll(-(self.page.get().max(1) as isize)) }
    pub fn page_down(&mut self) { self.scroll(self.page.get().max(1) as isize) }
    pub fn home(&mut self) { self.top.set(0) }
    pub fn end(&mut self) { self.top.set(self.total.get().saturating_sub(self.page.get())) }

    pub fn draw(&self, f: &mut Frame, frame: Rect, theme: &Theme) {
        let area = overlay::area(frame, 90, 0, frame.height.saturating_sub(4));
        let width = area.width.saturating_sub(2).max(1) as usize;
        let screen = layout(&self.rows, width);
        let page = area.height.saturating_sub(2) as usize;
        self.page.set(page.max(1));
        self.total.set(screen.len());
        let top = self.top.get().min(screen.len().saturating_sub(page));
        self.top.set(top);

        let shown_to = (top + page).min(screen.len());
        let hint = format!("↑↓ pgup pgdn home end · esc close · {}–{} of {}", top + 1, shown_to, screen.len());
        let inner = overlay::draw(f, area, &self.title, &hint, theme);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let base = match theme.bg {
            Some(bg) => Style::default().bg(bg),
            None => Style::default(),
        };
        let lines = screen[top..shown_to].iter().map(|row| match row {
            Screen::Text { text, part, heading } => {
                let mut style = base.fg(part.color(theme));
                if *heading {
                    style = style.add_modifier(Modifier::BOLD);
                }
                overlay::row(text.clone(), inner.width, style)
            }
            Screen::Usage(usage) => {
                let mut spans: Vec<Span<'static>> = bar_cells(usage, width)
                    .into_iter()
                    .map(|(part, cells)| Span::styled("█".repeat(cells), base.fg(part.color(theme))))
                    .collect();
                spans.push(Span::styled(" ".repeat(width.saturating_sub(spans.iter().map(|s| s.content.chars().count()).sum())), base));
                Line::from(spans)
            }
            Screen::Spans(pieces) => {
                let used: usize = pieces.iter().map(|(t, _)| t.chars().count()).sum();
                let mut spans: Vec<Span<'static>> = pieces.iter()
                    .map(|(t, part)| Span::styled(t.clone(), base.fg(part.color(theme))))
                    .collect();
                spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), base));
                Line::from(spans)
            }
        }).collect();
        overlay::body(f, inner, lines);
    }
}

/// Screen rows for `rows` at `width`: text wraps, the bar and legend do not.
fn layout(rows: &[Row], width: usize) -> Vec<Screen> {
    let mut out = Vec::new();
    for row in rows {
        match row {
            Row::Text { text, part, heading } => {
                for piece in wrap(text, width) {
                    out.push(Screen::Text { text: piece, part: *part, heading: *heading });
                }
            }
            Row::Usage(usage) => out.push(Screen::Usage(usage.clone())),
            Row::Spans(pieces) => out.push(Screen::Spans(pieces.clone())),
        }
    }
    out
}

/// Split a line into rows of at most `width` characters, breaking at a space
/// when there is one in the row (a single word longer than the row is cut).
/// A continuation row keeps the line's indentation so wrapped declarations
/// stay aligned.
fn wrap(line: &str, width: usize) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= width {
        return vec![line.to_string()];
    }
    let indent = chars.iter().take_while(|c| **c == ' ').count().min(width / 2);
    let mut rows = Vec::new();
    let mut start = 0;
    let mut first = true;
    while start < chars.len() {
        let room = if first { width } else { width - indent };
        let mut end = (start + room).min(chars.len());
        // Step back to the last space unless the row already ends on a word
        // boundary. Spaces of the indentation are not break points.
        let lo = start.max(indent) + 1;
        if end < chars.len() && chars[end] != ' ' && lo < end {
            if let Some(space) = chars[lo..end].iter().rposition(|c| *c == ' ') {
                end = lo + space;
            }
        }
        let piece: String = chars[start..end].iter().collect();
        rows.push(if first { piece } else { format!("{}{piece}", " ".repeat(indent)) });
        // The space a row broke at is not carried to the next one.
        start = end + usize::from(chars.get(end) == Some(&' '));
        first = false;
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_words_are_cut_and_keep_the_indentation() {
        assert_eq!(wrap("  abcdefgh", 6), ["  abcd", "  efgh"]);
        assert_eq!(wrap("short", 6), ["short"]);
    }

    #[test]
    fn lines_break_between_words_when_they_can() {
        assert_eq!(wrap("  it can be converted to JSON", 14), ["  it can be", "  converted to", "  JSON"]);
    }

    #[test]
    fn a_wrapped_line_keeps_its_part_on_every_row() {
        let rows = [Row::Text { text: "  aaaa bbbb cccc".into(), part: Part::Tools, heading: false }];
        let screen = layout(&rows, 8);
        assert_eq!(screen.len(), 3);
        assert!(screen.iter().all(|s| matches!(s, Screen::Text { part: Part::Tools, .. })));
    }

    #[test]
    fn scrolling_stays_within_the_content() {
        let rows = (0..10).map(|i| Row::Text { text: i.to_string(), part: Part::Meta, heading: false }).collect();
        let mut v = Viewer::new("t", rows);
        v.total.set(10);
        v.page.set(4);
        v.up();
        assert_eq!(v.top.get(), 0, "cannot scroll above the first row");
        v.page_down();
        v.page_down();
        assert_eq!(v.top.get(), 6, "the last page ends on the last row");
        v.home();
        assert_eq!(v.top.get(), 0);
        v.end();
        assert_eq!(v.top.get(), 6);
    }
}
