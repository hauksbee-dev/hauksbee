//! USB-C CC double-termination audit, and the DNP discipline behind it.
//!
//! Round 2 of the famous-board hunt raised a candidate: the ZSWatch DevKit's
//! receptacle CC lines each carry a discrete 5.1 kOhm Rd footprint AND route into
//! the nPM1300 PMIC (whose CC pins have an internal 5.1 kOhm Rd). If both were
//! populated they would parallel to 2.55 kOhm and the board would mis-detect
//! charger current. Chased to ground truth (the Tarski meta-lesson), this was a
//! galvani defect, not a board fault: the external 5.1 kOhm resistors (R603/R604,
//! and R304/R305 on v1.1.0) are marked Do-Not-Populate in both the schematic
//! (`(dnp yes)`) and the PCB (`(attr ... dnp)`), so they are not assembled. The
//! board ships with the nPM1300 internal Rd alone, the datasheet-correct design.
//! galvani was blind to DNP and counted the unplaced footprints as live, which
//! manufactured the phantom 2.55 kOhm. The fix teaches the extractor to read DNP
//! and the audit to skip DNP parts. Full write-up in docs/FAMOUS_SWEEP.md (R2).
//!
//! These tests pin the corrected behaviour: the DevKit, the mainboard and the
//! repaired RPi 4 all present a single, correct 5.1 kOhm Rd and the audit reports
//! NO double-termination on any of them. The DevKit case is the regression guard:
//! if the extractor ever stops honouring DNP, it goes back to firing here.
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
fn devkit_external_cc_rd_is_dnp_so_no_double_termination() {
    // The regression guard: the external 5.1k Rd footprints are DNP, so once the
    // extractor honours DNP the DevKit presents the nPM1300 internal Rd alone.
    // No double-termination on any revision. (Before the DNP fix this fired -
    // the false positive the Tarski meta-lesson exists to catch.)
    let Some(root) = famous_root() else { return };
    for rel in [
        "zswatch_devkit/v1.1.0/Dev-Kit.kicad_pcb",
        "zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb",
        "zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_pcb",
    ] {
        let board = load(&root, rel);
        let audit = audit_cc_termination(&board)
            .unwrap_or_else(|| panic!("{rel}: no CC termination found"));
        assert!(
            !audit.has_double_termination(),
            "{rel}: external Rd is DNP, must not double-terminate"
        );
        for (name, t) in [("CC1", &audit.cc1), ("CC2", &audit.cc2)] {
            // External Rd is unpopulated => not counted.
            assert_eq!(t.external_rd_ohms, None, "{rel} {name}: DNP Rd must not be counted");
            // The nPM1300 internal Rd is the sole, correct termination.
            assert_eq!(t.internal_rd_ohms, Some(5100.0), "{rel} {name}: nPM1300 Rd expected");
            assert_eq!(t.effective_rd_ohms(), Some(5100.0), "{rel} {name}: effective Rd must be 5.1k");
        }
    }
}

#[test]
fn devkit_external_cc_resistors_are_marked_dnp() {
    // Directly assert the populate status the audit depends on, so the test
    // documents the ground truth rather than trusting the audit's internals.
    let Some(root) = famous_root() else { return };
    let board = load(&root, "zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb");
    let find = |r: &str| board.components.iter().find(|c| c.reference == r).cloned();
    for r in ["R603", "R604"] {
        let c = find(r).unwrap_or_else(|| panic!("{r} not found"));
        assert_eq!(c.value, "5k1", "{r} should be the 5k1 external Rd");
        assert!(c.dnp, "{r} (external CC Rd) must be DNP");
    }
    for r in ["R605", "R606"] {
        let c = find(r).unwrap_or_else(|| panic!("{r} not found"));
        assert!(!c.dnp, "{r} (the 0R CC bridge) is populated");
    }
}

#[test]
fn mainboard_internal_rd_only_is_clean() {
    let Some(root) = famous_root() else { return };
    let board = load(&root, "zswatch_mainboard/watch/ZSWatch-Watch.kicad_pcb");
    let audit = audit_cc_termination(&board).expect("mainboard CC termination found");
    assert!(!audit.has_double_termination());
    assert_eq!(audit.cc1.external_rd_ohms, None);
    assert_eq!(audit.cc1.internal_rd_ohms, Some(5100.0));
    assert_eq!(audit.cc1.effective_rd_ohms(), Some(5100.0));
}

#[test]
fn rpi4_external_rd_without_integrated_pmic_is_not_doubled() {
    // The RPi 4 reconstruction has a populated external 5.1k Rd but no
    // integrated-Rd PMIC, so that Rd is the sole, correct termination - and it is
    // NOT marked DNP, so the audit still sees it (proving the DNP skip is
    // targeted, not a blanket suppression).
    let Some(root) = famous_root() else { return };
    for rel in [
        "rpi4_usbc_reconstruction/rpi4_usbc_repaired.kicad_sch",
        "rpi4_usbc_reconstruction/rpi4_usbc_as_designed.kicad_sch",
    ] {
        let board = load(&root, rel);
        let audit = audit_cc_termination(&board)
            .unwrap_or_else(|| panic!("{rel}: no CC termination found"));
        assert!(!audit.has_double_termination(), "{rel}: must not double-terminate");
        assert_eq!(audit.cc1.external_rd_ohms, Some(5100.0), "{rel}: populated external Rd seen");
        assert_eq!(audit.cc1.internal_rd_ohms, None, "{rel}: no integrated-Rd PMIC");
    }
}
