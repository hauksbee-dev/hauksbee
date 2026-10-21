//! The stages of a concurrent `models extract --unmodelled` batch.
//!
//! The slow part of a part's extraction is not the agent but the waiting around
//! it: searching for the datasheet, downloading it, naming its kind. Measured on
//! real batches, a third of every agent place went to the datasheet fetch, so the
//! batch is a pipeline instead:
//!
//! 1. **Fetch**: every part's datasheet is searched for and downloaded on its own
//!    small pool of downloader threads (`capacity::DOWNLOADERS`, no more than
//!    `capacity::PER_HOST` fetches to one host at a time), all parts at once,
//!    parts with a datasheet named on the board first. When a datasheet lands, a
//!    part whose kind is not known is named right there (two at a time at most),
//!    overlapping the other downloads, and the shared sessions the drafting forks
//!    from are warmed while the downloads still run. A part nothing could be
//!    fetched for ends here: it never takes an agent's place.
//! 2. **Draft**: a part enters the agent pool only when its datasheet is ready,
//!    the biggest datasheet first (the longest job starts earliest, so it is not
//!    left for last).
//!
//! A lane of parts that would write the same file (see `extract_batch::lanes`) is
//! not split: it runs as one job in one place, serially, the old way.
//!
//! Everything outside is injected (`Stages`, and the clock in `Ctx`), so the
//! ordering, the overlap and the pacing are tested without a network, a model or
//! a sleep.

use std::collections::{HashMap, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hauksbee_models::datasheet::Args;
use hauksbee_models::datasheet_discovery::Outcome;
use hauksbee_models::datasheet_fetch::{
    host_of, Http, Request, Response, Strategy, StrategyMemory, TransportError,
};
use hauksbee_models::extract_admission as admission;
use hauksbee_models::extract_capacity as capacity;
use hauksbee_models::extract_progress as progress;

use super::extract_batch::{
    args_for, draft_card, failure, no_datasheet, run_part, Console, Extract, PartRun, Pool, Timing,
    MAX_JOBS,
};
use super::models::UnmodelledPart;

/// How a part's datasheet is got: told each line to show as it happens.
pub type Acquire<'a> = &'a (dyn Fn(&UnmodelledPart, &mut dyn FnMut(String)) -> Outcome + Sync);

/// How a shared drafting session is warmed (given a part and its kind).
pub type WarmKind<'a> = &'a (dyn Fn(&str, &str) + Sync);

/// Everything a batch does that touches the outside world.
pub struct Stages<'a> {
    /// The card kind the board names for a part (empty: it is named from its
    /// datasheet).
    pub kind_of: &'a (dyn Fn(&UnmodelledPart) -> String + Sync),
    /// Get a part's datasheet: search, download and verify it. Told each line to
    /// show as it happens.
    pub acquire: Acquire<'a>,
    /// How big a datasheet is (bigger first).
    pub size_of: &'a (dyn Fn(&Path) -> u64 + Sync),
    /// Name the kind of the part the arguments describe, from its datasheet.
    pub classify: &'a (dyn Fn(&Args) -> anyhow::Result<String> + Sync),
    /// Warm the shared session that names kinds.
    pub warm_naming: Option<&'a (dyn Fn() + Sync)>,
    /// Warm the shared session that drafts a kind (given a part of that kind and
    /// the kind).
    pub warm_kind: Option<WarmKind<'a>>,
    /// Draft one card (the real one is `datasheet::run`).
    pub extract: Extract<'a>,
}

/// What the stage that fetches datasheets did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageStats {
    /// Parts fetched for outside the agents' places.
    pub covered: usize,
    /// Of those, how many had a datasheet found.
    pub found: usize,
    /// Of those, how many were given up for no datasheet: no agent started.
    pub given_up: usize,
    /// From the start to the last download ending.
    pub ran: Duration,
}

/// What a run needs.
pub(super) struct Ctx<'a> {
    pub work: &'a [&'a UnmodelledPart],
    pub stages: &'a Stages<'a>,
    pub template: &'a Args,
    pub pool: &'a Arc<Pool>,
    pub console: Console,
    pub timing: &'a Timing,
    /// How often a quiet stretch says how far the batch has got.
    pub every: Duration,
    /// Called with (done, total, running).
    pub announce: &'a (dyn Fn(usize, usize, usize) + Sync),
}

/// A part whose datasheet is ready to draft from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Item {
    pub(super) idx: usize,
    pub(super) size: u64,
}

/// The ready part to draft next: the biggest datasheet, the earlier part when
/// equal.
pub(super) fn best(items: &[Item]) -> Option<usize> {
    items
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.size.cmp(&b.size).then(b.idx.cmp(&a.idx)))
        .map(|(at, _)| at)
}

/// The order to fetch in: parts the board names a datasheet for first (they need
/// no search), the rest after, each group in board order.
pub(super) fn order_downloads(work: &[&UnmodelledPart], singles: &[usize]) -> Vec<usize> {
    let named = |at: &usize| {
        let p = work[*at];
        p.datasheet.is_some() || !p.more_datasheets.is_empty()
    };
    let mut order: Vec<usize> = singles.iter().copied().filter(named).collect();
    order.extend(singles.iter().copied().filter(|at| !named(at)));
    order
}

/// A counting semaphore.
struct Sem {
    free: Mutex<usize>,
    turn: Condvar,
}

struct Permit<'a>(&'a Sem);

impl Sem {
    const fn new(permits: usize) -> Self {
        Self {
            free: Mutex::new(permits),
            turn: Condvar::new(),
        }
    }

    fn acquire(&self) -> Permit<'_> {
        let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
        while *free == 0 {
            free = self.turn.wait(free).unwrap_or_else(PoisonError::into_inner);
        }
        *free -= 1;
        drop(free);
        Permit(self)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        self.0.turn.notify_one();
    }
}

/// An [`Http`] that lets at most `per_host` requests to one host run at once, so
/// a downloader pool does not hammer a vendor that already rate limits.
pub struct HostLimited<'a> {
    inner: &'a dyn Http,
    per_host: usize,
    hosts: Mutex<HashMap<String, Arc<Sem>>>,
}

impl<'a> HostLimited<'a> {
    #[must_use]
    pub fn new(inner: &'a dyn Http, per_host: usize) -> Self {
        Self {
            inner,
            per_host: per_host.max(1),
            hosts: Mutex::new(HashMap::new()),
        }
    }
}

impl Http for HostLimited<'_> {
    fn get(&self, req: &Request<'_>) -> Result<Response, TransportError> {
        let Some(host) = host_of(req.url) else {
            return self.inner.get(req);
        };
        let sem = {
            let mut hosts = self.hosts.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(
                hosts
                    .entry(host)
                    .or_insert_with(|| Arc::new(Sem::new(self.per_host))),
            )
        };
        let _one_of_few = sem.acquire();
        self.inner.get(req)
    }

    fn pause(&self, how_long: Duration) {
        self.inner.pause(how_long);
    }

    fn available(&self, strategy: Strategy) -> bool {
        self.inner.available(strategy)
    }

    fn memory(&self) -> Option<&StrategyMemory> {
        self.inner.memory()
    }
}

/// How many parts of each kind the batch has, so a kind with one part does not
/// pay for a shared session nobody else will fork from.
#[derive(Default)]
struct Kinds {
    counts: Mutex<HashMap<String, usize>>,
}

impl Kinds {
    /// Count a part of `kind`; the count now.
    fn add(&self, kind: &str) -> usize {
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let n = counts.entry(kind.to_string()).or_insert(0);
        *n += 1;
        let n = *n;
        drop(counts);
        n
    }

    fn count(&self, kind: &str) -> usize {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(kind)
            .copied()
            .unwrap_or(0)
    }
}

/// What the warm-up thread is asked to prepare.
enum Warm {
    Naming,
    Kind { part: String, kind: String },
}

/// A job for an agent place.
enum Work {
    Part(Item),
    Chain(Vec<usize>),
}

struct BoardState {
    ready: Vec<Item>,
    chains: VecDeque<Vec<usize>>,
    /// Parts whose datasheet is still being fetched.
    fetching: usize,
}

/// The queue the agent places draw from: parts with their datasheet ready, and
/// the lanes that run whole.
struct Board {
    state: Mutex<BoardState>,
    changed: Condvar,
}

impl Board {
    fn new(fetching: usize, chains: Vec<Vec<usize>>) -> Self {
        Self {
            state: Mutex::new(BoardState {
                ready: Vec::new(),
                chains: chains.into(),
                fetching,
            }),
            changed: Condvar::new(),
        }
    }

    fn state(&self) -> MutexGuard<'_, BoardState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A part's datasheet is ready.
    fn publish(&self, item: Item) {
        let mut s = self.state();
        s.ready.push(item);
        s.fetching -= 1;
        drop(s);
        self.changed.notify_all();
    }

    /// A part's fetch ended without a datasheet to draft from.
    fn downloaded(&self) {
        self.state().fetching -= 1;
        self.changed.notify_all();
    }

    /// Wait until there is something to draft (true), or there never will be
    /// (false).
    fn wait_for_work(&self) -> bool {
        let mut s = self.state();
        loop {
            if !s.ready.is_empty() || !s.chains.is_empty() {
                return true;
            }
            if s.fetching == 0 {
                return false;
            }
            s = self.changed.wait(s).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// The next job: the best ready part, else a whole lane.
    fn take(&self) -> Option<Work> {
        let mut s = self.state();
        let work = if let Some(at) = best(&s.ready) {
            Some(Work::Part(s.ready.swap_remove(at)))
        } else {
            s.chains.pop_front().map(Work::Chain)
        };
        drop(s);
        self.changed.notify_all();
        work
    }
}

/// A part's datasheet, fetched, and what the drafting needs to go on.
struct Landed {
    item: Item,
    pdf: PathBuf,
    kind: String,
}

/// Parts done, for the progress lines.
struct Done {
    count: Mutex<usize>,
    changed: Condvar,
}

struct Run<'a> {
    ctx: &'a Ctx<'a>,
    board: Board,
    /// The datasheets that landed, by part, until their drafting takes them.
    landed: Mutex<HashMap<usize, Landed>>,
    results: Vec<Mutex<Option<PartRun>>>,
    kinds: Kinds,
    done: Done,
    running: AtomicUsize,
    found: AtomicUsize,
    given_up: AtomicUsize,
    /// How many parts will be named from their datasheet (not known from the board).
    naming: usize,
    namers: Sem,
    /// The latest a download ended, on the batch clock.
    fetched_until: Mutex<Duration>,
}

impl Run<'_> {
    /// A part is finished, in whatever way.
    fn finish(&self, at: usize, run: Option<PartRun>) {
        *self.results[at]
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = run;
        *self
            .done
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner) += 1;
        self.done.changed.notify_all();
    }

    /// One part's fetch: its datasheet, and its kind when the board does not say.
    /// A part with nothing to draft from is finished here and `None` returned.
    fn fetch(&self, at: usize, warm: &Sender<Warm>) -> Option<Landed> {
        let ctx = self.ctx;
        let p = ctx.work[at];
        let got = progress::with_label(&p.part, || {
            (ctx.stages.acquire)(p, &mut |line| ctx.console.say(&p.part, &line))
        });
        let ready = match got {
            Outcome::Ready(ready) => ready,
            Outcome::NoDatasheet(missing) => {
                self.given_up.fetch_add(1, Ordering::SeqCst);
                self.finish(at, Some(no_datasheet(p, missing, ctx.console)));
                return None;
            }
        };
        self.found.fetch_add(1, Ordering::SeqCst);
        let mut kind = (ctx.stages.kind_of)(p);
        if kind.is_empty() {
            // Named off the critical path: here, while other downloads go on and
            // before the part waits for an agent place.
            let args = args_for(p, ctx.template, ready.pdf.clone(), String::new());
            let _two_at_a_time = self.namers.acquire();
            let named = progress::with_label(&p.part, || {
                admission::with_solo(self.naming < 2, || (ctx.stages.classify)(&args))
            });
            match named {
                Ok(named) => kind = named,
                Err(e) => {
                    self.finish(at, Some(failure(p, &e, ctx.console, ctx.pool)));
                    return None;
                }
            }
            if self.kinds.add(&kind) == 2 {
                let _ = warm.send(Warm::Kind {
                    part: p.part.clone(),
                    kind: kind.clone(),
                });
            }
        }
        let size = (ctx.stages.size_of)(&ready.pdf);
        Some(Landed {
            item: Item { idx: at, size },
            pdf: ready.pdf,
            kind,
        })
    }

    /// Take parts to fetch until none are left.
    fn download_loop(&self, queue: &Mutex<VecDeque<usize>>, warm: &Sender<Warm>) {
        loop {
            let next = queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            let Some(at) = next else { break };
            match catch_unwind(AssertUnwindSafe(|| self.fetch(at, warm))) {
                Ok(Some(landed)) => {
                    let item = landed.item.clone();
                    self.landed
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(at, landed);
                    self.board.publish(item);
                }
                Ok(None) => self.board.downloaded(),
                Err(_) => {
                    self.finish(at, None);
                    self.board.downloaded();
                }
            }
        }
        let now = (self.ctx.timing.clock)();
        let mut until = self
            .fetched_until
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *until = (*until).max(now);
    }

    /// Draft one part whose datasheet is ready.
    fn draft(&self, item: &Item) -> PartRun {
        let ctx = self.ctx;
        let p = ctx.work[item.idx];
        let landed = self
            .landed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&item.idx)
            .expect("a published part has landed");
        // A kind with only this part in the batch has nothing to fork from.
        let solo = !landed.kind.is_empty() && self.kinds.count(&landed.kind) < 2;
        let args = args_for(p, ctx.template, landed.pdf, landed.kind);
        progress::with_label(&p.part, || {
            admission::with_solo(solo, || {
                draft_card(p, args, ctx.stages.extract, ctx.console, ctx.pool)
            })
        })
    }

    /// An agent place's loop: wait for work, take a place, take the best job.
    fn model_loop(&self) {
        let pool = self.ctx.pool;
        while self.board.wait_for_work() {
            // The job is chosen after the place is won, so it is the best of
            // what is ready then, not of what was ready when the wait began.
            pool.take_place();
            let Some(work) = self.board.take() else {
                pool.give_up_place();
                continue;
            };
            self.running.fetch_add(1, Ordering::SeqCst);
            match work {
                Work::Part(item) => {
                    let made = catch_unwind(AssertUnwindSafe(|| self.draft(&item))).ok();
                    self.finish(item.idx, made);
                }
                Work::Chain(lane) => {
                    for at in lane {
                        let ctx = self.ctx;
                        let made = catch_unwind(AssertUnwindSafe(|| {
                            run_part(ctx.work[at], ctx.stages, ctx.template, ctx.console, pool)
                        }))
                        .ok();
                        self.finish(at, made);
                    }
                }
            }
            self.running.fetch_sub(1, Ordering::SeqCst);
            pool.give_up_place();
        }
    }

    /// Say how far the batch has got, whenever `every` passes quietly, until all
    /// parts are done.
    #[allow(clippy::significant_drop_tightening)] // the count's guard is the loop's condition
    fn announce_loop(&self, count: usize) {
        let ctx = self.ctx;
        let mut last = Instant::now();
        let mut now = self
            .done
            .count
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while *now < count {
            let left = ctx.every.saturating_sub(last.elapsed());
            now = self
                .done
                .changed
                .wait_timeout(now, left.max(Duration::from_millis(1)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
            if *now < count && last.elapsed() >= ctx.every {
                (ctx.announce)(*now, count, self.running.load(Ordering::SeqCst));
                last = Instant::now();
            }
        }
    }
}

/// Run the batch's parts through the stages: every part's result by index, and
/// what the fetch stage did. `lanes` is every part exactly once; a lane of more
/// than one part runs whole in one agent place.
pub(super) fn run(ctx: &Ctx<'_>, lanes: Vec<Vec<usize>>) -> (Vec<Option<PartRun>>, StageStats) {
    let count = ctx.work.len();
    let (chains, singles): (Vec<_>, Vec<_>) = lanes.into_iter().partition(|l| l.len() > 1);
    let singles: Vec<usize> = singles.into_iter().flatten().collect();
    let covered = singles.len();
    let order = order_downloads(ctx.work, &singles);

    // What is known of the kinds before any datasheet lands, and what is worth
    // warming for.
    let kinds = Kinds::default();
    let (warm_tx, warm_rx) = channel::<Warm>();
    let mut naming = 0;
    for &at in &singles {
        let p = ctx.work[at];
        let kind = (ctx.stages.kind_of)(p);
        if kind.is_empty() {
            naming += 1;
        } else if kinds.add(&kind) == 2 {
            let _ = warm_tx.send(Warm::Kind {
                part: p.part.clone(),
                kind,
            });
        }
    }
    let has_warm = ctx.stages.warm_naming.is_some() || ctx.stages.warm_kind.is_some();
    if naming >= 2 {
        let _ = warm_tx.send(Warm::Naming);
    }

    let run = Run {
        ctx,
        board: Board::new(covered, chains),
        landed: Mutex::new(HashMap::new()),
        results: (0..count).map(|_| Mutex::new(None)).collect(),
        kinds,
        done: Done {
            count: Mutex::new(0),
            changed: Condvar::new(),
        },
        running: AtomicUsize::new(0),
        found: AtomicUsize::new(0),
        given_up: AtomicUsize::new(0),
        naming,
        namers: Sem::new(2),
        fetched_until: Mutex::new(Duration::ZERO),
    };
    let began = (ctx.timing.clock)();
    let queue = Mutex::new(VecDeque::from(order));
    let downloaders = capacity::DOWNLOADERS.min(covered);
    // More threads than places: a thread whose part waits out a refusal has
    // given its place up, and another thread takes it.
    let agents = MAX_JOBS.min(count.max(1));
    let (run_ref, queue_ref) = (&run, &queue);
    std::thread::scope(|scope| {
        if has_warm {
            scope.spawn(move || {
                for request in warm_rx {
                    let _ = catch_unwind(AssertUnwindSafe(|| match request {
                        Warm::Naming => {
                            if let Some(warm) = ctx.stages.warm_naming {
                                warm();
                            }
                        }
                        Warm::Kind { part, kind } => {
                            if let Some(warm) = ctx.stages.warm_kind {
                                warm(&part, &kind);
                            }
                        }
                    }));
                }
            });
        } else {
            drop(warm_rx);
        }
        for _ in 0..downloaders {
            let tx = warm_tx.clone();
            scope.spawn(move || run_ref.download_loop(queue_ref, &tx));
        }
        // The warm-up thread ends when the downloaders drop their senders.
        drop(warm_tx);
        for _ in 0..agents {
            scope.spawn(move || run_ref.model_loop());
        }
        run_ref.announce_loop(count);
    });
    let until = *run
        .fetched_until
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let stats = StageStats {
        covered,
        found: run.found.load(Ordering::SeqCst),
        given_up: run.given_up.load(Ordering::SeqCst),
        ran: until.saturating_sub(began),
    };
    let results = run
        .results
        .into_iter()
        .map(|r| r.into_inner().unwrap_or_else(PoisonError::into_inner))
        .collect();
    (results, stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(name: &str) -> UnmodelledPart {
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

    #[test]
    fn the_biggest_ready_datasheet_is_drafted_first() {
        let items = [
            Item { idx: 0, size: 10 },
            Item { idx: 1, size: 300 },
            Item { idx: 2, size: 300 },
            Item { idx: 3, size: 7 },
        ];
        assert_eq!(best(&items), Some(1), "biggest; the earlier of equals");
        assert_eq!(best(&[]), None);
    }

    /// The board hands out ready parts biggest first, whatever order they were
    /// published in, and a whole lane only when nothing is ready.
    #[test]
    fn the_board_hands_out_biggest_first_and_lanes_last() {
        let board = Board::new(4, vec![vec![8, 9]]);
        for (idx, size) in [(0, 10), (1, 400), (2, 30), (3, 5000)] {
            board.publish(Item { idx, size });
        }
        let mut order = Vec::new();
        while let Some(work) = board.take() {
            order.push(match work {
                Work::Part(item) => item.idx.to_string(),
                Work::Chain(lane) => format!("lane{lane:?}"),
            });
        }
        assert_eq!(order, ["3", "1", "2", "0", "lane[8, 9]"]);
        assert!(!board.wait_for_work(), "nothing left, nothing coming");
    }

    /// A worker waiting for work is told when there will be none (every fetch
    /// ended without a part to draft) and when a part is ready.
    #[test]
    fn waiting_for_work_ends_when_the_fetches_do() {
        let board = Board::new(2, vec![]);
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| board.wait_for_work());
            board.publish(Item { idx: 0, size: 1 });
            assert!(waiter.join().unwrap(), "a published part is work");
        });
        assert!(board.take().is_some());
        std::thread::scope(|scope| {
            let waiter = scope.spawn(|| board.wait_for_work());
            board.downloaded();
            assert!(!waiter.join().unwrap(), "the last fetch ended with nothing");
        });
    }

    /// Parts the board names a datasheet for are fetched first (no search to
    /// pay for), the rest after, each in board order.
    #[test]
    fn downloads_start_with_the_parts_the_board_names_a_datasheet_for() {
        let mut parts: Vec<UnmodelledPart> = (0..5).map(|i| part(&format!("P{i}"))).collect();
        parts[1].datasheet = Some("https://example.com/a.pdf".into());
        parts[3].more_datasheets = vec!["https://example.com/b.pdf".into()];
        let work: Vec<&UnmodelledPart> = parts.iter().collect();
        assert_eq!(order_downloads(&work, &[0, 1, 2, 3, 4]), [1, 3, 0, 2, 4]);
        assert_eq!(
            order_downloads(&work, &[4, 2]),
            [4, 2],
            "none named: as given"
        );
    }

    #[test]
    fn kinds_count_the_parts_of_each_kind() {
        let kinds = Kinds::default();
        assert_eq!(kinds.count("opamp"), 0);
        assert_eq!(kinds.add("opamp"), 1);
        assert_eq!(kinds.add("opamp"), 2);
        assert_eq!(kinds.add("vreg"), 1);
        assert_eq!((kinds.count("opamp"), kinds.count("vreg")), (2, 1));
    }
}
