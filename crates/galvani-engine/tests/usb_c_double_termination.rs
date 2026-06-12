//! Round-2 finding: the ZSWatch DevKit double-terminates its USB-C CC lines.
//!
//! Each CC pin carries a discrete external 5.1 kOhm Rd to GND AND routes (through
//! a populated 0 Ohm bridge) into the nPM1300 PMIC, whose datasheet states its CC
//! pins already have internal pull-downs equal to Rd (5.1 kOhm). The two in
//! parallel give an effective Rd of 2.55 kOhm, which halves the CC voltage a
//! source sees and makes it under-detect the advertised current.
//!
//! This is a KNOWN-but-unfixed issue: the project itself opened issues #178
//! ("Set R605 and R606 (CC bridges) to NA") and #183 ("Remove 0R and 5k1 CC
//! resistors": "nPM1300 has 5k1 CC resistors so the external aren't needed
//! anymore"), but the removal is still in Unreleased, so every corpus revision
//! (1.1.0, 1.2.0, 1.2.1) still carries it. Full derivation in
//! `docs/FAMOUS_SWEEP.md` (Round 2).
//!
//! The test is the discriminator: the audit must fire on all three DevKit
//! revisions AND stay silent on the ZSWatch mainboard (which removed the
//! external Rd and relies on the nPM1300 internal Rd alone, the datasheet-correct
//! design) and on the RPi 4 boards (which have an external Rd but no
//! integrated-Rd PMIC, so the external Rd is correct, not doubled). That silence
//! is what proves the check is not a structural false-positive machine.
//!
//! Corpus-gated like the other corpus tests: absent corpus skips, but
//! GALVANI_REQUIRE_CORPUS=1 makes absence a hard fail.

use std::path::PathBuf;

use galvani_engine::checks::usb_c::audit_cc_termination;
use galvani_extract::ExtractedBoard;

fn famous_root() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous");
    if p.exists() {
        return Some(p);
    }
    if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
        panic!("GALVANI_REQUIRE_CORPUS set but board-corpus is missing at {}", p.display());
    }
    eprintln!("corpus absent; skipping (set GALVANI_REQUIRE_CORPUS=1 to fail)");
    None
}

fn load(root: &PathBuf, rel: &str) -> ExtractedBoard {
    let path = root.join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    if rel.ends_with(".kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(&path).expect("schematic parses")
    } else {
        ExtractedBoard::from_auto(&text).expect("board parses")
    }
}

#[test]
fn devkit_all_revisions_double_terminate_cc() {
    let Some(root) = famous_root() else { return };
    for rel in [
        "zswatch_devkit/v1.1.0/Dev-Kit.kicad_pcb",
        "zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb",
        "zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_pcb",
    ] {
        let board = load(&root, rel);
        let audit = audit_cc_termination(&board)
            .unwrap_or_else(|| panic!("{rel}: no CC termination found"));
        assert!(audit.has_double_termination(), "{rel}: expected double-termination");
        // Both pins: external 5.1k AND nPM1300 internal 5.1k => effective 2.55k.
        for (name, t) in [("CC1", &audit.cc1), ("CC2", &audit.cc2)] {
            assert!(t.is_double_terminated(), "{rel} {name}: not doubled");
            assert!(
                (t.external_rd_ohms.unwrap() - 5100.0).abs() < 1.0,
                "{rel} {name}: external Rd {:?} != 5.1k",
                t.external_rd_ohms
            );
            assert!(
                (t.internal_rd_ohms.unwrap() - 5100.0).abs() < 1.0,
                "{rel} {name}: internal Rd {:?} != 5.1k",
                t.internal_rd_ohms
            );
            assert!(
                (t.effective_rd_ohms().unwrap() - 2550.0).abs() < 1.0,
                "{rel} {name}: effective Rd {:?} != 2.55k",
                t.effective_rd_ohms()
            );
        }
    }
}

#[test]
fn mainboard_internal_rd_only_is_not_a_finding() {
    // The shipped mainboard removed the external Rd: receptacle CC -> nPM1300
    // directly, internal Rd alone. This is the datasheet-correct design and MUST
    // NOT fire, or the check would be a false positive on a shipped board.
    let Some(root) = famous_root() else { return };
    let board = load(&root, "zswatch_mainboard/watch/ZSWatch-Watch.kicad_pcb");
    let audit = audit_cc_termination(&board).expect("mainboard CC termination found");
    assert!(!audit.has_double_termination(), "mainboard must not double-terminate");
    // Internal Rd recognised, no external Rd, so effective Rd is the clean 5.1k.
    assert_eq!(audit.cc1.external_rd_ohms, None);
    assert_eq!(audit.cc1.internal_rd_ohms, Some(5100.0));
    assert_eq!(audit.cc1.effective_rd_ohms(), Some(5100.0));
}

#[test]
fn rpi4_external_rd_without_integrated_pmic_is_not_doubled() {
    // The RPi 4 reconstruction has an external 5.1k Rd but no integrated-Rd PMIC
    // on its CC lines, so the external Rd is the *only* termination: correct, not
    // doubled. (Its shared-net defect is a different finding, covered elsewhere.)
    let Some(root) = famous_root() else { return };
    for rel in [
        "rpi4_usbc_reconstruction/rpi4_usbc_repaired.kicad_sch",
        "rpi4_usbc_reconstruction/rpi4_usbc_as_designed.kicad_sch",
    ] {
        let board = load(&root, rel);
        let audit = audit_cc_termination(&board)
            .unwrap_or_else(|| panic!("{rel}: no CC termination found"));
        assert!(!audit.has_double_termination(), "{rel}: must not double-terminate");
        assert_eq!(audit.cc1.internal_rd_ohms, None, "{rel}: no integrated-Rd PMIC expected");
    }
}
