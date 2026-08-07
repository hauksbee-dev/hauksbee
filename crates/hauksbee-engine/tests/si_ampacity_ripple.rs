//! Integration tests for the two engine-layer `--si` checks wired in by the
//! tooling-gap work: IPC-2221 trace ampacity, and input-cap ripple current.
//!
//! These exercise the full `--si` augmentation path (the extract-layer SI report
//! plus `append_ampacity` / `append_ripple`) on synthetic boards, asserting the
//! check FIRES on a genuinely undersized routed trace / over-ripple cap and
//! stays SILENT when the discipline says it must (poured rails, no cited
//! current, unknown rating). A corpus-gated sweep pins zero false positives on
//! the known-good famous boards.

use std::path::{Path, PathBuf};

use hauksbee_engine::checks::{ampacity, converter, ripple};
use hauksbee_extract::{ExtractedBoard, SiCheck, SiReport, SiSeverity};
use hauksbee_models::ModelLibrary;

/// Build a board + its raw text from inline kicad_pcb body.
fn board_and_text(body: &str) -> (ExtractedBoard, String) {
    let text =
        format!("(kicad_pcb (version 20240101) (generator pcbnew)\n  (net 0 \"\")\n{body}\n)");
    let board = ExtractedBoard::from_kicad_pcb(&text).expect("kicad_pcb parses");
    (board, text)
}

fn run_si(board: &ExtractedBoard, text: &str) -> SiReport {
    let lib = ModelLibrary::builtin();
    let mut report = board.si_checks(Some(text));
    ampacity::append_ampacity(board, &lib, Some(text), &mut report);
    ripple::append_ripple(board, &lib, &mut report);
    report
}

fn run_si_with_library(board: &ExtractedBoard, text: &str, lib: &ModelLibrary) -> SiReport {
    let mut report = board.si_checks(Some(text));
    ampacity::append_ampacity(board, lib, Some(text), &mut report);
    ripple::append_ripple(board, lib, &mut report);
    report
}

fn test_programmed_load_library() -> ModelLibrary {
    let models = tempfile::tempdir().expect("temporary model directory");
    std::fs::write(
        models.path().join("programmed-load.toml"),
        r#"
[[models]]
id = "test_programmed_load"
kind = "vreg"
description = "Test-only 10 A regulated load"

[models.match]
value_re = "^TEST_PROGRAMMED_LOAD$"

[models.params]
vout = 2.5
dropout_v = 0.1
iq_a = 0.001

[models.pins]
"1" = "in"
"2" = "ground"
"3" = "prog"
"4" = "out"

[models.current_program]
pin = "prog"
semantics = "regulated_current"
current_in_roles = ["in"]
current_out_roles = ["out"]
max_operating_current_a = 10.0
equation = "inverse_resistance"
k_volts = 10000.0
"#,
    )
    .expect("write test model");
    ModelLibrary::builtin_with_user_dirs(&[models.path()])
}

fn exact_rated_ripple_fixture(extra_input_caps: &str) -> (ExtractedBoard, String) {
    board_and_text(&format!(
        r#"
  (net 1 "VIN_5V")
  (net 2 "SW_NODE")
  (net 3 "VOUT_2V5")
  (net 4 "GND")
  (net 5 "PROG")
  (net 6 "LOAD_OUT")
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 0 0)
    (property "Reference" "Q1") (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VIN_5V"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE")))
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 5 0)
    (property "Reference" "Q2") (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
  (footprint "Inductor_SMD:L_12x12mm" (layer "F.Cu") (at 10 0)
    (property "Reference" "L1") (property "Value" "47uH")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT_2V5")))
  (footprint "Capacitor_SMD:CP_Elec_16x31.5" (layer "F.Cu") (at 0 5)
    (property "Reference" "C1") (property "Value" "1200uF")
    (property "MPN" "EKYB630ELL122MLN3S")
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 1 "VIN_5V"))
    (pad "2" smd rect (at 2 0) (size 2 2) (layers "F.Cu") (net 4 "GND")))
  {extra_input_caps}
  (footprint "Package_QFN:QFN-4" (layer "F.Cu") (at 20 0)
    (property "Reference" "U1") (property "Value" "TEST_PROGRAMMED_LOAD")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT_2V5"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND"))
    (pad "3" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 5 "PROG"))
    (pad "4" smd rect (at 3 0) (size 1 1) (layers "F.Cu") (net 6 "LOAD_OUT")))
  (footprint "Resistor_SMD:R_0603" (layer "F.Cu") (at 25 0)
    (property "Reference" "R1") (property "Value" "1k")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 5 "PROG"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
"#,
    ))
}

/// A TP4054 in its 400 mA constant-current operating phase, programmed by the
/// published 1.66 kOhm application value. Unlike a regulator rating or current
/// limit, this is an explicit operating-current assertion.
fn programmed_charger_and_text(copper: &str) -> (ExtractedBoard, String) {
    board_and_text(&format!(
        r#"
  (net 1 "VBAT")
  (net 2 "VIN")
  (net 3 "GND")
  (net 4 "PROG")
  (footprint "Package_TO_SOT_SMD:SOT-23-5" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1")
    (property "Value" "TP4054")
    (pad "1" smd rect (at 0 3) (size 1 1) (layers "F.Cu"))
    (pad "2" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 3 "GND"))
    (pad "3" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VBAT"))
    (pad "4" smd rect (at 0 -2) (size 1 1) (layers "F.Cu") (net 2 "VIN"))
    (pad "5" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 4 "PROG")))
  (footprint "Resistor_SMD:R_0603" (layer "F.Cu") (at 4 0)
    (property "Reference" "R1")
    (property "Value" "1.66k")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 4 "PROG"))
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 3 "GND")))
  {copper}
"#,
    ))
}

fn findings_of<'a>(report: &'a SiReport, check: SiCheck) -> Vec<&'a hauksbee_extract::SiFinding> {
    report
        .findings
        .iter()
        .filter(|f| f.check == check && f.severity.is_finding())
        .collect()
}

// ===========================================================================
// Trace ampacity (item 2).
// ===========================================================================

/// A TP4054 charger explicitly regulating 400 mA whose VBAT rail is routed as
/// a hair-thin discrete trace. `--si` must surface the finding from the fitted
/// resistor and datasheet equation—not from a capability/limit rating.
#[test]
fn si_surfaces_ampacity_on_undersized_routed_rail() {
    let (board, text) = programmed_charger_and_text(
        r#"(segment (start 0 0) (end 10 0) (width 0.05) (layer "F.Cu") (net 1))"#,
    );
    let report = run_si(&board, &text);
    let amp = findings_of(&report, SiCheck::TraceAmpacity);
    assert_eq!(
        amp.len(),
        1,
        "the undersized +3V3 rail must fire one ampacity finding: {:?}",
        report.findings
    );
    let f = &amp[0];
    assert!(f.nets.contains(&"VBAT".to_string()), "fires on VBAT");
    assert!(
        f.message.contains("TP4054") || f.message.contains("0.40 A"),
        "cites the regulated charge current: {}",
        f.message
    );
}

/// Two-sided IdentityUnknown contract on the full `--si` surface: the same
/// undersized rail with the charger's identity refused fires NO ampacity
/// finding (no current may be attributed from an unknown part), and an Info
/// note names the skipped part and the refusal, so the silence reads as a
/// coverage hole rather than a clean pass.
#[test]
fn si_skips_an_identity_unknown_part_and_says_so() {
    let (mut board, text) = programmed_charger_and_text(
        r#"(segment (start 0 0) (end 10 0) (width 0.05) (layer "F.Cu") (net 1))"#,
    );
    let u1 = board
        .components
        .iter_mut()
        .find(|c| c.reference == "U1")
        .expect("the charger exists");
    u1.properties.push((
        hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.to_string(),
        "two populated records with different values".to_string(),
    ));
    let report = run_si(&board, &text);
    assert!(
        findings_of(&report, SiCheck::TraceAmpacity).is_empty(),
        "a refused identity must attribute no current, so nothing fires: {:?}",
        report.findings
    );
    let note = report
        .findings
        .iter()
        .find(|f| f.check == SiCheck::TraceAmpacity && f.refs.contains(&"U1".to_string()))
        .expect("the skip must be said out loud on the SI report");
    assert_eq!(note.severity, SiSeverity::Info);
    assert!(
        note.message.contains("duplicate designator"),
        "the note must carry the refusal reason: {}",
        note.message
    );
}

/// R15: the SI chokepoint `engine_si` must carry the engine-layer ampacity +
/// ripple findings that the bare extractor `si_checks` structurally cannot
/// produce (it has no ModelLibrary to attribute currents). This is the call the
/// aggregate surfaces (`--check`, the combined `--json`, the TUI, the web front
/// door) now make, so they no longer print a false-clean SI section over an
/// under-width power trace that `--si` flags.
#[test]
fn engine_si_chokepoint_adds_ampacity_missing_from_bare_si_checks() {
    let (board, text) = programmed_charger_and_text(
        r#"(segment (start 0 0) (end 10 0) (width 0.05) (layer "F.Cu") (net 1))"#,
    );
    let lib = ModelLibrary::builtin();
    // The bare extractor call cannot attribute the regulator's current, so it has
    // no ampacity finding, exactly what every aggregate surface used to call.
    let bare = board.si_checks(Some(&text));
    assert!(
        findings_of(&bare, SiCheck::TraceAmpacity).is_empty(),
        "bare si_checks cannot attribute currents, so it carries no ampacity finding"
    );
    // The chokepoint adds it, so the aggregate surfaces now see the under-width rail.
    let via_chokepoint = hauksbee_engine::checks::engine_si(&board, &lib, Some(&text));
    assert_eq!(
        findings_of(&via_chokepoint, SiCheck::TraceAmpacity).len(),
        1,
        "engine_si must surface the ampacity finding: {:?}",
        via_chokepoint.findings
    );
}

/// The same programmed charger + rail, but the rail is poured with only a
/// thin pad-entry stub: the trace-current engine's Poured exemption must hold, so
/// `--si` stays silent on it even though the part cites 0.4 A.
#[test]
fn si_is_silent_on_poured_rail_with_thin_stub() {
    let (board, text) = programmed_charger_and_text(
        r#"
  (segment (start 0 0) (end 1 0) (width 0.05) (layer "F.Cu") (net 1))
  (zone (net 1) (net_name "VBAT") (layers "F.Cu")
    (filled_polygon (layer "F.Cu") (pts (xy 0 0) (xy 30 0) (xy 30 30) (xy 0 30))))
"#,
    );
    let report = run_si(&board, &text);
    let amp = findings_of(&report, SiCheck::TraceAmpacity);
    assert!(
        amp.is_empty(),
        "a poured rail must never fire: {:?}",
        report.findings
    );
}

/// A board with no explicit operating-current source: no current is attributed,
/// so the ampacity check fires nothing no matter how thin the trace. A component
/// capability or protection threshold would not change that conclusion.
#[test]
fn si_attributes_no_current_without_an_operating_source() {
    let (board, text) = board_and_text(
        r#"
  (net 1 "SIG")
  (segment (start 0 0) (end 10 0) (width 0.1) (layer "F.Cu") (net 1))
"#,
    );
    let report = run_si(&board, &text);
    assert!(findings_of(&report, SiCheck::TraceAmpacity).is_empty());
}

// ===========================================================================
// Input-cap ripple (item 3).
// ===========================================================================

/// A discrete buck stage whose input rail current is attributable (an AMS1117
/// LDO standing in as a rated 1.0 A load-class part on the output rail) and whose
/// input bulk cap defaults to a low ripple rating: assert the converter topology
/// is recovered. The ripple FIRE path is unit-tested by the hand-checked mppt
/// case; here we check topology recovery + the honest info note on a real-ish
/// netlist.
#[test]
fn converter_topology_recovered_from_discrete_buck() {
    let (board, _text) = board_and_text(
        r#"
  (net 1 "VIN")
  (net 2 "SW_NODE")
  (net 3 "VOUT")
  (net 4 "GND")
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 0 0)
    (property "Reference" "Q1")
    (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VIN"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE")))
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 5 0)
    (property "Reference" "Q2")
    (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
  (footprint "Inductor_SMD:L_12x12mm" (layer "F.Cu") (at 10 0)
    (property "Reference" "L1")
    (property "Value" "47uH")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT")))
  (footprint "Capacitor_SMD:CP_Elec_10x10" (layer "F.Cu") (at 0 5)
    (property "Reference" "C1")
    (property "Value" "1200uF")
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 1 "VIN"))
    (pad "2" smd rect (at 2 0) (size 2 2) (layers "F.Cu") (net 4 "GND")))
"#,
    );
    let lib = ModelLibrary::builtin();
    let stages = converter::detect_converters(&board, &lib);
    assert_eq!(stages.len(), 1, "one buck stage recovered: {stages:?}");
    let s = &stages[0];
    assert_eq!(s.switch_node.1, "SW_NODE");
    assert_eq!(s.input_rail.1, "VIN");
    assert_eq!(s.output_rail.1, "VOUT");
    assert_eq!(s.inductor_ref, "L1");
    let [cap] = s.input_bulk_caps.as_slice() else {
        panic!("exactly one input bulk cap C1 expected: {s:?}");
    };
    assert_eq!(cap.reference, "C1");
}

/// End-to-end contract for the shared operating-current attribution. The
/// AMS1117's 1 A capability must contribute nothing, while the TP4054's fitted
/// programming resistor establishes 0.4 A on VOUT. C1 has no exact fitted-part
/// rating, so the check records the attributable current in an *info* note and
/// refuses to invent one from capacitance class. The genuine
/// over-ripple arithmetic is locked by the hand-checked mppt-1210 C1 unit test
/// (`ripple::tests::mppt_1210_c1_overstress_is_1_66x`).
#[test]
fn si_ripple_uses_programmed_load_but_not_capability_or_invented_rating() {
    let (board, text) = board_and_text(
        r#"
  (net 1 "VIN")
  (net 2 "SW_NODE")
  (net 3 "VOUT")
  (net 4 "GND")
  (net 5 "VBAT")
  (net 6 "PROG")
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 0 0)
    (property "Reference" "Q1")
    (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VIN"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE")))
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 5 0)
    (property "Reference" "Q2")
    (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
  (footprint "Inductor_SMD:L_12x12mm" (layer "F.Cu") (at 10 0)
    (property "Reference" "L1")
    (property "Value" "47uH")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT")))
  (footprint "Capacitor_SMD:CP_Elec_10x10" (layer "F.Cu") (at 0 5)
    (property "Reference" "C1")
    (property "Value" "120uF")
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 1 "VIN"))
    (pad "2" smd rect (at 2 0) (size 2 2) (layers "F.Cu") (net 4 "GND")))
  (footprint "Package_TO_SOT_SMD:SOT-223-3_TabPin2" (layer "F.Cu") (at 15 0)
    (property "Reference" "U2")
    (property "Value" "AMS1117-3.3")
    (pad "1" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 4 "GND"))
    (pad "2" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT"))
    (pad "3" smd rect (at 0 -2) (size 1 1) (layers "F.Cu") (net 1 "VIN")))
  (footprint "Package_TO_SOT_SMD:SOT-23-5" (layer "F.Cu") (at 20 0)
    (property "Reference" "U3")
    (property "Value" "TP4054")
    (pad "1" smd rect (at 0 3) (size 1 1) (layers "F.Cu"))
    (pad "2" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 4 "GND"))
    (pad "3" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 5 "VBAT"))
    (pad "4" smd rect (at 0 -2) (size 1 1) (layers "F.Cu") (net 3 "VOUT"))
    (pad "5" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 6 "PROG")))
  (footprint "Resistor_SMD:R_0603" (layer "F.Cu") (at 24 0)
    (property "Reference" "R1")
    (property "Value" "1.66k")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 6 "PROG"))
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
"#,
    );
    let report = run_si(&board, &text);
    // The AMS1117's 1.0 A rating is capability, not output draw. U3 contributes
    // only its programmed 0.4 A, and C1 has no decision-grade fitted-part rating.
    let fires = findings_of(&report, SiCheck::InputCapRipple);
    assert!(
        fires.is_empty(),
        "a missing exact rating must never support a finding: {:?}",
        report.findings
    );
    // The topology + citeable operating current must still be on the record.
    let info = report
        .findings
        .iter()
        .find(|f| f.check == SiCheck::InputCapRipple && f.severity == SiSeverity::Info)
        .expect("the resolved buck must leave an input-cap ripple info note");
    assert!(
        info.message.contains("I_out 0.40 A")
            && info.message.to_ascii_lowercase().contains("tp4054")
            && info
                .message
                .contains("has no part-specific datasheet ripple rating"),
        "the note must cite only the programmed 0.4 A load, not the AMS1117 rating: {}",
        info.message
    );
}

/// Production-path proof: a shipped exact-MPN capacitor rating, an explicitly
/// programmed operating load, structurally recovered buck topology, and named
/// nominal rail voltages must combine into one decision-grade finding. This is
/// intentionally end-to-end rather than another arithmetic-only unit test.
#[test]
fn si_ripple_fires_from_shipped_exact_cap_rating_and_named_nominal_duty() {
    let lib = test_programmed_load_library();
    let (board, text) = board_and_text(
        r#"
  (net 1 "VIN_5V")
  (net 2 "SW_NODE")
  (net 3 "VOUT_2V5")
  (net 4 "GND")
  (net 5 "PROG")
  (net 6 "LOAD_OUT")
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 0 0)
    (property "Reference" "Q1")
    (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VIN_5V"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE")))
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 5 0)
    (property "Reference" "Q2")
    (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
  (footprint "Inductor_SMD:L_12x12mm" (layer "F.Cu") (at 10 0)
    (property "Reference" "L1")
    (property "Value" "47uH")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT_2V5")))
  (footprint "Capacitor_SMD:CP_Elec_16x31.5" (layer "F.Cu") (at 0 5)
    (property "Reference" "C1")
    (property "Value" "1200uF")
    (property "MPN" "EKYB630ELL122MLN3S")
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 1 "VIN_5V"))
    (pad "2" smd rect (at 2 0) (size 2 2) (layers "F.Cu") (net 4 "GND")))
  (footprint "Package_QFN:QFN-4" (layer "F.Cu") (at 20 0)
    (property "Reference" "U1")
    (property "Value" "TEST_PROGRAMMED_LOAD")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT_2V5"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND"))
    (pad "3" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 5 "PROG"))
    (pad "4" smd rect (at 3 0) (size 1 1) (layers "F.Cu") (net 6 "LOAD_OUT")))
  (footprint "Resistor_SMD:R_0603" (layer "F.Cu") (at 25 0)
    (property "Reference" "R1")
    (property "Value" "1k")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 5 "PROG"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
"#,
    );

    let report = run_si_with_library(&board, &text, &lib);
    let findings = findings_of(&report, SiCheck::InputCapRipple);
    assert_eq!(
        findings.len(),
        1,
        "expected exact-rated ripple finding: {:?}",
        report.findings
    );
    let finding = findings[0];
    assert!(finding.message.contains("C1"));
    assert!(finding.message.contains("3.00 A_rms"));
    assert!(finding.message.contains("D=0.500"));
    assert!(finding.message.contains("VIN_5V") && finding.message.contains("VOUT_2V5"));
    assert!(finding.message.contains("~1.67x"));
}

/// A precise capacitor MPN and load current are still insufficient without an
/// operating duty/range. The old unconditional D=0.5 path could false-flag a
/// 48 V to 3.3 V converter; absence of named voltage evidence must now abstain.
#[test]
fn si_ripple_does_not_assume_half_duty_when_rail_voltages_are_unknown() {
    let lib = test_programmed_load_library();
    let (board, text) = board_and_text(
        r#"
  (net 1 "VIN")
  (net 2 "SW_NODE")
  (net 3 "VOUT")
  (net 4 "GND")
  (net 5 "PROG")
  (net 6 "LOAD_OUT")
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 0 0)
    (property "Reference" "Q1") (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "VIN"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE")))
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 5 0)
    (property "Reference" "Q2") (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
  (footprint "Inductor_SMD:L_12x12mm" (layer "F.Cu") (at 10 0)
    (property "Reference" "L1") (property "Value" "47uH")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 2 "SW_NODE"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT")))
  (footprint "Capacitor_SMD:CP_Elec_16x31.5" (layer "F.Cu") (at 0 5)
    (property "Reference" "C1") (property "Value" "1200uF")
    (property "MPN" "EKYB630ELL122MLN3S")
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 1 "VIN"))
    (pad "2" smd rect (at 2 0) (size 2 2) (layers "F.Cu") (net 4 "GND")))
  (footprint "Package_QFN:QFN-4" (layer "F.Cu") (at 20 0)
    (property "Reference" "U1") (property "Value" "TEST_PROGRAMMED_LOAD")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND"))
    (pad "3" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 5 "PROG"))
    (pad "4" smd rect (at 3 0) (size 1 1) (layers "F.Cu") (net 6 "LOAD_OUT")))
  (footprint "Resistor_SMD:R_0603" (layer "F.Cu") (at 25 0)
    (property "Reference" "R1") (property "Value" "1k")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 5 "PROG"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
"#,
    );
    let report = run_si_with_library(&board, &text, &lib);
    assert!(findings_of(&report, SiCheck::InputCapRipple).is_empty());
    let note = report
        .findings
        .iter()
        .find(|finding| finding.check == SiCheck::InputCapRipple)
        .expect("the evidence-complete stage records why it abstained");
    assert!(note.message.contains("duty") && note.message.contains("unknown"));
    assert!(note.message.contains("10.00 A") && note.message.contains("3.00 A_rms"));
}

/// Parallel input capacitors divide ripple according to their frequency-
/// dependent impedance, not nominal capacitance alone. Charging the full stage
/// ripple to whichever capacitor happens to sort first is a false positive.
#[test]
fn si_ripple_abstains_when_parallel_input_caps_make_current_sharing_unknown() {
    let lib = test_programmed_load_library();
    let (board, text) = exact_rated_ripple_fixture(
        r#"
  (footprint "Capacitor_SMD:CP_Elec_10x10" (layer "F.Cu") (at 4 5)
    (property "Reference" "C2") (property "Value" "470uF")
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 1 "VIN_5V"))
    (pad "2" smd rect (at 2 0) (size 2 2) (layers "F.Cu") (net 4 "GND")))
"#,
    );

    let report = run_si_with_library(&board, &text, &lib);
    assert!(findings_of(&report, SiCheck::InputCapRipple).is_empty());
    let note = report
        .findings
        .iter()
        .find(|finding| finding.check == SiCheck::InputCapRipple)
        .expect("parallel bank must leave an explicit abstention");
    assert!(note.message.contains("2 parallel input bulk capacitors"));
    assert!(note.message.contains("sharing") && note.message.contains("unknown"));
}

#[test]
fn si_ripple_abstains_when_two_stages_supply_the_same_output_load() {
    let lib = test_programmed_load_library();
    let (board, text) = exact_rated_ripple_fixture(
        r#"
  (net 7 "VIN2_5V")
  (net 8 "SW_NODE_2")
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 30 0)
    (property "Reference" "Q3") (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 7 "VIN2_5V"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 8 "SW_NODE_2")))
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 35 0)
    (property "Reference" "Q4") (property "Value" "PSMN5R2-60YLX")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 8 "SW_NODE_2"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 4 "GND")))
  (footprint "Inductor_SMD:L_12x12mm" (layer "F.Cu") (at 40 0)
    (property "Reference" "L2") (property "Value" "47uH")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 8 "SW_NODE_2"))
    (pad "2" smd rect (at 1 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT_2V5")))
  (footprint "Capacitor_SMD:CP_Elec_16x31.5" (layer "F.Cu") (at 30 5)
    (property "Reference" "C2") (property "Value" "1200uF")
    (property "MPN" "EKYB630ELL122MLN3S")
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 7 "VIN2_5V"))
    (pad "2" smd rect (at 2 0) (size 2 2) (layers "F.Cu") (net 4 "GND")))
"#,
    );

    let report = run_si_with_library(&board, &text, &lib);
    assert!(
        findings_of(&report, SiCheck::InputCapRipple).is_empty(),
        "a net-wide load cannot be charged in full to both suppliers: {:?}",
        report.findings
    );
    let notes: Vec<_> = report
        .findings
        .iter()
        .filter(|finding| {
            finding.check == SiCheck::InputCapRipple
                && !finding.severity.is_finding()
                && finding.message.contains("supplier split")
        })
        .collect();
    assert_eq!(notes.len(), 1, "emit one stage-group abstention: {notes:?}");
    assert!(notes[0].message.contains("L1") && notes[0].message.contains("L2"));
}

// ===========================================================================
// Corpus sweep: zero false positives on the known-good famous boards.
// ===========================================================================

/// The directory the corpus boards sit directly under, whichever layout this
/// machine has, or `None` with a printed note.
///
/// Resolved through the testkit rather than by joining `famous/` here: that
/// level exists only in the hand-built corpus, so a hard join found nothing on
/// the corpus `scripts/fetch-corpus.sh` produces, and every sweep below walked
/// an empty directory and reported the empty walk as a pass.
fn corpus_root() -> Option<PathBuf> {
    hauksbee_testkit::corpus_boards_root_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "SI ampacity/ripple corpus sweep",
    )
}

/// The ampacity and ripple checks must raise zero *findings* (info notes are
/// fine) across the known-good famous corpus: a check that cannot discriminate
/// does not ship. This is the same discipline the other SI checks hold.
#[test]
fn famous_corpus_has_no_ampacity_or_ripple_findings() {
    let Some(root) = corpus_root() else { return };
    let lib = ModelLibrary::builtin();
    let mut boards_checked = 0usize;
    let mut not_known_good = 0usize;
    for entry in walk_kicad_pcbs(&root) {
        // This gate's claim is "the checks stay quiet on hardware that is fine",
        // so a board the corpus does not vouch for cannot carry it. The manifest
        // records which entries were never manufactured, or whose findings are
        // unadjudicated, with a reason per entry; excluding them is announced per
        // board rather than silently narrowing the input set. Without this the
        // sweep graded itself on a KiCad demo board that no one ever built.
        if let Some(why) = hauksbee_testkit::not_known_good(&entry, &root) {
            hauksbee_testkit::excluded(
                "SI ampacity/ripple corpus sweep",
                &entry.file_name().unwrap_or_default().to_string_lossy(),
                &why,
            );
            not_known_good += 1;
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let Ok(board) = ExtractedBoard::from_kicad_pcb(&text) else {
            continue;
        };
        boards_checked += 1;
        let mut report = SiReport::default();
        ampacity::append_ampacity(&board, &lib, Some(&text), &mut report);
        ripple::append_ripple(&board, &lib, &mut report);
        let amp = report
            .findings
            .iter()
            .filter(|f| f.check == SiCheck::TraceAmpacity && f.severity.is_finding())
            .count();
        let rip = report
            .findings
            .iter()
            .filter(|f| f.check == SiCheck::InputCapRipple && f.severity.is_finding())
            .count();
        assert_eq!(
            amp + rip,
            0,
            "{}: ampacity findings {amp}, ripple findings {rip} - the known-good corpus must stay clean. {:?}",
            entry.display(),
            report.findings.iter().filter(|f| f.severity.is_finding()).collect::<Vec<_>>(),
        );
    }
    eprintln!(
        "SCANNED  SI ampacity/ripple corpus sweep: {boards_checked} board(s), \
         {not_known_good} excluded as not known-good"
    );
    if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        assert!(
            boards_checked > 0,
            "corpus required but no boards were checked"
        );
    }
}

/// Recursively collect `.kicad_pcb` files under a root (bounded depth, skips the
/// huge history dirs to keep the sweep quick).
fn walk_kicad_pcbs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == ".git" || name.contains("history") {
                    continue;
                }
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("kicad_pcb") {
                out.push(p);
            }
        }
    }
    out
}
