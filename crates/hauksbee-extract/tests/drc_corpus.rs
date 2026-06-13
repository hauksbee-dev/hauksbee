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
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus");
    p.exists().then_some(p)
}

/// Recursively collect every `.kicad_pcb` under `dir`.
fn find_boards(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
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
    assert!(scanned >= 40, "the bulk of the corpus parsed and was scanned");

    assert!(
        offenders.is_empty(),
        "known-good corpus boards must report zero true shorts; offenders:\n  {}",
        offenders.join("\n  ")
    );
}
