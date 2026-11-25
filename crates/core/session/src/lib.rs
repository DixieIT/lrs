//! Conversation state, isolated per `session_id`. Two implementations of the
//! SDK port:
//!
//! - [`SqliteSessionStore`]: the durable one. One SQLite database (WAL),
//!   every message of every turn, an FTS5 index for recall.
//! - [`InMemorySessionStore`]: same behaviour, nothing on disk; for tests and
//!   throwaway runs.
//!
//! Both follow the same rules, kept here: time-sortable session ids, a title
//! taken from the first user message, timestamps in Unix milliseconds.

mod memory;
mod recall;
mod sqlite;

pub use memory::InMemorySessionStore;
pub use recall::{Recall, RecallCommand, SessionRecallPlugin};
pub use sqlite::SqliteSessionStore;

use std::time::{SystemTime, UNIX_EPOCH};

/// Longest title kept, in characters.
const TITLE_CHARS: usize = 80;

pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis().try_into().unwrap_or(i64::MAX))
}

/// `20261122-143005-9f3a1c`: UTC time to the second, so ids sort by creation,
/// then a random suffix, so two sessions created in the same second (or by
/// two processes) never collide.
pub(crate) fn new_session_id() -> String {
    let secs = now_ms().div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}-{:06x}", t / 3600, t / 60 % 60, t % 60, random24())
}

/// 24 random bits from the standard library's per-process random hasher keys,
/// mixed with the clock and a counter: no dependency for a suffix.
fn random24() -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_i64(now_ms());
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    h.write_u32(std::process::id());
    h.finish() & 0xFF_FFFF
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A session's title from its first user message: the first non-empty line,
/// whitespace collapsed, cut at a readable length.
pub(crate) fn title_of(content: &str) -> Option<String> {
    let line = content.lines().map(str::trim).find(|l| !l.is_empty())?;
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    Some(if collapsed.chars().count() > TITLE_CHARS {
        collapsed.chars().take(TITLE_CHARS - 1).collect::<String>() + "…"
    } else {
        collapsed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_sort_by_time_and_do_not_collide() {
        let ids: Vec<String> = (0..200).map(|_| new_session_id()).collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        let shape = &ids[0];
        assert_eq!(shape.len(), "20261122-143005-9f3a1c".len(), "{shape}");
        assert!(shape.starts_with("20"), "{shape}");
    }

    #[test]
    fn calendar_dates_are_exact() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1));
        assert_eq!(civil_from_days(20_779), (2026, 11, 22));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn a_title_is_the_first_line_tidied_and_cut() {
        assert_eq!(title_of("\n  fix   the\tparser  \nand more").as_deref(), Some("fix the parser"));
        assert_eq!(title_of("   \n "), None);
        let long = "x".repeat(200);
        let title = title_of(&long).unwrap();
        assert_eq!(title.chars().count(), TITLE_CHARS);
        assert!(title.ends_with('…'));
    }
}
