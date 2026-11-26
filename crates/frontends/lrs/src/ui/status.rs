//! The one-line status strip under the input — what the sidebar used to say.
//!
//! Deliberately a single line: lrs has no sidebar, so this is the only
//! always-visible state. Segments that have nothing to report drop out entirely
//! rather than rendering as an empty label.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::theme::Theme;

#[derive(Debug, Default, Clone)]
pub struct Status {
    pub cwd: String,
    pub model: String,
    pub effort: String,
    /// Tokens in the last request's context window, 0 before the first reply.
    pub context: u64,
    /// Auto-compact threshold. 0 means the server couldn't resolve one, so the
    /// gauge shows a bare count rather than a made-up denominator.
    pub context_limit: u64,
    pub session: String,
    /// Shown while a turn is in flight.
    pub busy: bool,
    /// `ptc` (code mode) or `native`; empty when the backend does not say.
    pub executor: String,
    /// The user turned the context gauge off in `lrs.toml`.
    pub hide_context: bool,
}

fn seg<'a>(label: &str, value: String, theme: &Theme) -> Vec<Span<'a>> {
    let mut out = Vec::new();
    if !label.is_empty() {
        out.push(Span::styled(
            format!("{label} "),
            Style::default().fg(theme.muted),
        ));
    }
    out.push(Span::styled(value, Style::default().fg(theme.fg)));
    out
}

pub fn line(s: &Status, theme: &Theme) -> Line<'static> {
    let mut parts: Vec<Vec<Span>> = Vec::new();

    if s.busy {
        parts.push(vec![Span::styled(
            "● working".to_string(),
            Style::default().fg(theme.accent),
        )]);
    }
    if !s.cwd.is_empty() {
        parts.push(seg("", s.cwd.clone(), theme));
    }
    if !s.model.is_empty() {
        parts.push(seg("", s.model.clone(), theme));
    }
    // Code mode changes how the model works, so it stands out; native is the
    // plain case and stays quiet.
    match s.executor.as_str() {
        "ptc" => parts.push(vec![Span::styled("code mode".to_string(), Style::default().fg(theme.tool))]),
        "native" => parts.push(vec![Span::styled("native tools".to_string(), Style::default().fg(theme.muted))]),
        _ => {}
    }
    if !s.effort.is_empty() {
        parts.push(seg("effort", s.effort.clone(), theme));
    }
    if s.context > 0 && !s.hide_context {
        // Colour by pressure, so approaching auto-compact is visible without
        // reading the numbers.
        let (text, colour) = match s.context_limit {
            0 => (format!("ctx {}", human_tokens(s.context)), theme.muted),
            limit => {
                let pct = s.context * 100 / limit;
                let c = if pct < 70 {
                    theme.muted
                } else if pct < 90 {
                    theme.warn
                } else {
                    theme.err
                };
                (
                    format!(
                        "ctx {}/{} {pct}%",
                        human_tokens(s.context),
                        human_tokens(limit)
                    ),
                    c,
                )
            }
        };
        parts.push(vec![Span::styled(text, Style::default().fg(colour))]);
    }

    let mut spans = Vec::new();
    for (i, p) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" · ".to_string(), Style::default().fg(theme.border)));
        }
        spans.extend(p);
    }
    Line::from(spans)
}

/// 1234 -> "1.2k". Exact counts below 1000, because that range is where the
/// difference actually reads as information.
fn human_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

pub fn draw(f: &mut Frame, area: Rect, s: &Status, theme: &Theme) {
    f.render_widget(Paragraph::new(line(s, theme)), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &Status) -> String {
        line(s, &Theme::tokyonight())
            .spans
            .iter()
            .map(|x| x.content.as_ref())
            .collect()
    }

    #[test]
    fn empty_segments_disappear_instead_of_showing_labels() {
        let s = Status { model: "opus".into(), ..Default::default() };
        assert_eq!(text(&s), "opus");
    }

    #[test]
    fn the_context_gauge_can_be_hidden() {
        let shown = Status { model: "m".into(), context: 8_000, context_limit: 32_000, ..Default::default() };
        assert_eq!(text(&shown), "m · ctx 8.0k/32.0k 25%");
        let hidden = Status { hide_context: true, ..shown };
        assert_eq!(text(&hidden), "m");
    }

    #[test]
    fn segments_are_joined_with_a_separator() {
        let s = Status {
            cwd: "lele".into(),
            model: "opus".into(),
            effort: "high".into(),
            ..Default::default()
        };
        assert_eq!(text(&s), "lele · opus · effort high");
    }

    #[test]
    fn the_executor_says_whether_code_mode_is_on() {
        let mut s = Status { model: "m".into(), executor: "ptc".into(), ..Default::default() };
        assert_eq!(text(&s), "m · code mode");
        s.executor = "native".into();
        assert_eq!(text(&s), "m · native tools");
        s.executor = String::new();
        assert_eq!(text(&s), "m", "a backend that does not say shows nothing");
    }

    #[test]
    fn code_mode_takes_the_tool_colour_of_the_theme() {
        let theme = Theme::tokyonight();
        let s = Status { executor: "ptc".into(), ..Default::default() };
        let span = line(&s, &theme).spans.into_iter().find(|x| x.content == "code mode").unwrap();
        assert_eq!(span.style.fg, Some(theme.tool));
    }

    #[test]
    fn zero_context_is_hidden_until_the_first_reply() {
        let mut s = Status { model: "m".into(), ..Default::default() };
        assert_eq!(text(&s), "m");
        s.context = 32_500;
        assert_eq!(text(&s), "m · ctx 32.5k");
    }

    #[test]
    fn the_gauge_shows_the_limit_and_a_percentage_when_known() {
        let s = Status { context: 20_000, context_limit: 200_000, ..Default::default() };
        assert_eq!(text(&s), "ctx 20.0k/200.0k 10%");
    }

    #[test]
    fn without_a_known_limit_only_the_count_is_shown() {
        // Better a bare number than an invented denominator.
        let s = Status { context: 20_000, context_limit: 0, ..Default::default() };
        assert_eq!(text(&s), "ctx 20.0k");
    }

    #[test]
    fn the_gauge_warns_as_it_approaches_the_limit() {
        let t = Theme::tokyonight();
        let colour = |used: u64| {
            let s = Status { context: used, context_limit: 100, ..Default::default() };
            line(&s, &t).spans.last().unwrap().style.fg.unwrap()
        };
        assert_eq!(colour(50), t.muted);
        assert_eq!(colour(80), t.warn);
        assert_eq!(colour(95), t.err);
    }

    #[test]
    fn busy_leads_so_it_is_visible_at_a_glance() {
        let s = Status { model: "m".into(), busy: true, ..Default::default() };
        assert!(text(&s).starts_with("● working"));
    }

    #[test]
    fn token_counts_stay_exact_below_a_thousand() {
        assert_eq!(human_tokens(999), "999");
        assert_eq!(human_tokens(1000), "1.0k");
    }
}
