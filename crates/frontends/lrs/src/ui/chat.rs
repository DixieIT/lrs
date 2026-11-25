//! The transcript pane.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::block::Block;
use crate::theme::Theme;

/// Flatten the transcript into display lines.
///
/// Blocks are separated by a blank line; a *turn* — anything starting with the
/// user speaking again — gets a rule instead, so a long exchange reads as a
/// sequence of turns rather than one undifferentiated column. Never a leading or
/// trailing separator: stray padding at the top of the scrollback is the kind of
/// thing that makes a TUI feel unfinished.
pub fn lines(blocks: &[Block], theme: &Theme, expand_all: bool, width: u16) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for b in blocks {
        let rendered = b.render(theme, expand_all, width);
        if rendered.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(Line::from(""));
            if matches!(b, Block::User(_)) {
                out.push(rule(theme));
                out.push(Line::from(""));
            }
        }
        out.extend(rendered);
    }
    out
}

/// A turn divider. Short and dim on purpose — a full-width bright line would
/// dominate the very content it is separating.
pub fn rule(theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(RULE_WIDTH),
        ratatui::style::Style::default().fg(theme.border),
    ))
}

const RULE_WIDTH: usize = 28;

/// Word-wrap to `width`, preserving each span's style across the break.
///
/// Done here rather than by `Paragraph`'s own `Wrap` because the scroll maths
/// needs the post-wrap line count, and ratatui does not expose it. Wrapping
/// ourselves keeps "how tall is the transcript" and "what gets drawn" derived
/// from the same computation, so they cannot drift.
pub fn wrap(lines: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let w = width as usize;
    let mut out = Vec::new();

    for line in lines {
        let mut cur: Vec<Span<'static>> = Vec::new();
        let mut used = 0usize;
        // True once this source line has been broken. Leading whitespace is
        // meaningful at the start of a source line (it is the author's indent) and
        // meaningless at the start of a continuation row (it is the space we broke
        // on). Conflating the two silently ate every indented line in the
        // transcript except the very first.
        let mut continuation = false;

        for span in &line.spans {
            let style = span.style;
            for word in split_keeping_spaces(&span.content) {
                let wlen = word.chars().count();

                if used == 0 && continuation && word.chars().all(char::is_whitespace) {
                    continue;
                }
                if used + wlen > w && used > 0 {
                    push_trimmed(&mut out, std::mem::take(&mut cur));
                    used = 0;
                    continuation = true;
                    if word.chars().all(char::is_whitespace) {
                        continue;
                    }
                }
                // A single word longer than the viewport: hard-split it rather
                // than overflow, or a long path would push the layout sideways.
                if wlen > w {
                    let mut chunk = String::new();
                    for c in word.chars() {
                        if chunk.chars().count() + used == w {
                            cur.push(Span::styled(std::mem::take(&mut chunk), style));
                            out.push(Line::from(std::mem::take(&mut cur)));
                            used = 0;
                            continuation = true;
                        }
                        chunk.push(c);
                    }
                    used += chunk.chars().count();
                    cur.push(Span::styled(chunk, style));
                    continue;
                }
                used += wlen;
                cur.push(Span::styled(word, style));
            }
        }
        // An empty source line is a real blank row, so always push.
        push_trimmed(&mut out, cur);
    }
    out
}

/// Push a wrapped line with trailing whitespace removed. The space that *caused*
/// the break would otherwise survive at the end of the line — invisible against a
/// plain background, but it shifts centring and shows up under a selection.
fn push_trimmed(out: &mut Vec<Line<'static>>, mut spans: Vec<Span<'static>>) {
    while let Some(last) = spans.last() {
        if last.content.trim_end().is_empty() {
            spans.pop();
        } else {
            break;
        }
    }
    if let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end().to_string();
        *last = Span::styled(trimmed, last.style);
    }
    out.push(Line::from(spans));
}

/// Split into words and whitespace runs, keeping both so indentation survives.
fn split_keeping_spaces(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_space: Option<bool> = None;
    for c in s.chars() {
        let sp = c.is_whitespace();
        if in_space != Some(sp) && !buf.is_empty() {
            out.push(std::mem::take(&mut buf));
        }
        in_space = Some(sp);
        buf.push(c);
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Height of the transcript once wrapped to `width`.
pub fn height(wrapped: &[Line<'static>]) -> u16 {
    wrapped.len().min(u16::MAX as usize) as u16
}

pub fn draw(f: &mut Frame, area: Rect, wrapped: &[Line<'static>], scroll: u16, theme: &Theme) {
    // Already wrapped, so no `Wrap` here — applying it twice would re-break lines
    // the scroll maths has already counted.
    let mut p = Paragraph::new(wrapped.to_vec()).scroll((scroll, 0));
    if let Some(bg) = theme.bg {
        p = p.style(ratatui::style::Style::default().bg(bg));
    }
    f.render_widget(p, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::block::{Body, Level};

    fn texts(out: &[Line<'static>]) -> Vec<String> {
        out.iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_new_turn_gets_a_rule_and_the_first_one_does_not() {
        let t = Theme::tokyonight();
        let out = texts(&lines(&[Block::User("a".into()), Block::User("b".into())], &t, false, 80));
        assert_eq!(out[0], "❯ a", "no leading separator");
        assert!(out[2].starts_with('─'), "second turn not divided: {out:?}");
        assert_eq!(out.last().unwrap(), "❯ b");
    }

    #[test]
    fn non_user_blocks_are_separated_by_a_blank_line_only() {
        let t = Theme::tokyonight();
        let out = texts(&lines(&[Block::Assistant("a".into()), Block::Assistant("b".into())], &t, false, 80));
        assert_eq!(out, vec!["a", "", "b"], "a rule here would be noise");
    }

    #[test]
    fn an_empty_transcript_has_no_lines() {
        assert!(lines(&[], &Theme::tokyonight(), false, 80).is_empty());
    }

    #[test]
    fn a_block_that_renders_nothing_adds_no_separator() {
        let t = Theme::tokyonight();
        // An assistant block with no text yet must not push a blank line.
        let out = lines(&[Block::Assistant(String::new()), Block::Notice { text: "x".into(), level: Level::Info }], &t, false, 80);
        let text: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text, vec!["x"]);
    }

    fn plain(s: &str) -> Vec<Line<'static>> {
        vec![Line::from(Span::raw(s.to_string()))]
    }

    fn rendered(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn height_accounts_for_wrapping() {
        let t = Theme::tokyonight();
        let out = lines(&[Block::Assistant("parola ".repeat(10))], &t, false, 80);
        assert!(height(&wrap(&out, 10)) > height(&wrap(&out, 200)));
    }

    #[test]
    fn zero_width_does_not_panic() {
        let t = Theme::tokyonight();
        let out = lines(&[Block::Tool {
            title: "t".into(),
            body: Some("b".into()),
            kind: Body::Plain,
            failed: false,
            started: None,
            took: None,
        }], &t, false, 80);
        assert_eq!(height(&wrap(&out, 0)), 0);
    }

    #[test]
    fn wrapping_breaks_on_word_boundaries() {
        assert_eq!(rendered(&wrap(&plain("alfa beta gamma"), 10)), vec!["alfa beta", "gamma"]);
    }

    #[test]
    fn a_word_longer_than_the_viewport_is_hard_split() {
        // A long path must not overflow the pane.
        let out = rendered(&wrap(&plain("abcdefghijkl"), 5));
        assert_eq!(out, vec!["abcde", "fghij", "kl"]);
    }

    #[test]
    fn short_lines_are_left_alone() {
        assert_eq!(rendered(&wrap(&plain("corto"), 40)), vec!["corto"]);
    }

    #[test]
    fn blank_lines_survive_wrapping() {
        // Block separators are blank lines; losing them would collapse the layout.
        let src = vec![Line::from(""), Line::from(Span::raw("x".to_string()))];
        assert_eq!(rendered(&wrap(&src, 20)), vec!["", "x"]);
    }

    #[test]
    fn wrapping_preserves_span_styles_across_a_break() {
        let src = vec![Line::from(vec![
            Span::styled("aaaa ".to_string(), ratatui::style::Style::default().fg(ratatui::style::Color::Red)),
            Span::styled("bbbb".to_string(), ratatui::style::Style::default().fg(ratatui::style::Color::Blue)),
        ])];
        let out = wrap(&src, 6);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].spans[0].style.fg, Some(ratatui::style::Color::Red));
        assert_eq!(out[1].spans[0].style.fg, Some(ratatui::style::Color::Blue));
    }

    #[test]
    fn indentation_is_kept_but_not_re_emitted_after_a_break() {
        // Tool bodies are indented; that must survive...
        assert_eq!(rendered(&wrap(&plain("  indented"), 40)), vec!["  indented"]);
        // ...without leaving a dangling space at the start of a continuation.
        let out = rendered(&wrap(&plain("alfa beta"), 5));
        assert_eq!(out, vec!["alfa", "beta"]);
    }

    #[test]
    fn indentation_survives_on_every_line_not_just_the_first() {
        // Regression: leading whitespace was skipped whenever anything had already
        // been emitted, so every indented line in a real transcript lost its
        // indent — visible only once there was more than one block on screen.
        let src = vec![
            Line::from(Span::raw("titolo".to_string())),
            Line::from(Span::raw("  indentata".to_string())),
            Line::from(Span::raw("  anche questa".to_string())),
        ];
        assert_eq!(
            rendered(&wrap(&src, 40)),
            vec!["titolo", "  indentata", "  anche questa"]
        );
    }

    #[test]
    fn a_blank_line_between_indented_ones_does_not_reset_the_indent() {
        let src = vec![
            Line::from(Span::raw("x".to_string())),
            Line::from(""),
            Line::from(Span::raw("    profonda".to_string())),
        ];
        assert_eq!(rendered(&wrap(&src, 40)), vec!["x", "", "    profonda"]);
    }
}
