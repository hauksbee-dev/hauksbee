//! Two-sided validation + calibration for the MCU internal resource-conflict
//! check, against the famous-board corpus.
//!
//! Validation (the check MUST fire, with the documented evidence chain):
//!   - Olimex RP2040-PICO-PC rev C/D: the PicoDVI PWM pixel clock (GP12) and the
//!     PWM stereo audio left channel (GP28) both map to RP2040 PWM slice 6,
//!     channel A. Open issue #1 on OLIMEX/RP2040-PICO-PC, unfixed across the
//!     shipped revisions.
//!   - SparkFun SAMD51 Thing Plus: the AT25SF041 SPI flash sits on PA08..PA11,
//!     the SAM D5x QSPI DATA0..3 pins. sparkfun/Arduino_Boards issue #82.
//!
//! Ground-truth detail (the discriminator, not a miss): Olimex rev **B** is
//! SILENT and that is CORRECT - in rev B the DVI clock is on GP14/GP15 (PWM
//! slice 7), so it does not collide with the audio on slice 6A. The slice-6A
//! conflict was introduced in rev C when the DVI clock moved to GP12/GP13. The
//! check flags exactly the revisions where the fault exists.
//!
//! Calibration (the check MUST be silent - zero false positives): every other
//! corpus board carrying an RP2040 / SAMD51 / ESP32, plus a spread of unrelated
//! boards, must produce no finding. A fire on any of these would be the
//! confident false positive the Tarski meta-lesson forbids.
//!
//! Corpus-gated: skipped when the board-corpus symlink is absent, UNLESS
//! `GALVANI_REQUIRE_CORPUS=1` is set, in which case a missing corpus is a hard
//! failure so the validation cannot vacuously pass on a runner that should have
//! the corpus.

use std::path::{Path, PathBuf};

use galvani_extract::{ExtractedBoard, LintCheck};

fn corpus() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous");
    if p.exists() {
        Some(p)
    } else {
        if std::env::var("GALVANI_REQUIRE_CORPUS").is_ok() {
            panic!("GALVANI_REQUIRE_CORPUS=1 but board-corpus/famous not found at {p:?}");
        }
        None
    }
}

fn load(p: &Path) -> ExtractedBoard {
    let text = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
    if p.extension().and_then(|e| e.to_str()) == Some("kicad_sch") {
        ExtractedBoard::from_kicad_schematic_path(p).unwrap_or_else(|e| panic!("extract {p:?}: {e}"))
    } else {
        ExtractedBoard::from_auto(&text).unwrap_or_else(|e| panic!("extract {p:?}: {e}"))
    }
}

fn conflicts(c: &Path, rel: &str) -> Vec<String> {
    let board = load(&c.join(rel));
    board
        .resource_conflicts()
        .of_check(LintCheck::McuResourceConflict)
        .map(|f| f.message.clone())
        .collect()
}

#[test]
fn olimex_rp2040_pico_pc_pwm_slice_6a_conflict_flagged_rev_c_and_d() {
    let Some(c) = corpus() else { return };
    for rev in ["C", "D"] {
        let rel = format!(
            "olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision {rev}/RP2040-PICO-PC_rev_{rev}.net"
        );
        let msgs = conflicts(&c, &rel);
        assert_eq!(
            msgs.len(),
            1,
            "rev {rev}: expected exactly the slice-6A conflict, got {msgs:#?}"
        );
        let m = &msgs[0];
        // The documented evidence chain: slice 6A, GP12 DVI clock vs GP28 audio.
        assert!(m.contains("6A"), "rev {rev}: not slice 6A: {m}");
        assert!(m.contains("GP12") && m.contains("GP28"), "rev {rev}: pins missing: {m}");
        assert!(
            m.contains("PWM audio") && m.contains("PicoDVI PWM pixel clock"),
            "rev {rev}: both functions must be named: {m}"
        );
        assert!(m.contains("HDMI") && m.contains("AUDIO_JACK"), "rev {rev}: targets missing: {m}");
    }
}

#[test]
fn olimex_rp2040_pico_pc_rev_b_is_clean_dvi_clock_on_slice_7() {
    // rev B is the discriminator: the DVI clock is on GP14/GP15 (slice 7), so it
    // does NOT collide with the audio on slice 6A. The check must be silent -
    // this proves the slice-6A finding on rev C/D is real, not an any-RP2040
    // -with-DVI-and-audio false positive.
    let Some(c) = corpus() else { return };
    let msgs = conflicts(
        &c,
        "olimex_rp2040_pico_pc/HARDWARE/RP2040-PICO-PC hardware revision B/RP2040-PICO-PC_rev_B.net",
    );
    assert!(msgs.is_empty(), "rev B must be clean (DVI clock on slice 7), got: {msgs:#?}");
}

#[test]
fn sparkfun_samd51_thing_plus_qspi_flash_conflict_flagged() {
    let Some(c) = corpus() else { return };
    let msgs = conflicts(&c, "sparkfun_thingplus_samd51/Hardware/SAMD51_Thing_Plus.brd");
    assert_eq!(msgs.len(), 1, "expected exactly the QSPI flash conflict, got {msgs:#?}");
    let m = &msgs[0];
    assert!(m.contains("qspi_data"), "must name the QSPI group: {m}");
    assert!(m.contains("SPI flash"), "must name the flash function: {m}");
    // All four QSPI data pins committed (PA08..PA11).
    for port in ["PA08", "PA09", "PA10", "PA11"] {
        assert!(m.contains(port), "must cite {port}: {m}");
    }
    assert!(m.contains("U4"), "must cite the flash chip U4: {m}");
}

#[test]
fn clean_corpus_boards_raise_no_resource_conflict() {
    let Some(c) = corpus() else { return };
    // Known-good boards: RP2040 (minimal + SparkFun Thing Plus), ESP32 (fully
    // routable), and a spread of unrelated designs. None has a genuine internal
    // resource conflict of this class; the check must be silent on every one.
    let clean: &[&str] = &[
        "rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_sch",
        "sparkfun_thingplus_rp2040/Hardware/RP2040_Thing_Plus.brd",
        "olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch",
        "olimex_esp32/HARDWARE/REV-K1/ESP32-EVB_Rev_K1.net",
        "zswatch_mainboard/watch/ZSWatch-Watch.kicad_sch",
        "watchy/Watchy.kicad_sch",
        "lumenpnp/mobo/mobo.kicad_sch",
        "lily58/Pro_V2/Pro_V2.kicad_sch",
        "mnt_reform/reform2-motherboard30-pcb/reform2-motherboard30.kicad_sch",
        "adafruit_feather_m0/Adafruit Feather M0 Basic rev C.brd",
        "sparkfun_redboard/RedBoard.brd",
    ];
    let mut fires = Vec::new();
    for rel in clean {
        let path = c.join(rel);
        if !path.exists() {
            // A board added/renamed since: skip rather than fail the calibration
            // on a path drift (the validation tests above carry the load).
            continue;
        }
        let msgs = conflicts(&c, rel);
        if !msgs.is_empty() {
            fires.push(format!("{rel}: {msgs:#?}"));
        }
    }
    assert!(
        fires.is_empty(),
        "resource-conflict check must be SILENT on known-good boards; it fired on:\n{}",
        fires.join("\n")
    );
}
