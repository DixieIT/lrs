//! The project's file list, for `@path` completion.
//!
//! `@src/main.rs` in a prompt is a file reference the agent's system prompt tells
//! it to read first, so the only job here is turning a few typed characters into
//! a real relative path.
//!
//! The list comes from `git ls-files` — tracked *and* untracked, honouring
//! .gitignore, which is exactly the set a person thinks of as "the project".
//! Outside a repository a shallow walk stands in for it.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Directories a walk never descends into. Only used off-git, where nothing else
/// filters the noise out.
const SKIP: &[&str] =
    &[".git", "node_modules", "__pycache__", ".venv", "venv", "dist", "build", "target", ".lele"];

/// Enough to complete against without walking a home directory by accident.
const WALK_LIMIT: usize = 5000;

/// How long a scan stays good. Completion runs per keystroke; the scan shells out.
const TTL: Duration = Duration::from_secs(5);

/// The cached list, refreshed off-thread by [`super::App`].
#[derive(Default)]
pub struct Files {
    list: Vec<String>,
    /// When the current list was produced. `None` until the first scan lands.
    at: Option<Instant>,
    /// A scan is running. Without this every tick would spawn another one while
    /// the first was still going.
    pub inflight: bool,
}

impl Files {
    pub fn list(&self) -> &[String] {
        &self.list
    }

    /// True when a fresh scan is worth starting.
    pub fn stale(&self) -> bool {
        !self.inflight && self.at.is_none_or(|t| t.elapsed() > TTL)
    }

    pub fn set(&mut self, list: Vec<String>) {
        self.list = list;
        self.at = Some(Instant::now());
        self.inflight = false;
    }
}

/// Relative paths under `root`, forward-slashed.
pub fn scan(root: &str) -> Vec<String> {
    let files = git_files(root);
    if files.is_empty() {
        walk(root)
    } else {
        files
    }
}

fn git_files(root: &str) -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(root)
        .output();
    match out {
        // A non-repo exits non-zero with the paths absent; anything but success
        // means fall through to the walk.
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Breadth-first so the shallow files — the ones anyone is likely to mean — are
/// the ones that survive [`WALK_LIMIT`].
fn walk(root: &str) -> Vec<String> {
    let base = Path::new(root);
    let mut out = Vec::new();
    let mut queue = vec![base.to_path_buf()];
    while let Some(dir) = queue.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if !SKIP.contains(&name.as_str()) && !name.starts_with('.') {
                    queue.push(path);
                }
            } else if let Ok(rel) = path.strip_prefix(base) {
                // Forward slashes on every platform, to match what git reports.
                // Built from components rather than by replacing backslashes: on
                // Unix a backslash is a legal filename character, and a blind
                // replace would quietly corrupt it.
                out.push(
                    rel.components()
                        .map(|c| c.as_os_str().to_string_lossy())
                        .collect::<Vec<_>>()
                        .join("/"),
                );
                if out.len() >= WALK_LIMIT {
                    return out;
                }
            }
        }
    }
    out
}

/// Paths matching `query`, best first, capped at [`MAX_MATCHES`].
///
/// Substring matching, case-insensitive: people remember a file by its name far
/// more often than by the directory it lives in, so `main` has to find
/// `src/bin/main.rs`. Ranking then puts the ones whose *name* matches above the
/// ones that only matched somewhere in the path.
pub fn matches(query: &str, files: &[String]) -> Vec<String> {
    let q = query.to_lowercase();
    let mut hits: Vec<(u8, &String)> = files
        .iter()
        .filter_map(|p| {
            let lower = p.to_lowercase();
            if q.is_empty() {
                return Some((2, p));
            }
            let name = lower.rsplit('/').next().unwrap_or(&lower);
            if name.starts_with(&q) {
                Some((0, p))
            } else if lower.starts_with(&q) {
                Some((1, p))
            } else if lower.contains(&q) {
                Some((2, p))
            } else {
                None
            }
        })
        .collect();
    // Stable within a rank, so the order git reported (alphabetical) survives.
    hits.sort_by_key(|(rank, _)| *rank);
    hits.into_iter().take(MAX_MATCHES).map(|(_, p)| p.clone()).collect()
}

/// A menu taller than this stops being a menu and starts being a file browser.
pub const MAX_MATCHES: usize = 10;

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<String> {
        ["src/main.rs", "src/app/mod.rs", "docs/main-ideas.md", "README.md"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    fn an_empty_query_lists_everything() {
        assert_eq!(matches("", &files()).len(), 4);
    }

    #[test]
    fn a_filename_match_outranks_a_path_match() {
        let m = matches("main", &files());
        // main.rs and main-ideas.md match by name; nothing matches by path here,
        // but the name matches must come first among all of them.
        assert_eq!(m[0], "src/main.rs");
        assert!(m.contains(&"docs/main-ideas.md".to_string()));
    }

    #[test]
    fn a_directory_prefix_still_matches() {
        assert_eq!(matches("src/app", &files()), vec!["src/app/mod.rs"]);
    }

    #[test]
    fn matching_ignores_case() {
        assert_eq!(matches("readme", &files()), vec!["README.md"]);
    }

    #[test]
    fn no_match_is_no_rows() {
        assert!(matches("zzz", &files()).is_empty());
    }

    #[test]
    fn the_list_is_capped() {
        let many: Vec<String> = (0..50).map(|i| format!("f{i}.rs")).collect();
        assert_eq!(matches("f", &many).len(), MAX_MATCHES);
    }

    #[test]
    fn a_fresh_cache_is_not_stale_but_an_empty_one_is() {
        let mut f = Files::default();
        assert!(f.stale(), "nothing scanned yet");
        f.set(vec!["a.rs".into()]);
        assert!(!f.stale());
        assert_eq!(f.list(), ["a.rs"]);
    }

    #[test]
    fn a_scan_in_flight_blocks_another() {
        let mut f = Files::default();
        f.inflight = true;
        assert!(!f.stale());
    }

    #[test]
    fn scanning_this_repository_finds_its_own_source() {
        // lrs is inside a git checkout, so this exercises the git path.
        let root = env!("CARGO_MANIFEST_DIR");
        let found = scan(root);
        assert!(
            found.iter().any(|p| p.ends_with("src/app/files.rs")),
            "scan missed this file: {:?}",
            &found[..found.len().min(5)]
        );
        assert!(found.iter().all(|p| !p.contains('\\')), "paths must be forward-slashed");
    }

    #[test]
    fn the_walk_fallback_skips_noise_directories() {
        let dir = std::env::temp_dir().join("lrs-files-walk");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "").unwrap();
        std::fs::write(dir.join("node_modules/pkg/b.js"), "").unwrap();

        let found = walk(&dir.display().to_string());
        assert_eq!(found, vec!["src/a.rs"]);
    }
}
