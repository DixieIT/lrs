//! A deliberately small markdown renderer.
//!
//! Scope is what an agent actually emits: fenced code, headings, bullets, numbered
//! lists, blockquotes, and inline `code` / **bold** / *italic*. Anything else falls
//! through as plain text, which is the right failure mode for a chat log — never
//! swallow content just because it wasn't understood.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Render markdown source into styled lines. Wrapping is left to the widget, which
/// knows the viewport width.
pub fn render(src: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut fence_lang = String::new();

    for raw in src.lines() {
        let trimmed = raw.trim_start();

        if let Some(rest) = trimmed.strip_prefix("```") {
            if in_fence {
                in_fence = false;
                fence_lang.clear();
            } else {
                in_fence = true;
                fence_lang = rest.trim().to_string();
                // The language tag is a label, not content — show it dimmed so the
                // block's type is visible without stealing attention.
                if !fence_lang.is_empty() {
                    out.push(Line::from(Span::styled(
                        format!("  {fence_lang}"),
                        Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC),
                    )));
                }
            }
            continue;
        }

        if in_fence {
            out.push(Line::from(Span::styled(
                format!("  {raw}"),
                Style::default().fg(theme.accent),
            )));
            continue;
        }

        if trimmed.is_empty() {
            out.push(Line::from(""));
            continue;
        }

        // Headings — rendered by weight, not by echoing the '#' characters.
        if let Some(level) = heading_level(trimmed) {
            let text = trimmed[level..].trim_start();
            let style = Style::default()
                .fg(if level == 1 { theme.accent } else { theme.fg })
                .add_modifier(Modifier::BOLD);
            out.push(Line::from(Span::styled(text.to_string(), style)));
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled("▏ ", Style::default().fg(theme.border))];
            spans.extend(inline(rest, theme, Style::default().fg(theme.muted)));
            out.push(Line::from(spans));
            continue;
        }

        // Horizontal rule.
        if trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_') {
            out.push(Line::from(Span::styled(
                "─".repeat(24),
                Style::default().fg(theme.border),
            )));
            continue;
        }

        if let Some(rest) = bullet_body(trimmed) {
            let indent = raw.len() - trimmed.len();
            let mut spans = vec![Span::styled(
                format!("{}• ", " ".repeat(indent)),
                Style::default().fg(theme.accent),
            )];
            spans.extend(inline(rest, theme, Style::default().fg(theme.fg)));
            out.push(Line::from(spans));
            continue;
        }

        if let Some((marker, rest)) = numbered_body(trimmed) {
            let indent = raw.len() - trimmed.len();
            let mut spans = vec![Span::styled(
                format!("{}{marker} ", " ".repeat(indent)),
                Style::default().fg(theme.accent),
            )];
            spans.extend(inline(rest, theme, Style::default().fg(theme.fg)));
            out.push(Line::from(spans));
            continue;
        }

        out.push(Line::from(inline(raw, theme, Style::default().fg(theme.fg))));
    }
    out
}

fn heading_level(s: &str) -> Option<usize> {
    let hashes = s.chars().take_while(|c| *c == '#').count();
    // "#hashtag" is not a heading; a real one has a space after the run.
    if (1..=6).contains(&hashes) && s[hashes..].starts_with(' ') {
        Some(hashes)
    } else {
        None
    }
}

fn bullet_body(s: &str) -> Option<&str> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = s.strip_prefix(m) {
            return Some(rest);
        }
    }
    None
}

fn numbered_body(s: &str) -> Option<(&str, &str)> {
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = &s[digits..];
    let rest = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))?;
    Some((&s[..digits + 1], rest))
}

/// Inline spans: `code`, **bold**, *italic*. Unclosed markers stay literal.
fn inline(src: &str, theme: &Theme, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;

    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), base));
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        // Bold before italic: "**" must not be read as two italic markers.
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_close(&chars, i + 2, "**") {
                flush!();
                let text: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(text, base.add_modifier(Modifier::BOLD)));
                i = end + 2;
                continue;
            }
        }
        if c == '*' {
            if let Some(end) = find_close(&chars, i + 1, "*") {
                flush!();
                let text: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(text, base.add_modifier(Modifier::ITALIC)));
                i = end + 1;
                continue;
            }
        }
        if c == '`' {
            if let Some(end) = find_close(&chars, i + 1, "`") {
                flush!();
                let text: String = chars[i + 1..end].iter().collect();
                spans.push(Span::styled(text, Style::default().fg(theme.accent)));
                i = end + 1;
                continue;
            }
        }
        buf.push(c);
        i += 1;
    }
    flush!();
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

/// Index of the next `marker` at or after `from`, or None if it never closes.
fn find_close(chars: &[char], from: usize, marker: &str) -> Option<usize> {
    let m: Vec<char> = marker.chars().collect();
    let mut i = from;
    while i + m.len() <= chars.len() {
        if chars[i..i + m.len()] == m[..] {
            // An empty span (`` or **) is not emphasis — leave it literal.
            return if i == from { None } else { Some(i) };
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn headings_drop_their_hashes() {
        let t = Theme::tokyonight();
        assert_eq!(text_of(&render("## Titolo", &t)), vec!["Titolo"]);
    }

    #[test]
    fn a_hashtag_is_not_a_heading() {
        let t = Theme::tokyonight();
        assert_eq!(text_of(&render("#nope", &t)), vec!["#nope"]);
    }

    #[test]
    fn fenced_code_keeps_every_line_verbatim() {
        let t = Theme::tokyonight();
        let out = text_of(&render("```rust\nlet x = 1;\n```", &t));
        // the language label, then the code line — and no stray fence markers
        assert_eq!(out, vec!["  rust", "  let x = 1;"]);
    }

    #[test]
    fn markdown_inside_a_fence_is_not_interpreted() {
        let t = Theme::tokyonight();
        let out = text_of(&render("```\n# not a heading\n- not a bullet\n```", &t));
        assert_eq!(out, vec!["  # not a heading", "  - not a bullet"]);
    }

    #[test]
    fn unclosed_emphasis_stays_literal() {
        let t = Theme::tokyonight();
        assert_eq!(text_of(&render("2 * 3 = 6", &t)), vec!["2 * 3 = 6"]);
        assert_eq!(text_of(&render("a **bold start", &t)), vec!["a **bold start"]);
    }

    #[test]
    fn bold_wins_over_italic() {
        let t = Theme::tokyonight();
        let lines = render("**forte**", &t);
        assert_eq!(text_of(&lines), vec!["forte"]);
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn inline_code_is_extracted() {
        let t = Theme::tokyonight();
        let lines = render("usa `cargo build` ora", &t);
        assert_eq!(text_of(&lines), vec!["usa cargo build ora"]);
    }

    #[test]
    fn bullets_and_numbers_get_markers() {
        let t = Theme::tokyonight();
        assert_eq!(text_of(&render("- uno", &t)), vec!["• uno"]);
        assert_eq!(text_of(&render("1. uno", &t)), vec!["1. uno"]);
    }

    #[test]
    fn no_content_is_ever_dropped() {
        // The whole point of the fall-through: unsupported syntax still shows up.
        let t = Theme::tokyonight();
        let src = "| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(text_of(&render(src, &t)).len(), 3);
    }
}
