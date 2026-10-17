//! A 74HC14 Schmitt inverter with R from output back to input and C from that
//! input to ground is a relaxation oscillator whose period the RC and the two
//! thresholds set. It used to free-run at one toggle per co-sim chunk with the
//! capacitor decorative (multiplying C by ten left the toggle count IDENTICAL),
//! because each chunk opened with a DC point that discards the charge. Two
//! capacitor values 4.7x apart, each judged against its own analytic period, so
//! a model that ignores C cannot pass both; then the measurement's own honesty,
//! a rate read off too coarse a grid refusing rather than printing a number.

use crate::support;

/// Feedback resistor on the fixture board, plus the 74HC14 card's `ro`: the
/// output is a Thevenin source, so its drive resistance is in series with R.
const R_OHMS: f64 = 10_000.0 + 50.0;

/// The analytic period of the relaxation oscillator, from the `74hc14` card's
/// own params: the input charges from VT- towards VOH and discharges from VT+
/// towards VOL, each leg an RC exponential between the two thresholds.
fn analytic_hz(farads: f64) -> f64 {
    let (voh, vol, vt_plus, vt_minus) = (4.4_f64, 0.1, 2.5, 1.6);
    let period = R_OHMS
        * farads
        * (((voh - vt_minus) / (voh - vt_plus)).ln() + ((vt_plus - vol) / (vt_minus - vol)).ln());
    1.0 / period
}

/// One `toggle` assertion over the oscillator fixture with `C1` overridden.
fn run(
    name: &str,
    cap: &str,
    duration_ms: f64,
    frame_ms: f64,
    want_hz: f64,
) -> hauksbee_ci::CiResult {
    let board = support::repo("crates/hauksbee-ci/tests/fixtures/hc14_oscillator.kicad_pcb");
    support::run_body(
        name,
        &format!(
            "name = \"hc14 oscillator\"\nboard = {}\nduration_ms = {duration_ms}\n\
             frame_ms = {frame_ms}\nambient_c = 25\n\
             [[supply]]\nnet = \"VCC\"\nkind = \"ideal\"\nvolts = 4.5\n\
             [[override]]\nref = \"C1\"\nvalue = \"{cap}\"\n\
             [[assert]]\nkind = \"toggle\"\nnet = \"OSC_OUT\"\nfreq_hz = {want_hz}\n\
             tolerance = 0.05\n",
            support::toml_path(&board)
        ),
    )
}

#[test]
fn the_rc_sets_the_oscillator_period() {
    // (C1, duration_ms, frame_ms): each run covers >100 periods so the partial
    // period at the end is negligible, on a grid far finer than the period.
    for (cap, duration_ms, frame_ms, farads) in
        [("10nF", 20.0, 0.001, 10e-9), ("47nF", 40.0, 0.002, 47e-9)]
    {
        let want = analytic_hz(farads);
        let r = run(&format!("osc_{cap}.toml"), cap, duration_ms, frame_ms, want);
        let a = &r.results[0];
        assert!(
            a.passed && !a.invalid,
            "{cap} should oscillate within 5% of the analytic {want:.0} Hz: {a:?}"
        );
    }
}

#[test]
fn a_rate_read_off_too_coarse_a_grid_refuses() {
    // 50 us frames against an 86 us period: the edges land on the grid, not on
    // the thresholds, so whatever number came out would be the grid's.
    let want = analytic_hz(10e-9);
    let r = run("osc_aliased.toml", "10nF", 3.0, 0.05, want);
    let a = &r.results[0];
    assert!(!a.passed, "an under-sampled rate must not pass: {a:?}");
    assert!(
        a.detail.contains("under-sampled") && a.detail.contains("samples per period"),
        "the refusal must name the sampling grid, not report a frequency: {a:?}"
    );
    let why = a.why.clone().unwrap_or_default();
    assert!(why.contains("frame_ms"), "the user needs the knob: {why}");
}
