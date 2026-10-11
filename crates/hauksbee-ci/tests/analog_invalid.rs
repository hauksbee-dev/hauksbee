//! Finding 3 gate. When
//! the analog co-sim fails a chunk under an assertion's evaluation window, that
//! assertion must be reported INVALID (a distinct outcome, not a pass and not an
//! ordinary fail) and the run must exit 3 (invalid-for-analysis), even when the
//! failures never reach the 3-consecutive strict abort. A tripped abort forces
//! exit 3 on its own.
//!
//! These tests exercise the real `assertions::evaluate` overlap logic and the
//! real `CiResult::exit_code` aggregation over a synthesized `RunOutcome`. An
//! end-to-end diverging board cannot be built through the hauksbee-ci spec
//! surface: a singular analog system needs two ideal voltage sources on one node
//! (as the engine-level `cosim_failed_chunk::impossible_board` does directly),
//! but the spec's `net_drive` is a no-op on a rail that already has a source, its
//! `stimulus` injects through a 50 ohm series resistor (never singular), a
//! `[[supply]]` reconfigures the existing leg rather than adding a second, and a
//! `KiCad` board file cannot express two ideal rails on one net. So the divergence
//! itself is proven at the scheduler level (`cosim_failed_chunk.rs`), and the
//! CI-layer consequences are proven here at the outcome/evaluation boundary.

use std::collections::HashMap;

use hauksbee_ci::assertions::evaluate;
use hauksbee_ci::runner::{NetWindow, RunOutcome};

use crate::support::{assert_row_green, ci_result, load_spec, row_is_green};

/// A minimal one-seed outcome carrying a single settled voltage window on `net`
/// (a value that would PASS a `>= 3.0 V` check) plus a caller-supplied analog
/// validity state. Everything else is empty. `sim_ms` sets the run length so the
/// assertion's evaluation window is `[after_ms, sim_ms]`.
fn outcome_with(
    net: &str,
    settled_v: f64,
    sim_ms: f64,
    analog_valid: bool,
    failed_windows: Vec<(f64, f64)>,
    analog_abort: bool,
) -> RunOutcome {
    let mut windows: HashMap<(String, u64), NetWindow> = HashMap::new();
    // The `voltage` assertion buckets by (net, after_ms-bits); after_ms defaults
    // to 0.0, so key on the 0.0 threshold.
    windows.insert(
        (net.to_string(), 0.0_f64.to_bits()),
        NetWindow {
            min_v: settled_v,
            max_v: settled_v,
            last_v: settled_v,
            samples: 10,
        },
    );
    RunOutcome {
        windows,
        ambient_c: 25.0,
        sim_ms,
        analog_valid,
        failed_windows,
        analog_abort,
        ..Default::default()
    }
}

const VOLTAGE_SPEC: &str =
    "board=\"b.kicad_pcb\"\nduration_ms=1\n[[assert]]\nkind=\"voltage\"\nnet=\"n1\"\nmin=3.0\n";

#[test]
fn assertion_over_failed_window_is_invalid_and_exits_3_without_abort() {
    let spec = load_spec("overlap.toml", VOLTAGE_SPEC);
    // The rail settled at 5 V (would pass >= 3 V), but the solver failed a chunk
    // in [0, 0.5 ms], inside the assertion's [0, 1 ms] window. Two failed chunks
    // is BELOW the 3-consecutive abort, so analog_abort is false.
    let out = outcome_with("n1", 5.0, 1.0, false, vec![(0.0, 0.0005)], false);
    let results = evaluate(&spec, &[out]);
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert!(
        r.invalid,
        "the assertion must be INVALID, not evaluated: {}",
        r.detail
    );
    assert!(!r.passed, "an invalid assertion is never counted as passed");
    assert!(
        r.detail.contains("INVALID") && r.detail.to_lowercase().contains("held-stale"),
        "detail must explain the invalidity: {}",
        r.detail
    );

    let result = ci_result(results, false);
    assert!(result.analog_invalid(), "the run is invalid for analysis");
    assert_eq!(
        result.exit_code(),
        3,
        "an INVALID assertion exits 3 even without a consecutive abort"
    );
    // The rendered surfaces name the invalidity distinctly from pass/fail.
    assert!(
        result.render_human().contains("[INVALID]"),
        "{}",
        result.render_human()
    );
    assert!(
        result.render_junit().contains("<error"),
        "junit uses <error> for invalid"
    );
    assert!(result.render_github_annotations().contains("INVALID"));
}

#[test]
fn assertion_clear_of_failed_window_passes_normally() {
    let spec = load_spec("clear.toml", VOLTAGE_SPEC);
    // A failed chunk at [2 ms, 3 ms] is AFTER the run's 1 ms end, so it cannot
    // overlap the assertion's [0, 1 ms] window: the assertion evaluates normally.
    let out = outcome_with("n1", 5.0, 1.0, false, vec![(0.002, 0.003)], false);
    let results = evaluate(&spec, &[out]);
    let r = &results[0];
    assert!(
        !r.invalid,
        "a non-overlapping failed window does not invalidate"
    );
    assert_row_green(r, "5 V clears the >= 3 V bound");

    let result = ci_result(results, false);
    assert_eq!(result.exit_code(), 0, "a clean, passing assertion exits 0");
}

#[test]
fn tripped_abort_forces_exit_3_even_when_assertions_pass() {
    // A UART assertion is not analog-transient-derived, so it is never marked
    // INVALID by a failed analog chunk, but a tripped strict abort still forces
    // the whole run to refuse (exit 3).
    let spec = load_spec(
        "abort.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[[assert]]\nkind=\"uart\"\ncontains=\"hi\"\n",
    );
    let mut out = outcome_with("n1", 5.0, 1.0, false, vec![(0.0, 0.0003)], true);
    out.uart.insert("U1".to_string(), "hi there".to_string());
    let results = evaluate(&spec, &[out]);
    assert_row_green(&results[0], "the UART assertion itself passes");
    assert!(
        !results[0].invalid,
        "a UART assertion is not analog-invalidated"
    );

    let result = ci_result(results, true);
    // Deliberately numeric: every assertion passed, yet the tripped abort makes
    // the run INVALID (exit 3), which is exactly the gap `assert_green` guards.
    assert!(result.passed(), "every assertion passed");
    assert_eq!(
        result.exit_code(),
        3,
        "a tripped analog abort refuses even when assertions pass"
    );

    // C5.3: exit 3 is useful, not just distinct. The exact same structured
    // contract must feed JSON, terminal and CI artifacts, while preserving the
    // UART assertion that was evaluated independently of the failed analog side.
    let refusal = result.refusal().expect("exit 3 always carries a refusal");
    assert!(refusal.claim.contains("overall CI verdict"), "{refusal:#?}");
    assert!(
        refusal
            .missing_prerequisite
            .contains("converged analog solve"),
        "{refusal:#?}"
    );
    assert!(
        refusal
            .valid_partial_conclusions
            .iter()
            .any(|p| p.contains("UART") || p.contains("non-analog")),
        "the passing non-analog conclusion must be preserved: {refusal:#?}"
    );
    assert!(refusal.next_action.contains("rerun"), "{refusal:#?}");

    let json: serde_json::Value =
        serde_json::from_str(&result.render_json()).expect("CI JSON is valid");
    assert_eq!(json["exit_code"], 3);
    assert_eq!(json["refusal"]["claim"], refusal.claim);
    let junit = result
        .render_junit()
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    for rendered in [result.render_human(), junit] {
        for field in [
            &refusal.claim,
            &refusal.missing_prerequisite,
            &refusal.valid_partial_conclusions.join("; "),
            &refusal.next_action,
        ] {
            assert!(
                rendered.contains(field),
                "surface lost {field:?}:\n{rendered}"
            );
        }
    }
}

#[test]
fn tripped_abort_with_no_invalid_assertion_errors_in_junit_not_all_green() {
    // Same state as `tripped_abort_forces_exit_3_even_when_assertions_pass`: the
    // abort tripped but the only assertion (UART, not analog-derived) passed, so
    // no per-assertion result carries INVALID. The run exits 3, and the JUnit
    // surface must agree: `render_human` and `render_github_annotations` both
    // special-case this state, so `render_junit` must too, with one synthetic
    // errored testcase instead of a false `failures="0" errors="0"` ALL-GREEN.
    let spec = load_spec(
        "abort_junit.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=1\n[[assert]]\nkind=\"uart\"\ncontains=\"hi\"\n",
    );
    let mut out = outcome_with("n1", 5.0, 1.0, false, vec![(0.0, 0.0003)], true);
    out.uart.insert("U1".to_string(), "hi there".to_string());
    let results = evaluate(&spec, &[out]);
    assert!(results.iter().all(|r| r.passed && !r.invalid));

    let result = ci_result(results, true);
    assert_eq!(result.exit_code(), 3);
    let xml = result.render_junit();
    assert!(
        xml.contains("errors=\"1\"") && !xml.contains("errors=\"0\""),
        "junit must count the abort as an error, not report all-green: {xml}"
    );
    // 1 real assertion + 1 synthetic abort testcase.
    assert!(xml.contains("tests=\"2\""), "{xml}");
    let refusal = result.refusal().expect("exit 3 carries a refusal contract");
    assert!(
        xml.contains("<error") && xml.contains(&refusal.claim),
        "the synthetic errored testcase must be present and explain itself: {xml}"
    );
}

#[test]
fn abort_with_an_invalid_assertion_does_not_double_count_in_junit() {
    // When an assertion already carries the INVALID, the synthetic testcase must
    // NOT be added on top: the error count stays at the per-assertion count.
    let spec = load_spec("abort_junit_dup.toml", VOLTAGE_SPEC);
    let out = outcome_with("n1", 5.0, 1.0, false, vec![(0.0, 0.0005)], true);
    let results = evaluate(&spec, &[out]);
    assert_eq!(results.len(), 1);
    assert!(results[0].invalid);

    let result = ci_result(results, true);
    let xml = result.render_junit();
    assert!(
        xml.contains("tests=\"1\""),
        "no synthetic testcase added: {xml}"
    );
    assert!(xml.contains("errors=\"1\""), "{xml}");
}

/// A UART assertion whose text is missing at the end of a run on a host-paced
/// MCU clock is neither a PASS nor a FAIL: it is INVALID (exit 3), and the
/// refusal says to lengthen the run or change backend, not "inspect the failed
/// analog window".
#[test]
fn host_paced_uart_miss_is_invalid_and_the_refusal_names_the_clock() {
    let spec = load_spec(
        "host_paced_uart.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=200\n[[assert]]\nkind=\"uart\"\ncontains=\"boot ok\"\n",
    );
    let mut out = outcome_with("n1", 5.0, 200.0, true, vec![], false);
    out.host_paced_mcus = vec!["U1".to_string()];
    out.uart
        .insert("U1".to_string(), "ROM banner only".to_string());
    let results = evaluate(&spec, &[out.clone()]);
    let r = &results[0];
    assert!(r.invalid && !r.passed, "{}", r.detail);
    assert!(
        r.detail.contains("INVALID: host-paced clock:"),
        "{}",
        r.detail
    );

    let result = ci_result(results, false);
    assert_eq!(result.exit_code(), 3);
    let refusal = result.refusal().expect("an INVALID assertion refuses");
    assert!(
        refusal.missing_prerequisite.contains("host-paced firmware")
            && !refusal.missing_prerequisite.contains("analog solve"),
        "{}",
        refusal.missing_prerequisite
    );
    assert!(
        refusal.next_action.contains("duration_ms") && refusal.next_action.contains("simavr"),
        "{}",
        refusal.next_action
    );

    // The same silence on a clock-exact backend is a plain FAIL, exit 1.
    out.host_paced_mcus.clear();
    let results = evaluate(&spec, &[out]);
    assert!(!results[0].invalid && !results[0].passed);
    assert_eq!(ci_result(results, false).exit_code(), 1);
}

/// A 200 ms run on which firmware on host-paced MCU `U1` drives `EN`: `whole`
/// is its (min, max) over the run, `after` its (min, max) from 50 ms on, and it
/// ends at the `after` max.
fn firmware_level_run(whole: (f64, f64), after: (f64, f64), host_paced: bool) -> RunOutcome {
    let mut out = outcome_with("EN", whole.1, 200.0, true, vec![], false);
    for (thr, (min_v, max_v)) in [(0.0_f64, whole), (50.0, after)] {
        out.windows.insert(
            ("EN".to_string(), thr.to_bits()),
            NetWindow {
                min_v,
                max_v,
                last_v: after.1,
                samples: 10,
            },
        );
    }
    out.driven_nets.insert("EN".to_string());
    if host_paced {
        out.host_paced_mcus = vec!["U1".to_string()];
    }
    out
}

/// A level asserted after a simulated-time window start on a firmware-driven
/// net, on a host-paced clock: a pass is conditional and the condition reaches
/// the run verdict; a level that holds only inside the window refuses (exit 3)
/// with the host-paced refusal; and a clock-exact run judges the window.
#[test]
fn host_paced_windowed_level_is_a_conditional_pass_or_a_refusal() {
    let spec = load_spec(
        "host_paced_level.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=200\n[[assert]]\nname=\"EN high\"\nkind=\"voltage\"\n\
         net=\"EN\"\nmin=3.0\nafter_ms=50\n",
    );
    let results = evaluate(&spec, &[firmware_level_run((3.3, 3.3), (3.3, 3.3), true)]);
    assert_row_green(&results[0], "whole-run level");
    let mut result = ci_result(results, false);
    result.verdict_conditions = hauksbee_ci::assertions::host_paced_conditions(&result.results);
    assert_eq!(result.exit_code(), 0);
    let human = result.render_human();
    assert!(human.contains("GREEN, conditionally"), "{human}");
    assert!(
        human.contains("CONDITION: EN high: the 50 ms after_ms window was not judged"),
        "{human}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&result.render_json()).expect("the report is JSON");
    assert!(
        json["verdict_conditions"][0]
            .as_str()
            .is_some_and(|c| c.contains("host-paced clock")),
        "{json:#}"
    );

    let results = evaluate(&spec, &[firmware_level_run((0.0, 3.3), (3.3, 3.3), true)]);
    assert!(
        results[0].invalid && !results[0].passed,
        "{}",
        results[0].detail
    );
    let result = ci_result(results, false);
    assert_eq!(result.exit_code(), 3);
    let refusal = result.refusal().expect("an INVALID assertion refuses");
    assert!(
        refusal
            .missing_prerequisite
            .contains("simulated-time window"),
        "{}",
        refusal.missing_prerequisite
    );
    assert!(
        refusal.next_action.contains("drop the time window"),
        "{}",
        refusal.next_action
    );

    // The same waveform on a clock-exact backend: the window is judged, PASS.
    let results = evaluate(&spec, &[firmware_level_run((0.0, 3.3), (3.3, 3.3), false)]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
    assert!(hauksbee_ci::assertions::host_paced_conditions(&results).is_empty());
}

/// [`firmware_level_run`] read on `LOAD`, a load-switch output the firmware
/// moves through `EN` without driving it.
fn load_switch_run(whole: (f64, f64), after: (f64, f64), host_paced: bool) -> RunOutcome {
    let mut out = firmware_level_run(whole, after, host_paced);
    for thr in [0.0_f64, 50.0] {
        let win = out.windows[&("EN".to_string(), thr.to_bits())].clone();
        out.windows.insert(("LOAD".to_string(), thr.to_bits()), win);
    }
    out.influenced_nets
        .insert("LOAD".to_string(), "EN".to_string());
    out
}

/// A windowed level on a net downstream of a firmware-driven GPIO (a MOSFET
/// load switch's output) follows the host-paced level rule end to end: a
/// conditional GREEN whose condition names the driven net, a refusal when only
/// the window passes, and a hard verdict on a clock-exact backend.
#[test]
fn host_paced_windowed_level_downstream_of_firmware_is_conditional_or_refused() {
    let spec = load_spec(
        "host_paced_downstream.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=200\n[[assert]]\nname=\"load on\"\nkind=\"voltage\"\n\
         net=\"LOAD\"\nmin=3.0\nafter_ms=50\n",
    );
    let results = evaluate(&spec, &[load_switch_run((3.3, 3.3), (3.3, 3.3), true)]);
    assert_row_green(&results[0], "whole-run level");
    let mut result = ci_result(results, false);
    result.verdict_conditions = hauksbee_ci::assertions::host_paced_conditions(&result.results);
    assert_eq!(result.exit_code(), 0);
    let human = result.render_human();
    assert!(human.contains("GREEN, conditionally"), "{human}");
    assert!(
        human.contains(
            "CONDITION: load on: the 50 ms after_ms window on 'LOAD', downstream of \
             firmware-driven 'EN' was not judged"
        ),
        "{human}"
    );

    let results = evaluate(&spec, &[load_switch_run((0.0, 3.3), (3.3, 3.3), true)]);
    assert!(
        results[0].invalid && !results[0].passed,
        "{}",
        results[0].detail
    );
    assert!(
        results[0]
            .detail
            .contains("'LOAD' is downstream of firmware-driven 'EN'"),
        "{}",
        results[0].detail
    );
    assert_eq!(ci_result(results, false).exit_code(), 3);

    let results = evaluate(&spec, &[load_switch_run((0.0, 3.3), (3.3, 3.3), false)]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
    assert!(hauksbee_ci::assertions::host_paced_conditions(&results).is_empty());
}

/// [`firmware_level_run`] read on `VOUT`, a DAC output the firmware sets by
/// writing the I2C bus whose `SDA` it drives (no MCU pin drives `VOUT`).
fn dac_output_run(whole: (f64, f64), after: (f64, f64), host_paced: bool) -> RunOutcome {
    let mut out = firmware_level_run(whole, after, host_paced);
    out.driven_nets.remove("EN");
    out.driven_nets.insert("SDA".to_string());
    for thr in [0.0_f64, 50.0] {
        let win = out.windows[&("EN".to_string(), thr.to_bits())].clone();
        out.windows.insert(("VOUT".to_string(), thr.to_bits()), win);
    }
    out.influenced_nets
        .insert("VOUT".to_string(), "SDA".to_string());
    out
}

/// A windowed level on a DAC output the firmware writes over a bus is as
/// host-dependent as one on a pin it drives: a conditional GREEN naming the
/// bus net on a host-paced clock, a refusal when only the window passes, and a
/// hard verdict on a clock-exact backend.
#[test]
fn host_paced_windowed_level_on_a_bus_driven_dac_output_is_conditional_or_refused() {
    let spec = load_spec(
        "host_paced_dac.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=200\n[[assert]]\nname=\"setpoint\"\nkind=\"voltage\"\n\
         net=\"VOUT\"\nmin=3.0\nafter_ms=50\n",
    );
    let results = evaluate(&spec, &[dac_output_run((3.3, 3.3), (3.3, 3.3), true)]);
    assert_row_green(&results[0], "whole-run level");
    let mut result = ci_result(results, false);
    result.verdict_conditions = hauksbee_ci::assertions::host_paced_conditions(&result.results);
    assert_eq!(result.exit_code(), 0);
    let human = result.render_human();
    assert!(human.contains("GREEN, conditionally"), "{human}");
    assert!(
        human.contains(
            "CONDITION: setpoint: the 50 ms after_ms window on 'VOUT', downstream of \
             firmware-driven 'SDA' was not judged"
        ),
        "{human}"
    );

    // The level holds only inside the window: a host-paced run cannot say
    // when the DAC was written, so the claim is refused.
    let results = evaluate(&spec, &[dac_output_run((0.0, 3.3), (3.3, 3.3), true)]);
    assert!(
        results[0].invalid && !results[0].passed,
        "{}",
        results[0].detail
    );
    assert!(
        results[0]
            .detail
            .contains("'VOUT' is downstream of firmware-driven 'SDA'"),
        "{}",
        results[0].detail
    );
    assert_eq!(ci_result(results, false).exit_code(), 3);

    // A clock-exact backend orders the DAC write in simulated time: hard.
    let results = evaluate(&spec, &[dac_output_run((0.0, 3.3), (3.3, 3.3), false)]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
    assert!(hauksbee_ci::assertions::host_paced_conditions(&results).is_empty());

    // Without the walk's attribution the same net keeps its hard verdict,
    // which is what the scheduler's seeds change.
    let mut bare = dac_output_run((0.0, 3.3), (3.3, 3.3), true);
    bare.influenced_nets.clear();
    let results = evaluate(&spec, &[bare]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
}

/// [`firmware_level_run`] read on `FILT`, an RC filter behind `UART_TX`, a
/// transmit line the firmware drove by sending UART bytes (no GPIO write is
/// reported for it, and no other pin drives `FILT`).
fn uart_filter_run(whole: (f64, f64), after: (f64, f64), host_paced: bool) -> RunOutcome {
    let mut out = firmware_level_run(whole, after, host_paced);
    out.driven_nets.remove("EN");
    out.driven_nets.insert("UART_TX".to_string());
    for thr in [0.0_f64, 50.0] {
        let win = out.windows[&("EN".to_string(), thr.to_bits())].clone();
        out.windows.insert(("FILT".to_string(), thr.to_bits()), win);
    }
    out.influenced_nets
        .insert("FILT".to_string(), "UART_TX".to_string());
    out
}

/// A windowed level behind a UART transmit line is as host-dependent as one
/// behind a GPIO: a conditional GREEN naming the TX net on a host-paced clock,
/// a refusal when only the window passes, and a hard verdict on a clock-exact
/// backend.
#[test]
fn host_paced_windowed_level_behind_a_uart_tx_line_is_conditional_or_refused() {
    let spec = load_spec(
        "host_paced_uart_rc.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=200\n[[assert]]\nname=\"filtered\"\nkind=\"voltage\"\n\
         net=\"FILT\"\nmin=3.0\nafter_ms=50\n",
    );
    let results = evaluate(&spec, &[uart_filter_run((3.3, 3.3), (3.3, 3.3), true)]);
    assert_row_green(&results[0], "whole-run level");
    let mut result = ci_result(results, false);
    result.verdict_conditions = hauksbee_ci::assertions::host_paced_conditions(&result.results);
    assert_eq!(result.exit_code(), 0);
    let human = result.render_human();
    assert!(human.contains("GREEN, conditionally"), "{human}");
    assert!(
        human.contains(
            "CONDITION: filtered: the 50 ms after_ms window on 'FILT', downstream of \
             firmware-driven 'UART_TX' was not judged"
        ),
        "{human}"
    );

    // The level holds only inside the window: refused, not passed.
    let results = evaluate(&spec, &[uart_filter_run((0.0, 3.3), (3.3, 3.3), true)]);
    assert!(
        results[0].invalid && !results[0].passed,
        "{}",
        results[0].detail
    );
    assert!(
        results[0]
            .detail
            .contains("'FILT' is downstream of firmware-driven 'UART_TX'"),
        "{}",
        results[0].detail
    );
    assert_eq!(ci_result(results, false).exit_code(), 3);

    // A clock-exact backend orders the bytes in simulated time: hard.
    let results = evaluate(&spec, &[uart_filter_run((0.0, 3.3), (3.3, 3.3), false)]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
    assert!(hauksbee_ci::assertions::host_paced_conditions(&results).is_empty());

    // Legal control: the same windowed claim on a net the walk did not reach
    // keeps its hard verdict on a host-paced clock.
    let mut other = uart_filter_run((0.0, 3.3), (3.3, 3.3), true);
    other.influenced_nets.clear();
    let results = evaluate(&spec, &[other]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
    assert!(hauksbee_ci::assertions::host_paced_conditions(&results).is_empty());
}

/// [`uart_filter_run`] with the filter behind `I2C_SCL`, a bus clock line the
/// hardware I2C block drove (the scheduler counts it once the core delivered
/// I2C events, whether or not the backend reported a GPIO write for it).
fn i2c_filter_run(whole: (f64, f64), after: (f64, f64), host_paced: bool) -> RunOutcome {
    let mut out = uart_filter_run(whole, after, host_paced);
    out.driven_nets.remove("UART_TX");
    out.driven_nets.insert("I2C_SCL".to_string());
    out.influenced_nets
        .insert("FILT".to_string(), "I2C_SCL".to_string());
    out
}

/// A windowed level behind an I2C clock line is as host-dependent as one
/// behind a GPIO: a conditional GREEN naming the bus net on a host-paced
/// clock, a refusal when only the window passes, a hard verdict on a
/// clock-exact backend, and a hard verdict again when the walk did not reach
/// the net.
#[test]
fn host_paced_windowed_level_behind_an_i2c_line_is_conditional_or_refused() {
    let spec = load_spec(
        "host_paced_i2c_rc.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=200\n[[assert]]\nname=\"filtered\"\nkind=\"voltage\"\n\
         net=\"FILT\"\nmin=3.0\nafter_ms=50\n",
    );
    let results = evaluate(&spec, &[i2c_filter_run((3.3, 3.3), (3.3, 3.3), true)]);
    assert_row_green(&results[0], "whole-run level");
    let mut result = ci_result(results, false);
    result.verdict_conditions = hauksbee_ci::assertions::host_paced_conditions(&result.results);
    assert_eq!(result.exit_code(), 0);
    let human = result.render_human();
    assert!(human.contains("GREEN, conditionally"), "{human}");
    assert!(
        human.contains(
            "CONDITION: filtered: the 50 ms after_ms window on 'FILT', downstream of \
             firmware-driven 'I2C_SCL' was not judged"
        ),
        "{human}"
    );

    // The level holds only inside the window: refused, not passed.
    let results = evaluate(&spec, &[i2c_filter_run((0.0, 3.3), (3.3, 3.3), true)]);
    assert!(
        results[0].invalid && !results[0].passed,
        "{}",
        results[0].detail
    );
    assert!(
        results[0]
            .detail
            .contains("'FILT' is downstream of firmware-driven 'I2C_SCL'"),
        "{}",
        results[0].detail
    );
    assert_eq!(ci_result(results, false).exit_code(), 3);

    // A clock-exact backend orders the bus events in simulated time: hard.
    let results = evaluate(&spec, &[i2c_filter_run((0.0, 3.3), (3.3, 3.3), false)]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
    assert!(hauksbee_ci::assertions::host_paced_conditions(&results).is_empty());

    // Legal control: the same windowed claim on a net the walk did not reach
    // keeps its hard verdict on a host-paced clock.
    let mut other = i2c_filter_run((0.0, 3.3), (3.3, 3.3), true);
    other.influenced_nets.clear();
    let results = evaluate(&spec, &[other]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
    assert!(hauksbee_ci::assertions::host_paced_conditions(&results).is_empty());
}

/// [`firmware_level_run`] read on `FILT`, an RC filter behind `PWM_OUT`, an
/// output-capable pad of host-paced `U1` with no drive evidence, on a backend
/// that cannot observe peripheral routing: the scheduler lists the pad and the
/// nets behind it in `peripheral_pad_nets`, unobserved.
fn unobserved_pad_run(whole: (f64, f64), after: (f64, f64), host_paced: bool) -> RunOutcome {
    let mut out = firmware_level_run(whole, after, host_paced);
    out.driven_nets.clear();
    for thr in [0.0_f64, 50.0] {
        let win = out.windows[&("EN".to_string(), thr.to_bits())].clone();
        out.windows.insert(("FILT".to_string(), thr.to_bits()), win);
    }
    let pad = hauksbee_engine::scheduler::PeripheralPad {
        net: "PWM_OUT".to_string(),
        mcu: "U1".to_string(),
        pin: "GPIO5".to_string(),
        unobserved: Some(
            "a build that reports the matrix comes from scripts/install-sims.sh \
             --qemu-patched-source"
                .to_string(),
        ),
    };
    for net in ["PWM_OUT", "FILT"] {
        out.peripheral_pad_nets.insert(net.to_string(), pad.clone());
    }
    out
}

/// A windowed level behind a pad the firmware may drive through a peripheral
/// output the backend could not observe: a whole-run pass is a conditional
/// GREEN naming the pad; a pass only inside the window is INVALID (exit 3),
/// never a hard FAIL, with the pad, the blind spot and the remedy in the
/// reason; a clock-exact backend keeps the hard verdict; and a pad the
/// scheduler did not list (input-only, unwired or already driven) is unaffected.
#[test]
fn host_paced_windowed_level_behind_an_unobserved_pad_is_conditional_or_refused() {
    let spec = load_spec(
        "host_paced_unobserved_pad.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=200\n[[assert]]\nname=\"filtered\"\nkind=\"voltage\"\n\
         net=\"FILT\"\nmin=3.0\nafter_ms=50\n",
    );
    let results = evaluate(&spec, &[unobserved_pad_run((3.3, 3.3), (3.3, 3.3), true)]);
    assert_row_green(&results[0], "whole-run level");
    let mut result = ci_result(results, false);
    result.verdict_conditions = hauksbee_ci::assertions::host_paced_conditions(&result.results);
    assert_eq!(result.exit_code(), 0);
    let human = result.render_human();
    assert!(human.contains("GREEN, conditionally"), "{human}");
    assert!(
        human.contains(
            "CONDITION: filtered: the 50 ms after_ms window on 'FILT', downstream of MCU U1 \
             pad GPIO5 ('PWM_OUT')"
        ),
        "{human}"
    );

    // The level holds only inside the window: INVALID, not a hard FAIL.
    let results = evaluate(&spec, &[unobserved_pad_run((0.0, 3.3), (3.3, 3.3), true)]);
    assert!(
        results[0].invalid && !results[0].passed,
        "{}",
        results[0].detail
    );
    for part in [
        "'FILT' is downstream of MCU U1 pad GPIO5 ('PWM_OUT')",
        "the backend could not observe peripheral routing on this pad",
        "scripts/install-sims.sh --qemu-patched-source",
        "host-paced clock",
    ] {
        assert!(
            results[0].detail.contains(part),
            "{part}: {}",
            results[0].detail
        );
    }
    let result = ci_result(results, false);
    assert_eq!(result.exit_code(), 3);
    let json = result.render_json();
    assert!(
        json.contains("scripts/install-sims.sh --qemu-patched-source"),
        "{json}"
    );

    // A clock-exact backend orders the pad's output in simulated time: hard.
    let results = evaluate(&spec, &[unobserved_pad_run((0.0, 3.3), (3.3, 3.3), false)]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
    assert!(hauksbee_ci::assertions::host_paced_conditions(&results).is_empty());

    // An input-only pad has no output driver, so the scheduler lists nothing
    // for it: the windowed claim keeps its hard verdict on a host-paced clock.
    let mut input_only = unobserved_pad_run((0.0, 3.3), (3.3, 3.3), true);
    input_only.peripheral_pad_nets.clear();
    let results = evaluate(&spec, &[input_only]);
    assert!(row_is_green(&results[0]) && results[0].host_paced.is_none());
    assert!(hauksbee_ci::assertions::host_paced_conditions(&results).is_empty());
}
