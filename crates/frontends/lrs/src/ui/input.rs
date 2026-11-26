//! The prompt: a small multi-line editor plus the slash-command menu above it.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Theme;

/// A multi-line text buffer with a cursor. Byte-indexed internally but every
/// operation moves by *character*, so non-ASCII input can't split a codepoint.
#[derive(Debug, Default, Clone)]
pub struct Editor {
    text: String,
    cursor: usize,
    /// Byte range of a completed `<placeholder>`, highlighted and replaced whole by
    /// the next thing typed. It is the closest thing to a selection the prompt has,
    /// and it exists for exactly this: `/goal <objective>` should be ready to type
    /// over, not something to delete first.
    placeholder: Option<(usize, usize)>,
    /// The text is a command to run, not a message. A mode of the prompt, shown
    /// as `!` where the `❯` is, and not part of the text.
    shell: bool,
}

impl Editor {
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn shell_mode(&self) -> bool {
        self.shell
    }

    pub fn enter_shell(&mut self) {
        self.shell = true;
    }

    pub fn leave_shell(&mut self) {
        self.shell = false;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.placeholder = None;
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.cursor = self.text.len();
        self.placeholder = None;
    }

    /// Replace the prompt with a completion, marking any `<placeholder>` in it so
    /// the next keystroke types over it instead of appending to it.
    pub fn complete(&mut self, s: &str) {
        self.set(s);
        if let Some(start) = s.find('<') {
            if let Some(end) = s[start..].find('>') {
                self.placeholder = Some((start, start + end + 1));
                self.cursor = start;
            }
        }
    }

    /// The highlighted placeholder range, for drawing.
    pub fn placeholder(&self) -> Option<(usize, usize)> {
        self.placeholder
    }

    pub fn take(&mut self) -> String {
        let out = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.placeholder = None;
        out
    }

    /// Drop the placeholder text, leaving the caret where it was. Called before
    /// any edit, so typing over it behaves like typing over a selection.
    fn consume_placeholder(&mut self) {
        if let Some((s, e)) = self.placeholder.take() {
            if e <= self.text.len() && self.text.is_char_boundary(s) && self.text.is_char_boundary(e)
            {
                self.text.replace_range(s..e, "");
                self.cursor = s;
            }
        }
    }

    pub fn insert(&mut self, c: char) {
        self.consume_placeholder();
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        self.consume_placeholder();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn backspace(&mut self) {
        // Backspace on a highlighted placeholder deletes the placeholder, the way
        // it would delete a selection — not one character of it.
        if self.placeholder.is_some() {
            self.consume_placeholder();
            return;
        }
        if let Some(prev) = self.prev_boundary() {
            self.text.remove(prev);
            // `remove` takes a byte index and shifts the rest down; for a
            // multi-byte char we must drop the whole thing.
            while self.cursor > prev && !self.text.is_char_boundary(prev) {
                self.text.remove(prev);
            }
            self.cursor = prev;
        }
    }

    /// Ctrl+W — delete the whitespace-delimited word before the cursor.
    pub fn delete_word(&mut self) {
        let head = &self.text[..self.cursor];
        let trimmed = head.trim_end();
        let start = match trimmed.rfind(char::is_whitespace) {
            Some(i) => i + 1,
            None => 0,
        };
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn left(&mut self) {
        if let Some(p) = self.prev_boundary() {
            self.cursor = p;
        }
    }

    pub fn right(&mut self) {
        if self.cursor < self.text.len() {
            let mut n = self.cursor + 1;
            while n < self.text.len() && !self.text.is_char_boundary(n) {
                n += 1;
            }
            self.cursor = n;
        }
    }

    pub fn home(&mut self) {
        self.cursor = self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
    }

    pub fn end(&mut self) {
        self.cursor = self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |i| self.cursor + i);
    }

    /// Caret position as a byte offset. Only the chip logic needs this — it has to
    /// know what sits immediately before the caret.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Delete the `n` bytes before the caret as one unit. Used to remove a paste
    /// chip whole: shaving one character off `[Pasted 40 lines]` leaves a label the
    /// user can neither read nor repair.
    pub fn delete_back(&mut self, n: usize) {
        let start = self.cursor.saturating_sub(n);
        if !self.text.is_char_boundary(start) {
            return;
        }
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    /// Swap out one byte range, leaving the caret after what replaced it.
    ///
    /// This is what a partial completion needs: `@path` fills in the token under
    /// the caret and nothing else, so `read @src/ma` becomes `read @src/main.rs `
    /// with the rest of the line untouched.
    pub fn replace_range(&mut self, start: usize, end: usize, s: &str) {
        if start > end
            || end > self.text.len()
            || !self.text.is_char_boundary(start)
            || !self.text.is_char_boundary(end)
        {
            return;
        }
        self.text.replace_range(start..end, s);
        self.cursor = start + s.len();
        self.placeholder = None;
    }

    /// True when the caret sits on the buffer's first source line. The up arrow
    /// reaches the prompt history only from here, so a recalled multi-line prompt
    /// stays navigable instead of trapping the caret.
    pub fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains('\n')
    }

    pub fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains('\n')
    }

    /// Move to the same column on the previous source line (lines, not wrapped
    /// display rows — the buffer knows nothing about the terminal's width here).
    pub fn line_up(&mut self) {
        let start = self.line_start();
        if start == 0 {
            return;
        }
        let col = self.text[start..self.cursor].chars().count();
        let prev_start = self.text[..start - 1].rfind('\n').map_or(0, |i| i + 1);
        self.cursor = advance(&self.text, prev_start, start - 1, col);
    }

    pub fn line_down(&mut self) {
        let start = self.line_start();
        let Some(nl) = self.text[self.cursor..].find('\n').map(|i| self.cursor + i) else {
            return;
        };
        let col = self.text[start..self.cursor].chars().count();
        let next_start = nl + 1;
        let next_end = self.text[next_start..]
            .find('\n')
            .map_or(self.text.len(), |i| next_start + i);
        self.cursor = advance(&self.text, next_start, next_end, col);
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
    }

    fn prev_boundary(&self) -> Option<usize> {
        if self.cursor == 0 {
            return None;
        }
        let mut p = self.cursor - 1;
        while p > 0 && !self.text.is_char_boundary(p) {
            p -= 1;
        }
        Some(p)
    }

    /// Lay the buffer out for `width`, returning the display rows and where the
    /// cursor sits within them as (row, column).
    ///
    /// Hard character wrapping, not word wrapping: an input box that reflows words
    /// under the caret while you type is disorienting, and char wrapping is what
    /// makes the cursor position exactly computable rather than guessed.
    /// Row 0 accounts for the `PROMPT` prefix; later rows are indented to match.
    pub fn layout(&self, width: u16) -> (Vec<String>, (u16, u16)) {
        let inner = (width as usize).saturating_sub(PROMPT.chars().count()).max(1);
        let mut rows: Vec<String> = Vec::new();
        let mut cursor = (0u16, 0u16);
        let mut seen = 0usize; // bytes consumed, to locate the caret

        for (li, line) in self.text.split('\n').enumerate() {
            if li > 0 {
                seen += 1; // the '\n'
            }
            let chars: Vec<char> = line.chars().collect();
            let start_row = rows.len();
            // An empty line still occupies one row.
            for chunk in chars.chunks(inner) {
                rows.push(chunk.iter().collect());
            }
            if chars.is_empty() {
                rows.push(String::new());
            }
            // Is the caret on this source line?
            let line_bytes = line.len();
            if self.cursor >= seen && self.cursor <= seen + line_bytes {
                let off = self.text[seen..self.cursor].chars().count();
                cursor = ((start_row + off / inner) as u16, (off % inner) as u16);
            }
            seen += line_bytes;
        }
        if rows.is_empty() {
            rows.push(String::new());
        }
        (rows, cursor)
    }

    /// Rendered height at `width`, so the layout can grow the prompt as you type.
    pub fn height(&self, width: u16, max: u16) -> u16 {
        (self.layout(width).0.len() as u16).clamp(1, max)
    }
}

/// Byte offset `col` characters into `start..end`, clamped to `end`. A shorter
/// line puts the caret at its end rather than past it.
fn advance(text: &str, start: usize, end: usize, col: usize) -> usize {
    text[start..end]
        .char_indices()
        .nth(col)
        .map_or(end, |(i, _)| start + i)
}

/// Drawn before the first row; later rows align under it.
const PROMPT: &str = "❯ ";
/// The prompt while the text is a command to run. As wide as `PROMPT`, so the
/// caret and the wrapping do not move when the mode changes.
const SHELL_PROMPT: &str = "! ";

/// Draw the prompt and place the terminal caret. ratatui hides the cursor unless
/// a position is set every frame, so this must run on each draw or the input has
/// no visible caret at all.
pub fn draw(f: &mut Frame, area: Rect, ed: &Editor, placeholder: &str, theme: &Theme) {
    // The `!` stands where the `❯` does, so you can tell before pressing enter
    // that this runs here instead of going to the model.
    let (glyph, colour) = if ed.shell_mode() { (SHELL_PROMPT, theme.warn) } else { (PROMPT, theme.accent) };
    let prompt = Style::default().fg(colour).add_modifier(Modifier::BOLD);
    let indent = " ".repeat(PROMPT.chars().count());

    let lines: Vec<Line> = if ed.is_empty() {
        vec![Line::from(vec![
            Span::styled(glyph, prompt),
            Span::styled(
                if ed.shell_mode() { String::new() } else { placeholder.to_string() },
                Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC),
            ),
        ])]
    } else {
        let (rows, _) = ed.layout(area.width);
        let inner = (area.width as usize).saturating_sub(PROMPT.chars().count()).max(1);
        // A placeholder is only ever set by `complete()`, which replaces the whole
        // prompt with a single-line completion — so character offsets across the
        // wrapped rows are simply `row * inner`, with no newline to account for.
        let ph = ed.placeholder().map(|(s, e)| {
            let text = ed.text();
            (text[..s].chars().count(), text[..e].chars().count())
        });
        rows.into_iter()
            .enumerate()
            .map(|(i, r)| {
                let lead =
                    Span::styled(if i == 0 { glyph.to_string() } else { indent.clone() }, prompt);
                let plain = Style::default().fg(theme.fg);
                match ph {
                    Some((ps, pe)) => {
                        let base = i * inner;
                        let chars: Vec<char> = r.chars().collect();
                        let mut spans = vec![lead];
                        // Reverse video reads as "selected" under every theme, which
                        // is what this is: text the next keystroke replaces.
                        let hl = plain.add_modifier(Modifier::REVERSED);
                        let mut run = String::new();
                        let mut run_hl = false;
                        for (n, c) in chars.into_iter().enumerate() {
                            let on = (base + n) >= ps && (base + n) < pe;
                            if on != run_hl && !run.is_empty() {
                                spans.push(Span::styled(
                                    std::mem::take(&mut run),
                                    if run_hl { hl } else { plain },
                                ));
                            }
                            run_hl = on;
                            run.push(c);
                        }
                        if !run.is_empty() {
                            spans.push(Span::styled(run, if run_hl { hl } else { plain }));
                        }
                        Line::from(spans)
                    }
                    None => Line::from(vec![lead, Span::styled(r, plain)]),
                }
            })
            .collect()
    };
    // The pane stops growing at its cap, but the text does not: a long prompt
    // showed its first rows and hid the caret below the pane, so whatever was
    // typed or pasted last could not be seen. Scroll so the caret's row is in view.
    let (row, col) = ed.layout(area.width).1;
    let top = first_visible_row(row, area.height);
    f.render_widget(Paragraph::new(lines).scroll((top, 0)), area);

    let x = area.x + PROMPT.chars().count() as u16 + col;
    let y = area.y + row - top;
    // Clamp: a caret drawn outside its pane lands somewhere else on screen.
    if y < area.y + area.height {
        f.set_cursor_position((x.min(area.x + area.width.saturating_sub(1)), y));
    }
}

/// The first row drawn, so that `caret_row` lands on the pane's last row once it
/// would otherwise fall below it. Short prompts start at the top as before.
fn first_visible_row(caret_row: u16, height: u16) -> u16 {
    (caret_row + 1).saturating_sub(height.max(1))
}

/// One row of the completion menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuRow {
    /// What replaces the prompt when this row is taken.
    pub insert: String,
    /// What is shown in the menu — for a subcommand, the argument alone.
    pub label: String,
    pub desc: String,
    /// The byte range of the prompt `insert` replaces. `None` means the whole
    /// prompt, which is right for a slash command — it always starts the line.
    /// An `@path` only replaces the token under the caret, so it carries a range.
    pub replace: Option<(usize, usize)>,
}

impl MenuRow {
    fn command(insert: String, label: String, desc: String) -> Self {
        MenuRow { insert, label, desc, replace: None }
    }

    /// The prompt this row would produce, for deciding whether enter should
    /// complete or send.
    pub fn applied(&self, text: &str) -> String {
        match self.replace {
            Some((s, e)) if e <= text.len() => format!("{}{}{}", &text[..s], self.insert, &text[e..]),
            _ => self.insert.clone(),
        }
    }
}

/// The `@path` token the caret sits in: the byte range of the whole token
/// (`@` included) and the query typed after the `@`.
///
/// The `@` has to open a word — otherwise every `user@host` and every email
/// address in a prompt would pop the file menu open mid-sentence.
pub fn at_token(text: &str, cursor: usize) -> Option<(usize, usize, &str)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let before = &text[..cursor];
    let at = before.rfind('@')?;
    if at > 0 && !before[..at].ends_with(char::is_whitespace) {
        return None;
    }
    let query = &before[at + 1..];
    // Whitespace ends the token: the caret has moved on to ordinary prose.
    if query.contains(char::is_whitespace) {
        return None;
    }
    Some((at, cursor, query))
}

/// File rows for the `@path` token under the caret, or nothing when there isn't
/// one. `files` is the project list, already scanned.
pub fn file_matches(text: &str, cursor: usize, files: &[String]) -> Vec<MenuRow> {
    let Some((start, end, query)) = at_token(text, cursor) else { return Vec::new() };
    // Follow the completion with a space, because after picking a file you are
    // almost always about to keep typing — unless the line already has one there,
    // which happens whenever the caret is in the middle of an existing sentence.
    let tail = if text[end..].starts_with(char::is_whitespace) { "" } else { " " };
    crate::app::files::matches(query, files)
        .into_iter()
        .map(|path| MenuRow {
            insert: format!("@{path}{tail}"),
            label: format!("@{path}"),
            desc: String::new(),
            replace: Some((start, end)),
        })
        .collect()
}

/// The filtered menu: commands while a bare `/name` is being typed, then that
/// command's declared subcommands once a space has been typed.
///
/// Subcommand hints are the reason a plugin can teach the prompt about itself:
/// `/goal ` lists pause/resume/clear and `<objective>`, all declared by the
/// plugin, none of it known to this crate.
pub fn menu_matches(input: &str, table: &crate::app::commands::Table) -> Vec<MenuRow> {
    if !input.starts_with('/') {
        return Vec::new();
    }
    let Some((head, rest)) = input.split_once(' ') else {
        let q = &input[1..];
        return table
            .entries
            .iter()
            .filter(|e| e.name[1..].starts_with(q))
            .map(|e| MenuRow::command(format!("{} ", e.name), e.name.clone(), e.desc.clone()))
            .collect();
    };
    let Some(entry) = table.get(head) else { return Vec::new() };
    // Once the user is typing free text, hints are in the way.
    if entry.sub.is_empty() {
        return Vec::new();
    }
    let mut rows: Vec<MenuRow> = entry
        .sub
        .iter()
        .filter(|(arg, _)| !arg.starts_with('<') && arg.starts_with(rest))
        .map(|(arg, desc)| MenuRow::command(format!("{head} {arg}"), arg.clone(), desc.clone()))
        .collect();
    // The free-text placeholder only makes sense before anything is typed, and it
    // goes last so the default highlight lands on something that completes.
    if rest.is_empty() {
        rows.extend(
            entry.sub.iter().filter(|(a, _)| a.starts_with('<')).map(|(arg, desc)| {
                MenuRow::command(format!("{head} {arg}"), arg.clone(), desc.clone())
            }),
        );
    }
    rows
}

pub fn draw_menu(f: &mut Frame, area: Rect, rows: &[MenuRow], selected: usize, theme: &Theme) {
    const GUTTER: usize = 4; // the "  ▸ " marker
    const LABEL: usize = 14;
    let width = area.width as usize;
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let on = i == selected;
            let name_style = Style::default()
                .fg(if on { theme.accent } else { theme.fg })
                .add_modifier(if on { Modifier::BOLD } else { Modifier::empty() });
            let mut spans = vec![Span::styled(if on { "  ▸ " } else { "    " }, name_style)];
            if row.desc.is_empty() {
                // A file path: no column to line up with, and it can be long
                // enough to need the whole row.
                spans.push(Span::styled(
                    clip(&row.label, width.saturating_sub(GUTTER)),
                    name_style,
                ));
            } else {
                spans.push(Span::styled(format!("{:<LABEL$}", clip(&row.label, LABEL)), name_style));
                spans.push(Span::styled(
                    clip(&row.desc, width.saturating_sub(GUTTER + LABEL)),
                    Style::default().fg(theme.muted),
                ));
            }
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), area);
}

/// Cut to `max` columns, keeping the *end* of a path: `…/app/files.rs` says more
/// about which file this is than the repository root it shares with every other
/// candidate does.
fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max == 0 {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let skip = n - keep;
    let tail: String = s.chars().skip(skip).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Draw the prompt into a `height`-row pane; the rows shown and the caret.
    fn drawn(ed: &Editor, height: u16) -> (Vec<String>, (u16, u16)) {
        let backend = ratatui::backend::TestBackend::new(30, height);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        let theme = crate::theme::Theme::tokyonight();
        term.draw(|f| draw(f, f.area(), ed, "", &theme)).unwrap();
        let caret = term.get_cursor_position().unwrap();
        let buf = term.backend().buffer().clone();
        let rows = (0..height)
            .map(|y| (0..30).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>().trim_end().to_string())
            .collect();
        (rows, (caret.x, caret.y))
    }

    #[test]
    fn a_prompt_taller_than_its_pane_keeps_the_caret_in_view() {
        // Twelve lines in a three-row pane: the last three show, caret on the last.
        let mut e = Editor::default();
        e.set(&(1..=12).map(|n| format!("riga {n}")).collect::<Vec<_>>().join("\n"));
        let (rows, (x, y)) = drawn(&e, 3);
        assert_eq!(rows, ["  riga 10", "  riga 11", "  riga 12"]);
        assert_eq!((x, y), (9, 2), "caret after `riga 12`, on the pane's last row");
    }

    #[test]
    fn a_prompt_that_fits_still_starts_at_the_top() {
        let mut e = Editor::default();
        e.set("uno\ndue");
        let (rows, (_, y)) = drawn(&e, 3);
        assert_eq!(rows, ["❯ uno", "  due", ""]);
        assert_eq!(y, 1);
    }

    #[test]
    fn typing_and_deleting_ascii() {
        let mut e = Editor::default();
        for c in "ciao".chars() {
            e.insert(c);
        }
        assert_eq!(e.text(), "ciao");
        e.backspace();
        assert_eq!(e.text(), "cia");
    }

    #[test]
    fn multibyte_characters_are_never_split() {
        let mut e = Editor::default();
        for c in "però".chars() {
            e.insert(c);
        }
        e.backspace();
        assert_eq!(e.text(), "per");
        e.left();
        e.insert('ò');
        assert_eq!(e.text(), "peòr");
    }

    #[test]
    fn delete_word_stops_at_the_previous_space() {
        let mut e = Editor::default();
        e.set("fix the bug");
        e.delete_word();
        assert_eq!(e.text(), "fix the ");
        e.delete_word();
        assert_eq!(e.text(), "fix ");
    }

    #[test]
    fn delete_word_on_an_empty_buffer_is_a_noop() {
        let mut e = Editor::default();
        e.delete_word();
        assert_eq!(e.text(), "");
    }

    #[test]
    fn home_and_end_work_per_line_not_per_buffer() {
        let mut e = Editor::default();
        e.set("uno\ndue");
        e.home();
        assert_eq!(e.cursor, 4); // start of "due", not of the buffer
        e.end();
        assert_eq!(e.cursor, 7);
    }

    #[test]
    fn take_empties_the_buffer_and_resets_the_cursor() {
        let mut e = Editor::default();
        e.set("x");
        assert_eq!(e.take(), "x");
        assert!(e.is_empty());
        // must not panic on the next keystroke
        e.insert('y');
        assert_eq!(e.text(), "y");
    }

    use crate::app::commands::Table;
    use lrs::protocol::CommandSpec;

    fn table() -> Table {
        Table::builtin()
    }

    /// Built-ins plus the goal plugin's declared subcommands.
    fn with_goal() -> Table {
        Table::builtin().with_plugins(&[CommandSpec {
            name: "/goal".into(),
            desc: "Set, pause, resume, or clear a session goal".into(),
            sub: std::collections::BTreeMap::from([
                ("<objective>".to_string(), "set a new goal".to_string()),
                ("pause".to_string(), "pause the goal".to_string()),
                ("resume".to_string(), "resume a paused goal".to_string()),
            ]),
        }])
    }

    #[test]
    fn menu_filters_by_prefix() {
        let m = menu_matches("/the", &table());
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].label, "/theme");
        assert_eq!(m[0].insert, "/theme ");
    }

    #[test]
    fn menu_is_hidden_for_plain_text() {
        assert!(menu_matches("ciao", &table()).is_empty());
    }

    #[test]
    fn a_command_with_no_declared_subcommands_shows_no_hints() {
        // Otherwise the menu would sit over the prompt for every argument typed.
        assert!(menu_matches("/theme nord", &table()).is_empty());
        assert!(menu_matches("/theme ", &table()).is_empty());
    }

    #[test]
    fn bare_slash_lists_everything() {
        assert_eq!(menu_matches("/", &table()).len(), table().entries.len());
    }

    #[test]
    fn a_plugins_subcommands_become_hints_once_you_type_a_space() {
        let rows = menu_matches("/goal ", &with_goal());
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["pause", "resume", "<objective>"]);
        // the placeholder goes last, so the default highlight completes something real
        assert_eq!(rows[0].insert, "/goal pause");
        assert_eq!(rows[2].insert, "/goal <objective>");
    }

    #[test]
    fn typing_filters_the_hints_and_drops_the_placeholder() {
        let rows = menu_matches("/goal pa", &with_goal());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].insert, "/goal pause");
        // <objective> is free text: offering it after typing would be noise
        assert!(rows.iter().all(|r| !r.label.starts_with('<')));
    }

    #[test]
    fn a_completed_placeholder_is_typed_over_not_appended_to() {
        let mut e = Editor::default();
        e.complete("/goal <objective>");
        assert_eq!(e.placeholder(), Some((6, 17)));
        for c in "spedire".chars() {
            e.insert(c);
        }
        assert_eq!(e.text(), "/goal spedire");
    }

    #[test]
    fn backspace_on_a_placeholder_removes_all_of_it() {
        let mut e = Editor::default();
        e.complete("/goal <objective>");
        e.backspace();
        assert_eq!(e.text(), "/goal ");
        assert_eq!(e.placeholder(), None);
    }

    #[test]
    fn a_completion_without_a_placeholder_leaves_the_caret_at_the_end() {
        let mut e = Editor::default();
        e.complete("/goal pause");
        assert_eq!(e.placeholder(), None);
        e.insert('!');
        assert_eq!(e.text(), "/goal pause!");
    }

    #[test]
    fn a_pasted_value_also_replaces_the_placeholder() {
        let mut e = Editor::default();
        e.complete("/goal <objective>");
        e.insert_str("spedire la release");
        assert_eq!(e.text(), "/goal spedire la release");
    }

    // ── @path completion ──────────────────────────────────────────────────────

    fn project() -> Vec<String> {
        ["src/main.rs", "src/app/mod.rs", "README.md"].iter().map(|s| s.to_string()).collect()
    }

    /// Type `text` into a fresh editor and hand back the menu rows it would show.
    fn at_rows(text: &str) -> Vec<MenuRow> {
        let mut e = Editor::default();
        e.set(text);
        file_matches(e.text(), e.cursor(), &project())
    }

    #[test]
    fn a_bare_at_offers_the_whole_project() {
        let rows = at_rows("@");
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.label.starts_with('@')));
    }

    #[test]
    fn typing_narrows_the_file_list() {
        let rows = at_rows("@READ");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "@README.md");
        assert_eq!(rows[0].insert, "@README.md ", "a trailing space to keep typing after");
    }

    #[test]
    fn an_at_mid_sentence_completes_only_its_own_token() {
        let rows = at_rows("leggi @src/ma");
        assert_eq!(rows[0].label, "@src/main.rs");
        // "leggi " is 6 bytes; the token runs from there to the caret at 13.
        assert_eq!(rows[0].replace, Some((6, 13)));
        assert_eq!(rows[0].applied("leggi @src/ma"), "leggi @src/main.rs ");
    }

    #[test]
    fn an_at_inside_a_word_is_not_a_file_reference() {
        // Otherwise every email address in a prompt would open the file menu.
        assert!(at_rows("mail me at me@example.com").is_empty());
        assert!(at_token("me@example.com", 14).is_none());
    }

    #[test]
    fn whitespace_closes_the_token() {
        assert!(at_rows("@README.md e poi").is_empty());
    }

    #[test]
    fn a_prompt_with_no_at_offers_nothing() {
        assert!(at_rows("sistema il bug").is_empty());
    }

    #[test]
    fn the_token_is_the_one_the_caret_sits_in() {
        let text = "@uno @due";
        // caret at the end: the second token
        assert_eq!(at_token(text, 9), Some((5, 9, "due")));
        // caret just after the first: the first
        assert_eq!(at_token(text, 4), Some((0, 4, "uno")));
    }

    #[test]
    fn completing_a_file_leaves_the_rest_of_the_line_alone() {
        let mut e = Editor::default();
        e.set("leggi @src/ma e dimmi");
        // caret back to just after "ma", not at the end of the line
        for _ in 0.." e dimmi".len() {
            e.left();
        }
        let rows = file_matches(e.text(), e.cursor(), &project());
        let (s, t) = rows[0].replace.unwrap();
        e.replace_range(s, t, &rows[0].insert);
        assert_eq!(e.text(), "leggi @src/main.rs e dimmi", "no doubled space before ' e dimmi'");
        assert_eq!(&e.text()[..e.cursor()], "leggi @src/main.rs", "caret follows the insert");
    }

    #[test]
    fn replace_range_refuses_a_range_it_cannot_honour() {
        let mut e = Editor::default();
        e.set("però"); // 'ò' spans bytes 3..5
        e.replace_range(4, 5, "x"); // mid-codepoint
        assert_eq!(e.text(), "però");
        e.replace_range(0, 99, "x"); // past the end
        assert_eq!(e.text(), "però");
    }

    #[test]
    fn a_clipped_path_keeps_its_tail() {
        assert_eq!(clip("@src/app/files.rs", 10), "…/files.rs");
        assert_eq!(clip("short", 10), "short");
    }

    #[test]
    fn cursor_starts_at_the_origin_and_follows_typing() {
        let mut e = Editor::default();
        assert_eq!(e.layout(40).1, (0, 0));
        for c in "ciao".chars() {
            e.insert(c);
        }
        assert_eq!(e.layout(40).1, (0, 4));
        e.left();
        assert_eq!(e.layout(40).1, (0, 3));
    }

    #[test]
    fn cursor_moves_to_the_next_row_after_a_newline() {
        let mut e = Editor::default();
        e.set("uno\ndue");
        assert_eq!(e.layout(40).1, (1, 3));
        e.home();
        assert_eq!(e.layout(40).1, (1, 0));
    }

    #[test]
    fn cursor_follows_the_text_across_a_wrap() {
        let mut e = Editor::default();
        // width 12 minus the 2-char prompt leaves 10 usable columns
        e.set(&"x".repeat(12));
        let (rows, cur) = e.layout(12);
        assert_eq!(rows.len(), 2);
        assert_eq!(cur, (1, 2));
    }

    #[test]
    fn cursor_counts_characters_not_bytes() {
        let mut e = Editor::default();
        e.set("però");
        // 4 characters, 5 bytes — a byte count would put the caret past the text
        assert_eq!(e.layout(40).1, (0, 4));
    }

    #[test]
    fn layout_of_an_empty_buffer_is_one_blank_row() {
        assert_eq!(Editor::default().layout(40).0, vec![String::new()]);
    }

    #[test]
    fn a_trailing_newline_gets_its_own_row() {
        let mut e = Editor::default();
        e.set("uno\n");
        let (rows, cur) = e.layout(40);
        assert_eq!(rows.len(), 2);
        assert_eq!(cur, (1, 0), "caret belongs on the new empty line");
    }

    #[test]
    fn height_grows_with_wrapping_and_is_capped() {
        let mut e = Editor::default();
        e.set(&"x".repeat(100));
        assert!(e.height(22, 10) > 1);
        assert_eq!(e.height(22, 2), 2, "must respect the cap");
        assert_eq!(Editor::default().height(80, 10), 1);
    }
}
