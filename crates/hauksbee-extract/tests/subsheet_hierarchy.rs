//! A hierarchical sub-sheet is not a design, and asking connectivity questions
//! of one alone produces confident nonsense.
//!
//! A child sheet's `(hierarchical_label "X")` is wired to a `(sheet (pin "X"))`
//! in its parent. Read the child file on its own and that net touches exactly one
//! pin, which is the same shape as a genuinely floating stub. `net_lint`'s
//! floating-control-pin check duly raised six [high] findings across four MNT
//! Reform sub-sheets, one of them on `USB_PWR_EN`, a net driven from
//! `reform2-lpc.kicad_sch` in the same project. The top-level
//! `reform2-motherboard30.kicad_sch` was clean the whole time.
//!
//! The fix is in the loader, not in the check: `from_kicad_schematic_path`
//! resolves the hierarchy a sub-sheet belongs to and extracts from its root, and
//! refuses with a message naming what it needs when no parent exists. These tests
//! pin the false-positive case, the clean top-level case, and the refusal.

use std::path::{Path, PathBuf};

use hauksbee_extract::{ExtractedBoard, LintCheck, Severity};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn floating_control_highs(board: &ExtractedBoard) -> Vec<String> {
    board
        .net_lint()
        .of_check(LintCheck::FloatingControlPin)
        .filter(|f| f.severity == Severity::High)
        .map(|f| f.message.clone())
        .collect()
}

/// The bug, reproduced against the single-sheet reader: the child's `USB_PWR_EN`
/// net touches only U1's EN pin, so the check calls it floating. This is the
/// behaviour the loader now keeps callers away from, and it is asserted here so
/// the fix cannot be mistaken for the check having gone quiet.
#[test]
fn a_sub_sheet_read_alone_looks_like_a_floating_control_pin() {
    let text = std::fs::read_to_string(fixture("subsheet_child.kicad_sch")).expect("read fixture");
    let board = ExtractedBoard::from_kicad_schematic(&text).expect("extract single sheet");
    let highs = floating_control_highs(&board);
    assert_eq!(
        highs.len(),
        1,
        "the single-sheet read is expected to mis-see the sheet pin as floating; \
         got {highs:#?}"
    );
    assert!(
        highs[0].contains("USB_PWR_EN"),
        "the mis-seen net is the one wired to the parent's sheet pin: {}",
        highs[0]
    );
}

/// The fix: handed the same child file *by path*, the loader finds the parent
/// that owns it and extracts the whole hierarchy, where `USB_PWR_EN` is pulled
/// up by R1. No finding.
#[test]
fn a_sub_sheet_by_path_pulls_in_its_parent_and_is_clean() {
    let board = ExtractedBoard::from_kicad_schematic_path(&fixture("subsheet_child.kicad_sch"))
        .expect("the child resolves through its parent");
    // The parent's pull-up is present, which is how we know the hierarchy came in
    // and not just that the check stopped firing.
    assert!(
        board.components.iter().any(|c| c.reference == "R1"),
        "the parent's R1 pull-up must be in the extraction, got {:?}",
        board
            .components
            .iter()
            .map(|c| &c.reference)
            .collect::<Vec<_>>()
    );
    assert!(
        board.components.iter().any(|c| c.reference == "U1"),
        "the child's U1 must still be in the extraction"
    );
    let highs = floating_control_highs(&board);
    assert!(
        highs.is_empty(),
        "USB_PWR_EN is driven from the parent sheet; nothing floats: {highs:#?}"
    );
}

/// The top-level sheet of the same hierarchy: clean, and clean for the same
/// reason. Loading the root was never the broken case, and this pins that the
/// sub-sheet path now agrees with it.
#[test]
fn the_top_level_sheet_is_clean() {
    let board = ExtractedBoard::from_kicad_schematic_path(&fixture("subsheet_parent.kicad_sch"))
        .expect("extract the root");
    let highs = floating_control_highs(&board);
    assert!(highs.is_empty(), "the root sheet is clean: {highs:#?}");
}

/// A sub-sheet with no parent anywhere: refused, with the refusal naming both
/// what the file is and what would be needed to answer. Returning a netlist here
/// would be returning a guess dressed as a measurement.
#[test]
fn an_orphan_sub_sheet_is_refused_with_a_message_naming_what_it_needs() {
    let err = ExtractedBoard::from_kicad_schematic_path(&fixture("subsheet_orphan.kicad_sch"))
        .expect_err("an orphan sub-sheet cannot be extracted honestly");
    let msg = err.to_string();
    assert!(
        msg.contains("sub-sheet"),
        "the message must say what the file is: {msg}"
    );
    assert!(
        msg.contains("root schematic"),
        "the message must name what it needs: {msg}"
    );
    assert!(
        msg.contains("subsheet_orphan"),
        "the message must name the file: {msg}"
    );
}

// ---------------------------------------------------------------------------
// The corpus case this was found on. Corpus-gated; a scan of zero fails.
// ---------------------------------------------------------------------------

/// Every MNT Reform motherboard sub-sheet, plus the top-level sheet: no
/// floating-control-pin [high] anywhere. Six of these fired before the loader
/// resolved the hierarchy.
#[test]
fn mnt_reform_sub_sheets_raise_no_floating_control_pin() {
    let Some(root) = hauksbee_testkit::corpus_boards_root_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "MNT Reform sub-sheet hierarchy guard",
    ) else {
        return;
    };
    let dir = root.join("mnt_reform");
    if !dir.is_dir() {
        assert!(
            !hauksbee_testkit::require_assets(),
            "HAUKSBEE_REQUIRE_CORPUS=1 but {} is absent",
            dir.display()
        );
        eprintln!("NOT RUN  MNT Reform absent");
        return;
    }
    // Every schematic in the laptop, across all 19 boards: the motherboard
    // revisions and the keyboard revisions each have their own hierarchy, and
    // both contributed to the original findings.
    let mut sheets: Vec<PathBuf> = Vec::new();
    let mut stack = vec![dir];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("kicad_sch") {
                sheets.push(p);
            }
        }
    }
    sheets.sort();

    let mut offenders: Vec<String> = Vec::new();
    let mut checked = 0usize;
    // What the single-sheet read makes of the same files. Counted so this guard
    // states the size of the bug it protects against rather than asserting the
    // absence of something that might never have been there: six [high] findings
    // across four sub-sheets, on this project, at this pinned revision.
    let mut single_sheet_highs = 0usize;
    for sheet in &sheets {
        if let Ok(text) = std::fs::read_to_string(sheet) {
            if let Ok(alone) = ExtractedBoard::from_kicad_schematic(&text) {
                single_sheet_highs += floating_control_highs(&alone).len();
            }
        }
        let board = match ExtractedBoard::from_kicad_schematic_path(sheet) {
            Ok(b) => b,
            // An orphan sub-sheet is a refusal, not a finding: the loader said it
            // cannot answer, which is the honest outcome and not a false positive.
            Err(e) => {
                eprintln!("REFUSED  {}: {e}", sheet.display());
                continue;
            }
        };
        checked += 1;
        for m in floating_control_highs(&board) {
            offenders.push(format!("{}: {m}", sheet.display()));
        }
    }
    hauksbee_testkit::scanned("MNT Reform sub-sheet hierarchy guard", checked);
    eprintln!(
        "single-sheet reads of the same {} file(s) produce {single_sheet_highs} \
         floating-control-pin [high] finding(s)",
        sheets.len()
    );
    assert!(
        single_sheet_highs > 0,
        "the sub-sheet mis-read is what this guard exists for; if the single-sheet \
         read is now clean too, either the project changed or the check stopped \
         firing, and this test is no longer proving anything"
    );
    assert!(
        offenders.is_empty(),
        "no MNT Reform sheet, top-level or sub, has a floating control pin; \
         a finding here is the sibling-sheet driver being missed:\n  {}",
        offenders.join("\n  ")
    );
}
