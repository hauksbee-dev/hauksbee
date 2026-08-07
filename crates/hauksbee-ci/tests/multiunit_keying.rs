//! Regression: base-ref assertions against per-unit-keyed run maps.
//!
//! A multi-unit package (dual MOSFET, quad switch, ...) stamps one device per
//! unit, so `RunOutcome.peak_temp_c` and the fault list carry per-unit keys
//! ("SW1_q1", "SW1_s0"), never the bare package ref the spec's `max_temp`
//! names. The trackability gate (`check_trackable_assert_refs`) accepts the
//! bare ref because its units ARE monitored, so before the fix `check_max_temp`
//! looked up `peak_temp_c["SW1"]`, always found nothing, and a safety ceiling
//! on a multi-unit package could never fail: a structurally-silent pass.
//!
//! These tests drive the real `assertions::evaluate` over a synthesized
//! `RunOutcome` (same boundary as `analog_invalid.rs`): a unit exceeding the
//! ceiling must FAIL the base-ref assertion, and per-unit overtemperature
//! faults must match a base-ref `max_temp` with no explicit ceiling.

use std::collections::HashMap;

use hauksbee_ci::assertions::evaluate;
use hauksbee_ci::runner::{RunFault, RunOutcome};
use hauksbee_ci::spec::Spec;

/// Write a spec TOML to a uniquely-named temp file and load it (tests run in
/// parallel, so the file name is per-test).
fn load_spec(name: &str, toml: &str) -> Spec {
    let dir = std::env::temp_dir().join("hauksbee_ci_multiunit_keying_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, toml).unwrap();
    Spec::load(&path).expect("spec loads")
}

/// A minimal clean one-seed outcome carrying only the supplied thermal peaks
/// and faults. Everything else is empty / valid.
fn outcome_with(peak_temp_c: HashMap<String, f64>, faults: Vec<RunFault>) -> RunOutcome {
    RunOutcome {
        bind: None,
        seed: 0,
        windows: HashMap::new(),
        uart: HashMap::new(),
        faults,
        toggles: HashMap::new(),
        peak_current: HashMap::new(),
        peak_temp_c,
        peripherals: HashMap::new(),
        rail_windows: HashMap::new(),
        protection_tripped: HashMap::new(),
        protection_tripped_scoped: HashMap::new(),
        ambient_c: 25.0,
        sim_ms: 10.0,
        boot_first_cross_ms: HashMap::new(),
        boot_drop_after_cross_ms: HashMap::new(),
        driven_nets: Default::default(),
        drive_direction_observable: false,
        first_fault_ms: None,
        ac: None,
        analog_valid: true,
        failed_windows: Vec::new(),
        analog_abort: false,
        sampled_values: Vec::new(),
        net_series: HashMap::new(),
        substitutions: Vec::new(),
        coverage_warnings: Vec::new(),
        timing_coverage: Vec::new(),
        timing_refusals: Vec::new(),
        dead_rails: Vec::new(),
        unexercised_bus_ids: Default::default(),
        spi_framing: Default::default(),
    }
}

fn temps(entries: &[(&str, f64)]) -> HashMap<String, f64> {
    entries.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

const MAX_TEMP_85_SPEC: &str = "board=\"b.kicad_pcb\"\nduration_ms=1\n\
    [[assert]]\nkind=\"max_temp\"\nref=\"SW1\"\ncelsius=85.0\n";

/// THE regression: a dual device whose units are keyed SW1_q1/SW1_q2, with one
/// unit over the 85 C ceiling, must FAIL the base-ref max_temp. Before the fix
/// the bare-ref lookup never matched a per-unit key, the peak read as None, and
/// this assertion structurally could not fail.
#[test]
fn max_temp_on_multi_unit_package_fails_when_a_unit_exceeds_ceiling() {
    let spec = load_spec("dual_hot.toml", MAX_TEMP_85_SPEC);
    let out = outcome_with(temps(&[("SW1_q1", 41.0), ("SW1_q2", 96.5)]), Vec::new());
    let results = evaluate(&spec, &[out]);
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(
        !r.passed,
        "a unit at 96.5C must fail the 85C ceiling on the package ref: {}",
        r.detail
    );
    assert!(
        r.detail.contains("96.5"),
        "detail must carry the hottest unit's peak: {}",
        r.detail
    );
    assert!(
        r.detail.contains("SW1_q2"),
        "detail should name the hottest unit: {}",
        r.detail
    );
}

/// The same package with every unit under the ceiling passes, and the pass is
/// a measured one (a real peak, not the "no dissipation" skip).
#[test]
fn max_temp_on_multi_unit_package_passes_on_the_hottest_unit_measurement() {
    let spec = load_spec("dual_cool.toml", MAX_TEMP_85_SPEC);
    let out = outcome_with(temps(&[("SW1_q1", 41.0), ("SW1_q2", 60.0)]), Vec::new());
    let results = evaluate(&spec, &[out]);
    let r = &results[0];
    assert!(r.passed, "60C peak is within 85C: {}", r.detail);
    assert!(
        r.detail.contains("60.0") && !r.detail.contains("no dissipation"),
        "the pass must be measured against the hottest unit, not skipped: {}",
        r.detail
    );
}

/// `_s<N>` unit suffixes (analog-switch style) aggregate the same way.
#[test]
fn max_temp_matches_s_suffixed_units_too() {
    let spec = load_spec("s_suffix.toml", MAX_TEMP_85_SPEC);
    let out = outcome_with(temps(&[("SW1_s0", 90.0)]), Vec::new());
    let results = evaluate(&spec, &[out]);
    assert!(!results[0].passed, "got: {}", results[0].detail);
}

/// The suffix rule must not over-match: SW1 is not a prefix-match for SW10 or
/// for a non-unit underscore name, so another component's heat can never fail
/// this package's ceiling.
#[test]
fn max_temp_does_not_aggregate_other_components() {
    let spec = load_spec("no_overmatch.toml", MAX_TEMP_85_SPEC);
    let out = outcome_with(
        temps(&[("SW10", 150.0), ("SW10_q1", 150.0), ("SW1_heater", 150.0)]),
        Vec::new(),
    );
    let results = evaluate(&spec, &[out]);
    let r = &results[0];
    assert!(
        r.passed,
        "no key belongs to SW1, so the (gate-approved) no-dissipation pass applies: {}",
        r.detail
    );
    assert!(r.detail.contains("no dissipation"), "got: {}", r.detail);
}

/// No explicit ceiling: an overtemperature fault raised against a UNIT of the
/// package must fail the base-ref max_temp (the fault list is keyed by the
/// stamped per-unit device name, same as peak_temp_c).
#[test]
fn max_temp_without_ceiling_matches_per_unit_overtemperature_fault() {
    let spec = load_spec(
        "fault_unit.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[[assert]]\nkind=\"max_temp\"\nref=\"SW1\"\n",
    );
    let out = outcome_with(
        temps(&[("SW1_q1", 41.0), ("SW1_q2", 160.0)]),
        vec![RunFault {
            component: "SW1_q2".to_string(),
            kind: "overtemperature".to_string(),
            value: 160.0,
            limit: 150.0,
            t_ms: 3.0,
        }],
    );
    let results = evaluate(&spec, &[out]);
    let r = &results[0];
    assert!(
        !r.passed,
        "a per-unit overtemperature fault must fail the base-ref assert: {}",
        r.detail
    );
    assert!(r.detail.contains("SW1_q2"), "got: {}", r.detail);
}

/// Single-unit devices keep the exact-key behaviour.
#[test]
fn max_temp_on_single_unit_device_is_unchanged() {
    let spec = load_spec(
        "single.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n\
         [[assert]]\nkind=\"max_temp\"\nref=\"Q7\"\ncelsius=85.0\n",
    );
    let hot = outcome_with(temps(&[("Q7", 91.2)]), Vec::new());
    let results = evaluate(&spec, &[hot]);
    assert!(!results[0].passed, "got: {}", results[0].detail);

    let cool = outcome_with(temps(&[("Q7", 55.0)]), Vec::new());
    let results = evaluate(&spec, &[cool]);
    assert!(results[0].passed, "got: {}", results[0].detail);
}
