//! An optional trace of what the loop is doing, for when it stops doing it.
//!
//! Not telemetry. Nothing leaves the machine and nothing is on by default — this
//! exists because a frozen TUI tells you nothing at all. Chasing one hang cost an
//! afternoon of theories, and the clipboard was only cleared of it by reproducing
//! the call outside the app; a file saying which call the loop entered and never
//! came back from would have settled it on the first reproduction.
//!
//! Hence the two shapes below. A [`risky`] span writes its opening line *before*
//! the call, so a hang leaves a `→` with no `←` and names itself. A [`timed`] span
//! writes nothing unless it overran, so the ordinary frame costs one atomic load.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Checked before any formatting happens, so a disabled trace costs one load.
static ON: AtomicBool = AtomicBool::new(false);
static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();
static WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Stop before a long session with the mouse fills a disk. A hang is diagnosed
/// from the tail, and the tail is what a cap keeps.
const MAX_BYTES: u64 = 64 * 1024 * 1024;

struct Sink {
    out: File,
    start: Instant,
}

/// Start tracing to `<LELE_HOME>/lrs.log`, returning where it went.
///
/// Truncates: two runs interleaved in one file is worse than losing the older.
pub fn enable() -> std::io::Result<std::path::PathBuf> {
    let dir = super::config::dir()
        .ok_or_else(|| std::io::Error::other("no home directory to log into"))?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("lrs.log");
    let out = File::create(&path)?;
    SINK.set(Mutex::new(Sink { out, start: Instant::now() }))
        .map_err(|_| std::io::Error::other("tracing is already on"))?;
    ON.store(true, Ordering::Relaxed);
    // Wall clock once, at the top: every line after it is milliseconds from here,
    // which is what you actually compare when hunting a stall.
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    line(format_args!(
        "lrs {} · started at epoch {epoch} · times are ms since start",
        env!("CARGO_PKG_VERSION")
    ));
    Ok(path)
}

pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

/// Write one line. Flushed immediately — a buffered tail is exactly the part a
/// hang would eat.
pub fn line(args: std::fmt::Arguments) {
    let Some(sink) = SINK.get() else { return };
    let mut sink = sink.lock().unwrap_or_else(|e| e.into_inner());
    let at = sink.start.elapsed().as_secs_f64() * 1000.0;
    let text = format!("{at:10.1}  {args}\n");
    let total = WRITTEN.fetch_add(text.len() as u64, Ordering::Relaxed) + text.len() as u64;
    if total > MAX_BYTES {
        // The tracer has nowhere else to report its own failure, and tracing
        // stops just below either way: the closing note is all that is tried.
        if total - text.len() as u64 <= MAX_BYTES
            && writeln!(sink.out, "{at:10.1}  — log capped at {MAX_BYTES} bytes —")
                .and_then(|()| sink.out.flush())
                .is_err()
        {
            WRITTEN.store(u64::MAX, Ordering::Relaxed);
        }
        ON.store(false, Ordering::Relaxed);
        return;
    }
    // A log file that cannot be written stops tracing instead of failing every line.
    if sink.out.write_all(text.as_bytes()).and_then(|()| sink.out.flush()).is_err() {
        ON.store(false, Ordering::Relaxed);
    }
}

/// A call worth naming before it is made. Logs on entry and on exit, so a hang
/// leaves an opening line with nothing closing it.
pub fn risky(what: &'static str) -> Span {
    if on() {
        line(format_args!("→ {what}"));
    }
    Span { what, at: Instant::now(), budget_ms: None }
}

/// Something that runs constantly and is only interesting when it overruns.
pub fn timed(what: &'static str, budget_ms: u64) -> Span {
    Span { what, at: Instant::now(), budget_ms: Some(budget_ms) }
}

pub struct Span {
    what: &'static str,
    at: Instant,
    /// `None` for a risky span: it always reports. `Some(ms)` reports only over.
    budget_ms: Option<u64>,
}

impl Drop for Span {
    fn drop(&mut self) {
        if !on() {
            return;
        }
        let ms = self.at.elapsed().as_secs_f64() * 1000.0;
        match self.budget_ms {
            None => line(format_args!("← {} {ms:.1}ms", self.what)),
            Some(b) if ms >= b as f64 => line(format_args!("  {} slow: {ms:.1}ms", self.what)),
            Some(_) => {}
        }
    }
}

/// Write a line only when tracing is on, without formatting its arguments first.
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::app::log::on() {
            $crate::app::log::line(format_args!($($arg)*));
        }
    };
}
pub(crate) use trace;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracing_is_off_until_it_is_turned_on() {
        // The whole point of the atomic: a disabled trace must cost nothing and,
        // more importantly, must never touch the disk.
        assert!(!on());
        line(format_args!("this goes nowhere"));
        let _s = risky("neither does this");
    }

    #[test]
    fn a_timed_span_stays_quiet_under_its_budget() {
        // Nothing to assert on disk with tracing off; this is here to pin the
        // shape of the API and to prove neither span panics when unused.
        let _quiet = timed("frame", 50);
        let _loud = timed("frame", 0);
    }
}
