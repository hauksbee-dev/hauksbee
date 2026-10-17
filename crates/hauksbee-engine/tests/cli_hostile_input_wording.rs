//! What the user is told about inputs that are not a full board (adversarial
//! hostile-input findings W1, W2, W7): a schematic is not "OK TO ORDER", a
//! board with no nets is refused in plain words and not with an internal spec
//! key, and fabrication files are not told to "route" their copper.

use crate::support::{board, run_board, stderr, stdout};

/// A passing two-resistor KiCad schematic: no board layout behind it.
fn schematic() -> std::path::PathBuf {
    board("../hauksbee-extract/tests/fixtures/two_resistors.kicad_sch")
}

/// A gerber folder with a part list (`.pos`) and copper: a fab package.
fn fab_package() -> std::path::PathBuf {
    board("../../testdata/ingest-robustness/gerber_kicad_pos")
}

#[test]
fn w1_a_schematic_that_passes_is_checked_not_orderable() {
    let out = run_board(&schematic(), &["--plain"]);
    let so = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(so.contains("CHECKED, NOT ORDERABLE"), "headline: {so}");
    assert!(
        !so.contains("OK TO ORDER"),
        "a schematic is not OK to order: {so}"
    );
    let verdict = so
        .lines()
        .find(|line| line.starts_with("VERDICT:"))
        .unwrap_or_else(|| panic!("a VERDICT line: {so}"));
    assert!(
        verdict.starts_with("VERDICT: checked, not orderable"),
        "{verdict}"
    );
    assert!(!verdict.contains("clean"), "{verdict}");
    // It says what was checked and what is needed to judge ordering.
    for phrase in ["schematic", "board layout", ".kicad_pcb"] {
        assert!(verdict.contains(phrase), "{phrase:?} missing: {verdict}");
    }
}

#[test]
fn w1_a_board_layout_still_reads_ok_to_order() {
    // The control: the committed layout passes as before.
    let blinky = board("../hauksbee-ci/examples/boards/blinky.kicad_pcb");
    let so = stdout(&run_board(&blinky, &["--plain"]));
    assert!(!so.contains("CHECKED, NOT ORDERABLE"), "{so}");
}

#[test]
fn w2_a_board_with_parts_and_no_nets_is_refused_in_plain_words() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("a.kicad_pcb");
    std::fs::write(
        &path,
        r#"(kicad_pcb (version 20171130)
 (net 0 "")
 (module R_0805 (layer F.Cu) (at 0 0)
  (fp_text reference R1 (at 0 0) (layer F.SilkS))
  (fp_text value 10k (at 0 2) (layer F.Fab))
  (pad 1 smd rect (at -1 0) (net 0 ""))
  (pad 2 smd rect (at 1 0) (net 0 ""))
 ))
"#,
    )
    .expect("write board");
    for args in [&["--plain"][..], &["--json"][..]] {
        let out = run_board(&path, args);
        let all = format!("{}{}", stdout(&out), stderr(&out));
        assert!(!out.status.success(), "no verdict can be given: {all}");
        assert!(
            !all.contains("net_scope") && !all.contains("must not be empty"),
            "an internal spec key leaked: {all}"
        );
        assert!(all.contains("no connections to check"), "{all}");
        assert!(all.contains("netlist"), "says what to give instead: {all}");
    }
}

#[test]
fn w7_fabrication_files_are_not_told_to_route_their_copper() {
    let out = run_board(&fab_package(), &["--plain"]);
    let so = stdout(&out);
    assert!(so.contains("Copper spacing was not checked"), "{so}");
    assert!(
        !so.contains("No copper was checked"),
        "fab files do contain traces: {so}"
    );
    assert!(
        !so.contains("may have no routed copper yet: route it"),
        "routing is not the fix for fabrication files: {so}"
    );
    assert!(
        so.contains("CLEARANCE RULE SOURCE: none"),
        "no rule was read, so no 0.000 mm rule is claimed: {so}"
    );
    assert!(!so.contains("0.000 mm"), "{so}");
}
