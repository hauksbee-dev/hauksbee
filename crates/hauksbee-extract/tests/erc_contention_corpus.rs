//! Round-4 Surface B calibration guard: the `output_contention` schematic-ERC
//! check must stay SILENT on every known-good board in the famous schematic
//! corpus. This is the round-2 rejected-check discipline made a regression: a
//! check whose findings are only trustworthy if it is provably quiet on
//! boards known to be correct. If a future change makes it fire on any of these,
//! this test goes red before the false positive ships.
//!
//! Corpus-gated: skipped when the board-corpus symlink is absent, unless
//! `HAUKSBEE_REQUIRE_CORPUS=1` is set (then a missing corpus is a failure).

use std::path::{Path, PathBuf};

use hauksbee_extract::{ExtractedBoard, LintCheck};

fn corpus() -> Option<PathBuf> {
    let p = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or_default()
        .join("famous");
    p.exists().then_some(p)
}

/// Known-good schematic-bearing boards (schematic roots + pin-typed netlists).
fn known_good() -> Vec<&'static str> {
    vec![
        "zswatch_mainboard/watch/ZSWatch-Watch.kicad_sch",
        "zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_sch",
        "zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_sch",
        "zswatch_devkit/v1.1.0/Dev-Kit.kicad_sch",
        "watchy/Watchy.kicad_sch",
        "lumenpnp/mobo/mobo.kicad_sch",
        "lumenpnp/ring-light/ringLight.kicad_sch",
        "olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch",
        "lily58/Pro_V2/Pro_V2.kicad_sch",
        "rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_sch",
        "mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_sch",
        "mnt_reform/reform2-motherboard25-pcb/reform2-motherboard25.kicad_sch",
        "olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision D/RP2040-PICO-PC_rev_D.net",
        "olimex_esp32/HARDWARE/REV-K1/ESP32-EVB_Rev_K1.net",
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
    let Some(root) = corpus() else {
        assert!(
            std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_err(),
            "HAUKSBEE_REQUIRE_CORPUS set but board-corpus is absent"
        );
        eprintln!("board-corpus not present; skipping ERC contention calibration");
        return;
    };

    let mut scanned = 0usize;
    let mut offenders: Vec<String> = Vec::new();
    for rel in known_good() {
        let path = root.join(rel);
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
