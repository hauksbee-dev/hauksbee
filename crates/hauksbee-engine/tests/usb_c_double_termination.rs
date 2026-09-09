//! USB-C CC double-termination audit, and the DNP discipline behind it.
//!
//! Round 2 of the famous-board hunt raised a candidate: the ZSWatch DevKit's
//! receptacle CC lines each carry a discrete 5.1 kOhm Rd footprint AND route into
//! the nPM1300 PMIC (whose CC pins have an internal 5.1 kOhm Rd). If both were
//! populated they would parallel to 2.55 kOhm and the board would mis-detect
//! charger current. Chased to ground truth (the Tarski meta-lesson), this was a
//! hauksbee defect, not a board fault: the external 5.1 kOhm resistors (R603/R604,
//! and R304/R305 on v1.1.0) are marked Do-Not-Populate in both the schematic
//! (`(dnp yes)`) and the PCB (`(attr ... dnp)`), so they are not assembled. The
//! board ships with the nPM1300 internal Rd alone, the datasheet-correct design.
//! hauksbee was blind to DNP and counted the unplaced footprints as live, which
//! manufactured the phantom 2.55 kOhm. The fix teaches the extractor to read DNP
//! and the audit to skip DNP parts. Full write-up in docs/evidence/CORPUS.md (R2).
//!
//! These tests pin the corrected behaviour: the DevKit, the mainboard and the
//! repaired RPi 4 all present a single, correct 5.1 kOhm Rd and the audit reports
//! NO double-termination on any of them. The DevKit case is the regression guard:
//! if the extractor ever stops honouring DNP, it goes back to firing here.
//!
//! Corpus-gated like the other corpus tests: absent corpus skips, but
//! HAUKSBEE_REQUIRE_CORPUS=1 makes absence a hard fail.

use std::path::{Path, PathBuf};

use hauksbee_engine::checks::usb_c::audit_cc_termination;
use hauksbee_extract::ExtractedBoard;

/// The directory the corpus boards sit directly under, whichever layout this
/// machine has, or `None` with a printed note.
///
/// Resolved through the testkit rather than by joining `famous/` here: that
/// level exists only in the hand-built corpus, so a hard join found nothing on
/// the corpus `scripts/fetch-corpus.sh` produces, and every sweep below walked
/// an empty directory and reported the empty walk as a pass.
fn corpus_root() -> Option<PathBuf> {
    hauksbee_testkit::corpus_boards_root_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "USB-C CC double-termination corpus sweep",
    )
}

fn load(root: &Path, rel: &str) -> ExtractedBoard {
    let path = root.join(rel);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    if rel.ends_with(".kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(&path).expect("schematic parses")
    } else {
        ExtractedBoard::from_auto(&text).expect("board parses")
    }
}

/// [`load`] for a board that is not redistributable, so absence is a skip.
///
/// `corpus.toml` lists the boards it cannot point at and states that nothing in
/// the public test suite depends on them. This suite did: it read the RPi 4
/// reconstruction through [`load`], which panics on a missing file, so the claim
/// held only for as long as nobody ran the sweep on a corpus without it. Under
/// `HAUKSBEE_REQUIRE_CORPUS` a missing local-only board is still a skip rather
/// than a failure, because no fetch can obtain it.
fn load_local_only(root: &Path, rel: &str) -> Option<ExtractedBoard> {
    if !root.join(rel).exists() {
        eprintln!("NOT RUN  {rel}: local-only board, not redistributable (corpus.toml)");
        return None;
    }
    Some(load(root, rel))
}

#[test]
fn devkit_external_cc_rd_is_dnp_so_no_double_termination() {
    // The regression guard: the external 5.1k Rd footprints are DNP, so once the
    // extractor honours DNP the DevKit presents the nPM1300 internal Rd alone.
    // No double-termination on any revision. (Before the DNP fix this fired -
    // the false positive the Tarski meta-lesson exists to catch.)
    let Some(root) = corpus_root() else { return };
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
            assert_eq!(
                t.external_rd_ohms, None,
                "{rel} {name}: DNP Rd must not be counted"
            );
            // The nPM1300 internal Rd is the sole, correct termination.
            assert_eq!(
                t.internal_rd_ohms,
                Some(5100.0),
                "{rel} {name}: nPM1300 Rd expected"
            );
            assert_eq!(
                t.effective_rd_ohms(),
                Some(5100.0),
                "{rel} {name}: effective Rd must be 5.1k"
            );
        }
    }
}

#[test]
fn devkit_external_cc_resistors_are_marked_dnp() {
    // Directly assert the populate status the audit depends on, so the test
    // documents the ground truth rather than trusting the audit's internals.
    let Some(root) = corpus_root() else { return };
    let board = load(
        &root,
        "zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb",
    );
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
    let Some(root) = corpus_root() else { return };
    let board = load(&root, "zswatch_mainboard/watch/ZSWatch-Watch.kicad_pcb");
    let audit = audit_cc_termination(&board).expect("mainboard CC termination found");
    assert!(!audit.has_double_termination());
    assert_eq!(audit.cc1.external_rd_ohms, None);
    assert_eq!(audit.cc1.internal_rd_ohms, Some(5100.0));
    assert_eq!(audit.cc1.effective_rd_ohms(), Some(5100.0));
}

#[test]
fn lily58_dual_receptacle_both_halves_terminated() {
    // Round 3 recorded an honest tool limitation: the CC audit under-read the
    // device-side Rd on a board with two USB-C receptacles (the Lily58's two
    // halves J1 and J6). Chased to ground truth, the under-read was a hauksbee
    // defect with two compounding causes, not a board fault:
    //   1. the audit resolved a single best-scoring receptacle, so the second
    //      receptacle's independent Rd was never read; and
    //   2. J6's two 5.1k Rd resistors (R11/R12) return to GNDA, a secondary
    //      analog ground, which the audit's GND lookup did not recognise, so
    //      even J6 alone read as un-terminated.
    // The fix audits every receptacle and credits any recognised ground. Both
    // halves now present an independent, correct 5.1k Rd on each CC pin (R2/R3
    // on J1 to GND, R11/R12 on J6 to GNDA), verified against the .kicad_pcb.
    let Some(root) = corpus_root() else { return };
    let board = load(&root, "lily58/Pro_V2/Pro_V2.kicad_pcb");
    let audit = audit_cc_termination(&board).expect("lily58 CC termination found");

    // Two distinct receptacles, both credited.
    let refs: Vec<&str> = audit
        .receptacles
        .iter()
        .map(|r| r.reference.as_str())
        .collect();
    assert!(
        refs.contains(&"J1"),
        "J1 receptacle must be audited, got {refs:?}"
    );
    assert!(
        refs.contains(&"J6"),
        "J6 receptacle must be audited, got {refs:?}"
    );

    for rec in &audit.receptacles {
        for (name, t) in [("CC1", &rec.cc1), ("CC2", &rec.cc2)] {
            assert_eq!(
                t.external_rd_ohms,
                Some(5100.0),
                "{} {name}: independent 5.1k Rd expected",
                rec.reference
            );
            assert!(
                !t.is_double_terminated(),
                "{} {name}: no PMIC, must not double",
                rec.reference
            );
        }
    }
    // Both halves terminated, and clean of any double-termination.
    assert!(
        audit.all_receptacles_terminated(),
        "both Lily58 halves must be terminated"
    );
    assert!(!audit.has_double_termination(), "Lily58 is clean");

    // The CLASSIFIER path (extract_sink_termination → usb_c_report) must agree
    // with the audit: a J6 Rd that returns to GNDA (a secondary analog ground)
    // must still be credited, so the board-level USB-C verdict is OK, not a false
    // "no discrete Rd → INFO". (The audit fix above only fixed the audit; the
    // classifier kept the single-GND lookup until this was caught via `--usb-c`.)
    let report = hauksbee_engine::usb_c_report(&board).expect("usb-c report");
    assert_eq!(
        report.level,
        hauksbee_engine::UsbcLevel::Ok,
        "Lily58 is a correctly-terminated sink; verdict must be OK, not INFO. \
         headline: {}",
        report.headline
    );
    assert!(
        report.has_discrete_rd,
        "the GNDA-returned Rd must be credited"
    );
}

#[test]
fn rpi4_external_rd_without_integrated_pmic_is_not_doubled() {
    // The RPi 4 reconstruction has a populated external 5.1k Rd but no
    // integrated-Rd PMIC, so that Rd is the sole, correct termination - and it is
    // NOT marked DNP, so the audit still sees it (proving the DNP skip is
    // targeted, not a blanket suppression).
    let Some(root) = corpus_root() else { return };
    let mut scanned = 0usize;
    for rel in [
        "rpi4_usbc_reconstruction/rpi4_usbc_repaired.kicad_sch",
        "rpi4_usbc_reconstruction/rpi4_usbc_as_designed.kicad_sch",
    ] {
        let Some(board) = load_local_only(&root, rel) else {
            continue;
        };
        scanned += 1;
        let audit = audit_cc_termination(&board)
            .unwrap_or_else(|| panic!("{rel}: no CC termination found"));
        assert!(
            !audit.has_double_termination(),
            "{rel}: must not double-terminate"
        );
        assert_eq!(
            audit.cc1.external_rd_ohms,
            Some(5100.0),
            "{rel}: populated external Rd seen"
        );
        assert_eq!(
            audit.cc1.internal_rd_ohms, None,
            "{rel}: no integrated-Rd PMIC"
        );
    }
    // Say what ran. A silent zero here reads as a pass, and this case's whole
    // point is that the DNP skip is targeted rather than a blanket suppression:
    // proving that on nothing proves nothing.
    eprintln!("SCANNED  rpi4 external-Rd case: {scanned} board(s)");
}
