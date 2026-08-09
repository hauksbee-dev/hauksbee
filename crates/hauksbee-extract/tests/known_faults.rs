//! Known-fault validation: run hauksbee against open-hardware board revisions
//! that fixed a real, *documented* electrical fault between two public releases.
//! The revision history is ground truth: "hauksbee flags rev N for exactly the
//! thing rev N+1 fixed" is the strongest calibration the tool can have.
//!
//! These tests are corpus-gated (skipped, not failed, when the board corpus is
//! absent) like `drc_corpus.rs`, because they read the historical revision pairs
//! grouped as `{zswatch_devkit,watchy_history}/<version>/` under the corpus root.
//! Every board goes through the layout-tolerant resolver, so the hand-built and
//! fetched corpora both work; and every gate here prints the number of boards it
//! opened, because a gold row that opened none has validated nothing.
//!
//! Each gold pair below has a prior-art citation in
//! `docs/evidence/KNOWN_FAULTS_VALIDATION.md`. The point of encoding them as tests is
//! that the calibration is now CI-enforced: if a future change to the I2C
//! pull-up check stops flagging the faulty ZSWatch DevKit 1.2.0, or starts
//! flagging the fixed 1.2.1, the regression fails here immediately.

use std::path::PathBuf;

use hauksbee_extract::{ExtractedBoard, LintCheck};

/// The directory the board ids sit under, whichever corpus layout is on disk.
///
/// Corpus-gated skip; `HAUKSBEE_REQUIRE_CORPUS=1` turns absence into a hard
/// fail, so the gold-row calibration cannot vacuously green-out on a runner
/// that is supposed to have the corpus.
///
/// This hardcoded `<corpus>/famous`, which exists only in the hand-built layout.
/// A corpus produced by `scripts/fetch-corpus.sh` has the board ids at its root,
/// so the join named a directory that was not there and every gold row here
/// failed on board LOCATION rather than on a finding.
fn boards_root() -> Option<PathBuf> {
    match hauksbee_testkit::corpus_boards_root(env!("CARGO_MANIFEST_DIR")) {
        Some(p) => Some(p),
        None => {
            require_corpus("the board corpus (neither <corpus>/famous/ nor <corpus>/ resolved)");
            None
        }
    }
}

/// One gold-row board, by corpus-relative path, in whichever layout holds it.
///
/// The revision pairs are the awkward case: the hand-built corpus groups them as
/// `zswatch_devkit/v1.2.0/`, and the fetch writes the same grouping via the
/// manifest's `dest`. Going through the resolver keeps both live.
fn board(rel: &str) -> Option<PathBuf> {
    match hauksbee_testkit::corpus_board(env!("CARGO_MANIFEST_DIR"), rel) {
        Some(p) => Some(p),
        None => {
            require_corpus(rel);
            None
        }
    }
}

/// A required corpus file is missing: skip, unless HAUKSBEE_REQUIRE_CORPUS is set,
/// in which case fail loudly so the calibration is genuinely CI-enforced.
fn require_corpus(what: &str) {
    if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        panic!("HAUKSBEE_REQUIRE_CORPUS set but required corpus path is missing: {what}");
    }
    eprintln!("corpus path absent ({what}); skipping (set HAUKSBEE_REQUIRE_CORPUS=1 to fail)");
}

fn lint_pcb(path: &PathBuf) -> hauksbee_extract::NetLintReport {
    let text = std::fs::read_to_string(path).expect("read board");
    ExtractedBoard::from_auto(&text)
        .expect("parse board")
        .net_lint()
}

fn i2c_findings(r: &hauksbee_extract::NetLintReport) -> usize {
    r.of_check(LintCheck::MissingI2cPullup).count()
}

/// Count medium-or-higher I2C pull-up findings (the on-board, high-confidence
/// ones; Low is the intentional header break-out and is not a fault).
fn i2c_medium_plus(r: &hauksbee_extract::NetLintReport) -> usize {
    use hauksbee_extract::Severity;
    r.of_check(LintCheck::MissingI2cPullup)
        .filter(|f| matches!(f.severity, Severity::Medium | Severity::High))
        .count()
}

// ---------------------------------------------------------------------------
// GOLD ROW: ZSWatch Watch-DevKit-HW, "missing I2C pull-ups" (PR #158).
//
//   github.com/ZSWatch/Watch-DevKit-HW, CHANGELOG 1.2.1:
//     "Add missing pull-ups for I2C lines (#158)"
//
// Faulty 1.2.0: the RTC-side I2C bus (PCA9306 SDA2/SCL2 -> RV-8263 RTC) carries
// no pull-up. The PCA9306 is a bare pass-gate (no integrated pulls), so that
// translated bus genuinely needs its own resistors. hauksbee flags it MEDIUM.
// Fixed 1.2.1: R504/R505 (3k3) added; hauksbee goes clean. This is the strongest
// possible calibration: the tool flags exactly what the next revision fixed.
// ---------------------------------------------------------------------------

#[test]
fn zswatch_devkit_i2c_pullup_flagged_in_faulty_clean_in_fixed() {
    let Some(faulty) = board("zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb") else {
        return;
    };
    let Some(fixed) = board("zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_pcb") else {
        return;
    };
    hauksbee_testkit::scanned("ZSWatch DevKit I2C gold row", 2);

    // Faulty 1.2.0: at least the two on-board RTC bus lines flag at medium+.
    let rf = lint_pcb(&faulty);
    let med_faulty = i2c_medium_plus(&rf);
    assert!(
        med_faulty >= 2,
        "faulty DevKit 1.2.0 should flag the missing RTC I2C pull-ups (>=2 medium), got {med_faulty}: {:?}",
        rf.of_check(LintCheck::MissingI2cPullup).map(|f| &f.message).collect::<Vec<_>>()
    );

    // Fixed 1.2.1: the pull-ups are present; ZERO I2C findings of any severity.
    let rfx = lint_pcb(&fixed);
    assert_eq!(
        i2c_findings(&rfx),
        0,
        "fixed DevKit 1.2.1 added R504/R505 and should be I2C-clean, got: {:?}",
        rfx.of_check(LintCheck::MissingI2cPullup)
            .map(|f| &f.message)
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// Cross-check: the SHIPPED ZSWatch mainboard (corpus board, the FIXED design
// for this same PCA9306 + RV-8263 RTC topology) carries its pull-ups and is
// clean. This is the same circuit the FAMOUS_SWEEP C2 deep-dive analysed, where
// the pulls WERE present; here we assert hauksbee stays clean on it, so the
// faulty/fixed contrast above is a true discriminator and not a parser quirk.
// ---------------------------------------------------------------------------

#[test]
fn zswatch_mainboard_rtc_i2c_is_clean() {
    let Some(mainboard) = board("zswatch_mainboard/watch/ZSWatch-Watch.kicad_pcb") else {
        return;
    };
    hauksbee_testkit::scanned("ZSWatch mainboard RTC I2C cross-check", 1);
    let r = lint_pcb(&mainboard);
    assert_eq!(
        i2c_medium_plus(&r),
        0,
        "shipped ZSWatch mainboard should have no on-board missing-pull-up findings, got: {:?}",
        r.of_check(LintCheck::MissingI2cPullup)
            .map(|f| &f.message)
            .collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// DOCUMENTED MISS (guard, not a gold row): the "control input driven only by a
// Hi-Z-capable MCU GPIO with no pull" fault. Two independently-cited instances:
//
//   * Watchy issue #14: e-paper RES# on a normal GPIO, no pull-up; fixed in
//     v2.0 by adding R20 (100K) to 3V3.
//   * ZSWatch DevKit issue #123: DISPLAY-EN (the NTS0104 OE) on GPIO P0.20,
//     no pull-up; fixed in 1.2.0 by adding R613/R614 (100K).
//
// hauksbee does NOT flag these, and the validation doc explains why this is the
// correct, honest behaviour: the identical structure (GPIO -> IC enable/reset,
// no pull) appears on DISPLAY-CS, DISPLAY-RST, VIB-EN and on the shipped
// mainboard / Reform / Watchy boost-EN, none of which are faults. A structural
// check cannot tell the one documented fault from the many benign twins without
// manufacturing false positives. This test pins that the FIXED revisions, which
// DO carry the pull, are clean, so if a future check ever tries to fire here it
// must at least not fire on the corrected designs.
// ---------------------------------------------------------------------------

#[test]
fn fixed_control_pull_revisions_are_lint_clean() {
    if boards_root().is_none() {
        return;
    }
    // Watchy v2.0 (R20 RES# pull-up present) and ZSWatch DevKit 1.2.0
    // (R613/R614 DISPLAY-EN pull present) must be clean of floating-control-pin
    // findings.
    let wanted = [
        "watchy_history/v2.0/Watchy.kicad_pcb",
        "zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb",
    ];
    let mut checked = 0usize;
    for rel in wanted {
        let Some(b) = board(rel) else { continue };
        checked += 1;
        let r = lint_pcb(&b);
        let floating = r.of_check(LintCheck::FloatingControlPin).count();
        assert_eq!(
            floating,
            0,
            "fixed revision {} should have no floating-control-pin findings, got {floating}",
            b.display()
        );
    }
    hauksbee_testkit::scanned("fixed-control-pull guard", checked);
}

// ---------------------------------------------------------------------------
// ODrive v2 GND <-> AGND overlap gold row. The upstream repo's v2 directory
// carries two layouts: Inverter45attempt.PcbDoc (an attempt that was never the
// shipped design) and the final Inverter.PcbDoc. The attempt overlaps its GND
// and AGND pours on F.Cu at x=153.2, y=183.9 (gap -1.0 mm), and the file's own
// rule set forbids it: its only ShortCircuit rule is the Altium default (scope
// All/All, ALLOWED=FALSE), so Altium's DRC would flag the identical overlap and
// no scoped allowance sanctions a deliberate ground join. The final layout is
// short-clean, which makes the pair a true discriminator: hauksbee flags the
// attempt for exactly the defect the final file does not have. The odrive_v2
// corpus entry is known_good = false for this adjudicated reason (the marker is
// per-directory and both files share one), so THIS test, not the fetched-sweep
// silence gate, owns both files' coverage.
// ---------------------------------------------------------------------------

#[test]
fn odrive_v2_attempt_ground_short_flagged_and_final_is_clean() {
    let Some(attempt) = board("odrive/v2/v2/Inverter45attempt.PcbDoc") else {
        return;
    };
    let Some(fixed) = board("odrive/v2/v2/Inverter.PcbDoc") else {
        return;
    };
    hauksbee_testkit::scanned("ODrive v2 ground-short gold row", 2);

    let bytes = std::fs::read(&attempt).expect("read attempt board");
    let report = hauksbee_extract::ExtractedBoard::altium_drc(&bytes).expect("drc runs");
    let shorts: Vec<_> = report.shorts().collect();
    assert_eq!(
        shorts.len(),
        1,
        "the attempt layout carries exactly the one GND/AGND overlap, got: {:?}",
        shorts
            .iter()
            .map(|s| format!("{} <-> {} on {}", s.net_a_name, s.net_b_name, s.layer))
            .collect::<Vec<_>>()
    );
    let s = shorts[0];
    let mut nets = [s.net_a_name.as_str(), s.net_b_name.as_str()];
    nets.sort_unstable();
    assert_eq!(
        nets,
        ["AGND", "GND"],
        "the attempt's short is the analog/digital ground overlap"
    );
    assert_eq!(s.layer, "F.Cu");

    let bytes = std::fs::read(&fixed).expect("read final board");
    let report = hauksbee_extract::ExtractedBoard::altium_drc(&bytes).expect("drc runs");
    assert_eq!(
        report.shorts().count(),
        0,
        "the final Inverter.PcbDoc is short-clean, so the contrast is real"
    );
}

// ---------------------------------------------------------------------------
// MWGEN-G1 pad-overlap gold row. A shipped 10 MHz-to-6 GHz signal generator,
// 373 parts, drawn in KiCad 6. Its reference-input corner puts an SMAJ48CA TVS
// in an SMA body (D503, 2.5 x 1.8 mm pads) on top of two SOT-23 diodes and two
// hand-solder 0603s, and its Laird BMI-S-205-F shield-can fence pad on J206 on
// top of a 0603 ferrite bead. Six pads of different nets overlap as a result.
//
// The ground truth is the same shape as the ODrive row's: the design's own rule
// set forbids the overlaps. MWGEN-G1.kicad_pro sets `shorting_items` and
// `clearance` to `error` and carries no drc_exclusions, and KiCad 9.0.3's own
// `kicad-cli pcb drc` on this file reports the same six shorting_items
// violations with the same net pairs and the same pad anchors. The
// pin-every-pair assertion below is what makes this a regression gate rather
// than a count: a change that loses one of the six, or that adds a seventh KiCad
// does not report, fails here.
// ---------------------------------------------------------------------------

#[test]
fn mwgen_g1_pad_overlap_shorts_match_kicads_own_drc() {
    let Some(pcb) = board("mwgen_g1/MWGEN-G1.kicad_pcb") else {
        return;
    };
    hauksbee_testkit::scanned("MWGEN-G1 pad-overlap gold row", 1);

    let text = std::fs::read_to_string(&pcb).expect("read MWGEN-G1 board");
    let report = hauksbee_extract::ExtractedBoard::drc(&text).expect("drc runs");

    // Net pair (sorted), layer, and the two footprints whose pads overlap, as
    // KiCad's own DRC anchors them.
    let expected = [
        (
            [
                "/Clock reference/Input Clocks/REFIN",
                "/Clock reference/Input Clocks/REFIN_GND",
            ],
            "F.Cu",
            ["C205", "D503"],
        ),
        (
            [
                "/Clock reference/Input Clocks/REFIN",
                "/Clock reference/Input Clocks/REFIN_GND",
            ],
            "F.Cu",
            ["D503", "R204"],
        ),
        (
            ["/Clock reference/Input Clocks/REFIN_GND", "Net-(D202-Pad3)"],
            "F.Cu",
            ["D202", "D503"],
        ),
        (
            ["/Clock reference/Input Clocks/REFIN_GND", "Net-(D202-Pad3)"],
            "F.Cu",
            ["D203", "D503"],
        ),
        (
            ["/Clock reference/REFOUT_SEC", "GND"],
            "F.Cu",
            ["D203", "D503"],
        ),
        (["GND", "Net-(FB204-Pad1)"], "F.Cu", ["FB204", "J206"]),
    ];

    let mut got: Vec<([String; 2], String, [String; 2])> = report
        .shorts()
        .map(|s| {
            let mut nets = [s.net_a_name.clone(), s.net_b_name.clone()];
            nets.sort();
            let mut owners = [s.item_a.owner.clone(), s.item_b.owner.clone()];
            owners.sort();
            (nets, s.layer.clone(), owners)
        })
        .collect();
    got.sort();
    let want: Vec<([String; 2], String, [String; 2])> = expected
        .iter()
        .map(|(nets, layer, owners)| {
            (
                [nets[0].to_string(), nets[1].to_string()],
                (*layer).to_string(),
                [owners[0].to_string(), owners[1].to_string()],
            )
        })
        .collect();
    assert_eq!(
        got, want,
        "the six pad overlaps KiCad 9.0.3 reports on this file, exactly"
    );

    // Every one is a pad-to-pad overlap, not a zone or track artefact: that is
    // what makes the KiCad cross-check a like-for-like comparison.
    for s in report.shorts() {
        assert!(
            s.gap_mm < 0.0,
            "overlapping pads, so the gap is negative; {} <-> {} measured {}",
            s.net_a_name,
            s.net_b_name,
            s.gap_mm
        );
    }
}

// ---------------------------------------------------------------------------
// emonTx V3.4.5 pour-overlap row: one file, one net pair, two layers, opposite
// verdicts, and the fabrication output to settle which is which.
//
// GND and AGND are separate nets with no bridging component, and their
// same-rank pours overlap in an identical 0.349 mm band on both copper layers
// (GND's outline runs along y=43.205 with fill below; AGND's runs along
// y=42.856 with fill above). Reading that outline overlap as copper reported a
// short on both layers. The gerbers upstream ships beside the `.brd` disagree:
// on F.Cu, where both pours hold isolate="0.3048", copper_top.gbr keeps them as
// two distinct filled regions; on B.Cu, where the AGND pour carries
// isolate="0.00030625", copper_bottom.gbr merges them into one region. So F.Cu
// was a false positive and B.Cu is real copper, and `isolate` is the whole
// difference. The emontx3 corpus entry is known_good = false for the B.Cu join,
// so this test owns the file's coverage.
// ---------------------------------------------------------------------------

#[test]
fn emontx_ground_pour_join_is_flagged_on_the_merged_layer_only() {
    let Some(brd) = board("emontx3/hardware/V3.4.5/emonTx V3.4.5.brd") else {
        return;
    };
    hauksbee_testkit::scanned("emonTx V3.4.5 pour-overlap row", 1);

    let text = std::fs::read_to_string(&brd).expect("read emonTx board");
    let report = hauksbee_extract::ExtractedBoard::drc(&text).expect("drc runs");
    let shorts: Vec<_> = report.shorts().collect();
    assert_eq!(
        shorts.len(),
        1,
        "only the layer whose fill actually merged, got: {:?}",
        shorts
            .iter()
            .map(|s| format!("{} <-> {} on {}", s.net_a_name, s.net_b_name, s.layer))
            .collect::<Vec<_>>()
    );
    let s = shorts[0];
    let mut nets = [s.net_a_name.as_str(), s.net_b_name.as_str()];
    nets.sort_unstable();
    assert_eq!(nets, ["AGND", "GND"]);
    assert_eq!(
        s.layer, "B.Cu",
        "copper_bottom.gbr is the layer with one merged region"
    );
    assert!(
        s.item_a.owner.contains("isolate 0.00030625")
            || s.item_b.owner.contains("isolate 0.00030625"),
        "the finding names the zeroed isolate that let the pours meet, got {:?} / {:?}",
        s.item_a.owner,
        s.item_b.owner
    );
}
