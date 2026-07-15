//! Spec parsing, validation, output formats, and the firmware demo run.

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig, Spec};

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

fn write_tmp(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("hauksbee_ci_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

#[test]
fn empty_assertions_is_rejected() {
    let p = write_tmp(
        "noassert.toml",
        "name = \"x\"\nboard = \"b.kicad_pcb\"\nduration_ms = 1\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(err.to_string().contains("no [[assert]]"), "got: {err}");
}

#[test]
fn asbuilt_field_parses_and_resolves_beside_the_spec() {
    let p = write_tmp(
        "asbuilt.toml",
        "board = \"b.kicad_pcb\"\nasbuilt = \"b.asbuilt.toml\"\nduration_ms = 1\n\
         [[assert]]\nkind = \"voltage\"\nnet = \"X\"\nmin = 0.0\nmax = 5.0\n",
    );
    let spec = Spec::load(&p).unwrap();
    let resolved = spec.asbuilt_path().expect("asbuilt path");
    assert!(resolved.ends_with("hauksbee_ci_tests/b.asbuilt.toml"), "got: {resolved:?}");
}

#[test]
fn unknown_assertion_kind_is_rejected() {
    let p = write_tmp(
        "badkind.toml",
        "board=\"b\"\nduration_ms=1\n[[assert]]\nkind=\"smoke\"\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(
        err.to_string().contains("unknown assertion kind"),
        "got: {err}"
    );
}

#[test]
fn bad_regex_in_uart_assert_is_rejected() {
    let p = write_tmp(
        "badre.toml",
        "board=\"b\"\nduration_ms=1\n[[assert]]\nkind=\"uart\"\nmatches=\"(unclosed\"\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(err.to_string().contains("regex"), "got: {err}");
}

#[test]
fn unknown_field_is_rejected() {
    // deny_unknown_fields means a typo'd key is a loud error, not silent.
    let p = write_tmp(
        "typo.toml",
        "board=\"b\"\ndurations_ms=1\n[[assert]]\nkind=\"no_faults\"\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(
        err.to_string().contains("durations_ms") || err.to_string().contains("unknown"),
        "got: {err}"
    );
}

#[test]
fn unknown_net_lists_near_matches() {
    let board =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tarski_brownout_cell.net");
    let p = write_tmp(
        "typonet.toml",
        &format!(
            "board=\"{}\"\nduration_ms=1\n[[assert]]\nkind=\"voltage\"\nnet=\"ANALOG_VDDD\"\nmin=4.9\n",
            board.display()
        ),
    );
    let err = run(&RunConfig { spec: p, ..Default::default() }).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not found"), "got: {msg}");
    assert!(
        msg.contains("ANALOG_VDD"),
        "should suggest the real net: {msg}"
    );
}

#[test]
fn typoed_max_current_ref_is_rejected_not_silently_green() {
    let board =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tarski_brownout_cell.net");
    let p = write_tmp(
        "typoref.toml",
        &format!(
            "board=\"{}\"\nduration_ms=1\n[[assert]]\nkind=\"max_current\"\nref=\"R_Shnt15301\"\namps=0.1\n",
            board.display()
        ),
    );
    let err = run(&RunConfig { spec: p, ..Default::default() }).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown component"), "got: {msg}");
    assert!(
        msg.contains("R_Shunt15301"),
        "should suggest the real ref: {msg}"
    );
}

#[test]
fn max_current_on_untracked_component_kind_is_rejected_not_green() {
    // C1 is a real capacitor on the board, so the typo check passes — but peak
    // current is only measured for resistors and diodes, so the guard would
    // never be evaluated. That must be a loud rejection, never a green pass.
    let board =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/inkplate_class.net");
    let p = write_tmp(
        "untracked_current.toml",
        &format!(
            "board=\"{}\"\nduration_ms=1\n[[assert]]\nkind=\"max_current\"\nref=\"C1\"\namps=1.0\n",
            board.display()
        ),
    );
    let err = run(&RunConfig { spec: p, ..Default::default() }).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("C1"), "should name the ref: {msg}");
    assert!(
        msg.contains("resistors and diodes"),
        "should explain what is trackable: {msg}"
    );
}

#[test]
fn max_temp_on_component_without_thermal_model_is_rejected_not_green() {
    // U1 (the ESP32 module) is a real component, but MCUs are not
    // stress-monitored, so no junction temperature is ever estimated for it: a
    // max_temp guard on it would report green without being evaluated.
    let board =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/inkplate_class.net");
    let p = write_tmp(
        "untracked_temp.toml",
        &format!(
            "board=\"{}\"\nduration_ms=1\n[[assert]]\nkind=\"max_temp\"\nref=\"U1\"\ncelsius=85\n",
            board.display()
        ),
    );
    let err = run(&RunConfig { spec: p, ..Default::default() }).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("U1"), "should name the ref: {msg}");
    assert!(
        msg.contains("no thermal model"),
        "should explain why it is untrackable: {msg}"
    );
}

#[test]
fn after_ms_on_toggle_is_rejected() {
    let p = write_tmp(
        "toggleafter.toml",
        "board=\"b\"\nduration_ms=1\n[[assert]]\nkind=\"toggle\"\nnet=\"D13\"\nfreq_hz=5.0\nafter_ms=50\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(err.to_string().contains("after_ms"), "got: {err}");
}

#[test]
fn junit_xml_is_well_formed_and_escaped() {
    let result = run(&RunConfig {
        spec: example("tarski_brownout_repaired.toml"),
        ..Default::default()
    })
    .unwrap();
    let xml = result.render_junit();
    assert!(xml.starts_with("<?xml"));
    assert!(xml.contains("<testsuites"));
    assert!(xml.contains("&gt;"), "the '>=' in details must be escaped");
    // Crude well-formedness: balanced testcase tags.
    let opens = xml.matches("<testcase").count();
    let closes = xml.matches("</testcase>").count();
    assert_eq!(opens, closes);
}

#[test]
fn github_annotations_emit_error_on_failure() {
    let result = run(&RunConfig {
        spec: example("tarski_brownout.toml"),
        ..Default::default()
    })
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
// GPL-gated `avr` feature (the MIT-clean renode/qemu build refuses AVR
// firmware by design).
#[cfg(feature = "avr")]
#[test]
fn demo_firmware_blink_uart_and_rail_all_pass() {
    // The full co-sim path: boot the demo firmware, assert rail + UART + blink
    // + no faults. Slower (1 s of simulated time) but exercises the MCU.
    let result = run(&RunConfig {
        spec: example("blinky.toml"),
        ..Default::default()
    })
    .expect("blinky spec runs");
    assert!(
        result.passed(),
        "blinky must pass all assertions:\n{}",
        result.render_human()
    );
    assert_eq!(result.results.len(), 4);
}

#[test]
fn boot_coverage_requires_net_min_and_deadline() {
    let board =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tarski_brownout_cell.net");
    // Missing deadline_ms.
    let p = write_tmp(
        "bootcov_bad.toml",
        &format!(
            "board=\"{}\"\nduration_ms=1\n[[assert]]\nkind=\"boot-coverage\"\nnet=\"FOO\"\nmin=3.0\n",
            board.display()
        ),
    );
    let err = run(&RunConfig { spec: p, ..Default::default() }).unwrap_err();
    assert!(err.to_string().contains("deadline_ms"), "got: {err}");
}
