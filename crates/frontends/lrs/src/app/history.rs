//! Shell-style prompt history, one file per project.
//!
//! Per project rather than global on purpose: what you asked in one repo is rarely
//! what you want to re-send in another, and a single shared file turns the up-arrow
//! into a lottery once you work in more than one place.

use std::io::Write;
use std::path::{Path, PathBuf};

/// Entries kept per project. Old enough prompts stop being recall and start being
/// archaeology, and the whole file is read at startup.
const MAX_ENTRIES: usize = 500;

pub struct History {
    path: Option<PathBuf>,
    /// Oldest first, so pushing is an append on both sides.
    items: Vec<String>,
    /// Where the up-arrow currently is. `None` means "not browsing" — the prompt
    /// holds the user's live draft.
    idx: Option<usize>,
    /// The draft that was in the prompt when browsing started, restored when the
    /// user arrows back down past the newest entry.
    draft: String,
}

impl History {
    /// Load this project's history. A directory that can't be read yields an
    /// in-memory history rather than an error: losing recall is not worth a
    /// failed startup.
    pub fn load(cwd: &str) -> Self {
        let Some(path) = super::config::dir().map(|d| d.join("history").join(file_name(cwd)))
        else {
            return History::ephemeral();
        };
        let items = std::fs::read_to_string(&path)
            .map(|s| s.lines().filter(|l| !l.is_empty()).map(decode).collect())
            .unwrap_or_default();
        History { path: Some(path), items, idx: None, draft: String::new() }
    }

    /// Off-disk history — for a session with nowhere to write, and for tests.
    pub fn ephemeral() -> Self {
        History { path: None, items: Vec::new(), idx: None, draft: String::new() }
    }

    /// Record a submitted prompt and stop browsing.
    pub fn push(&mut self, entry: &str) {
        self.idx = None;
        self.draft.clear();
        let entry = entry.trim_end();
        if entry.is_empty() {
            return;
        }
        // Re-sending the same thing twice shouldn't need two up-arrows to get past.
        if self.items.last().map(String::as_str) == Some(entry) {
            return;
        }
        self.items.push(entry.to_string());
        if self.items.len() > MAX_ENTRIES {
            let excess = self.items.len() - MAX_ENTRIES;
            self.items.drain(..excess);
            self.rewrite();
            return;
        }
        self.append(entry);
    }

    /// True while the arrows own the prompt. Once browsing, they keep owning it
    /// until the user submits or arrows back to the draft — otherwise recalling a
    /// two-line prompt would trap the cursor inside it.
    pub fn browsing(&self) -> bool {
        self.idx.is_some()
    }

    /// The previous entry, or `None` at the oldest one. `current` is the prompt's
    /// text, kept so arrowing back down returns it untouched.
    pub fn prev(&mut self, current: &str) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let next = match self.idx {
            None => {
                self.draft = current.to_string();
                self.items.len() - 1
            }
            Some(0) => return None, // already at the oldest
            Some(i) => i - 1,
        };
        self.idx = Some(next);
        Some(self.items[next].clone())
    }

    /// The next entry, or the original draft once past the newest.
    pub fn next(&mut self) -> Option<String> {
        let i = self.idx?;
        if i + 1 < self.items.len() {
            self.idx = Some(i + 1);
            return Some(self.items[i + 1].clone());
        }
        self.idx = None;
        Some(std::mem::take(&mut self.draft))
    }

    /// Leave browsing without changing the prompt — after an edit, the recalled
    /// text is now the user's own draft.
    pub fn stop(&mut self) {
        self.idx = None;
    }

    /// Best-effort: a full disk or a locked file must not break the prompt,
    /// so a failure is traced and the in-memory history carries on.
    fn append(&mut self, entry: &str) {
        let Some(path) = self.path.clone() else { return };
        let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::OpenOptions::new().create(true).append(true).open(&path))
            .and_then(|mut f| writeln!(f, "{}", encode(entry)));
        if let Err(e) = saved {
            super::log::line(format_args!("prompt history not saved to {}: {e}", path.display()));
        }
    }

    fn rewrite(&self) {
        let Some(path) = self.path.as_deref() else { return };
        let body: String = self.items.iter().map(|e| encode(e) + "\n").collect();
        let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(path, body));
        if let Err(e) = saved {
            super::log::line(format_args!("prompt history not rewritten at {}: {e}", path.display()));
        }
    }
}

/// One entry per line, so a multi-line prompt has to survive as one. `\` and
/// newlines are escaped; nothing else is touched.
fn encode(entry: &str) -> String {
    entry.replace('\\', r"\\").replace('\n', r"\n").replace('\r', "")
}

fn decode(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            // A stray backslash was never written by `encode`; keep it verbatim
            // rather than silently eating the character after it.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A filename that is readable *and* unique. The directory's own name makes the
/// file recognisable; the hash keeps two projects with the same name apart.
fn file_name(cwd: &str) -> String {
    let base: String = Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".into())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("{base}-{:016x}.txt", fnv1a(cwd))
}

/// FNV-1a. A hash here only has to spread directory paths across filenames, so a
/// dependency for it would be all cost and no benefit.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(entries: &[&str]) -> History {
        let mut h = History::ephemeral();
        for e in entries {
            h.push(e);
        }
        h
    }

    #[test]
    fn up_walks_back_from_the_newest() {
        let mut h = seeded(&["uno", "due", "tre"]);
        assert_eq!(h.prev(""), Some("tre".into()));
        assert_eq!(h.prev("tre"), Some("due".into()));
        assert_eq!(h.prev("due"), Some("uno".into()));
        assert_eq!(h.prev("uno"), None, "must stop at the oldest, not wrap");
    }

    #[test]
    fn down_returns_the_draft_that_was_being_typed() {
        let mut h = seeded(&["uno", "due"]);
        assert_eq!(h.prev("mezza frase"), Some("due".into()));
        assert_eq!(h.prev("due"), Some("uno".into()));
        assert_eq!(h.next(), Some("due".into()));
        assert_eq!(h.next(), Some("mezza frase".into()), "the draft must come back intact");
        assert!(!h.browsing());
    }

    #[test]
    fn down_without_having_gone_up_does_nothing() {
        let mut h = seeded(&["uno"]);
        assert_eq!(h.next(), None);
    }

    #[test]
    fn an_empty_history_has_nothing_to_recall() {
        let mut h = History::ephemeral();
        assert_eq!(h.prev("x"), None);
        assert!(!h.browsing(), "and must not trap the arrows");
    }

    #[test]
    fn submitting_the_same_prompt_twice_stores_it_once() {
        let h = seeded(&["stessa", "stessa"]);
        assert_eq!(h.items.len(), 1);
    }

    #[test]
    fn blank_prompts_are_not_recorded() {
        let h = seeded(&["", "   \n"]);
        assert_eq!(h.items.len(), 0);
    }

    #[test]
    fn pushing_ends_a_browse() {
        let mut h = seeded(&["uno"]);
        h.prev("");
        assert!(h.browsing());
        h.push("due");
        assert!(!h.browsing());
        assert_eq!(h.prev(""), Some("due".into()));
    }

    #[test]
    fn a_multiline_prompt_round_trips_as_one_entry() {
        let entry = "prima riga\nseconda riga";
        assert_eq!(decode(&encode(entry)), entry);
        assert!(!encode(entry).contains('\n'), "it would become two entries");
    }

    #[test]
    fn backslashes_survive_the_escaping() {
        // A Windows path in a prompt is the common case, and `C:\new` must not
        // come back as a newline.
        for s in [r"C:\new\tab", r"a\\b", "trailing\\"] {
            assert_eq!(decode(&encode(s)), s, "round trip failed for {s:?}");
        }
    }

    #[test]
    fn two_projects_with_the_same_name_get_different_files() {
        let a = file_name(r"D:\work\api");
        let b = file_name(r"D:\personal\api");
        assert_ne!(a, b);
        assert!(a.starts_with("api-"), "the name should stay recognisable: {a}");
        assert!(a.ends_with(".txt"));
    }

    #[test]
    fn a_filename_never_carries_path_separators() {
        // Otherwise the "file" would be a nested directory that doesn't exist.
        let n = file_name(r"D:\a b\c.d");
        assert!(!n.contains(['/', '\\', ':']), "unsafe filename: {n}");
    }

    #[test]
    fn history_persists_across_a_reload() {
        let tmp = std::env::temp_dir().join(format!("lrs-hist-{:x}", fnv1a("persist")));
        let _ = std::fs::remove_dir_all(&tmp);
        let mut h = History {
            path: Some(tmp.join("history").join("p.txt")),
            items: Vec::new(),
            idx: None,
            draft: String::new(),
        };
        h.push("prima");
        h.push("seconda\ncon a capo");

        let items: Vec<String> = std::fs::read_to_string(tmp.join("history").join("p.txt"))
            .unwrap()
            .lines()
            .map(decode)
            .collect();
        assert_eq!(items, vec!["prima", "seconda\ncon a capo"]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn the_file_is_capped_and_keeps_the_newest() {
        let tmp = std::env::temp_dir().join(format!("lrs-hist-{:x}", fnv1a("cap")));
        let _ = std::fs::remove_dir_all(&tmp);
        let path = tmp.join("history").join("p.txt");
        let mut h = History { path: Some(path.clone()), items: Vec::new(), idx: None, draft: String::new() };
        for i in 0..MAX_ENTRIES + 10 {
            h.push(&format!("prompt {i}"));
        }
        assert_eq!(h.items.len(), MAX_ENTRIES);
        let lines: Vec<String> = std::fs::read_to_string(&path).unwrap().lines().map(decode).collect();
        assert_eq!(lines.len(), MAX_ENTRIES, "the file grew past the cap");
        assert_eq!(lines.last().unwrap(), &format!("prompt {}", MAX_ENTRIES + 9));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
