//! "Still working" feedback for a datasheet extraction, and how long one takes.
//!
//! An extraction is minutes of an agent working silently, and a silent process
//! that long is indistinguishable from a hung one. The pipeline records its
//! phase here and each surface shows it: the CLI through [`with_spinner`], the
//! web server by polling [`snapshot`] into its stream. Process-wide, because one
//! extraction runs per process (the server holds a slot for exactly that).
//!
//! The estimate is the median of recent completed runs (one stuck run drags a
//! mean for weeks), each scaled by relative page count within 0.5-2x, kept
//! beside the backend config. With no history it is a typical agent run.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const DEFAULT_SECS: u64 = 180;
const KEEP: usize = 20;

struct State {
    phase: String,
    pages: u64,
    estimate: u64,
    /// A spinner line is on screen and must be erased before anything prints.
    drawn: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    phase: String::new(),
    pages: 0,
    estimate: DEFAULT_SECS,
    drawn: false,
});

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A new run: forget the previous one's phase and page count.
pub fn start() {
    pages(0);
    phase("reading the datasheet");
}

pub fn phase(now: impl Into<String>) {
    state().phase = now.into();
}

/// The datasheet's page count, once known (0 = unknown); rescales the estimate.
pub fn pages(count: usize) {
    let estimate = median_scaled(&history(), count as u64);
    let mut s = state();
    (s.pages, s.estimate) = (count as u64, estimate);
}

/// The current phase and the estimated whole-run seconds.
#[must_use]
pub fn snapshot() -> (String, u64) {
    let s = state();
    (s.phase.clone(), s.estimate)
}

/// Print narration on stderr (or a result on stdout) without ever sharing a
/// line with the spinner: the line is erased and the print made under one lock.
pub fn say(line: &str) {
    let _s = erased();
    eprintln!("{line}");
}

pub fn out(text: &str) {
    let _s = erased();
    println!("{text}");
}

fn erased() -> MutexGuard<'static, State> {
    let mut s = state();
    if std::mem::take(&mut s.drawn) {
        eprint!("\r\x1b[2K");
    }
    s
}

fn history_path() -> Option<PathBuf> {
    let config = crate::extract_config::config_path().ok()?;
    Some(config.with_file_name("extract-history.txt"))
}

/// `(seconds, pages)` per completed run, oldest first.
fn history() -> Vec<(u64, u64)> {
    let text = history_path().and_then(|p| std::fs::read_to_string(p).ok());
    (text.unwrap_or_default().lines())
        .filter_map(|l| {
            l.split_once(' ')
                .and_then(|(s, p)| Some((s.parse().ok()?, p.parse().ok()?)))
        })
        .collect()
}

// Second counts and page counts sit far below 2^52, and a median run is never negative.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn median_scaled(history: &[(u64, u64)], pages: u64) -> u64 {
    let mut runs: Vec<f64> = (history.iter().copied())
        .map(|(secs, was)| match (pages, was) {
            (0, _) | (_, 0) => secs as f64,
            _ => secs as f64 * (pages as f64 / was as f64).clamp(0.5, 2.0),
        })
        .collect();
    runs.sort_by(f64::total_cmp);
    runs.get(runs.len() / 2)
        .map_or(DEFAULT_SECS, |m| m.round() as u64)
}

/// Remember a completed run. Best effort: an unwritable history only costs a
/// worse estimate, and a canned mock reply says nothing about real runs.
pub fn record(took: Duration) {
    let Some(path) = history_path() else { return };
    if std::env::var_os("HAUKSBEE_EXTRACT_MOCK_REPLY").is_some() {
        return;
    }
    let mut runs = history();
    runs.push((took.as_secs(), state().pages));
    let keep = runs[runs.len().saturating_sub(KEEP)..].iter();
    let text = keep
        .map(|(s, p)| format!("{s} {p}\n"))
        .collect::<Vec<_>>()
        .concat();
    let _ = path.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(path, text);
}

/// Run a CLI extraction with its phase, elapsed time and typical duration on
/// stderr, and remember how long it took if it succeeded. On a terminal that is
/// one line redrawn in place (no cursor or mode changes, so Ctrl-C leaves the
/// terminal as it was); anywhere else a plain line every 30 s, never a control
/// code. The line is erased before `run` returns, success or error.
// The ticker holds its lock across a redraw so `stop` cannot land mid-line.
#[allow(clippy::significant_drop_tightening)]
pub fn with_spinner<T, E>(run: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    let (started, tty) = (Instant::now(), std::io::stderr().is_terminal());
    let stop = Arc::new((Mutex::new(false), Condvar::new()));
    let flag = Arc::clone(&stop);
    let ticker = std::thread::spawn(move || {
        let tick = Duration::from_millis(if tty { 120 } else { 30_000 });
        let mut stopped = flag.0.lock().unwrap_or_else(PoisonError::into_inner);
        for frame in "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".chars().cycle() {
            let waited = flag.1.wait_timeout_while(stopped, tick, |s| !*s);
            stopped = waited.unwrap_or_else(PoisonError::into_inner).0;
            if *stopped {
                break;
            }
            let (phase, typical) = snapshot();
            let (secs, typical) = (clock(started.elapsed().as_secs()), about(typical));
            if tty {
                // Short of an 80-column terminal: a wrapped line cannot be erased.
                let line = format!("{secs} {phase} · {typical} typical");
                let line: String = line.chars().take(76).collect();
                let mut s = state();
                eprint!("\r\x1b[2K{frame} {line}");
                s.drawn = true;
            } else {
                say(&format!(
                    "[model-extract] still working after {secs}: {phase} ({typical} typical)"
                ));
            }
        }
    });
    let result = run();
    *stop.0.lock().unwrap_or_else(PoisonError::into_inner) = true;
    stop.1.notify_all();
    let _ = ticker.join();
    drop(erased());
    if result.is_ok() {
        record(started.elapsed());
    }
    result
}

fn clock(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        _ => format!("{}m{:02}s", secs / 60, secs % 60),
    }
}

/// An estimate without false precision: "~40s", "~3 min".
fn about(secs: u64) -> String {
    match secs {
        0..90 => format!("~{secs}s"),
        _ => format!("~{} min", (secs + 30) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::{about, clock, median_scaled, DEFAULT_SECS};

    #[test]
    fn times_read_as_seconds_then_minutes_and_estimates_stay_rough() {
        assert_eq!((clock(42), clock(185)), ("42s".into(), "3m05s".into()));
        assert_eq!((about(45), about(170)), ("~45s".into(), "~3 min".into()));
    }

    /// The median, not the mean: one stuck run must not move the estimate. A
    /// longer datasheet scales each run up, but by at most 2x.
    #[test]
    fn the_estimate_is_a_page_scaled_median_with_a_default() {
        assert_eq!(median_scaled(&[], 10), DEFAULT_SECS);
        let runs = [(60, 10), (90, 10), (3000, 10)];
        assert_eq!(median_scaled(&runs, 0), 90);
        assert_eq!(median_scaled(&runs, 15), 135);
        assert_eq!(median_scaled(&runs, 100), 180);
    }
}
