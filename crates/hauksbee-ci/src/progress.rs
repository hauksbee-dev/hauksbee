//! Say what a run is doing while it does it.
//!
//! A co-simulation is minutes of work with nothing to show until it finishes:
//! on the flagship board a newcomer's first run takes over three minutes. A
//! silent process that runs that long is indistinguishable from a hung one, and
//! the reasonable thing for a first-time user to do with a hung process is kill
//! it.
//!
//! Progress goes to **stderr**, never stdout, so `--json` output stays a clean
//! parse and a redirect keeps working. It is off unless something installs a
//! sink, so a piped or CI run stays quiet by default and only a human at a
//! terminal pays for the noise.

use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Sink = Box<dyn FnMut(&str) + Send>;

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Install a sink, or `None` to go quiet again. Tests use this to capture.
pub fn set_sink(sink: Option<Sink>) {
    *SINK.lock().expect("progress sink") = sink;
}

/// Install the default sink: overwrite one line on stderr, if stderr is a
/// terminal. A redirected stderr gets nothing rather than a file full of
/// carriage returns.
pub fn to_stderr() {
    if !std::io::stderr().is_terminal() {
        return;
    }
    set_sink(Some(Box::new(|line: &str| {
        // \r to the start, then erase to end of line: a short line after a long
        // one must not leave the long one's tail on screen.
        let mut err = std::io::stderr();
        let _ = write!(err, "\r{line}\x1b[K");
        let _ = err.flush();
    })));
}

/// True when anything is listening. Callers can skip formatting work.
pub fn enabled() -> bool {
    SINK.lock().expect("progress sink").is_some()
}

fn emit(line: &str) {
    if let Some(sink) = SINK.lock().expect("progress sink").as_mut() {
        sink(line);
    }
}

/// A one-off line: a phase starting, a board loaded.
pub fn say(line: &str) {
    if enabled() {
        emit(line);
        end_line();
    }
}

/// Finish the current line so ordinary output starts on a fresh one.
fn end_line() {
    if enabled() && std::io::stderr().is_terminal() {
        let _ = writeln!(std::io::stderr());
    }
}

/// Tracks one long phase and reports how far through it is.
///
/// Emits only when the whole-number percentage changes, so a loop that runs a
/// hundred thousand frames costs a hundred writes rather than a hundred
/// thousand. It also holds off entirely for the first moment: a phase that
/// finishes quickly should not flash a progress line at all.
pub struct Ticker {
    label: String,
    started: Instant,
    last_pct: Option<u8>,
}

/// Below this, a phase is fast enough that reporting on it is just noise.
const QUIET_FOR: Duration = Duration::from_millis(750);

impl Ticker {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            started: Instant::now(),
            last_pct: None,
        }
    }

    /// Report progress through the phase. `frac` is 0.0 to 1.0.
    pub fn at(&mut self, frac: f64) {
        if !enabled() {
            return;
        }
        let elapsed = self.started.elapsed();
        if elapsed < QUIET_FOR {
            return;
        }
        let frac = frac.clamp(0.0, 1.0);
        let pct = (frac * 100.0) as u8;
        if self.last_pct == Some(pct) {
            return;
        }
        self.last_pct = Some(pct);
        emit(&format!(
            "  {} {pct:>3}%{}",
            self.label,
            remaining(elapsed, frac)
        ));
    }

    /// Close the phase out, clearing the line if one was ever drawn.
    pub fn done(self) {
        if self.last_pct.is_some() {
            emit("");
            end_line();
        }
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
    format!("  about {} left", human_secs(left))
}

fn human_secs(s: f64) -> String {
    if s < 60.0 {
        format!("{}s", s.round() as u64)
    } else {
        let m = (s / 60.0).floor() as u64;
        let rem = (s - (m as f64) * 60.0).round() as u64;
        format!("{m}m{rem:02}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex as StdMutex};

    /// The tests share one process-wide sink, so they must not interleave.
    static SERIAL: StdMutex<()> = StdMutex::new(());

    fn capture() -> (
        Arc<StdMutex<Vec<String>>>,
        std::sync::MutexGuard<'static, ()>,
    ) {
        let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let lines = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        set_sink(Some(Box::new(move |l: &str| {
            sink.lock().expect("capture").push(l.to_string());
        })));
        (lines, guard)
    }

    #[test]
    fn silent_until_a_sink_is_installed() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_sink(None);
        assert!(!enabled());
        // The point of the check: this must not panic and must not write.
        say("nobody should see this");
        let mut t = Ticker::new("phase");
        t.at(0.5);
        t.done();
    }

    #[test]
    fn a_quick_phase_says_nothing() {
        let (lines, _guard) = capture();
        let mut t = Ticker::new("quick");
        for i in 0..=100 {
            t.at(i as f64 / 100.0);
        }
        t.done();
        set_sink(None);
        assert!(
            lines.lock().unwrap().is_empty(),
            "a phase over in microseconds must not flash a progress line: {:?}",
            lines.lock().unwrap()
        );
    }

    #[test]
    fn a_slow_phase_reports_once_per_percent() {
        let (lines, _guard) = capture();
        let mut t = Ticker::new("slow");
        // Backdate the start so the phase counts as slow without sleeping.
        t.started = Instant::now() - Duration::from_secs(10);
        // Ten thousand calls across the range: one line per percent, not per call.
        for i in 0..=10_000 {
            t.at(i as f64 / 10_000.0);
        }
        t.done();
        set_sink(None);
        let got = lines.lock().unwrap().clone();
        let ticks = got.iter().filter(|l| !l.is_empty()).count();
        assert_eq!(
            ticks, 101,
            "0% through 100% inclusive, once each, whatever the call rate"
        );
        assert!(
            got[0].contains("slow"),
            "the label names the phase: {got:?}"
        );
    }

    #[test]
    fn an_estimate_waits_until_it_means_something() {
        // 1% done after a second would extrapolate to 100 seconds from almost no
        // evidence. Better to say nothing than to say a number that will move.
        assert_eq!(remaining(Duration::from_secs(1), 0.01), "");
        let late = remaining(Duration::from_secs(10), 0.5);
        assert!(
            late.contains("10s"),
            "half done after 10s means ~10s: {late}"
        );
    }

    #[test]
    fn long_estimates_read_as_minutes() {
        assert_eq!(human_secs(45.0), "45s");
        assert_eq!(human_secs(125.0), "2m05s");
    }
}
