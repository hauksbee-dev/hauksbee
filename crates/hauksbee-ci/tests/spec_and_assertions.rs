//! Spec parsing, validation, output formats, and the firmware demo run.

mod support;

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
    assert!(
        resolved.ends_with("hauksbee_ci_tests/b.asbuilt.toml"),
        "got: {resolved:?}"
    );
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
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/board-as-code/starter.board");
    let p = write_tmp(
        "typonet.toml",
        &format!(
            "board={}\nduration_ms=1\n[[assert]]\nkind=\"voltage\"\nnet=\"+5VV\"\nmin=4.9\n",
            support::toml_path(&board)
        ),
    );
    let err = run(&RunConfig {
        spec: p,
        ..Default::default()
    })
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("not found"), "got: {msg}");
    assert!(msg.contains("+5V"), "should suggest the real net: {msg}");
}

#[test]
fn typoed_max_current_ref_is_rejected_not_silently_green() {
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/board-as-code/starter.board");
    let p = write_tmp(
        "typoref.toml",
        &format!(
            "board={}\nduration_ms=1\n[[assert]]\nkind=\"max_current\"\nref=\"R11\"\namps=0.1\n",
            support::toml_path(&board)
        ),
    );
    let err = run(&RunConfig {
        spec: p,
        ..Default::default()
    })
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown component"), "got: {msg}");
    assert!(msg.contains("R1"), "should suggest the real ref: {msg}");
}

#[test]
fn max_current_on_untracked_component_kind_is_rejected_not_green() {
    // C1 is a real capacitor on the board, so the typo check passes, but peak
    // current is only measured for resistors and diodes, so the guard would
    // never be evaluated. That must be a loud rejection, never a green pass.
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/inkplate_class.net");
    let p = write_tmp(
        "untracked_current.toml",
        &format!(
            "board={}\nduration_ms=1\n[[assert]]\nkind=\"max_current\"\nref=\"C1\"\namps=1.0\n",
            support::toml_path(&board)
        ),
    );
    let err = run(&RunConfig {
        spec: p,
        ..Default::default()
    })
    .unwrap_err();
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
    let board = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/inkplate_class.net");
    let p = write_tmp(
        "untracked_temp.toml",
        &format!(
            "board={}\nduration_ms=1\n[[assert]]\nkind=\"max_temp\"\nref=\"U1\"\ncelsius=85\n",
            support::toml_path(&board)
        ),
    );
    let err = run(&RunConfig {
        spec: p,
        ..Default::default()
    })
    .unwrap_err();
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
        spec: example("power_resistor_cool.toml"),
        ..Default::default()
    })
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
    let result = run(&RunConfig {
        spec: example("power_resistor_hot.toml"),
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
// GPL-gated `avr` feature (the GPL-free renode/qemu build refuses AVR
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
fn board_as_code_spec_runs_end_to_end() {
    // B5: a spec may point straight at a Board-as-Code `.board` source.
    // hauksbee-ci used to reject it with "compile it to a layout first with
    // from-code --route"; the shared normalizer compiles it in-process (the
    // compiled text carries full net connectivity, and CI is netlist-driven,
    // so no routing step is needed). The board must bind and the voltage
    // assertion on one of its nets must actually evaluate.
    let dir = std::env::temp_dir().join("hauksbee_ci_tests");
    std::fs::create_dir_all(&dir).unwrap();
    let board = dir.join("cell.board");
    std::fs::write(
        &board,
        r#"# Board-as-Code (hauksbee board DSL v1)
board version 20241229

fn main {
    net "A"
    net "B"
    comp R1 lib "Resistor_SMD:R_0402_1005Metric" val "10k" layer "F.Cu" at 0 0 rot 0 {
        pad "1" smd rect at 0 0 size 1 1 layers [F.Cu] net "A"
        pad "2" smd rect at 1 0 size 1 1 layers [F.Cu] net "B"
    }
}
"#,
    )
    .unwrap();
    let p = write_tmp(
        "board_as_code.toml",
        &format!(
            "board={}\nduration_ms=1\n[[supply]]\nnet=\"A\"\nkind=\"ideal\"\nvolts=3.3\n\
             [[assert]]\nkind=\"voltage\"\nnet=\"A\"\nmin=3.0\nmax=3.6\n",
            support::toml_path(&board)
        ),
    );
    let result = run(&RunConfig {
        spec: p,
        ..Default::default()
    })
    .expect(".board spec runs");
    assert!(
        result.passed(),
        "the supplied rail on the compiled .board must hold 3.3 V:\n{}",
        result.render_human()
    );
}

#[test]
fn boot_coverage_requires_net_min_and_deadline() {
    let board =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/tarski_brownout_cell.net");
    // Missing deadline_ms.
    let p = write_tmp(
        "bootcov_bad.toml",
        &format!(
            "board={}\nduration_ms=1\n[[assert]]\nkind=\"boot-coverage\"\nnet=\"FOO\"\nmin=3.0\n",
            support::toml_path(&board)
        ),
    );
    let err = run(&RunConfig {
        spec: p,
        ..Default::default()
    })
    .unwrap_err();
    assert!(err.to_string().contains("deadline_ms"), "got: {err}");
}

// E51: boot_coverage with NO firmware staged is a hollow gate; the net could
// only reach its level passively (a board pull / bias settling), which is the
// vacuous pass the check exists to prevent. The spec must refuse to LOAD.
#[test]
fn boot_coverage_without_firmware_is_refused_at_load() {
    let p = write_tmp(
        "bootcov_no_fw.toml",
        "board = \"b.kicad_pcb\"\nduration_ms = 10\n\
         [[assert]]\nkind = \"boot_coverage\"\nnet = \"RES\"\nmin = 2.6\ndeadline_ms = 5.0\n",
    );
    let err = Spec::load(&p).unwrap_err().to_string();
    assert!(
        err.contains("needs `firmware = ...`"),
        "the refusal must say the fix (add firmware): {err}"
    );
    assert!(
        err.contains("passively") && err.contains("voltage"),
        "the refusal must explain the passive-reach trap and point at `voltage`: {err}"
    );

    // Control: the same spec WITH a firmware line loads.
    let p = write_tmp(
        "bootcov_fw.toml",
        "board = \"b.kicad_pcb\"\nfirmware = \"app.elf\"\nduration_ms = 10\n\
         [[assert]]\nkind = \"boot_coverage\"\nnet = \"RES\"\nmin = 2.6\ndeadline_ms = 5.0\n",
    );
    Spec::load(&p).expect("boot_coverage with firmware loads");
}

// E32: hold_ms parses on boot_coverage, and a negative value is a load error.
#[test]
fn boot_coverage_hold_ms_parses_and_rejects_negative() {
    let p = write_tmp(
        "bootcov_hold.toml",
        "board = \"b.kicad_pcb\"\nfirmware = \"app.elf\"\nduration_ms = 10\n\
         [[assert]]\nkind = \"boot_coverage\"\nnet = \"HB\"\nmin = 3.0\ndeadline_ms = 10.0\nhold_ms = 2.5\n",
    );
    let spec = Spec::load(&p).expect("hold_ms parses");
    assert_eq!(spec.asserts[0].hold_ms, Some(2.5));

    let p = write_tmp(
        "bootcov_hold_neg.toml",
        "board = \"b.kicad_pcb\"\nfirmware = \"app.elf\"\nduration_ms = 10\n\
         [[assert]]\nkind = \"boot_coverage\"\nnet = \"HB\"\nmin = 3.0\ndeadline_ms = 10.0\nhold_ms = -1.0\n",
    );
    let err = Spec::load(&p).unwrap_err().to_string();
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

#[test]
fn rail_window_scoped_to_undeclared_scenario_is_rejected() {
    let p = write_tmp(
        "railwin_badscope.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=10\n\
         [[scenario]]\nid=\"burst\"\npart=\"U1\"\nprofile=\"p\"\nsupply_net=\"+3.3V\"\nstart_ms=1.0\n\
         [[profile]]\nid=\"p\"\n[[profile.segment]]\nlevel_a=0.1\n\
         [[assert]]\nkind=\"rail_window\"\nnet=\"+3.3V\"\nmin=3.0\nscenario=\"brust\"\n",
    );
    let err = Spec::load(&p).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("'brust'"), "must name the bad scope: {msg}");
    assert!(msg.contains("burst"), "must list the declared ids: {msg}");
    assert!(
        msg.contains("whole run"),
        "must explain the silent-whole-run hazard: {msg}"
    );
}

#[test]
fn scenario_scope_with_no_scenarios_declared_is_rejected() {
    let p = write_tmp(
        "railwin_noscenarios.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=10\n\
         [[assert]]\nkind=\"rail_window\"\nnet=\"+3.3V\"\nmin=3.0\nscenario=\"burst\"\n",
    );
    let err = Spec::load(&p).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("no [[scenario]]"),
        "must say no scenarios are declared: {msg}"
    );
}

// The scope check covers every assertion kind that carries `scenario`, not
// just rail_window: any sibling scoped to an unknown id is rejected the same
// way rather than silently evaluated whole-run.
#[test]
fn any_assert_kind_scoped_to_undeclared_scenario_is_rejected() {
    let p = write_tmp(
        "trip_badscope.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=10\n\
         [[assert]]\nkind=\"protection_trip\"\nsupply_net=\"VBAT\"\nexpect_trip=true\nscenario=\"nope\"\n",
    );
    let err = Spec::load(&p).unwrap_err();
    assert!(err.to_string().contains("'nope'"), "got: {err}");
}

// A correctly-scoped rail_window (declared id) still loads.
#[test]
fn rail_window_scoped_to_declared_scenario_loads() {
    let p = write_tmp(
        "railwin_goodscope.toml",
        "board=\"b.kicad_pcb\"\nduration_ms=10\n\
         [[scenario]]\nid=\"burst\"\npart=\"U1\"\nprofile=\"p\"\nsupply_net=\"+3.3V\"\nstart_ms=1.0\n\
         [[profile]]\nid=\"p\"\n[[profile.segment]]\nlevel_a=0.1\n\
         [[assert]]\nkind=\"rail_window\"\nnet=\"+3.3V\"\nmin=3.0\nscenario=\"burst\"\n",
    );
    Spec::load(&p).expect("declared scope loads");
}
