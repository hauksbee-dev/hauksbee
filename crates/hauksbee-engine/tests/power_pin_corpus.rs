//! Power-pin wiring lint, pinned to a real board fault in the corpus.
//!
//! Ground truth: the apfaudio eurorack-pmod calibration EEPROM, U5, is a
//! Microchip 24AA025UID (BOM `24AA025UIDT-I/OT`). That part exists only in the
//! 6-lead SOT-23 (DS20005202A, Table 2-1: SCL 1, VSS 2, SDA 3, A1 4, A0 5,
//! VCC 6); the 5-lead SOT-23 is the 24AA02UID, which has VCC on pad 4. The
//! board's KiCad symbol is the 5-pin `24AA02-OT` on a `SOT-23-6` footprint, so
//! revisions r3.2, r3.3 and r3.5 route `VDD` to pad 4 (the 24AA025UID's A1
//! address input) and ground to pad 5 (its A0), and leave pad 6, the real `VCC`
//! pad, on no net. The EEPROM is unpowered, and its address inputs are driven
//! from the rails. Revision r3.1 also ties pad 6 to `VDD`, so it is wired
//! correctly and is the fixed-side control: the same part and footprint, the
//! lint silent.
//!
//! The boards are CERN-OHL-S-2.0 and are not vendored (see `corpus.toml`); the
//! tests read them from the fetched corpus. Corpus-gated like the other corpus
//! tests: an absent corpus skips, and `HAUKSBEE_REQUIRE_CORPUS=1` makes
//! absence a hard fail so the pin cannot go green by not running.

use std::path::PathBuf;

use hauksbee_engine::checks::engine_lint;
use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, Severity};
use hauksbee_models::ModelLibrary;

fn corpus_root() -> Option<PathBuf> {
    hauksbee_testkit::corpus_boards_root_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "eurorack-pmod U5 power-pin corpus pin",
    )
}

/// Every `power_pin_wiring` finding the full engine lint raises on the given
/// eurorack-pmod revision's layout.
fn power_pin_findings(root: &std::path::Path, revision: &str) -> Vec<LintFinding> {
    let rel =
        format!("eurorack_pmod/hardware/eurorack-pmod-{revision}/eurorack-pmod-pcb.kicad_pcb");
    let path = root.join(&rel);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let board = ExtractedBoard::from_auto(&text).unwrap_or_else(|e| panic!("parse {rel}: {e}"));
    let lib = ModelLibrary::builtin();
    engine_lint(&board, &lib)
        .findings
        .into_iter()
        .filter(|f| f.check == LintCheck::PowerPinWiring)
        .collect()
}

/// The fault: VCC (pad 6) of the 6-lead 24AA025UID is on no net.
#[test]
fn eurorack_pmod_u5_vcc_pad_is_unpowered_on_r32_r33_r35() {
    let Some(root) = corpus_root() else { return };
    for revision in ["r3.2", "r3.3", "r3.5"] {
        let found = power_pin_findings(&root, revision);
        let on_u5: Vec<&LintFinding> = found
            .iter()
            .filter(|f| f.refs.iter().any(|r| r == "U5"))
            .collect();
        assert_eq!(
            on_u5.len(),
            1,
            "{revision}: exactly one power-pin finding on U5, got: {:#?}",
            found.iter().map(|f| &f.message).collect::<Vec<_>>()
        );
        let f = on_u5[0];
        assert_eq!(f.severity, Severity::High, "{revision}: serious");
        assert_eq!(f.refs, ["U5"], "{revision}: names only U5");
        for needle in [
            "U5 (24AA025UIDT EEPROM)",
            "has no supply on its VCC input",
            "pad 6 (VCC)",
            "24aa025uid_sot23_6",
        ] {
            assert!(
                f.message.contains(needle),
                "{revision}: message should contain {needle:?}: {}",
                f.message
            );
        }
        // The board's own symbol puts V_CC on pad 4, which on this package is
        // the A1 address input: the diagnosis says where the symbol disagrees.
        assert!(
            f.message.contains("pad 4") && f.message.contains("puts VCC on pad 6"),
            "{revision}: the symbol/footprint disagreement is named: {}",
            f.message
        );
    }
}

/// The control: r3.1 routes pad 6 to `VDD`, so the same part on the same
/// footprint is powered and the lint is silent on U5.
#[test]
fn eurorack_pmod_r31_u5_vcc_pad_is_powered() {
    let Some(root) = corpus_root() else { return };
    let found = power_pin_findings(&root, "r3.1");
    assert!(
        !found.iter().any(|f| f.refs.iter().any(|r| r == "U5")),
        "r3.1 wires U5 pad 6 to VDD; no power-pin finding expected: {:#?}",
        found.iter().map(|f| &f.message).collect::<Vec<_>>()
    );
}
