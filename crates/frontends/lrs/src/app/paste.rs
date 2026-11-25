//! Chips: the short stand-ins the prompt shows for things too big to display.
//!
//! A 400-line paste in the prompt buries everything else, and an image has no text
//! form at all. Both become a one-line marker — `[Pasted 400 lines]`, `[Image #1]` —
//! that the user can edit around and delete as a unit, while the full content is
//! held here and put back only when the turn is sent.
//!
//! The transcript echoes the chip, not the expansion: seeing your own 400-line
//! paste replayed above the answer is not useful. The model always gets the whole
//! thing.

/// A paste longer than either of these collapses. Same thresholds the Textual TUI
/// uses, so the two frontends behave alike.
const COLLAPSE_LINES: usize = 10;
const COLLAPSE_CHARS: usize = 800;

/// Opening of an image chip. One definition, because the chips are also found
/// by scanning for it — see [`Pastes::without_dead_images`].
const IMAGE_CHIP: &str = "[Image #";

#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    Text(String),
    /// A `data:image/png;base64,…` URL.
    Image(String),
}

#[derive(Debug, Default)]
pub struct Pastes {
    /// Insertion order matters: images are sent in the order they were pasted.
    items: Vec<(String, Content)>,
    seq: usize,
}

impl Pastes {
    /// Should this paste collapse, or is it small enough to show as itself?
    pub fn should_collapse(text: &str) -> bool {
        text.lines().count() > COLLAPSE_LINES || text.chars().count() > COLLAPSE_CHARS
    }

    /// Register a pasted blob and return the chip to put in the prompt.
    pub fn add_text(&mut self, text: &str) -> String {
        let n = text.lines().count();
        let label = if n > 1 {
            format!("{n} lines")
        } else {
            format!("{} chars", text.chars().count())
        };
        let chip = self.unique(&format!("[Pasted {label}]"));
        self.items.push((chip.clone(), Content::Text(text.to_string())));
        chip
    }

    pub fn add_image(&mut self, data_url: String) -> String {
        self.seq += 1;
        let chip = format!("{IMAGE_CHIP}{}]", self.seq);
        self.items.push((chip.clone(), Content::Image(data_url)));
        chip
    }

    /// Two identical pastes must not share one chip, or deleting either would
    /// orphan the other's content.
    fn unique(&mut self, base: &str) -> String {
        if !self.items.iter().any(|(c, _)| c == base) {
            return base.to_string();
        }
        self.seq += 1;
        format!("{} #{}]", base.trim_end_matches(']'), self.seq)
    }

    /// Put the pasted text back. Image chips are left as they are: they read fine
    /// in place ("guarda [Image #1]") and the picture travels as its own part.
    pub fn expand(&self, prompt: &str) -> String {
        let mut out = prompt.to_string();
        for (chip, content) in &self.items {
            if let Content::Text(full) = content {
                out = out.replace(chip.as_str(), full);
            }
        }
        out
    }

    /// The same prompt with each text chip opened just far enough to recognise.
    ///
    /// The prompt keeps the chip — it has to stay one token you can delete whole —
    /// but the transcript is the record of what was sent, and `[Pasted 52 lines]`
    /// does not tell you whether those were the right 52 lines. Replaying all of
    /// them is no better: the answer would be pushed off the screen by your own
    /// input. The first few lines settle it either way.
    pub fn preview(&self, prompt: &str) -> String {
        let mut out = prompt.to_string();
        for (chip, content) in &self.items {
            if let Content::Text(full) = content {
                out = out.replace(chip.as_str(), &glimpse(full));
            }
        }
        out
    }

    /// Data URLs for the image chips still present in `prompt`, in paste order.
    /// A chip the user deleted takes its image with it.
    pub fn images_in(&self, prompt: &str) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|(chip, content)| match content {
                Content::Image(url) if prompt.contains(chip.as_str()) => Some(url.clone()),
                _ => None,
            })
            .collect()
    }

    /// The prompt with any image chip nothing is holding a picture for removed,
    /// and how many were dropped.
    ///
    /// The pictures are released when the turn they belonged to starts, so a
    /// prompt recalled from the history still reads `[Image #1]` with nothing
    /// behind it. Left in, that literal text is what the model would receive —
    /// a turn that looks like it carries a screenshot and doesn't.
    pub fn without_dead_images(&self, prompt: &str) -> (String, usize) {
        let mut out = String::with_capacity(prompt.len());
        let mut rest = prompt;
        let mut dropped = 0;
        while let Some(start) = rest.find(IMAGE_CHIP) {
            let Some(len) = rest[start..].find(']').map(|i| i + 1) else { break };
            let chip = &rest[start..start + len];
            out.push_str(&rest[..start]);
            if self.items.iter().any(|(c, _)| c == chip) {
                out.push_str(chip); // a live one: the picture is still held
            } else {
                dropped += 1;
            }
            rest = &rest[start + len..];
        }
        out.push_str(rest);
        (out, dropped)
    }

    /// The chip ending exactly at `cursor`, so backspace can delete it whole
    /// rather than shaving one character off a label the user can't repair.
    pub fn chip_ending_at<'a>(&'a self, text: &str, cursor: usize) -> Option<&'a str> {
        let head = text.get(..cursor)?;
        self.items
            .iter()
            .map(|(c, _)| c.as_str())
            .find(|c| head.ends_with(c))
    }

    /// Drop anything no longer mentioned in the prompt. Called after a send, so a
    /// long session doesn't accumulate every blob ever pasted.
    pub fn retain_used(&mut self, prompt: &str) {
        self.items.retain(|(chip, _)| prompt.contains(chip.as_str()));
        if self.items.is_empty() {
            self.seq = 0;
        }
    }
}

/// How much of a pasted blob the transcript shows. Three lines is enough to tell
/// the right paste from the wrong one; the character cap is for the blob that is
/// all one 5000-character line.
const PREVIEW_LINES: usize = 3;
const PREVIEW_CHARS: usize = 240;

/// The opening of `text`, with what was left out accounted for.
fn glimpse(text: &str) -> String {
    let total = text.lines().count();
    let head: Vec<&str> = text.lines().take(PREVIEW_LINES).collect();
    let hidden = total.saturating_sub(head.len());
    let mut body = head.join("\n");
    let clipped = body.chars().count() > PREVIEW_CHARS;
    if clipped {
        body = body.chars().take(PREVIEW_CHARS).collect();
    }
    match (hidden, clipped) {
        (0, false) => body,
        // Never a silent cut: the count is the whole point of showing a preview
        // rather than the text.
        (0, true) => format!("{body}…"),
        (n, _) => format!("{body}\n… +{n} more lines"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_paste_is_left_alone() {
        assert!(!Pastes::should_collapse("una riga"));
        assert!(!Pastes::should_collapse(&"x".repeat(COLLAPSE_CHARS)));
    }

    #[test]
    fn a_long_paste_collapses_by_lines_or_by_size() {
        assert!(Pastes::should_collapse(&"riga\n".repeat(COLLAPSE_LINES + 1)));
        assert!(Pastes::should_collapse(&"x".repeat(COLLAPSE_CHARS + 1)));
    }

    #[test]
    fn the_chip_says_how_much_was_pasted() {
        let mut p = Pastes::default();
        assert_eq!(p.add_text("a\nb\nc"), "[Pasted 3 lines]");
        let mut p = Pastes::default();
        assert_eq!(p.add_text("abcd"), "[Pasted 4 chars]");
    }

    #[test]
    fn the_model_gets_the_whole_paste_back() {
        let mut p = Pastes::default();
        let body = "riga\n".repeat(20);
        let chip = p.add_text(&body);
        let expanded = p.expand(&format!("spiega questo: {chip} grazie"));
        assert_eq!(expanded, format!("spiega questo: {body} grazie"));
    }

    #[test]
    fn two_identical_pastes_keep_separate_chips() {
        // One shared chip would make deleting either drop both blobs.
        let mut p = Pastes::default();
        let a = p.add_text("a\nb\nc");
        let b = p.add_text("a\nb\nc");
        assert_ne!(a, b, "both pastes collapsed onto the same chip");
        assert_eq!(p.expand(&format!("{a}|{b}")), "a\nb\nc|a\nb\nc");
    }

    #[test]
    fn deleting_a_chip_drops_its_content_from_the_turn() {
        let mut p = Pastes::default();
        let a = p.add_text("primo\n".repeat(20).as_str());
        let b = p.add_text("secondo\n".repeat(20).as_str());
        // The user backspaced `a` away before sending.
        let prompt = format!("solo {b}");
        assert!(!p.expand(&prompt).contains("primo"));
        assert!(p.expand(&prompt).contains("secondo"));
        assert_eq!(a.is_empty(), false);
    }

    #[test]
    fn image_chips_stay_in_the_text_and_travel_as_parts() {
        let mut p = Pastes::default();
        let c1 = p.add_image("data:image/png;base64,AAA".into());
        let c2 = p.add_image("data:image/png;base64,BBB".into());
        assert_eq!((c1.as_str(), c2.as_str()), ("[Image #1]", "[Image #2]"));
        let prompt = format!("confronta {c1} e {c2}");
        // The label reads fine in place; the model gets the pictures separately.
        assert_eq!(p.expand(&prompt), prompt);
        assert_eq!(
            p.images_in(&prompt),
            vec!["data:image/png;base64,AAA", "data:image/png;base64,BBB"]
        );
    }

    #[test]
    fn a_deleted_image_chip_is_not_sent() {
        let mut p = Pastes::default();
        let c1 = p.add_image("A".into());
        let _c2 = p.add_image("B".into());
        assert_eq!(p.images_in(&format!("solo {c1}")), vec!["A"]);
    }

    #[test]
    fn images_are_sent_in_the_order_they_were_pasted() {
        let mut p = Pastes::default();
        let a = p.add_image("A".into());
        let b = p.add_image("B".into());
        // Even when the prompt mentions them the other way round, the pairing with
        // the "#N" labels in the text is what has to hold.
        assert_eq!(p.images_in(&format!("{b} poi {a}")), vec!["A", "B"]);
    }

    #[test]
    fn backspace_finds_the_chip_that_ends_at_the_caret() {
        let mut p = Pastes::default();
        let chip = p.add_image("A".into());
        let text = format!("guarda {chip}");
        assert_eq!(p.chip_ending_at(&text, text.len()), Some(chip.as_str()));
        // one character earlier the caret is inside the chip, not after it
        assert_eq!(p.chip_ending_at(&text, text.len() - 1), None);
        assert_eq!(p.chip_ending_at(&text, 3), None);
    }

    #[test]
    fn chip_lookup_never_panics_on_a_multibyte_boundary() {
        let mut p = Pastes::default();
        p.add_image("A".into());
        // A cursor landing mid-codepoint must return None, not slice-panic.
        assert_eq!(p.chip_ending_at("però", 2), None);
    }

    #[test]
    fn the_transcript_shows_the_start_of_what_was_sent() {
        // `[Pasted 52 lines]` above the answer leaves you unable to tell whether
        // you sent the right 52 lines.
        let mut p = Pastes::default();
        let body = (1..=52).map(|i| format!("riga {i}")).collect::<Vec<_>>().join("\n");
        let chip = p.add_text(&body);
        assert_eq!(
            p.preview(&format!("spiega {chip}")),
            "spiega riga 1\nriga 2\nriga 3\n… +49 more lines"
        );
    }

    #[test]
    fn a_preview_never_hides_how_much_it_left_out() {
        assert_eq!(glimpse("a\nb\nc\nd"), "a\nb\nc\n… +1 more lines");
        let long = "x".repeat(PREVIEW_CHARS + 50);
        let seen = glimpse(&long);
        assert_eq!(seen.chars().count(), PREVIEW_CHARS + 1);
        assert!(seen.ends_with('…'));
    }

    #[test]
    fn a_paste_short_enough_to_read_is_previewed_whole() {
        assert_eq!(glimpse("una\ndue"), "una\ndue");
    }

    #[test]
    fn the_preview_leaves_image_chips_alone() {
        // They read fine in place and there is nothing to preview.
        let mut p = Pastes::default();
        let c = p.add_image("data:image/png;base64,AAA".into());
        assert_eq!(p.preview(&format!("guarda {c}")), format!("guarda {c}"));
    }

    #[test]
    fn a_live_image_chip_survives_the_dead_sweep() {
        let mut p = Pastes::default();
        let c = p.add_image("A".into());
        assert_eq!(p.without_dead_images(&format!("guarda {c}")), (format!("guarda {c}"), 0));
    }

    #[test]
    fn an_image_chip_with_no_picture_behind_it_is_removed() {
        // What the history hands back: the label, the picture long since released.
        let p = Pastes::default();
        assert_eq!(
            p.without_dead_images("confronta [Image #1] e [Image #2] grazie"),
            ("confronta  e  grazie".to_string(), 2)
        );
    }

    #[test]
    fn the_sweep_leaves_ordinary_brackets_alone() {
        let p = Pastes::default();
        let text = "array[0] e [TODO] e [Pasted 3 lines]";
        assert_eq!(p.without_dead_images(text), (text.to_string(), 0));
    }

    #[test]
    fn an_unterminated_chip_does_not_eat_the_rest_of_the_prompt() {
        let p = Pastes::default();
        let text = "scrivi [Image #1 senza chiusura";
        assert_eq!(p.without_dead_images(text), (text.to_string(), 0));
    }

    #[test]
    fn unused_blobs_are_released_after_a_send() {
        let mut p = Pastes::default();
        let a = p.add_text(&"x\n".repeat(20));
        p.add_text(&"y\n".repeat(20));
        p.retain_used(&format!("tieni {a}"));
        assert_eq!(p.items.len(), 1);
        p.retain_used("niente");
        assert!(p.items.is_empty());
        // and the numbering starts over once nothing is held
        assert_eq!(p.add_image("A".into()), "[Image #1]");
    }
}
