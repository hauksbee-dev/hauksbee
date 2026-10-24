//! Zero-false-positive calibration for the swapped-bus-lines check and the
//! USB-C CC-termination check.
//!
//! The project ships a check only if it is silent on boards known to be good.
//! This sweeps every CAD file of the public board corpus and lists what the two
//! checks find; every finding must be in `CONFIRMED`, with the reason the board
//! really has that flaw. Corpus-gated: skipped when the corpus is absent, unless
//! `HAUKSBEE_REQUIRE_CORPUS=1` turns absence into a failure.

use std::path::{Path, PathBuf};

use hauksbee_checks::checks::{bus_swap::bus_swap_lint, cc_termination::cc_termination_lint};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;

/// Findings on corpus boards that were checked against the board's own
/// documents and are real: `(file name, check, reference)`. Empty today.
const CONFIRMED: &[(&str, &str, &str)] = &[];

fn corpus_root() -> Option<PathBuf> {
    let p = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR")).unwrap_or_default();
    if p.exists() {
        return Some(p);
    }
    assert!(
        std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_err(),
        "HAUKSBEE_REQUIRE_CORPUS set but board-corpus is missing: {}",
        p.display()
    );
    eprintln!("board-corpus absent; skipping bus-swap and CC calibration");
    None
}

fn load(p: &Path) -> Option<ExtractedBoard> {
    if p.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
        return ExtractedBoard::from_kicad_schematic_path(p).ok();
    }
    ExtractedBoard::from_auto(&std::fs::read_to_string(p).ok()?).ok()
}

#[test]
fn bus_swap_and_cc_lints_are_silent_on_the_known_good_corpus() {
    let Some(root) = corpus_root() else { return };
    let lib = ModelLibrary::builtin();
    let mut scanned = 0usize;
    let mut found: Vec<(String, String, String)> = Vec::new();
    for path in hauksbee_testkit::walk_files_with_ext(&root, &["kicad_pcb", "kicad_sch", "net", "brd"]) {
        let Some(board) = load(&path) else { continue };
        scanned += 1;
        let name = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let reports = [
            ("bus_swap", bus_swap_lint(&board, &lib)),
            ("cc_termination", cc_termination_lint(&board, &lib)),
        ];
        for (check, report) in reports {
            for f in report.findings {
                eprintln!("CORPUS FINDING {name} [{check}] {:?}: {}", f.severity, f.message);
                found.push((name.clone(), check.to_string(), f.refs.join(",")));
            }
        }
    }
    eprintln!("scanned {scanned} corpus boards, {} findings", found.len());
    assert!(scanned > 0, "the corpus has no loadable board");
    let unexpected: Vec<_> = found
        .iter()
        .filter(|(file, check, refs)| {
            !CONFIRMED.iter().any(|(f, c, r)| {
                file.ends_with(f) && check == c && refs.split(',').any(|x| x == *r)
            })
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "findings on the known-good corpus that are not confirmed flaws: {unexpected:#?}"
    );
}
