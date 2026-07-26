//! Corpus sweep: run geometric DRC across every `.kicad_pcb` in board-corpus
//! and assert the known-good, shipped boards report zero TRUE shorts. Clearance
//! violations are expected on tightly-routed boards and are not asserted away.
//!
//! The sweep is skipped (not failed) when the corpus is absent, so the test is
//! safe in checkouts without the large board-corpus symlink.
//!
//! ## Documented corpus finding
//!
//! Earlier sweeps surfaced 2 "shorts" on several Olimex ESP32-EVB revisions
//! (REV-A..D, L) and were investigated: they were different-net pads placed
//! deliberately *abutting inside a single footprint* (a fuse-clip and a
//! capacitor footprint). KiCad does not treat intra-footprint pad copper as a
//! board short, so neither does the detector: pads sharing a footprint owner
//! are skipped. This is a real geometric fact handled by a principled rule, not
//! a per-board allowlist. With that rule the entire corpus is short-clean.

use std::path::{Path, PathBuf};

use hauksbee_extract::ExtractedBoard;

/// Locate board-corpus relative to this crate, if present.
fn corpus_root() -> Option<PathBuf> {
    let p = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR")).unwrap_or_default();
    if p.exists() {
        Some(p)
    } else if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        panic!(
            "HAUKSBEE_REQUIRE_CORPUS=1 but board-corpus is missing at {}",
            p.display()
        );
    } else {
        None
    }
}

/// Recursively collect every `.kicad_pcb` under `dir`.
fn find_boards(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            // The `hunt/` area holds un-reviewed, actively-probed boards (some
            // with genuine defects we are reporting upstream, e.g. the BMS
            // REG1_3V3<->GND short). They are deliberately not "known-good", so
            // they are excluded from this short-clean assertion.
            if path.file_name().and_then(|s| s.to_str()) == Some("hunt") {
                continue;
            }
            find_boards(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("kicad_pcb") {
            out.push(path);
        }
    }
}

#[test]
fn corpus_boards_have_no_true_shorts() {
    let Some(root) = corpus_root() else {
        eprintln!("board-corpus not present; skipping corpus DRC sweep");
        return;
    };
    let mut boards = Vec::new();
    find_boards(&root, &mut boards);
    boards.sort();
    assert!(!boards.is_empty(), "found at least one corpus board");

    let mut scanned = 0usize;
    let mut skipped = 0usize;
    let mut total_clearance = 0usize;
    let mut total_prims = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    for board in &boards {
        let Ok(text) = std::fs::read_to_string(board) else {
            continue;
        };
        let report = match ExtractedBoard::drc(&text) {
            Ok(r) => r,
            Err(_) => {
                // A handful of corpus boards are malformed at the s-expression
                // level (e.g. RoyalBlue54L-Feather has a `)`-jammed token and
                // an unbalanced paren) and forge-sexpr rejects them upstream of
                // the DRC. That is a parser/data issue, not a short, so skip it.
                skipped += 1;
                continue;
            }
        };
        scanned += 1;
        total_clearance += report.clearance_violations().count();
        total_prims += report.primitive_count;
        if report.short_count() > 0 {
            let names: Vec<String> = report
                .shorts()
                .take(3)
                .map(|f| format!("{}<->{}@{}", f.net_a_name, f.net_b_name, f.layer))
                .collect();
            offenders.push(format!(
                "{}: {} short(s) [{}]",
                board.file_name().unwrap().to_string_lossy(),
                report.short_count(),
                names.join(", ")
            ));
        }
    }

    eprintln!(
        "corpus DRC: scanned {scanned} board(s) ({skipped} skipped unparseable), \
         {total_prims} primitive(s), {total_clearance} clearance violation(s)"
    );
    assert!(
        scanned >= 40,
        "the bulk of the corpus parsed and was scanned"
    );

    assert!(
        offenders.is_empty(),
        "known-good corpus boards must report zero true shorts; offenders:\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn hunt_sbc_a13_project_rules_resolve_netclasses_and_diff_pairs() {
    let Some(root) = corpus_root() else {
        eprintln!("board-corpus not present; skipping sbc-a13 project-rule regression");
        return;
    };
    let board_path = root.join("famous/hunt/sbc-a13/hardware/module.kicad_pcb");
    let pro_path = root.join("famous/hunt/sbc-a13/hardware/module.kicad_pro");
    if !board_path.exists() || !pro_path.exists() {
        if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
            panic!("HAUKSBEE_REQUIRE_CORPUS=1 but sbc-a13 hunt board/project files are missing");
        }
        eprintln!("sbc-a13 hunt board absent; skipping project-rule regression");
        return;
    }
    let board_text = std::fs::read_to_string(&board_path).expect("read sbc-a13 board");
    let project_text = std::fs::read_to_string(&pro_path).expect("read sbc-a13 project");
    let board = ExtractedBoard::from_kicad_pcb(&board_text).expect("extract sbc-a13 board nets");
    let rules = hauksbee_extract::clearance_rules_from_kicad_pro(
        &project_text,
        board.nets.iter().map(|n| n.name.as_str()),
    )
    .expect("parse sbc-a13 project rules");

    assert!((rules.clearance_for_net("/DDR3 Memory/ddr-ck+") - 0.2).abs() < 1e-9);
    assert!((rules.clearance_for_net("+3V3") - 0.2).abs() < 1e-9);
    assert!((rules.clearance_for_net("USB0-D+") - 0.2).abs() < 1e-9);
    assert!(
        (rules.effective_clearance("/DDR3 Memory/ddr-ck+", "/DDR3 Memory/ddr-ck-") - 0.127).abs()
            < 1e-9
    );
}
