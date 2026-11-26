//! Keyboard handling.
//!
//! Split out of the loop so the binding table is one readable match rather than
//! nesting inside the draw rhythm.

use anyhow::Result;
use crossterm::event::{
    self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::{App, Overlay};
use crate::ui::block::Level;

/// Rows scrolled by PageUp/PageDown.
const PAGE: i32 = 10;

/// Events taken from one drain. Bounded so a paste of a megabyte can't hold the
/// frame — the rest arrives on the next tick.
const BATCH_MAX: usize = 4096;

/// Printable keystrokes in a single drain past which it cannot be typing: the
/// loop drains every 30ms, so this is over 250 characters a second.
const PASTE_BURST: usize = 8;

/// Stop accumulating past this and hand over what we have. A paste this big is
/// already a chip; holding more only delays it.
const PASTE_MAX: usize = 1 << 20;

/// Quiet time that ends a paste.
///
/// The console does not deliver a paste in one go — it fills the input buffer in
/// pieces, and the gaps between them are longer than a 30ms tick. Ending the
/// paste at the first empty tick split one `^v` into `Edit d:\P` followed by a
/// separate chip. Long enough to ride out those gaps, short enough that an enter
/// pressed after the paste still sends rather than adding a newline.
const PASTE_QUIET: Duration = Duration::from_millis(120);

/// Read terminal events on a thread of their own, and hand them over a channel.
///
/// `event::read()` blocks — and not only when the input buffer is empty. It
/// blocks whenever the records that *are* pending all get filtered out on the way
/// to an `Event`: a mouse move that stayed in the same cell, a key release, a
/// focus change. `event::poll` counts console records, so it says "ready" for
/// those too, and the read that follows then waits for input that may not come
/// until you touch the mouse again.
///
/// On the render thread that is the app hanging. Here it is one thread parked in
/// a blocking read, which is what it is for.
pub fn spawn_reader() -> std::sync::mpsc::Receiver<TermEvent> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(ev) => {
                    if tx.send(ev).is_err() {
                        return; // the app is gone
                    }
                }
                // A terminal that can no longer be read is not going to recover;
                // dropping the sender tells the loop to quit.
                Err(_) => return,
            }
        }
    });
    rx
}

pub fn poll(app: &mut App, input: &Receiver<TermEvent>, timeout: Duration) -> Result<()> {
    use std::sync::mpsc::RecvTimeoutError;
    let first = match input.recv_timeout(timeout) {
        Ok(ev) => ev,
        Err(RecvTimeoutError::Timeout) => {
            // Nothing arrived this tick — which settles typed characters, and may
            // or may not be the end of a paste. `flush_keys` decides by how long
            // the silence has lasted.
            flush_keys(app);
            return Ok(());
        }
        // The reader stopped, so no key will ever arrive again — including the
        // one that would quit.
        Err(RecvTimeoutError::Disconnected) => {
            app.should_quit = true;
            return Ok(());
        }
    };
    // Take everything already queued, not just the first event. On Windows this
    // is the only way to tell a paste from typing (see `printable_text`), and it
    // also keeps a fast stream of input from being metered out one per frame.
    let mut batch = vec![first];
    while batch.len() < BATCH_MAX {
        match input.try_recv() {
            Ok(ev) => batch.push(ev),
            Err(_) => break,
        }
    }
    take_batch(app, batch);
    Ok(())
}

/// Act on one drain of terminal events.
fn take_batch(app: &mut App, mut batch: Vec<TermEvent>) {
    // A pointer drifting across the window while a paste streams in puts mouse
    // moves between its characters. Left in, they made the batch "not text", so
    // every pasted newline ran as enter and each line went out as its own message.
    // Nothing acts on these events, so they are dropped before the batch is judged.
    batch.retain(|ev| !inert(ev));
    if batch.is_empty() {
        // Only inert events: as good as silence. Flushing outright would cut a
        // held paste in two; never flushing would strand typing while the mouse
        // keeps moving, since the quiet tick that settles it never comes.
        flush_keys(app);
        return;
    }

    // Everything typable waits a tick before it counts, whether it looks like a
    // paste or not. Deciding chunk by chunk was wrong at both ends: a paste's
    // first chunk can be a single character — `S[Pasted 11 lines]` — and its
    // middle chunks can be a bare "\n\n" between two paragraphs, and either one
    // going through as keystrokes breaks the paste apart.
    if let Some(text) = printable_text(&batch) {
        let now = std::time::Instant::now();
        let (buf, last) = app.key_buf.get_or_insert_with(|| (String::new(), now));
        buf.push_str(&text);
        *last = now;
        if buf.len() >= PASTE_MAX {
            force_flush(app);
        }
        return;
    }

    // This batch is not text, so nothing more can join what is held: it has to
    // land before these events are acted on, or a click would take effect ahead
    // of the characters typed before it.
    force_flush(app);
    let mut last_drag = None;
    for ev in batch {
        match ev {
            TermEvent::Key(k) => handle_key(app, k),
            TermEvent::Paste(text) => paste(app, &text),
            // Capturing the mouse means every cell the pointer crosses is an
            // event, and a drag is a torrent of them. Only where it ended up
            // matters, so the intermediate ones are dropped rather than each
            // one dragging the selection to a position already superseded.
            TermEvent::Mouse(m) if matches!(m.kind, MouseEventKind::Drag(MouseButton::Left)) => {
                last_drag = Some(m);
            }
            TermEvent::Mouse(m) => {
                // Order matters: a release in the same batch has to see the drag.
                if let Some(d) = last_drag.take() {
                    handle_mouse(app, d);
                }
                handle_mouse(app, m);
            }
            // Inert events were filtered out above.
            TermEvent::FocusGained | TermEvent::FocusLost | TermEvent::Resize(..) => {}
        }
    }
    if let Some(d) = last_drag {
        handle_mouse(app, d);
    }
}

/// Events nothing in the app acts on: a pointer move (only drags select), a focus
/// change, a resize (the next frame lays itself out again).
fn inert(ev: &TermEvent) -> bool {
    match ev {
        TermEvent::Mouse(m) => m.kind == MouseEventKind::Moved,
        TermEvent::FocusGained | TermEvent::FocusLost | TermEvent::Resize(..) => true,
        TermEvent::Key(_) | TermEvent::Paste(_) => false,
    }
}

/// Act on held input once it has settled.
fn flush_keys(app: &mut App) {
    let ready = match app.key_buf.as_ref() {
        // A paste stalls mid-delivery, so it needs a real silence to be over.
        // Typed characters need to appear now: one empty tick is the whole wait.
        Some((buf, last)) => !looks_pasted(buf) || last.elapsed() >= PASTE_QUIET,
        None => return,
    };
    if ready {
        force_flush(app);
    }
}

/// Act on it now, whatever the timing.
fn force_flush(app: &mut App) {
    let Some((text, last)) = app.key_buf.take() else { return };
    crate::app::log::trace!(
        "flush {} chars, {} CR, {} LF, quiet {}ms, pasted {}",
        text.chars().count(),
        text.matches('\r').count(),
        text.matches('\n').count(),
        last.elapsed().as_millis(),
        looks_pasted(&text)
    );
    if looks_pasted(&text) {
        // One paste, one chip: collapsing each chunk separately would leave a row
        // of `[Pasted 40 lines]` markers for a single ^v.
        paste(app, &text);
    } else {
        replay_typed(app, &text);
    }
}

/// Hand held characters to the normal keystroke path, in order.
///
/// Only the keys [`printable_text`] accepts can be in here, so rebuilding the
/// events from the text loses nothing that the handler looks at: CR is enter,
/// LF is ctrl+enter (a newline in the prompt), tab is tab.
fn replay_typed(app: &mut App, text: &str) {
    for c in text.chars() {
        let (code, mods) = match c {
            '\r' => (KeyCode::Enter, KeyModifiers::NONE),
            '\n' => (KeyCode::Enter, KeyModifiers::CONTROL),
            '\t' => (KeyCode::Tab, KeyModifiers::NONE),
            c => (KeyCode::Char(c), KeyModifiers::NONE),
        };
        handle_key(app, KeyEvent::new(code, mods));
    }
}

/// Does this drain open a paste?
///
/// Two tells, either one enough. Eight printable keys in a 30ms drain is over 250
/// characters a second. And a newline with more text behind it in the same drain
/// is something no one types: pressing enter ends what you were writing, so the
/// next character is always at least a frame away.
fn looks_pasted(text: &str) -> bool {
    text.chars().count() >= PASTE_BURST
        || text.find(['\r', '\n']).is_some_and(|i| i + 1 < text.len())
}

fn paste(app: &mut App, text: &str) {
    // Bracketed paste hands over the clipboard's own line endings, and a CR left
    // in the prompt draws as a stray glyph and reaches the model as one.
    let text = &text.replace("\r\n", "\n").replace('\r', "\n");
    match app.overlay.as_mut() {
        // A key is pasted, never typed — the modal has to take it.
        Some(Overlay::Ask(a)) => {
            a.insert_str(text);
            return;
        }
        Some(Overlay::Pick(_) | Overlay::View(_)) => return,
        None => {}
    }
    // A paste of file paths is a paste of pictures. This is the route that works
    // on every terminal, because it needs no keybinding at all.
    if !app.attach_pasted_images(text) {
        paste_text(app, text);
    }
}

/// The batch read back as text, or `None` if it holds anything a paste cannot.
///
/// The Windows console has no bracketed paste — crossterm only ever emits
/// `Event::Paste` on Unix — so an incoming paste is delivered as ordinary key
/// records, newlines included. Each of those newlines was being handled as enter,
/// which submitted the line: pasting a ten-line snippet sent ten messages.
///
/// Anything that isn't a plain character — a chord, an arrow, a function key —
/// rules the batch out entirely, since a paste contains none of them. Whether a
/// batch of pure text *is* a paste is [`looks_pasted`]'s question, not this one's.
fn printable_text(batch: &[TermEvent]) -> Option<String> {
    let mut out = String::new();
    for (i, ev) in batch.iter().enumerate() {
        // Which event broke a batch is the whole question when a paste goes out
        // line by line, and it depends on the terminal in front of the console.
        let rejected = |why: &str| {
            crate::app::log::trace!(
                "batch of {} not text: {why} at {i} after {} chars: {ev:?}",
                batch.len(),
                out.chars().count()
            );
        };
        let TermEvent::Key(k) = ev else {
            rejected("not a key");
            return None;
        };
        // Windows sends a release for every press; only the presses carry text.
        if k.kind == KeyEventKind::Release {
            continue;
        }
        // The console encodes each pasted character as the keystroke that would
        // produce it on the *current layout*, so on an Italian keyboard every
        // pasted `{ } [ ] @ #` arrives carrying AltGr — reported as ctrl+alt.
        // Treating that as a chord split a pasted JSON blob at every brace: one
        // ^v became a row of chips with the braces stranded between them.
        //
        // A real chord is ctrl without alt. Alt alone is how the console encodes
        // characters the layout cannot reach at all (`—`, `…`), so it is text too.
        //
        // Ctrl+enter is the exception: it is LF. Behind ConPTY (herdr, any
        // terminal that writes the paste into a pseudo-console) every pasted line
        // feed arrives as ctrl+enter. Rejecting it as a chord threw out the whole
        // batch, so the CR beside each LF ran as a real enter and a CRLF paste
        // went out one message per line.
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let line_feed = k.code == KeyCode::Enter && k.modifiers == KeyModifiers::CONTROL;
        if k.modifiers.contains(KeyModifiers::CONTROL) && !alt && !line_feed {
            rejected("ctrl chord");
            return None;
        }
        match k.code {
            // Shift is how a capital arrives, so it cannot disqualify a character.
            KeyCode::Char(c) => out.push(c),
            // Enter is CR and ctrl+enter is LF, kept apart so that replaying typed
            // keys still tells "send" from "newline". A paste turns both into LF.
            KeyCode::Enter if line_feed => out.push('\n'),
            // Any other *modified* enter is a binding, not text. Shift+enter is the
            // newline the editor inserts itself, and buffering it here replayed it
            // as a bare enter — which submitted the message instead.
            KeyCode::Enter if k.modifiers.is_empty() => out.push('\r'),
            KeyCode::Tab if k.modifiers.is_empty() => out.push('\t'),
            _ => {
                rejected("not a character");
                return None;
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Rows the wheel moves per notch.
const WHEEL: i32 = 3;

/// Select-to-copy, terminal style: press, drag, release copies.
///
/// Capturing the mouse is what makes this possible and is also its cost — the
/// terminal's own selection stops working while lrs is up. Most terminals
/// hand it back if you hold shift while dragging.
fn handle_mouse(app: &mut App, m: MouseEvent) {
    // A modal is on top of the transcript; dragging a selection out from under it
    // would highlight text the user can't even see.
    if app.overlay_open() {
        return;
    }
    match m.kind {
        // Capturing the mouse also captures the wheel, so scrolling has to be
        // re-implemented or it simply stops working.
        MouseEventKind::ScrollUp => app.scroll_by(-WHEEL),
        MouseEventKind::ScrollDown => app.scroll_by(WHEEL),
        MouseEventKind::Down(MouseButton::Left) => app.begin_selection(m.column, m.row),
        MouseEventKind::Drag(MouseButton::Left) => app.drag_selection(m.column, m.row),
        MouseEventKind::Up(MouseButton::Left) => app.copy_selection(),
        _ => {}
    }
}

fn handle_key(app: &mut App, k: KeyEvent) {
    // Windows reports both press and release; acting on both double-types.
    if k.kind == KeyEventKind::Release {
        return;
    }
    // Every key, so "the binding is broken" and "the terminal ate the key before
    // we saw it" can be told apart — Windows Terminal binds ^v to its own paste,
    // and from inside the app that is indistinguishable from a dead binding.
    crate::app::log::trace!("key {:?} mods {:?}", k.code, k.modifiers);
    // AltGr, not a chord. The Windows console reports AltGr as ctrl+alt pressed
    // together, and on an Italian layout that is the only way to type @ # [ ] { }
    // — all of which were being swallowed by the ctrl branch below. Nothing here
    // binds a ctrl+alt chord, so the combination can only mean a character.
    //
    // Harmless elsewhere: X11 and macOS compose the character themselves and send
    // it with no modifiers at all, so this never fires there. The only thing it
    // gives up on any platform is a literal ctrl+alt chord, and there are none.
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL) && !alt;
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);

    // A modal owns the keyboard while it is up: anything that fell through to the
    // prompt underneath would type into an input the user can't see.
    if app.overlay_open() {
        handle_overlay(app, k, ctrl);
        return;
    }

    // The highlight survives the copy, like a terminal's, but any keystroke means
    // the user has moved on — leaving it up would be a stale bar over the text.
    app.selection = None;

    if ctrl {
        match k.code {
            // Escalating, like a shell: stop what's running, else throw away what
            // you typed, and only quit once there is nothing left to interrupt.
            // Reaching the exit therefore always takes a deliberate second press
            // after a non-empty prompt. While a stop is already under way the
            // second press leaves: a turn that will not stop must not hold you.
            KeyCode::Char('c') => {
                if app.working() && app.cancelling() {
                    app.should_quit = true;
                } else if app.working() {
                    app.cancel();
                } else if !app.ed.is_empty() {
                    app.ed.clear();
                } else if app.ed.shell_mode() {
                    app.ed.leave_shell();
                } else {
                    app.should_quit = true;
                }
            }
            KeyCode::Char('d') | KeyCode::Char('q') => app.should_quit = true,
            KeyCode::Char('o') => {
                app.expand_all = !app.expand_all;
            }
            KeyCode::Char('l') => app.to_bottom(),
            KeyCode::Char('w') => app.ed.delete_word(),
            KeyCode::Char('u') => app.ed.clear(),
            // Both are LF: ^j by name, ctrl+enter as the console encodes it.
            KeyCode::Char('j') | KeyCode::Enter => app.ed.insert('\n'),
            // ^v mostly never gets here: Windows Terminal binds it to its own
            // paste, and with only a picture on the clipboard that pastes nothing
            // at all — proved with the trace, which logged the control keystroke
            // typed straight after it and no ^v. ^p is the one that survives:
            // Ctrl+Shift+P is the command palette, plain Ctrl+P is unbound.
            // /image still works everywhere and needs no binding at all.
            KeyCode::Char('v') | KeyCode::Char('p') => app.paste_image(),
            // The bare arrows now recall history, so line-by-line scrolling moves
            // here — pgup/pgdn still jump a screenful.
            KeyCode::Up => app.scroll_by(-1),
            KeyCode::Down => app.scroll_by(1),
            _ => {}
        }
        return;
    }

    match k.code {
        KeyCode::Enter if shift => app.ed.insert('\n'),
        KeyCode::Enter => {
            if app.menu_open() && !app.menu_exact() {
                complete_menu(app);
            } else {
                app.submit();
            }
        }
        KeyCode::Tab if app.menu_open() => complete_menu(app),
        KeyCode::Esc => {
            if app.working() {
                app.cancel();
            } else if !app.ed.is_empty() {
                app.ed.clear();
            } else {
                app.ed.leave_shell();
            }
        }
        KeyCode::Backspace => backspace(app),
        KeyCode::Left => app.ed.left(),
        KeyCode::Right => app.ed.right(),
        KeyCode::Home => app.ed.home(),
        KeyCode::End => app.ed.end(),
        // The menu takes the arrows only while you are typing into it. Once the
        // history owns the prompt they belong to the history, or recalling a
        // command would end the walk: `/help` comes back, its own menu opens over
        // it, and the next up arrow moves that menu instead of going further
        // back. Editing the recalled text ends the browse and hands them over.
        KeyCode::Up if app.menu_open() && !app.hist.browsing() => {
            app.menu_sel = app.menu_sel.saturating_sub(1);
        }
        KeyCode::Down if app.menu_open() && !app.hist.browsing() => {
            let n = app.menu_rows().len();
            app.menu_sel = (app.menu_sel + 1).min(n.saturating_sub(1));
        }
        KeyCode::PageUp => app.scroll_by(-PAGE),
        KeyCode::PageDown => app.scroll_by(PAGE),
        KeyCode::Up => history_up(app),
        KeyCode::Down => history_down(app),
        // `!` on an empty prompt turns it into a command prompt instead of typing.
        KeyCode::Char('!') if app.ed.is_empty() && !app.ed.shell_mode() => {
            app.ed.enter_shell();
            app.hist.stop();
        }
        KeyCode::Char(c) => {
            app.ed.insert(c);
            // Editing makes the recalled text the user's own draft.
            app.hist.stop();
            // The old selection is meaningless once the filter changes.
            app.menu_sel = 0;
        }
        _ => {}
    }
}

/// A big paste becomes a chip instead of flooding the prompt. Anything short goes
/// in as itself — collapsing three lines would be pure obstruction.
fn paste_text(app: &mut App, text: &str) {
    if crate::app::paste::Pastes::should_collapse(text) {
        let chip = app.pastes.add_text(text);
        app.ed.insert_str(&chip);
    } else {
        app.ed.insert_str(text);
    }
    app.hist.stop();
}

/// Backspace deletes a chip as one unit. Removing a single character would leave
/// `[Pasted 40 lines` — a label that no longer matches anything held, so the paste
/// would silently stop being sent with no way to tell.
fn backspace(app: &mut App) {
    // Backspace on an empty command prompt takes the `!` back.
    if app.ed.is_empty() && app.ed.shell_mode() {
        app.ed.leave_shell();
        return;
    }
    let chip = app
        .pastes
        .chip_ending_at(app.ed.text(), app.ed.cursor())
        .map(str::len);
    match chip {
        Some(n) => app.ed.delete_back(n),
        None => app.ed.backspace(),
    }
    app.hist.stop();
    // Deleting widens the match set just as typing narrows it; a held-over
    // selection would then complete a row the user never highlighted.
    app.menu_sel = 0;
}

/// Shell-style recall, with the concession multi-line prompts need: the arrow only
/// reaches the history when the caret is on the prompt's first line, so a recalled
/// two-line prompt can still be navigated. Once browsing, the arrows keep the
/// history until the user edits or submits.
fn history_up(app: &mut App) {
    if !app.hist.browsing() && !app.ed.on_first_line() {
        app.ed.line_up();
        return;
    }
    let current = app.ed.text().to_string();
    if let Some(entry) = app.hist.prev(&current) {
        app.recall(&entry);
    }
}

fn history_down(app: &mut App) {
    if !app.hist.browsing() {
        if !app.ed.on_last_line() {
            app.ed.line_down();
        }
        return;
    }
    if let Some(entry) = app.hist.next() {
        app.recall(&entry);
    }
}

/// Route a keystroke to the open modal.
fn handle_overlay(app: &mut App, k: KeyEvent, ctrl: bool) {
    if ctrl {
        // ^c out of a modal, and never into a quit: the prompt behind it is intact.
        if matches!(k.code, KeyCode::Char('c') | KeyCode::Char('u')) {
            app.close_overlay();
        }
        return;
    }
    match k.code {
        KeyCode::Esc => app.close_overlay(),
        KeyCode::Enter => app.overlay_submit(),
        _ => match app.overlay.as_mut() {
            Some(Overlay::Pick(p)) => match k.code {
                KeyCode::Up => p.up(),
                KeyCode::Down => p.down(),
                KeyCode::Backspace => p.backspace(),
                KeyCode::Char(c) => p.insert(c),
                _ => {}
            },
            Some(Overlay::Ask(a)) => match k.code {
                KeyCode::Backspace => a.backspace(),
                KeyCode::Char(c) => a.insert(c),
                _ => {}
            },
            Some(Overlay::View(v)) => match k.code {
                KeyCode::Up | KeyCode::Char('k') => v.up(),
                KeyCode::Down | KeyCode::Char('j') => v.down(),
                KeyCode::PageUp => v.page_up(),
                KeyCode::PageDown | KeyCode::Char(' ') => v.page_down(),
                KeyCode::Home | KeyCode::Char('g') => v.home(),
                KeyCode::End | KeyCode::Char('G') => v.end(),
                KeyCode::Char('q') => app.close_overlay(),
                _ => {}
            },
            None => {}
        },
    }
}

fn complete_menu(app: &mut App) {
    let rows = app.menu_rows();
    match rows.get(app.menu_sel).or_else(|| rows.first()) {
        Some(row) => {
            let insert = row.insert.clone();
            match row.replace {
                // An @path fills in its own token; the rest of the line stays.
                Some((s, e)) => app.ed.replace_range(s, e, &insert),
                // `complete` marks any `<placeholder>` so the next keystroke types
                // over it, which is what makes `/goal <objective>` usable in one go.
                None => app.ed.complete(&insert),
            }
            app.menu_sel = 0;
        }
        None => app.notice("no matching command", Level::Warn),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::app::tests::health_fixture;
    use crate::net::Client;

    fn app() -> App {
        App::new(std::sync::Arc::new(crate::backend::HttpBackend(Client::new(1))), false, health_fixture())
    }

    fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        handle_key(app, KeyEvent::new(code, mods));
    }

    /// Drive a keystroke from another module's tests — the routing lives here, so
    /// asserting on its effects shouldn't mean reimplementing it there.
    pub(crate) fn press_key(app: &mut App, c: char) {
        press(app, KeyCode::Char(c), KeyModifiers::NONE);
    }

    pub(crate) fn press_code(app: &mut App, code: KeyCode, mods: KeyModifiers) {
        press(app, code, mods);
    }

    pub(crate) fn press_esc(app: &mut App) {
        press(app, KeyCode::Esc, KeyModifiers::NONE);
    }

    pub(crate) fn press_tab(app: &mut App) {
        press(app, KeyCode::Tab, KeyModifiers::NONE);
    }

    pub(crate) fn press_enter(app: &mut App) {
        press(app, KeyCode::Enter, KeyModifiers::NONE);
    }

    pub(crate) fn press_up(app: &mut App) {
        press(app, KeyCode::Up, KeyModifiers::NONE);
    }

    pub(crate) fn press_down(app: &mut App) {
        press(app, KeyCode::Down, KeyModifiers::NONE);
    }

    pub(crate) fn wheel_up(app: &mut App) {
        handle_mouse(
            app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            },
        );
    }

    /// AltGr, as the Windows console reports it.
    fn press_altgr(app: &mut App, c: char) {
        press(app, KeyCode::Char(c), KeyModifiers::CONTROL | KeyModifiers::ALT);
    }

    #[test]
    fn altgr_types_a_character_instead_of_running_a_chord() {
        // On an Italian layout AltGr is the only way to reach @ # [ ] { }, and the
        // console reports it as ctrl+alt. Treating that as ctrl made those keys
        // do nothing at all.
        let mut a = app();
        for c in ['@', '#', '[', ']', '{', '}'] {
            press_altgr(&mut a, c);
        }
        assert_eq!(a.ed.text(), "@#[]{}");
    }

    #[test]
    fn altgr_still_reaches_a_modal() {
        let mut a = app();
        a.ed.set("/theme");
        press_enter(&mut a); // opens the picker
        assert!(a.overlay_open());
        press_altgr(&mut a, '@');
        // The picker's search took it; the prompt behind it is untouched.
        assert!(!a.ed.text().contains('@'));
    }

    #[test]
    fn a_real_ctrl_chord_is_not_mistaken_for_altgr() {
        let mut a = app();
        a.ed.set("da buttare");
        press(&mut a, KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert!(a.ed.is_empty(), "^u must still clear the prompt");
    }

    /// The key records a Windows console produces for a paste: press+release per
    /// character, newlines as enter, and AltGr on anything that needs it on the
    /// active layout — `{ } [ ] @ #` on an Italian keyboard.
    fn console_paste(text: &str) -> Vec<TermEvent> {
        let mut out = Vec::new();
        for c in text.chars() {
            let code = if c == '\n' { KeyCode::Enter } else { KeyCode::Char(c) };
            let mods = if "{}[]@#".contains(c) {
                KeyModifiers::CONTROL | KeyModifiers::ALT
            } else {
                KeyModifiers::NONE
            };
            let mut down = KeyEvent::new(code, mods);
            down.kind = KeyEventKind::Press;
            let mut up = down;
            up.kind = KeyEventKind::Release;
            out.push(TermEvent::Key(down));
            out.push(TermEvent::Key(up));
        }
        out
    }

    /// Feed one console-delivered chunk through the same decision `poll` makes,
    /// without needing a real terminal to read from.
    fn feed(app: &mut App, chunk: &str) {
        take_batch(app, console_paste(chunk));
    }

    /// Whether a prompt went out: sending echoes it into the transcript.
    fn sent(app: &App) -> bool {
        app.blocks.iter().any(|b| matches!(b, crate::ui::block::Block::User(_)))
    }

    fn pointer_moved() -> TermEvent {
        TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 10,
            row: 5,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Input goes quiet for long enough that even a paste is over.
    fn idle(app: &mut App) {
        if let Some((_, last)) = app.key_buf.as_mut() {
            *last -= PASTE_QUIET;
        }
        flush_keys(app);
    }

    /// One empty tick — settles typing, not a paste.
    fn stall(app: &mut App) {
        flush_keys(app);
    }

    #[test]
    fn a_pasted_brace_does_not_split_the_paste() {
        // On an Italian layout the console encodes `{ } [ ]` as AltGr keystrokes.
        // Reading those as chords broke a pasted JSON blob at every brace: the
        // text before became a chip, the brace was typed, and the rest started a
        // new chip — `[Pasted 33 lines #23]{ (Invoke-RestMethod`.
        let mut a = app();
        let json = "{\"job_id\": \"abc\", \"tools\": [\"bash\", \"read_file\"]}";
        feed(&mut a, json);
        idle(&mut a);
        assert_eq!(a.ed.text(), json);
        assert_eq!(a.pastes.expand(a.ed.text()), json, "no chip should have been made");
    }

    #[test]
    fn a_long_pasted_blob_full_of_braces_is_one_chip() {
        let mut a = app();
        let body = "  {\"riga\": [1, 2]},\n".repeat(30);
        feed(&mut a, &body);
        idle(&mut a);
        assert_eq!(a.ed.text(), "[Pasted 30 lines]", "got {:?}", a.ed.text());
        assert_eq!(a.pastes.expand(a.ed.text()), body);
    }

    #[test]
    fn a_stall_in_the_middle_of_a_paste_does_not_end_it() {
        // The console fills its input buffer in pieces. Ending the paste at the
        // first empty tick split one ^v into `Edit d:\P` plus a separate chip.
        let mut a = app();
        feed(&mut a, "Edit d:\\work\\lele-harness\ne poi");
        stall(&mut a);
        assert!(a.key_buf.is_some(), "the paste was cut short by a stall");
        feed(&mut a, " il resto");
        idle(&mut a);
        assert_eq!(a.ed.text(), "Edit d:\\work\\lele-harness\ne poi il resto");
    }

    #[test]
    fn a_pastes_first_chunk_is_not_typed_out_ahead_of_it() {
        // The console can deliver the first character or three on their own —
        // too little to look like a paste. Acting on them immediately produced
        // `S[Pasted 11 lines]` and `OUT[Pasted 52 lines]`: the head of the paste
        // stranded outside its own chip.
        let mut a = app();
        feed(&mut a, "OUT");
        feed(&mut a, &"\nriga di output".repeat(20));
        idle(&mut a);
        assert_eq!(a.ed.text(), "[Pasted 21 lines]", "got {:?}", a.ed.text());
        assert!(a.pastes.expand(a.ed.text()).starts_with("OUT\nriga"));
    }

    #[test]
    fn a_pointer_moving_during_a_paste_does_not_send_each_line() {
        // Mouse moves landing between pasted characters made the batch "not
        // text", so every newline ran as enter: one paste, one message per line.
        let mut a = app();
        let mut batch = console_paste("prima riga\n\nseconda");
        batch.insert(6, pointer_moved());
        batch.push(pointer_moved());
        take_batch(&mut a, batch);
        idle(&mut a);
        assert_eq!(a.ed.text(), "prima riga\n\nseconda", "the paste was submitted line by line");
        assert!(!sent(&a), "nothing may have been sent");
    }

    #[test]
    fn a_drain_of_only_pointer_moves_does_not_cut_a_paste() {
        // Between two console chunks of one paste, a drain can hold nothing but
        // mouse moves. Treating it as "not text" flushed the first half, and the
        // second half — ending in a newline — was typed out and sent.
        let mut a = app();
        feed(&mut a, "uno\ndue\ntre");
        take_batch(&mut a, vec![pointer_moved(), pointer_moved()]);
        assert!(a.key_buf.is_some(), "the paste was flushed by a mouse move");
        feed(&mut a, " quattro\n");
        idle(&mut a);
        assert_eq!(a.ed.text(), "uno\ndue\ntre quattro\n");
        assert!(!sent(&a), "nothing may have been sent");
    }

    #[test]
    fn typing_still_settles_while_the_pointer_keeps_moving() {
        // Moves arriving every tick mean the input channel never times out;
        // held typing must still appear rather than wait for the mouse to stop.
        let mut a = app();
        feed(&mut a, "c");
        take_batch(&mut a, vec![pointer_moved()]);
        assert_eq!(a.ed.text(), "c");
    }

    /// The key records ConPTY writes for a paste (herdr, or any terminal that
    /// feeds a pseudo-console): CR as enter, LF as ctrl+enter. Captured from a
    /// real herdr paste in the trace.
    fn conpty_paste(text: &str) -> Vec<TermEvent> {
        text.chars()
            .map(|c| {
                let (code, mods) = match c {
                    '\r' => (KeyCode::Enter, KeyModifiers::NONE),
                    '\n' => (KeyCode::Enter, KeyModifiers::CONTROL),
                    c => (KeyCode::Char(c), KeyModifiers::NONE),
                };
                TermEvent::Key(KeyEvent::new(code, mods))
            })
            .collect()
    }

    #[test]
    fn a_crlf_paste_through_conpty_is_one_prompt_not_one_message_per_line() {
        // Each LF came as ctrl+enter, was taken for a chord, and threw out the
        // batch — so the CR next to it ran as enter and sent the line.
        let mut a = app();
        take_batch(&mut a, conpty_paste("uno\r\n\r\ndue\r\ntre"));
        idle(&mut a);
        assert_eq!(a.ed.text(), "uno\n\ndue\ntre");
        assert!(!sent(&a), "nothing may have been sent");
    }

    #[test]
    fn an_lf_only_paste_through_conpty_keeps_its_line_breaks() {
        // With LF alone the lines were not sent, but they were glued together:
        // `provariga` instead of `prova` / `riga`.
        let mut a = app();
        take_batch(&mut a, conpty_paste("prova\n\nriga due\nultima"));
        idle(&mut a);
        assert_eq!(a.ed.text(), "prova\n\nriga due\nultima");
        assert!(!sent(&a));
    }

    #[test]
    fn a_typed_ctrl_enter_is_a_newline_not_a_send() {
        let mut a = app();
        feed(&mut a, "ciao");
        stall(&mut a);
        take_batch(&mut a, conpty_paste("\n"));
        stall(&mut a);
        assert_eq!(a.ed.text(), "ciao\n");
        assert!(!sent(&a));
    }

    #[test]
    fn a_bracketed_paste_keeps_its_lines_whatever_their_endings() {
        let mut a = app();
        take_batch(&mut a, vec![TermEvent::Paste("uno\r\ndue\rtre\nquattro".into())]);
        assert_eq!(a.ed.text(), "uno\ndue\ntre\nquattro");
        assert!(!sent(&a), "a paste never submits");
    }

    #[test]
    fn a_single_typed_character_still_appears_on_the_next_tick() {
        // The cost of holding input back is one frame. Anything more than that
        // and the prompt would feel laggy.
        let mut a = app();
        feed(&mut a, "c");
        assert_eq!(a.ed.text(), "", "not acted on yet");
        stall(&mut a);
        assert_eq!(a.ed.text(), "c");
    }

    #[test]
    fn a_typed_enter_still_sends_after_one_tick() {
        let mut a = app();
        feed(&mut a, "domanda");
        stall(&mut a);
        feed(&mut a, "\n");
        stall(&mut a);
        assert!(a.ed.is_empty(), "enter no longer sends");
    }

    #[test]
    fn a_console_paste_is_text_not_a_dozen_enters() {
        // The Windows console has no bracketed paste, so a multi-line paste used
        // to submit one message per line.
        let batch = console_paste("prima riga\nseconda riga\nterza");
        let text = printable_text(&batch).unwrap();
        assert_eq!(text, "prima riga\rseconda riga\rterza", "enter is held as CR");
        assert!(looks_pasted(&text));
    }

    #[test]
    fn typing_is_never_mistaken_for_a_paste() {
        // A person produces one or two events per 30ms drain, not eight.
        assert!(!looks_pasted(&printable_text(&console_paste("ciao")).unwrap()));
        // and pressing enter at the end of a word is typing, not a paste
        assert!(!looks_pasted(&printable_text(&console_paste("ciao\n")).unwrap()));
    }

    #[test]
    fn a_newline_with_text_behind_it_is_always_a_paste() {
        // Nobody presses enter and types the next character within 30ms. This is
        // what catches the short chunks a big paste is split into.
        assert!(looks_pasted("\n\n"));
        assert!(looks_pasted("a\nb"));
        assert!(!looks_pasted("ab\n"));
    }

    #[test]
    fn a_burst_containing_a_chord_is_not_a_paste() {
        // ^c in the middle means these were real keystrokes; treating them as text
        // would swallow the cancel.
        let mut batch = console_paste("abcdefghij");
        batch.insert(6, TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(printable_text(&batch).is_none());
    }

    #[test]
    fn a_burst_containing_an_arrow_is_not_a_paste() {
        let mut batch = console_paste("abcdefghij");
        batch.push(TermEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)));
        assert!(printable_text(&batch).is_none());
    }

    #[test]
    fn shift_enter_still_inserts_a_newline() {
        // Buffering printable input replayed a held enter without its modifier,
        // so shift+enter submitted the message instead of breaking the line.
        let mut a = app();
        feed(&mut a, "prima riga");
        idle(&mut a);
        press(&mut a, KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(a.ed.text(), "prima riga\n", "shift+enter sent the message");
        assert!(!a.ed.is_empty());
    }

    #[test]
    fn a_modified_enter_is_never_taken_for_pasted_text() {
        let shift_enter = vec![TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT))];
        assert!(printable_text(&shift_enter).is_none());
        // A plain one still is — that is what a pasted line break arrives as.
        let plain = vec![TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))];
        assert_eq!(printable_text(&plain).as_deref(), Some("\r"));
    }

    #[test]
    fn a_capital_letter_is_still_ordinary_text() {
        // Shift disqualifies enter, not characters.
        let batch = console_paste("Ciao");
        let shifted: Vec<TermEvent> = batch
            .into_iter()
            .map(|e| match e {
                TermEvent::Key(mut k) => {
                    k.modifiers = KeyModifiers::SHIFT;
                    TermEvent::Key(k)
                }
                other => other,
            })
            .collect();
        assert_eq!(printable_text(&shifted).as_deref(), Some("Ciao"));
    }

    #[test]
    fn a_pasted_image_path_never_reaches_the_prompt_as_text() {
        let mut a = app();
        paste(&mut a, r"C:\Users\me\schermata.png");
        assert_eq!(a.ed.text(), "", "the path was typed instead of attached");
    }

    #[test]
    fn a_pasted_sentence_is_still_a_sentence() {
        let mut a = app();
        paste(&mut a, "guarda questo e dimmi");
        assert_eq!(a.ed.text(), "guarda questo e dimmi");
    }

    #[test]
    fn a_pasted_burst_lands_in_the_prompt_without_sending() {
        let mut a = app();
        feed(&mut a, "una riga\nun'altra riga\n");
        idle(&mut a);
        assert_eq!(a.ed.text(), "una riga\nun'altra riga\n");
        assert!(
            a.blocks.iter().all(|b| !matches!(b, crate::ui::block::Block::User(_))),
            "the paste was submitted instead of typed"
        );
    }

    #[test]
    fn a_paste_split_across_drains_stays_one_message() {
        // The console feeds a big paste in chunks, and the short ones — a blank
        // line between two paragraphs — used to fall through as enters. That is
        // how one ^v arrived as four separate messages.
        let mut a = app();
        for chunk in ["primo paragrafo", "\n\n", "secondo paragrafo", "\n\n", "terzo"] {
            feed(&mut a, chunk);
        }
        idle(&mut a);
        assert_eq!(a.ed.text(), "primo paragrafo\n\nsecondo paragrafo\n\nterzo");
        assert_eq!(
            a.blocks.iter().filter(|b| matches!(b, crate::ui::block::Block::User(_))).count(),
            0,
            "the paste was sent in pieces"
        );
    }

    #[test]
    fn a_split_paste_becomes_one_chip_not_several() {
        let mut a = app();
        // Two chunks that would each collapse on their own.
        feed(&mut a, &"riga\n".repeat(20));
        feed(&mut a, &"riga\n".repeat(20));
        idle(&mut a);
        assert_eq!(a.ed.text(), "[Pasted 40 lines]", "got {:?}", a.ed.text());
        assert_eq!(a.pastes.expand(a.ed.text()), "riga\n".repeat(40));
    }

    #[test]
    fn typing_right_after_a_paste_is_still_typing() {
        let mut a = app();
        feed(&mut a, "contenuto incollato\n");
        idle(&mut a); // the paste ends here
        feed(&mut a, "x");
        stall(&mut a); // a typed character settles on the next tick
        assert_eq!(a.ed.text(), "contenuto incollato\nx");
    }

    #[test]
    fn a_keystroke_that_cannot_be_part_of_a_paste_flushes_it_first() {
        // Otherwise the pending text would land *after* whatever the key did.
        let mut a = app();
        feed(&mut a, "testo incollato");
        assert!(a.key_buf.is_some());
        let batch = vec![TermEvent::Key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE))];
        assert!(printable_text(&batch).is_none());
        force_flush(&mut a); // what poll does before acting on such a batch
        assert_eq!(a.ed.text(), "testo incollato");
        assert!(a.key_buf.is_none());
    }

    #[test]
    fn enter_after_a_paste_has_settled_sends_it() {
        // The paste must still be sendable — the fix is about *when*, not whether.
        let mut a = app();
        feed(&mut a, "domanda su due\nrighe");
        idle(&mut a);
        press_enter(&mut a);
        assert!(a.ed.is_empty(), "enter no longer sends");
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> TermEvent {
        TermEvent::Mouse(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE })
    }

    #[test]
    fn a_drag_is_only_applied_where_it_ended() {
        // A capture-mode drag is one event per cell crossed. Replaying every one
        // of them just moves the selection to a position already superseded.
        let (tx, rx) = std::sync::mpsc::channel();
        let mut a = app();
        a.transcript_rect = ratatui::layout::Rect::new(0, 0, 40, 10);
        tx.send(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0)).unwrap();
        for col in 1..30 {
            tx.send(mouse(MouseEventKind::Drag(MouseButton::Left), col, 0)).unwrap();
        }
        poll(&mut a, &rx, Duration::from_millis(20)).unwrap();

        let sel = a.selection.expect("no selection");
        assert!(!sel.is_empty());
        // The last drag still has to be the one that counts.
        assert_eq!(sel.head, (0, 29), "the selection stopped short of the last drag");
        assert_eq!(sel.anchor, (0, 0));
    }

    #[test]
    fn a_release_in_the_same_batch_still_sees_the_drag() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut a = app();
        a.transcript_rect = ratatui::layout::Rect::new(0, 0, 40, 10);
        tx.send(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0)).unwrap();
        tx.send(mouse(MouseEventKind::Drag(MouseButton::Left), 10, 0)).unwrap();
        tx.send(mouse(MouseEventKind::Up(MouseButton::Left), 10, 0)).unwrap();
        poll(&mut a, &rx, Duration::from_millis(20)).unwrap();
        // A drag applied after the release would have copied nothing at all.
        assert_eq!(a.selection.map(|s| s.head), Some((0, 10)));
    }

    #[test]
    fn a_quiet_tick_returns_rather_than_waiting_on_the_terminal() {
        // The render thread must never sit in a terminal read: crossterm's `read`
        // blocks whenever the pending console records all get filtered out, which
        // is what a mouse moving inside one cell produces.
        let (_tx, rx) = std::sync::mpsc::channel();
        let mut a = app();
        let t = std::time::Instant::now();
        poll(&mut a, &rx, Duration::from_millis(20)).unwrap();
        assert!(t.elapsed() < Duration::from_millis(500), "poll waited {:?}", t.elapsed());
        assert!(!a.should_quit);
    }

    #[test]
    fn a_dead_reader_quits_instead_of_spinning_forever() {
        // No key can ever arrive again, including the one that would quit.
        let (tx, rx) = std::sync::mpsc::channel::<TermEvent>();
        drop(tx);
        let mut a = app();
        poll(&mut a, &rx, Duration::from_millis(20)).unwrap();
        assert!(a.should_quit);
    }

    #[test]
    fn everything_already_queued_is_taken_in_one_pass() {
        let (tx, rx) = std::sync::mpsc::channel();
        for c in "ciao".chars() {
            tx.send(TermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))).unwrap();
        }
        let mut a = app();
        poll(&mut a, &rx, Duration::from_millis(20)).unwrap();
        // Held for a tick, as all printable input is, then settled.
        idle(&mut a);
        assert_eq!(a.ed.text(), "ciao");
    }

    #[test]
    fn key_release_is_ignored() {
        // Windows sends press+release; acting on both would double every letter.
        let mut a = app();
        let mut k = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        handle_key(&mut a, k);
        assert!(a.ed.is_empty());
    }

    #[test]
    fn typing_reaches_the_editor() {
        let mut a = app();
        press(&mut a, KeyCode::Char('h'), KeyModifiers::NONE);
        press(&mut a, KeyCode::Char('i'), KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "hi");
    }

    #[test]
    fn ctrl_c_clears_the_input_before_it_will_quit() {
        let mut a = app();
        a.ed.set("mezza frase");
        press(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.ed.is_empty(), "first press should clear");
        assert!(!a.should_quit, "and must not quit with text in the prompt");

        press(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.should_quit, "second press on an empty prompt quits");
    }

    #[test]
    fn a_second_ctrl_c_while_the_turn_is_stopping_quits() {
        let mut a = app();
        let (_tx, rx) = std::sync::mpsc::channel();
        a.turn = Some(crate::app::run::Turn { job_id: "job".into(), events: rx, stopping: false });

        press(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.cancelling(), "the first press asks the turn to stop");
        assert!(!a.should_quit, "and does not leave yet");

        press(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.should_quit, "a stop that has not landed must not trap the user");
    }

    #[test]
    fn ctrl_c_quits_on_an_already_empty_prompt() {
        let mut a = app();
        assert!(a.ed.is_empty());
        press(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(a.should_quit);
    }

    #[test]
    fn shift_enter_inserts_a_newline_instead_of_sending() {
        let mut a = app();
        a.ed.set("riga");
        press(&mut a, KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(a.ed.text(), "riga\n");
        assert!(!a.should_quit);
    }

    #[test]
    fn enter_completes_the_menu_rather_than_sending() {
        let mut a = app();
        a.ed.set("/the");
        press(&mut a, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "/theme ");
    }

    #[test]
    fn escape_clears_the_input_when_idle() {
        let mut a = app();
        a.ed.set("scarta");
        press(&mut a, KeyCode::Esc, KeyModifiers::NONE);
        assert!(a.ed.is_empty());
    }

    #[test]
    fn ctrl_o_toggles_expansion() {
        let mut a = app();
        assert!(!a.expand_all);
        press(&mut a, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(a.expand_all);
    }

    #[test]
    fn ctrl_arrows_scroll_a_line_at_a_time() {
        let mut a = app();
        a.follow = true;
        press(&mut a, KeyCode::Up, KeyModifiers::CONTROL);
        assert!(!a.follow, "scrolling up should detach from the tail");
        press(&mut a, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert!(a.follow);
    }

    #[test]
    fn the_bare_up_arrow_recalls_history_instead_of_scrolling() {
        let mut a = app();
        a.follow = true;
        a.hist = crate::app::history::History::ephemeral();
        a.hist.push("comando precedente");

        press(&mut a, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "comando precedente");
        assert!(a.follow, "recall must not move the transcript");
    }

    #[test]
    fn down_walks_back_to_the_draft_that_was_being_typed() {
        let mut a = app();
        a.hist = crate::app::history::History::ephemeral();
        a.hist.push("vecchio");
        a.ed.set("mezza frase");

        press(&mut a, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "vecchio");
        press(&mut a, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "mezza frase", "the draft must come back");
    }

    #[test]
    fn typing_after_a_recall_makes_the_text_your_own_draft() {
        // Otherwise the next up-arrow would jump the history instead of moving
        // the caret in what is now an edited line.
        let mut a = app();
        a.hist = crate::app::history::History::ephemeral();
        a.hist.push("uno");
        a.hist.push("due");
        press(&mut a, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "due");
        press(&mut a, KeyCode::Char('!'), KeyModifiers::NONE);
        assert!(!a.hist.browsing());
    }

    #[test]
    fn the_up_arrow_moves_the_caret_inside_a_multiline_prompt() {
        let mut a = app();
        a.hist = crate::app::history::History::ephemeral();
        a.hist.push("storia");
        a.ed.set("prima\nseconda");
        press(&mut a, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "prima\nseconda", "history stole a caret move");
        // and from the first line it does reach the history
        press(&mut a, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "storia");
    }

    #[test]
    fn an_empty_history_leaves_the_prompt_alone() {
        let mut a = app();
        a.hist = crate::app::history::History::ephemeral();
        a.ed.set("bozza");
        press(&mut a, KeyCode::Up, KeyModifiers::NONE);
        press(&mut a, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "bozza");
    }

    #[test]
    fn a_big_paste_becomes_a_chip_and_a_small_one_does_not() {
        let mut a = app();
        paste_text(&mut a, "due\nrighe");
        assert_eq!(a.ed.text(), "due\nrighe");
        a.ed.clear();

        let big = "riga\n".repeat(40);
        paste_text(&mut a, &big);
        assert_eq!(a.ed.text(), "[Pasted 40 lines]");
        // and the model still gets the whole thing
        assert_eq!(a.pastes.expand(a.ed.text()), big);
    }

    #[test]
    fn backspace_removes_a_chip_whole() {
        // One character off the label would orphan the paste: it would no longer
        // match, so the content would silently stop being sent.
        let mut a = app();
        paste_text(&mut a, &"riga\n".repeat(40));
        a.ed.insert_str(" spiega");
        press(&mut a, KeyCode::Backspace, KeyModifiers::NONE);
        for _ in 0..6 {
            press(&mut a, KeyCode::Backspace, KeyModifiers::NONE);
        }
        assert_eq!(a.ed.text(), "[Pasted 40 lines]");
        press(&mut a, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "", "the chip should go in one press");
    }

    #[test]
    fn a_modal_takes_the_keys_the_prompt_would_have_had() {
        let mut a = app();
        a.open_effort_picker();
        press(&mut a, KeyCode::Char('h'), KeyModifiers::NONE);
        assert!(a.ed.is_empty(), "the modal's search took the keystroke, not the prompt");
        press(&mut a, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!a.overlay_open());
        press(&mut a, KeyCode::Char('h'), KeyModifiers::NONE);
        assert_eq!(a.ed.text(), "h");
    }

    #[test]
    fn enter_in_a_picker_applies_the_choice() {
        let mut a = app();
        a.open_effort_picker();
        for c in "high".chars() {
            press(&mut a, KeyCode::Char(c), KeyModifiers::NONE);
        }
        press(&mut a, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!a.overlay_open());
        assert_eq!(a.status.effort, "high");
    }

    #[test]
    fn ctrl_c_closes_a_modal_rather_than_quitting() {
        let mut a = app();
        a.open_effort_picker();
        press(&mut a, KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!a.overlay_open());
        assert!(!a.should_quit, "^c out of a modal must not also exit");
    }

    #[test]
    fn arrows_move_the_menu_selection_when_it_is_open() {
        let mut a = app();
        a.ed.set("/");
        press(&mut a, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(a.menu_sel, 1);
        press(&mut a, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(a.menu_sel, 0);
        // and never past the end
        for _ in 0..50 {
            press(&mut a, KeyCode::Down, KeyModifiers::NONE);
        }
        assert_eq!(a.menu_sel, a.menu_rows().len() - 1);
    }

    #[test]
    fn typing_resets_the_menu_selection() {
        let mut a = app();
        a.ed.set("/");
        press(&mut a, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(a.menu_sel, 1);
        press(&mut a, KeyCode::Char('t'), KeyModifiers::NONE);
        assert_eq!(a.menu_sel, 0, "stale selection would pick the wrong command");
    }
}
