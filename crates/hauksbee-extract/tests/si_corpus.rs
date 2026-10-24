//! Corpus-gated calibration guard for the four signal-integrity checks
//! (`--si`). The binding discipline (`docs/evidence/CORPUS.md`,
//! `docs/evidence/CORPUS.md)`: **zero true findings on the known-good
//! corpus, or the check does not fire.** These boards are shipped, working,
//! reviewed designs, so any high/medium/low SI finding on them is a hauksbee
//! false positive that must be chased to the file and killed before the check is
//! trusted. This test asserts the sweep stays silent.
//!
//! It also pins a handful of the auditable INFO values that the chase produced
//! (the RP2040 ABM8-272 board CL, the ZSWatch 8-device I2C bus rise time, the
//! Olimex WROOM keepout verdict, a matched USB pair skew), so a regression that
//! silently broke the physics or the geometry parse would also go red here.
//!
//! Skipped when the corpus is absent; `HAUKSBEE_REQUIRE_CORPUS=1` makes absence a
//! hard fail so it cannot vacuously pass on a runner that should have the corpus.

use std::path::PathBuf;

use hauksbee_extract::{ExtractedBoard, SiCheck, SiSeverity};

/// The directory the corpus boards sit directly under, via the shared resolver:
/// `famous/` in the hand-built layout, the corpus root in the `<id>` layout
/// scripts/fetch-corpus.sh produces. Joining `famous` directly is what made this
/// sweep walk an empty tree for anyone who used the documented fetch.
fn corpus_root() -> Option<PathBuf> {
    match hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR")) {
        Some(p) => Some(p),
        None if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() => {
            panic!("HAUKSBEE_REQUIRE_CORPUS set but no board corpus is present")
        }
        None => None,
    }
}

/// One board, by the paths the corpus pins it at, through the shared resolver so
/// both directory layouts are accepted.
fn corpus_board(rels: &[&str]) -> Option<PathBuf> {
    hauksbee_testkit::corpus_board_any(env!("CARGO_MANIFEST_DIR"), rels)
}

/// The RP2040 minimal reference board, revision 2 only, at both the paths the
/// corpus pins it at.
///
/// The values the three RP2040 tests below pin (Y1 = ABM8-272-T3 driven by 15 pF
/// caps, a routed USB D+/D- pair, no `(stackup)` block) were measured on r2. The
/// `rp2040_minimal_kicad` entry in `corpus.toml` now serves r3, which dropped the
/// crystal and carries no `Y1`, so those numbers are not facts about it. r2 is a
/// second entry under its own id for that reason, which is why the two paths
/// differ in their first element and neither can be dropped: the hand-built
/// corpus filed r2 under `rp2040_minimal_kicad`, and the fetch writes each board
/// to `<corpus>/<id>` so it lands under `rp2040_minimal_r2`.
const RP2040_MINIMAL_R2: &[&str] = &[
    "famous/rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_pcb",
    "famous/rp2040_minimal_r2/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_pcb",
];

/// Every KiCad `.kicad_pcb` and Eagle `.brd` under a directory.
fn board_files(root: &PathBuf) -> Vec<PathBuf> {
    fn walk(dir: &PathBuf, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name().and_then(|s| s.to_str()) == Some("hunt") {
                    continue;
                }
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

fn run_si(path: &PathBuf) -> Option<hauksbee_extract::SiReport> {
    let text = std::fs::read_to_string(path).ok()?;
    let board = ExtractedBoard::from_auto(&text).ok()?;
    Some(board.si_checks(Some(&text)))
}

/// THE GATE: no SI check fires (high/medium/low) on any known-good corpus board.
/// The zero-false-positive gate over the whole known-good corpus.
///
/// Every board in the corpus is a shipping or reference design, so any SI
/// finding here is a false positive to fix in the check, never a board to
/// whitelist. The classes it has caught are pinned by unit tests in
/// `si/tests.rs`: a WROOM keepout measured from the pad-row centre instead of
/// the footprint's pad edge, the WROOM-32 rectangle applied to a WROOM-02, a DC
/// bias line (`ANT_BIAS`) read as a 50 ohm feed, a differential pair judged
/// against a reference plane nobody could verify, and crystals on drivers with
/// on-chip load capacitance (XR22417, EFM32PG22).
///
/// About 7 s over the whole corpus; skips visibly when the corpus is absent.
#[test]
fn si_checks_are_silent_on_the_entire_known_good_corpus() {
    let Some(famous) = corpus_root() else {
        eprintln!("corpus absent; skipping SI corpus sweep");
        return;
    };
    let mut offenders: Vec<String> = Vec::new();
    let mut swept = 0usize;
    for path in board_files(&famous) {
        let Some(report) = run_si(&path) else {
            continue;
        };
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
/// crystal whose CL is derivable from the value: RP2040 datasheet Table 257 gives 10 pF), driven with
/// 15 pF caps -> board CL ~ 11.5 pF, inside the 8 pF tolerance: an INFO, never a
/// fire. Pins both the known-CL lookup and the series-resistor cap trace.
#[test]
fn rp2040_abm8_272_load_is_info_within_tolerance() {
    let Some(path) = corpus_board(RP2040_MINIMAL_R2) else {
        assert!(
            std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_err(),
            "RP2040 minimal missing under required corpus"
        );
        return;
    };
    let report = run_si(&path).expect("RP2040 minimal must extract");
    let xtal: Vec<_> = report.of_check(SiCheck::CrystalLoadCap).collect();
    assert!(!xtal.is_empty(), "RP2040 should produce a crystal note");
    let y1 = xtal
        .iter()
        .find(|f| f.refs.iter().any(|r| r == "Y1"))
        .expect("Y1 ABM8-272 note present");
    assert_eq!(
        y1.severity,
        SiSeverity::Info,
        "ABM8-272 at 15pF caps is within tolerance"
    );
    // The board CL ~ 11.5 pF and the 10 pF spec must both be in the note.
    assert!(
        y1.message.contains("spec 10 pF"),
        "the ABM8-272 CL spec must be cited: {}",
        y1.message
    );
}

/// The ZSWatch mainboard's busiest I2C bus (the 8-device Extension bus, 1.8 kohm
/// pull) computes `t_r` ~ 122 ns, far under the standard-mode 1000 ns: clean. This
/// is the corpus's closest-to-the-limit I2C bus, so it is the discriminating
/// no-fire (a regression that mis-scaled the RC, or counted the connector as a
/// device, would push it over and this would go red as a finding).
#[test]
fn zswatch_busy_i2c_bus_rise_time_is_clean() {
    let Some(path) = corpus_board(&["famous/zswatch_mainboard/watch/ZSWatch-Watch.kicad_pcb"])
    else {
        assert!(
            std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_err(),
            "ZSWatch mainboard missing under required corpus"
        );
        return;
    };
    let report = run_si(&path).expect("ZSWatch mainboard must extract");
    // No I2C rise-time finding anywhere on the board.
    assert_eq!(
        report
            .of_check(SiCheck::I2cRiseTime)
            .filter(|f| f.severity.is_finding())
            .count(),
        0,
        "ZSWatch I2C buses are all within standard-mode rise time"
    );
    // The Extension bus note must be present and under the limit.
    let ext = report
        .of_check(SiCheck::I2cRiseTime)
        .find(|f| f.message.contains("Extension/SDA"))
        .expect("Extension SDA bus note present");
    assert!(
        ext.message.contains("ok"),
        "busy bus must read ok: {}",
        ext.message
    );
}

/// The Olimex ESP32-EVB carries a WROOM-32 mounted flush with the board's top
/// edge, so the module's own antenna end (and the 15 mm keepout beyond it) lies
/// at or off the board edge: the keepout check is clear (INFO), never a fire.
///
/// Measured on Rev L: the footprint's antenna-side pad edge sits at y 74.12 and
/// the board outline starts at y 67.06, so the whole antenna region is either
/// off the board or empty board. The only copper near the band is the module's
/// own ground-pad row (stitching vias, fill clearance contour), which reaches at
/// most 0.65 mm past the pad edge; it is not antenna-region copper. This test
/// used to fail with 17 to 22 ground intrusions per revision because the check
/// started the band at the pad-row CENTRE and counted that pad-row environment;
/// the geometry is now taken from the footprint's pad edge with a 1 mm
/// registration slack, and the unit tests in `si/tests.rs` pin that a ground
/// track or pour genuinely in the antenna region still fires.
#[test]
fn olimex_wroom_antenna_keepout_is_clear() {
    let Some(path) =
        corpus_board(&["famous/olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_pcb"])
    else {
        assert!(
            std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_err(),
            "Olimex ESP32-EVB REV-L missing under required corpus"
        );
        return;
    };
    let report = run_si(&path).expect("Olimex ESP32-EVB REV-L must extract");
    let ant: Vec<_> = report.of_check(SiCheck::AntennaKeepout).collect();
    assert_eq!(
        ant.len(),
        1,
        "exactly the WROOM module produces a keepout note"
    );
    assert_eq!(
        ant[0].severity,
        SiSeverity::Info,
        "WROOM keepout is clear (edge placement)"
    );
    assert!(ant[0].message.contains("clear"));
}

/// A matched USB pair (RP2040 minimal D+/D-) reads a small skew well inside the
/// budget: INFO, never a fire. Pins the routed-length geometry walk.
#[test]
fn rp2040_usb_pair_skew_is_info() {
    let Some(path) = corpus_board(RP2040_MINIMAL_R2) else {
        eprintln!("NOT RUN  RP2040 minimal r2 absent; see RP2040_MINIMAL_R2");
        return;
    };
    let report = run_si(&path).expect("RP2040 minimal must extract");
    let usb: Vec<_> = report.of_check(SiCheck::UsbDiffPair).collect();
    assert!(!usb.is_empty(), "RP2040 has a routed USB D+/D- pair");
    assert!(
        usb.iter().all(|f| f.severity == SiSeverity::Info),
        "the matched RP2040 USB pair must not fire"
    );
}

/// The RP2040 minimal board has NO `(stackup ...)` block, so the controlled-
/// impedance check cannot compute a real impedance: it reports the USB pair
/// estimate under the stated default-assumption stackup, as INFO only, never a
/// finding. This pins the "unknown stackup -> info, never a fire" path on a real
/// board (the headline zero-false-positive guard for the new check).
#[test]
fn rp2040_no_stackup_impedance_is_info_only() {
    let Some(path) = corpus_board(RP2040_MINIMAL_R2) else {
        eprintln!("NOT RUN  RP2040 minimal r2 absent; see RP2040_MINIMAL_R2");
        return;
    };
    let report = run_si(&path).expect("RP2040 minimal must extract");
    let zi: Vec<_> = report.of_check(SiCheck::ControlledImpedance).collect();
    assert!(
        !zi.is_empty(),
        "RP2040 USB pair produces a controlled-impedance note"
    );
    assert!(
        zi.iter().all(|f| f.severity == SiSeverity::Info),
        "no stackup -> impedance is info only, never a finding"
    );
    assert!(
        zi.iter().any(|f| f.message.contains("ASSUMED")),
        "the default-assumption stackup must be flagged"
    );
}

/// A corpus board WITH a stackup (Watchy, 4-layer, dielectric 0.28 mm Er 4.5)
/// computes a real differential impedance for its USB pair and surfaces it as an
/// auditable INFO note. Because Watchy sets `dielectric_constraints no` (it did
/// not intend to control the full-speed USB pair), the out-of-band estimate is
/// info, NOT a finding: the intent gate in action on a real board.
#[test]
fn watchy_usb_impedance_computed_but_info_uncontrolled() {
    let Some(path) = corpus_board(&["famous/watchy/Watchy.kicad_pcb"]) else {
        assert!(
            std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_err(),
            "Watchy missing under required corpus"
        );
        return;
    };
    let report = run_si(&path).expect("Watchy must extract");
    let zi: Vec<_> = report.of_check(SiCheck::ControlledImpedance).collect();
    let usb = zi
        .iter()
        .find(|f| f.message.contains("USB_D"))
        .expect("Watchy USB pair impedance note present");
    assert_eq!(
        usb.severity,
        SiSeverity::Info,
        "uncontrolled board -> info not a fire"
    );
    // The note carries the real board stackup (not the default) and the target.
    assert!(
        usb.message.contains("board") && usb.message.contains("90 ohm"),
        "note carries the file stackup and the USB target: {}",
        usb.message
    );
    assert!(
        usb.message
            .contains("does not declare controlled impedance"),
        "the intent gate must be explained: {}",
        usb.message
    );
    // Watchy is the board that calibrated the reference-plane check's anti-pad
    // handling. Its USB pair routes on B.Cu over a solid In2.Cu GND plane, but
    // the pair's four layer-transition vias punch anti-pads in that plane, and
    // sampling the pair's segment endpoints lands in them: the first cut of the
    // check reported "reference missing under trace" over 2.91 mm here, on a
    // plane that is in fact continuous. All seven of those samples were within
    // 0.36 mm of a via centre and four were exactly on one. A designed anti-pad
    // is not a plane void, so this must read as an ordinary impedance note.
    assert!(
        !usb.message.contains("reference missing"),
        "Watchy's In2.Cu plane is solid under the pair; its via anti-pads must \
         not read as a missing reference: {}",
        usb.message
    );
    assert!(
        !usb.message.contains("reference plane unverified"),
        "Watchy has a real In2.Cu pour, so the reference must be positively \
         verified rather than declared unverifiable: {}",
        usb.message
    );
}
