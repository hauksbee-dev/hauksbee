//! Tolerance-ensemble integration tests, anchored to a hand-computable
//! circuit: a 10k/10k divider off an ideal 5 V rail, VOUT = 5 * R2/(R1+R2).
//!
//! With both resistors at ±10% the output spread has *analytic* bounds:
//!   min VOUT = 5 * 9k  / (11k + 9k)  = 2.25 V   (R1 max, R2 min)
//!   max VOUT = 5 * 11k / (9k + 11k)  = 2.75 V   (R1 min, R2 max)
//! The divider is monotonic in each resistor, so the true worst case is a
//! corner. These tests assert the Monte-Carlo ensemble lands strictly INSIDE
//! that envelope and the corner enumeration lands ON it; the analytic
//! spot-check that keeps the sampling and the corner math honest.
//!
//! They also pin the two doctrine properties:
//!   - determinism: the same spec runs to byte-identical sampled values;
//!   - isolation: a failing seed re-run alone (`--seed N`) reproduces the full
//!     run's values exactly (sampling is keyed by the absolute seed number).

use std::path::PathBuf;

use hauksbee_ci::runner::{run_spec, run_spec_seeded};
use hauksbee_ci::{run, RunConfig, Spec};

const V_MIN: f64 = 5.0 * 9_000.0 / (11_000.0 + 9_000.0); // 2.25
const V_MAX: f64 = 5.0 * 11_000.0 / (9_000.0 + 11_000.0); // 2.75

/// The divider board: R1 +5V->VOUT, R2 VOUT->GND, both 10k, plus a spare
/// pulldown R3 on its own net (a strap for the fuzz-composition test).
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "VOUT")
  (net 4 "SPARE")
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 2 "+5V"))
    (pad 2 thru_hole circle (at 2 0) (net 3 "VOUT"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 110)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "VOUT"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 100 120)
    (fp_text reference R3 (at 0 0) (layer F.SilkS))
    (fp_text value 100k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 4 "SPARE"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;

const SPEC_COMMON: &str = r#"duration_ms = 1

[[supply]]
net = "+5V"
kind = "ideal"
volts = 5.0

[[assert]]
kind = "voltage"
net = "VOUT"
min = 2.4
max = 2.6
"#;

/// Write the board + a spec into a per-test temp dir and load the spec.
fn write_spec(test: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hauksbee_ci_tolerance_{test}_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let board = dir.join("divider.kicad_pcb");
    std::fs::write(&board, BOARD).unwrap();
    let spec = dir.join(format!("{test}.toml"));
    std::fs::write(&spec, format!("board = \"divider.kicad_pcb\"\n{body}")).unwrap();
    spec
}

/// The settled VOUT for one outcome (threshold-0 window).
fn vout(out: &hauksbee_ci::runner::RunOutcome) -> f64 {
    out.windows
        .get(&("VOUT".to_string(), 0.0f64.to_bits()))
        .expect("VOUT sampled")
        .last_v
}

fn mc_spec(test: &str, seeds: u32) -> Spec {
    let body = format!(
        "{SPEC_COMMON}\n[[tolerance]]\nref = \"R1\"\npercent = 10.0\n\n\
         [[tolerance]]\nref = \"R2\"\npercent = 10.0\n\n[ensemble]\nseeds = {seeds}\n"
    );
    Spec::load(&write_spec(test, &body)).expect("spec loads")
}

// ── The analytic envelope (gate e) ─────────────────────────────────────────

/// Every Monte-Carlo member's VOUT lies inside the hand-computed [2.25, 2.75]
/// envelope, and seed 0 (the nominal baseline) sits at 2.5 V exactly.
#[test]
fn monte_carlo_ensemble_stays_inside_the_analytic_envelope() {
    let spec = mc_spec("mc_envelope", 32);
    let outcomes = run_spec(&spec).expect("runs");
    assert_eq!(outcomes.len(), 32);
    for out in &outcomes {
        let v = vout(out);
        assert!(
            (V_MIN - 1e-6..=V_MAX + 1e-6).contains(&v),
            "seed {}: VOUT {v} outside analytic envelope [{V_MIN}, {V_MAX}]",
            out.seed
        );
    }
    let nominal = vout(&outcomes[0]);
    assert!(
        (nominal - 2.5).abs() < 1e-6,
        "seed 0 must be the nominal baseline: VOUT {nominal} != 2.5"
    );
    // The ensemble genuinely spreads (it is not stuck at nominal).
    let spread = outcomes
        .iter()
        .map(|o| (vout(o) - 2.5).abs())
        .fold(0.0f64, f64::max);
    assert!(spread > 0.02, "ensemble never moved off nominal: {spread}");
}

/// Corner mode lands ON the analytic envelope: min corner = 2.25 V, max
/// corner = 2.75 V (the divider is monotonic in each R, so corners bound it).
#[test]
fn corners_land_on_the_analytic_envelope() {
    let body = format!(
        "{SPEC_COMMON}\n[[tolerance]]\nref = \"R*\"\npercent = 10.0\n\n\
         [ensemble]\nmode = \"corners\"\n"
    );
    let spec = Spec::load(&write_spec("corners_envelope", &body)).expect("spec loads");
    let outcomes = run_spec(&spec).expect("runs");
    // R1, R2, R3 toleranced by the pattern -> 2^3 = 8 corners (R3 is on its own
    // net and does not move VOUT).
    assert_eq!(outcomes.len(), 8);
    let vs: Vec<f64> = outcomes.iter().map(vout).collect();
    let lo = vs.iter().cloned().fold(f64::INFINITY, f64::min);
    let hi = vs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    assert!(
        (lo - V_MIN).abs() < 1e-6,
        "min corner {lo} != analytic {V_MIN}"
    );
    assert!(
        (hi - V_MAX).abs() < 1e-6,
        "max corner {hi} != analytic {V_MAX}"
    );
    for v in &vs {
        assert!(
            (V_MIN - 1e-6..=V_MAX + 1e-6).contains(v),
            "corner VOUT {v} off envelope"
        );
    }
}

// ── Nominal-passes / ensemble-fails + isolation re-run (gate b) ────────────

/// The flagship shape: nominal (seed 0) passes the [2.4, 2.6] window, the
/// ensemble fails on some sampled seed, and re-running that seed in isolation
/// reproduces its sampled values and measurement bit-for-bit.
#[test]
fn failing_seed_reruns_in_isolation_with_identical_values() {
    let spec = mc_spec("mc_isolation", 24);
    let outcomes = run_spec(&spec).expect("runs");

    // Nominal passes...
    let v0 = vout(&outcomes[0]);
    assert!((2.4..=2.6).contains(&v0), "nominal must pass: {v0}");
    // ...but some sampled member fails.
    let failing = outcomes
        .iter()
        .find(|o| !(2.4..=2.6).contains(&vout(o)))
        .expect("with ±10% on both R, 24 seeds must produce an out-of-window member");

    let isolated = run_spec_seeded(&spec, Some(failing.seed)).expect("isolated run");
    assert_eq!(isolated.len(), 1);
    let iso = &isolated[0];
    assert_eq!(iso.seed, failing.seed);
    assert_eq!(iso.sampled_values.len(), failing.sampled_values.len());
    for (a, b) in iso.sampled_values.iter().zip(&failing.sampled_values) {
        assert_eq!(a.reference, b.reference);
        assert_eq!(
            a.si.to_bits(),
            b.si.to_bits(),
            "isolated {} value {} != full-run {}",
            a.reference,
            a.si,
            b.si
        );
    }
    assert_eq!(
        vout(iso).to_bits(),
        vout(failing).to_bits(),
        "isolated VOUT must reproduce the full run exactly"
    );

    // And the report surfaces the artifact: failing seed + sampled values +
    // pass-rate, with the honest coverage wording on the banner.
    let result = run(&RunConfig {
        spec: spec.base_dir.join("mc_isolation.toml"),
        ..Default::default()
    })
    .expect("ci run");
    assert!(!result.passed());
    let r = &result.results[0];
    assert_eq!(r.failing_seed, Some(failing.seed));
    assert!(r.seeds_total == 24 && !r.failing_seeds.is_empty());
    assert!(
        r.detail.contains("R1=") && r.detail.contains("R2="),
        "detail names values: {}",
        r.detail
    );
    assert!(
        r.detail.contains("/24 seeds"),
        "detail carries the pass-rate: {}",
        r.detail
    );
    let human = result.render_human();
    assert!(
        human.contains("statistical coverage, not worst-case proof"),
        "banner must not over-claim: {human}"
    );
}

// ── Determinism (gate d) ────────────────────────────────────────────────────

/// The same spec run twice produces byte-identical sampled values and
/// measurements.
#[test]
fn ensemble_is_deterministic_across_runs() {
    let spec = mc_spec("mc_determinism", 16);
    let a = run_spec(&spec).expect("first run");
    let b = run_spec(&spec).expect("second run");
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.seed, y.seed);
        for (sx, sy) in x.sampled_values.iter().zip(&y.sampled_values) {
            assert_eq!(sx.reference, sy.reference);
            assert_eq!(sx.si.to_bits(), sy.si.to_bits());
        }
        assert_eq!(vout(x).to_bits(), vout(y).to_bits());
    }
}

// ── Fuzz-stream composition ─────────────────────────────────────────────────

/// Adding tolerances must not change which net-fuzz levels seed N straps: the
/// tolerance stream is domain-separated from the fuzz stream. SPARE is strapped
/// per seed by fuzz; its level per seed must be identical with and without
/// tolerances in the spec.
#[test]
fn tolerances_do_not_perturb_the_fuzz_stream() {
    let fuzz_block = "\n[[net_drive]]\nnet = \"SPARE\"\nvolts = 0.0\n\n\
                      [fuzz]\nseeds = 6\nnets = [\"SPARE\"]\nlevels = [0.0, 5.0]\n";
    let plain = Spec::load(&write_spec(
        "fuzz_plain",
        &format!("{SPEC_COMMON}{fuzz_block}"),
    ))
    .expect("plain spec loads");
    let with_tol = Spec::load(&write_spec(
        "fuzz_with_tol",
        &format!(
            "{SPEC_COMMON}{fuzz_block}\n[[tolerance]]\nref = \"R1\"\npercent = 10.0\n\n\
             [ensemble]\nseeds = 6\n"
        ),
    ))
    .expect("tol spec loads");

    let a = run_spec(&plain).expect("plain runs");
    let b = run_spec(&with_tol).expect("tol runs");
    assert_eq!(a.len(), b.len());
    let spare = |o: &hauksbee_ci::runner::RunOutcome| {
        o.windows
            .get(&("SPARE".to_string(), 0.0f64.to_bits()))
            .expect("SPARE sampled")
            .last_v
    };
    for (x, y) in a.iter().zip(&b) {
        assert!(
            (spare(x) - spare(y)).abs() < 1e-9,
            "seed {}: fuzz strap changed when tolerances were added ({} vs {})",
            x.seed,
            spare(x),
            spare(y)
        );
    }
}

// ── Spec validation & wording ───────────────────────────────────────────────

/// Corner mode refuses to compose with [fuzz], and [ensemble] without any
/// tolerance rule is rejected.
#[test]
fn spec_validation_rejects_bad_ensembles() {
    let corners_fuzz = format!(
        "{SPEC_COMMON}\n[fuzz]\nseeds = 4\nnets = [\"SPARE\"]\n\n\
         [[tolerance]]\nref = \"R1\"\npercent = 5.0\n\n[ensemble]\nmode = \"corners\"\n"
    );
    let err = Spec::load(&write_spec("bad_corners_fuzz", &corners_fuzz)).unwrap_err();
    assert!(
        err.to_string().contains("does not compose with [fuzz]"),
        "{err}"
    );

    let empty_ensemble = format!("{SPEC_COMMON}\n[ensemble]\nseeds = 8\n");
    let err = Spec::load(&write_spec("bad_empty_ensemble", &empty_ensemble)).unwrap_err();
    assert!(err.to_string().contains("nothing to sample"), "{err}");

    let bad_ref = format!("{SPEC_COMMON}\n[[tolerance]]\nref = \"R9\"\npercent = 5.0\n");
    let spec = Spec::load(&write_spec("bad_tol_ref", &bad_ref)).expect("loads");
    let err = run_spec(&spec).unwrap_err();
    assert!(err.to_string().contains("matches no component"), "{err}");
}

/// A green corner run words its claim as monotonic-only bounds, and the fail
/// side names the corner extremes.
#[test]
fn corner_report_wording_claims_only_monotonic_bounds() {
    let body = format!(
        "{SPEC_COMMON}\n[[assert]]\nkind = \"voltage\"\nnet = \"VOUT\"\nmin = 2.2\nmax = 2.8\n\n\
         [[tolerance]]\nref = \"R1\"\npercent = 10.0\n\n\
         [[tolerance]]\nref = \"R2\"\npercent = 10.0\n\n[ensemble]\nmode = \"corners\"\n"
    );
    let spec_path = write_spec("corner_wording", &body);
    let result = run(&RunConfig {
        spec: spec_path,
        ..Default::default()
    })
    .expect("runs");
    let human = result.render_human();
    // The wide [2.2, 2.8] assert passes with the bounded-claim wording...
    assert!(
        human.contains("monotonic"),
        "corner wording must state the monotonicity caveat: {human}"
    );
    // ...the tight [2.4, 2.6] assert fails naming a corner and its values.
    let tight = &result.results[0];
    assert!(!tight.passed);
    assert!(
        tight.detail.contains("corner") && tight.detail.contains("(min)"),
        "corner failure names the extreme: {}",
        tight.detail
    );
}
