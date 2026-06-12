//! Spec parsing, validation, output formats, and the firmware demo run.

use std::path::PathBuf;

use galvani_ci::{run, RunConfig, Spec};

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples")
        .join(name)
}

fn write_tmp(name: &str, body: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("galvani_ci_tests");
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
    assert!(
        err.to_string().contains("no [[assert]]"),
        "got: {err}"
    );
}

#[test]
fn unknown_assertion_kind_is_rejected() {
    let p = write_tmp(
        "badkind.toml",
        "board=\"b\"\nduration_ms=1\n[[assert]]\nkind=\"smoke\"\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(err.to_string().contains("unknown assertion kind"), "got: {err}");
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
    assert!(err.to_string().contains("durations_ms") || err.to_string().contains("unknown"), "got: {err}");
}

#[test]
fn unknown_net_lists_near_matches() {
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tarski_brownout_cell.net");
    let p = write_tmp(
        "typonet.toml",
        &format!(
            "board=\"{}\"\nduration_ms=1\n[[assert]]\nkind=\"voltage\"\nnet=\"ANALOG_VDDD\"\nmin=4.9\n",
            board.display()
        ),
    );
    let err = run(&RunConfig { spec: p }).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not found"), "got: {msg}");
    assert!(msg.contains("ANALOG_VDD"), "should suggest the real net: {msg}");
}

#[test]
fn typoed_max_current_ref_is_rejected_not_silently_green() {
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/tarski_brownout_cell.net");
    let p = write_tmp(
        "typoref.toml",
        &format!(
            "board=\"{}\"\nduration_ms=1\n[[assert]]\nkind=\"max_current\"\nref=\"R_Shnt15301\"\namps=0.1\n",
            board.display()
        ),
    );
    let err = run(&RunConfig { spec: p }).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown component"), "got: {msg}");
    assert!(msg.contains("R_Shunt15301"), "should suggest the real ref: {msg}");
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
    })
    .unwrap();
    let ann = result.render_github_annotations();
    assert!(ann.contains("::error"), "a failing run must emit ::error: {ann}");
    // Percent signs in the detail must be escaped to %25.
    assert!(!ann.lines().any(|l| l.contains(" %") && !l.contains("%25")));
}

#[test]
fn demo_firmware_blink_uart_and_rail_all_pass() {
    // The full co-sim path: boot the demo firmware, assert rail + UART + blink
    // + no faults. Slower (1 s of simulated time) but exercises the MCU.
    let result = run(&RunConfig {
        spec: example("blinky.toml"),
    })
    .expect("blinky spec runs");
    assert!(
        result.passed(),
        "blinky must pass all assertions:\n{}",
        result.render_human()
    );
    assert_eq!(result.results.len(), 4);
}
