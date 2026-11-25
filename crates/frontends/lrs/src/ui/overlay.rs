//! The frame every modal shares: a centred, bordered box drawn over whatever the
//! transcript was showing.
//!
//! Both the [`super::picker`] and the [`super::ask`] prompt render into this, so a
//! new modal is a body function and nothing else — the geometry, the border, the
//! title and the key hint are decided once, here.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as Frame_, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::theme::Theme;

/// A box of `width_pct` of the frame, centred, `rows` of content tall (the border
/// adds two).
///
/// `min_cols` is the content the box would rather not truncate — a modal that cuts
/// off the env var it is asking about has failed at its one job. It only widens the
/// box up to the frame; past that, truncation is unavoidable.
pub fn area(frame: Rect, width_pct: u16, min_cols: u16, rows: u16) -> Rect {
    const MIN_WIDTH: u16 = 24;
    let w = ((frame.width * width_pct) / 100)
        .max(MIN_WIDTH)
        .max(min_cols.saturating_add(2)) // room for the two border columns
        .min(frame.width);
    // A terminal shorter than the modal wants gets a modal that fits it instead of
    // one drawn partly off-screen.
    let h = (rows + 2).min(frame.height);
    Rect {
        x: frame.x + (frame.width.saturating_sub(w)) / 2,
        y: frame.y + (frame.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

/// Clear the region, draw the border, and return the inner area for the body.
pub fn draw(f: &mut Frame, area: Rect, title: &str, hint: &str, theme: &Theme) -> Rect {
    // Without this the transcript shows through the gaps between our own spans.
    f.render_widget(Clear, area);
    // Only paint a background when the theme actually declares one: themes that
    // leave `bg` unset are deliberately transparent, and a modal that forces a
    // colour there would be the one opaque rectangle in an otherwise see-through UI.
    let base = match theme.bg {
        Some(bg) => Style::default().bg(bg),
        None => Style::default(),
    };
    let block = Frame_::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(base.fg(theme.accent))
        .style(base)
        .title(Line::from(vec![
            Span::styled(format!(" {title} "), base.fg(theme.fg).add_modifier(Modifier::BOLD)),
        ]))
        .title_bottom(Line::from(Span::styled(format!(" {hint} "), base.fg(theme.muted))));
    let inner = block.inner(area);
    f.render_widget(block, area);
    inner
}

/// A single body row: `text` in `style`, padded so the modal's background covers
/// the full width rather than stopping at the last glyph.
pub fn row(text: String, width: u16, style: Style) -> Line<'static> {
    let pad = (width as usize).saturating_sub(text.chars().count());
    Line::from(Span::styled(format!("{text}{}", " ".repeat(pad)), style))
}

/// Render `lines` into `area`, dropping any that don't fit.
pub fn body(f: &mut Frame, area: Rect, lines: Vec<Line<'static>>) {
    f.render_widget(Paragraph::new(lines), area);
}

/// Cut a string to `width` display columns, marking the cut with an ellipsis.
pub fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        return s.to_string();
    }
    let keep = width.saturating_sub(1);
    s.chars().take(keep).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    #[test]
    fn the_box_is_centred() {
        let a = area(frame(100, 40), 50, 0, 10);
        assert_eq!(a.width, 50);
        assert_eq!(a.x, 25);
        assert_eq!(a.height, 12, "content plus the two border rows");
        assert_eq!(a.y, 14);
    }

    #[test]
    fn the_box_widens_rather_than_truncate_what_it_is_asking_about() {
        let a = area(frame(100, 40), 20, 60, 4);
        assert!(a.width >= 62, "20% would have cut the message: {a:?}");
        // but never past the frame
        assert_eq!(area(frame(30, 40), 20, 200, 4).width, 30);
    }

    #[test]
    fn a_tiny_terminal_gets_a_box_that_still_fits_inside_it() {
        // Drawing outside the frame panics ratatui; clamping must happen here.
        let f = frame(20, 6);
        let a = area(f, 90, 0, 40);
        assert!(a.width <= f.width, "{a:?} wider than {f:?}");
        assert!(a.height <= f.height, "{a:?} taller than {f:?}");
        assert!(a.x + a.width <= f.width && a.y + a.height <= f.height);
    }

    #[test]
    fn truncation_marks_the_cut() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("abc", 4), "abc");
        // and counts characters, not bytes, so an accented model name isn't split
        assert_eq!(truncate("però-modello", 5), "però…");
    }

    #[test]
    fn a_row_is_padded_to_the_full_width() {
        // A short row that isn't padded leaves the transcript visible behind it.
        let l = row("ab".into(), 6, Style::default());
        assert_eq!(l.spans[0].content.chars().count(), 6);
    }
}
