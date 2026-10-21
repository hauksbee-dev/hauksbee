//! Running the parts of a `models extract --unmodelled` batch: several at once
//! on a bounded pool, one readable line at a time, and one honest tally.
//!
//! A whole board is dozens of parts, each a web search, a download and an
//! agent run that is mostly waiting, so they run side by side. How many is
//! worked out from the plan's headroom (`hauksbee_models::extract_capacity`)
//! unless `--jobs N` says; `--jobs 1` is the plain serial loop with the
//! spinner. The pool is paced, because the limit that bites is how many agents
//! hit the service at once, not the plan's headroom: it starts a couple, adds
//! one every `ramp_every` while nothing is refused, up to the planned ceiling;
//! halves when the backend rate limits (at most once per `backoff_every`); and
//! grows back one at a time after `regrow_after` without a refusal. Only the
//! ceiling is lowered by the plan's headroom (when the observed usage says the
//! 5-hour window will not last), never raised. Each part is its own unit of work and ends in exactly one of three
//! ways, and the tally counts them apart:
//!
//! - **drafted**: a card was written;
//! - **no datasheet**: none could be obtained (nothing was sent), listed at the
//!   end for the person to supply;
//! - **failed**: the extraction itself ran and was refused (the model, the
//!   validator). Only these make the command exit non-zero.
//!
//! Safety of the shared output library: two parts whose card (and downloaded
//! datasheet) would be the same file name run one after the other in the same
//! lane, never at once, and every other part writes only its own files.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hauksbee_ir::{eprintln, println};
use hauksbee_models::datasheet::{self, Args};
use hauksbee_models::datasheet_discovery::{Acquirer, Missing, Outcome, Wanted};
use hauksbee_models::extract_capacity as capacity;
use hauksbee_models::extract_messages::{self as words, Need};
use hauksbee_models::extract_plan::{self, Reading};
use hauksbee_models::extract_progress as progress;

use super::models::UnmodelledPart;

/// The most parts worked on at once: one constant, in `extract_capacity`, so a
/// measured ramp can tune it in one place.
pub const MAX_JOBS: usize = capacity::JOBS_CAP;

/// What a running batch watches of the plan to refine its cost per part.
#[derive(Debug, Clone, Copy)]
pub struct PlanWatch {
    /// The 5-hour usage when the batch began.
    pub start_session: Option<f64>,
    /// The prior cost per part, in points of the 5-hour window.
    pub prior_session: f64,
}

/// How a batch is run.
#[derive(Debug, Clone)]
pub struct BatchOptions {
    /// Parts worked on at once as asked (`--jobs`); `None` takes the
    /// recommendation for the plan's headroom. 1 is the serial loop.
    pub jobs: Option<usize>,
    /// A folder of PDFs named `<part>.pdf` to use instead of fetching
    /// (`--datasheet-dir`). `None` uses `<out-dir>/datasheets`, where fetched
    /// ones are kept, so the same folder serves both.
    pub datasheet_dir: Option<PathBuf>,
    /// How often a quiet stretch of a long batch says how far it has got.
    pub progress_every: Duration,
    /// The shortest time between two halvings of the pool for rate limiting.
    pub backoff_every: Duration,
    /// While nothing is refused, how often one more agent is let in.
    pub ramp_every: Duration,
    /// After a refusal, the quiet stretch before the pool grows by one.
    pub regrow_after: Duration,
    /// Refine the plan's cost during the run (`None`: say nothing of the plan).
    pub watch: Option<PlanWatch>,
    /// The newest plan reading ([`hauksbee_models::extract_plan::latest_reading`]).
    pub reading: fn() -> Option<Reading>,
}

impl Default for BatchOptions {
    fn default() -> Self {
        Self {
            jobs: None,
            datasheet_dir: None,
            progress_every: Duration::from_secs(30),
            backoff_every: Duration::from_secs(30),
            ramp_every: capacity::RAMP_EVERY,
            regrow_after: capacity::REGROW_AFTER,
            watch: None,
            reading: extract_plan::latest_reading,
        }
    }
}

/// What a batch did, part by part.
#[derive(Debug, Default)]
pub struct Tally {
    pub drafted: usize,
    /// Parts nothing could be fetched for, with every link tried.
    pub no_datasheet: Vec<(String, Missing)>,
    /// Parts whose extraction ran and failed, with the error.
    pub failed: Vec<(String, String)>,
    /// Questions an agent asked that nobody was there to answer.
    pub needs: Vec<Need>,
    /// How the pool moved: planned ceiling, peak running, final, refusals.
    pub pool: capacity::PoolStats,
}

impl Tally {
    /// The names of the failed parts.
    #[must_use]
    pub fn failed_names(&self) -> Vec<&str> {
        self.failed.iter().map(|(p, _)| p.as_str()).collect()
    }
}

/// How one part ended.
enum Ended {
    Drafted,
    NoDatasheet(Missing),
    Failed(String),
}

struct PartRun {
    ended: Ended,
    needs: Vec<String>,
}

/// Runs one extraction (the real one is [`datasheet::run`] under the spinner).
pub type Extract<'a> = &'a (dyn Fn(Args) -> anyhow::Result<PathBuf> + Sync);

/// Lines from several parts, each whole and each saying whose it is.
struct Console {
    prefixed: bool,
}

/// One message at a time, so a multi-line message stays together.
static PRINT: Mutex<()> = Mutex::new(());

impl Console {
    fn say(&self, part: &str, text: &str) {
        self.emit(part, text, false);
    }

    fn complain(&self, part: &str, text: &str) {
        self.emit(part, text, true);
    }

    fn emit(&self, part: &str, text: &str, to_err: bool) {
        let _one_at_a_time = PRINT.lock().unwrap_or_else(PoisonError::into_inner);
        for line in text.lines() {
            // Most lines already lead with the part; a prefix would only repeat it.
            let lead: String = line.chars().take(part.chars().count() + 24).collect();
            let line = if self.prefixed && !lead.contains(part) {
                format!("[{part}] {line}")
            } else {
                line.to_string()
            };
            if to_err {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    }
}

/// Time since some fixed start (injectable so pacing can be tested).
type Clock<'a> = &'a (dyn Fn() -> Duration + Sync);

/// How a pool paces itself.
#[derive(Debug, Clone, Copy)]
struct Pacing {
    backoff_every: Duration,
    ramp_every: Duration,
    regrow_after: Duration,
}

/// What the pool remembers between moves.
struct Moves {
    last_halved: Option<Duration>,
    /// When the pool last changed or a refusal was last seen: growth waits a
    /// full interval after it.
    last_event: Duration,
    /// A refusal has been seen, so growth waits `regrow_after` not `ramp_every`.
    refused: bool,
}

/// How many jobs may run at once: it starts small and ramps up to a ceiling,
/// halves when the backend rate limits and grows back after a quiet stretch.
/// The ceiling itself only ever goes down.
struct Pool<'a> {
    /// The ceiling the batch was planned with.
    planned: usize,
    ceiling: AtomicUsize,
    limit: AtomicUsize,
    active: Mutex<usize>,
    turn: Condvar,
    peak: AtomicUsize,
    refusals: AtomicUsize,
    /// The rate-limit refusals seen when last looked.
    seen: AtomicUsize,
    hits: &'a (dyn Fn() -> usize + Sync),
    moves: Mutex<Moves>,
    pacing: Pacing,
    clock: Clock<'a>,
    say: &'a (dyn Fn(&str) + Sync),
}

impl<'a> Pool<'a> {
    fn new(
        ceiling: usize,
        pacing: Pacing,
        clock: Clock<'a>,
        hits: &'a (dyn Fn() -> usize + Sync),
        say: &'a (dyn Fn(&str) + Sync),
    ) -> Self {
        let ceiling = ceiling.max(1);
        Self {
            planned: ceiling,
            ceiling: AtomicUsize::new(ceiling),
            limit: AtomicUsize::new(capacity::ramp_start(ceiling)),
            active: Mutex::new(0),
            turn: Condvar::new(),
            peak: AtomicUsize::new(0),
            refusals: AtomicUsize::new(0),
            seen: AtomicUsize::new(hits()),
            hits,
            moves: Mutex::new(Moves {
                last_halved: None,
                last_event: clock(),
                refused: false,
            }),
            pacing,
            clock,
            say,
        }
    }

    fn limit(&self) -> usize {
        self.limit.load(Ordering::SeqCst)
    }

    fn ceiling(&self) -> usize {
        self.ceiling.load(Ordering::SeqCst)
    }

    fn stats(&self) -> capacity::PoolStats {
        capacity::PoolStats {
            planned: self.planned,
            peak: self.peak.load(Ordering::SeqCst),
            last: self.limit(),
            rate_limits: self.refusals.load(Ordering::SeqCst),
        }
    }

    fn moves(&self) -> std::sync::MutexGuard<'_, Moves> {
        self.moves.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lower the ceiling (and the pool with it) to `to`, never raise either;
    /// true when the ceiling went down.
    fn lower_to(&self, to: usize, wording: fn(usize, usize) -> String) -> bool {
        let to = to.max(1);
        let _moves = self.moves();
        let from = self.ceiling.fetch_min(to, Ordering::SeqCst);
        self.limit.fetch_min(to, Ordering::SeqCst);
        if to < from {
            (self.say)(&wording(from, to));
        }
        to < from
    }

    /// The backend refused `times` for rate limiting: run half as many from
    /// now on, at most once per `backoff_every` however many refusals arrive
    /// together. Every refusal restarts the quiet stretch before regrowing.
    fn rate_limited(&self, times: usize) {
        self.refusals.fetch_add(times, Ordering::SeqCst);
        let mut moves = self.moves();
        let now = (self.clock)();
        moves.refused = true;
        moves.last_event = now;
        if moves
            .last_halved
            .is_some_and(|t| now.saturating_sub(t) < self.pacing.backoff_every)
        {
            return;
        }
        let from = self.limit();
        let to = capacity::halve(from);
        if to < from {
            self.limit.store(to, Ordering::SeqCst);
            (self.say)(&words::rate_limited(from, to));
            moves.last_halved = Some(now);
        }
    }

    /// Notice rate-limit refusals the backend layer has counted since last time.
    fn poll(&self) {
        let now = (self.hits)();
        let before = self.seen.swap(now, Ordering::SeqCst);
        if now > before {
            self.rate_limited(now - before);
        }
    }

    /// Let one more agent in when the pool is below its ceiling and has been
    /// quiet for a full interval: `ramp_every` while nothing has been refused,
    /// `regrow_after` once something has.
    fn grow(&self) {
        let mut moves = self.moves();
        let (limit, ceiling) = (self.limit(), self.ceiling());
        if limit >= ceiling {
            return;
        }
        let wait = if moves.refused {
            self.pacing.regrow_after
        } else {
            self.pacing.ramp_every
        };
        let now = (self.clock)();
        if now.saturating_sub(moves.last_event) < wait {
            return;
        }
        let to = capacity::grown(limit, ceiling);
        self.limit.store(to, Ordering::SeqCst);
        moves.last_event = now;
        if moves.refused {
            (self.say)(&words::pool_regrew(limit, to));
        }
        drop(moves);
        self.turn.notify_all();
    }

    /// Wait for a place in the pool.
    fn enter(&self) {
        self.poll();
        self.grow();
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        while *active >= self.limit() {
            active = self
                .turn
                .wait_timeout(active, Duration::from_millis(100))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            self.poll();
            self.grow();
        }
        *active += 1;
        self.peak.fetch_max(*active, Ordering::SeqCst);
    }

    fn leave(&self) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        self.turn.notify_all();
    }
}

/// Group the parts that would write the same file (a card, its downloaded
/// datasheet, its rejected draft) so one worker takes them in turn.
fn lanes(work: &[&UnmodelledPart]) -> Vec<Vec<usize>> {
    let mut lanes: Vec<(String, Vec<usize>)> = Vec::new();
    for (at, p) in work.iter().enumerate() {
        let key = datasheet::sanitise_filename(&p.part).to_ascii_lowercase();
        match lanes.iter_mut().find(|(k, _)| *k == key) {
            Some((_, lane)) => lane.push(at),
            None => lanes.push((key, vec![at])),
        }
    }
    lanes.into_iter().map(|(_, lane)| lane).collect()
}

/// Run `job(0..count)` on the `pool`'s threads, every lane (indexes that must
/// not overlap) on one thread in order, and return each result by index. A job
/// that panics leaves `None` and the rest carry on. While it runs,
/// `announce(done, count, running)` is called whenever `every` passes without
/// one having been made.
fn schedule<T: Send>(
    count: usize,
    lanes: Vec<Vec<usize>>,
    pool: &Pool<'_>,
    every: Duration,
    job: &(dyn Fn(usize) -> T + Sync),
    announce: &(dyn Fn(usize, usize, usize) + Sync),
) -> Vec<Option<T>> {
    let slots: Vec<Mutex<Option<T>>> = (0..count).map(|_| Mutex::new(None)).collect();
    let workers = pool.planned.clamp(1, MAX_JOBS).min(lanes.len().max(1));
    if workers == 1 {
        for at in lanes.into_iter().flatten() {
            let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(at)));
            *slots[at].lock().unwrap_or_else(PoisonError::into_inner) = made.ok();
        }
        return slots
            .into_iter()
            .map(|s| s.into_inner().unwrap_or_else(PoisonError::into_inner))
            .collect();
    }
    let queue = Mutex::new(VecDeque::from(lanes));
    let (done, finished) = (Mutex::new(0usize), Condvar::new());
    let running = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let lane = queue
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .pop_front();
                let Some(lane) = lane else { break };
                for at in lane {
                    pool.enter();
                    running.fetch_add(1, Ordering::SeqCst);
                    let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(at)));
                    *slots[at].lock().unwrap_or_else(PoisonError::into_inner) = made.ok();
                    running.fetch_sub(1, Ordering::SeqCst);
                    pool.leave();
                    *done.lock().unwrap_or_else(PoisonError::into_inner) += 1;
                    finished.notify_all();
                }
            });
        }
        let mut last = Instant::now();
        let mut now = done.lock().unwrap_or_else(PoisonError::into_inner);
        while *now < count {
            let left = every.saturating_sub(last.elapsed());
            now = finished
                .wait_timeout(now, left.max(Duration::from_millis(1)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            if *now < count && last.elapsed() >= every {
                announce(*now, count, running.load(Ordering::SeqCst));
                last = Instant::now();
            }
        }
        drop(now);
    });
    slots
        .into_iter()
        .map(|s| s.into_inner().unwrap_or_else(PoisonError::into_inner))
        .collect()
}

/// Get one part's datasheet and draft its card.
fn run_part(
    p: &UnmodelledPart,
    acquirer: &Acquirer<'_>,
    template: &Args,
    kind_of: &(dyn Fn(&UnmodelledPart) -> String + Sync),
    extract: Extract<'_>,
    console: &Console,
    pool: Option<&Pool<'_>>,
) -> PartRun {
    progress::with_label(&p.part, || {
        let want = Wanted {
            part: p.part.clone(),
            aliases: p.aliases.clone(),
            footprints: p.footprints.clone(),
            description: p.description.clone(),
            manufacturer: p.manufacturer.clone(),
        };
        let sources: Vec<String> = p
            .datasheet
            .iter()
            .chain(&p.more_datasheets)
            .cloned()
            .collect();
        let got = acquirer.acquire(&want, &sources, &mut |line| console.say(&p.part, &line));
        let pdf = match got {
            Outcome::Ready(ready) => ready.pdf,
            Outcome::NoDatasheet(missing) => {
                console.say(&p.part, &words::no_datasheet_found(&p.part, &missing.why));
                return PartRun {
                    ended: Ended::NoDatasheet(missing),
                    needs: Vec::new(),
                };
            }
        };
        let args = Args {
            pdf,
            part: p.part.clone(),
            aliases: p.aliases.clone(),
            footprints: p.footprints.clone(),
            kind_str: kind_of(p),
            ..template.clone()
        }
        .board(p.board.clone());
        let ended = match extract(args) {
            Ok(path) => {
                console.say(&p.part, &format!("{}: drafted {}", p.part, path.display()));
                Ended::Drafted
            }
            Err(e) => {
                let why = format!("{e:#}");
                if capacity::is_rate_limited(&why) {
                    if let Some(pool) = pool {
                        pool.rate_limited(1);
                    }
                }
                console.complain(&p.part, &format!("{}: {why}", p.part));
                Ended::Failed(why)
            }
        };
        PartRun {
            ended,
            needs: progress::take_questions(&p.part),
        }
    })
}

/// Draft a card for each part, up to `jobs` at a time.
///
/// The pool ramps up to that, halves when the backend rate limits and grows
/// back after a quiet stretch. A part that fails is reported and the batch
/// carries on.
pub fn draft_parts(
    work: &[&UnmodelledPart],
    acquirer: &Acquirer<'_>,
    template: &Args,
    kind_of: &(dyn Fn(&UnmodelledPart) -> String + Sync),
    extract: Extract<'_>,
    jobs: usize,
    opts: &BatchOptions,
) -> Tally {
    let lanes = lanes(work);
    let start = jobs.clamp(1, MAX_JOBS).min(lanes.len().max(1));
    let concurrent = start > 1;
    let console = Console {
        prefixed: concurrent,
    };
    hauksbee_models::extract_usage::reset_calls();
    if concurrent {
        // One ledger for the whole batch, reported once at the end.
        progress::set_concurrent(true);
        hauksbee_models::extract_usage::reset();
    }
    let say_pool = |text: &str| console.say("", text);
    let hits = progress::rate_limit_hits;
    let origin = Instant::now();
    let clock = || origin.elapsed();
    let pacing = Pacing {
        backoff_every: opts.backoff_every,
        ramp_every: opts.ramp_every,
        regrow_after: opts.regrow_after,
    };
    let pool = Pool::new(start, pacing, &clock, &hits, &say_pool);
    let announce = |done: usize, total: usize, running: usize| {
        let plan = opts.watch.and_then(|w| {
            let now = (opts.reading)()?.session?;
            let (per, observed) =
                capacity::refined_per_part(w.prior_session, w.start_session, Some(now), done);
            let lowered = capacity::relimit(total - done, now, per, pool.ceiling());
            pool.lower_to(lowered, words::window_tight);
            Some(words::plan_now(now, per, observed))
        });
        let line = plan.map_or_else(
            || words::progress(done, total, running),
            |plan| words::progress_plan(done, total, running, &plan),
        );
        console.say("", &line);
    };
    let ran = schedule(
        work.len(),
        lanes,
        &pool,
        opts.progress_every,
        &|at| {
            run_part(
                work[at],
                acquirer,
                template,
                kind_of,
                extract,
                &console,
                Some(&pool),
            )
        },
        &announce,
    );
    // The parts' shared codex base sessions are no longer needed.
    hauksbee_models::extract_cache::codex::release_bases();
    if concurrent {
        progress::set_concurrent(false);
        let cost = hauksbee_models::extract_usage::summary();
        if !cost.is_empty() {
            println!("[model-extract] {cost}");
        }
    }
    let mut tally = Tally {
        pool: pool.stats(),
        ..Tally::default()
    };
    for (p, run) in work.iter().zip(ran) {
        let Some(run) = run else {
            tally
                .failed
                .push((p.part.clone(), "the extraction crashed".into()));
            continue;
        };
        match run.ended {
            Ended::Drafted => tally.drafted += 1,
            Ended::NoDatasheet(m) => tally.no_datasheet.push((p.part.clone(), m)),
            Ended::Failed(why) => tally.failed.push((p.part.clone(), why)),
        }
        tally
            .needs
            .extend(run.needs.into_iter().map(|question| Need {
                part: p.part.clone(),
                question,
            }));
    }
    tally
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn no_hits() -> usize {
        0
    }

    /// A plan nearly used up (the batch's `reading` hook has this fixed shape).
    #[allow(clippy::unnecessary_wraps)]
    fn nearly_spent() -> Option<Reading> {
        Some(Reading {
            week: Some(10.0),
            session: Some(95.0),
        })
    }

    fn silent(_: &str) {}

    fn zero_clock() -> Duration {
        Duration::ZERO
    }

    fn pacing(backoff: u64, ramp: u64, regrow: u64) -> Pacing {
        Pacing {
            backoff_every: Duration::from_secs(backoff),
            ramp_every: Duration::from_secs(ramp),
            regrow_after: Duration::from_secs(regrow),
        }
    }

    /// A pool already at its full size, for the scheduling tests.
    fn test_pool(n: usize) -> Pool<'static> {
        let pool = Pool::new(n, pacing(0, 0, 0), &zero_clock, &no_hits, &silent);
        pool.limit.store(n, Ordering::SeqCst);
        pool
    }

    fn fake_parts(n: usize) -> Vec<UnmodelledPart> {
        (0..n).map(|i| named(&format!("P{i}"))).collect()
    }

    /// A batch whose parts all have a PDF supplied, so only the fake extractor
    /// runs: returns the temp dir to keep it alive.
    fn supplied_dir(parts: &[UnmodelledPart]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for p in parts {
            std::fs::write(dir.path().join(format!("{}.pdf", p.part)), b"%PDF-1.4\n").unwrap();
        }
        dir
    }

    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// A backend that answers 429 halves the pool; the refusals are failed
    /// extractions, not lost, and within the quiet interval the pool stays down.
    #[test]
    fn a_rate_limited_backend_halves_the_pool_and_the_run_carries_on() {
        use hauksbee_models::datasheet_fetch::{CurlHttp, HostLedger};
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let parts = fake_parts(8);
        let dir = supplied_dir(&parts);
        let supplied = [dir.path().to_path_buf()];
        let (http, hosts) = (CurlHttp::default(), HostLedger::default());
        let fetched = dir.path().join("fetched");
        let acquirer = Acquirer {
            http: &http,
            searcher: None,
            dir: &fetched,
            supplied: &supplied,
            hosts: &hosts,
            read_text: |_| String::new(),
        };
        let work: Vec<&UnmodelledPart> = parts.iter().collect();
        let template = Args::new(PathBuf::new(), String::new(), String::new());
        let kind_of = |_: &UnmodelledPart| String::from("opamp");
        let extract = |args: Args| -> anyhow::Result<PathBuf> {
            std::thread::sleep(Duration::from_millis(20));
            if args.part == "P0" {
                anyhow::bail!("HTTP 429 Too Many Requests");
            }
            Ok(PathBuf::from(format!("{}.toml", args.part)))
        };
        let opts = BatchOptions {
            backoff_every: Duration::from_secs(60),
            progress_every: Duration::from_secs(60),
            ramp_every: Duration::from_secs(60),
            regrow_after: Duration::from_secs(60),
            ..BatchOptions::default()
        };
        let tally = draft_parts(&work, &acquirer, &template, &kind_of, &extract, 4, &opts);
        let pool = tally.pool;
        assert_eq!(
            (pool.planned, pool.last, pool.rate_limits),
            (4, 1, 1),
            "started at 2, halved once on P0's 429, no regrowth inside the interval"
        );
        assert!(pool.peak <= 2, "never above the ramp's start: {pool:?}");
        assert_eq!(tally.drafted, 7);
        assert_eq!(tally.failed.len(), 1);
        assert!(tally.failed[0].1.contains("429"));
    }

    /// Refusals the backend layer counted (retried inside the extraction) shrink
    /// the pool too, though no part failed.
    #[test]
    fn counted_rate_limit_refusals_shrink_the_pool_once_per_interval() {
        let said = Mutex::new(Vec::<String>::new());
        let hits = AtomicUsize::new(0);
        let read = || hits.load(Ordering::SeqCst);
        let say = |t: &str| said.lock().unwrap().push(t.to_string());
        let pool = Pool::new(8, pacing(60, 10, 60), &zero_clock, &read, &say);
        pool.limit.store(8, Ordering::SeqCst);
        pool.poll();
        assert_eq!(pool.limit(), 8, "no refusals, no change");
        hits.fetch_add(3, Ordering::SeqCst);
        pool.poll();
        assert_eq!(pool.limit(), 4);
        hits.fetch_add(1, Ordering::SeqCst);
        pool.poll();
        assert_eq!(pool.limit(), 4, "a burst inside the interval halves once");
        assert_eq!(said.lock().unwrap().len(), 1);
        assert!(said.lock().unwrap()[0].contains("running 4 agents instead of 8"));
        assert!(
            pool.lower_to(6, words::window_tight),
            "headroom lowers the ceiling"
        );
        assert!(!pool.lower_to(7, words::window_tight), "never raised");
        assert_eq!(pool.limit(), 4, "a ceiling above the pool leaves it be");
        assert_eq!(pool.stats().rate_limits, 4, "every refusal is counted");
    }

    /// The progress line carries the plan's state, and a window that is
    /// draining faster than the prior lowers the pool.
    #[test]
    fn progress_reports_the_plan_and_a_draining_window_lowers_the_pool() {
        use hauksbee_models::datasheet_fetch::{CurlHttp, HostLedger};
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
        let parts = fake_parts(12);
        let dir = supplied_dir(&parts);
        let supplied = [dir.path().to_path_buf()];
        let (http, hosts) = (CurlHttp::default(), HostLedger::default());
        let fetched = dir.path().join("fetched");
        let acquirer = Acquirer {
            http: &http,
            searcher: None,
            dir: &fetched,
            supplied: &supplied,
            hosts: &hosts,
            read_text: |_| String::new(),
        };
        let work: Vec<&UnmodelledPart> = parts.iter().collect();
        let template = Args::new(PathBuf::new(), String::new(), String::new());
        let kind_of = |_: &UnmodelledPart| String::from("opamp");
        let extract = |args: Args| -> anyhow::Result<PathBuf> {
            std::thread::sleep(Duration::from_millis(150));
            Ok(PathBuf::from(format!("{}.toml", args.part)))
        };
        // The plan reads 95% used, from 40% when the batch began: once a few
        // parts are done that is several points per part, far above the 0.8
        // prior, so the 5 points left cannot carry the parts still to do.
        let opts = BatchOptions {
            progress_every: Duration::from_millis(400),
            backoff_every: Duration::from_secs(60),
            ramp_every: Duration::ZERO,
            watch: Some(PlanWatch {
                start_session: Some(40.0),
                prior_session: 0.8,
            }),
            reading: nearly_spent,
            ..BatchOptions::default()
        };
        let tally = draft_parts(&work, &acquirer, &template, &kind_of, &extract, 4, &opts);
        assert_eq!(tally.drafted, 12);
        assert!(tally.pool.last < 4, "pool was lowered: {:?}", tally.pool);
    }

    #[test]
    fn parts_that_would_share_a_file_share_a_lane() {
        let part = |name: &str| UnmodelledPart {
            part: name.into(),
            aliases: vec![],
            footprints: vec![],
            references: vec![],
            datasheet: None,
            more_datasheets: vec![],
            description: None,
            manufacturer: None,
            doubt: None,
            board: None,
        };
        let parts = [part("A/B"), part("C"), part("A_B"), part("a?b")];
        let refs: Vec<&UnmodelledPart> = parts.iter().collect();
        assert_eq!(lanes(&refs), vec![vec![0, 2, 3], vec![1]]);
    }

    /// Eight jobs of 250 ms on four threads take about two rounds, not eight;
    /// never more than four at once; every result lands at its own index.
    #[test]
    fn jobs_run_in_parallel_up_to_the_bound_and_results_keep_their_places() {
        let (in_flight, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let lanes: Vec<Vec<usize>> = (0..8).map(|i| vec![i]).collect();
        let started = Instant::now();
        let out = schedule(
            8,
            lanes,
            &test_pool(4),
            Duration::from_secs(60),
            &|i| {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(250));
                in_flight.fetch_sub(1, Ordering::SeqCst);
                i * 10
            },
            &|_, _, _| {},
        );
        let took = started.elapsed();
        assert_eq!(
            out,
            (0..8).map(|i| Some(i * 10)).collect::<Vec<_>>(),
            "results by index"
        );
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert!(
            took < Duration::from_millis(1400),
            "serial would be 2 s; took {took:?}"
        );
    }

    /// One lane is never run in two places at once, and stays in order.
    #[test]
    fn a_lane_runs_in_order_on_one_thread_at_a_time() {
        let busy = AtomicUsize::new(0);
        let order = Mutex::new(Vec::new());
        schedule(
            4,
            vec![vec![0, 1, 2], vec![3]],
            &test_pool(4),
            Duration::from_secs(60),
            &|i| {
                if i < 3 {
                    assert_eq!(busy.fetch_add(1, Ordering::SeqCst), 0, "overlap in a lane");
                    order.lock().unwrap().push(i);
                    std::thread::sleep(Duration::from_millis(30));
                    busy.fetch_sub(1, Ordering::SeqCst);
                }
            },
            &|_, _, _| {},
        );
        assert_eq!(*order.lock().unwrap(), [0, 1, 2]);
    }

    /// A long quiet stretch says how far it has got: the counts are right and
    /// it speaks again each time the interval passes.
    #[test]
    fn a_quiet_batch_reports_progress() {
        let said = Mutex::new(Vec::new());
        schedule(
            4,
            (0..4).map(|i| vec![i]).collect(),
            &test_pool(2),
            Duration::from_millis(60),
            &|_| std::thread::sleep(Duration::from_millis(300)),
            &|done, total, running| said.lock().unwrap().push((done, total, running)),
        );
        let said = said.into_inner().unwrap();
        assert!(said.len() >= 3, "{said:?}");
        assert_eq!(said[0], (0, 4, 2), "{said:?}");
        assert!(said.iter().all(|s| s.1 == 4 && s.2 <= 2), "{said:?}");
        assert!(said.iter().any(|s| s.0 == 2), "{said:?}");
        assert_eq!(
            words::progress(2, 4, 2),
            "[batch] 2 of 4 parts done, 2 running"
        );
    }

    fn named(name: &str) -> UnmodelledPart {
        UnmodelledPart {
            part: name.into(),
            aliases: vec![],
            footprints: vec![],
            references: vec![],
            datasheet: None,
            more_datasheets: vec![],
            description: None,
            manufacturer: None,
            doubt: None,
            board: None,
        }
    }

    /// Eight parts through the real `draft_parts` with a fake extractor that
    /// takes 250 ms each: four at a time is about a quarter of the serial time,
    /// and the tally keeps the three outcomes apart, in board order.
    #[test]
    fn a_batch_of_eight_with_four_jobs_takes_about_a_quarter_of_the_serial_time() {
        use hauksbee_models::datasheet_discovery::supplied_pdf;
        use hauksbee_models::datasheet_fetch::{CurlHttp, HostLedger};
        let dir = tempfile::tempdir().unwrap();
        // Seven parts have a PDF supplied; P7 has none and no searcher, so it
        // is a part with no datasheet.
        let names: Vec<String> = (0..8).map(|i| format!("P{i}")).collect();
        for n in &names[..7] {
            std::fs::write(dir.path().join(format!("{n}.pdf")), b"%PDF-1.4\n").unwrap();
        }
        let supplied = [dir.path().to_path_buf()];
        assert!(supplied_pdf(&supplied, "P3", &[]).is_some());
        let (http, hosts) = (CurlHttp::default(), HostLedger::default());
        let fetched = dir.path().join("fetched");
        let acquirer = Acquirer {
            http: &http,
            searcher: None,
            dir: &fetched,
            supplied: &supplied,
            hosts: &hosts,
            read_text: |_| String::new(),
        };
        let parts: Vec<UnmodelledPart> = names.iter().map(|n| named(n)).collect();
        let work: Vec<&UnmodelledPart> = parts.iter().collect();
        let template = Args::new(PathBuf::new(), String::new(), String::new());
        let kind_of = |_: &UnmodelledPart| String::from("opamp");
        let (running, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let extract = |args: Args| -> anyhow::Result<PathBuf> {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(250));
            running.fetch_sub(1, Ordering::SeqCst);
            if args.part == "P5" {
                anyhow::bail!("the model refused");
            }
            Ok(PathBuf::from(format!("{}.toml", args.part)))
        };
        let run = |jobs: usize| {
            let opts = BatchOptions {
                jobs: Some(jobs),
                progress_every: Duration::from_secs(60),
                ramp_every: Duration::ZERO,
                ..BatchOptions::default()
            };
            let started = Instant::now();
            let tally = draft_parts(&work, &acquirer, &template, &kind_of, &extract, jobs, &opts);
            (tally, started.elapsed())
        };
        let (tally, fast) = run(4);
        assert_eq!(peak.load(Ordering::SeqCst), 4);
        assert_eq!(tally.drafted, 6);
        assert_eq!(tally.failed.len(), 1);
        assert_eq!(tally.failed[0].0, "P5");
        assert!(tally.failed[0].1.contains("refused"));
        assert_eq!(tally.no_datasheet.len(), 1);
        assert_eq!(tally.no_datasheet[0].0, "P7");
        assert!(
            !progress::concurrent(),
            "the batch leaves the flag as it found it"
        );
        peak.store(0, Ordering::SeqCst);
        let (serial_tally, slow) = run(1);
        assert_eq!(serial_tally.drafted, 6);
        assert_eq!(peak.load(Ordering::SeqCst), 1, "jobs 1 never overlaps");
        assert!(
            fast * 2 < slow,
            "4 jobs {fast:?} should be well under half of serial {slow:?}"
        );
        assert!(slow >= Duration::from_millis(7 * 250), "{slow:?}");
    }

    /// A pool on a fake clock (seconds) with a settable count of refusals.
    struct Fake {
        secs: AtomicUsize,
        hits: AtomicUsize,
        said: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                secs: AtomicUsize::new(0),
                hits: AtomicUsize::new(0),
                said: Mutex::new(Vec::new()),
            }
        }

        fn at(&self, secs: usize) {
            self.secs.store(secs, Ordering::SeqCst);
        }

        fn refuse(&self, n: usize) {
            self.hits.fetch_add(n, Ordering::SeqCst);
        }
    }

    /// Run `f` with a pool of `ceiling` on the fake clock: backoff 30 s, ramp
    /// every 10 s, regrow after 60 s quiet.
    fn with_fake_pool(ceiling: usize, f: impl FnOnce(&Pool<'_>, &Fake)) {
        let fake = Fake::new();
        let clock = || Duration::from_secs(fake.secs.load(Ordering::SeqCst) as u64);
        let hits = || fake.hits.load(Ordering::SeqCst);
        let say = |t: &str| fake.said.lock().unwrap().push(t.to_string());
        let pool = Pool::new(ceiling, pacing(30, 10, 60), &clock, &hits, &say);
        f(&pool, &fake);
    }

    #[test]
    fn the_pool_starts_with_a_couple_and_ramps_up_one_per_interval_to_the_ceiling() {
        with_fake_pool(4, |pool, fake| {
            assert_eq!(pool.limit(), 2, "a couple at the start, not all at once");
            let mut seen = vec![];
            for t in [0, 9, 10, 15, 19, 20, 29, 30, 60, 600] {
                fake.at(t);
                pool.grow();
                seen.push((t, pool.limit()));
            }
            assert_eq!(
                seen,
                [
                    (0, 2),
                    (9, 2),
                    (10, 3),
                    (15, 3),
                    (19, 3),
                    (20, 4),
                    (29, 4),
                    (30, 4),
                    (60, 4),
                    (600, 4)
                ],
                "one more per 10 s, never above the ceiling"
            );
            assert!(fake.said.lock().unwrap().is_empty(), "the ramp is silent");
        });
        with_fake_pool(2, |pool, fake| {
            fake.at(1000);
            pool.grow();
            assert_eq!(pool.limit(), 2, "a ceiling of the start size does not move");
        });
        with_fake_pool(1, |pool, _| assert_eq!(pool.limit(), 1));
    }

    #[test]
    fn a_rate_limit_halves_the_pool_at_most_once_per_backoff_interval() {
        with_fake_pool(8, |pool, fake| {
            pool.limit.store(8, Ordering::SeqCst);
            fake.at(5);
            fake.refuse(3);
            pool.poll();
            assert_eq!(pool.limit(), 4);
            fake.at(20);
            fake.refuse(1);
            pool.poll();
            assert_eq!(
                pool.limit(),
                4,
                "inside the 30 s backoff: no second halving"
            );
            fake.at(36);
            fake.refuse(1);
            pool.poll();
            assert_eq!(pool.limit(), 2, "after the interval it halves again");
            assert_eq!(pool.stats().rate_limits, 5);
            assert_eq!(fake.said.lock().unwrap().len(), 2);
            // Never below one.
            for t in [100, 200, 300] {
                fake.at(t);
                pool.rate_limited(1);
            }
            assert_eq!(pool.limit(), 1);
        });
    }

    #[test]
    fn the_pool_grows_back_one_at_a_time_after_a_quiet_stretch_never_above_the_ceiling() {
        with_fake_pool(4, |pool, fake| {
            fake.at(30);
            pool.grow();
            fake.at(40);
            pool.grow();
            assert_eq!(pool.limit(), 4);
            fake.at(100);
            fake.refuse(2);
            pool.poll();
            assert_eq!(pool.limit(), 2, "halved");
            fake.at(159);
            pool.grow();
            assert_eq!(pool.limit(), 2, "59 s quiet is not enough");
            fake.at(160);
            pool.grow();
            assert_eq!(pool.limit(), 3, "60 s quiet: one more");
            pool.grow();
            assert_eq!(pool.limit(), 3, "one step per quiet interval");
            // A new refusal inside the interval restarts the quiet stretch.
            fake.at(200);
            fake.refuse(1);
            pool.poll();
            assert_eq!(pool.limit(), 1, "halved again (3 to 1) after the backoff");
            fake.at(259);
            pool.grow();
            assert_eq!(pool.limit(), 1);
            for t in [260, 320, 380, 440, 500, 900] {
                fake.at(t);
                pool.grow();
            }
            assert_eq!(pool.limit(), 4, "back to, and never past, the ceiling");
            let said = fake.said.lock().unwrap().clone();
            assert!(
                said.iter()
                    .any(|l| l == "[batch] the backend has stopped rate limiting; running 3 agents instead of 2 again"),
                "{said:?}"
            );
            assert_eq!(pool.stats().rate_limits, 3);
        });
    }

    #[test]
    fn headroom_only_lowers_the_ceiling_and_the_ramp_respects_it() {
        with_fake_pool(4, |pool, fake| {
            assert!(pool.lower_to(3, words::window_tight));
            assert_eq!((pool.ceiling(), pool.limit()), (3, 2));
            assert!(!pool.lower_to(4, words::window_tight), "never raised");
            for t in [10, 20, 30, 40] {
                fake.at(t);
                pool.grow();
            }
            assert_eq!(pool.limit(), 3, "ramps to the lowered ceiling and stops");
            assert!(pool.lower_to(2, words::window_tight));
            assert_eq!(
                (pool.ceiling(), pool.limit()),
                (2, 2),
                "pulls the pool down"
            );
            fake.at(1000);
            pool.grow();
            assert_eq!(pool.limit(), 2);
        });
    }

    /// Four agents' starts are spread by the ramp, not all at the same moment:
    /// the third cannot begin before one interval, the fourth before two.
    #[test]
    fn agents_start_staggered_while_nothing_is_refused() {
        let origin = Instant::now();
        let clock = || origin.elapsed();
        let pool = Pool::new(4, pacing(30, 0, 60), &clock, &no_hits, &silent);
        let pool = Pool {
            pacing: Pacing {
                ramp_every: Duration::from_millis(150),
                ..pool.pacing
            },
            ..pool
        };
        let starts = Mutex::new(Vec::new());
        schedule(
            4,
            (0..4).map(|i| vec![i]).collect(),
            &pool,
            Duration::from_secs(60),
            &|_| {
                starts.lock().unwrap().push(origin.elapsed());
                std::thread::sleep(Duration::from_millis(500));
            },
            &|_, _, _| {},
        );
        let mut starts = starts.into_inner().unwrap();
        starts.sort();
        assert!(starts[2] >= Duration::from_millis(150), "{starts:?}");
        assert!(starts[3] >= Duration::from_millis(300), "{starts:?}");
        assert_eq!(pool.stats().peak, 4);
    }

    /// A job that panics is a gap, not the end of the batch.
    #[test]
    fn a_panicking_job_leaves_a_gap_and_the_rest_finish() {
        let out = schedule(
            3,
            (0..3).map(|i| vec![i]).collect(),
            &test_pool(2),
            Duration::from_secs(60),
            &|i| {
                assert_ne!(i, 1, "boom");
                i
            },
            &|_, _, _| {},
        );
        assert_eq!(out, vec![Some(0), None, Some(2)]);
    }
}
