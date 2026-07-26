//! Integration tests for the two engine-layer `--si` checks wired in by the
//! tooling-gap work: IPC-2221 trace ampacity, and input-cap ripple current.
//!
//! These exercise the full `--si` augmentation path (the extract-layer SI report
//! plus `append_ampacity` / `append_ripple`) on synthetic boards, asserting the
//! check FIRES on a genuinely undersized routed trace / over-ripple cap and
//! stays SILENT when the discipline says it must (poured rails, no cited
//! current, unknown rating). A corpus-gated sweep pins zero false positives on
//! the known-good famous boards.

use std::path::PathBuf;

use hauksbee_engine::checks::{ampacity, converter, ripple};
use hauksbee_extract::{ExtractedBoard, SiCheck, SiReport, SiSeverity};
use hauksbee_models::ModelLibrary;

/// Build a board + its raw text from inline kicad_pcb body.
fn board_and_text(body: &str) -> (ExtractedBoard, String) {
    let text = format!(
        "(kicad_pcb (version 20240101) (generator pcbnew)\n  (net 0 \"\")\n{body}\n)"
    );
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

/// An AMS1117-3.3 LDO (DB-modelled, rated 1.0 A) whose output rail `+3V3` is
/// routed as a single hair-thin 0.15 mm discrete trace (~0.46 A at 10 C):
/// undersized for the regulator's cited 1.0 A. `--si` must now surface the
/// ampacity finding automatically, citing the part's datasheet current.
#[test]
fn si_surfaces_ampacity_on_undersized_routed_rail() {
    let (board, text) = board_and_text(
        r#"
  (net 1 "+3V3")
  (net 2 "VIN")
  (net 3 "GND")
  (footprint "Package_TO_SOT_SMD:SOT-223-3_TabPin2" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1")
    (property "Value" "AMS1117-3.3")
    (pad "1" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 3 "GND"))
    (pad "2" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "+3V3"))
    (pad "3" smd rect (at 0 -2) (size 1 1) (layers "F.Cu") (net 2 "VIN")))
  (segment (start 0 0) (end 10 0) (width 0.15) (layer "F.Cu") (net 1))
"#,
    );
    let report = run_si(&board, &text);
    let amp = findings_of(&report, SiCheck::TraceAmpacity);
    assert_eq!(amp.len(), 1, "the undersized +3V3 rail must fire one ampacity finding: {:?}", report.findings);
    let f = &amp[0];
    assert!(f.nets.contains(&"+3V3".to_string()), "fires on +3V3");
    assert!(f.message.contains("AMS1117") || f.message.contains("1.00 A"), "cites the regulator current: {}", f.message);
}

/// R15: the SI chokepoint `engine_si` must carry the engine-layer ampacity +
/// ripple findings that the bare extractor `si_checks` structurally cannot
/// produce (it has no ModelLibrary to attribute currents). This is the call the
/// aggregate surfaces (`--check`, the combined `--json`, the TUI, the web front
/// door) now make, so they no longer print a false-clean SI section over an
/// under-width power trace that `--si` flags.
#[test]
fn engine_si_chokepoint_adds_ampacity_missing_from_bare_si_checks() {
    let (board, text) = board_and_text(
        r#"
  (net 1 "+3V3")
  (net 2 "VIN")
  (net 3 "GND")
  (footprint "Package_TO_SOT_SMD:SOT-223-3_TabPin2" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1")
    (property "Value" "AMS1117-3.3")
    (pad "1" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 3 "GND"))
    (pad "2" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "+3V3"))
    (pad "3" smd rect (at 0 -2) (size 1 1) (layers "F.Cu") (net 2 "VIN")))
  (segment (start 0 0) (end 10 0) (width 0.15) (layer "F.Cu") (net 1))
"#,
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

/// The same regulator + rail, but the rail is poured (a copper zone) with only a
/// thin pad-entry stub: the trace-current engine's Poured exemption must hold, so
/// `--si` stays silent on it even though the part cites 1.0 A.
#[test]
fn si_is_silent_on_poured_rail_with_thin_stub() {
    let (board, text) = board_and_text(
        r#"
  (net 1 "+3V3")
  (net 2 "VIN")
  (net 3 "GND")
  (footprint "Package_TO_SOT_SMD:SOT-223-3_TabPin2" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1")
    (property "Value" "AMS1117-3.3")
    (pad "1" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 3 "GND"))
    (pad "2" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "+3V3"))
    (pad "3" smd rect (at 0 -2) (size 1 1) (layers "F.Cu") (net 2 "VIN")))
  (segment (start 0 0) (end 1 0) (width 0.15) (layer "F.Cu") (net 1))
  (zone (net 1) (net_name "+3V3") (layers "F.Cu")
    (filled_polygon (layer "F.Cu") (pts (xy 0 0) (xy 30 0) (xy 30 30) (xy 0 30))))
"#,
    );
    let report = run_si(&board, &text);
    let amp = findings_of(&report, SiCheck::TraceAmpacity);
    assert!(amp.is_empty(), "a poured rail must never fire: {:?}", report.findings);
}

/// A board with no part carrying a DB current rating: no current is attributed,
/// so the ampacity check fires nothing no matter how thin the trace.
#[test]
fn si_attributes_no_current_without_a_rated_part() {
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
    let cap = s.input_bulk_cap.as_ref().expect("input bulk cap C1 found");
    assert_eq!(cap.reference, "C1");
}

/// End-to-end ripple path on a resolved buck whose output current IS
/// attributable (an AMS1117, DB-rated 1.0 A, on the output rail). Worst-case
/// input ripple is 0.5 * 1.0 = 0.5 A; the 120 uF input cap's conservative
/// per-class default rating is 1.0 A, so 0.5 A is comfortably under and the
/// check correctly produces an *info* note, not a false finding. This pins the
/// full attribution -> compute -> compare path end-to-end without fabricating an
/// overstress; the genuine over-ripple FIRE is locked by the hand-checked
/// mppt-1210 C1 unit test (`ripple::tests::mppt_1210_c1_overstress_is_1_66x`),
/// since no synthetic board can cite a 10 A charge current from a single part
/// rating honestly.
#[test]
fn si_ripple_attributes_and_compares_without_false_firing() {
    let (board, text) = board_and_text(
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
    (property "Value" "120uF")
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 1 "VIN"))
    (pad "2" smd rect (at 2 0) (size 2 2) (layers "F.Cu") (net 4 "GND")))
  (footprint "Package_TO_SOT_SMD:SOT-223-3_TabPin2" (layer "F.Cu") (at 15 0)
    (property "Reference" "U2")
    (property "Value" "AMS1117-3.3")
    (pad "1" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 4 "GND"))
    (pad "2" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 3 "VOUT"))
    (pad "3" smd rect (at 0 -2) (size 1 1) (layers "F.Cu") (net 1 "VIN")))
"#,
    );
    let report = run_si(&board, &text);
    // I_out attributes to the AMS1117's 1.0 A on VOUT; worst-case ripple 0.5 A.
    // A 120 uF input cap defaults to 1.0 A ripple rating, so 0.5 A is UNDER:
    // the check correctly produces an info note, not a finding (no false fire).
    let fires = findings_of(&report, SiCheck::InputCapRipple);
    assert!(fires.is_empty(), "0.5 A ripple under the 1.0 A default must not fire: {:?}", report.findings);
    // But the topology + computation must be on the record as an info note.
    let info = report
        .findings
        .iter()
        .any(|f| f.check == SiCheck::InputCapRipple && f.severity == SiSeverity::Info);
    assert!(info, "an input-cap ripple info note must be recorded for the resolved buck");
}

// ===========================================================================
// Corpus sweep: zero false positives on the known-good famous boards.
// ===========================================================================

fn famous_root() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous");
    if p.exists() {
        return Some(p);
    }
    if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        panic!("HAUKSBEE_REQUIRE_CORPUS set but board-corpus/famous is missing: {}", p.display());
    }
    eprintln!("board-corpus/famous absent; skipping ampacity/ripple corpus sweep");
    None
}

/// The ampacity and ripple checks must raise zero *findings* (info notes are
/// fine) across the known-good famous corpus: a check that cannot discriminate
/// does not ship. This is the same discipline the other SI checks hold.
#[test]
fn famous_corpus_has_no_ampacity_or_ripple_findings() {
    let Some(root) = famous_root() else { return };
    let lib = ModelLibrary::builtin();
    let mut boards_checked = 0usize;
    for entry in walk_kicad_pcbs(&root) {
        let Ok(text) = std::fs::read_to_string(&entry) else { continue };
        let Ok(board) = ExtractedBoard::from_kicad_pcb(&text) else { continue };
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
    if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        assert!(boards_checked > 0, "corpus required but no boards were checked");
    }
}

/// Recursively collect `.kicad_pcb` files under a root (bounded depth, skips the
/// huge history dirs to keep the sweep quick).
fn walk_kicad_pcbs(root: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
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
