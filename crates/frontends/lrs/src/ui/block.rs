//! The transcript's unit of content, and how each kind draws itself.
//!
//! One enum rather than a widget hierarchy: the chat is an append-only list that
//! only ever grows at the tail and re-renders wholesale, so there is nothing for
//! object identity to buy us.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::markdown;
use crate::theme::Theme;

/// Lines of a collapsed tool body shown before the "… N more" marker.
const PREVIEW_LINES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Body {
    /// Colour `+`/`-` prefixed lines. Used for apply_patch, whose arguments are
    /// literally a patch.
    Diff,
    /// Verbatim, in the accent colour — file contents, command output.
    Code,
    Plain,
}

#[derive(Debug, Clone)]
pub enum Block {
    User(String),
    /// Markdown source. Grows in place as tokens stream in.
    Assistant(String),
    /// Reasoning text. Collapsed to its tail while streaming.
    Thinking(String),
    Tool {
        title: String,
        /// `None` while the call is still running.
        body: Option<String>,
        kind: Body,
        /// The call failed: marked, not shouted, so the output stays the focus.
        failed: bool,
        /// When the call started, for the live counter. `None` for calls
        /// replayed from a past session, which have no timing.
        started: Option<std::time::Instant>,
        /// How long the finished call took.
        took: Option<std::time::Duration>,
    },
    Notice {
        text: String,
        level: Level,
    },
    /// The opening capability report. Its own variant rather than a Notice
    /// because it is the one thing on screen that needs a real frame: a flat list
    /// of four comma-separated runs gives the eye nothing to tell sections apart.
    Banner {
        title: String,
        sections: Vec<(String, Vec<String>)>,
    },
    /// Shows the *tail* rather than the head: a sub-agent's report can run to
    /// hundreds of lines, and while it works you want to see where it has got to,
    /// not how it opened. ^o expands.
    Agent {
        /// Correlates the child's events; not displayed.
        run_id: String,
        label: String,
        text: String,
        done: bool,
    },
}

impl Block {
    /// Append streamed text to a block that grows (assistant / thinking).
    /// Returns false if this kind doesn't accumulate, so the caller knows to
    /// start a new block instead.
    pub fn push_text(&mut self, chunk: &str) -> bool {
        match self {
            Block::Assistant(s) | Block::Thinking(s) | Block::Agent { text: s, .. } => {
                s.push_str(chunk);
                true
            }
            _ => false,
        }
    }

    /// `expand_all` is a view-wide toggle (^o) rather than per-block state: with no
    /// cursor or mouse selection in the UI, "which block" has no answer to give.
    ///
    /// `width` is the viewport the lines will be laid out in. Only the framed
    /// banner needs it — a box has to know how wide to draw itself, and it must
    /// pre-wrap its own contents so that no line is ever re-broken afterwards,
    /// which would leave a continuation row hanging outside the frame.
    pub fn render(&self, theme: &Theme, expand_all: bool, width: u16) -> Vec<Line<'static>> {
        match self {
            Block::User(text) => render_user(text, theme),
            Block::Assistant(src) => markdown::render(src, theme),
            Block::Thinking(text) => render_thinking(text, theme),
            Block::Tool { title, body, kind, failed, started, took } => {
                // Running: time so far, redrawn every frame. Done: the total.
                let time = match (body, took, started) {
                    (Some(_), Some(took), _) => Some(*took),
                    (None, _, Some(started)) => Some(started.elapsed()),
                    _ => None,
                };
                render_tool(title, body.as_deref(), *kind, *failed, time, expand_all, theme)
            }
            Block::Notice { text, level } => render_notice(text, *level, theme),
            Block::Banner { title, sections } => render_banner(title, sections, theme, width),
            Block::Agent { label, text, done, .. } => {
                render_agent(label, text, *done, expand_all, theme, width)
            }
        }
    }
}

fn render_user(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    text.lines()
        .enumerate()
        .map(|(i, l)| {
            // Marker on the first line only, so a pasted multi-line prompt reads
            // as one message instead of a list.
            let marker = if i == 0 { "❯ " } else { "  " };
            Line::from(vec![
                Span::styled(marker, Style::default().fg(theme.user).add_modifier(Modifier::BOLD)),
                Span::styled(l.to_string(), Style::default().fg(theme.user)),
            ])
        })
        .collect()
}

fn render_thinking(text: &str, theme: &Theme) -> Vec<Line<'static>> {
    let style = Style::default().fg(theme.thinking).add_modifier(Modifier::ITALIC);
    let mut out = vec![Line::from(Span::styled(
        "thinking".to_string(),
        Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC),
    ))];
    for l in text.lines() {
        out.push(Line::from(Span::styled(format!("  {l}"), style)));
    }
    out
}

fn render_notice(text: &str, level: Level, theme: &Theme) -> Vec<Line<'static>> {
    let color = match level {
        Level::Info => theme.muted,
        Level::Warn => theme.warn,
        Level::Error => theme.err,
    };
    text.lines()
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(color))))
        .collect()
}

fn render_tool(
    title: &str,
    body: Option<&str>,
    kind: Body,
    failed: bool,
    time: Option<std::time::Duration>,
    expanded: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let running = body.is_none();
    let (marker, colour) = if running {
        ("▸", theme.muted)
    } else if failed {
        ("✗", theme.err)
    } else {
        ("▪", theme.tool)
    };
    let mut out = vec![Line::from(vec![
        Span::styled(format!("{marker} "), Style::default().fg(colour)),
        Span::styled(
            title.to_string(),
            Style::default().fg(theme.tool).add_modifier(Modifier::BOLD),
        ),
    ])];
    if let Some(time) = time {
        out[0].spans.push(Span::styled(format!("  {}", elapsed(time)), Style::default().fg(theme.muted)));
    }

    let Some(body) = body else {
        return out;
    };
    let body = body.trim_end();
    if body.is_empty() {
        return out;
    }

    // A gutter down the left ties the output to the title above it — without one,
    // a tool body and a plain assistant paragraph look the same.
    let gutter = || Span::styled(GUTTER, Style::default().fg(theme.border));

    let all: Vec<&str> = body.lines().collect();
    let shown = if expanded { all.len() } else { all.len().min(PREVIEW_LINES) };
    // Collapsed, a line is also cut by length: one line of JSON can be
    // thousands of characters, and counted as one line it would fill the screen.
    let mut hidden_chars = 0;
    for (i, l) in all[..shown].iter().enumerate() {
        let text = match l.char_indices().nth(PREVIEW_LINE_CHARS) {
            Some((cut, _)) if !expanded => {
                hidden_chars += l[cut..].chars().count();
                format!("{}…", &l[..cut])
            }
            _ => (*l).to_string(),
        };
        // A failure's first line is its message: red. What follows (a stack,
        // the output before an exit code) stays readable in its usual colour.
        let style = if failed && i == 0 {
            Style::default().fg(theme.err)
        } else {
            body_style(l, kind, theme)
        };
        out.push(Line::from(vec![gutter(), Span::styled(text, style)]));
    }
    let hidden_lines = all.len() - shown;
    let footer = match (hidden_lines, hidden_chars) {
        (0, 0) => None,
        (0, c) => Some(format!("… {c} more characters (^o)")),
        (n, 0) => Some(format!("… {n} more lines (^o)")),
        (n, c) => Some(format!("… {n} more lines, {c} more characters (^o)")),
    };
    if let Some(footer) = footer {
        out.push(Line::from(vec![
            gutter(),
            Span::styled(footer, Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC)),
        ]));
    }
    out
}

/// Characters a collapsed tool-output line keeps: about two rows of a normal
/// terminal, enough to recognise what the output is.
const PREVIEW_LINE_CHARS: usize = 160;

const GUTTER: &str = "│ ";

/// `0.4s`, `12.3s`, `1m 05s`: tenths while short, then whole seconds.
pub fn elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let whole = d.as_secs();
        format!("{}m {:02}s", whole / 60, whole % 60)
    }
}

/// Tail lines kept for a sub-agent while collapsed. Larger than a tool's preview:
/// a sub-agent's report is the substance of the turn, not an aside.
const AGENT_TAIL_LINES: usize = 10;

fn render_agent(
    label: &str,
    text: &str,
    done: bool,
    expanded: bool,
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let edge = Style::default().fg(theme.border);
    let marker = if done { "▪" } else { "▸" };
    let mut out = vec![Line::from(vec![
        Span::styled(
            format!("{marker} "),
            Style::default().fg(if done { theme.tool } else { theme.muted }),
        ),
        Span::styled(
            label.to_string(),
            Style::default().fg(theme.tool).add_modifier(Modifier::BOLD),
        ),
    ])];

    let body = text.trim_end();
    if body.is_empty() {
        return out;
    }

    // Markdown, like any other agent's output — a sub-agent's report is prose with
    // headings, lists and paths, and showing it raw was the only place in the UI
    // where `**bold**` leaked to the screen.
    //
    // Wrapped *here*, at the width left over after the gutter, because the outer
    // pass runs after this one: a line broken later would put its continuation
    // outside the gutter, which is what made a long paragraph look like it had
    // escaped the block.
    let gutter_w = GUTTER.chars().count() as u16;
    let inner = width.saturating_sub(gutter_w).max(1);
    let rendered = super::chat::wrap(&markdown::render(body, theme), inner);

    // Keep the END, not the beginning: while it streams you want the live edge,
    // and once finished the conclusion is what matters.
    let skipped = if expanded {
        0
    } else {
        rendered.len().saturating_sub(AGENT_TAIL_LINES)
    };
    if skipped > 0 {
        out.push(Line::from(vec![
            Span::styled(GUTTER, edge),
            Span::styled(
                format!("… {skipped} earlier lines (^o)"),
                Style::default().fg(theme.muted).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }
    for line in &rendered[skipped..] {
        let mut spans = vec![Span::styled(GUTTER, edge)];
        spans.extend(line.spans.iter().cloned());
        out.push(Line::from(spans));
    }

    // Close the block. Without it the sub-agent's last paragraph and the parent's
    // first run together, and there is no way to tell who said what.
    out.push(Line::from(Span::styled(
        format!("╰{}", "─".repeat(12)),
        edge,
    )));
    out
}

/// Widest the banner box is allowed to get. Past this a full-width frame stops
/// reading as a box and starts reading as two unrelated horizontal lines.
const BANNER_MAX: usize = 78;

fn render_banner(
    title: &str,
    sections: &[(String, Vec<String>)],
    theme: &Theme,
    width: u16,
) -> Vec<Line<'static>> {
    let w = (width as usize).clamp(20, BANNER_MAX);
    let edge = Style::default().fg(theme.border);
    // Room for "│ " on the left and " │" on the right.
    let inner = w.saturating_sub(4);

    let mut out = Vec::new();

    // ╭─ title ─────╮
    let head = format!("╭─ {title} ");
    let head_len = head.chars().count();
    out.push(Line::from(vec![
        Span::styled(head, edge),
        Span::styled("─".repeat(w.saturating_sub(head_len + 1)), edge),
        Span::styled("╮", edge),
    ]));

    let mut row = |content: Vec<Span<'static>>| {
        let used: usize = content.iter().map(|s| s.content.chars().count()).sum();
        let mut spans = vec![Span::styled("│ ", edge)];
        spans.extend(content);
        spans.push(Span::styled(
            format!("{}│", " ".repeat(inner.saturating_sub(used) + 1)),
            edge,
        ));
        out.push(Line::from(spans));
    };

    for (i, (label, items)) in sections.iter().enumerate() {
        if items.is_empty() {
            continue;
        }
        if i > 0 {
            row(vec![]);
        }
        row(vec![Span::styled(
            label.clone(),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        )]);
        // Pre-wrapped here, at the box's inner width, so the frame stays square.
        for chunk in wrap_words(&items.join(", "), inner.saturating_sub(2)) {
            row(vec![Span::styled(
                format!("  {chunk}"),
                Style::default().fg(theme.fg),
            )]);
        }
    }

    out.push(Line::from(vec![
        Span::styled(format!("╰{}╯", "─".repeat(w.saturating_sub(2))), edge),
    ]));
    out
}

/// Greedy word wrap to `width`. Long single words are left over-long rather than
/// split — inside the banner they are tool names, and a broken name is worse than
/// a ragged edge.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let extra = if cur.is_empty() { 0 } else { 1 };
        if !cur.is_empty() && cur.chars().count() + extra + word.chars().count() > width {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn body_style(line: &str, kind: Body, theme: &Theme) -> Style {
    match kind {
        Body::Diff if line.starts_with('+') => Style::default().fg(theme.add),
        Body::Diff if line.starts_with('-') => Style::default().fg(theme.del),
        Body::Diff => Style::default().fg(theme.muted),
        Body::Code => Style::default().fg(theme.accent),
        Body::Plain => Style::default().fg(theme.fg),
    }
}

/// A one-line label for a tool call, from its name and arguments — the same idea
/// as the Textual TUI's `_tool_title`: show the argument that identifies the call
/// (a path, a command), not the whole JSON.
pub fn tool_title(name: &str, args: &serde_json::Value) -> String {
    let pick = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);

    // Spawning a sub-agent is not the same kind of event as calling a tool, and
    // "subagent explore" reads as a verb applied to a noun. Name the thing that
    // is about to run instead.
    if matches!(name, "subagent" | "subagent_fork") {
        return match pick("agent") {
            Some(a) => format!("Agent({a})"),
            None => "Agent".to_string(),
        };
    }

    let detail = match name {
        "read_file" | "write_file" | "apply_patch" => pick("path"),
        "bash" | "shell" => pick("command"),
        "grep" | "search" | "glob" => pick("pattern"),
        _ => None,
    };
    match detail {
        Some(d) => format!("{name}  {}", one_line(&d, 70)),
        None => name.to_string(),
    }
}

/// Collapse to a single line and ellipsise — a title that wraps stops being a title.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = flat.chars().count();
    if n <= max {
        flat
    } else {
        let head: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// How a tool's output should be coloured.
pub fn body_kind(name: &str) -> Body {
    match name {
        "apply_patch" => Body::Diff,
        "read_file" | "write_file" => Body::Code,
        _ => Body::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
            .collect()
    }

    #[test]
    fn title_shows_the_identifying_argument() {
        assert_eq!(tool_title("read_file", &json!({"path": "src/a.rs"})), "read_file  src/a.rs");
        assert_eq!(tool_title("bash", &json!({"command": "ls -la"})), "bash  ls -la");
    }

    #[test]
    fn title_without_a_known_argument_is_just_the_name() {
        assert_eq!(tool_title("todo_write", &json!({"items": []})), "todo_write");
        assert_eq!(tool_title("read_file", &json!({})), "read_file");
    }

    #[test]
    fn long_titles_are_ellipsised_to_one_line() {
        let t = tool_title("bash", &json!({"command": "x".repeat(200)}));
        assert!(t.chars().count() < 90, "still too long: {}", t.chars().count());
        assert!(t.ends_with('…'));
        assert!(!t.contains('\n'));
    }

    #[test]
    fn multiline_command_becomes_one_line() {
        let t = tool_title("bash", &json!({"command": "a\nb\nc"}));
        assert_eq!(t, "bash  a b c");
    }

    #[test]
    fn a_running_tool_shows_only_its_title() {
        let b = Block::Tool {
            title: "read_file  a.rs".into(),
            body: None,
            kind: Body::Code,
            failed: false,
            started: None,
            took: None,
        };
        assert_eq!(text_of(&b.render(&Theme::tokyonight(), false, 80)).len(), 1);
    }

    #[test]
    fn collapsed_body_is_truncated_and_says_so() {
        let body = (0..20).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let b = Block::Tool {
            title: "t".into(),
            body: Some(body),
            kind: Body::Plain,
            failed: false,
            started: None,
            took: None,
        };
        let lines = text_of(&b.render(&Theme::tokyonight(), false, 80));
        assert_eq!(lines.len(), 1 + PREVIEW_LINES + 1);
        assert!(lines.last().unwrap().contains("14 more lines"));
    }

    #[test]
    fn a_failed_call_is_marked_with_the_error_colour_only() {
        let theme = Theme::tokyonight();
        let b = Block::Tool {
            title: "run_js".into(),
            body: Some("ReferenceError: require is not defined\n    at <anonymous> (eval_script:3:12)".into()),
            kind: Body::Plain, failed: true, started: None, took: None,
        };
        let lines = b.render(&theme, false, 80);
        assert_eq!(lines[0].spans[0].content, "✗ ");
        assert_eq!(lines[0].spans[0].style.fg, Some(theme.err));
        assert_eq!(lines[0].spans[1].style.fg, Some(theme.tool), "the title keeps its colour");
        assert!(text_of(&lines)[1].contains("ReferenceError"));
        assert_eq!(lines[1].spans[1].style.fg, Some(theme.err), "the message is red");
        assert_ne!(lines[2].spans[1].style.fg, Some(theme.err), "the stack below is not");
    }

    #[test]
    fn a_successful_call_has_no_red_line() {
        let theme = Theme::tokyonight();
        let b = Block::Tool {
            title: "bash".into(), body: Some("Error in a file name is just output".into()),
            kind: Body::Plain, failed: false, started: None, took: None,
        };
        let lines = b.render(&theme, false, 80);
        assert_ne!(lines[1].spans[1].style.fg, Some(theme.err));
    }

    #[test]
    fn a_running_call_counts_up_and_a_finished_one_shows_its_duration() {
        let theme = Theme::tokyonight();
        let running = Block::Tool {
            title: "bash".into(), body: None, kind: Body::Plain, failed: false,
            started: Some(std::time::Instant::now() - std::time::Duration::from_millis(3200)),
            took: None,
        };
        let header = text_of(&running.render(&theme, false, 80))[0].clone();
        assert!(header.starts_with("▸ bash  3."), "{header}");
        let done = Block::Tool {
            title: "bash".into(), body: Some("ok".into()), kind: Body::Plain, failed: false,
            started: None, took: Some(std::time::Duration::from_millis(4100)),
        };
        assert_eq!(text_of(&done.render(&theme, false, 80))[0], "▪ bash  4.1s");
        let replayed = Block::Tool {
            title: "bash".into(), body: Some("ok".into()), kind: Body::Plain, failed: false,
            started: None, took: None,
        };
        assert_eq!(text_of(&replayed.render(&theme, false, 80))[0], "▪ bash", "no timing, no counter");
    }

    #[test]
    fn elapsed_reads_in_tenths_then_minutes() {
        assert_eq!(elapsed(std::time::Duration::from_millis(400)), "0.4s");
        assert_eq!(elapsed(std::time::Duration::from_millis(12_340)), "12.3s");
        assert_eq!(elapsed(std::time::Duration::from_secs(65)), "1m 05s");
    }

    #[test]
    fn one_huge_line_is_cut_when_collapsed_and_whole_when_expanded() {
        let line = "x".repeat(PREVIEW_LINE_CHARS + 5000);
        let b = Block::Tool { title: "run_js".into(), body: Some(line.clone()), kind: Body::Plain, failed: false, started: None, took: None };
        let collapsed = text_of(&b.render(&Theme::tokyonight(), false, 80));
        assert_eq!(collapsed.len(), 3, "title, the cut line, the footer: {collapsed:?}");
        assert!(collapsed[1].ends_with('…'));
        assert!(collapsed[2].contains("5000 more characters (^o)"), "{collapsed:?}");
        let expanded = text_of(&b.render(&Theme::tokyonight(), true, 80));
        assert!(expanded[1].ends_with(&line), "nothing is lost when expanded");
    }

    #[test]
    fn long_lines_and_many_lines_are_both_reported() {
        let body = std::iter::once("y".repeat(PREVIEW_LINE_CHARS + 10))
            .chain((0..9).map(|i| i.to_string())).collect::<Vec<_>>().join("\n");
        let b = Block::Tool { title: "t".into(), body: Some(body), kind: Body::Plain, failed: false, started: None, took: None };
        let lines = text_of(&b.render(&Theme::tokyonight(), false, 80));
        assert!(lines.last().unwrap().contains("4 more lines, 10 more characters (^o)"), "{lines:?}");
    }

    #[test]
    fn expand_all_overrides_the_per_block_flag() {
        let body = (0..20).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let b = Block::Tool {
            title: "t".into(),
            body: Some(body),
            kind: Body::Plain,
            failed: false,
            started: None,
            took: None,
        };
        assert_eq!(text_of(&b.render(&Theme::tokyonight(), true, 80)).len(), 21);
    }

    #[test]
    fn streaming_appends_only_to_growable_blocks() {
        let mut a = Block::Assistant("ci".into());
        assert!(a.push_text("ao"));
        assert!(matches!(&a, Block::Assistant(s) if s == "ciao"));

        let mut t = Block::Tool { title: "t".into(), body: None, kind: Body::Plain, failed: false, started: None, took: None };
        assert!(!t.push_text("x"), "a tool block must not absorb streamed tokens");
    }

    #[test]
    fn multiline_user_message_marks_only_the_first_line() {
        let lines = text_of(&Block::User("uno\ndue".into()).render(&Theme::tokyonight(), false, 80));
        assert_eq!(lines, vec!["❯ uno", "  due"]);
    }
}
