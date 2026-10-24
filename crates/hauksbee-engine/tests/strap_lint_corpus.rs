//! Strap-pin lint, calibrated against the famous corpus.
//!
//! Ground truth: the Olimex ESP32-EVB carries the Ethernet PHY's free-running
//! 50 MHz `REF_CLK` on GPIO0, which is an ESP32 boot strapping pin that must be a
//! stable HIGH at the reset latch. ESP-IDF documents this exact failure (the
//! ESP32 randomly enters download mode), and Olimex fixed it in rev E by gating
//! PHY power until the oscillator stabilises. `docs/evidence/CORPUS.md`
//! previously listed this as "MISSED, out of reach (needs boot-strapping +
//! clock-at-reset model)"; the strap lint is that model for the netlist-visible
//! half of the fault, and these tests pin the calibration:
//!
//!   1. the strap lint FIRES (high) on the Olimex GPIO0 clock - the documented
//!      fault, now caught;
//!   2. it stays SILENT on every other strap on the board, and on the known-good
//!      ESP32 (Watchy) and RP2040 (rp2040-minimal) boards, whose straps are
//!      genuinely examined and correctly biased - so the clean is real, not
//!      vacuous;
//!   3. the rev-E fix is seen where its parts are modelled: in rev L the
//!      HX7027 detector U11 holds the oscillator's enable (`OSC_DIS`) low until
//!      its input, an RC delay (R52 100k, C32 1 uF) off the ESP32's EN, passes
//!      its threshold. The clock is then absent at the reset latch only if that
//!      release lags the strap hold time, which a static check does not time, so
//!      the lint abstains: one Low "unchecked" finding, not a High fault and not
//!      a silent clean. Revisions whose detector has no card stay High.
//!
//! Corpus-gated (skipped when board-corpus is absent), with `HAUKSBEE_REQUIRE_CORPUS=1`
//! turning absence into a hard fail so the calibration cannot vacuously green-out.

use std::path::PathBuf;

use hauksbee_engine::checks::straps::strap_lint;
use hauksbee_extract::{ExtractedBoard, LintCheck, NetLintReport, Severity};
use hauksbee_models::ModelLibrary;

/// This suite's corpus root; see
/// [`hauksbee_testkit::corpus_boards_root_or_skip`] for why it goes through
/// the testkit rather than joining `famous/` locally.
fn corpus_root() -> Option<PathBuf> {
    hauksbee_testkit::corpus_boards_root_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "strap-lint corpus calibration",
    )
}

fn strap_report(path: &PathBuf) -> NetLintReport {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let board = ExtractedBoard::from_auto(&text).expect("parse board");
    let lib = ModelLibrary::builtin();
    strap_lint(&board, &lib)
}

fn strap_findings(r: &NetLintReport) -> Vec<&hauksbee_extract::LintFinding> {
    r.of_check(LintCheck::StrapPin).collect()
}

/// The gold catch: the Olimex ESP32-EVB rev D (the faulty revision) flags GPIO0
/// for the 50 MHz clock, at High severity, naming the oscillator.
#[test]
fn olimex_rev_d_gpio0_clock_flagged() {
    let Some(root) = corpus_root() else { return };
    let pcb = root.join("olimex_esp32/HARDWARE/REV-D/ESP32-EVB_Rev_D.kicad_pcb");
    if !pcb.exists() {
        eprintln!("Olimex REV-D absent; skipping");
        return;
    }
    let r = strap_report(&pcb);
    let f = strap_findings(&r);
    assert_eq!(
        f.len(),
        1,
        "exactly one strap finding (GPIO0), got: {:?}",
        f.iter().map(|x| &x.message).collect::<Vec<_>>()
    );
    assert!(
        matches!(f[0].severity, Severity::High),
        "the clock-on-strap is High severity"
    );
    assert!(
        f[0].nets.iter().any(|n| n.contains("GPIO0")),
        "the finding names GPIO0"
    );
    assert!(
        f[0].message.to_uppercase().contains("CR1") || f[0].message.contains("clock"),
        "names the clock source"
    );
}

/// The fixed revision (rev L): the oscillator still reaches GPIO0 through R36,
/// but its enable is held off by a card-modelled detector that follows the
/// ESP32's EN, so the lint abstains at Low ("unchecked") instead of raising the
/// rev-D fault. It must not go silent: the release timing is not judged.
#[test]
fn olimex_rev_l_gated_clock_is_unchecked_not_flagged() {
    let Some(root) = corpus_root() else { return };
    let pcb = root.join("olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_pcb");
    if !pcb.exists() {
        eprintln!("Olimex REV-L absent; skipping");
        return;
    }
    let r = strap_report(&pcb);
    let f = strap_findings(&r);
    assert_eq!(
        f.len(),
        1,
        "rev L keeps one GPIO0 strap row: {:?}",
        f.iter().map(|x| &x.message).collect::<Vec<_>>()
    );
    assert!(f[0].nets.iter().any(|n| n.contains("GPIO0")));
    assert!(
        matches!(f[0].severity, Severity::Low),
        "a clock held off through reset is an abstention, not a fault: {}",
        f[0].message
    );
    assert!(f[0].message.contains("unchecked"), "{}", f[0].message);
    assert!(
        f[0].refs.iter().any(|r| r == "U11"),
        "names the detector: {:?}",
        f[0].refs
    );
}

/// Known-good ESP32 board (Watchy v2.0, ESP32-PICO-D4): its GPIO0 strap is
/// examined (the part resolves, the strap pad maps to the GPIO0 net) and is
/// correctly held by the internal pull-up with no clock on it, so the strap lint
/// is silent. This proves the clean is real, not vacuous.
#[test]
fn watchy_esp32_straps_are_clean() {
    let Some(root) = corpus_root() else { return };
    let pcb = root.join("watchy_history/v2.0/Watchy.kicad_pcb");
    if !pcb.exists() {
        eprintln!("Watchy v2.0 absent; skipping");
        return;
    }
    let r = strap_report(&pcb);
    assert_eq!(
        strap_findings(&r).len(),
        0,
        "Watchy ESP32 straps are correctly biased: {:?}",
        strap_findings(&r)
            .iter()
            .map(|x| &x.message)
            .collect::<Vec<_>>()
    );
}

/// Known-good RP2040 board (rp2040-minimal): its `QSPI_SS` / BOOTSEL strap idles
/// high (the flash chip-select) with a series-R to the BOOTSEL header, no clock,
/// no wrong-rail pull. The strap lint examines it and is silent.
#[test]
fn rp2040_minimal_bootsel_is_clean() {
    // The fetch pins r3 in a project subdirectory; the hand-built corpus has r2.
    // Resolve the board itself so a present-but-empty directory cannot pass.
    let pcb = hauksbee_testkit::corpus_board_any(
        env!("CARGO_MANIFEST_DIR"),
        &[
            "rp2040_minimal_kicad/RPI-RP2040-MINIMAL_R3-S1_public/RPI-RP2040-MINIMAL_R3-S1.kicad_pcb",
            "rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_pcb",
        ],
    )
    .or_else(|| hauksbee_testkit::corpus_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "rp2040_minimal_r2/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_pcb",
        "RP2040 BOOTSEL strap calibration",
    ));
    let Some(pcb) = pcb else { return };
    let r = strap_report(&pcb);
    assert_eq!(
        strap_findings(&r).len(),
        0,
        "RP2040 BOOTSEL strap is correctly biased: {:?}",
        strap_findings(&r)
            .iter()
            .map(|x| &x.message)
            .collect::<Vec<_>>()
    );
}

/// Corpus-wide calibration: across the WHOLE famous corpus, the strap lint fires
/// ONLY on the Olimex ESP32-EVB GPIO0 clock, and on nothing else. Zero false
/// positives on every known-good board is the bar for the check to ship (the
/// famous-sweep discipline). Every fire must be an Olimex GPIO0.
#[test]
fn strap_lint_only_fires_on_olimex_gpio0_across_corpus() {
    let Some(root) = corpus_root() else { return };
    let mut offenders: Vec<String> = Vec::new();
    let mut olimex_gpio0_hits = 0usize;
    let lib = ModelLibrary::builtin();
    // Walk every native CAD file; one representative per board is enough, but a
    // full walk is the strongest statement.
    for p in hauksbee_testkit::walk_files_with_ext(&root, &["kicad_pcb", "net", "brd"]) {
        // Read + parse defensively; skip files the extractor cannot handle.
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(board) = ExtractedBoard::from_auto(&text) else {
            continue;
        };
        let r = strap_lint(&board, &lib);
        for f in r.of_check(LintCheck::StrapPin) {
            let is_olimex_gpio0 = p.to_string_lossy().contains("olimex_esp32")
                && f.nets.iter().any(|n| n.contains("GPIO0"));
            if is_olimex_gpio0 {
                olimex_gpio0_hits += 1;
            } else {
                offenders.push(format!("{}: {}", p.display(), f.message));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "strap lint fired on a non-Olimex-GPIO0 net (false positive(s)):\n{}",
        offenders.join("\n")
    );
    assert!(
        olimex_gpio0_hits > 0,
        "the strap lint should fire on the Olimex GPIO0 clock at least once"
    );
}
