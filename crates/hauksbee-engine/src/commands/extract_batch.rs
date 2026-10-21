//! Running the parts of a `models extract --unmodelled` batch: several at once
//! in stages, one readable line at a time, and one honest tally.
//!
//! A whole board is dozens of parts, each a web search, a download and an
//! agent run that is mostly waiting, so they run side by side. The stages are
//! pipelined (see `super::extract_stages`): getting each part's datasheet (the
//! search and the download) runs on its own bounded downloader pool, outside the
//! agent pool, all parts at once; a part enters the agent pool only when its
//! datasheet is ready, the biggest datasheet first; a part nothing could be
//! fetched for goes straight to the "supply these by hand" list and never takes
//! an agent's place. How many agents run at once is worked out from the plan's
//! headroom (`hauksbee_models::extract_capacity`) unless `--jobs N` says;
//! `--jobs 1` is the plain serial loop with the spinner. The pool is paced,
//! because the limit that bites is how many agents hit the service at once, not
//! the plan's headroom: it starts a couple, adds one every `ramp_every` while
//! nothing is refused, up to the planned ceiling; halves when the backend rate
//! limits (at most once per `backoff_every`); and grows back one at a time after
//! `regrow_after` without a refusal. Only the ceiling is lowered by the plan's
//! headroom (when the observed usage says the 5-hour window will not last),
//! never raised.
//!
//! Sessions start through a gate (`BatchGate`): their starts are spaced
//! (`hauksbee_models::extract_admission`), and a session whose first request is
//! refused waits a few jittered seconds with its agent place given up, so the
//! refusal costs the place nothing. Each part is its own unit of work and ends
//! in exactly one of three ways, and the tally counts them apart:
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

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use hauksbee_ir::{eprintln, println};
use hauksbee_models::datasheet::{self, Args};
use hauksbee_models::datasheet_discovery::{Acquirer, Missing, Outcome, Wanted};
use hauksbee_models::extract_admission::{self as admission, Gate, StartPacer};
use hauksbee_models::extract_capacity as capacity;
use hauksbee_models::extract_messages::{self as words, Need};
use hauksbee_models::extract_plan::{self, Reading};
use hauksbee_models::extract_progress as progress;
use hauksbee_models::extract_stage_messages as stage_words;

use super::extract_stages::{self as stages, HostLimited, StageStats, Stages};
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
    /// Warm the shared codex bases (naming a kind, drafting a kind) while the
    /// datasheets are still being fetched. Off in tests, which must never start
    /// a session.
    pub warm_ahead: bool,
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
            warm_ahead: true,
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
    /// The datasheet stage (`None` for the serial loop, which has none).
    pub stage: Option<StageStats>,
}

impl Tally {
    /// The names of the failed parts.
    #[must_use]
    pub fn failed_names(&self) -> Vec<&str> {
        self.failed.iter().map(|(p, _)| p.as_str()).collect()
    }
}

/// How one part ended.
pub(super) enum Ended {
    Drafted,
    NoDatasheet(Missing),
    Failed(String),
}

pub(super) struct PartRun {
    pub(super) ended: Ended,
    pub(super) needs: Vec<String>,
}

/// Runs one extraction (the real one is [`datasheet::run`] under the spinner).
pub type Extract<'a> = &'a (dyn Fn(Args) -> anyhow::Result<PathBuf> + Sync);

/// Lines from several parts, each whole and each saying whose it is.
#[derive(Clone, Copy)]
pub(super) struct Console {
    pub(super) prefixed: bool,
}

/// One message at a time, so a multi-line message stays together.
static PRINT: Mutex<()> = Mutex::new(());

impl Console {
    pub(super) fn say(self, part: &str, text: &str) {
        self.emit(part, text, false);
    }

    pub(super) fn complain(self, part: &str, text: &str) {
        self.emit(part, text, true);
    }

    fn emit(self, part: &str, text: &str, to_err: bool) {
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
pub type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;
type Hits = Arc<dyn Fn() -> usize + Send + Sync>;
type Say = Arc<dyn Fn(&str) + Send + Sync>;

/// The time a batch runs on: its clock, and how it waits. Both injectable, so
/// the pacing, the retries and the stage clock are tested without sleeping.
#[derive(Clone)]
pub struct Timing {
    pub clock: Clock,
    pub sleep: Arc<dyn Fn(Duration) + Send + Sync>,
}

impl Timing {
    /// The wall clock, from now, and a real sleep.
    #[must_use]
    pub fn real() -> Self {
        let origin = Instant::now();
        Self {
            clock: Arc::new(move || origin.elapsed()),
            sleep: Arc::new(std::thread::sleep),
        }
    }
}

/// How a pool paces itself.
#[derive(Debug, Clone, Copy)]
pub(super) struct Pacing {
    pub(super) backoff_every: Duration,
    pub(super) ramp_every: Duration,
    pub(super) regrow_after: Duration,
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

thread_local! {
    /// Whether this thread holds a place in the agent pool right now.
    static HOLDS_PLACE: Cell<bool> = const { Cell::new(false) };
}

/// Whether the calling thread holds a place in the agent pool.
pub(super) fn holds_place() -> bool {
    HOLDS_PLACE.get()
}

/// How many jobs may run at once: it starts small and ramps up to a ceiling,
/// halves when the backend rate limits and grows back after a quiet stretch.
/// The ceiling itself only ever goes down.
pub(super) struct Pool {
    /// The ceiling the batch was planned with.
    pub(super) planned: usize,
    ceiling: AtomicUsize,
    limit: AtomicUsize,
    active: Mutex<usize>,
    turn: Condvar,
    peak: AtomicUsize,
    refusals: AtomicUsize,
    /// The rate-limit refusals seen when last looked.
    seen: AtomicUsize,
    hits: Hits,
    moves: Mutex<Moves>,
    pacing: Pacing,
    clock: Clock,
    say: Say,
}

impl Pool {
    pub(super) fn new(ceiling: usize, pacing: Pacing, clock: Clock, hits: Hits, say: Say) -> Self {
        let ceiling = ceiling.max(1);
        let last_event = clock();
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
                last_event,
                refused: false,
            }),
            pacing,
            clock,
            say,
        }
    }

    pub(super) fn limit(&self) -> usize {
        self.limit.load(Ordering::SeqCst)
    }

    fn ceiling(&self) -> usize {
        self.ceiling.load(Ordering::SeqCst)
    }

    /// Places in use right now.
    #[cfg(test)]
    pub(super) fn active(&self) -> usize {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn stats(&self) -> capacity::PoolStats {
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
    pub(super) fn rate_limited(&self, times: usize) {
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
    pub(super) fn enter(&self) {
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

    pub(super) fn leave(&self) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) -= 1;
        self.turn.notify_all();
    }

    /// Take a place for the calling thread (see [`holds_place`]).
    pub(super) fn take_place(&self) {
        self.enter();
        HOLDS_PLACE.set(true);
    }

    /// Give the calling thread's place back.
    /// A no-op when the thread holds none, so a panic caught between giving a
    /// place up and taking it back cannot give it up twice.
    pub(super) fn give_up_place(&self) {
        if HOLDS_PLACE.replace(false) {
            self.leave();
        }
    }
}

/// What a batch does about session starts and refusals: spaces the starts of
/// every agent session in the process, counts a refused start as pressure on
/// that spacing, and, while a refused session waits to retry, gives up the
/// agent place its thread holds (if it holds one) so another part can use it.
pub(super) struct BatchGate {
    pool: Arc<Pool>,
    pacer: StartPacer,
    timing: Timing,
}

impl BatchGate {
    pub(super) const fn new(pool: Arc<Pool>, pacer: StartPacer, timing: Timing) -> Self {
        Self {
            pool,
            pacer,
            timing,
        }
    }
}

impl Gate for BatchGate {
    fn before_start(&self) {
        self.pacer.admit(&*self.timing.clock, &*self.timing.sleep);
    }

    fn refused(&self) {
        self.pacer.pressure((self.timing.clock)());
    }

    fn pause(&self, how_long: Duration) {
        if holds_place() {
            self.pool.give_up_place();
            (self.timing.sleep)(how_long);
            self.pool.take_place();
        } else {
            (self.timing.sleep)(how_long);
        }
    }
}

/// Group the parts that would write the same file (a card, its downloaded
/// datasheet, its rejected draft) so one worker takes them in turn.
pub(super) fn lanes(work: &[&UnmodelledPart]) -> Vec<Vec<usize>> {
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

/// Run `job(at)` for every index, lane by lane in order, on this thread, and
/// return each result by index. A job that panics leaves `None` and the rest
/// carry on.
fn run_serial<T>(count: usize, lanes: Vec<Vec<usize>>, job: &dyn Fn(usize) -> T) -> Vec<Option<T>> {
    let mut out: Vec<Option<T>> = (0..count).map(|_| None).collect();
    for at in lanes.into_iter().flatten() {
        out[at] = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(at))).ok();
    }
    out
}

/// What the datasheet search and the extraction are told about a part.
pub(super) fn wanted(p: &UnmodelledPart) -> (Wanted, Vec<String>) {
    let want = Wanted {
        part: p.part.clone(),
        aliases: p.aliases.clone(),
        footprints: p.footprints.clone(),
        description: p.description.clone(),
        manufacturer: p.manufacturer.clone(),
    };
    let sources = p
        .datasheet
        .iter()
        .chain(&p.more_datasheets)
        .cloned()
        .collect();
    (want, sources)
}

/// The extraction's arguments for `p` on the datasheet `pdf`, drafting `kind`
/// (empty: the extraction names it itself).
pub(super) fn args_for(p: &UnmodelledPart, template: &Args, pdf: PathBuf, kind: String) -> Args {
    Args {
        pdf,
        part: p.part.clone(),
        aliases: p.aliases.clone(),
        footprints: p.footprints.clone(),
        kind_str: kind,
        ..template.clone()
    }
    .board(p.board.clone())
}

/// A part nothing could be fetched for: said, and listed for the person.
pub(super) fn no_datasheet(p: &UnmodelledPart, missing: Missing, console: Console) -> PartRun {
    console.say(&p.part, &words::no_datasheet_found(&p.part, &missing.why));
    PartRun {
        ended: Ended::NoDatasheet(missing),
        needs: Vec::new(),
    }
}

/// A part whose run was refused: said, and a rate limit counted against the pool.
pub(super) fn failure(
    p: &UnmodelledPart,
    e: &anyhow::Error,
    console: Console,
    pool: &Pool,
) -> PartRun {
    let why = format!("{e:#}");
    if capacity::is_rate_limited(&why) {
        pool.rate_limited(1);
    }
    console.complain(&p.part, &format!("{}: {why}", p.part));
    PartRun {
        ended: Ended::Failed(why),
        needs: progress::take_questions(&p.part),
    }
}

/// Draft one part's card from its datasheet.
pub(super) fn draft_card(
    p: &UnmodelledPart,
    args: Args,
    extract: Extract<'_>,
    console: Console,
    pool: &Pool,
) -> PartRun {
    match extract(args) {
        Ok(path) => {
            console.say(&p.part, &format!("{}: drafted {}", p.part, path.display()));
            PartRun {
                ended: Ended::Drafted,
                needs: progress::take_questions(&p.part),
            }
        }
        Err(e) => failure(p, &e, console, pool),
    }
}

/// Get one part's datasheet and draft its card, all on this thread (the serial
/// loop, and a lane of parts that share a file).
pub(super) fn run_part(
    p: &UnmodelledPart,
    stages: &Stages<'_>,
    template: &Args,
    console: Console,
    pool: &Pool,
) -> PartRun {
    progress::with_label(&p.part, || {
        let got = (stages.acquire)(p, &mut |line| console.say(&p.part, &line));
        match got {
            Outcome::Ready(ready) => {
                let args = args_for(p, template, ready.pdf, (stages.kind_of)(p));
                draft_card(p, args, stages.extract, console, pool)
            }
            Outcome::NoDatasheet(missing) => no_datasheet(p, missing, console),
        }
    })
}

/// Draft a card for each part, up to `jobs` at a time.
///
/// The datasheets are fetched on their own pool, outside the agents'; the agent
/// pool ramps up to `jobs`, halves when the backend rate limits and grows back
/// after a quiet stretch. A part that fails is reported and the batch carries
/// on.
pub fn draft_parts(
    work: &[&UnmodelledPart],
    acquirer: &Acquirer<'_>,
    template: &Args,
    kind_of: &(dyn Fn(&UnmodelledPart) -> String + Sync),
    extract: Extract<'_>,
    jobs: usize,
    opts: &BatchOptions,
) -> Tally {
    // No more than a couple of fetches to one host at a time.
    let limited = HostLimited::new(acquirer.http, capacity::PER_HOST);
    let fetching = Acquirer {
        http: &limited,
        searcher: acquirer.searcher,
        dir: acquirer.dir,
        supplied: acquirer.supplied,
        hosts: acquirer.hosts,
        read_text: acquirer.read_text,
    };
    let acquire = |p: &UnmodelledPart, say: &mut dyn FnMut(String)| {
        let (want, sources) = wanted(p);
        fetching.acquire(&want, &sources, say)
    };
    let size_of = |pdf: &Path| std::fs::metadata(pdf).map_or(0, |m| m.len());
    let warm_naming = || {
        let _ = datasheet::warm_identify_base(template);
    };
    let warm_kind = |part: &str, kind: &str| {
        let _ = datasheet::warm_kind_base(template, part, kind);
    };
    let stages = Stages {
        kind_of,
        acquire: &acquire,
        size_of: &size_of,
        classify: &datasheet::classify_part,
        warm_naming: opts.warm_ahead.then_some(&warm_naming),
        warm_kind: opts.warm_ahead.then_some(&warm_kind),
        extract,
    };
    draft_parts_with(work, &stages, template, jobs, opts, &Timing::real())
}

/// [`draft_parts`] with every outside effect passed in: how a datasheet is
/// acquired, named, sized and warmed for, how a card is drafted, and the time.
pub fn draft_parts_with(
    work: &[&UnmodelledPart],
    stages: &Stages<'_>,
    template: &Args,
    jobs: usize,
    opts: &BatchOptions,
    timing: &Timing,
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
    let say_pool: Say = Arc::new(move |text: &str| console.say("", text));
    let pacing = Pacing {
        backoff_every: opts.backoff_every,
        ramp_every: opts.ramp_every,
        regrow_after: opts.regrow_after,
    };
    let pool = Arc::new(Pool::new(
        start,
        pacing,
        Arc::clone(&timing.clock),
        Arc::new(progress::rate_limit_hits),
        say_pool,
    ));
    // Every session this batch starts is spaced, and waits out a refusal without
    // holding a place.
    let _gate = admission::install(Arc::new(BatchGate::new(
        Arc::clone(&pool),
        StartPacer::standard(),
        timing.clone(),
    )));
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
    let (ran, stage) = if concurrent {
        let ctx = stages::Ctx {
            work,
            stages,
            template,
            pool: &pool,
            console,
            timing,
            every: opts.progress_every,
            announce: &announce,
        };
        let (ran, stage) = stages::run(&ctx, lanes);
        (ran, Some(stage))
    } else {
        let ran = run_serial(work.len(), lanes, &|at| {
            run_part(work[at], stages, template, console, &pool)
        });
        (ran, None)
    };
    // The parts' shared codex base sessions are no longer needed.
    hauksbee_models::extract_cache::codex::release_bases();
    if concurrent {
        progress::set_concurrent(false);
        let cost = hauksbee_models::extract_usage::summary();
        if !cost.is_empty() {
            println!("[model-extract] {cost}");
        }
    }
    if let Some(stage) = stage.filter(|s| s.covered > 0) {
        println!(
            "[batch] {}",
            stage_words::stage_line(stage.covered, stage.found, stage.ran)
        );
        if let Some(line) = stage_words::given_up_line(stage.given_up) {
            println!("[batch] {line}");
        }
    }
    tally_of(work, ran, pool.stats(), stage)
}

/// The tally of a batch: each part's run, in board order, kept apart by how it ended.
fn tally_of(
    work: &[&UnmodelledPart],
    ran: Vec<Option<PartRun>>,
    pool: capacity::PoolStats,
    stage: Option<StageStats>,
) -> Tally {
    let mut tally = Tally {
        pool,
        stage,
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
    use crate::commands::extract_stages::Acquire;
    use hauksbee_models::datasheet_discovery::Ready;
    use hauksbee_models::datasheet_fetch::Http;
    use hauksbee_models::datasheet_fetch::{Request, Response, Strategy, TransportError};
    use std::collections::HashMap;
    use std::sync::MutexGuard;

    /// Long enough never to matter when things work, short enough that a broken
    /// pipeline fails a test instead of hanging it.
    const PATIENCE: Duration = Duration::from_secs(20);

    /// The tests that run a batch share process-wide state (the session gate, the
    /// progress flags), so they take turns.
    fn one_at_a_time() -> MutexGuard<'static, ()> {
        static TURN: Mutex<()> = Mutex::new(());
        TURN.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Something one thread waits for and another does.
    struct Latch {
        on: Mutex<bool>,
        turn: Condvar,
    }

    impl Latch {
        fn new() -> Self {
            Self {
                on: Mutex::new(false),
                turn: Condvar::new(),
            }
        }

        fn set(&self) {
            *self.on.lock().unwrap() = true;
            self.turn.notify_all();
        }

        /// True when it was set (within [`PATIENCE`]).
        fn wait(&self) -> bool {
            let on = self.on.lock().unwrap();
            *self
                .turn
                .wait_timeout_while(on, PATIENCE, |on| !*on)
                .unwrap()
                .0
        }
    }

    /// `n` threads each wait here until all `n` have arrived.
    struct Meet {
        n: usize,
        arrived: Mutex<usize>,
        turn: Condvar,
    }

    impl Meet {
        fn new(n: usize) -> Self {
            Self {
                n,
                arrived: Mutex::new(0),
                turn: Condvar::new(),
            }
        }

        /// Wait for `n` arrivals without being one of them.
        fn meet_without_joining(&self, n: usize) -> bool {
            let (arrived, _) = self
                .turn
                .wait_timeout_while(self.arrived.lock().unwrap(), PATIENCE, |a| *a < n)
                .unwrap();
            let all = *arrived >= n;
            drop(arrived);
            all
        }

        fn meet(&self) -> bool {
            *self.arrived.lock().unwrap() += 1;
            self.turn.notify_all();
            let n = self.n;
            let (arrived, _) = self
                .turn
                .wait_timeout_while(self.arrived.lock().unwrap(), PATIENCE, |a| *a < n)
                .unwrap();
            let all = *arrived >= n;
            drop(arrived);
            all
        }
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

    fn fake_parts(n: usize) -> Vec<UnmodelledPart> {
        (0..n).map(|i| named(&format!("P{i}"))).collect()
    }

    fn template() -> Args {
        Args::new(PathBuf::new(), String::new(), String::new())
    }

    fn ready(p: &UnmodelledPart, _say: &mut dyn FnMut(String)) -> Outcome {
        Outcome::Ready(Ready {
            pdf: PathBuf::from(format!("{}.pdf", p.part)),
        })
    }

    fn missing() -> Outcome {
        Outcome::NoDatasheet(Missing {
            why: "none found".into(),
            tried: vec![],
        })
    }

    #[allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)] // the Extract hook's fixed shape
    fn drafted(args: Args) -> anyhow::Result<PathBuf> {
        Ok(PathBuf::from(format!("{}.toml", args.part)))
    }

    /// The fakes a test does not care about: every part is an opamp, no datasheet
    /// has a size, nothing is warmed.
    fn stages<'a>(acquire: Acquire<'a>, extract: Extract<'a>) -> Stages<'a> {
        Stages {
            kind_of: &|_: &UnmodelledPart| String::from("opamp"),
            acquire,
            size_of: &|_: &Path| 0,
            classify: &|_: &Args| Ok(String::from("opamp")),
            warm_naming: None,
            warm_kind: None,
            extract,
        }
    }

    fn quick() -> BatchOptions {
        BatchOptions {
            progress_every: Duration::from_secs(60),
            ramp_every: Duration::ZERO,
            warm_ahead: false,
            ..BatchOptions::default()
        }
    }

    fn run_batch(parts: &[UnmodelledPart], jobs: usize, stages: &Stages<'_>) -> Tally {
        let work: Vec<&UnmodelledPart> = parts.iter().collect();
        draft_parts_with(&work, stages, &template(), jobs, &quick(), &Timing::real())
    }

    fn no_hits() -> usize {
        0
    }

    fn silent(_: &str) {}

    fn pacing(backoff: u64, ramp: u64, regrow: u64) -> Pacing {
        Pacing {
            backoff_every: Duration::from_secs(backoff),
            ramp_every: Duration::from_secs(ramp),
            regrow_after: Duration::from_secs(regrow),
        }
    }

    /// A plan nearly used up (the batch's `reading` hook has this fixed shape).
    #[allow(clippy::unnecessary_wraps)] // the hook's fixed shape
    fn nearly_spent() -> Option<Reading> {
        Some(Reading {
            week: Some(10.0),
            session: Some(95.0),
        })
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

    // The pipeline.

    /// Every part's download is in flight at the same moment, outside the agent
    /// places: the downloads of four parts meet before any of them returns.
    #[test]
    fn downloads_overlap_one_another() {
        let _one = one_at_a_time();
        let parts = fake_parts(4);
        let meet = Meet::new(4);
        let acquire = |p: &UnmodelledPart, say: &mut dyn FnMut(String)| {
            assert!(meet.meet(), "the four downloads never overlapped");
            ready(p, say)
        };
        let stages = stages(&acquire, &drafted);
        let tally = run_batch(&parts, 2, &stages);
        assert_eq!(tally.drafted, 4);
        let stage = tally.stage.unwrap();
        assert_eq!((stage.covered, stage.found, stage.given_up), (4, 4, 0));
    }

    /// A download goes on while another part is being drafted: the second part's
    /// datasheet does not arrive until the first is already in an agent place.
    #[test]
    fn downloads_continue_while_parts_are_being_drafted() {
        let _one = one_at_a_time();
        let parts = fake_parts(2);
        let drafting = Latch::new();
        let acquire = |p: &UnmodelledPart, say: &mut dyn FnMut(String)| {
            if p.part == "P1" {
                assert!(drafting.wait(), "P0 was never drafted while P1 downloaded");
            }
            ready(p, say)
        };
        let extract = |args: Args| {
            if args.part == "P0" {
                drafting.set();
            }
            drafted(args)
        };
        let stages = stages(&acquire, &extract);
        let tally = run_batch(&parts, 2, &stages);
        assert_eq!(tally.drafted, 2, "{:?}", tally.failed);
    }

    /// A part nothing can be fetched for never takes an agent place: with every
    /// place held by a drafting part, the two without a datasheet are still
    /// given up (a part that needed a place would wait behind them).
    #[test]
    fn parts_without_a_datasheet_never_take_an_agent_place() {
        let _one = one_at_a_time();
        let parts = fake_parts(4);
        let given_up = Meet::new(2);
        let acquire = |p: &UnmodelledPart, say: &mut dyn FnMut(String)| {
            if p.part == "P2" || p.part == "P3" {
                *given_up.arrived.lock().unwrap() += 1;
                given_up.turn.notify_all();
                return missing();
            }
            ready(p, say)
        };
        let calls = AtomicUsize::new(0);
        let extract = |args: Args| {
            calls.fetch_add(1, Ordering::SeqCst);
            // Both places are taken by these two until the others were given up.
            assert!(
                given_up.meet_without_joining(2),
                "the datasheet-less parts waited for a place"
            );
            drafted(args)
        };
        let stages = stages(&acquire, &extract);
        let tally = run_batch(&parts, 2, &stages);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "no agent for P2 or P3");
        assert_eq!(tally.drafted, 2);
        let gone: Vec<&str> = tally.no_datasheet.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(gone, ["P2", "P3"]);
        let stage = tally.stage.unwrap();
        assert_eq!((stage.covered, stage.found, stage.given_up), (4, 2, 2));
    }

    /// A kind with one part has nothing to fork from: it is drafted solo and its
    /// base is never warmed; a kind with two is warmed once and not solo.
    #[test]
    fn a_kind_with_a_single_part_skips_the_warm_up() {
        let _one = one_at_a_time();
        let parts = fake_parts(3);
        let kind_of =
            |p: &UnmodelledPart| String::from(if p.part == "P2" { "vreg" } else { "opamp" });
        let warmed = Mutex::new(Vec::new());
        let warm_kind = |_part: &str, kind: &str| warmed.lock().unwrap().push(kind.to_string());
        let solo = Mutex::new(Vec::new());
        let extract = |args: Args| {
            solo.lock()
                .unwrap()
                .push((args.part.clone(), admission::solo()));
            drafted(args)
        };
        let acquire = ready;
        let mut s = stages(&acquire, &extract);
        s.kind_of = &kind_of;
        s.warm_kind = Some(&warm_kind);
        let tally = run_batch(&parts, 2, &s);
        assert_eq!(tally.drafted, 3);
        assert_eq!(*warmed.lock().unwrap(), ["opamp"], "vreg has one part");
        let mut solo = solo.into_inner().unwrap();
        solo.sort();
        assert_eq!(
            solo,
            [
                ("P0".to_string(), false),
                ("P1".to_string(), false),
                ("P2".to_string(), true)
            ]
        );
    }

    /// A part whose kind the board does not say is named as soon as its
    /// datasheet lands, not inside an agent place; with two of them the shared
    /// naming session is warmed, with one it runs on its own.
    #[test]
    fn naming_a_kind_happens_off_the_agent_places() {
        let _one = one_at_a_time();
        let parts = fake_parts(2);
        let places = Mutex::new(Vec::new());
        let named_solo = Mutex::new(Vec::new());
        let classify = |args: &Args| {
            places.lock().unwrap().push(holds_place());
            named_solo.lock().unwrap().push(admission::solo());
            Ok(format!("kind-of-{}", args.pdf.display()))
        };
        let seen = Mutex::new(Vec::new());
        let extract = |args: Args| {
            seen.lock().unwrap().push(args.kind_str.clone());
            drafted(args)
        };
        let naming_warmed = AtomicUsize::new(0);
        let warm_naming = || {
            naming_warmed.fetch_add(1, Ordering::SeqCst);
        };
        let acquire = ready;
        let mut s = stages(&acquire, &extract);
        s.kind_of = &|_: &UnmodelledPart| String::new();
        s.classify = &classify;
        s.warm_naming = Some(&warm_naming);
        let tally = run_batch(&parts, 2, &s);
        assert_eq!(tally.drafted, 2, "{:?}", tally.failed);
        assert_eq!(*places.lock().unwrap(), [false, false], "not in a place");
        assert_eq!(*named_solo.lock().unwrap(), [false, false]);
        assert_eq!(naming_warmed.load(Ordering::SeqCst), 1);
        let mut seen = seen.lock().unwrap().clone();
        seen.sort();
        assert_eq!(seen, ["kind-of-P0.pdf", "kind-of-P1.pdf"]);

        // One part to name: no warm-up, and the naming call runs alone.
        named_solo.lock().unwrap().clear();
        naming_warmed.store(0, Ordering::SeqCst);
        let one = fake_parts(2);
        let kind_of = |p: &UnmodelledPart| String::from(if p.part == "P0" { "" } else { "opamp" });
        s.kind_of = &kind_of;
        let tally = run_batch(&one, 2, &s);
        assert_eq!(tally.drafted, 2);
        assert_eq!(naming_warmed.load(Ordering::SeqCst), 0);
        assert_eq!(*named_solo.lock().unwrap(), [true]);
    }

    /// A part that cannot be named fails there, without ever taking a place.
    #[test]
    fn a_part_that_cannot_be_named_fails_without_an_agent_place() {
        let _one = one_at_a_time();
        let parts = fake_parts(2);
        let classify = |args: &Args| -> anyhow::Result<String> {
            if args.pdf.ends_with("P0.pdf") {
                anyhow::bail!("not a datasheet for a part we model");
            }
            Ok("opamp".into())
        };
        let calls = Mutex::new(Vec::new());
        let extract = |args: Args| {
            calls.lock().unwrap().push(args.part.clone());
            drafted(args)
        };
        let acquire = ready;
        let mut s = stages(&acquire, &extract);
        s.kind_of = &|_: &UnmodelledPart| String::new();
        s.classify = &classify;
        let tally = run_batch(&parts, 2, &s);
        assert_eq!(tally.drafted, 1);
        assert_eq!(tally.failed_names(), ["P0"]);
        assert!(tally.failed[0].1.contains("not a datasheet"));
        assert_eq!(*calls.lock().unwrap(), ["P1"]);
    }

    /// A job that panics is a gap, not the end of the batch.
    #[test]
    fn a_panicking_extraction_is_reported_and_the_rest_finish() {
        let _one = one_at_a_time();
        let parts = fake_parts(3);
        let extract = |args: Args| {
            assert_ne!(args.part, "P1", "boom");
            drafted(args)
        };
        let acquire = ready;
        let s = stages(&acquire, &extract);
        let tally = run_batch(&parts, 2, &s);
        assert_eq!(tally.drafted, 2);
        assert_eq!(
            tally.failed,
            [("P1".to_string(), "the extraction crashed".to_string())]
        );
    }

    /// A part that panics while its datasheet is fetched is a gap too.
    #[test]
    fn a_panicking_download_is_reported_and_the_rest_finish() {
        let _one = one_at_a_time();
        let parts = fake_parts(3);
        let acquire = |p: &UnmodelledPart, say: &mut dyn FnMut(String)| {
            assert_ne!(p.part, "P0", "boom");
            ready(p, say)
        };
        let s = stages(&acquire, &drafted);
        let tally = run_batch(&parts, 2, &s);
        assert_eq!(tally.drafted, 2);
        assert_eq!(tally.failed_names(), ["P0"]);
    }

    /// Parts whose files would collide run whole, in order, in one place.
    #[test]
    fn a_lane_runs_in_order_in_one_place_and_the_rest_are_pipelined() {
        let _one = one_at_a_time();
        let parts = [
            named("A/B"),
            named("C"),
            named("A_B"),
            named("a?b"),
            named("D"),
        ];
        let order = Mutex::new(Vec::new());
        let busy = AtomicUsize::new(0);
        let extract = |args: Args| {
            if ["A/B", "A_B", "a?b"].contains(&args.part.as_str()) {
                assert_eq!(busy.fetch_add(1, Ordering::SeqCst), 0, "overlap in a lane");
                order.lock().unwrap().push(args.part.clone());
                std::thread::sleep(Duration::from_millis(20));
                busy.fetch_sub(1, Ordering::SeqCst);
            }
            drafted(args)
        };
        let acquire = ready;
        let s = stages(&acquire, &extract);
        let tally = run_batch(&parts, 3, &s);
        assert_eq!(tally.drafted, 5);
        assert_eq!(*order.lock().unwrap(), ["A/B", "A_B", "a?b"]);
        assert_eq!(tally.stage.unwrap().covered, 2, "C and D are pipelined");
    }

    #[test]
    fn parts_that_would_share_a_file_share_a_lane() {
        let parts = [named("A/B"), named("C"), named("A_B"), named("a?b")];
        let refs: Vec<&UnmodelledPart> = parts.iter().collect();
        assert_eq!(lanes(&refs), vec![vec![0, 2, 3], vec![1]]);
    }

    // Start pacing and the wait after a refusal.

    /// A pool and gate on a fake clock; `slept` collects every wait.
    fn gate_on_fake_time(ceiling: usize) -> (Arc<Pool>, BatchGate, Arc<Mutex<Vec<Duration>>>) {
        let slept = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&slept);
        let timing = Timing {
            clock: Arc::new(|| Duration::from_secs(10)),
            sleep: Arc::new(move |d| log.lock().unwrap().push(d)),
        };
        let pool = Arc::new(Pool::new(
            ceiling,
            pacing(0, 0, 0),
            Arc::clone(&timing.clock),
            Arc::new(no_hits),
            Arc::new(silent),
        ));
        let gate = BatchGate::new(Arc::clone(&pool), StartPacer::standard(), timing);
        (pool, gate, slept)
    }

    /// Sessions start one per interval however many want to at once, and a
    /// refused start spreads the following ones further apart.
    #[test]
    fn session_starts_are_paced_and_a_refusal_widens_the_spacing() {
        let (_pool, gate, slept) = gate_on_fake_time(4);
        for _ in 0..4 {
            gate.before_start();
        }
        let secs = |v: &Mutex<Vec<Duration>>| -> Vec<u64> {
            v.lock().unwrap().iter().map(Duration::as_secs).collect()
        };
        assert_eq!(secs(&slept), [2, 4, 6], "the first goes at once");
        slept.lock().unwrap().clear();
        gate.refused();
        gate.refused();
        // The spacing is now 8 s: the next turn is after the four already booked.
        gate.before_start();
        gate.before_start();
        assert_eq!(secs(&slept), [8, 16], "booked 8 s after the previous turn");
    }

    /// While a refused session waits to retry, the place its part holds is free
    /// for another part, and is taken again afterwards.
    #[test]
    fn a_refused_start_gives_up_its_place_while_it_waits() {
        let places = Arc::new(Mutex::new(Vec::new()));
        let pool_cell: Arc<Mutex<Option<Arc<Pool>>>> = Arc::new(Mutex::new(None));
        let (log, cell) = (Arc::clone(&places), Arc::clone(&pool_cell));
        let timing = Timing {
            clock: Arc::new(|| Duration::ZERO),
            sleep: Arc::new(move |_| {
                let pool = cell.lock().unwrap().clone().unwrap();
                log.lock().unwrap().push(pool.active());
            }),
        };
        let pool = Arc::new(Pool::new(
            2,
            pacing(0, 0, 0),
            Arc::clone(&timing.clock),
            Arc::new(no_hits),
            Arc::new(silent),
        ));
        *pool_cell.lock().unwrap() = Some(Arc::clone(&pool));
        let gate = BatchGate::new(Arc::clone(&pool), StartPacer::standard(), timing);
        pool.take_place();
        assert_eq!((pool.active(), holds_place()), (1, true));
        gate.pause(Duration::from_secs(5));
        assert_eq!(*places.lock().unwrap(), [0], "free during the wait");
        assert_eq!(
            (pool.active(), holds_place()),
            (1, true),
            "taken again after"
        );
        pool.give_up_place();
        // A thread with no place (a download, a naming call) just waits.
        gate.pause(Duration::from_secs(5));
        assert_eq!(*places.lock().unwrap(), [0, 0]);
        assert_eq!((pool.active(), holds_place()), (0, false));
    }

    /// In a running batch: with both places held, a part that waits out a
    /// refusal lets the third part in, which is what lets the other finish.
    #[test]
    fn a_waiting_refusal_lets_another_part_use_its_place() {
        let _one = one_at_a_time();
        let parts = fake_parts(3);
        let waited = Arc::new(Mutex::new(false));
        // The sleeper must be 'static, so the latch is shared by Arc.
        let latch = Arc::new(Latch::new());
        let sleeper = Arc::clone(&latch);
        let marker = Arc::clone(&waited);
        let timing = Timing {
            clock: Arc::new(|| Duration::ZERO),
            sleep: Arc::new(move |d| {
                if d == Duration::from_secs(7) {
                    *marker.lock().unwrap() = true;
                    assert!(sleeper.wait(), "P2 never got P0's place while it waited");
                }
            }),
        };
        let extract = |args: Args| {
            match args.part.as_str() {
                "P0" => admission::pause(Duration::from_secs(7)),
                "P1" => assert!(latch.wait(), "P2 never started"),
                _ => latch.set(),
            }
            drafted(args)
        };
        let acquire = ready;
        let s = stages(&acquire, &extract);
        let work: Vec<&UnmodelledPart> = parts.iter().collect();
        let tally = draft_parts_with(&work, &s, &template(), 2, &quick(), &timing);
        assert_eq!(tally.drafted, 3, "{:?}", tally.failed);
        assert!(*waited.lock().unwrap(), "P0 did wait");
    }

    // Per-host fetch limit.

    /// Fetches in flight and the most there have been, per host.
    struct Counting {
        seen: Mutex<HashMap<String, (usize, usize)>>,
    }

    impl hauksbee_models::datasheet_fetch::Http for Counting {
        fn get(&self, req: &Request<'_>) -> Result<Response, TransportError> {
            let host = hauksbee_models::datasheet_fetch::host_of(req.url).unwrap();
            let mut seen = self.seen.lock().unwrap();
            let at = seen.entry(host.clone()).or_default();
            at.0 += 1;
            at.1 = at.1.max(at.0);
            drop(seen);
            std::thread::sleep(Duration::from_millis(15));
            self.seen.lock().unwrap().get_mut(&host).unwrap().0 -= 1;
            Ok(Response {
                status: 200,
                final_url: req.url.to_string(),
            })
        }
    }

    #[test]
    fn no_more_than_the_limit_of_fetches_run_against_one_host() {
        let inner = Counting {
            seen: Mutex::new(HashMap::new()),
        };
        let limited = HostLimited::new(&inner, 2);
        let dest = PathBuf::from("x.pdf");
        std::thread::scope(|scope| {
            for i in 0..8 {
                let (limited, dest) = (&limited, &dest);
                scope.spawn(move || {
                    let url = format!("https://vendor.example/{i}.pdf");
                    let req = Request {
                        url: &url,
                        dest,
                        strategy: Strategy::CurlPlain,
                        http1: false,
                    };
                    limited.get(&req).unwrap();
                });
            }
        });
        let most = inner
            .seen
            .lock()
            .unwrap()
            .get("vendor.example")
            .map(|at| at.1);
        assert_eq!(most, Some(2), "the limit is reached and held");
    }

    // The pool's pacing.

    /// A pool on a fake clock (seconds) with a settable count of refusals.
    struct Fake {
        secs: AtomicUsize,
        hits: AtomicUsize,
        said: Mutex<Vec<String>>,
    }

    impl Fake {
        fn at(&self, secs: usize) {
            self.secs.store(secs, Ordering::SeqCst);
        }

        fn refuse(&self, n: usize) {
            self.hits.fetch_add(n, Ordering::SeqCst);
        }
    }

    /// Run `f` with a pool of `ceiling` on the fake clock: backoff 30 s, ramp
    /// every 10 s, regrow after 60 s quiet.
    fn with_fake_pool(ceiling: usize, f: impl FnOnce(&Pool, &Fake)) {
        let fake = Arc::new(Fake {
            secs: AtomicUsize::new(0),
            hits: AtomicUsize::new(0),
            said: Mutex::new(Vec::new()),
        });
        let (c, h, s) = (Arc::clone(&fake), Arc::clone(&fake), Arc::clone(&fake));
        let pool = Pool::new(
            ceiling,
            pacing(30, 10, 60),
            Arc::new(move || Duration::from_secs(c.secs.load(Ordering::SeqCst) as u64)),
            Arc::new(move || h.hits.load(Ordering::SeqCst)),
            Arc::new(move |t: &str| s.said.lock().unwrap().push(t.to_string())),
        );
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
                said.iter().any(|l| l
                    == "[batch] the backend has stopped rate limiting; running 3 agents instead of 2 again"),
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

    /// Refusals the backend layer counted (retried inside the extraction) shrink
    /// the pool too, though no part failed.
    #[test]
    fn counted_rate_limit_refusals_shrink_the_pool_once_per_interval() {
        with_fake_pool(8, |pool, fake| {
            pool.limit.store(8, Ordering::SeqCst);
            pool.poll();
            assert_eq!(pool.limit(), 8, "no refusals, no change");
            fake.refuse(3);
            pool.poll();
            assert_eq!(pool.limit(), 4);
            fake.refuse(1);
            pool.poll();
            assert_eq!(pool.limit(), 4, "a burst inside the interval halves once");
            let said = fake.said.lock().unwrap().clone();
            assert_eq!(said.len(), 1);
            assert!(said[0].contains("running 4 agents instead of 8"));
            assert_eq!(pool.stats().rate_limits, 4, "every refusal is counted");
        });
    }

    // The batch as a whole, through the real acquirer (PDFs supplied).

    /// A backend that answers 429 halves the pool; the refusals are failed
    /// extractions, not lost, and within the quiet interval the pool stays down.
    #[test]
    fn a_rate_limited_backend_halves_the_pool_and_the_run_carries_on() {
        use hauksbee_models::datasheet_fetch::{CurlHttp, HostLedger};
        let _one = one_at_a_time();
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
        let kind_of = |_: &UnmodelledPart| String::from("opamp");
        let extract = |args: Args| -> anyhow::Result<PathBuf> {
            std::thread::sleep(Duration::from_millis(20));
            if args.part == "P0" {
                anyhow::bail!("HTTP 429 Too Many Requests");
            }
            drafted(args)
        };
        let opts = BatchOptions {
            backoff_every: Duration::from_secs(60),
            progress_every: Duration::from_secs(60),
            ramp_every: Duration::from_secs(60),
            regrow_after: Duration::from_secs(60),
            warm_ahead: false,
            ..BatchOptions::default()
        };
        let tally = draft_parts(&work, &acquirer, &template(), &kind_of, &extract, 4, &opts);
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

    /// The progress line carries the plan's state, and a window that is
    /// draining faster than the prior lowers the pool.
    #[test]
    fn progress_reports_the_plan_and_a_draining_window_lowers_the_pool() {
        use hauksbee_models::datasheet_fetch::{CurlHttp, HostLedger};
        let _one = one_at_a_time();
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
        let kind_of = |_: &UnmodelledPart| String::from("opamp");
        let extract = |args: Args| -> anyhow::Result<PathBuf> {
            std::thread::sleep(Duration::from_millis(150));
            drafted(args)
        };
        // The plan reads 95% used, from 40% when the batch began: once a few
        // parts are done that is several points per part, far above the 0.8
        // prior, so the 5 points left cannot carry the parts still to do.
        let opts = BatchOptions {
            progress_every: Duration::from_millis(400),
            backoff_every: Duration::from_secs(60),
            ramp_every: Duration::ZERO,
            warm_ahead: false,
            watch: Some(PlanWatch {
                start_session: Some(40.0),
                prior_session: 0.8,
            }),
            reading: nearly_spent,
            ..BatchOptions::default()
        };
        let tally = draft_parts(&work, &acquirer, &template(), &kind_of, &extract, 4, &opts);
        assert_eq!(tally.drafted, 12);
        assert!(tally.pool.last < 4, "pool was lowered: {:?}", tally.pool);
    }

    /// Eight parts through the real `draft_parts` with a fake extractor that
    /// takes 250 ms each: four at a time is about a quarter of the serial time,
    /// and the tally keeps the three outcomes apart, in board order.
    #[test]
    fn a_batch_of_eight_with_four_jobs_takes_about_a_quarter_of_the_serial_time() {
        use hauksbee_models::datasheet_discovery::supplied_pdf;
        use hauksbee_models::datasheet_fetch::{CurlHttp, HostLedger};
        let _one = one_at_a_time();
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
            drafted(args)
        };
        let run = |jobs: usize| {
            let opts = BatchOptions {
                jobs: Some(jobs),
                ..quick()
            };
            let started = Instant::now();
            let tally = draft_parts(
                &work,
                &acquirer,
                &template(),
                &kind_of,
                &extract,
                jobs,
                &opts,
            );
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
        let stage = tally.stage.unwrap();
        assert_eq!((stage.covered, stage.found, stage.given_up), (8, 7, 1));
        assert!(
            !progress::concurrent(),
            "the batch leaves the flag as it found it"
        );
        peak.store(0, Ordering::SeqCst);
        let (serial_tally, slow) = run(1);
        assert_eq!(serial_tally.drafted, 6);
        assert!(serial_tally.stage.is_none(), "the serial loop has no stage");
        assert_eq!(peak.load(Ordering::SeqCst), 1, "jobs 1 never overlaps");
        assert!(
            fast * 2 < slow,
            "4 jobs {fast:?} should be well under half of serial {slow:?}"
        );
        assert!(slow >= Duration::from_millis(7 * 250), "{slow:?}");
    }
}
