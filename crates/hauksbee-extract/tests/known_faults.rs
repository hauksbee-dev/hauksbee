//! Known-fault validation: run hauksbee against open-hardware board revisions
//! that fixed a real, *documented* electrical fault between two public releases.
//! The revision history is ground truth: "hauksbee flags rev N for exactly the
//! thing rev N+1 fixed" is the strongest calibration the tool can have.
//!
//! These tests are corpus-gated (skipped, not failed, when board-corpus is
//! absent) like `drc_corpus.rs`, because they read the historical revision
//! pairs added under `board-corpus/famous/{zswatch_devkit,watchy_history}`.
//!
//! Each gold pair below has a prior-art citation in
//! `docs/evidence/KNOWN_FAULTS_VALIDATION.md`. The point of encoding them as tests is
//! that the calibration is now CI-enforced: if a future change to the I2C
//! pull-up check stops flagging the faulty ZSWatch DevKit 1.2.0, or starts
//! flagging the fixed 1.2.1, the regression fails here immediately.

use std::path::PathBuf;

use hauksbee_extract::{ExtractedBoard, LintCheck};

/// Locate board-corpus/famous relative to this crate, if present.
///
/// Corpus-gated skip; `HAUKSBEE_REQUIRE_CORPUS=1` turns absence into a hard
/// fail, so the gold-row calibration cannot vacuously green-out on a runner
/// that is supposed to have the corpus. (Matches the convention in
/// `hauksbee-engine/tests/boardcode_miswire.rs`.)
fn famous_root() -> Option<PathBuf> {
    let p = hauksbee_testkit::corpus_dir(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or_default()
        .join("famous");
    if p.exists() {
        return Some(p);
    }
    require_corpus(&p.display().to_string());
    None
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
    let Some(root) = famous_root() else {
        eprintln!("board-corpus/famous absent; skipping ZSWatch DevKit I2C gold row");
        return;
    };
    let faulty = root.join("zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb");
    let fixed = root.join("zswatch_devkit/v1.2.1/ZSWatch-Watch-DevKit.kicad_pcb");
    if !faulty.exists() || !fixed.exists() {
        require_corpus("zswatch_devkit v1.2.0/v1.2.1");
        return;
    }

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
    let Some(root) = famous_root() else {
        eprintln!("board-corpus/famous absent; skipping ZSWatch mainboard cross-check");
        return;
    };
    let board = root.join("zswatch_mainboard/watch/ZSWatch-Watch.kicad_pcb");
    if !board.exists() {
        require_corpus("zswatch_mainboard");
        return;
    }
    let r = lint_pcb(&board);
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
    let Some(root) = famous_root() else {
        eprintln!("board-corpus/famous absent; skipping fixed-control-pull guard");
        return;
    };
    // Watchy v2.0 (R20 RES# pull-up present) and ZSWatch DevKit 1.2.0
    // (R613/R614 DISPLAY-EN pull present) must be clean of floating-control-pin
    // findings.
    let watchy_fixed = root.join("watchy_history/v2.0/Watchy.kicad_pcb");
    let devkit_fixed = root.join("zswatch_devkit/v1.2.0/ZSWatch-Watch-DevKit.kicad_pcb");
    for b in [watchy_fixed, devkit_fixed] {
        if !b.exists() {
            require_corpus(&b.display().to_string());
            continue;
        }
        let r = lint_pcb(&b);
        let floating = r.of_check(LintCheck::FloatingControlPin).count();
        assert_eq!(
            floating,
            0,
            "fixed revision {} should have no floating-control-pin findings, got {floating}",
            b.display()
        );
    }
}
