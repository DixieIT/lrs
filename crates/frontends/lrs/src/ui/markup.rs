//! Rich console markup, as plugins write it.
//!
//! Plugin widgets are strings like `Pursuing goal ([bold cyan]…[/bold cyan] ship it)`
//! — the format Textual and Rich take. Rendering them raw would put literal square
//! brackets on screen, so the tags are parsed into ratatui spans here.
//!
//! Deliberately partial: colours, `bold`/`dim`/`italic`/`underline`, `#rrggbb`, and
//! the bare `[/]` close. Anything unrecognised is left as literal text rather than
//! swallowed — a plugin writing `[TODO]` in a widget should see `[TODO]`, not a
//! blank space where a tag was assumed.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;

/// Parse `markup` into styled spans. `base` is the style tags modify.
pub fn line(markup: &str, base: Style, theme: &Theme) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    // A stack, so nested tags close back to what they interrupted.
    let mut stack: Vec<Style> = vec![base];
    let mut text = String::new();
    let mut rest = markup;

    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else { break };
        let tag = &after[..close];

        match parse_tag(tag, *stack.last().unwrap(), theme) {
            Tag::Open(style) => {
                push(&mut spans, &mut text, *stack.last().unwrap());
                text.push_str(&rest[..open]);
                push(&mut spans, &mut text, *stack.last().unwrap());
                stack.push(style);
            }
            Tag::Close => {
                text.push_str(&rest[..open]);
                push(&mut spans, &mut text, *stack.last().unwrap());
                if stack.len() > 1 {
                    stack.pop();
                }
            }
            // Not a tag we know: it is the author's own square brackets.
            Tag::Literal => {
                text.push_str(&rest[..open + close + 2]);
                rest = &rest[open + close + 2..];
                continue;
            }
        }
        rest = &rest[open + close + 2..];
    }
    text.push_str(rest);
    push(&mut spans, &mut text, *stack.last().unwrap());
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    Line::from(spans)
}

/// Markup with every tag removed — for widths, and for anywhere styling can't go.
pub fn plain(markup: &str) -> String {
    line(markup, Style::default(), &Theme::tokyonight())
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

fn push(spans: &mut Vec<Span<'static>>, text: &mut String, style: Style) {
    if !text.is_empty() {
        spans.push(Span::styled(std::mem::take(text), style));
    }
}

enum Tag {
    Open(Style),
    Close,
    Literal,
}

fn parse_tag(tag: &str, current: Style, theme: &Theme) -> Tag {
    let tag = tag.trim();
    if tag.is_empty() {
        return Tag::Literal;
    }
    if let Some(body) = tag.strip_prefix('/') {
        // `[/]` and `[/bold green]` both just close the innermost tag: matching
        // them by name would mean tracking which tag opened what, for no gain.
        return if body.is_empty() || body.split_whitespace().all(is_word) {
            Tag::Close
        } else {
            Tag::Literal
        };
    }
    let mut style = current;
    let mut any = false;
    for word in tag.split_whitespace() {
        match word.to_ascii_lowercase().as_str() {
            "bold" => style = style.add_modifier(Modifier::BOLD),
            "dim" => style = style.add_modifier(Modifier::DIM),
            "italic" => style = style.add_modifier(Modifier::ITALIC),
            "underline" => style = style.add_modifier(Modifier::UNDERLINED),
            "reverse" => style = style.add_modifier(Modifier::REVERSED),
            "strike" => style = style.add_modifier(Modifier::CROSSED_OUT),
            other => match colour(other, theme) {
                Some(c) => style = style.fg(c),
                None => return Tag::Literal,
            },
        }
        any = true;
    }
    if any { Tag::Open(style) } else { Tag::Literal }
}

/// Is this a plausible tag word? Used only to tell `[/bold]` from `[/path/to/x]`.
fn is_word(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '#' || c == '_')
}

fn colour(name: &str, theme: &Theme) -> Option<Color> {
    if let Some(hex) = name.strip_prefix('#') {
        if hex.len() == 6 {
            let n = u32::from_str_radix(hex, 16).ok()?;
            return Some(Color::Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8));
        }
        return None;
    }
    // Route the common names through the theme so a plugin's "green" is the same
    // green as the rest of the UI rather than the terminal's raw ANSI one.
    Some(match name {
        "green" => theme.ok,
        "yellow" => theme.warn,
        "red" => theme.err,
        "cyan" => theme.accent,
        "blue" => theme.accent,
        "magenta" | "purple" => theme.thinking,
        "white" => theme.fg,
        "black" => Color::Black,
        "grey" | "gray" | "bright_black" => theme.muted,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(l: &Line<'static>) -> Vec<String> {
        l.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn plain_text_comes_through_untouched() {
        assert_eq!(plain("nessun tag qui"), "nessun tag qui");
    }

    #[test]
    fn tags_are_stripped_from_the_text_and_applied_as_style() {
        let t = Theme::tokyonight();
        let l = line("Goal ([bold green]✓[/bold green] fatto)", Style::default(), &t);
        assert_eq!(texts(&l).concat(), "Goal (✓ fatto)");
        let tick = l.spans.iter().find(|s| s.content == "✓").expect("no ✓ span");
        assert!(tick.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(tick.style.fg, Some(t.ok));
    }

    #[test]
    fn closing_restores_what_was_there_before() {
        let t = Theme::tokyonight();
        let l = line("prima[bold]dentro[/bold]dopo", Style::default(), &t);
        let after = l.spans.iter().find(|s| s.content == "dopo").unwrap();
        assert!(!after.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn the_bare_close_tag_works_too() {
        let t = Theme::tokyonight();
        let l = line("[bold]x[/]y", Style::default(), &t);
        assert_eq!(texts(&l).concat(), "xy");
        assert!(!l.spans.iter().find(|s| s.content == "y").unwrap().style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_hex_colour_is_honoured() {
        let l = line("[#ff8800]arancio[/]", Style::default(), &Theme::tokyonight());
        assert_eq!(l.spans[0].style.fg, Some(Color::Rgb(0xff, 0x88, 0x00)));
    }

    #[test]
    fn brackets_that_are_not_tags_stay_on_screen() {
        // A widget saying "[TODO] 3 left" must not lose the word.
        assert_eq!(plain("[TODO] 3 rimasti"), "[TODO] 3 rimasti");
        assert_eq!(plain("array[0] = 1"), "array[0] = 1");
        assert_eq!(plain("vedi [1] e [2]"), "vedi [1] e [2]");
    }

    #[test]
    fn an_unclosed_bracket_is_not_swallowed() {
        assert_eq!(plain("prezzo [molto alto"), "prezzo [molto alto");
    }

    #[test]
    fn an_unclosed_tag_does_not_leak_into_a_panic() {
        // Plugins get this wrong; a missing [/bold] must simply style to the end.
        let l = line("[bold]per sempre", Style::default(), &Theme::tokyonight());
        assert_eq!(texts(&l).concat(), "per sempre");
    }

    #[test]
    fn too_many_closes_do_not_underflow_the_stack() {
        assert_eq!(plain("[/][/][/]x"), "x");
    }

    #[test]
    fn nesting_unwinds_one_level_at_a_time() {
        let t = Theme::tokyonight();
        let l = line("[bold][red]a[/red]b[/bold]", Style::default(), &t);
        let a = l.spans.iter().find(|s| s.content == "a").unwrap();
        let b = l.spans.iter().find(|s| s.content == "b").unwrap();
        assert_eq!(a.style.fg, Some(t.err));
        assert!(a.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(b.style.fg, None, "the red should have closed");
        assert!(b.style.add_modifier.contains(Modifier::BOLD), "the bold should not have");
    }

    #[test]
    fn the_real_goal_widget_renders() {
        let t = Theme::tokyonight();
        let m = "Pursuing goal ([bold cyan]…[/bold cyan] spedire la release)";
        assert_eq!(plain(m), "Pursuing goal (… spedire la release)");
        let l = line(m, Style::default(), &t);
        assert_eq!(l.spans.iter().find(|s| s.content == "…").unwrap().style.fg, Some(t.accent));
    }

    #[test]
    fn the_base_style_applies_where_no_tag_does() {
        let base = Style::default().fg(Color::Rgb(1, 2, 3));
        let l = line("nudo", base, &Theme::tokyonight());
        assert_eq!(l.spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
    }
}
