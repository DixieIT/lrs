//! The searchable list every modal picker is built from.
//!
//! One widget serves `/model`, `/connect`, `/effort` and `/theme`: they differ only
//! in the rows they hand it and in what the app does with the id that comes back.
//! Rows can be grouped — a heading is a row with no id, shown but never selectable —
//! which is what lets the model list stay readable at 300+ entries.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;

use super::overlay;
use crate::theme::Theme;

/// Which picker is open, so the app knows what to do with the chosen id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Model,
    Provider,
    Effort,
    Theme,
    /// Enable/disable a plugin or an extension. `plugins: false` = extensions.
    Registry { plugins: bool },
    /// The command list. Picking one puts it in the prompt.
    Help,
}

#[derive(Debug, Clone, Default)]
pub struct Row {
    /// `None` marks a row that cannot be chosen — a group heading, or with
    /// `info` set, a line that is only there to be read.
    pub id: Option<String>,
    /// Shown and searchable like an item, but skipped by the cursor. A key
    /// binding in the help list is exactly this: worth finding, nothing to pick.
    pub info: bool,
    pub label: String,
    /// Extra lower-cased text the search matches on — a provider's base url, so
    /// "opencode" finds the zen/go groups even though the config keys don't say it.
    pub hay: String,
    /// Trailing muted note, e.g. "← current".
    pub note: String,
    /// `Some(true)` draws a filled dot (connected, current), `Some(false)` a hollow one.
    pub dot: Option<bool>,
}

impl Row {
    pub fn item(id: impl Into<String>, label: impl Into<String>) -> Self {
        Row { id: Some(id.into()), label: label.into(), ..Default::default() }
    }

    pub fn heading(label: impl Into<String>) -> Self {
        Row { id: None, label: label.into(), ..Default::default() }
    }

    pub fn info(label: impl Into<String>) -> Self {
        Row { id: None, info: true, label: label.into(), ..Default::default() }
    }

    /// Opens a group. An `info` row belongs to whatever group it is in, so it
    /// must not close one.
    fn is_heading(&self) -> bool {
        self.id.is_none() && !self.info
    }

    pub fn hay(mut self, extra: impl Into<String>) -> Self {
        self.hay = extra.into().to_lowercase();
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    pub fn dot(mut self, on: bool) -> Self {
        self.dot = Some(on);
        self
    }

    /// Everything the search reads for this row: its group's name and extra text,
    /// then its own. So `openrouter hy` finds the `hy` models of that group.
    fn haystack(&self, group: Option<&Row>) -> String {
        let mut text = String::new();
        if let Some(g) = group {
            text.push_str(&format!("{} {} ", g.label.to_lowercase(), g.hay));
        }
        text.push_str(&format!("{} {}", self.label.to_lowercase(), self.hay));
        text
    }
}

/// How well `terms` fit `hay`, lower being better; `None` when one does not fit.
///
/// Every whitespace-separated term has to be there, in any order. A term fits as
/// a substring of anything searchable, or failing that as scattered letters in
/// order inside the row's own label (`gm4` finds `gemini-4`). Scattered letters
/// are not tried across the group's name and address: spread over that much text
/// almost anything would fit. The more spread out the letters, the worse the score.
fn fuzzy_score(terms: &[String], hay: &str, label: &str) -> Option<usize> {
    let mut total = 0;
    for term in terms {
        if hay.contains(term.as_str()) {
            continue;
        }
        let mut wanted = term.chars();
        let mut next = wanted.next();
        let (mut first, mut last) = (None, 0);
        for (at, c) in label.chars().enumerate() {
            if Some(c) == next {
                first.get_or_insert(at);
                last = at;
                next = wanted.next();
                if next.is_none() {
                    break;
                }
            }
        }
        if next.is_some() {
            return None;
        }
        total += last + 1 - first.unwrap_or(0) - term.chars().count() + 1;
    }
    Some(total)
}

/// Rows shown at once before the list scrolls.
const VIEW_ROWS: usize = 12;

pub struct Picker {
    pub kind: Kind,
    pub title: String,
    rows: Vec<Row>,
    query: String,
    /// Index into the *filtered* list.
    sel: usize,
    /// First filtered row on screen.
    top: usize,
    /// The rows haven't arrived yet (a catalog fetch is in flight).
    pub loading: bool,
}

impl Picker {
    pub fn new(kind: Kind, title: impl Into<String>, rows: Vec<Row>) -> Self {
        let mut p = Picker {
            kind,
            title: title.into(),
            rows,
            query: String::new(),
            sel: 0,
            top: 0,
            loading: false,
        };
        p.settle();
        p
    }

    /// A picker whose rows are still being fetched. It opens immediately and fills
    /// in later — a modal that appears only after a network round trip feels broken.
    pub fn pending(kind: Kind, title: impl Into<String>) -> Self {
        Picker { loading: true, ..Picker::new(kind, title, Vec::new()) }
    }

    pub fn set_rows(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.loading = false;
        self.sel = 0;
        self.top = 0;
        self.settle();
    }

    pub fn insert(&mut self, c: char) {
        self.query.push(c);
        // The old selection means nothing once the list changes under it.
        self.sel = 0;
        self.top = 0;
        self.settle();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.sel = 0;
        self.top = 0;
        self.settle();
    }

    pub fn up(&mut self) {
        self.step(-1);
    }

    pub fn down(&mut self) {
        self.step(1);
    }

    /// The id under the cursor, or `None` when nothing matches the query.
    pub fn selected(&self) -> Option<&str> {
        let vis = self.filtered();
        vis.get(self.sel)
            .and_then(|&i| self.rows[i].id.as_deref())
    }

    /// Indices of the rows the query keeps, best fit first within each group. A
    /// heading survives only if at least one of its children does, and its own
    /// text counts for every child.
    fn filtered(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        let terms: Vec<String> = query.split_whitespace().map(str::to_string).collect();
        if terms.is_empty() {
            return (0..self.rows.len()).collect();
        }
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.rows.len() {
            if !self.rows[i].is_heading() {
                if fuzzy_score(&terms, &self.rows[i].haystack(None), &self.rows[i].label.to_lowercase()).is_some() {
                    out.push(i);
                }
                i += 1;
                continue;
            }
            let start = i + 1;
            let mut end = start;
            while end < self.rows.len() && !self.rows[end].is_heading() {
                end += 1;
            }
            let mut kids: Vec<(usize, usize)> = (start..end)
                .filter_map(|j| {
                    let row = &self.rows[j];
                    fuzzy_score(&terms, &row.haystack(Some(&self.rows[i])), &row.label.to_lowercase())
                        .map(|s| (s, j))
                })
                .collect();
            kids.sort();
            if !kids.is_empty() {
                out.push(i);
                out.extend(kids.into_iter().map(|(_, j)| j));
            }
            i = end;
        }
        out
    }

    /// Move the cursor by `delta`, skipping headings and stopping at the ends.
    fn step(&mut self, delta: isize) {
        let vis = self.filtered();
        if vis.is_empty() {
            return;
        }
        let mut i = self.sel as isize;
        loop {
            i += delta;
            if i < 0 || i as usize >= vis.len() {
                return; // no selectable row that way; stay put
            }
            if self.rows[vis[i as usize]].id.is_some() {
                self.sel = i as usize;
                self.scroll_into_view(vis.len());
                return;
            }
        }
    }

    /// Put the cursor on the first selectable row and fix up the scroll offset.
    fn settle(&mut self) {
        let vis = self.filtered();
        if let Some(first) = vis.iter().position(|&i| self.rows[i].id.is_some()) {
            if self.sel >= vis.len() || self.rows[vis[self.sel]].id.is_none() {
                self.sel = first;
            }
        }
        self.scroll_into_view(vis.len());
    }

    fn scroll_into_view(&mut self, len: usize) {
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + VIEW_ROWS {
            self.top = self.sel + 1 - VIEW_ROWS;
        }
        // A shrinking list can leave `top` past the end, showing a blank modal.
        self.top = self.top.min(len.saturating_sub(1));
    }

    /// Content height this picker wants: search row, rule, and the visible rows.
    pub fn height(&self) -> u16 {
        let list = self.filtered().len().clamp(1, VIEW_ROWS);
        (list + 2) as u16
    }

    /// Columns the widest row would like, so a list of long lines gets a box that
    /// fits it instead of one truncated at 60%. Capped, and `overlay::area` clamps
    /// to the window besides — a picker wider than the terminal helps nobody.
    fn min_cols(&self) -> u16 {
        const CAP: usize = 96;
        // marker, dot, and the two spaces before a note
        const CHROME: usize = 6;
        self.rows
            .iter()
            .map(|r| r.label.chars().count() + r.note.chars().count() + CHROME)
            .max()
            .unwrap_or(0)
            .min(CAP) as u16
    }

    pub fn draw(&self, f: &mut Frame, frame: Rect, theme: &Theme) {
        let area = overlay::area(frame, 60, self.min_cols(), self.height());
        let hint = "↑↓ move · enter pick · esc cancel";
        let inner = overlay::draw(f, area, &self.title, hint, theme);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let w = inner.width as usize;
        let mut lines = Vec::with_capacity(inner.height as usize);

        lines.push(Line::from(vec![
            Span::styled("❯ ", Style::default().fg(theme.accent)),
            Span::styled(self.query.clone(), Style::default().fg(theme.fg)),
            // A block caret: the terminal's real cursor belongs to the prompt below.
            Span::styled("▏", Style::default().fg(theme.accent)),
        ]));
        lines.push(Line::from(Span::styled(
            "─".repeat(w),
            Style::default().fg(theme.muted),
        )));

        let vis = self.filtered();
        if self.loading {
            lines.push(overlay::row(
                "  loading…".into(),
                inner.width,
                Style::default().fg(theme.muted),
            ));
        } else if vis.is_empty() {
            lines.push(overlay::row(
                "  no match".into(),
                inner.width,
                Style::default().fg(theme.muted),
            ));
        }

        for (n, &i) in vis.iter().enumerate().skip(self.top).take(VIEW_ROWS) {
            let row = &self.rows[i];
            let on = n == self.sel;
            if row.is_heading() {
                lines.push(overlay::row(
                    overlay::truncate(&row.label, w),
                    inner.width,
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                ));
                continue;
            }
            if row.info {
                // Indented like an item so the columns line up with the ones
                // above it, muted so it doesn't read as something to choose.
                let text = format!("  {}  {}", row.label, row.note);
                lines.push(overlay::row(
                    overlay::truncate(&text, w),
                    inner.width,
                    Style::default().fg(theme.muted),
                ));
                continue;
            }
            let dot = match row.dot {
                Some(true) => "● ",
                Some(false) => "○ ",
                None => "",
            };
            let text = format!(
                "{}{dot}{}{}",
                if on { "▸ " } else { "  " },
                row.label,
                if row.note.is_empty() { String::new() } else { format!("  {}", row.note) }
            );
            let style = if on {
                Style::default().fg(theme.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.fg)
            };
            lines.push(overlay::row(overlay::truncate(&text, w), inner.width, style));
        }
        overlay::body(f, inner, lines);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grouped() -> Picker {
        Picker::new(
            Kind::Model,
            "Select model",
            vec![
                Row::heading("anthropic").hay("https://api.anthropic.com"),
                Row::item("anthropic:claude-opus-5", "claude-opus-5"),
                Row::item("anthropic:claude-sonnet-5", "claude-sonnet-5"),
                Row::heading("opencode-go").hay("https://opencode.ai/zen/go/v1"),
                Row::item("opencode-go:gpt-5.6-luna", "gpt-5.6-luna"),
            ],
        )
    }

    #[test]
    fn the_cursor_starts_on_the_first_item_never_on_a_heading() {
        // Enter on a heading would dismiss the picker with nothing chosen.
        assert_eq!(grouped().selected(), Some("anthropic:claude-opus-5"));
    }

    #[test]
    fn moving_down_skips_headings() {
        let mut p = grouped();
        p.down();
        assert_eq!(p.selected(), Some("anthropic:claude-sonnet-5"));
        p.down();
        assert_eq!(p.selected(), Some("opencode-go:gpt-5.6-luna"), "skipped the heading");
    }

    #[test]
    fn the_cursor_stops_at_the_ends_instead_of_wrapping() {
        let mut p = grouped();
        for _ in 0..20 {
            p.down();
        }
        assert_eq!(p.selected(), Some("opencode-go:gpt-5.6-luna"));
        for _ in 0..20 {
            p.up();
        }
        assert_eq!(p.selected(), Some("anthropic:claude-opus-5"));
    }

    #[test]
    fn searching_filters_and_drops_the_headings_left_empty() {
        let mut p = grouped();
        for c in "luna".chars() {
            p.insert(c);
        }
        let vis = p.filtered();
        assert_eq!(vis.len(), 2, "the opencode-go heading and its one match");
        assert_eq!(p.selected(), Some("opencode-go:gpt-5.6-luna"));
    }

    fn models() -> Picker {
        let rows = vec![
            Row::heading("llamacpp").hay("llamacpp http://127.0.0.1:8080/v1"),
            Row::item("llamacpp/inkling/Q4.gguf", "  inkling/Q4.gguf").hay("llamacpp llamacpp/inkling/Q4.gguf"),
            Row::heading("openrouter").hay("openrouter https://openrouter.ai/api/v1"),
            Row::item("google/gemini-4-neon", "  google/gemini-4-neon").hay("openrouter google/gemini-4-neon"),
            Row::item("tencent/hy4-flash", "  tencent/hy4-flash").hay("openrouter tencent/hy4-flash"),
            Row::item("tencent/hy4-pro", "  tencent/hy4-pro").hay("openrouter tencent/hy4-pro"),
        ];
        Picker::new(Kind::Model, "Select model", rows)
    }

    fn typed(mut p: Picker, query: &str) -> Picker {
        for c in query.chars() {
            p.insert(c);
        }
        p
    }

    #[test]
    fn a_group_name_and_a_model_fragment_can_be_typed_together() {
        let p = typed(models(), "openrouter hy");
        let shown: Vec<&str> = p.filtered().iter().map(|&i| p.rows[i].label.trim()).collect();
        assert_eq!(shown, vec!["openrouter", "tencent/hy4-flash", "tencent/hy4-pro"]);
    }

    #[test]
    fn terms_can_come_in_any_order() {
        let p = typed(models(), "hy openrouter");
        assert_eq!(p.filtered().len(), 3);
    }

    #[test]
    fn scattered_letters_still_find_the_model() {
        let p = typed(models(), "gm4");
        assert_eq!(p.selected(), Some("google/gemini-4-neon"));
    }

    #[test]
    fn a_tighter_match_is_listed_before_a_looser_one() {
        let rows = vec![
            Row::heading("openrouter"),
            Row::item("a", "  h-x-x-y-model"),
            Row::item("b", "  hy4-flash"),
        ];
        let p = typed(Picker::new(Kind::Model, "m", rows), "hy");
        assert_eq!(p.selected(), Some("b"), "the substring beats the scattered match");
    }

    #[test]
    fn every_term_has_to_fit() {
        let p = typed(models(), "openrouter inkling");
        assert!(p.filtered().is_empty(), "inkling lives under llamacpp, not openrouter");
    }

    #[test]
    fn a_query_matching_a_group_keeps_the_whole_group() {
        let mut p = grouped();
        for c in "anthropic".chars() {
            p.insert(c);
        }
        assert_eq!(p.filtered().len(), 3, "heading plus both of its models");
    }

    #[test]
    fn a_group_is_findable_by_its_endpoint_url() {
        // The config key is "opencode-go"; the user types what they know.
        let mut p = grouped();
        for c in "zen".chars() {
            p.insert(c);
        }
        assert_eq!(p.selected(), Some("opencode-go:gpt-5.6-luna"));
    }

    #[test]
    fn a_query_that_matches_nothing_selects_nothing() {
        let mut p = grouped();
        for c in "banana".chars() {
            p.insert(c);
        }
        assert!(p.filtered().is_empty());
        assert_eq!(p.selected(), None, "enter here must not pick a stale row");
    }

    #[test]
    fn backspacing_brings_the_list_back() {
        let mut p = grouped();
        for c in "luna".chars() {
            p.insert(c);
        }
        for _ in 0..4 {
            p.backspace();
        }
        assert_eq!(p.filtered().len(), 5);
        assert_eq!(p.selected(), Some("anthropic:claude-opus-5"));
    }

    #[test]
    fn selection_resets_when_the_query_changes_under_it() {
        let mut p = grouped();
        p.down();
        p.down();
        assert_eq!(p.selected(), Some("opencode-go:gpt-5.6-luna"));
        for c in "claude".chars() {
            p.insert(c);
        }
        assert_eq!(p.selected(), Some("anthropic:claude-opus-5"), "stale index would pick wrong");
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_cursor_visible() {
        let rows: Vec<Row> = (0..40).map(|i| Row::item(format!("m{i}"), format!("model-{i}"))).collect();
        let mut p = Picker::new(Kind::Model, "t", rows);
        assert_eq!(p.top, 0);
        for _ in 0..20 {
            p.down();
        }
        assert_eq!(p.selected(), Some("m20"));
        assert!(p.top <= 20 && 20 < p.top + VIEW_ROWS, "cursor off screen: top={}", p.top);
    }

    #[test]
    fn the_modal_never_grows_past_its_window() {
        let rows: Vec<Row> = (0..400).map(|i| Row::item(format!("m{i}"), format!("model-{i}"))).collect();
        let p = Picker::new(Kind::Model, "t", rows);
        assert_eq!(p.height(), (VIEW_ROWS + 2) as u16);
    }

    #[test]
    fn an_empty_picker_still_has_a_row_to_draw_into() {
        let p = Picker::pending(Kind::Model, "t");
        assert!(p.loading);
        assert!(p.height() >= 3, "a zero-height body would draw nothing at all");
        assert_eq!(p.selected(), None);
    }

    #[test]
    fn rows_arriving_late_replace_the_loading_state() {
        let mut p = Picker::pending(Kind::Model, "t");
        p.set_rows(vec![Row::item("a", "alpha")]);
        assert!(!p.loading);
        assert_eq!(p.selected(), Some("a"));
    }
}
