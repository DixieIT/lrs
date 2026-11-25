//! Plugin-drawn panels, pinned above the prompt.
//!
//! A plugin calls `api.widget("goal").set("Pursuing goal (…)")` and that string
//! shows up here. lrs knows nothing about goal, or about any other plugin:
//! it renders whatever markup arrives, keyed by widget id, in the order the
//! plugins created them. That is the whole contract — the same one the Textual
//! TUI implements with a `Static` per widget.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::markup;
use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Widget {
    pub id: String,
    pub markup: String,
}

/// Replace or insert one widget. Empty markup removes it, so a plugin's `clear()`
/// and a `set("")` behave the same rather than leaving a blank row behind.
pub fn upsert(list: &mut Vec<Widget>, id: &str, markup: Option<String>) {
    let markup = markup.filter(|m| !m.trim().is_empty());
    match markup {
        None => list.retain(|w| w.id != id),
        Some(m) => match list.iter_mut().find(|w| w.id == id) {
            // Updating in place keeps a plugin's panel from jumping to the bottom
            // of the stack every time its content changes.
            Some(w) => w.markup = m,
            None => list.push(Widget { id: id.to_string(), markup: m }),
        },
    }
}

pub fn height(list: &[Widget]) -> u16 {
    list.len() as u16
}

pub fn lines(list: &[Widget], theme: &Theme) -> Vec<Line<'static>> {
    list.iter()
        .map(|w| {
            let mut line = markup::line(&w.markup, Style::default().fg(theme.fg), theme);
            // A left marker ties the row to the pinned strip and tells it apart
            // from transcript content that happens to sit above it.
            line.spans.insert(0, Span::styled("▎ ", Style::default().fg(theme.accent)));
            line
        })
        .collect()
}

/// Trim a rendered line to `width` columns, marking the cut. Clipping at the pane
/// edge would look identical to a line that simply ends there; a long objective
/// should say it was shortened.
pub fn fit(line: Line<'static>, width: u16) -> Line<'static> {
    let width = width as usize;
    let total: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    if total <= width || width == 0 {
        return line;
    }
    let keep = width.saturating_sub(1);
    let mut left = keep;
    let mut spans = Vec::new();
    for span in line.spans {
        if left == 0 {
            break;
        }
        let n = span.content.chars().count();
        if n <= left {
            left -= n;
            spans.push(span);
        } else {
            let cut: String = span.content.chars().take(left).collect();
            left = 0;
            spans.push(Span::styled(cut, span.style));
        }
    }
    spans.push(Span::raw("…"));
    Line::from(spans)
}

pub fn draw(f: &mut Frame, area: Rect, list: &[Widget], theme: &Theme) {
    let rows: Vec<Line<'static>> = lines(list, theme)
        .into_iter()
        .map(|l| fit(l, area.width))
        .collect();
    f.render_widget(Paragraph::new(rows), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line<'static>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn a_widget_appears_and_then_updates_in_place() {
        let mut list = Vec::new();
        upsert(&mut list, "goal", Some("primo".into()));
        upsert(&mut list, "other", Some("altro".into()));
        upsert(&mut list, "goal", Some("secondo".into()));
        assert_eq!(list.len(), 2);
        // Order is the plugins' creation order; an update must not reshuffle it.
        assert_eq!(list[0].id, "goal");
        assert_eq!(list[0].markup, "secondo");
        assert_eq!(list[1].id, "other");
    }

    #[test]
    fn clearing_removes_the_row_rather_than_blanking_it() {
        let mut list = Vec::new();
        upsert(&mut list, "goal", Some("x".into()));
        upsert(&mut list, "goal", None);
        assert!(list.is_empty());
        assert_eq!(height(&list), 0, "an empty strip must take no rows at all");
    }

    #[test]
    fn empty_markup_counts_as_a_clear() {
        // The goal plugin sets "" when there is no objective.
        let mut list = Vec::new();
        upsert(&mut list, "goal", Some("x".into()));
        upsert(&mut list, "goal", Some("   ".into()));
        assert!(list.is_empty());
    }

    #[test]
    fn clearing_a_widget_that_was_never_there_is_harmless() {
        let mut list = Vec::new();
        upsert(&mut list, "ghost", None);
        assert!(list.is_empty());
    }

    #[test]
    fn markup_is_rendered_not_shown_raw() {
        let t = Theme::tokyonight();
        let list = vec![Widget {
            id: "goal".into(),
            markup: "Pursuing goal ([bold cyan]…[/bold cyan] spedire)".into(),
        }];
        let rendered = text(&lines(&list, &t)[0]);
        assert!(rendered.contains("Pursuing goal (… spedire)"), "got {rendered:?}");
        assert!(!rendered.contains("[bold"), "raw markup leaked: {rendered:?}");
    }

    #[test]
    fn a_long_widget_is_cut_with_a_visible_mark() {
        let t = Theme::tokyonight();
        let list = vec![Widget {
            id: "goal".into(),
            markup: "[bold]obiettivo[/bold] ".to_string() + &"molto lungo ".repeat(10),
        }];
        let cut = fit(lines(&list, &t).remove(0), 20);
        let text: String = cut.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.chars().count(), 20);
        assert!(text.ends_with('…'), "silent clipping reads like the line just ends");
        // and the styling of the surviving part is kept
        assert!(cut.spans.len() > 2, "everything collapsed into one span: {cut:?}");
    }

    #[test]
    fn a_line_that_fits_is_left_exactly_as_it_was() {
        let t = Theme::tokyonight();
        let list = vec![Widget { id: "a".into(), markup: "corto".into() }];
        let before = lines(&list, &t).remove(0);
        let after = fit(before.clone(), 80);
        assert_eq!(text(&before), text(&after));
    }

    #[test]
    fn each_widget_takes_exactly_one_row() {
        let list = vec![
            Widget { id: "a".into(), markup: "uno".into() },
            Widget { id: "b".into(), markup: "due".into() },
        ];
        assert_eq!(height(&list), 2);
        assert_eq!(lines(&list, &Theme::tokyonight()).len(), 2);
    }
}
