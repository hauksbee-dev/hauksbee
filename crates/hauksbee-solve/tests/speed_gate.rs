//! The faster-than-SPICE claim, defended by an assertion instead of a print.
//!
//! README and `docs/about/COMPARISON.md` market a speed advantage over ngspice.
//! Until now that claim rested on `#[ignore]`d benches in `tests/perf.rs` which
//! PRINT a ratio and assert nothing, so the number in the docs could drift
//! arbitrarily far from the code with no test going red. A marketed number that
//! nothing checks is a number the project is asking to be caught out on.
//!
//! This gate runs the two marketed cases against ngspice, asserts every figure
//! it reports against a recorded floor or bound, and writes
//! `docs/about/speed-gate-results.md` so the docs cite a measured table rather
//! than a memory. With no ngspice on the machine it SKIPS, loudly, naming what
//! it skipped and why: a gate that silently passes when its oracle is missing is
//! worse than no gate.
//!
//! Three things about the METHOD, because the numbers mean nothing without them:
//!
//!   * ngspice's wall clock includes process start (fork, exec, dynamic link;
//!     ~12 ms here). On a circuit that solves in 1 ms that DOMINATES, so a ratio
//!     against it measures `fork()`, not numerics. The gate measures the startup
//!     floor separately and reports both the raw ratio (what a user waiting at a
//!     shell experiences) and the startup-corrected one. Floors are asserted
//!     against the CORRECTED ratio, because only that is a claim about the
//!     solver, and only that is honest to market.
//!   * Accuracy against ngspice is a claim on the rectifier and NOT a claim on
//!     the synapse array. The two engines' voltage-switch models have different
//!     mid-transition conductance shapes, which on a 10 µs edge into a high-gain
//!     current mirror skews the membrane's turn-off by microseconds. That
//!     disagreement is real (it survives a 50x finer time grid and is identical
//!     with partitioning off, so it is not a grid or a decomposition artifact).
//!     It is therefore reported as a disclosed DRIFT CEILING, not as agreement,
//!     and the array's accuracy claim is the one the docs actually make: the
//!     partitioned solve against our own monolithic reference.
//!   * Floors sit well below measured values and bounds well above measured
//!     errors. A gate pinned to its own measurement goes red on unrelated CI
//!     noise and gets disabled, which is how a claim ends up undefended again.
//!
//! Run: `cargo test --release -p hauksbee-solve --test speed_gate -- --nocapture`.
//! `HAUKSBEE_REQUIRE_NGSPICE=1` turns the skip into a hard failure (what CI does,
//! so a runner whose ngspice install broke cannot report green).

#[path = "support/cases.rs"]
mod cases;
#[path = "support/oracle.rs"]
mod oracle;

use cases::{build_synapse_array, rectifier_opts, synapse_opts, RECTIFIER_DECK};
use hauksbee_solve::{Partitioning, Transient};
use std::time::{Duration, Instant};

/// How many times each side of a comparison is run, keeping the best.
///
/// Both sides are measuring a floor (the least time the machine needed), so the
/// minimum is the right statistic and three samples are enough to shake off a
/// single preemption without turning the gate into a long job.
const REPEATS: usize = 3;

// --- recorded floors and bounds ----------------------------------------------
//
// Measured by this very test on Apple Silicon (M-series) against ngspice-45.2,
// release build, best of three per side, across four repeats of the whole gate.
// The measured values are in the generated results table; each floor is about a
// third of its measurement and each bound several times its error.

/// Startup-corrected speedup floor, half-wave rectifier, 5 ms transient.
/// Measured 8.1-10.1x corrected (17.9-20.4x raw).
const RECTIFIER_SPEEDUP_FLOOR: f64 = 3.0;
/// Worst relative error against ngspice on `v(out)`. An ACCURACY CLAIM.
/// Measured 1.9e-3.
const RECTIFIER_NGSPICE_BOUND: f64 = 1e-2;

/// Startup-corrected speedup floor, 90-block synapse array, 400 µs.
/// Measured 10.4-16.7x corrected (10.6-17.0x raw).
const SYNAPSE_SPEEDUP_FLOOR: f64 = 3.5;
/// Ceiling on the DISCLOSED disagreement with ngspice on `v(mem0)`. Not an
/// agreement claim: see the module docs on the switch-model difference. It is
/// gated so the disagreement cannot silently grow. Measured 7.2.
const SYNAPSE_NGSPICE_DRIFT_CEILING: f64 = 12.0;
/// Worst relative error of the partitioned solve against our own monolithic
/// reference, over the WHOLE `v(mem0)` waveform. An ACCURACY CLAIM. Measured
/// 2.0e-3, which is a different and stricter measure than the 1e-7 the docs
/// quote: that figure is the FINAL membrane value (gated separately below).
/// A worst-case over a switching edge is dominated by a sub-step timing
/// difference in where the two paths place the transition, not by drift in the
/// settled answer.
const SYNAPSE_SELF_BOUND: f64 = 1e-2;
/// Relative error of the FINAL `v(mem0)` value, partitioned vs monolithic: the
/// settled-answer figure the docs quote as ~1e-7. An ACCURACY CLAIM.
/// Measured below 1e-6.
const SYNAPSE_SELF_FINAL_BOUND: f64 = 1e-5;

/// What an accuracy number against ngspice means for a case.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NgspiceAccuracy {
    /// The engines are claimed to agree, and the bound enforces it.
    Claim,
    /// The engines are known to disagree for a documented modelling reason. The
    /// bound is a drift ceiling: it stops the disagreement growing, and the
    /// project must never present this case as ngspice agreement.
    DisclosedDrift,
}

/// One measured case, ready for the results table.
struct Case {
    name: &'static str,
    note: &'static str,
    ours: Duration,
    steps: usize,
    ngspice_raw: Duration,
    ngspice_solver: Duration,
    speedup_raw: f64,
    speedup_corrected: f64,
    speedup_floor: f64,
    ngspice_rel: f64,
    ngspice_bound: f64,
    ngspice_kind: NgspiceAccuracy,
    /// Partitioned vs our own monolithic reference over the whole waveform, when
    /// the case has a reference.
    self_rel: Option<f64>,
    self_bound: f64,
    /// The same comparison on the FINAL value only: the settled-answer figure.
    self_final_rel: Option<f64>,
    self_final_bound: f64,
}

impl Case {
    fn passed(&self) -> bool {
        self.speedup_corrected >= self.speedup_floor
            && self.ngspice_rel <= self.ngspice_bound
            && self.self_rel.is_none_or(|r| r <= self.self_bound)
            && self
                .self_final_rel
                .is_none_or(|r| r <= self.self_final_bound)
    }

    fn accuracy_label(&self) -> &'static str {
        match self.ngspice_kind {
            NgspiceAccuracy::Claim => "agreement",
            NgspiceAccuracy::DisclosedDrift => "disclosed drift",
        }
    }
}

// --- the gate ----------------------------------------------------------------

/// What to do when the oracle is or is not on the machine.
///
/// A separate function with its own tests because "skips honestly" is a
/// requirement, and the skip branch is exactly the branch that never runs on a
/// developer machine with ngspice installed. An untested skip is how a gate ends
/// up silently green on every runner that lost its oracle.
#[derive(Debug, PartialEq, Eq)]
enum Oracle {
    /// Proceed: run the gate against this binary.
    Found(std::path::PathBuf),
    /// Skip, printing this reason. The claim is NOT defended by this run.
    Skip(String),
    /// Refuse: the caller demanded the oracle and it is missing.
    Refuse(String),
}

fn oracle_decision(found: Option<std::path::PathBuf>, require: bool) -> Oracle {
    if let Some(bin) = found {
        return Oracle::Found(bin);
    }
    let msg = "the speed gate needs ngspice as its oracle and none was found ($NGSPICE, \
               PATH, or the per-OS default locations). The faster-than-SPICE claim is \
               therefore NOT defended by this run. Install ngspice (brew install ngspice / \
               apt-get install ngspice) and re-run, or set HAUKSBEE_REQUIRE_NGSPICE=1 to \
               make a missing oracle a hard failure.";
    if require {
        Oracle::Refuse(format!("REFUSING: {msg}"))
    } else {
        Oracle::Skip(format!("SKIPPED: {msg}"))
    }
}

#[test]
fn a_missing_oracle_skips_loudly_and_refuses_when_required() {
    let here = std::path::PathBuf::from("/opt/homebrew/bin/ngspice");
    assert_eq!(
        oracle_decision(Some(here.clone()), false),
        Oracle::Found(here.clone()),
        "a present oracle is used, not skipped"
    );
    assert_eq!(
        oracle_decision(Some(here.clone()), true),
        Oracle::Found(here),
        "requiring the oracle changes nothing when it is there"
    );

    let Oracle::Skip(msg) = oracle_decision(None, false) else {
        panic!("a missing oracle must skip when it is not required");
    };
    assert!(msg.starts_with("SKIPPED"), "{msg}");
    assert!(
        msg.contains("NOT defended by this run"),
        "the skip must say the CLAIM went undefended, not merely that a test was \
         skipped: {msg}"
    );
    assert!(
        msg.contains("apt-get install ngspice"),
        "the skip must say how to fix it: {msg}"
    );

    let Oracle::Refuse(msg) = oracle_decision(None, true) else {
        panic!("HAUKSBEE_REQUIRE_NGSPICE must turn a missing oracle into a refusal");
    };
    assert!(msg.starts_with("REFUSING"), "{msg}");
}

#[test]
fn faster_than_ngspice_on_the_marketed_cases() {
    let require = std::env::var("HAUKSBEE_REQUIRE_NGSPICE").is_ok();
    let bin = match oracle_decision(oracle::find_ngspice(), require) {
        Oracle::Found(bin) => bin,
        Oracle::Skip(msg) => {
            eprintln!("{msg}");
            println!("{msg}");
            return;
        }
        Oracle::Refuse(msg) => panic!("{msg}"),
    };
    let version = oracle::ngspice_version(&bin);
    let startup = oracle::ngspice_startup_cost(&bin);
    println!(
        "oracle: {version} at {} (process-start floor {:.3?})",
        bin.display(),
        startup
    );

    let cases = vec![
        measure_rectifier(&bin, startup),
        measure_synapse(&bin, startup, 90),
    ];

    for c in &cases {
        println!(
            "{}: hauksbee {:.3?} ({} steps), ngspice {:.3?} raw / {:.3?} solver-only, \
             speedup {:.2}x raw / {:.2}x corrected (floor {:.2}x), ngspice {} {:.3e} \
             (bound {:.3e}), self waveform {}, self settled {} -> {}",
            c.name,
            c.ours,
            c.steps,
            c.ngspice_raw,
            c.ngspice_solver,
            c.speedup_raw,
            c.speedup_corrected,
            c.speedup_floor,
            c.accuracy_label(),
            c.ngspice_rel,
            c.ngspice_bound,
            c.self_rel
                .map(|r| format!("{r:.3e} (bound {:.3e})", c.self_bound))
                .unwrap_or_else(|| "n/a".to_string()),
            c.self_final_rel
                .map(|r| format!("{r:.3e} (bound {:.3e})", c.self_final_bound))
                .unwrap_or_else(|| "n/a".to_string()),
            if c.passed() { "PASS" } else { "FAIL" }
        );
    }

    write_results_table(&version, startup, &cases);

    // Accuracy first: a speed number next to a wrong answer is worthless, so the
    // accuracy bounds are what give the speed numbers their meaning.
    for c in &cases {
        match c.ngspice_kind {
            NgspiceAccuracy::Claim => assert!(
                c.ngspice_rel <= c.ngspice_bound,
                "{}: worst relative error vs ngspice {:.3e} exceeds the recorded bound \
                 {:.3e}. Fix the accuracy; do NOT relax the bound to make this pass.",
                c.name,
                c.ngspice_rel,
                c.ngspice_bound
            ),
            NgspiceAccuracy::DisclosedDrift => assert!(
                c.ngspice_rel <= c.ngspice_bound,
                "{}: the DISCLOSED disagreement with ngspice grew to {:.3e}, past the \
                 recorded drift ceiling {:.3e}. This case is not an agreement claim, but \
                 the disagreement must not widen unnoticed: find what changed.",
                c.name,
                c.ngspice_rel,
                c.ngspice_bound
            ),
        }
        if let Some(r) = c.self_rel {
            assert!(
                r <= c.self_bound,
                "{}: the partitioned solve drifted {r:.3e} from our own monolithic \
                 reference over the waveform, past the recorded bound {:.3e}.",
                c.name,
                c.self_bound
            );
        }
        if let Some(r) = c.self_final_rel {
            assert!(
                r <= c.self_final_bound,
                "{}: the partitioned solve's SETTLED answer drifted {r:.3e} from our own \
                 monolithic reference, past the recorded bound {:.3e}. The decomposition \
                 must agree on the settled answer; this is a correctness failure, not a \
                 tolerance question.",
                c.name,
                c.self_final_bound
            );
        }
    }
    for c in &cases {
        assert!(
            c.speedup_corrected >= c.speedup_floor,
            "{}: startup-corrected speedup {:.2}x fell below the recorded floor {:.2}x \
             (raw {:.2}x, ngspice solver-only {:.3?}, ours {:.3?}). Either the solver \
             regressed, or the marketed claim no longer holds and the DOCS must change, \
             not this floor.",
            c.name,
            c.speedup_corrected,
            c.speedup_floor,
            c.speedup_raw,
            c.ngspice_solver,
            c.ours
        );
    }
}

fn measure_rectifier(bin: &std::path::Path, startup: Duration) -> Case {
    let circuit = hauksbee_ir::SpiceLoader::load(RECTIFIER_DECK).expect("rectifier deck loads");
    let opts = rectifier_opts();
    // One warm run, discarded: the timed runs must not pay first-touch page
    // faults when ngspice's equivalent cost is separately accounted for.
    let _ = Transient::new(opts).run(&circuit, 5e-3);
    let mut ours = Duration::from_secs(3600);
    let mut wf = None;
    for _ in 0..REPEATS {
        let t0 = Instant::now();
        let w = Transient::new(opts)
            .run(&circuit, 5e-3)
            .expect("rectifier solves");
        ours = ours.min(t0.elapsed());
        wf = Some(w);
    }
    let wf = wf.expect("REPEATS is at least one");

    let ours_series = series(&wf, &circuit, "out");
    let (stdout, raw) = oracle::run_ngspice_best_of(bin, RECTIFIER_DECK, "rectifier", REPEATS)
        .expect("ngspice runs the rectifier");
    let want = oracle::parse_tran_table(&stdout);
    assert!(
        want.len() > 100,
        "ngspice produced no usable v(out) table for the rectifier ({} rows): the oracle \
         is not answering, so no speed claim can be made from this run",
        want.len()
    );
    let ngspice_rel = oracle::worst_rel_error(&ours_series, &want);

    let solver = solver_time(raw, startup);
    Case {
        name: "half-wave rectifier, 5 ms tran",
        note: "accuracy is v(out) against ngspice's own .print tran table",
        ours,
        steps: wf.time.len(),
        ngspice_raw: raw,
        ngspice_solver: solver,
        speedup_raw: raw.as_secs_f64() / ours.as_secs_f64(),
        speedup_corrected: solver.as_secs_f64() / ours.as_secs_f64(),
        speedup_floor: RECTIFIER_SPEEDUP_FLOOR,
        ngspice_rel,
        ngspice_bound: RECTIFIER_NGSPICE_BOUND,
        ngspice_kind: NgspiceAccuracy::Claim,
        self_rel: None,
        self_bound: f64::INFINITY,
        self_final_rel: None,
        self_final_bound: f64::INFINITY,
    }
}

fn measure_synapse(bin: &std::path::Path, startup: Duration, blocks: usize) -> Case {
    let syn = build_synapse_array(blocks);
    let opts = synapse_opts();
    let _ = Transient::new(opts).run(&syn.circuit, 400e-6);
    let mut ours = Duration::from_secs(3600);
    let mut wf = None;
    for _ in 0..REPEATS {
        let t0 = Instant::now();
        let w = Transient::new(opts)
            .run(&syn.circuit, 400e-6)
            .expect("synapse array solves");
        ours = ours.min(t0.elapsed());
        wf = Some(w);
    }
    let wf = wf.expect("REPEATS is at least one");
    let ours_series = series(&wf, &syn.circuit, "mem0");

    // The array's actual marketed accuracy figure: the partitioned solve against
    // our own monolithic reference. Untimed, so it does not pollute the ratio.
    let mut mono_opts = opts;
    mono_opts.partitioning = Partitioning::Off;
    let mono = Transient::new(mono_opts)
        .run(&syn.circuit, 400e-6)
        .expect("monolithic reference solves");
    let mono_series = series(&mono, &syn.circuit, "mem0");
    let self_rel = oracle::worst_rel_error(&ours_series, &mono_series);
    // The settled answer, compared across every block's membrane rather than
    // just mem0: a decomposition that is exact on one block and wrong on the
    // ninetieth is not exact.
    let mut self_final_rel = 0.0f64;
    for k in 0..blocks {
        let name = format!("mem{k}");
        let a = mono.final_node(&syn.circuit, &name).unwrap_or(0.0);
        let b = wf.final_node(&syn.circuit, &name).unwrap_or(0.0);
        self_final_rel = self_final_rel.max((a - b).abs() / a.abs().max(0.1));
    }

    let (stdout, raw) = oracle::run_ngspice_best_of(bin, &syn.netlist, "synapse", REPEATS)
        .expect("ngspice runs the synapse array");
    let want = oracle::parse_tran_table(&stdout);
    assert!(
        want.len() > 100,
        "ngspice produced no usable v(mem0) table for the synapse array ({} rows): the \
         oracle is not answering, so no speed claim can be made from this run",
        want.len()
    );
    let ngspice_rel = oracle::worst_rel_error(&ours_series, &want);

    let solver = solver_time(raw, startup);
    Case {
        name: "synapse array, 90 blocks, 400 us tran",
        note: "NOT an ngspice agreement claim: the two voltage-switch models have \
               different mid-transition conductance shapes, which skews the membrane \
               turn-off on a 10 us edge. The claim here is partitioned vs our own \
               monolithic reference",
        ours,
        steps: wf.time.len(),
        ngspice_raw: raw,
        ngspice_solver: solver,
        speedup_raw: raw.as_secs_f64() / ours.as_secs_f64(),
        speedup_corrected: solver.as_secs_f64() / ours.as_secs_f64(),
        speedup_floor: SYNAPSE_SPEEDUP_FLOOR,
        ngspice_rel,
        ngspice_bound: SYNAPSE_NGSPICE_DRIFT_CEILING,
        ngspice_kind: NgspiceAccuracy::DisclosedDrift,
        self_rel: Some(self_rel),
        self_bound: SYNAPSE_SELF_BOUND,
        self_final_rel: Some(self_final_rel),
        self_final_bound: SYNAPSE_SELF_FINAL_BOUND,
    }
}

fn series(
    wf: &hauksbee_solve::Waveforms,
    circuit: &hauksbee_ir::Circuit,
    node: &str,
) -> Vec<(f64, f64)> {
    let v = wf
        .node(circuit, node)
        .unwrap_or_else(|| panic!("the circuit has a '{node}' node"));
    wf.time.iter().copied().zip(v.iter().copied()).collect()
}

/// ngspice's solver-only time: its wall clock less the process-start floor.
///
/// Never zero or negative: on a machine where the circuit is cheaper than
/// fork/exec the honest statement is "below the measurement floor", represented
/// as the floor itself rather than as an infinite speedup.
fn solver_time(raw: Duration, startup: Duration) -> Duration {
    raw.checked_sub(startup)
        .unwrap_or(Duration::ZERO)
        .max(Duration::from_micros(1))
}

/// Write the measured table where the docs can cite it.
fn write_results_table(version: &str, startup: Duration, cases: &[Case]) {
    let mut s = String::new();
    s.push_str("<!-- GENERATED by `cargo test --release -p hauksbee-solve --test speed_gate`.\n");
    s.push_str("     Do not hand-edit: the next run overwrites it. -->\n\n");
    s.push_str("# Speed gate results\n\n");
    s.push_str(
        "Every number here is asserted, not observed. The gate fails the build when a \
         startup-corrected speedup drops below its recorded floor, when an accuracy claim \
         rises above its bound, or when a disclosed disagreement widens past its ceiling.\n\n",
    );
    s.push_str(&format!(
        "Oracle: {version}. ngspice process-start floor: {:.3} ms, measured as the minimum \
         of five trivial-deck runs.\n\n",
        startup.as_secs_f64() * 1e3
    ));
    s.push_str(
        "`raw` divides ngspice's whole wall clock, process start included, by ours: it is \
         what a user waiting at a shell experiences. `corrected` subtracts the process-start \
         floor first: it is what the two solvers actually did, and it is the number the gate \
         asserts, because it is the only one that is a claim about numerics.\n\n",
    );
    s.push_str(
        "| case | hauksbee | steps | ngspice (raw) | ngspice (solver only) | speedup raw | \
         speedup corrected | floor | vs ngspice | kind | bound | vs own monolith \
         (waveform) | vs own monolith (settled) | verdict |\n",
    );
    s.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for c in cases {
        s.push_str(&format!(
            "| {} | {:.3} ms | {} | {:.3} ms | {:.3} ms | {:.2}x | {:.2}x | {:.2}x | \
             {:.3e} | {} | {:.3e} | {} | {} | {} |\n",
            c.name,
            c.ours.as_secs_f64() * 1e3,
            c.steps,
            c.ngspice_raw.as_secs_f64() * 1e3,
            c.ngspice_solver.as_secs_f64() * 1e3,
            c.speedup_raw,
            c.speedup_corrected,
            c.speedup_floor,
            c.ngspice_rel,
            c.accuracy_label(),
            c.ngspice_bound,
            c.self_rel
                .map(|r| format!("{r:.3e} (bound {:.3e})", c.self_bound))
                .unwrap_or_else(|| "n/a".to_string()),
            c.self_final_rel
                .map(|r| format!("{r:.3e} (bound {:.3e})", c.self_final_bound))
                .unwrap_or_else(|| "n/a".to_string()),
            if c.passed() { "PASS" } else { "FAIL" }
        ));
    }
    s.push_str(
        "\nAccuracy against ngspice is measured on the oracle's own `.print tran` table, \
         sampled at the oracle's time points with linear interpolation into ours, relative \
         to `max(|oracle|, 1% of full scale)` so a zero crossing cannot manufacture an \
         infinite error.\n\n\
         A row marked `agreement` is a claim that the two engines match, enforced by its \
         bound. A row marked `disclosed drift` is a known disagreement with a documented \
         modelling cause: the bound stops it widening, and the project must never present \
         such a row as ngspice agreement.\n\n",
    );
    for c in cases {
        s.push_str(&format!("- **{}**: {}.\n", c.name, c.note));
    }

    // Best-effort: a read-only checkout still runs the gate, and the assertions
    // ARE the gate. Losing the table is not losing the check.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/about/speed-gate-results.md");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::write(&path, &s) {
        Ok(()) => println!("results table written to {}", path.display()),
        Err(e) => println!("could not write {}: {e}", path.display()),
    }
}
