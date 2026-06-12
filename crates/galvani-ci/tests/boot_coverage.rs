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
//! This is a constructed, two-sided proof on the same board with two real AVR
//! firmware variants: variant A drives a floating MOSFET gate promptly (PASS),
//! variant B never touches it (FAIL, naming the net). The AVR backend is one of
//! the two co-sim backends (simavr); the mechanism is backend-agnostic, so it is
//! ready for the STM32 (Renode) backend too. The boards in the corpus that carry
//! the real misses (Watchy ESP32, ZSWatch nRF52) are NOT yet co-simmable (no
//! ESP32 / nRF backend, see docs/MCU.md), so this proof uses a supported MCU and
//! the doc records the mechanism as ready-for-backends rather than flipping those
//! verdicts.

use std::path::PathBuf;

use galvani_ci::{run, RunConfig};

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
