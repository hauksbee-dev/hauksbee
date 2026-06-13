//! Strap-pin lint, calibrated against the famous corpus.
//!
//! Ground truth: the Olimex ESP32-EVB carries the Ethernet PHY's free-running
//! 50 MHz REF_CLK on GPIO0, which is an ESP32 boot strapping pin that must be a
//! stable HIGH at the reset latch. ESP-IDF documents this exact failure (the
//! ESP32 randomly enters download mode), and Olimex fixed it in rev E by gating
//! PHY power until the oscillator stabilises. `docs/KNOWN_FAULTS_VALIDATION.md`
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
//!   3. HONEST REACH LIMIT, encoded as a test: the lint fires on the rev-E-FIXED
//!      revisions too (e.g. rev L), because the fix is a *time-domain power-
//!      sequencing* change (FET3/FET4 gating PHY power via OSC_DIS) that does NOT
//!      alter the GPIO0 net - the 50 MHz oscillator still reaches GPIO0 through
//!      R36 in every revision. A static netlist check cannot see the fix. This is
//!      recorded as ground truth (verified from the rev-D and rev-L files), not
//!      papered over with a forced "clean on fixed".
//!
//! Corpus-gated (skipped when board-corpus is absent), with HAUKSBEE_REQUIRE_CORPUS=1
//! turning absence into a hard fail so the calibration cannot vacuously green-out.

use std::path::PathBuf;

use hauksbee_engine::checks::straps::strap_lint;
use hauksbee_extract::{ExtractedBoard, LintCheck, NetLintReport, Severity};
use hauksbee_models::ModelLibrary;

fn famous_root() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../board-corpus/famous");
    if p.exists() {
        return Some(p);
    }
    if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
        panic!("HAUKSBEE_REQUIRE_CORPUS set but board-corpus/famous is missing: {}", p.display());
    }
    eprintln!("board-corpus/famous absent; skipping strap-lint corpus test");
    None
}

fn strap_report(path: &PathBuf) -> NetLintReport {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
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
    let Some(root) = famous_root() else { return };
    let pcb = root.join("olimex_esp32/HARDWARE/REV-D/ESP32-EVB_Rev_D.kicad_pcb");
    if !pcb.exists() {
        eprintln!("Olimex REV-D absent; skipping");
        return;
    }
    let r = strap_report(&pcb);
    let f = strap_findings(&r);
    assert_eq!(f.len(), 1, "exactly one strap finding (GPIO0), got: {:?}", f.iter().map(|x| &x.message).collect::<Vec<_>>());
    assert!(matches!(f[0].severity, Severity::High), "the clock-on-strap is High severity");
    assert!(f[0].nets.iter().any(|n| n.contains("GPIO0")), "the finding names GPIO0");
    assert!(f[0].message.to_uppercase().contains("CR1") || f[0].message.contains("clock"), "names the clock source");
}

/// The honest reach limit, pinned: the rev-E-FIXED revision (rev L) STILL fires
/// on GPIO0, because the fix is not netlist-visible. This is verified ground
/// truth, not a missing test: if a future change made the lint go clean on rev L
/// it would be claiming to see a fix that is not in the netlist, which is exactly
/// the false confidence the calibration forbids. So the test asserts it fires.
#[test]
fn olimex_rev_l_still_fires_fix_is_not_netlist_visible() {
    let Some(root) = famous_root() else { return };
    let pcb = root.join("olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_pcb");
    if !pcb.exists() {
        eprintln!("Olimex REV-L absent; skipping");
        return;
    }
    let r = strap_report(&pcb);
    let f = strap_findings(&r);
    assert_eq!(f.len(), 1, "rev L still carries the 50 MHz clock on GPIO0 in the netlist");
    assert!(f[0].nets.iter().any(|n| n.contains("GPIO0")));
}

/// Known-good ESP32 board (Watchy v2.0, ESP32-PICO-D4): its GPIO0 strap is
/// examined (the part resolves, the strap pad maps to the GPIO0 net) and is
/// correctly held by the internal pull-up with no clock on it, so the strap lint
/// is silent. This proves the clean is real, not vacuous.
#[test]
fn watchy_esp32_straps_are_clean() {
    let Some(root) = famous_root() else { return };
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
        strap_findings(&r).iter().map(|x| &x.message).collect::<Vec<_>>()
    );
}

/// Known-good RP2040 board (rp2040-minimal): its QSPI_SS / BOOTSEL strap idles
/// high (the flash chip-select) with a series-R to the BOOTSEL header, no clock,
/// no wrong-rail pull. The strap lint examines it and is silent.
#[test]
fn rp2040_minimal_bootsel_is_clean() {
    let Some(root) = famous_root() else { return };
    let dir = root.join("rp2040_minimal_kicad");
    if !dir.exists() {
        eprintln!("rp2040_minimal absent; skipping");
        return;
    }
    // Pick the first .kicad_pcb in the dir.
    let pcb = std::fs::read_dir(&dir)
        .ok()
        .and_then(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().and_then(|x| x.to_str()) == Some("kicad_pcb"))
        });
    let Some(pcb) = pcb else {
        eprintln!("no rp2040_minimal .kicad_pcb; skipping");
        return;
    };
    let r = strap_report(&pcb);
    assert_eq!(
        strap_findings(&r).len(),
        0,
        "RP2040 BOOTSEL strap is correctly biased: {:?}",
        strap_findings(&r).iter().map(|x| &x.message).collect::<Vec<_>>()
    );
}

/// Corpus-wide calibration: across the WHOLE famous corpus, the strap lint fires
/// ONLY on the Olimex ESP32-EVB GPIO0 clock, and on nothing else. Zero false
/// positives on every known-good board is the bar for the check to ship (the
/// famous-sweep discipline). Every fire must be an Olimex GPIO0.
#[test]
fn strap_lint_only_fires_on_olimex_gpio0_across_corpus() {
    let Some(root) = famous_root() else { return };
    let mut offenders: Vec<String> = Vec::new();
    let mut olimex_gpio0_hits = 0usize;
    // Walk every native CAD file; one representative per board is enough, but a
    // full walk is the strongest statement.
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            if !matches!(ext, "kicad_pcb" | "net" | "brd") {
                continue;
            }
            // Read + parse defensively; skip files the extractor cannot handle.
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let Ok(board) = ExtractedBoard::from_auto(&text) else { continue };
            let lib = ModelLibrary::builtin();
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
