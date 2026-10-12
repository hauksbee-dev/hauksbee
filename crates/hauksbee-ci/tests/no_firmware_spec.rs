//! A spec that names no firmware never starts the MCU, so an assertion that
//! rests on the firmware having run (a UART line, a pin toggle on an MCU pin)
//! is NOT EXERCISED: INVALID, exit 3, with the fix stated. It used to read
//! "FAIL ... evidence: clean" and exit 1, which sent the reader to debug
//! firmware they had never loaded. The same run with a firmware keeps its
//! ordinary verdict, and a missing emulator is an INVALID with the install
//! command rather than an "invalid spec".

use crate::support::{bin, repo_root, stderr};
#[cfg(feature = "avr")]
use crate::support::{blinky_board, stdout};
use std::process::Command;
#[cfg(feature = "avr")]
use std::process::Output;

/// The bundled blinky spec's board, supply and assertions, with `firmware`
/// optionally present, run from a scratch directory.
#[cfg(feature = "avr")]
fn run_blinky(firmware: Option<&str>, asserts: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(blinky_board(), dir.path().join("blinky.kicad_pcb")).unwrap();
    let firmware = firmware.map_or_else(String::new, |f| format!("firmware = \"{f}\"\n"));
    let body = format!(
        "name = \"blinky power-up sanity\"\nboard = \"blinky.kicad_pcb\"\n{firmware}\
         mcu = \"atmega328p\"\nduration_ms = 1000\n\n\
         [[supply]]\nnet = \"+5V\"\nkind = \"usb\"\nusb = \"5v0.5a\"\n\n{asserts}"
    );
    let spec = dir.path().join("blinky.toml");
    std::fs::write(&spec, body).unwrap();
    Command::new(bin())
        .arg("run")
        .arg(&spec)
        .current_dir(dir.path())
        .output()
        .expect("binary runs")
}

#[cfg(feature = "avr")]
const ASSERTS: &str = r#"
[[assert]]
kind = "voltage"
net = "+5V"
min = 4.75

[[assert]]
kind = "uart"
contains = "hauksbee-demo v1"

[[assert]]
kind = "toggle"
net = "D13"
freq_hz = 5.0
tolerance = 0.4
"#;

#[cfg(feature = "avr")]
#[test]
fn firmware_assertions_without_firmware_are_invalid_with_the_fix_stated() {
    let out = run_blinky(None, ASSERTS);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(3), "{text}\n{}", stderr(&out));
    // Not a verdict on any firmware: neither assertion is a FAIL.
    assert!(!text.contains("[FAIL]"), "{text}");
    assert!(!text.contains("evidence: clean\n        why:"), "{text}");
    // The fix is stated where the reader looks, once per assertion.
    for want in [
        "the MCU never ran: this spec has no firmware",
        "firmware = \"path/to/app.elf\"",
        "drop the UART/toggle asserts",
    ] {
        assert!(text.contains(want), "missing {want:?}:\n{text}");
    }
    // The rail assertion needs no firmware and keeps its pass.
    assert!(text.contains("[PASS] +5V >= 4.75 V"), "{text}");
    assert_eq!(text.matches("INVALID: not exercised").count(), 2, "{text}");
}

#[cfg(feature = "avr")]
#[test]
fn a_toggle_on_a_net_no_mcu_pin_can_move_is_still_an_ordinary_verdict() {
    // +5V is a declared supply: no firmware can move it, so "it does not
    // toggle" is a real reading of the circuit, not an idle MCU.
    let out = run_blinky(
        None,
        "\n[[assert]]\nkind = \"toggle\"\nnet = \"+5V\"\nfreq_hz = 5.0\ntolerance = 0.4\n",
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}\n{}", stderr(&out));
    assert!(!text.contains("INVALID"), "{text}");
}

#[cfg(feature = "avr")]
#[test]
fn the_same_spec_with_its_firmware_is_unchanged() {
    let hex = repo_root().join("testdata/firmware/demo/demo.hex");
    if !hex.exists() {
        eprintln!("SKIP: {} is not built", hex.display());
        return;
    }
    let out = run_blinky(Some(hex.to_str().unwrap()), ASSERTS);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}\n{}", stderr(&out));
    assert!(!text.contains("INVALID"), "{text}");
    assert!(text.contains("[PASS] UART contains"), "{text}");
}

#[cfg(feature = "renode")]
#[test]
fn a_missing_emulator_is_invalid_with_the_install_command_not_an_invalid_spec() {
    let spec = repo_root().join("crates/hauksbee-ci/examples/lm75_thermostat_cold.toml");
    let out = Command::new(bin())
        .arg("run")
        .arg(&spec)
        .output()
        .expect("binary runs");
    let err = stderr(&out);
    if !err.contains("Renode not found.") {
        eprintln!("SKIP: Renode is installed here, so nothing is missing");
        return;
    }
    assert_eq!(out.status.code(), Some(3), "{err}");
    assert!(!err.contains("invalid spec"), "{err}");
    assert!(err.contains("INVALID"), "{err}");
    assert!(err.contains("hauksbee install renode"), "{err}");
}
