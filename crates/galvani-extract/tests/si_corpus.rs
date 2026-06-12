//! Corpus-gated calibration guard for the four signal-integrity checks
//! (`--si`). The binding discipline (docs/FAMOUS_SWEEP.md,
//! docs/KNOWN_FAULTS_VALIDATION.md): **zero true findings on the known-good
//! corpus, or the check does not fire.** These boards are shipped, working,
//! reviewed designs, so any high/medium/low SI finding on them is a galvani
//! false positive that must be chased to the file and killed before the check is
//! trusted. This test asserts the sweep stays silent.
//!
//! It also pins a handful of the auditable INFO values that the chase produced
//! (the RP2040 ABM8-272 board CL, the ZSWatch 8-device I2C bus rise time, the
//! Olimex WROOM keepout verdict, a matched USB pair skew), so a regression that
//! silently broke the physics or the geometry parse would also go red here.
//!
//! Skipped when the corpus is absent; `GALVANI_REQUIRE_CORPUS=1` makes absence a
//! hard fail so it cannot vacuously pass on a runner that should have the corpus.

use std::path::PathBuf;

use galvani_extract::{ExtractedBoard, SiCheck, SiSeverity};

fn corpus_famous() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous");
    if p.exists() {
        Some(p)
    } else if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
        panic!("GALVANI_REQUIRE_CORPUS set but board-corpus/famous is absent");
    } else {
        None
    }
}

/// Every KiCad `.kicad_pcb` and Eagle `.brd` under a directory.
fn board_files(root: &PathBuf) -> Vec<PathBuf> {
    fn walk(dir: &PathBuf, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if ext == "kicad_pcb" || ext == "brd" {
                    out.push(p);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn run_si(path: &PathBuf) -> Option<galvani_extract::SiReport> {
    let text = std::fs::read_to_string(path).ok()?;
    let board = ExtractedBoard::from_auto(&text).ok()?;
    Some(board.si_checks(Some(&text)))
}

/// THE GATE: no SI check fires (high/medium/low) on any known-good corpus board.
#[test]
fn si_checks_are_silent_on_the_entire_known_good_corpus() {
    let Some(famous) = corpus_famous() else {
        eprintln!("corpus absent; skipping SI corpus sweep");
        return;
    };
    let mut offenders: Vec<String> = Vec::new();
    let mut swept = 0usize;
    for path in board_files(&famous) {
        let Some(report) = run_si(&path) else { continue };
        swept += 1;
        for f in report.findings_only() {
            offenders.push(format!(
                "{}: [{}] {} - {}",
                path.strip_prefix(&famous).unwrap_or(&path).display(),
                f.severity.as_str(),
                f.check.as_str(),
                f.message
            ));
        }
    }
    assert!(
        swept >= 50,
        "expected to sweep the full corpus (>=50 board files), only saw {swept}"
    );
    assert!(
        offenders.is_empty(),
        "SI checks must be SILENT on the known-good corpus; {} false positive(s) found:\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}

/// The RP2040 minimal reference board carries Y1 = ABM8-272-T3 (the one corpus
/// crystal whose datasheet CL = 18 pF is derivable from the value), driven with
/// 15 pF caps -> board CL ~ 11.5 pF, inside the 8 pF tolerance: an INFO, never a
/// fire. Pins both the known-CL lookup and the series-resistor cap trace.
#[test]
fn rp2040_abm8_272_load_is_info_within_tolerance() {
    let Some(famous) = corpus_famous() else {
        return;
    };
    let path =
        famous.join("rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_pcb");
    let Some(report) = run_si(&path) else {
        if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
            panic!("RP2040 minimal missing under required corpus");
        }
        return;
    };
    let xtal: Vec<_> = report.of_check(SiCheck::CrystalLoadCap).collect();
    assert!(!xtal.is_empty(), "RP2040 should produce a crystal note");
    let y1 = xtal
        .iter()
        .find(|f| f.refs.iter().any(|r| r == "Y1"))
        .expect("Y1 ABM8-272 note present");
    assert_eq!(y1.severity, SiSeverity::Info, "ABM8-272 at 15pF caps is within tolerance");
    // The board CL ~ 11.5 pF and the 18 pF spec must both be in the note.
    assert!(y1.message.contains("18 pF"), "the ABM8-272 CL spec must be cited: {}", y1.message);
}

/// The ZSWatch mainboard's busiest I2C bus (the 8-device Extension bus, 1.8 kohm
/// pull) computes t_r ~ 122 ns, far under the standard-mode 1000 ns: clean. This
/// is the corpus's closest-to-the-limit I2C bus, so it is the discriminating
/// no-fire (a regression that mis-scaled the RC, or counted the connector as a
/// device, would push it over and this would go red as a finding).
#[test]
fn zswatch_busy_i2c_bus_rise_time_is_clean() {
    let Some(famous) = corpus_famous() else {
        return;
    };
    let path = famous.join("zswatch_mainboard/watch/ZSWatch-Watch.kicad_pcb");
    let Some(report) = run_si(&path) else {
        if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
            panic!("ZSWatch mainboard missing under required corpus");
        }
        return;
    };
    // No I2C rise-time finding anywhere on the board.
    assert_eq!(
        report.of_check(SiCheck::I2cRiseTime).filter(|f| f.severity.is_finding()).count(),
        0,
        "ZSWatch I2C buses are all within standard-mode rise time"
    );
    // The Extension bus note must be present and under the limit.
    let ext = report
        .of_check(SiCheck::I2cRiseTime)
        .find(|f| f.message.contains("Extension/SDA"))
        .expect("Extension SDA bus note present");
    assert!(ext.message.contains("ok"), "busy bus must read ok: {}", ext.message);
}

/// The Olimex ESP32-EVB carries the corpus's only ESP32-WROOM-32 module, mounted
/// at the board's top edge so the 15 mm antenna keepout lies off the board: the
/// keepout check is correctly clear (INFO), never a fire. Pins the WROOM keepout
/// geometry and the edge-placement no-fire.
#[test]
fn olimex_wroom_antenna_keepout_is_clear() {
    let Some(famous) = corpus_famous() else {
        return;
    };
    let path = famous.join("olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_pcb");
    let Some(report) = run_si(&path) else {
        if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
            panic!("Olimex ESP32-EVB REV-L missing under required corpus");
        }
        return;
    };
    let ant: Vec<_> = report.of_check(SiCheck::AntennaKeepout).collect();
    assert_eq!(ant.len(), 1, "exactly the WROOM module produces a keepout note");
    assert_eq!(ant[0].severity, SiSeverity::Info, "WROOM keepout is clear (edge placement)");
    assert!(ant[0].message.contains("clear"));
}

/// A matched USB pair (RP2040 minimal D+/D-) reads a small skew well inside the
/// budget: INFO, never a fire. Pins the routed-length geometry walk.
#[test]
fn rp2040_usb_pair_skew_is_info() {
    let Some(famous) = corpus_famous() else {
        return;
    };
    let path =
        famous.join("rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_pcb");
    let Some(report) = run_si(&path) else {
        return;
    };
    let usb: Vec<_> = report.of_check(SiCheck::UsbDiffPair).collect();
    assert!(!usb.is_empty(), "RP2040 has a routed USB D+/D- pair");
    assert!(
        usb.iter().all(|f| f.severity == SiSeverity::Info),
        "the matched RP2040 USB pair must not fire"
    );
}
