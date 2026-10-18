//! Determinate progress for loading and checking a board.
//!
//! A large board takes tens of seconds to read, bind and check, and a spinner
//! that does not move is indistinguishable from a hang. This module is the ONE
//! place that knows the stages of that work, what to call them in user words,
//! and what each costs, so the browser's loading bar and the terminal's both
//! read the same stage names and the same percentage.
//!
//! The pipeline reports to a reporter installed for the current thread
//! ([`with_reporter`]); with none installed every call here is a single atomic
//! load, so an unwatched run pays nothing. The reporter is thread-scoped rather
//! than passed down every signature: the analysis already runs start to finish
//! on one thread, and the stage boundaries are many call frames from where a
//! caller could hand a parameter in.
//!
//! # How the percentage is made
//!
//! Progress is real work, never a timer. Each [`Stage`] has a cost, in
//! milliseconds, that scales with a quantity the board itself states: bytes read,
//! parts, nets, copper primitives, simulated seconds ([`STAGES`], measured; see
//! the comment on that table). The overall fraction is the cost of the stages
//! finished, plus the current stage's share of its own cost, over the total.
//! Inside a stage the share is the units done over the units known
//! ([`begin`] / [`at`]). When a stage learns a size the plan guessed (the real
//! part count replaces the one estimated from the file size), every later stage's
//! cost is recomputed, so the bar stays honest as the board becomes known.
//!
//! The reported fraction never decreases, and stays under 100% until the caller
//! says the work is done ([`finish`]).

// Sizes are counts turned into f64 to weigh work against work, and the percent
// shown is a rounded fraction: none of it needs more than a few digits, and no
// count here comes near 2^52.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the numbers are estimates of work, shown to whole percents"
)]

use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A step of loading a board, in the order the web analysis meets them.
///
/// The command line's `--check` meets bind, nets and copper in a different order;
/// progress does not depend on the order, only on which stages have finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Stage {
    Read,
    Models,
    Bind,
    Copper,
    Nets,
    Signals,
    Report,
    Simulate,
}

/// What a stage's cost scales with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// A fixed cost, independent of the board.
    Fixed,
    /// Bytes of the board file.
    Bytes,
    /// Components on the board.
    Parts,
    /// Nets on the board.
    Nets,
    /// Copper primitives (tracks, vias, pads, zones) the spacing check walks.
    Copper,
    /// Seconds of board time the simulation will cover.
    SimSeconds,
}

/// One row of the stage table.
#[derive(Clone, Copy, Debug)]
pub struct StageSpec {
    pub stage: Stage,
    /// Stable machine name, sent to the browser.
    pub key: &'static str,
    /// What the person watching reads.
    pub label: &'static str,
    pub unit: Unit,
    /// Cost independent of size, in milliseconds.
    pub fixed_ms: f64,
    /// Cost per unit, in milliseconds.
    pub per_unit_ms: f64,
    /// Cost per unit squared, in milliseconds: the part of a stage that grows
    /// faster than the board (a scan of every part against every net) and is
    /// negligible on a small one.
    pub per_unit_sq_ms: f64,
    /// Further cost per copper primitive, on top of the unit's, for a stage
    /// whose work has a part that follows the copper (the lint's layout
    /// checks, the report's per-primitive findings).
    pub per_copper_ms: f64,
}

impl StageSpec {
    /// What the stage costs on a board of these sizes, in milliseconds.
    fn cost(&self, sizes: &Sizes) -> f64 {
        let units = sizes.of(self.unit);
        self.per_copper_ms.mul_add(
            sizes.copper,
            self.per_unit_sq_ms.mul_add(
                units * units,
                self.per_unit_ms.mul_add(units, self.fixed_ms),
            ),
        )
    }
}

/// The stages, their user-facing words and their measured costs.
///
/// The costs are relative weights, in milliseconds of an optimised (`--release`)
/// build on Apple silicon. The units are the quantities each stage loops over
/// (bytes, parts, copper primitives), so a stage that grows with copper shows up
/// as a per-unit cost on `Copper` rather than as a flat share. A stage whose
/// work has a part that follows the copper as well (the lint's layout checks,
/// the report) carries that part as `per_copper_ms`.
///
/// How they were measured (after the per-part and per-net scans were indexed,
/// which made binding, the lint and the report several times cheaper on large
/// boards): the load was run with `HAUKSBEE_PROGRESS_LOG` on (every stage
/// change and tick, with its time), through the web analysis and through the
/// command line's `check` and `run`, over `testdata/boards` (`pic_programmer`,
/// `stickhub`: 63 and 94 parts), `stickhub` tiled 4, 12, 30 and 200 times into
/// synthetic `.kicad_pcb` files (up to 208 MB, 18,800 parts, 1.9 million copper
/// primitives as the spacing check counts them: tracks, vias, pads and zones)
/// and a private 6,884-part netlist (no layout). A stage's duration is the time
/// from its first report to the next stage's; dividing by its unit count gave a
/// figure per board. From 4 to 30 tiles the boards agreed to within about 30%
/// (read 5 to 7 microseconds per KB, bind 0.03 to 0.05 ms per part, copper
/// 0.0035 to 0.004 ms per primitive, signals 0.0014 per primitive, report 0.03
/// per part plus 0.001 per primitive). Binding and the lint are not linear:
/// per part they cost 3 to 4 times as much on the 18,800-part board as on the
/// 1,100-part one (and the netlist with 6,884 parts sits between), so those
/// two rows carry a squared term, fitted to the five large boards. The nets
/// stage is the lint (a start-up of about 60 ms, then the squared and linear
/// terms) followed by the layout checks, about 0.0014 ms per primitive, which
/// on a dense layout take about as long as the lint itself. Simulating is the
/// exception: a second of board time costs 80 to 6,000 ms depending on the
/// circuit, so the row is only the opening guess and the stage reads its real
/// speed once it is under way. Re-measure after a change that moves a stage's
/// cost by several times; the bar stays monotonic either way, it just drifts
/// from the clock.
pub const STAGES: [StageSpec; 8] = [
    StageSpec {
        stage: Stage::Read,
        key: "read",
        label: "Reading the board file",
        unit: Unit::Bytes,
        fixed_ms: 1.0,
        per_unit_ms: 0.000_005,
        per_unit_sq_ms: 0.0,
        per_copper_ms: 0.0,
    },
    StageSpec {
        stage: Stage::Models,
        key: "models",
        label: "Loading part models",
        unit: Unit::Fixed,
        fixed_ms: 65.0,
        per_unit_ms: 0.0,
        per_unit_sq_ms: 0.0,
        per_copper_ms: 0.0,
    },
    StageSpec {
        stage: Stage::Bind,
        key: "bind",
        label: "Matching parts to models",
        unit: Unit::Parts,
        fixed_ms: 1.0,
        per_unit_ms: 0.046,
        per_unit_sq_ms: 0.000_000_54,
        per_copper_ms: 0.0,
    },
    StageSpec {
        stage: Stage::Copper,
        key: "copper",
        label: "Checking copper and spacing",
        unit: Unit::Copper,
        fixed_ms: 1.0,
        per_unit_ms: 0.004_2,
        per_unit_sq_ms: 0.000_000_000_059,
        per_copper_ms: 0.0,
    },
    StageSpec {
        stage: Stage::Nets,
        key: "nets",
        label: "Checking nets and connections",
        unit: Unit::Parts,
        fixed_ms: 60.0,
        per_unit_ms: 0.133,
        per_unit_sq_ms: 0.000_021_5,
        per_copper_ms: 0.001_4,
    },
    StageSpec {
        stage: Stage::Signals,
        key: "signals",
        label: "Checking signal integrity",
        unit: Unit::Copper,
        fixed_ms: 1.0,
        per_unit_ms: 0.000_9,
        per_unit_sq_ms: 0.0,
        per_copper_ms: 0.0,
    },
    StageSpec {
        stage: Stage::Report,
        key: "report",
        label: "Writing up the findings",
        unit: Unit::Parts,
        fixed_ms: 3.0,
        per_unit_ms: 0.028,
        per_unit_sq_ms: 0.0,
        per_copper_ms: 0.001_5,
    },
    StageSpec {
        stage: Stage::Simulate,
        key: "simulate",
        label: "Simulating",
        unit: Unit::SimSeconds,
        fixed_ms: 1.0,
        per_unit_ms: 1000.0,
        per_unit_sq_ms: 0.0,
        per_copper_ms: 0.0,
    },
];

/// Units a bind counts per part.
///
/// Binding derives facts about the whole board (which nets are grounds, which
/// are rails) before it visits a single part, and that derivation is most of its
/// time on a large board. A bind therefore counts `BIND_STEPS` units per part:
/// `BIND_STEPS - 1` for the derivation ([`bind_derived`]) and one for the visit
/// to the part (a tick per part). Measured on a 6,884-part netlist, where the
/// derivation took about three times as long as the visits.
pub const BIND_STEPS: u64 = 4;

/// Enter the copper stage before its sweep has counted anything.
///
/// The layout is
/// being read into primitives, which [`begin_sweep`] credits once it is done, so
/// the bar waits in the middle of that stretch, not at the start of it.
pub fn begin_copper() {
    begin(Stage::Copper, 100);
    if watched() {
        with_tracker(|t| {
            t.stage_done = COPPER_PREP_PERCENT / 2;
            t.emit(true);
        });
    }
}

/// Enter a stage that does its work in one pass with nothing inside to tick.
///
/// Signal integrity and the report are such stages:
/// (signal integrity, the report): half its share is credited at once, so the
/// bar waits in the middle of the stretch the stage takes, not at one end of it.
pub fn begin_untracked(stage: Stage) {
    begin(stage, 2);
    if watched() {
        with_tracker(|t| {
            t.stage_done = 1;
            t.emit(true);
        });
    }
}

/// Enter the bind stage for `passes` binds of a board of `parts` parts.
pub fn begin_bind(parts: usize, passes: u64) {
    begin(Stage::Bind, parts as u64 * BIND_STEPS * passes);
}

/// The board-wide derivation of one bind is done; the visits to parts follow.
pub fn bind_derived(parts: usize) {
    advance_in(Stage::Bind, parts as u64 * (BIND_STEPS - 1));
}

/// Share of the copper stage, in percent, spent reading the layout into
/// primitives before the sweep that the stage counts (measured on the tiled
/// stickhub boards; the sweep is the other 65%).
const COPPER_PREP_PERCENT: u64 = 35;

/// The copper sweep is about to visit `primitives` pieces of copper: size the
/// stage by them, and count the reading that came before as already done.
pub fn begin_sweep(primitives: usize) {
    learned_copper(primitives);
    let total = primitives as u64 * 100 / (100 - COPPER_PREP_PERCENT);
    begin(Stage::Copper, total);
    advance_in(Stage::Copper, total - primitives as u64);
}

/// What the nets stage counts for the lint proper.
///
/// The lint reports its progress in
/// percent of its own time, so a pass worth a seventh of it ticks 14
/// ([`lint_advance`]). See `lint_tick` in the checks crate for where the
/// percentages come from.
const LINT_UNITS: u64 = 100;

/// Enter the nets stage.
///
/// It counts the lint's [`LINT_UNITS`], then the layout checks that
/// follow it ([`layout_begins`]), sized by the table's own costs for each so a
/// board with copper gets the layout's share and a netlist gets none.
pub fn begin_lint() {
    if !watched() {
        return;
    }
    let mut layout = 0;
    with_tracker(|t| layout = t.layout_units());
    begin(Stage::Nets, LINT_UNITS + layout);
    with_tracker(|t| {
        t.lint_done = 0;
        t.layout_total = layout;
        t.layout_credited = 0;
    });
}

/// The lint has run passes worth `percent` of its own time.
///
/// Counted up to the
/// lint's [`LINT_UNITS`] and no further, so the extra runs of the corner sweep
/// do not eat the share that belongs to the layout checks.
pub fn lint_advance(percent: u64) {
    if !watched() {
        return;
    }
    let mut credit = 0;
    with_tracker(|t| {
        if t.current == Some(Stage::Nets) {
            credit = percent.min(LINT_UNITS.saturating_sub(t.lint_done));
            t.lint_done += credit;
        }
    });
    if credit > 0 {
        advance_in(Stage::Nets, credit);
    }
}

/// The layout checks are starting.
///
/// Half of their share is credited now, so the
/// bar waits in the middle of the stretch they take, not at one end of it. They
/// are a single pass over the layout, with nothing inside to tick.
pub fn layout_begins() {
    credit_layout(true);
}

/// The layout checks are done (or had nothing to do): the rest of their share.
pub fn layout_ends() {
    credit_layout(false);
}

fn credit_layout(half: bool) {
    if !watched() {
        return;
    }
    with_tracker(|t| {
        if t.current != Some(Stage::Nets) {
            return;
        }
        let target = if half {
            t.layout_total / 2
        } else {
            t.layout_total
        };
        let credit = target.saturating_sub(t.layout_credited);
        t.layout_credited += credit;
        t.stage_done += credit;
        // A pass with nothing inside it to tick is a long wait with no report, so
        // what came before it is shown now, past the gap that holds ticks back:
        // the lint's last ticks are otherwise lost behind the one before them.
        if half {
            t.emit(true);
        } else if credit > 0 {
            t.emit(false);
        }
    });
}

/// Size ratios used before the board has been read: parts, nets and copper
/// primitives per byte of file, measured on the same boards as [`STAGES`].
const BYTES_PER_PART: f64 = 10_000.0;
const NETS_PER_PART: f64 = 1.3;
const BYTES_PER_COPPER: f64 = 100.0;

/// A progress report: where the load is, in words and as a fraction.
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    pub stage: Stage,
    pub key: &'static str,
    /// The words for the current stage.
    pub label: &'static str,
    /// Overall progress, `0.0..=1.0`. Never decreases within one load.
    pub fraction: f64,
    pub elapsed: Duration,
}

impl Update {
    /// The update as a single-line JSON object: what the browser's stream carries.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "stage": self.key,
            "label": self.label,
            "fraction": (self.fraction * 10_000.0).round() / 10_000.0,
            "elapsed_ms": u64::try_from(self.elapsed.as_millis()).unwrap_or(u64::MAX),
        })
        .to_string()
    }
}

/// What the caller knows about the work before it starts.
#[derive(Clone, Copy, Debug, Default)]
pub struct Plan {
    /// Size of the board file in bytes (0 when unknown).
    pub bytes: u64,
    /// Seconds of board time the run will simulate, when it simulates at all.
    pub simulate_s: Option<f64>,
}

type Sink = Box<dyn FnMut(&Update)>;

/// Smallest gap between two updates from the same stage. A stage change always
/// reports; ticks inside a stage do not need to arrive faster than a bar can
/// usefully move.
const MIN_GAP: Duration = Duration::from_millis(60);

struct Tracker {
    sink: Sink,
    started: Instant,
    last_emit: Option<Instant>,
    sizes: Sizes,
    simulates: bool,
    finished: u16,
    current: Option<Stage>,
    /// Units known for the current stage and done so far.
    stage_total: u64,
    stage_done: u64,
    /// Next `done` count at which `at` bothers to do any work.
    next_check: u64,
    /// When the simulating stage began, to read its real speed once it is under way.
    sim_began: Option<Instant>,
    shown: f64,
    /// Lint units counted so far in the nets stage (capped at the lint's own).
    lint_done: u64,
    /// Units of the nets stage given to the layout checks, and how many of
    /// them have been counted.
    layout_total: u64,
    layout_credited: u64,
}

#[derive(Clone, Copy)]
struct Sizes {
    bytes: f64,
    parts: f64,
    nets: f64,
    copper: f64,
    sim_s: f64,
}

impl Sizes {
    fn estimate(plan: Plan) -> Self {
        let bytes = plan.bytes as f64;
        let parts = (bytes / BYTES_PER_PART).max(1.0);
        Self {
            bytes,
            parts,
            nets: parts * NETS_PER_PART,
            copper: (bytes / BYTES_PER_COPPER).max(1.0),
            sim_s: plan.simulate_s.unwrap_or(0.0),
        }
    }

    const fn of(&self, unit: Unit) -> f64 {
        match unit {
            Unit::Fixed => 0.0,
            Unit::Bytes => self.bytes,
            Unit::Parts => self.parts,
            Unit::Nets => self.nets,
            Unit::Copper => self.copper,
            Unit::SimSeconds => self.sim_s,
        }
    }
}

const fn bit(stage: Stage) -> u16 {
    1 << stage as u16
}

impl Tracker {
    /// Units of the nets stage the layout checks are worth, against the
    /// lint's [`LINT_UNITS`]: the ratio of their two costs in the table.
    fn layout_units(&self) -> u64 {
        let spec = &STAGES[Stage::Nets as usize];
        let layout = spec.per_copper_ms * self.sizes.copper;
        let lint = spec.cost(&self.sizes) - layout;
        (LINT_UNITS as f64 * layout / lint.max(1.0)).round() as u64
    }

    fn cost(&self, spec: &StageSpec) -> f64 {
        if spec.stage == Stage::Simulate && !self.simulates {
            return 0.0;
        }
        if spec.stage == Stage::Simulate {
            if let Some(measured) = self.measured_sim_ms() {
                return measured;
            }
        }
        spec.cost(&self.sizes)
    }

    /// What simulating costs, read from how fast it is actually going. A second
    /// of board time takes anywhere from tens of milliseconds to several seconds
    /// depending on the circuit, which nothing known beforehand predicts; the
    /// table's figure is only the guess until the simulation has covered enough
    /// of its time (2%) to tell.
    fn measured_sim_ms(&self) -> Option<f64> {
        if self.current != Some(Stage::Simulate) || self.stage_total == 0 {
            return None;
        }
        let share = self.stage_done as f64 / self.stage_total as f64;
        if share < 0.02 {
            return None;
        }
        let began = self.sim_began?;
        Some(began.elapsed().as_secs_f64() * 1000.0 / share.min(1.0))
    }

    fn fraction(&self) -> f64 {
        let (mut done, mut total) = (0.0, 0.0);
        for spec in &STAGES {
            let cost = self.cost(spec);
            total += cost;
            if self.finished & bit(spec.stage) != 0 {
                done += cost;
            } else if self.current == Some(spec.stage) && self.stage_total > 0 {
                let share = (self.stage_done as f64 / self.stage_total as f64).min(1.0);
                done += cost * share;
            }
        }
        if total <= 0.0 {
            0.0
        } else {
            // Not 100% until `finish`: the work after the last tick is still work.
            (done / total).min(0.99)
        }
    }

    fn emit(&mut self, force: bool) {
        let now = Instant::now();
        if !force
            && self
                .last_emit
                .is_some_and(|t| now.duration_since(t) < MIN_GAP)
        {
            return;
        }
        let fraction = self.fraction().max(self.shown);
        self.shown = fraction;
        self.last_emit = Some(now);
        let stage = self.current.unwrap_or(Stage::Read);
        let spec = &STAGES[stage as usize];
        let update = Update {
            stage,
            key: spec.key,
            label: spec.label,
            fraction,
            elapsed: now.duration_since(self.started),
        };
        (self.sink)(&update);
    }

    fn begin(&mut self, stage: Stage, units: u64) {
        if let Some(prev) = self.current {
            if prev != stage {
                self.finished |= bit(prev);
            }
        }
        self.current = Some(stage);
        if stage == Stage::Simulate && self.sim_began.is_none() {
            self.sim_began = Some(Instant::now());
        }
        self.stage_total = units;
        self.stage_done = 0;
        self.next_check = (units / 200).max(1);
        self.emit(true);
    }

    fn advance_to(&mut self, done: u64) {
        if done < self.stage_done {
            return;
        }
        self.stage_done = done;
        self.next_check = done + (self.stage_total / 200).max(1);
        self.emit(false);
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<Tracker>> = const { RefCell::new(None) };
}

/// Installed reporters across all threads: lets an idle process skip the
/// thread-local lookup altogether.
static WATCHERS: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn watched() -> bool {
    WATCHERS.load(Ordering::Relaxed) != 0
}

fn with_tracker(f: impl FnOnce(&mut Tracker)) {
    ACTIVE.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut() {
            if let Some(tracker) = slot.as_mut() {
                f(tracker);
            }
        }
    });
}

struct Scope {
    owned: bool,
}

impl Drop for Scope {
    fn drop(&mut self) {
        if self.owned {
            ACTIVE.with(|cell| cell.borrow_mut().take());
            WATCHERS.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Run `work` with `sink` receiving its progress. The sink hears of every stage
/// change and, between them, of ticks no more often than a bar can use. After
/// `work` returns the sink receives a final `1.0`.
///
/// A scope opened while another is already open on this thread does nothing:
/// the outermost reporter keeps the run.
pub fn with_reporter<R>(
    plan: Plan,
    sink: impl FnMut(&Update) + 'static,
    work: impl FnOnce() -> R,
) -> R {
    let owned = ACTIVE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_some() {
            return false;
        }
        *slot = Some(Tracker {
            sink: Box::new(sink),
            started: Instant::now(),
            last_emit: None,
            sizes: Sizes::estimate(plan),
            simulates: plan.simulate_s.is_some_and(|s| s > 0.0),
            finished: 0,
            current: None,
            stage_total: 0,
            stage_done: 0,
            next_check: 0,
            sim_began: None,
            shown: 0.0,
            lint_done: 0,
            layout_total: 0,
            layout_credited: 0,
        });
        true
    });
    if !owned {
        return work();
    }
    WATCHERS.fetch_add(1, Ordering::Relaxed);
    let _scope = Scope { owned };
    begin(Stage::Read, 0);
    let out = work();
    finish();
    out
}

/// Enter `stage`, which will count `units` units of work (0 when it cannot
/// say). Whatever stage was running is complete.
pub fn begin(stage: Stage, units: u64) {
    if watched() {
        with_tracker(|t| t.begin(stage, units));
    }
}

/// `done` of the current stage's units are finished, counted from the stage's start.
///
/// A count lower than one already reported is ignored. Cheap to call in
/// a loop: it returns on a comparison until enough work has passed to matter.
#[inline]
pub fn at(done: u64) {
    if watched() {
        with_tracker(|t| {
            if done >= t.next_check {
                t.advance_to(done);
            }
        });
    }
}

/// `units` more units of the current stage are finished. For work done in
/// passes (binding runs more than once), where each pass counts its own units.
#[inline]
pub fn advance_in(stage: Stage, units: u64) {
    if watched() {
        with_tracker(|t| {
            if t.current == Some(stage) {
                let done = t.stage_done + units;
                if done >= t.next_check {
                    t.advance_to(done);
                } else {
                    t.stage_done = done;
                }
            }
        });
    }
}

/// The board turned out to have these sizes, replacing the guesses made from
/// the file size. Every later stage's share is recomputed from them.
pub fn learned(parts: usize, nets: usize) {
    if watched() {
        with_tracker(|t| {
            t.sizes.parts = (parts as f64).max(1.0);
            t.sizes.nets = (nets as f64).max(1.0);
        });
    }
}

/// The board's layout text, if it has one, tells how much copper there is.
///
/// Copper is walked
/// walk: only a KiCad layout is read primitive by primitive, and a netlist (whose
/// text the readers keep too) has none, so it must not be priced from its size.
pub fn learned_layout(layout_text: Option<&str>) {
    if !layout_text.is_some_and(|t| t.contains("(kicad_pcb")) {
        learned_copper(0);
    }
}

/// The copper check will walk this many primitives.
pub fn learned_copper(primitives: usize) {
    if watched() {
        with_tracker(|t| t.sizes.copper = (primitives as f64).max(1.0));
    }
}

/// The run will simulate `seconds` of board time, which it did not know at the
/// start (a firmware upload, say).
pub fn will_simulate(seconds: f64) {
    if watched() {
        with_tracker(|t| {
            t.sizes.sim_s = seconds;
            t.simulates = seconds > 0.0;
        });
    }
}

/// The load is complete: report 100%.
pub fn finish() {
    if watched() {
        with_tracker(|t| {
            if let Some(stage) = t.current {
                t.finished |= bit(stage);
            }
            t.shown = 1.0;
            let spec = &STAGES[t.current.unwrap_or(Stage::Read) as usize];
            let update = Update {
                stage: spec.stage,
                key: spec.key,
                label: spec.label,
                fraction: 1.0,
                elapsed: t.started.elapsed(),
            };
            (t.sink)(&update);
        });
    }
}

// ── Sinks ───────────────────────────────────────────────────────────────────

/// Whether a bar is on screen at the start of the current stderr line.
static BAR_SHOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Wipe the bar, if one is showing, so the text about to go to stderr starts on
/// a clean line. The terminal-safe `eprint!` family calls this first.
pub fn clear_stderr_line() {
    if BAR_SHOWN.swap(false, Ordering::Relaxed) {
        use std::io::Write as _;
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(b"\r\x1b[2K");
        let _ = err.flush();
    }
}

/// Width of the bar's track, in cells.
const BAR_CELLS: usize = 20;

/// The bar line for an update: `[########------------]  40%  Matching parts to models`.
#[must_use]
pub fn render_bar(update: &Update) -> String {
    let percent = (update.fraction * 100.0).floor().clamp(0.0, 100.0) as usize;
    let filled = (percent * BAR_CELLS / 100).min(BAR_CELLS);
    format!(
        "[{}{}] {percent:>3}%  {}",
        "#".repeat(filled),
        "-".repeat(BAR_CELLS - filled),
        update.label
    )
}

/// A sink that draws the bar on stderr with a carriage return, once the load has
/// run for `delay`.
///
/// A load that finishes sooner shows nothing at all. The caller decides whether
/// stderr is a terminal and whether a bar is wanted.
pub fn stderr_bar(delay: Duration) -> impl FnMut(&Update) + 'static {
    move |update| {
        use std::io::Write as _;
        if update.fraction >= 1.0 {
            clear_stderr_line();
            return;
        }
        if update.elapsed < delay {
            return;
        }
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[2K{}", render_bar(update));
        let _ = err.flush();
        BAR_SHOWN.store(true, Ordering::Relaxed);
    }
}

/// A sink that appends `elapsed_ms<TAB>fraction<TAB>stage` to `path`, for
/// measuring how well the fraction tracks the clock (`HAUKSBEE_PROGRESS_LOG`).
#[must_use]
pub fn log_to(path: &std::path::Path) -> Option<impl FnMut(&Update) + 'static> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()?;
    Some(move |update: &Update| {
        use std::io::Write as _;
        let _ = writeln!(
            file,
            "{}\t{:.4}\t{}",
            update.elapsed.as_millis(),
            update.fraction,
            update.key
        );
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn collect(plan: Plan, work: impl FnOnce()) -> Vec<Update> {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&seen);
        with_reporter(plan, move |u| sink.borrow_mut().push(u.clone()), work);
        let out = seen.borrow().clone();
        out
    }

    #[test]
    fn the_table_is_in_stage_order_with_one_key_each() {
        for (index, spec) in STAGES.iter().enumerate() {
            assert_eq!(spec.stage as usize, index, "{} is out of order", spec.key);
        }
        let mut keys: Vec<_> = STAGES.iter().map(|s| s.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), STAGES.len());
    }

    #[test]
    fn progress_never_goes_backwards_and_ends_at_one() {
        let updates = collect(
            Plan {
                bytes: 100_000,
                simulate_s: None,
            },
            || {
                begin(Stage::Read, 100_000);
                for done in (0..100_000).step_by(10) {
                    at(done);
                }
                // The real board is much bigger than the file size suggested.
                learned(5_000, 9_000);
                begin(Stage::Bind, 5_000);
                for done in 0..5_000 {
                    at(done);
                }
                begin(Stage::Copper, 0);
                begin(Stage::Report, 0);
            },
        );
        assert!(updates.len() > 4, "{updates:?}");
        assert!(updates.windows(2).all(|w| w[0].fraction <= w[1].fraction));
        assert!((updates.last().unwrap().fraction - 1.0).abs() < f64::EPSILON);
        assert!(updates[..updates.len() - 1]
            .iter()
            .all(|u| u.fraction < 1.0));
    }

    #[test]
    fn a_finished_stage_counts_whatever_order_the_stages_ran_in() {
        let updates = collect(
            Plan {
                bytes: 10_000,
                simulate_s: None,
            },
            || {
                begin(Stage::Bind, 10);
                begin(Stage::Nets, 10);
                begin(Stage::Copper, 10);
            },
        );
        let labels: Vec<_> = updates.iter().map(|u| u.key).collect();
        assert!(labels.contains(&"nets") && labels.contains(&"copper"));
    }

    #[test]
    fn simulated_time_is_the_unit_of_the_simulating_stage() {
        let updates = collect(
            Plan {
                bytes: 2_000,
                simulate_s: Some(4.0),
            },
            || {
                begin(Stage::Bind, 1);
                begin(Stage::Simulate, 4_000);
                std::thread::sleep(MIN_GAP);
                at(1_000);
                std::thread::sleep(MIN_GAP);
                at(3_000);
            },
        );
        let sim: Vec<_> = updates.iter().filter(|u| u.key == "simulate").collect();
        assert!(sim.len() >= 3, "{updates:?}");
        assert!(
            sim[1].fraction > 0.1,
            "a quarter of a dominant sim is visible: {sim:?}"
        );
        assert!(sim[2].fraction > sim[1].fraction);
    }

    #[test]
    fn a_simulation_slower_than_the_table_says_is_read_from_its_own_speed() {
        // Four milliseconds of board time was promised, so the table prices the
        // simulation at a couple of milliseconds; half of it takes 70 ms of the
        // clock, so the simulation is worth about 140 and the bar says so.
        let updates = collect(
            Plan {
                bytes: 2_000,
                simulate_s: Some(0.004),
            },
            || {
                begin(Stage::Bind, 1);
                begin(Stage::Simulate, 100);
                std::thread::sleep(Duration::from_millis(70));
                at(50);
            },
        );
        let halfway = updates
            .iter()
            .filter(|u| u.key == "simulate" && u.fraction < 1.0)
            .map(|u| u.fraction)
            .fold(0.0, f64::max);
        assert!(
            halfway > 0.2,
            "half the sim is worth more than the table's guess: {updates:?}"
        );
    }

    #[test]
    fn an_unwatched_thread_pays_nothing_and_nested_scopes_defer_to_the_outer() {
        begin(Stage::Bind, 3);
        at(1);
        let inner = Rc::new(std::cell::Cell::new(0));
        let heard = Rc::clone(&inner);
        let outer = collect(Plan::default(), || {
            with_reporter(Plan::default(), move |_| heard.set(heard.get() + 1), || {});
        });
        // The nested scope's sink is never called; the outer sink heard the run.
        assert!(!outer.is_empty());
        assert_eq!(inner.get(), 0);
    }

    fn nets_counts() -> (u64, u64) {
        let mut out = (0, 0);
        with_tracker(|t| out = (t.stage_done, t.stage_total));
        out
    }

    #[test]
    fn the_lint_stops_at_its_own_share_and_the_layout_checks_take_the_rest() {
        let mut seen = Vec::new();
        collect(
            Plan {
                bytes: 1_000_000,
                simulate_s: None,
            },
            || {
                learned(1_000, 500);
                learned_copper(100_000);
                begin_lint();
                let (_, total) = nets_counts();
                assert!(total > LINT_UNITS, "a board with copper has layout checks");
                // The corner sweep runs the lint again: it must not eat the layout's share.
                lint_advance(70);
                lint_advance(70);
                seen.push(nets_counts().0);
                layout_begins();
                seen.push(nets_counts().0);
                layout_ends();
                seen.push(nets_counts().0);
                layout_ends();
                seen.push(nets_counts().0);
                seen.push(total);
            },
        );
        let total = seen[4];
        assert_eq!(seen[0], LINT_UNITS);
        assert!(seen[1] > seen[0] && seen[1] < total);
        assert_eq!(seen[2], total);
        assert_eq!(seen[3], total, "finishing twice counts once");
    }

    #[test]
    fn a_netlist_has_no_layout_share_in_the_nets_stage() {
        collect(
            Plan {
                bytes: 1_000_000,
                simulate_s: None,
            },
            || {
                learned(1_000, 500);
                learned_copper(0);
                begin_lint();
                assert_eq!(nets_counts().1, LINT_UNITS);
                layout_ends();
                assert_eq!(nets_counts().0, 0, "nothing to credit");
            },
        );
    }

    #[test]
    fn the_bar_shows_percent_and_the_stage_words() {
        let u = Update {
            stage: Stage::Bind,
            key: "bind",
            label: "Matching parts to models",
            fraction: 0.4,
            elapsed: Duration::ZERO,
        };
        assert_eq!(
            render_bar(&u),
            "[########------------]  40%  Matching parts to models"
        );
    }

    #[test]
    fn json_carries_the_stage_label_and_a_rounded_fraction() {
        let u = Update {
            stage: Stage::Bind,
            key: "bind",
            label: "Matching parts to models",
            fraction: 0.123_456_7,
            elapsed: Duration::from_millis(1500),
        };
        let v: serde_json::Value = serde_json::from_str(&u.to_json()).unwrap();
        assert_eq!(v["label"], "Matching parts to models");
        assert_eq!(v["fraction"], 0.1235);
        assert_eq!(v["elapsed_ms"], 1500);
    }
}
