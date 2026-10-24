//! Spec parsing, validation, output formats, and the firmware demo run.

use hauksbee_ci::{run, Spec};

use crate::support;

fn load_spec(body: &str) -> Spec {
    Spec::load(&support::write_tmp("spec.toml", body)).expect("spec loads")
}

fn load_error(body: &str) -> String {
    Spec::load(&support::write_tmp("invalid.toml", body))
        .unwrap_err()
        .to_string()
}

#[test]
fn empty_assertions_is_rejected() {
    let err = load_error("name = \"x\"\nboard = \"b.kicad_pcb\"\nduration_ms = 1\n");
    assert!(err.contains("no [[assert]]"), "got: {err}");
}

#[test]
fn asbuilt_field_parses_and_resolves_beside_the_spec() {
    let p = support::write_tmp(
        "asbuilt.toml",
        "board = \"b.kicad_pcb\"\nasbuilt = \"b.asbuilt.toml\"\nduration_ms = 1\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"X\"\nmin = 0.0\nmax = 5.0\n",
    );
    let spec = Spec::load(&p).unwrap();
    let resolved = spec.asbuilt_path().expect("asbuilt path");
    // "beside the spec": same directory as the spec file, exact file name --
    // stronger than matching a hardcoded temp-dir name (an implementation
    // detail of whichever helper wrote the spec, not of the resolution rule
    // under test).
    assert_eq!(
        resolved.parent(),
        p.parent(),
        "asbuilt path must resolve in the spec's own directory: got {resolved:?}"
    );
    assert_eq!(
        resolved.file_name().and_then(|n| n.to_str()),
        Some("b.asbuilt.toml"),
        "got: {resolved:?}"
    );
}

#[test]
fn malformed_assertions_and_unknown_fields_are_rejected() {
    for (case, body, expected) in [
        (
            "unknown assertion kind",
            "board=\"b\"\nduration_ms=1\n[[assert]]\nkind=\"smoke\"\n",
            "unknown assertion kind",
        ),
        (
            "bad UART regex",
            "board=\"b\"\nduration_ms=1\n[[assert]]\nkind=\"uart\"\nmatches=\"(unclosed\"\n",
            "regex",
        ),
        // deny_unknown_fields makes a typo'd key a loud error, not silent.
        (
            "unknown field",
            "board=\"b\"\ndurations_ms=1\n[[assert]]\nkind=\"no_faults\"\n",
            "durations_ms",
        ),
    ] {
        let err = load_error(body);
        assert!(
            err.contains(expected),
            "{case}: expected {expected:?} in {err}"
        );
    }
}

/// Four ways to point an assertion at something the run can never judge: a net
/// that is not on the board, a component that is not on it, a component whose
/// through-current is never measured, and one whose junction temperature never
/// is. All four must be a LOUD rejection at load -- a guard that is never
/// evaluated reads green, which is the failure mode worth spending a test on --
/// and each rejection must name the offending token and say why.
#[test]
fn an_assertion_that_could_never_be_evaluated_is_rejected_not_silently_green() {
    let starter = "crates/hauksbee-ci/tests/fixtures/starter.kicad_pcb";
    let inkplate = "testdata/inkplate_class.net";
    for (case, board, assertion, wants) in [
        (
            "typo'd net",
            starter,
            "kind=\"voltage\"\nnet=\"+5VV\"\nmin=4.9",
            ["not found", "+5V"],
        ),
        (
            "typo'd ref",
            starter,
            "kind=\"max_current\"\nref=\"R11\"\namps=0.1",
            ["unknown component", "R1"],
        ),
        // C1 is a real capacitor, so the typo check passes, but peak current is
        // only measured for resistors and diodes.
        (
            "untracked current",
            inkplate,
            "kind=\"max_current\"\nref=\"C1\"\namps=1.0",
            ["C1", "resistors and diodes"],
        ),
        // U1 (the ESP32 module) is real, but MCUs are not stress-monitored, so
        // no junction temperature is ever estimated for it.
        (
            "untracked temperature",
            inkplate,
            "kind=\"max_temp\"\nref=\"U1\"\ncelsius=85",
            ["U1", "no thermal model"],
        ),
    ] {
        let p = support::write_tmp(
            &format!("{case}.toml"),
            &format!(
                "board={}\nduration_ms=1\n[[assert]]\n{assertion}\n",
                support::toml_path(&support::repo(board))
            ),
        );
        let msg = run(&support::run_config(p)).unwrap_err().to_string();
        for want in wants {
            assert!(msg.contains(want), "{case}: expected {want:?} in: {msg}");
        }
    }
}

#[test]
fn after_ms_on_toggle_is_rejected() {
    let err = load_error(
        "board=\"b\"\nduration_ms=1\n[[assert]]\nkind=\"toggle\"\nnet=\"D13\"\nfreq_hz=5.0\nafter_ms=50\n",
    );
    assert!(err.contains("after_ms"), "got: {err}");
}

#[test]
fn junit_xml_is_well_formed_and_escaped() {
    let result = run(&support::run_config(support::example(
        "power_resistor_cool.toml",
    )))
    .unwrap();
    let xml = result.render_junit();
    assert!(xml.starts_with("<?xml"));
    assert!(xml.contains("<testsuites"));
    assert!(
        xml.contains("&lt;") || xml.contains("&gt;"),
        "comparison operators in details must be escaped"
    );
    // Crude well-formedness: balanced testcase tags.
    let opens = xml.matches("<testcase").count();
    let closes = xml.matches("</testcase>").count();
    assert_eq!(opens, closes);
}

#[test]
fn github_annotations_emit_error_on_failure() {
    let result = run(&support::run_config(support::example(
        "power_resistor_hot.toml",
    )))
    .unwrap();
    let ann = result.render_github_annotations();
    assert!(
        ann.contains("::error"),
        "a failing run must emit ::error: {ann}"
    );
    // Percent signs in the detail must be escaped to %25.
    assert!(!ann.lines().any(|l| l.contains(" %") && !l.contains("%25")));
}

// Boots the AVR demo .hex on the blinky ATmega board, so it needs the
// GPL-gated `avr` feature (the GPL-free renode/qemu build refuses AVR
// firmware by design).
#[cfg(feature = "avr")]
#[test]
fn demo_firmware_blink_uart_and_rail_all_pass() {
    // The full co-sim path: boot the demo firmware, assert rail + UART + blink
    // + no faults. Slower (1 s of simulated time) but exercises the MCU.
    let result =
        run(&support::run_config(support::example("blinky.toml"))).expect("blinky spec runs");
    support::assert_green(&result, "blinky must pass all assertions");
    assert_eq!(result.results.len(), 4);
}

#[test]
fn a_spec_pointing_straight_at_a_kicad_board_runs_end_to_end() {
    // A spec may point straight at a board file: CI loads it in-process with no
    // pre-compile or routing step (the board text carries full net
    // connectivity, and CI is netlist-driven). The board must bind and the
    // voltage assertion on one of its nets must actually evaluate.
    // Its two pads sit 0.4 mm apart, so the static check has no short to flag,
    // and with one pad per net there is nothing left to route.
    let parts = support::comp(
        "R1",
        "Resistor_SMD:R_0402_1005Metric",
        "10k",
        0,
        support::pad(1, "A") + &support::pad(2, "B"),
    );
    let board = support::write_tmp("cell.kicad_pcb", &support::board_text(&[], &parts));
    let p = support::write_tmp(
        "kicad_board.toml",
        &format!(
            "board={}\nduration_ms=1\n[[supply]]\nnet=\"A\"\nkind=\"ideal\"\nvolts=3.3\n\
             [[assert]]\nkind=\"voltage\"\nnet=\"A\"\nmin=3.0\nmax=3.6\n",
            support::toml_path(&board)
        ),
    );
    let result = run(&support::run_config(p)).expect("kicad_pcb spec runs");
    support::assert_green(
        &result,
        "the supplied rail on the loaded board must hold 3.3 V",
    );
}

/// A spec pointing at a `.board` file gets the refusal any unreadable file
/// gets: the formats hauksbee does read, with no special case for the name.
#[test]
fn a_dot_board_file_is_refused_like_any_unrecognised_format() {
    let board = support::write_tmp("something.board", "not a board at all\n");
    let p = support::write_tmp(
        "dot_board.toml",
        &format!(
            "board={}\nduration_ms=1\n[[assert]]\nkind=\"no_faults\"\n",
            support::toml_path(&board)
        ),
    );
    let msg = run(&support::run_config(p)).unwrap_err().to_string();
    assert!(
        msg.contains("unrecognized board format") && msg.contains("KiCad") && msg.contains("Eagle"),
        "the refusal names the accepted formats: {msg}"
    );
    assert!(
        !msg.contains("Board-as-Code") && !msg.contains("DSL"),
        "no special case for the retired name: {msg}"
    );
}

const BOOT_COVERAGE: &str =
    "[[assert]]\nkind = \"boot_coverage\"\nnet = \"HB\"\nmin = 3.0\ndeadline_ms = 10.0\n";

// E51: without firmware this is a hollow gate: the net could only reach its
// level passively, which is the vacuous pass the check exists to prevent.
#[test]
fn boot_coverage_without_firmware_is_refused_at_load() {
    let err = load_error(&format!(
        "board = \"b.kicad_pcb\"\nduration_ms = 10\n{BOOT_COVERAGE}"
    ));
    for want in ["needs `firmware = ...`", "passively", "voltage"] {
        assert!(err.contains(want), "expected {want:?} in: {err}");
    }

    // Control: the same spec WITH a firmware line loads.
    load_spec(&format!(
        "board = \"b.kicad_pcb\"\nfirmware = \"app.elf\"\nduration_ms = 10\n{BOOT_COVERAGE}"
    ));
}

// E32: hold_ms parses on boot_coverage, and a negative value is a load error.
#[test]
fn boot_coverage_hold_ms_parses_and_rejects_negative() {
    let base = format!(
        "board = \"b.kicad_pcb\"\nfirmware = \"app.elf\"\nduration_ms = 10\n{BOOT_COVERAGE}"
    );
    assert_eq!(
        load_spec(&format!("{base}hold_ms = 2.5\n")).asserts[0].hold_ms,
        Some(2.5)
    );
    let err = load_error(&format!("{base}hold_ms = -1.0\n"));
    assert!(
        err.contains("hold_ms") && err.contains(">= 0"),
        "negative hold_ms must be a named load error: {err}"
    );
}

// --- scenario-scope validation (regression) --------------------------------
//
// A `scenario = "id"` scope on an assertion must name a declared [[scenario]]
// id. Before the fix, an unknown scope silently defaulted the window start to
// t=0, so the rail_window was measured over the WHOLE run instead of the
// scenario window it claimed to judge, and the "never sampled in scenario
// window" failure could never fire.

const SCENARIO: &str = "[[scenario]]\nid=\"burst\"\npart=\"U1\"\nprofile=\"p\"\n\
    supply_net=\"+3.3V\"\nstart_ms=1.0\n[[profile]]\nid=\"p\"\n\
    [[profile.segment]]\nlevel_a=0.1\n";

#[test]
fn assertion_scenario_scopes_are_validated() {
    for (case, body, wants) in [
        (
            "typo'd declared scope",
            format!(
                "board=\"b.kicad_pcb\"\nduration_ms=10\n{SCENARIO}\
                 [[assert]]\nkind=\"rail_window\"\nnet=\"+3.3V\"\nmin=3.0\nscenario=\"brust\"\n"
            ),
            &["'brust'", "burst", "whole run"][..],
        ),
        (
            "scope without scenarios",
            "board=\"b.kicad_pcb\"\nduration_ms=10\n[[assert]]\nkind=\"rail_window\"\n\
             net=\"+3.3V\"\nmin=3.0\nscenario=\"burst\"\n"
                .into(),
            &["no [[scenario]]"],
        ),
        // The check covers every assertion kind carrying `scenario`, not just
        // rail_window; no sibling may silently fall back to the whole run.
        (
            "other assertion kind",
            "board=\"b.kicad_pcb\"\nduration_ms=10\n[[assert]]\nkind=\"protection_trip\"\n\
             supply_net=\"VBAT\"\nexpect_trip=true\nscenario=\"nope\"\n"
                .into(),
            &["'nope'"],
        ),
    ] {
        let msg = load_error(&body);
        for want in wants {
            assert!(msg.contains(want), "{case}: expected {want:?} in {msg}");
        }
    }

    // A correctly-scoped rail_window still loads.
    load_spec(&format!(
        "board=\"b.kicad_pcb\"\nduration_ms=10\n{SCENARIO}\
         [[assert]]\nkind=\"rail_window\"\nnet=\"+3.3V\"\nmin=3.0\nscenario=\"burst\"\n"
    ));
}

/// The vendor-recommended ESP32 `CHIP_PU` delay, as a board: 10k from the rail to
/// PU, 1 uF from PU to ground, so the crossing time is `R*C*ln(4)` = 13.86 ms.
fn rc_delay_board() -> String {
    let parts = support::comp(
        "R",
        "Resistor_SMD:R_0805_2012Metric",
        "10k",
        20,
        support::pad(1, "V33") + &support::pad(2, "PU"),
    ) + &support::comp(
        "C",
        "Capacitor_SMD:C_0805_2012Metric",
        "1uF",
        40,
        support::pad(1, "PU") + &support::pad(2, "GND"),
    );
    support::board_text(&["V33", "PU", "GND"], &parts)
}

/// The RC-delay board with `supply` as its `[[supply]]` body and `asserts`
/// appended, run for 40 ms.
fn rc_delay_run(case: &str, supply: &str, asserts: &str) -> hauksbee_ci::CiResult {
    let board = support::placement_board(&format!("{case}.kicad_pcb"), &rc_delay_board());
    support::run_body(
        &format!("{case}.toml"),
        &format!(
            "name = \"{case}\"\nboard = {}\nduration_ms = 40\nframe_ms = 0.05\n\
             [[supply]]\nnet = \"V33\"\nkind = \"ideal\"\nvolts = 3.3\n{supply}{asserts}",
            support::toml_path(&board),
        ),
    )
}

/// An assertion with NO bound is a measurement: it reports its number on every
/// surface, cannot fail, and -- crucially -- does not let a run of nothing but
/// measurements borrow the word GREEN, which is what a reader gates on.
#[test]
fn an_unbounded_assertion_reports_a_number_and_judges_nothing() {
    let probe = "[[assert]]\nkind = \"voltage\"\nname = \"pu\"\nnet = \"PU\"\n";
    let r = rc_delay_run("measure", "", probe);
    let pu = support::row(&r, "pu");
    assert!(pu.measured && pu.passed && !pu.invalid, "{pu:?}");
    assert!(
        pu.detail.contains("measured") && pu.detail.contains("3.3 V"),
        "{}",
        pu.detail
    );
    assert_eq!(r.exit_code(), 0, "a measurement cannot redden a run");
    support::assert_placement_waived(&r);

    let human = r.render_human();
    assert!(human.contains("[MEASURED]"), "{human}");
    assert!(
        human.contains("MEASURED - 1/1 measurement(s) reported, nothing judged"),
        "a measurement-only run must not headline GREEN: {human}"
    );
    assert!(!human.contains("GREEN"), "{human}");
    assert!(
        r.render_json().contains("\"measured\":true"),
        "{}",
        r.render_json()
    );
    assert!(
        r.render_junit().contains("<system-out>measured:"),
        "{}",
        r.render_junit()
    );

    // Mixed with a real bound, the run is judged on that bound as usual and the
    // measurement rides along.
    let mixed = rc_delay_run(
        "mixed",
        "",
        &format!(
            "{probe}[[assert]]\nkind = \"voltage\"\nname = \"rail\"\nnet = \"V33\"\nmin = 3.0\n"
        ),
    );
    // GREEN, conditionally: the asserted net is held by the spec's own ideal
    // source, which the pass states as a reduced-fidelity condition. The
    // pre-layout board's waived not-routed row is counted and shown, not hidden.
    support::assert_placement_waived(&mixed);
    let human = mixed.render_human();
    assert!(
        human.contains("GREEN, conditionally - 2/3 assertions passed")
            && human.contains("1 failure(s) waived, visible above"),
        "{human}"
    );
}

/// A `[[supply]]` starts already settled, so an RC power-on delay reads 0 ms.
/// `ramp_ms` is what makes the cold start real; without it, any result that
/// reads a start-up time has to say the timing is not simulated.
#[test]
fn ramp_ms_models_a_cold_power_on_and_its_absence_is_declared() {
    // R*C*ln(4) = 13.86 ms from a step; the 0.5 ms ramp shifts it slightly
    // later. `min` over a window starting at t is the level AT t on a rising RC.
    let level = |after_ms: f64| {
        format!(
            "[[assert]]\nkind = \"voltage\"\nname = \"pu at {after_ms}\"\nnet = \"PU\"\n\
             min = 2.475\nafter_ms = {after_ms}\n"
        )
    };
    let bracket = format!("{}{}", level(12.5), level(16.0));
    let ramped = rc_delay_run("ramped", "ramp_ms = 0.5\n", &bracket);
    let held =
        |r: &hauksbee_ci::CiResult, label: &str| support::row_is_green(support::row(r, label));
    // Deliberate negative: before the crossing the level is a numeric miss.
    let missed = |r: &hauksbee_ci::CiResult, label: &str| !support::row(r, label).passed;
    assert!(
        missed(&ramped, "pu at 12.5") && held(&ramped, "pu at 16"),
        "the 2.475 V crossing must land between 12.5 and 16 ms: {:?}",
        ramped.results
    );
    assert!(
        !ramped
            .render_human()
            .contains("power-on timing is not simulated"),
        "a ramped rail needs no note: {}",
        ramped.render_human()
    );

    // Without the ramp every window reads the settled rail, so the answer is a
    // convention and the note has to say so, once per timing result.
    let settled = rc_delay_run("settled", "", &bracket);
    assert!(
        held(&settled, "pu at 12.5") && held(&settled, "pu at 16"),
        "{:?}",
        settled.results
    );
    for label in ["pu at 12.5", "pu at 16"] {
        let row = support::row(&settled, label);
        assert!(
            row.detail.contains("power-on timing is not simulated")
                && row.detail.contains("ramp_ms"),
            "{label}: {}",
            row.detail
        );
    }
    // Cheap and precise: a window at t=0 asks no timing question, so no note.
    let t0 = rc_delay_run(
        "t0",
        "",
        "[[assert]]\nkind = \"voltage\"\nname = \"pu\"\nnet = \"PU\"\nmin = 2.475\n",
    );
    assert!(
        !t0.render_human().contains("power-on timing"),
        "{}",
        t0.render_human()
    );
}
