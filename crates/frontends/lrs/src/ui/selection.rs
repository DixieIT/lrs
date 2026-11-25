//! Drag-to-select over the transcript.
//!
//! Coordinates here are *content* coordinates — a line index into the wrap cache
//! and a character column — not screen ones. Scrolling while a selection is up
//! then moves the highlight with the text instead of leaving it stuck to the
//! terminal rows it was drawn on.
//!
//! The selection is linear, like a terminal's: from the first point to the last,
//! taking whole lines in between. Both ends are inclusive, so dragging across a
//! word takes the character under the cursor too; a click that never moves selects
//! nothing at all.

use ratatui::text::Line;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    pub fn new(at: (usize, usize)) -> Self {
        Selection { anchor: at, head: at }
    }

    pub fn extend(&mut self, to: (usize, usize)) {
        self.head = to;
    }

    /// A click with no drag. Copying on one would replace the clipboard with a
    /// single character every time the user tapped the transcript.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Start and end, in reading order — a drag upwards is just as valid.
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    pub fn contains(&self, line: usize, col: usize) -> bool {
        if self.is_empty() {
            return false;
        }
        let (s, e) = self.ordered();
        (line, col) >= s && (line, col) <= e
    }

    /// The selected text. Lines are joined with `\n` and trailing blanks on each
    /// row are dropped — the padding to the right of a short line is not content,
    /// and pasting it back would carry a tail of spaces.
    pub fn text(&self, cache: &[Line<'static>]) -> String {
        if self.is_empty() {
            return String::new();
        }
        let (s, e) = self.ordered();
        let mut rows = Vec::new();
        for li in s.0..=e.0.min(cache.len().saturating_sub(1)) {
            let Some(line) = cache.get(li) else { break };
            let chars: Vec<char> = line_text(line).chars().collect();
            let from = if li == s.0 { s.1 } else { 0 };
            let to = if li == e.0 { (e.1 + 1).min(chars.len()) } else { chars.len() };
            let slice: String = if from >= to {
                String::new()
            } else {
                chars[from..to].iter().collect()
            };
            rows.push(slice.trim_end().to_string());
        }
        rows.join("\n")
    }
}

/// A rendered line's plain text, spans concatenated.
pub fn line_text(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    fn cache(rows: &[&str]) -> Vec<Line<'static>> {
        rows.iter()
            .map(|r| Line::from(Span::raw(r.to_string())))
            .collect()
    }

    #[test]
    fn a_click_that_never_moves_selects_nothing() {
        let s = Selection::new((1, 3));
        assert!(s.is_empty());
        assert_eq!(s.text(&cache(&["uno", "due"])), "");
        assert!(!s.contains(1, 3), "a bare click must not highlight a cell");
    }

    #[test]
    fn dragging_across_a_word_includes_the_character_under_the_cursor() {
        let mut s = Selection::new((0, 0));
        s.extend((0, 2));
        assert_eq!(s.text(&cache(&["abcdef"])), "abc");
    }

    #[test]
    fn a_selection_spanning_lines_takes_the_middle_ones_whole() {
        let mut s = Selection::new((0, 4));
        s.extend((2, 1));
        assert_eq!(s.text(&cache(&["prima riga", "seconda", "terza"])), "a riga\nseconda\nte");
    }

    #[test]
    fn dragging_upwards_selects_the_same_text() {
        let rows = cache(&["prima", "seconda"]);
        let mut down = Selection::new((0, 2));
        down.extend((1, 3));
        let mut up = Selection::new((1, 3));
        up.extend((0, 2));
        assert_eq!(down.text(&rows), up.text(&rows));
        assert_eq!(up.text(&rows), "ima\nseco");
    }

    #[test]
    fn trailing_padding_is_not_part_of_the_copy() {
        // Dragging past the end of a short line must not add spaces to the paste.
        let mut s = Selection::new((0, 0));
        s.extend((1, 40));
        assert_eq!(s.text(&cache(&["corta", "anche"])), "corta\nanche");
    }

    #[test]
    fn a_selection_past_the_end_of_the_cache_does_not_panic() {
        // The transcript can shrink while a selection is up (a /clear mid-drag).
        let mut s = Selection::new((0, 0));
        s.extend((99, 99));
        assert_eq!(s.text(&cache(&["sola"])), "sola");
    }

    #[test]
    fn highlighting_covers_exactly_the_selected_cells() {
        let mut s = Selection::new((0, 2));
        s.extend((1, 1));
        assert!(!s.contains(0, 1));
        assert!(s.contains(0, 2) && s.contains(0, 50), "to the end of the first row");
        assert!(s.contains(1, 0) && s.contains(1, 1));
        assert!(!s.contains(1, 2), "past the end point");
        assert!(!s.contains(2, 0));
    }

    #[test]
    fn styled_spans_contribute_their_text_and_nothing_else() {
        let line = Line::from(vec![
            Span::styled("│ ", ratatui::style::Style::default()),
            Span::raw("corpo"),
        ]);
        assert_eq!(line_text(&line), "│ corpo");
    }
}
