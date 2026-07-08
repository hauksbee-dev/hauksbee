//! The W7 performance regression gate (`08-validation-and-test-campaign.md` §4,
//! `03-solver-performance.md` §9.3).
//!
//! This is the ASSERTING sibling of `tests/perf.rs`. `perf.rs` prints and never
//! asserts ("environments vary"); it stays an exploratory smoke you run by hand.
//! This file turns the plan's soft ±15% wall-time gate into a real, checked-in
//! test with per-machine-class baselines.
//!
//! ## The two-gate philosophy (03 §9.3)
//!
//! The NUMERICAL gates are the hard ones and block unconditionally
//! (bit-identical across threads, tear-vs-monolith to 1e-6, planned-vs-Off to
//! reltol — see `tests/rail_tear.rs`, `tests/planned_assembly.rs`). This SPEED
//! gate is a soft gate: wall time is noisy, so it only ever fires against a
//! same-machine-class baseline, it skips loudly rather than guess, it keys on
//! the QUIET number (the minimum across reps — contention only adds time, so the
//! min is the least-disturbed estimate of true compute cost), and a board whose
//! quiet number is not even reproducible on this machine is marked advisory
//! (tracked, non-blocking) until a quiet-machine recapture confirms it.
//!
//! ## Machine-class keying (the honest scheme)
//!
//! A wall-time comparison across machine classes is worse than none: it flaps or
//! lies. So the gate is keyed by an EXPLICIT class name from `HAUKSBEE_PERF_CLASS`.
//!
//!   * unset / `unclassified` (the default)  -> SKIP, loudly. No guess.
//!   * set, but no `benches/baselines/<class>.toml`  -> SKIP, loudly, telling
//!     you how to capture one.
//!   * set, but the running CPU arch != the arch the baseline was captured on
//!     -> SKIP, loudly (the class name was asserted on the wrong hardware).
//!   * set, file present, arch matches  -> RUN the gate.
//!
//! Setting `HAUKSBEE_PERF_CLASS=<name>` is you ASSERTING "this machine is that
//! class". The dev box (Apple Silicon) is the first class with real baselines:
//! `benches/baselines/apple-silicon-dev.toml`.
//!
//! ## Debug vs release
//!
//! Wall numbers only mean something in `--release`; a debug measurement cannot be
//! compared to a release baseline. Under a plain `cargo test` (debug) the gate
//! SKIPS loudly. Run it for real with:
//!
//! ```sh
//! HAUKSBEE_PERF_CLASS=apple-silicon-dev \
//!   cargo test -p hauksbee-solve --release --test perf_gate perf_gate -- --nocapture
//! ```
//!
//! ## Capturing / updating a baseline
//!
//! `perf_capture_baseline` (ignored) prints a ready-to-paste TOML block with
//! medians and spreads from real runs on this machine. HONEST-UPDATE RULE
//! (08 §4): a baseline change lands in its OWN commit whose message explains the
//! delta (a flame-graph diff or a written reason). Never fold it into an
//! unrelated change; never loosen the gate silently to make a regression pass.

use hauksbee_ir::Circuit;
use hauksbee_solve::{Integration, Partitioning, SolverOptions, StepControl, Transient};
use std::time::Instant;

#[path = "../benches/fixtures.rs"]
mod fixtures;
use fixtures::{build_rc_ladder, build_shunt_array};

// --- workload, identical to benches/graded_boards.rs ------------------------

const RC_STAGES: usize = 1000;
const RC_DT: f64 = 1e-7;
const RC_TSTOP: f64 = 20e-6;

const MIRROR_DT: f64 = 1e-6;
const MIRROR_TSTOP: f64 = 60e-6;
const MIRROR_SIZES: [usize; 3] = [24, 90, 240];

/// The same tight fixed-step options the benchmark harness uses, so a gate
/// measurement drives the engine identically to `graded_boards.rs`.
fn opts(part: Partitioning, dt: f64) -> SolverOptions {
    SolverOptions {
        integration: Integration::Trapezoidal,
        step: StepControl::Fixed { dt },
        reltol: 1e-9,
        vntol: 1e-9,
        max_newton: 200,
        gmin: 1e-9,
        partitioning: part,
        ..SolverOptions::default()
    }
}

/// One timed board: a stable id and a closure that runs exactly the workload the
/// benchmark times. The closure returns nothing; we only care about wall time.
struct Board {
    id: &'static str,
    circuit: Circuit,
    part: Partitioning,
    dt: f64,
    tstop: f64,
}

/// The gated board set: the §4 benchmark subset at smoke scale (the 5-minute
/// marches and the flagship full inference stay out). rc_ladder_1k plus the
/// mirror array at 24/90/240, both torn and monolithic.
fn boards() -> Vec<Board> {
    let mut v = Vec::new();
    v.push(Board {
        id: "rc_ladder_1k",
        circuit: build_rc_ladder(RC_STAGES),
        part: Partitioning::Off,
        dt: RC_DT,
        tstop: RC_TSTOP,
    });
    for &n in &MIRROR_SIZES {
        let (c, _m) = build_shunt_array(n);
        v.push(Board {
            id: leak(format!("mirror_{n}_mono")),
            circuit: c.clone(),
            part: Partitioning::Off,
            dt: MIRROR_DT,
            tstop: MIRROR_TSTOP,
        });
        v.push(Board {
            id: leak(format!("mirror_{n}_torn")),
            circuit: c,
            part: Partitioning::Auto,
            dt: MIRROR_DT,
            tstop: MIRROR_TSTOP,
        });
    }
    v
}

/// Small helper: the board ids are built from sizes, so intern them to `'static`
/// (there are six, once per process — a bounded, deliberate leak).
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

impl Board {
    fn run_once(&self) {
        let wf = Transient::new(opts(self.part, self.dt))
            .run(&self.circuit, self.tstop)
            .expect("perf-gate board converged");
        std::hint::black_box(wf.time.len());
    }

    /// Time `reps` runs (after `warmup` untimed runs) and return per-rep seconds.
    fn measure(&self, warmup: usize, reps: usize) -> Vec<f64> {
        for _ in 0..warmup {
            self.run_once();
        }
        let mut secs = Vec::with_capacity(reps);
        for _ in 0..reps {
            let t0 = Instant::now();
            self.run_once();
            secs.push(t0.elapsed().as_secs_f64());
        }
        secs
    }
}

/// Median of a sample (sorts a copy).
fn median(samples: &[f64]) -> f64 {
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        0.5 * (s[n / 2 - 1] + s[n / 2])
    }
}

/// The QUIET NUMBER: the minimum across reps. On a shared/loaded machine (the
/// dev box regularly sits at load 20+ while sibling builds run), contention only
/// ever ADDS wall time — it never makes a solve faster than its true compute
/// cost. So the minimum is the least-disturbed estimate of that cost, and it is
/// reproducible where the median is not. This is the "pick the quiet-machine
/// number honestly" escape the plan (08 §4) grants for exactly this situation.
/// The gate compares quiet-vs-quiet; the median and spread are reported for
/// transparency only.
fn quiet(samples: &[f64]) -> f64 {
    samples.iter().cloned().fold(f64::INFINITY, f64::min)
}

/// Spread as (max - min) / median: the worst-case relative swing across reps.
/// This is deliberately pessimistic (not an IQR) so a single jittery rep shows
/// up and gets the board marked advisory rather than silently gated on noise.
fn spread_frac(samples: &[f64]) -> f64 {
    let med = median(samples);
    if med <= 0.0 {
        return f64::INFINITY;
    }
    let min = samples.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = samples.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    (max - min) / med
}

fn human_ms(secs: f64) -> String {
    format!("{:.3} ms", secs * 1e3)
}

// --- baseline file ----------------------------------------------------------

#[derive(serde::Deserialize)]
struct Baseline {
    class: String,
    arch: String,
    #[serde(default = "default_tol")]
    tolerance_frac: f64,
    boards: std::collections::BTreeMap<String, BoardBaseline>,
}

#[derive(serde::Deserialize)]
struct BoardBaseline {
    /// The gate number: the quiet (minimum) wall time in ms. See `quiet()`.
    quiet_ms: f64,
    /// Median and capture-time spread, informational: they document how loaded
    /// the machine was when this baseline was taken. Read by humans reviewing
    /// the TOML, not by the gate logic.
    #[serde(default)]
    #[allow(dead_code)]
    median_ms: f64,
    #[serde(default)]
    #[allow(dead_code)]
    spread_frac: f64,
    /// A board whose quiet number is not reproducible on this machine (the long
    /// torn runs can't catch a clean window under load): tracked and reported,
    /// never blocking. Set in the TOML with a reason in the comment above it;
    /// flip to false after a quiet-machine recapture confirms stability.
    #[serde(default)]
    advisory: bool,
}

fn default_tol() -> f64 {
    0.15
}

fn perf_class() -> String {
    std::env::var("HAUKSBEE_PERF_CLASS").unwrap_or_else(|_| "unclassified".to_string())
}

fn baseline_path(class: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("benches")
        .join("baselines")
        .join(format!("{class}.toml"))
}

// Spread threshold (~10% per the plan) used only by the capture tool to warn
// that the quiet number should be checked for reproducibility across two
// captures before a board is gated (rather than left advisory). The live gate
// keys on the quiet number, which is robust to spread, so it does not demote on
// spread per run — advisory is a deliberate, committed decision in the TOML.
const ADVISORY_SPREAD: f64 = 0.10;

/// Reps for the real gate. The gate keys on the QUIET number (min), so it needs
/// enough reps to catch at least one near-uncontended window even on a loaded
/// machine; 21 reliably reaches the floor here (three independent captures of
/// the min agreed to ~1.5% even on the noisy torn boards). Still finishes in a
/// couple of seconds on this fixture set.
const GATE_WARMUP: usize = 2;
const GATE_REPS: usize = 21;

#[test]
fn perf_gate() {
    // 1. Debug builds cannot be compared to a release baseline: skip loudly.
    if cfg!(debug_assertions) {
        eprintln!(
            "PERF GATE SKIPPED: debug build. Wall numbers only mean something in \
             --release.\n  Run: HAUKSBEE_PERF_CLASS=<class> cargo test -p hauksbee-solve \
             --release --test perf_gate perf_gate -- --nocapture"
        );
        return;
    }

    // 2. Resolve the machine class. Unclassified -> no guess, skip loudly.
    let class = perf_class();
    if class == "unclassified" {
        eprintln!(
            "PERF GATE SKIPPED: machine class is 'unclassified' (HAUKSBEE_PERF_CLASS unset).\n  \
             A wall-time comparison on an unknown machine is worse than none. Set \
             HAUKSBEE_PERF_CLASS to a class with a baseline in benches/baselines/, or capture \
             one with:\n  cargo test -p hauksbee-solve --release --test perf_gate \
             perf_capture_baseline -- --ignored --nocapture"
        );
        return;
    }

    // 3. Load the baseline for this class, or skip loudly if none exists.
    let path = baseline_path(&class);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            eprintln!(
                "PERF GATE SKIPPED: no baseline for class '{class}' at {}.\n  Capture one \
                 (honest-update rule 08 §4 — commit it on its own with an explanation):\n  \
                 HAUKSBEE_PERF_CLASS={class} cargo test -p hauksbee-solve --release --test \
                 perf_gate perf_capture_baseline -- --ignored --nocapture",
                path.display()
            );
            return;
        }
    };
    let base: Baseline = toml::from_str(&text).expect("baseline TOML parses");

    // 4. Arch sanity: the class name was asserted on the wrong hardware if the
    //    running arch differs from the one the baseline was captured on.
    let arch = std::env::consts::ARCH;
    if base.arch != arch {
        eprintln!(
            "PERF GATE SKIPPED: baseline '{class}' was captured on arch '{}', but this machine \
             is '{arch}'.\n  The class name is being asserted on the wrong hardware; that \
             comparison would lie. Use a class whose baseline matches this arch.",
            base.arch
        );
        return;
    }

    let tol = base.tolerance_frac;
    eprintln!("=== perf gate: class '{}' (arch {arch}), ±{:.0}% ===", base.class, tol * 100.0);

    let mut violations: Vec<String> = Vec::new();
    for board in boards() {
        let Some(bb) = base.boards.get(board.id) else {
            eprintln!("  {:<16} no baseline entry — skipped", board.id);
            continue;
        };
        let samples = board.measure(GATE_WARMUP, GATE_REPS);
        let quiet_ms = quiet(&samples) * 1e3; // the gate number
        let med_ms = median(&samples) * 1e3; // reported for context
        let spread = spread_frac(&samples);
        let ratio = quiet_ms / bb.quiet_ms - 1.0;

        let tag = if bb.advisory { " [advisory]" } else { "" };
        eprintln!(
            "  {:<16} quiet {:>10} vs baseline {:>10}  ({:+.1}%)  [median {:.3} ms, spread {:.0}%]{}",
            board.id,
            format!("{quiet_ms:.3} ms"),
            format!("{:.3} ms", bb.quiet_ms),
            ratio * 100.0,
            med_ms,
            spread * 100.0,
            tag
        );

        if !bb.advisory && ratio.abs() > tol {
            let dir = if ratio > 0.0 {
                "REGRESSION (slower)"
            } else {
                "SPEEDUP (faster than baseline — a stale baseline is itself a reason to update it)"
            };
            violations.push(format!(
                "  {}: quiet {} vs baseline {}  ({:+.1}%, {})",
                board.id,
                human_ms(quiet_ms / 1e3),
                human_ms(bb.quiet_ms / 1e3),
                ratio * 100.0,
                dir
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "\nPERF GATE FAILED (class '{}', ±{:.0}% wall-time soft gate):\n{}\n\n\
         This is the soft speed gate (03 §9.3); the numerical gates in \
         tests/rail_tear.rs / tests/planned_assembly.rs are the hard ones. You have \
         exactly two honest options:\n\
         \n  1. FIX THE REGRESSION. Profile it: scripts/bench.sh, then a flame graph \
         per docs/dev-plans/perf/README.md (samply on macOS, cargo flamegraph on Linux).\n\
         \n  2. If the change is a legitimate, understood cost (or a real speedup), UPDATE \
         THE BASELINE in\n     {}\n     in a COMMIT WHOSE MESSAGE EXPLAINS THE DELTA (the \
         honest-baseline rule, 08 §4). Recapture with:\n       HAUKSBEE_PERF_CLASS={} cargo \
         test -p hauksbee-solve --release --test perf_gate perf_capture_baseline -- --ignored \
         --nocapture\n     Never loosen the gate silently to make a regression pass.\n",
        base.class,
        tol * 100.0,
        violations.join("\n"),
        baseline_path(&base.class).display(),
        base.class,
    );
}

/// Print a ready-to-paste baseline TOML block from real runs on this machine.
/// Ignored by default; this is the capture tool, not a gate.
///
/// ```sh
/// HAUKSBEE_PERF_CLASS=apple-silicon-dev cargo test -p hauksbee-solve --release \
///   --test perf_gate perf_capture_baseline -- --ignored --nocapture
/// ```
#[test]
#[ignore = "capture tool, not a gate: run explicitly to record/update a baseline"]
fn perf_capture_baseline() {
    if cfg!(debug_assertions) {
        eprintln!("Capture must run in --release, or the numbers are meaningless. Add --release.");
        return;
    }
    let class = perf_class();
    let arch = std::env::consts::ARCH;
    const WARMUP: usize = 3;
    const REPS: usize = 25;

    eprintln!("=== perf baseline capture: class '{class}', arch '{arch}', reps {REPS} ===\n");

    let mut lines = Vec::new();
    lines.push("# Perf baseline — HONEST-UPDATE RULE (08-validation-and-test-campaign.md §4):".to_string());
    lines.push("# change this file only in its OWN commit whose message explains the delta".to_string());
    lines.push("# (a flame-graph diff or a written reason). Never fold a baseline move into an".to_string());
    lines.push("# unrelated change; never loosen the gate silently to make a regression pass.".to_string());
    lines.push(format!("class = \"{class}\""));
    lines.push(format!("arch = \"{arch}\""));
    lines.push("tolerance_frac = 0.15".to_string());
    lines.push(String::new());

    for board in boards() {
        let samples = board.measure(WARMUP, REPS);
        let q_ms = quiet(&samples) * 1e3;
        let med_ms = median(&samples) * 1e3;
        let spread = spread_frac(&samples);
        let max = samples.iter().cloned().fold(f64::NEG_INFINITY, f64::max) * 1e3;
        let noisy = spread > ADVISORY_SPREAD;
        eprintln!(
            "  {:<16} quiet {:>10}  median {:>10}  spread {:>6.0}%  (max {:.3} ms){}",
            board.id,
            human_ms(q_ms / 1e3),
            human_ms(med_ms / 1e3),
            spread * 100.0,
            max,
            if noisy { "  <- high spread: verify the quiet number is reproducible across two captures before gating" } else { "" }
        );
        lines.push(format!("[boards.{}]", board.id));
        lines.push(format!("quiet_ms = {q_ms:.4}"));
        lines.push(format!("median_ms = {med_ms:.4}"));
        lines.push(format!("spread_frac = {spread:.4}"));
        lines.push(String::new());
    }

    eprintln!("\n----- paste into benches/baselines/{class}.toml -----\n");
    eprintln!("{}", lines.join("\n"));
}
