//! Boot-coverage assertion: the formerly-rejected "Hi-Z control input" class,
//! made decidable by running the firmware.
//!
//! `docs/KNOWN_FAULTS_VALIDATION.md` records two faults (Watchy e-paper RES#,
//! ZSWatch DISPLAY-EN) as honest misses: a control net is driven only by an MCU
//! GPIO that goes Hi-Z at reset, so its power-up default is undefined, and the
//! netlist *cannot encode the load's intended default* (a display that must be
//! on by default looks byte-identical to a haptic motor that must be off). A
//! static check firing there would be a confident false positive on a shipped
//! board.
//!
//! The `boot-coverage` assertion turns "we cannot know the intended default"
//! into "watch what the firmware actually does": run the co-sim from reset and
//! require the MCU to actively drive the control net to a defined level within a
//! deadline, with no stress fault during the boot window before it.
//!
//! Two proofs run here. First, a constructed two-sided proof on a synthetic
//! board with two real AVR firmware variants: variant A drives a floating
//! MOSFET gate promptly (PASS), variant B never touches it (FAIL, naming the
//! net). Second, the real Watchy v1.5 board under the Espressif QEMU ESP32
//! backend (the e-paper RES# net, GPIO9), also two-sided. The mechanism is
//! backend-agnostic across simavr (AVR), Renode (STM32/nRF/RISC-V) and the
//! Espressif QEMU fork (ESP32/ESP32-C3); see docs/MCU.md for the matrix. The
//! ESP32 tests skip cleanly when the deterministic fork is absent.

use std::path::PathBuf;

use hauksbee_ci::{run, RunConfig};

fn example(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples").join(name)
}

/// The firmware hex files are committed, but rebuild them from source if a
/// toolchain is present so the test tracks the .c, not a stale artifact. Absence
/// of avr-gcc is fine - the committed hex is used.
fn ensure_firmware(variant: &str) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware")
        .join(format!("boot_gate_{variant}"));
    let hex = dir.join("boot_gate.hex");
    if hex.exists() {
        // Try to refresh if avr-gcc is available; ignore failure.
        if which_avr_gcc() {
            let _ = std::process::Command::new("make").current_dir(&dir).output();
        }
        return;
    }
    assert!(
        which_avr_gcc(),
        "firmware hex missing and avr-gcc not on PATH: cannot build {variant}"
    );
    let out = std::process::Command::new("make")
        .current_dir(&dir)
        .output()
        .expect("run make");
    assert!(out.status.success(), "building firmware {variant} failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn which_avr_gcc() -> bool {
    std::process::Command::new("avr-gcc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Variant A drives the gate promptly: the boot-coverage assertion PASSES, and
/// so does the no_faults assertion (boot window clean).
#[test]
fn gate_driven_promptly_passes() {
    ensure_firmware("a");
    let result = run(&RunConfig { spec: example("boot_gate_pass.toml") })
        .expect("PASS spec runs");
    assert!(
        result.passed(),
        "variant A drives the gate within the deadline, so all assertions pass:\n{}",
        result.render_human()
    );
    // The boot-coverage assertion is present and green.
    let bc = result
        .results
        .iter()
        .find(|r| r.kind == "boot-coverage")
        .expect("boot-coverage assertion present");
    assert!(bc.passed, "boot-coverage must pass: {}", bc.detail);
    assert!(bc.detail.contains("GATE_CTRL"), "names the control net: {}", bc.detail);
}

/// Variant B never drives the gate: the boot-coverage assertion FAILS, naming
/// the control net that was left Hi-Z. This is the discriminating half - the
/// check has teeth only because this case goes RED.
#[test]
fn gate_left_floating_fails_naming_the_net() {
    ensure_firmware("b");
    let result = run(&RunConfig { spec: example("boot_gate_fail.toml") })
        .expect("FAIL spec runs");
    assert!(
        !result.passed(),
        "variant B never drives the gate, so boot-coverage must FAIL:\n{}",
        result.render_human()
    );
    let bc = result
        .results
        .iter()
        .find(|r| r.kind == "boot-coverage")
        .expect("boot-coverage assertion present");
    assert!(!bc.passed, "boot-coverage must fail for the never-driven gate");
    assert!(
        bc.detail.contains("GATE_CTRL") && bc.detail.to_uppercase().contains("NEVER"),
        "the failure names the undriven control net: {}",
        bc.detail
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Watchy v1.5 e-paper RES# boot-coverage on the REAL board, ESP32 QEMU backend.
//
// This executes the formerly-MISSED Watchy display-RES# validation row
// (docs/KNOWN_FAULTS_VALIDATION.md) end-to-end: the unmodified corpus Watchy
// v1.5 layout, its RES net (no pull in v1.5, U1 ESP32-PICO-D4 pad 28 = GPIO9),
// the Espressif QEMU esp32 machine booting a reduced display-init firmware that
// drives GPIO9 HIGH. Two-sided: the display-init firmware PASSES (drives RES in
// time), the esp32_blinky firmware (drives GPIO2/4, never GPIO9) FAILS.
//
// Gated twice: skip when the corpus is absent (hard-fail under
// HAUKSBEE_REQUIRE_CORPUS=1), and skip when the Espressif QEMU binary is not
// installed (QEMU is an environmental dependency, not part of the corpus, so its
// absence is always a skip - the proven backend tests gate the same way).

fn watchy_v15_board() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../board-corpus/famous/watchy_history/v1.5/Watchy.kicad_pcb");
    p.exists().then_some(p)
}

fn qemu_xtensa_available() -> bool {
    // Reuse the backend's real discovery so this gate agrees exactly with what
    // the co-sim will actually do. is_available runs the candidate's
    // `-machine help` (is_esp_fork) and only returns true for the Espressif
    // fork that advertises an esp32 machine. A bare path check is not enough:
    // a stock Homebrew qemu-system-xtensa exists on PATH and at an env-pointed
    // path but cannot boot an ESP32 image, so a path check would let the heavy
    // test run and FAIL instead of skipping. This makes the skip decision
    // identical to the backend's accept/reject decision.
    hauksbee_mcu::qemu::is_available(hauksbee_mcu::qemu::QemuArch::Xtensa)
}

fn corpus_or_skip(what: &str) -> Option<PathBuf> {
    match watchy_v15_board() {
        Some(p) => Some(p),
        None => {
            if std::env::var("HAUKSBEE_REQUIRE_CORPUS").is_ok() {
                panic!("corpus required but Watchy v1.5 board missing ({what})");
            }
            eprintln!("skipping {what} (corpus absent)");
            None
        }
    }
}

/// PASS: the reduced display-init firmware drives the Watchy e-paper RES# net
/// (GPIO9) HIGH within the boot window on the real v1.5 board under QEMU.
#[test]
fn watchy_v15_display_res_driven_passes() {
    if corpus_or_skip("watchy_v15_display_res PASS").is_none() {
        return;
    }
    if !qemu_xtensa_available() {
        eprintln!("skipping watchy_v15 boot-coverage (Espressif QEMU not installed)");
        return;
    }
    let t0 = std::time::Instant::now();
    let result = run(&RunConfig { spec: example("watchy_v15_display_res.toml") })
        .expect("Watchy PASS spec runs");
    let wall = t0.elapsed();
    let bc = result
        .results
        .iter()
        .find(|r| r.kind == "boot-coverage")
        .expect("boot-coverage assertion present");
    assert!(
        bc.passed,
        "display-init firmware must drive RES in time on the real Watchy board:\n{}",
        result.render_human()
    );
    assert!(bc.detail.contains("RES"), "names the RES net: {}", bc.detail);
    // Regression guard for the external-backend chunk coarsening
    // (Scheduler::has_external_backend -> runner sets chunk_s ~ frame size).
    // Without it, this 800 ms / 8 ms-frame co-sim sub-divides into ~80 QMP
    // cont/stop pairs per frame at the 100 us AVR-default chunk and takes over
    // ten MINUTES of wall time; with it, ~3.5 s. A generous 120 s ceiling
    // tolerates slow CI machines while still failing hard if the coarsening is
    // ever lost (the regression is a ~200x slowdown, not a marginal one).
    assert!(
        wall.as_secs() < 120,
        "Watchy QEMU co-sim took {wall:?}; the external-backend chunk \
         coarsening in hauksbee-ci's runner has likely regressed"
    );
}

/// FAIL (negative control): firmware that drives GPIO2/4 but never GPIO9 leaves
/// the RES net Hi-Z on the same board, so boot-coverage goes RED. This is the
/// teeth - the check passes above only because it genuinely watches the firmware
/// drive the net.
#[test]
fn watchy_v15_display_res_undriven_fails() {
    if corpus_or_skip("watchy_v15_display_res FAIL").is_none() {
        return;
    }
    if !qemu_xtensa_available() {
        eprintln!("skipping watchy_v15 boot-coverage negative (Espressif QEMU not installed)");
        return;
    }
    let result = run(&RunConfig { spec: example("watchy_v15_display_res_undriven.toml") })
        .expect("Watchy FAIL spec runs");
    let bc = result
        .results
        .iter()
        .find(|r| r.kind == "boot-coverage")
        .expect("boot-coverage assertion present");
    assert!(
        !bc.passed,
        "a firmware that never drives RES must make boot-coverage FAIL:\n{}",
        result.render_human()
    );
    assert!(
        bc.detail.contains("RES") && bc.detail.to_uppercase().contains("NEVER"),
        "the failure names the undriven RES net: {}",
        bc.detail
    );
}
