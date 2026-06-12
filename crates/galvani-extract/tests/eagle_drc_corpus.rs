//! Corpus sweep over the eight famous Eagle `.brd` boards (Arduino Uno official,
//! five Adafruit, two SparkFun). Every one of these shipped and sold in volume,
//! so the geometric DRC must report ZERO true shorts. Clearance violations are
//! expected on tightly-routed boards and are reported, not asserted away.
//!
//! Skipped (not failed) when the corpus is absent, so the test is safe in
//! checkouts without the large board-corpus symlink.
//!
//! ## The Tarski meta-lesson, applied
//!
//! An early version of this extractor reported dozens of "shorts" on these
//! boards. Per `docs/BUG_HUNT.md`, each was chased to the XML before being
//! believed, and every one was the detector's fault, not the board's:
//!
//! - the mirrored-element transform flipped X instead of Y, scrambling the pads
//!   of every bottom-side / `MR`-rotated package;
//! - the wire-`curve` arc flattening picked the wrong circumcircle centre,
//!   swinging parallel differential-pair arcs across each other;
//! - signal polygons (copper pours) were treated as solid copper, when a `.brd`
//!   stores only the requested outline (no `isolate` antipads), so every trace
//!   crossing into a pour read as a short;
//! - solder jumpers / star-ground ties (two nets meeting at one component, e.g.
//!   GND↔UGND through the Uno's `GROUND` SJ jumper) read as track shorts.
//!
//! Each was fixed at the cause with a principled rule (see `drc.rs`), never a
//! per-board allowlist. With those fixes the eight boards are short-clean.

use std::path::PathBuf;
use std::time::Instant;

use galvani_extract::ExtractedBoard;

/// Locate board-corpus relative to this crate, if present.
fn corpus_root() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus");
    p.exists().then_some(p)
}

/// The eight famous Eagle main board files (the "Main board file" column of
/// `board-corpus/famous/SOURCES.md`), relative to `board-corpus/famous`.
const FAMOUS_EAGLE: &[(&str, &str)] = &[
    (
        "Arduino Uno R3 (official)",
        "arduino_uno_r3_official/A000066-cad-files/UNO-TH_Rev3e.brd",
    ),
    (
        "Adafruit Circuit Playground Express",
        "adafruit_circuit_playground/Adafruit Circuit Playground Express.brd",
    ),
    (
        "Adafruit Feather M0 Basic",
        "adafruit_feather_m0/Adafruit Feather M0 Basic rev C.brd",
    ),
    (
        "Adafruit Metro M4 Express",
        "adafruit_metro_m4/Adafruit Metro M4 Express.brd",
    ),
    ("Adafruit QT Py", "adafruit_qtpy/Adafruit QT Py.brd"),
    ("Adafruit Trinket M0", "adafruit_trinket_m0/Trinket M0 rev D.brd"),
    ("SparkFun RedBoard", "sparkfun_redboard/Hardware/RedBoard.brd"),
    (
        "SparkFun Pro Micro",
        "sparkfun_pro_micro/Hardware/v20/SparkFun_Pro_Micro.brd",
    ),
    // Round-3 additions. The RP2040 Thing Plus is the regression guard for the
    // Eagle mirror-transform fix (drc.rs): its MR0 micro-SD socket J6 used to
    // drop pads ~23 mm onto the V_USB/EN bottom traces and report 5 false
    // shorts. It must stay short-clean.
    (
        "SparkFun Thing Plus SAMD51",
        "sparkfun_thingplus_samd51/Hardware/SAMD51_Thing_Plus.brd",
    ),
    (
        "SparkFun Thing Plus RP2040",
        "sparkfun_thingplus_rp2040/Hardware/RP2040_Thing_Plus.brd",
    ),
];

#[test]
fn famous_eagle_boards_have_no_true_shorts() {
    let Some(root) = corpus_root() else {
        eprintln!("board-corpus not present; skipping famous Eagle DRC sweep");
        return;
    };
    let famous = root.join("famous");

    let mut scanned = 0usize;
    let mut offenders: Vec<String> = Vec::new();

    eprintln!(
        "{:<38} {:>6} {:>7} {:>9} {:>8} {:>9}",
        "board", "shorts", "clrnce", "rule(mm)", "prims", "time"
    );
    for (name, rel) in FAMOUS_EAGLE {
        let path = famous.join(rel);
        let Ok(text) = std::fs::read_to_string(&path) else {
            // A board we expect should be present; if the corpus is partial we
            // skip the missing one rather than fail the whole sweep.
            eprintln!("{name:<38} (missing on disk, skipped)");
            continue;
        };
        let t = Instant::now();
        let report = ExtractedBoard::drc(&text).expect("eagle drc runs");
        let dt = t.elapsed();
        scanned += 1;
        let shorts = report.short_count();
        eprintln!(
            "{:<38} {:>6} {:>7} {:>9.4} {:>8} {:>8.1}ms",
            name,
            shorts,
            report.clearance_violations().count(),
            report.clearance_mm,
            report.primitive_count,
            dt.as_secs_f64() * 1e3,
        );
        if shorts > 0 {
            let detail: Vec<String> = report
                .shorts()
                .take(4)
                .map(|f| {
                    format!(
                        "{}<->{}@{} gap{:.3}[{}/{}]",
                        f.net_a_name,
                        f.net_b_name,
                        f.layer,
                        f.gap_mm,
                        f.item_a.kind.as_str(),
                        f.item_b.kind.as_str()
                    )
                })
                .collect();
            offenders.push(format!("{name}: {shorts} short(s) [{}]", detail.join(", ")));
        }
    }

    assert!(scanned >= 1, "at least one famous Eagle board was scanned");
    assert!(
        offenders.is_empty(),
        "the famous Eagle boards shipped and must be short-clean; if one really \
         shows a short, chase it to the XML before believing it (docs/BUG_HUNT.md). \
         Offenders:\n  {}",
        offenders.join("\n  ")
    );
}
