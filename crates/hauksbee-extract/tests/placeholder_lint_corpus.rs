//! Corpus silence gate for the placeholder-value lint.
//!
//! The check's contract is "a passive whose value was never set". Real boards
//! carry parts whose value is empty BY DESIGN - solder jumpers, solder bridges,
//! net ties - and several of them look passive by reference prefix (the Arduino
//! Uno's RESET-EN solder jumper starts with 'R'). Before the link-class
//! exemption, the lint fired a [medium] "set the actual R value" on exactly
//! that part, a confident false positive on one of the most-manufactured boards
//! in existence.
//!
//! This gate pins the calibration: across the ENTIRE known-good corpus, on
//! every extraction path (layout, netlist, Eagle board, schematic), the
//! placeholder-value lint produces ZERO medium-or-high findings. Any fire is a
//! false positive by construction, because these boards all shipped with fully
//! specified BOMs.
//!
//! Corpus-gated: skipped when board-corpus is absent, with
//! `HAUKSBEE_REQUIRE_CORPUS=1` (CI) turning absence into a hard failure so the
//! gate cannot vacuously green-out.

use std::path::PathBuf;

use hauksbee_extract::{ExtractedBoard, LintCheck, Severity};

/// The corpus root. The sweep recurses, so it covers both the hand-built
/// (`board-corpus/famous/<id>`) and fetch (`board-corpus/<id>`) layouts - and
/// hybrids - without caring which level the boards sit at.
fn boards_root() -> Option<PathBuf> {
    match hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR")) {
        Some(p) => Some(p),
        None => {
            if hauksbee_testkit::require_assets() {
                panic!("HAUKSBEE_REQUIRE_CORPUS set but board-corpus is missing");
            }
            eprintln!("board-corpus absent; skipping placeholder-lint corpus gate");
            None
        }
    }
}

#[test]
fn placeholder_value_is_silent_at_medium_and_above_across_corpus() {
    let Some(root) = boards_root() else { return };
    let mut offenders: Vec<String> = Vec::new();
    let mut exercised = 0usize;
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            // Every extraction path the lint runs on. `.kicad_sch` goes through
            // the schematic extractor; the rest through format auto-detection.
            // Parse defensively: a file the extractor cannot read is a coverage
            // gap for a different test, not a placeholder-lint false positive.
            let board = match ext {
                "kicad_sch" => match ExtractedBoard::from_kicad_schematic_path(&p) {
                    Ok(b) => b,
                    Err(_) => continue,
                },
                "kicad_pcb" | "net" | "brd" => {
                    let Ok(text) = std::fs::read_to_string(&p) else {
                        continue;
                    };
                    match ExtractedBoard::from_auto(&text) {
                        Ok(b) => b,
                        Err(_) => continue,
                    }
                }
                _ => continue,
            };
            exercised += 1;
            for f in board.net_lint().of_check(LintCheck::PlaceholderValue) {
                if matches!(f.severity, Severity::Medium | Severity::High) {
                    offenders.push(format!(
                        "{} [{}]: {}",
                        p.display(),
                        f.severity.as_str(),
                        f.message
                    ));
                }
            }
        }
    }
    // A walk that parsed nothing proves nothing; refuse the vacuous pass. The
    // tally is printed so a run's coverage is auditable, not inferred.
    eprintln!("placeholder-lint corpus gate: {exercised} board files exercised");
    assert!(
        exercised > 0,
        "placeholder-lint corpus gate walked the corpus but parsed zero boards"
    );
    assert!(
        offenders.is_empty(),
        "placeholder_value fired at medium+ on known-good corpus boards (false positive(s)):\n{}",
        offenders.join("\n")
    );
}
