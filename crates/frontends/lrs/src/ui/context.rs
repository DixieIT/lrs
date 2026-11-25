//! The `/context` report, split into the parts of what the model receives —
//! system prompt, tools, messages by role — so the viewer can colour each with
//! the theme and draw how the estimate divides between them.
//!
//! The report is plain text from the backend; this module only reads its
//! structure (headings `── …`, entries `▸ …`, the `estimate` line).

use crate::theme::Theme;
use ratatui::style::Color;

/// Which part of the context a line belongs to. Decides its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Model, executor, estimate: about the context, not in it.
    Meta,
    System,
    Tools,
    /// Tool parameters as JSON Schema: detail under a tool.
    Schema,
    User,
    Assistant,
    /// A tool call the assistant made.
    Call,
    /// What a tool returned.
    Result,
    /// Headings of sections that mix parts (messages).
    Section,
}

impl Part {
    pub fn color(self, theme: &Theme) -> Color {
        match self {
            Part::Meta | Part::Schema | Part::Result => theme.muted,
            Part::System => theme.thinking,
            Part::Tools | Part::Call => theme.tool,
            Part::User => theme.user,
            Part::Assistant => theme.fg,
            Part::Section => theme.accent,
        }
    }
}

/// One row of the viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// Text in one part's colour; `heading` rows are bold.
    Text { text: String, part: Part, heading: bool },
    /// How the estimate divides between the parts, drawn as a bar.
    Usage(Vec<(Part, u64)>),
    /// Coloured pieces on one line: the bar's legend.
    Spans(Vec<(String, Part)>),
}

/// Rows for a `/context` report.
pub fn rows(lines: &[String]) -> Vec<Row> {
    let mut out = Vec::new();
    let mut section = Part::Meta;
    let mut entry = Part::Meta;
    let mut calls: Vec<String> = Vec::new();
    for line in lines {
        if let Some(title) = line.strip_prefix("── ") {
            section = if title.starts_with("system") {
                Part::System
            } else if title.starts_with("tools") {
                Part::Tools
            } else {
                Part::Section
            };
            entry = section;
            out.push(Row::Text { text: line.clone(), part: section, heading: true });
            continue;
        }
        if let Some(label) = line.strip_prefix("▸ ") {
            (entry, calls) = match section {
                Part::Tools => (Part::Tools, Vec::new()),
                _ => entry_of(label),
            };
            out.push(Row::Text { text: line.clone(), part: entry, heading: true });
            continue;
        }
        let part = match section {
            Part::Meta => Part::Meta,
            Part::Tools if line.trim_start().starts_with("parameters:") => {
                entry = Part::Schema;
                Part::Schema
            }
            Part::Tools => entry,
            _ if entry == Part::Assistant && calls.iter().any(|c| line.starts_with(&format!("  {c}("))) => Part::Call,
            _ => entry,
        };
        out.push(Row::Text { text: line.clone(), part, heading: false });
        if section == Part::Meta {
            if let Some(usage) = usage(line) {
                out.push(Row::Usage(usage.clone()));
                out.push(Row::Spans(legend(&usage)));
            }
        }
    }
    out
}

/// The part of a message entry from its label, and the calls it names.
fn entry_of(label: &str) -> (Part, Vec<String>) {
    let mut pieces = label.split(" · ");
    let part = match pieces.next().unwrap_or("") {
        "user" => Part::User,
        "assistant" => Part::Assistant,
        "tool" => Part::Result,
        "system" => Part::System,
        _ => Part::Assistant,
    };
    let calls = pieces
        .find_map(|p| p.strip_prefix("calls "))
        .map(|names| names.split(", ").map(str::to_string).collect())
        .unwrap_or_default();
    (part, calls)
}

/// `estimate   ~N tokens (system ~a, tools ~b, messages ~c)` → the three parts.
fn usage(line: &str) -> Option<Vec<(Part, u64)>> {
    let inner = line.strip_prefix("estimate")?.split_once('(')?.1.strip_suffix(')')?;
    let mut parts = Vec::new();
    for piece in inner.split(", ") {
        let (name, amount) = piece.split_once(" ~")?;
        let part = match name {
            "system" => Part::System,
            "tools" => Part::Tools,
            "messages" => Part::Assistant,
            _ => return None,
        };
        parts.push((part, amount.parse().ok()?));
    }
    Some(parts)
}

fn legend(usage: &[(Part, u64)]) -> Vec<(String, Part)> {
    let mut out = Vec::new();
    for (i, (part, tokens)) in usage.iter().enumerate() {
        let name = match part {
            Part::System => "system",
            Part::Tools => "tools",
            _ => "messages",
        };
        out.push((format!("{}■ ", if i == 0 { "" } else { "   " }), *part));
        out.push((format!("{name} ~{tokens}"), Part::Meta));
    }
    out
}

/// Cells of a `width`-wide bar for each part, in proportion. A part with any
/// tokens gets at least one cell, so a small system prompt stays visible.
pub fn bar_cells(usage: &[(Part, u64)], width: usize) -> Vec<(Part, usize)> {
    let total: u64 = usage.iter().map(|(_, t)| t).sum();
    if total == 0 || width == 0 {
        return vec![];
    }
    let mut cells: Vec<(Part, usize)> = usage.iter()
        .map(|(p, t)| (*p, if *t == 0 { 0 } else { ((*t as f64 / total as f64) * width as f64).round().max(1.0) as usize }))
        .collect();
    // Rounding can overshoot: take the excess from the widest part.
    while cells.iter().map(|(_, c)| c).sum::<usize>() > width {
        if let Some(widest) = cells.iter_mut().max_by_key(|(_, c)| *c) {
            widest.1 -= 1;
        }
    }
    cells
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> Vec<String> {
        [
            "model      m",
            "executor   ptc",
            "estimate   ~400 tokens (system ~10, tools ~300, messages ~90)",
            "",
            "── system prompt",
            "You are Lele",
            "",
            "── tools offered to the model (1)",
            "▸ run_js",
            "  Run a program.",
            "  parameters:",
            "    {}",
            "",
            "── messages (3)",
            "▸ user",
            "  hi",
            "▸ assistant · calls run_js",
            "  looking",
            "  run_js({\"code\":\"1\"})",
            "▸ tool · answers c1",
            "  1",
        ].iter().map(|s| s.to_string()).collect()
    }

    fn part_of(rows: &[Row], text: &str) -> Part {
        rows.iter().find_map(|r| match r {
            Row::Text { text: t, part, .. } if t == text => Some(*part),
            _ => None,
        }).unwrap_or_else(|| panic!("no row {text:?}"))
    }

    #[test]
    fn every_line_gets_the_part_it_belongs_to() {
        let rows = rows(&report());
        assert_eq!(part_of(&rows, "executor   ptc"), Part::Meta);
        assert_eq!(part_of(&rows, "You are Lele"), Part::System);
        assert_eq!(part_of(&rows, "  Run a program."), Part::Tools);
        assert_eq!(part_of(&rows, "    {}"), Part::Schema);
        assert_eq!(part_of(&rows, "  hi"), Part::User);
        assert_eq!(part_of(&rows, "  looking"), Part::Assistant);
        assert_eq!(part_of(&rows, "  run_js({\"code\":\"1\"})"), Part::Call);
        assert_eq!(part_of(&rows, "  1"), Part::Result);
        assert_eq!(part_of(&rows, "── messages (3)"), Part::Section);
    }

    #[test]
    fn the_estimate_becomes_a_bar_and_a_legend() {
        let rows = rows(&report());
        let usage = vec![(Part::System, 10), (Part::Tools, 300), (Part::Assistant, 90)];
        assert!(rows.contains(&Row::Usage(usage.clone())));
        let legend: String = rows.iter().find_map(|r| match r {
            Row::Spans(s) => Some(s.iter().map(|(t, _)| t.as_str()).collect()),
            _ => None,
        }).unwrap();
        assert_eq!(legend, "■ system ~10   ■ tools ~300   ■ messages ~90");
    }

    #[test]
    fn the_bar_fills_its_width_and_keeps_small_parts_visible() {
        let cells = bar_cells(&[(Part::System, 3), (Part::Tools, 360), (Part::Assistant, 0)], 40);
        assert_eq!(cells.iter().map(|(_, c)| c).sum::<usize>(), 40);
        assert_eq!(cells[0], (Part::System, 1), "a tiny part still shows");
        assert_eq!(cells[2], (Part::Assistant, 0), "an empty part takes no room");
        assert!(bar_cells(&[(Part::System, 0)], 40).is_empty());
    }
}
