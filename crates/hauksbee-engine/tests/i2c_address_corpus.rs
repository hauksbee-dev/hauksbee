//! I2C address-clash lint, swept over the known-good board corpus.
//!
//! The check reads each target's address from its card's `[models.i2c]` block and
//! the address-select pins from the netlist, so a fire on a board that works
//! means a wrong card row, a wrong bus join or a strap read the wrong way. Every
//! known-good KiCad layout in the corpus is linted with the built-in cards and
//! no `i2c_address_clash` finding that is not a real clash may remain. Corpus-gated
//! like the other corpus tests: an absent corpus skips, and
//! `HAUKSBEE_REQUIRE_CORPUS=1` makes absence a hard fail.

use std::path::PathBuf;

use hauksbee_engine::checks::engine_lint;
use hauksbee_extract::{ExtractedBoard, LintCheck};
use hauksbee_models::ModelLibrary;

fn corpus_root() -> Option<PathBuf> {
    hauksbee_testkit::corpus_boards_root_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "I2C address-clash corpus sweep",
    )
}

/// Layouts whose clash is real, as `(path suffix, reference)` with the reason.
/// A finding here is a true flaw of the published board, not a false alarm.
const REAL_CLASHES: &[(&str, &str, &str)] = &[];

#[test]
fn no_known_good_corpus_board_reports_an_i2c_address_clash() {
    let Some(root) = corpus_root() else { return };
    let lib = ModelLibrary::builtin();
    let mut scanned = 0;
    let mut with_targets = 0;
    let mut judged = 0;
    let mut bad = Vec::new();
    for path in hauksbee_testkit::walk_files_with_ext(&root, &["kicad_pcb", "brd", "net"]) {
        if let Some(why) = hauksbee_testkit::not_known_good(&path, &root) {
            hauksbee_testkit::excluded("i2c_address", &path.display().to_string(), &why);
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(board) = ExtractedBoard::from_auto(&text) else {
            continue;
        };
        scanned += 1;
        let targets = board
            .components
            .iter()
            .filter_map(|c| hauksbee_extract::assembly::AssemblyState::of(c).fitted())
            .filter(|part| {
                hauksbee_bind::binder::resolve(&lib, *part)
                    .model
                    .is_some_and(|m| m.i2c.is_some())
            })
            .count();
        if targets > 0 {
            with_targets += 1;
        }
        if targets > 1 {
            judged += 1;
            eprintln!("I2C TARGETS {targets} on {}", path.display());
        }
        for f in engine_lint(&board, &lib)
            .findings
            .into_iter()
            .filter(|f| f.check == LintCheck::I2cAddressClash)
        {
            let rel = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            eprintln!(
                "I2C CLASH {rel}: {:?} {:?}: {}",
                f.severity, f.refs, f.message
            );
            let known = REAL_CLASHES
                .iter()
                .any(|(suffix, r, _)| rel.ends_with(suffix) && f.refs.iter().any(|x| x == r));
            if !known && !f.is_abstention() {
                bad.push(format!(
                    "{rel}: {:?} {:?}: {}",
                    f.severity, f.refs, f.message
                ));
            }
        }
    }
    hauksbee_testkit::scanned("i2c_address", scanned);
    eprintln!("I2C boards with a carded target: {with_targets}, with two or more: {judged}");
    assert!(
        bad.is_empty(),
        "unexpected I2C address findings:\n{}",
        bad.join("\n")
    );
}
