//! Round-4 Surface B calibration guard: the `output_contention` schematic-ERC
//! check must stay SILENT on every known-good board in the famous schematic
//! corpus. This is the round-2 rejected-check discipline made a regression: a
//! check whose findings are only trustworthy if it is provably quiet on
//! boards known to be correct. If a future change makes it fire on any of these,
//! this test goes red before the false positive ships.
//!
//! Corpus-gated: skipped when the board corpus is absent, unless
//! `HAUKSBEE_REQUIRE_CORPUS=1` is set (then a missing corpus is a failure). The
//! board count it scanned is printed on every run and a scan of zero is a
//! failure, because a calibration gate that opened no board has calibrated
//! nothing.

use std::path::{Path, PathBuf};

use hauksbee_extract::{ExtractedBoard, LintCheck};

/// The directory the board ids sit under, whichever layout this machine has.
///
/// This used to be `corpus_dir(..).unwrap_or_default().join("famous")`, which
/// only ever resolved on the hand-built corpus. On the corpus that
/// `scripts/fetch-corpus.sh` produces there is no `famous/` level, so the path
/// did not exist, the guard read it as "no corpus", and the gate skipped.
fn corpus() -> Option<PathBuf> {
    hauksbee_testkit::corpus_boards_root_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "output_contention corpus calibration",
    )
}

/// Known-good schematic-bearing boards (schematic roots + pin-typed netlists).
///
/// Each entry is a list of alternate relative paths for ONE board, tried in
/// order. Most boards need only one; the RP2040 reference design needs two
/// because Raspberry Pi replaced revision 2 with revision 3 at the same URL, so
/// the hand-built corpus holds r2 and the public fetch holds both under
/// different ids. Naming a single path there meant the board silently dropped out
/// of the calibration on whichever corpus you had.
fn known_good() -> Vec<&'static [&'static str]> {
    vec![
        &["zswatch_mainboard/watch/ZSWatch-Watch.kicad_sch"],
        &["zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_sch"],
        &["zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_sch"],
        &["zswatch_devkit/v1.1.0/Dev-Kit.kicad_sch"],
        &["watchy/Watchy.kicad_sch"],
        &["lumenpnp/mobo/mobo.kicad_sch"],
        &["lumenpnp/ring-light/ringLight.kicad_sch"],
        &["olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch"],
        &["lily58/Pro_V2/Pro_V2.kicad_sch"],
        &[
            "rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_sch",
            "rp2040_minimal_r2/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_sch",
            "rp2040_minimal_kicad/RPI-RP2040-MINIMAL_R3-S1_public/RPI-RP2040-MINIMAL_R3-S1.kicad_sch",
        ],
        &["mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_sch"],
        &["mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_sch"],
        &["olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision D/RP2040-PICO-PC_rev_D.net"],
        &["olimex_esp32/HARDWARE/REV-K1/ESP32-EVB_Rev_K1.net"],
    ]
}

fn load(p: &Path) -> Option<ExtractedBoard> {
    if p.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(p).ok()
    } else {
        ExtractedBoard::from_auto(&std::fs::read_to_string(p).ok()?).ok()
    }
}

#[test]
fn output_contention_is_silent_on_known_good_corpus() {
    // `corpus()` already prints the not-run note, and panics under
    // HAUKSBEE_REQUIRE_CORPUS. It used to be reported here as "board-corpus is
    // absent", which named the wrong thing: board-corpus was present, the
    // `famous/` level under it was not, and the message sent readers looking for
    // a directory that was already there.
    let Some(root) = corpus() else { return };

    let mut scanned = 0usize;
    let mut offenders: Vec<String> = Vec::new();
    for alts in known_good() {
        let Some(path) = alts.iter().map(|rel| root.join(rel)).find(|p| p.exists()) else {
            // Present-but-unreadable and absent are different failures, and this
            // one is absence. Recorded so the coverage gap is visible; the
            // `scanned` floor below is what makes it matter.
            offenders.push(format!("{}: NOT PRESENT in this corpus", alts[0]));
            continue;
        };
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let Some(board) = load(&path) else {
            // A board that fails to load is recorded, not silently skipped.
            offenders.push(format!("{rel}: FAILED TO LOAD"));
            continue;
        };
        scanned += 1;
        let report = board.net_lint();
        for f in report.of_check(LintCheck::OutputContention) {
            offenders.push(format!("{rel}: {} [{}]", f.message, f.severity.as_str()));
        }
    }

    // Say what was covered, and refuse a pass on zero. `scanned >= 10` alone
    // was not enough: a corpus root that resolved to a directory holding none of
    // these boards produced a load failure per entry rather than a scan, and the
    // failure list was the only thing that went red - never the coverage.
    hauksbee_testkit::scanned("output_contention corpus calibration", scanned);
    assert!(
        scanned >= 10,
        "expected to scan the known-good corpus, scanned {scanned}"
    );
    assert!(
        offenders.is_empty(),
        "output_contention must be silent on known-good boards, but fired:\n  {}",
        offenders.join("\n  ")
    );
}
