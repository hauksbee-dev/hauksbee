//! "Still working" feedback for a datasheet extraction, and how long one takes.
//!
//! An extraction is minutes of an agent working silently, and a silent process
//! that long is indistinguishable from a hung one. The pipeline records its
//! phase here and each surface shows it: the CLI through [`with_spinner`], the
//! web server by polling [`snapshot`] into its stream. Process-wide, because one
//! extraction runs per process (the server holds a slot for exactly that).
//!
//! It also keeps the run's activity feed: structured [`event`]s (tier, stage,
//! pages read, tokens and cost so far, validator verdicts, escalations) that the
//! web stream forwards as `activity: <json>` lines and `HAUKSBEE_EXTRACT_EVENTS`
//! appends to a JSONL file, so a person can watch what the agent is doing.
//!
//! The estimate is the median of recent completed runs of the SAME model (a
//! strong model's minutes are no estimate for a fast one's, and one stuck run
//! drags a mean for weeks), each scaled by relative page count within 0.5-2x,
//! kept beside the backend config. A model with no history yet gets its tier's
//! typical time ([`tier_default_secs`]): the fast cheap tier (Luna, Flash,
//! Haiku) takes a couple of minutes a part, the strong tier (Sol, Opus, Sonnet)
//! several times that.

use std::cell::RefCell;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const DEFAULT_SECS: u64 = 180;
/// A fast, cheap model's typical run: measured 1-3 minutes a part on Luna.
const FAST_TIER_SECS: u64 = 120;
/// A strong model's typical run, until its own history says otherwise.
const STRONG_TIER_SECS: u64 = 600;
const KEEP: usize = 20;

struct State {
    phase: String,
    pages: u64,
    /// The model the run uses, whose history sets the estimate ("" = unknown).
    model: String,
    estimate: u64,
    /// A spinner line is on screen and must be erased before anything prints.
    drawn: bool,
    /// Events not yet taken by [`drain_events`], oldest first, at most [`KEEP_EVENTS`].
    events: Vec<String>,
    started: Option<Instant>,
}

static STATE: Mutex<State> = Mutex::new(State {
    phase: String::new(),
    pages: 0,
    model: String::new(),
    estimate: DEFAULT_SECS,
    drawn: false,
    events: Vec::new(),
    started: None,
});

/// Appends every activity event to this file as JSONL, for scripts and benchmarks.
pub const ENV_EVENTS: &str = "HAUKSBEE_EXTRACT_EVENTS";
const KEEP_EVENTS: usize = 500;

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A new run: forget the previous one's phase and page count.
pub fn start() {
    pages(0);
    {
        let mut s = state();
        s.events.clear();
        s.started = Some(Instant::now());
    }
    phase("reading the datasheet");
}

pub fn phase(now: impl Into<String>) {
    state().phase = now.into();
}

/// Record one activity event: `fields` (a JSON object) with `kind` and `t`,
/// the seconds since [`start`], added.
pub fn event(kind: &str, fields: serde_json::Value) {
    let mut s = state();
    let t = s.started.get_or_insert_with(Instant::now).elapsed();
    let mut value =
        serde_json::json!({ "t": (t.as_secs_f64() * 10.0).round() / 10.0, "kind": kind });
    if let (Some(all), serde_json::Value::Object(more)) = (value.as_object_mut(), fields) {
        all.extend(more);
    }
    let line = value.to_string();
    if let Some(path) = std::env::var_os(ENV_EVENTS) {
        use std::io::Write as _;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path);
        let _ = file.and_then(|mut f| writeln!(f, "{line}"));
    }
    if s.events.len() >= KEEP_EVENTS {
        s.events.remove(0);
    }
    s.events.push(line);
}

/// Take the events recorded since the last call, oldest first.
#[must_use]
pub fn drain_events() -> Vec<String> {
    std::mem::take(&mut state().events)
}

/// The datasheet's page count, once known (0 = unknown); rescales the estimate.
pub fn pages(count: usize) {
    let model = state().model.clone();
    let estimate = estimate_for(&model, count as u64);
    let mut s = state();
    (s.pages, s.estimate) = (count as u64, estimate);
}

/// The model the run uses: its own recorded runs set the estimate, and with
/// none it is its tier's typical time.
pub fn model(name: &str) {
    let pages = state().pages;
    let estimate = estimate_for(name, pages);
    let mut s = state();
    s.model = name.to_string();
    s.estimate = estimate;
}

/// Set the estimate directly, for a step that is not an extraction (a web
/// search for a datasheet); the next [`pages`] or [`model`] call replaces it.
pub fn expect(secs: u64) {
    state().estimate = secs;
}

/// How long a model's run takes when it has no history: by name, the fast
/// cheap tier or the strong tier; an unrecognised model gets the generic time.
#[must_use]
pub fn tier_default_secs(model: &str) -> u64 {
    let name = model.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if has(&["luna", "flash", "haiku", "mini", "nano", "lite"]) {
        FAST_TIER_SECS
    } else if has(&["sol", "opus", "sonnet", "pro", "max"]) {
        STRONG_TIER_SECS
    } else {
        DEFAULT_SECS
    }
}

fn estimate_for(model: &str, pages: u64) -> u64 {
    estimate_in(&history(), model, pages)
}

/// The estimate for `model` from `history`: the page-scaled median of its own
/// runs, else its tier's typical time.
fn estimate_in(history: &[(u64, u64, String)], model: &str, pages: u64) -> u64 {
    let own: Vec<(u64, u64)> = (history.iter())
        .filter(|run| !model.is_empty() && run.2 == model)
        .map(|(secs, was, _)| (*secs, *was))
        .collect();
    if own.is_empty() {
        return tier_default_secs(model);
    }
    median_scaled(&own, pages)
}

/// The current phase and the estimated whole-run seconds.
#[must_use]
pub fn snapshot() -> (String, u64) {
    let s = state();
    (s.phase.clone(), s.estimate)
}

/// Print narration on stderr (or a result on stdout) without ever sharing a
/// line with the spinner: the line is erased and the print made under one lock.
///
/// While several extractions run at once ([`set_concurrent`]) each line is
/// prefixed with the part its thread is working on ([`with_label`]), so lines
/// from different parts stay attributable and never interleave mid-line.
pub fn say(line: &str) {
    if CAPTURE
        .with(|c| {
            c.borrow_mut()
                .as_mut()
                .map(|lines| lines.push(line.to_string()))
        })
        .is_some()
    {
        return;
    }
    let _s = erased();
    eprintln!("{}", labelled(line));
}

thread_local! {
    static CAPTURE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Run `f`, collecting what [`say`] would print on this thread: `(result, lines said)`.
///
/// For a caller that reports what a step did (the repairs the harness makes to a
/// draft) rather than narrating it.
pub fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    struct Restore(Option<Vec<String>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAPTURE.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let outer = CAPTURE.with(|c| c.borrow_mut().replace(Vec::new()));
    let restore = Restore(outer);
    let result = f();
    let said = CAPTURE.with(|c| c.borrow_mut().take()).unwrap_or_default();
    drop(restore);
    (result, said)
}

pub fn out(text: &str) {
    let _s = erased();
    println!("{}", labelled(text));
}

static CONCURRENT: AtomicBool = AtomicBool::new(false);

/// Several extractions share this process (`models extract --unmodelled --jobs N`).
///
/// The process-wide spinner, per-run plan and cost summary describe
/// one run, so they stand down: every line is labelled with its part instead,
/// and the batch reports the cost once for all of them.
pub fn set_concurrent(on: bool) {
    CONCURRENT.store(on, Ordering::SeqCst);
}

/// Whether [`set_concurrent`] is on.
#[must_use]
pub fn concurrent() -> bool {
    CONCURRENT.load(Ordering::SeqCst)
}

thread_local! {
    static LABEL: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The part this thread is working on, if it was told ([`with_label`]).
#[must_use]
pub fn label() -> Option<String> {
    LABEL.with(|l| l.borrow().clone())
}

/// Set (or clear) this thread's label. For a helper thread that works for a
/// labelled one: capture [`label`] before spawning and set it first thing.
pub fn set_label(label: Option<String>) {
    LABEL.with(|l| *l.borrow_mut() = label);
}

/// Run `f` as work on `part`: its narration is labelled with it and any agent
/// question it raises ([`question`]) is kept under it.
pub fn with_label<T>(part: &str, f: impl FnOnce() -> T) -> T {
    let before = label();
    set_label(Some(part.to_string()));
    let result = f();
    set_label(before);
    result
}

fn labelled(text: &str) -> String {
    label().filter(|_| concurrent()).map_or_else(
        || text.to_string(),
        |part| {
            text.lines()
                .map(|l| format!("[{part}] {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
    )
}

static RATE_LIMITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A backend call was refused for rate limiting (HTTP 429 or its wording). A
/// batch reads [`rate_limit_hits`] to run fewer agents at once.
pub fn note_rate_limit() {
    RATE_LIMITS.fetch_add(1, Ordering::SeqCst);
}

/// How many rate-limit refusals this process has seen.
#[must_use]
pub fn rate_limit_hits() -> usize {
    RATE_LIMITS.load(Ordering::SeqCst)
}

static QUESTIONS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
const KEEP_QUESTIONS: usize = 500;

/// An agent asked a question it could not answer from the datasheet.
///
/// Said now, and kept under the current label for [`take_questions`], so a run nobody is
/// watching still ends with it on the list instead of losing it in the log.
pub fn question(text: &str) {
    say(&format!("[model-extract] agent question: {text}"));
    let mut all = QUESTIONS.lock().unwrap_or_else(PoisonError::into_inner);
    if all.len() < KEEP_QUESTIONS {
        all.push((label().unwrap_or_default(), text.to_string()));
    }
}

/// The questions raised under `label` since the last call, oldest first.
#[must_use]
pub fn take_questions(label: &str) -> Vec<String> {
    let mut all = QUESTIONS.lock().unwrap_or_else(PoisonError::into_inner);
    let (mine, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *all)
        .into_iter()
        .partition(|(l, _)| l == label);
    *all = rest;
    drop(all);
    mine.into_iter().map(|(_, q)| q).collect()
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

/// `(seconds, pages, model)` per completed run, oldest first. A line written
/// before runs were kept per model has no model and counts for none.
fn history() -> Vec<(u64, u64, String)> {
    let text = history_path().and_then(|p| std::fs::read_to_string(p).ok());
    parse_history(&text.unwrap_or_default())
}

fn parse_history(text: &str) -> Vec<(u64, u64, String)> {
    text.lines()
        .filter_map(|l| {
            let mut fields = l.splitn(3, ' ');
            let secs = fields.next()?.parse().ok()?;
            let pages = fields.next()?.parse().ok()?;
            Some((secs, pages, fields.next().unwrap_or_default().to_string()))
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

/// The history file's text: each model keeps its own last [`KEEP`] runs, oldest
/// first, and a run from before models were recorded is dropped.
fn history_text(runs: &[(u64, u64, String)]) -> String {
    let mut kept: Vec<&(u64, u64, String)> = Vec::new();
    for run in runs.iter().rev().filter(|run| !run.2.is_empty()) {
        if kept.iter().filter(|k| k.2 == run.2).count() < KEEP {
            kept.push(run);
        }
    }
    kept.reverse();
    (kept.iter())
        .map(|(s, p, m)| format!("{s} {p} {m}\n"))
        .collect::<Vec<_>>()
        .concat()
}

/// Remember a completed run. Best effort: an unwritable history only costs a
/// worse estimate, and a canned mock reply says nothing about real runs.
pub fn record(took: Duration) {
    let Some(path) = history_path() else { return };
    if std::env::var_os("HAUKSBEE_EXTRACT_MOCK_REPLY").is_some() {
        return;
    }
    let (pages, model) = {
        let s = state();
        (s.pages, s.model.clone())
    };
    // Without a model the run says nothing about any one of them.
    if model.is_empty() {
        return;
    }
    let mut runs = history();
    runs.push((took.as_secs(), pages, model));
    let text = history_text(&runs);
    let _ = path.parent().map(std::fs::create_dir_all);
    let _ = std::fs::write(path, text);
}

/// Run a CLI extraction with its phase, elapsed time and typical duration on
/// stderr, and remember how long it took if it succeeded.
///
/// On a terminal that is one line redrawn in place (no cursor or mode changes,
/// so Ctrl-C leaves the terminal as it was); anywhere else a plain line every
/// 30 s, never a control code. The line is erased before `run` returns, success
/// or error.
///
/// # Errors
///
/// Returns `run`'s own error unchanged; the spinner adds none.
pub fn with_spinner<T, E>(run: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    spinner(true, run)
}

/// [`with_spinner`] for a step that is not an extraction (a web search): its
/// duration says nothing about how long a model takes to draft a card, so it is
/// not remembered.
///
/// # Errors
///
/// Returns `run`'s own error unchanged; the spinner adds none.
pub fn with_unrecorded_spinner<T, E>(run: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    spinner(false, run)
}

// The ticker holds its lock across a redraw so `stop` cannot land mid-line.
#[allow(clippy::significant_drop_tightening)]
fn spinner<T, E>(record_run: bool, run: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
    // One redrawn line and one run's estimate mean nothing with several runs at
    // once; the batch prints its own progress.
    if concurrent() {
        return run();
    }
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
    if record_run && result.is_ok() {
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
    use super::{
        about, clock, estimate_in, history_text, median_scaled, parse_history, tier_default_secs,
        DEFAULT_SECS, FAST_TIER_SECS, KEEP, STRONG_TIER_SECS,
    };

    /// The hint is per model: the fast cheap tier reads in minutes, the strong
    /// tier several times that, and a model nobody has classified keeps the
    /// generic time rather than borrowing either.
    #[test]
    fn the_typical_time_depends_on_the_model_tier() {
        assert_eq!(tier_default_secs("gpt-6-luna"), FAST_TIER_SECS);
        assert_eq!(tier_default_secs("gemini-3.8-flash"), FAST_TIER_SECS);
        assert_eq!(tier_default_secs("gpt-6.1-sol"), STRONG_TIER_SECS);
        assert_eq!(tier_default_secs("claude-opus-5-5"), STRONG_TIER_SECS);
        assert_eq!(tier_default_secs("something-else"), DEFAULT_SECS);
        assert_eq!(
            (
                about(tier_default_secs("gpt-6-luna")),
                about(STRONG_TIER_SECS)
            ),
            ("~2 min".into(), "~10 min".into())
        );
    }

    /// History is kept per model: a slow strong-model run must not set the
    /// estimate for the fast one (the old single history showed ~14 min for
    /// Luna, whose runs take 1-3), and a model without runs falls back to its
    /// tier.
    #[test]
    fn recorded_runs_set_only_their_own_models_estimate() {
        let runs = parse_history("840 20 gpt-6.1-sol\n900 20 gpt-6.1-sol\n95 20 gpt-6-luna\n");
        assert_eq!(estimate_in(&runs, "gpt-6-luna", 0), 95);
        assert_eq!(estimate_in(&runs, "gpt-6.1-sol", 0), 900);
        assert_eq!(estimate_in(&runs, "gemini-3.8-flash", 0), FAST_TIER_SECS);
        assert_eq!(estimate_in(&runs, "", 0), DEFAULT_SECS);
    }

    /// A line from before models were recorded counts for no model and is not
    /// written back; each model keeps its own last runs.
    #[test]
    fn old_history_lines_count_for_no_model_and_each_model_keeps_its_own_tail() {
        let old = parse_history("700 12\n");
        assert_eq!(old, vec![(700, 12, String::new())]);
        assert_eq!(estimate_in(&old, "gpt-6-luna", 0), FAST_TIER_SECS);
        let mut runs = old;
        runs.extend((0..KEEP as u64 + 5).map(|n| (n, 10, "a".to_string())));
        runs.push((7, 10, "b".to_string()));
        let kept = parse_history(&history_text(&runs));
        assert_eq!(kept.iter().filter(|r| r.2 == "a").count(), KEEP);
        assert_eq!(kept.iter().filter(|r| r.2 == "b").count(), 1);
        assert!(kept.iter().all(|r| !r.2.is_empty()));
        assert_eq!(
            kept.iter().find(|r| r.2 == "a").map(|r| r.0),
            Some(5),
            "the oldest are the ones dropped"
        );
    }

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
