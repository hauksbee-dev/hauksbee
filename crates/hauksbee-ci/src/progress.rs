//! Say what a run is doing while it does it.
//!
//! A co-simulation is minutes of work with nothing to show until it finishes:
//! on a large board a newcomer's first run takes minutes, and a CI job can run
//! for eleven. A silent process that runs that long is indistinguishable from a
//! hung one, and the reasonable thing to do with a hung process is kill it.
//!
//! Progress goes to **stderr**, never stdout, so `--json` output stays a clean
//! parse and a redirect keeps working. It is off unless something installs a
//! sink. At a terminal the run narrates its phases and prints one line every
//! ten seconds while simulating; with stderr redirected (a CI log) only that
//! heartbeat survives, once a minute, so a log never fills with progress yet a
//! long run is never silent.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Sink = Box<dyn FnMut(&str) + Send>;

/// Phase narration ("reading the board"): a human at a terminal only.
static SINK: Mutex<Option<Sink>> = Mutex::new(None);
/// The simulation heartbeat: also wanted in a redirected CI log.
static BEAT: Mutex<Option<Sink>> = Mutex::new(None);
/// Seconds between heartbeat lines.
static BEAT_EVERY_SECS: AtomicU64 = AtomicU64::new(10);

/// Heartbeat spacing at a terminal, and in a log file.
const TTY_BEAT: Duration = Duration::from_secs(10);
const LOG_BEAT: Duration = Duration::from_secs(60);

/// Install a phase sink, or `None` to go quiet again. Tests use this to capture.
///
/// # Panics
///
/// Panics if the sink mutex is poisoned, i.e. an earlier sink panicked while
/// writing a line.
pub fn set_sink(sink: Option<Sink>) {
    *SINK.lock().expect("progress sink") = sink;
}

/// Install the heartbeat sink, printed at most once per `every`.
fn set_heartbeat(sink: Option<Sink>, every: Duration) {
    BEAT_EVERY_SECS.store(every.as_secs(), Ordering::Relaxed);
    *BEAT.lock().expect("progress heartbeat") = sink;
}

fn print_line(line: &str) {
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{}", hauksbee_ir::terminal_safe::terminal_safe(line));
    let _ = err.flush();
}

/// Install the default sinks on stderr.
///
/// At a terminal: phase lines and a heartbeat every 10 s. Redirected: a
/// heartbeat every 60 s, as plain lines, and nothing else, so a log file
/// collects no carriage returns and no chatter.
pub fn to_stderr() {
    if std::io::stderr().is_terminal() {
        set_sink(Some(Box::new(print_line)));
        set_heartbeat(Some(Box::new(print_line)), TTY_BEAT);
    } else {
        set_heartbeat(Some(Box::new(print_line)), LOG_BEAT);
    }
}

/// True when anything is listening for phase lines.
///
/// # Panics
///
/// Panics if the sink mutex is poisoned, i.e. an earlier sink panicked while
/// writing a line.
pub fn enabled() -> bool {
    SINK.lock().expect("progress sink").is_some()
}

/// A one-off line: a phase starting, a board loaded.
///
/// # Panics
///
/// Panics if the sink mutex is poisoned.
pub fn say(line: &str) {
    if let Some(sink) = SINK.lock().expect("progress sink").as_mut() {
        sink(line);
    }
}

/// Reports a long simulation: one line per heartbeat interval saying how much
/// board time has run, which ensemble member it is, and how long it has taken.
pub struct Ticker {
    label: String,
    detail: String,
    total_s: f64,
    started: Instant,
    last: Instant,
}

impl Ticker {
    /// `total_s` is the board time the phase will cover; `detail` names the
    /// ensemble member (`seed 2/16`).
    pub fn new(label: impl Into<String>, total_s: f64, detail: impl Into<String>) -> Self {
        let now = Instant::now();
        Self {
            label: label.into(),
            detail: detail.into(),
            total_s,
            started: now,
            last: now,
        }
    }

    /// Report progress through the phase: `t_s` of board time has run. Emits at
    /// most once per heartbeat interval, so a loop that calls it every frame
    /// costs a clock read, and a phase that finishes quickly prints nothing.
    ///
    /// # Panics
    ///
    /// Panics if the heartbeat mutex is poisoned.
    pub fn at(&mut self, t_s: f64) {
        let every = Duration::from_secs(BEAT_EVERY_SECS.load(Ordering::Relaxed));
        if self.last.elapsed() < every {
            return;
        }
        let mut beat = BEAT.lock().expect("progress heartbeat");
        let Some(sink) = beat.as_mut() else {
            return;
        };
        self.last = Instant::now();
        let elapsed = self.started.elapsed();
        let frac = if self.total_s > 0.0 {
            t_s / self.total_s
        } else {
            0.0
        };
        sink(&format!(
            "  ... {}: {t_s:.3} / {:.3} s board time ({}), {} elapsed{}",
            self.label,
            self.total_s,
            self.detail,
            human_secs(elapsed.as_secs_f64()),
            remaining(elapsed, frac)
        ));
        drop(beat);
    }
}

/// A rough time-remaining, from how long the work so far took. Omitted until
/// there is enough of the phase behind us for the estimate to mean anything;
/// a guess from the first 1% is worse than no guess.
fn remaining(elapsed: Duration, frac: f64) -> String {
    if frac < 0.05 {
        return String::new();
    }
    let total = elapsed.as_secs_f64() / frac;
    let left = (total - elapsed.as_secs_f64()).max(0.0);
    if left < 1.0 {
        return String::new();
    }
    format!(", about {} left", human_secs(left))
}

// A non-negative duration in seconds, rounded to whole seconds on purpose.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn human_secs(s: f64) -> String {
    if s < 60.0 {
        format!("{}s", s.round() as u64)
    } else {
        let m = (s / 60.0).floor();
        format!("{}m{:02}s", m as u64, m.mul_add(-60.0, s).round() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    /// The tests share one process-wide sink, so they must not interleave.
    static SERIAL: StdMutex<()> = StdMutex::new(());

    fn capture(
        every: Duration,
    ) -> (
        Arc<StdMutex<Vec<String>>>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let lines = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        set_heartbeat(
            Some(Box::new(move |l: &str| {
                sink.lock().expect("capture").push(l.to_string());
            })),
            every,
        );
        (lines, guard)
    }

    #[test]
    fn silent_without_a_sink_and_for_a_quick_phase() {
        let guard = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set_sink(None);
        set_heartbeat(None, TTY_BEAT);
        assert!(!enabled());
        // Must neither panic nor write.
        say("nobody should see this");
        let mut t = Ticker::new("phase", 1.0, "seed 1/1");
        t.at(0.5);
        drop(guard);

        let (lines, _guard) = capture(TTY_BEAT);
        let mut t = Ticker::new("quick", 1.0, "seed 1/1");
        for i in 0..=100 {
            t.at(f64::from(i) / 100.0);
        }
        set_heartbeat(None, TTY_BEAT);
        assert!(
            lines.lock().unwrap().is_empty(),
            "a phase over in microseconds must not print a progress line: {:?}",
            lines.lock().unwrap()
        );
    }

    #[test]
    fn a_slow_phase_prints_one_line_per_interval_with_board_time_and_member() {
        let (lines, _guard) = capture(Duration::from_secs(10));
        let mut t = Ticker::new("simulating", 0.2, "seed 2/16");
        // Backdate the last line so the first interval has already passed.
        t.started = Instant::now().checked_sub(Duration::from_secs(30)).unwrap();
        t.last = t.started;
        for i in 0..=1000 {
            t.at(0.2 * f64::from(i) / 1000.0);
        }
        set_heartbeat(None, TTY_BEAT);
        let got = lines.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            1,
            "one line per interval, however often it is called: {got:?}"
        );
        let line = &got[0];
        assert!(
            line.contains("simulating: 0.000 / 0.200 s board time"),
            "{line}"
        );
        assert!(line.contains("(seed 2/16)"), "{line}");
        assert!(line.contains("30s elapsed"), "{line}");
    }

    #[test]
    fn estimates_wait_for_evidence_and_read_as_minutes_when_long() {
        // 1% done after a second would extrapolate from almost no evidence.
        assert_eq!(remaining(Duration::from_secs(1), 0.01), "");
        let late = remaining(Duration::from_secs(10), 0.5);
        assert!(
            late.contains("10s"),
            "half done after 10s means ~10s: {late}"
        );
        assert_eq!(human_secs(45.0), "45s");
        assert_eq!(human_secs(125.0), "2m05s");
    }
}
