//! Rendering. No sidebar by design: the transcript gets the full width, and the
//! only persistent chrome is a single status line under the prompt.
//!
//! ```text
//! ┌──────────────────────────────┐
//! │ transcript (scrolls)         │
//! │                              │
//! ├──────────────────────────────┤
//! │   ▸ /theme   switch theme    │  completion menu: /commands and @paths
//! │ ❯ prompt                     │  grows with the text, up to a cap
//! │ cwd · model · ctx 12k        │  status
//! └──────────────────────────────┘
//! ```

pub mod ask;
pub mod block;
pub mod chat;
pub mod context;
pub mod input;
pub mod markdown;
pub mod markup;
pub mod overlay;
pub mod picker;
pub mod selection;
pub mod spinner;
pub mod status;
pub mod steer;
pub mod viewer;
pub mod widgets;

use ratatui::layout::{Constraint, Direction, Layout, Rect};

/// The prompt may not eat more than this many rows, however much you paste.
const MAX_INPUT_ROWS: u16 = 8;

pub struct Panes {
    pub chat: Rect,
    /// Plugin-drawn panels, pinned so they stay visible as the transcript scrolls
    /// past. The checklist is one of these now: the todo package publishes markup
    /// like any other panel, so this client no longer carries a checklist
    /// renderer of its own to keep in step with it.
    pub widgets: Option<Rect>,
    /// The divider between transcript and prompt. `None` on a terminal too short
    /// to spare a row for it — chrome yields to content.
    pub rule: Option<Rect>,
    /// Messages waiting to be injected into the running turn.
    pub queued: Option<Rect>,
    pub menu: Option<Rect>,
    pub input: Rect,
    pub status: Rect,
}

/// Rows the prompt and status always need, before any optional chrome.
const FIXED_ROWS: u16 = 2;

/// Split the frame bottom-up: status and prompt take what they need, the
/// transcript takes the rest. Optional chrome is granted in priority order and
/// each piece only if a row remains for the transcript itself.
pub fn layout(
    area: Rect,
    input_rows: u16,
    menu_rows: u16,
    widget_rows: u16,
    queued_rows: u16,
) -> Panes {
    let input_rows = input_rows.clamp(1, MAX_INPUT_ROWS);
    let mut spare = area.height.saturating_sub(input_rows + FIXED_ROWS);

    let menu_rows = menu_rows.min(spare);
    spare -= menu_rows;
    // Right after the menu, and before the pinned panels can claim anything: a
    // message the user just sent going unacknowledged is worse than a checklist
    // losing a row.
    let queued_rows = queued_rows.min(spare);
    spare -= queued_rows;
    // Pinned panels may take at most half of what's left, so neither a long
    // checklist nor a chatty plugin can squeeze the conversation out of view.
    let widget_rows = widget_rows.min(spare / 2);
    spare -= widget_rows;
    let rule_rows = u16::from(spare > 0);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(widget_rows),
            Constraint::Length(rule_rows),
            Constraint::Length(queued_rows),
            Constraint::Length(menu_rows),
            Constraint::Length(input_rows),
            Constraint::Length(1),
        ])
        .split(area);

    Panes {
        chat: chunks[0],
        widgets: (widget_rows > 0).then(|| chunks[1]),
        rule: (rule_rows > 0).then(|| chunks[2]),
        queued: (queued_rows > 0).then(|| chunks[3]),
        menu: (menu_rows > 0).then(|| chunks[4]),
        input: chunks[5],
        status: chunks[6],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(h: u16) -> Rect {
        Rect::new(0, 0, 80, h)
    }

    #[test]
    fn status_is_always_exactly_one_row() {
        let p = layout(area(24), 1, 0, 0, 0);
        assert_eq!(p.status.height, 1);
    }

    #[test]
    fn no_menu_means_no_menu_pane() {
        assert!(layout(area(24), 1, 0, 0, 0).menu.is_none());
        assert!(layout(area(24), 1, 3, 0, 0).menu.is_some());
    }

    #[test]
    fn the_prompt_is_capped_however_much_you_paste() {
        let p = layout(area(40), 100, 0, 0, 0);
        assert_eq!(p.input.height, MAX_INPUT_ROWS);
    }

    #[test]
    fn transcript_keeps_room_on_a_short_terminal() {
        // A tall menu on a small window must not squeeze the chat to nothing.
        let p = layout(area(10), 3, 20, 0, 0);
        assert!(p.chat.height >= 1, "chat vanished: {:?}", p.chat);
    }

    #[test]
    fn panes_do_not_overlap_and_cover_the_area() {
        let p = layout(area(24), 2, 3, 1, 2);
        let total = p.chat.height
            + p.widgets.map_or(0, |w| w.height)
            + p.rule.map_or(0, |r| r.height)
            + p.queued.map_or(0, |q| q.height)
            + p.menu.map_or(0, |m| m.height)
            + p.input.height
            + p.status.height;
        assert_eq!(total, 24);
    }

    #[test]
    fn the_steer_queue_sits_just_above_the_prompt() {
        let p = layout(area(24), 1, 0, 0, 2);
        let q = p.queued.expect("no queue strip");
        assert_eq!(q.height, 2);
        assert!(q.y < p.input.y, "the queue must be above the prompt");
    }

    #[test]
    fn the_queue_outranks_the_pinned_panels_for_space() {
        // A checklist losing a row is survivable; a message you just sent going
        // unacknowledged is not.
        let p = layout(area(12), 1, 0, 6, 3);
        assert_eq!(p.queued.map_or(0, |q| q.height), 3);
        assert!(p.chat.height >= 1);
    }

    #[test]
    fn a_plugin_panel_gets_its_own_strip_above_the_divider() {
        let p = layout(area(24), 1, 0, 3, 0);
        let w = p.widgets.expect("no widget strip");
        assert_eq!(w.height, 3);
        assert!(p.rule.is_none_or(|r| w.y < r.y), "panels belong above the divider");
    }

    #[test]
    fn pinned_panels_never_take_more_than_half_the_room() {
        // A chatty plugin and a long checklist share one strip now, and it must
        // still leave a conversation behind.
        let p = layout(area(20), 1, 0, 30, 0);
        let pinned = p.widgets.map_or(0, |w| w.height);
        assert!(pinned <= (20 - 3) / 2, "pinned rows took {pinned}");
        assert!(p.chat.height >= 1);
    }

    #[test]
    fn the_divider_yields_before_the_transcript_does() {
        // Roomy: divider present.
        assert!(layout(area(24), 1, 0, 0, 0).rule.is_some());
        // Squeezed to the bone: prompt and status only, and the chat still exists.
        let tight = layout(area(3), 1, 0, 0, 0);
        assert!(tight.rule.is_none(), "chrome must yield first");
        assert!(tight.chat.height >= 1);
    }
}
