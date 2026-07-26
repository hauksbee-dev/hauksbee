//! Firmware-architecture gate (fix #10): a wrong-ISA image must be refused at
//! load time with a clear two-sided message, BEFORE the backend runs it as
//! garbage. Uses the real testdata ELFs so the e_machine values are genuine,
//! not synthetic.
//!
//! Background: docs/hunts/personas/persona-esp32-iot.md loaded an Xtensa ESP32
//! image onto a RISC-V ESP32-C3 board and got ~136 MB of UART garbage
//! ("invalid header: 0xffffffff") with no error. These tests lock that shut.

use hauksbee_mcu::elf::{self, EM_ARM, EM_AVR, EM_RISCV, EM_XTENSA};
use std::path::{Path, PathBuf};

/// Resolve a path under the workspace `testdata/firmware/` directory.
fn fw(rel: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/hauksbee-mcu; testdata lives at the repo root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    root.join("testdata/firmware").join(rel)
}

#[test]
fn real_testdata_elfs_report_expected_machines() {
    // These are the genuine e_machine values read from the committed firmware.
    let cases = [
        ("esp32_blinky/esp32_blinky.elf", EM_XTENSA),
        ("esp32_blinky/esp32c3_blinky.elf", EM_RISCV),
        ("stm32_blinky/blinky.elf", EM_ARM),
        ("renode_demos/zephyr-fe310-shell.elf", EM_RISCV),
        ("renode_demos/nrf52840-zephyr_shell.elf", EM_ARM),
        ("demo/demo.elf", EM_AVR),
    ];
    for (rel, expected) in cases {
        let p = fw(rel);
        if !p.exists() {
            continue; // testdata not present in this checkout
        }
        assert_eq!(
            elf::read_e_machine(&p).unwrap(),
            Some(expected),
            "e_machine for {rel}"
        );
    }
}

#[test]
fn matched_arch_loads_fine_avr_arm_riscv_xtensa() {
    // Each real ELF against its correct backend ISA: must be accepted.
    let ok = [
        ("demo/demo.elf", EM_AVR, "atmega (AVR)"),
        ("stm32_blinky/blinky.elf", EM_ARM, "STM32"),
        ("renode_demos/zephyr-fe310-shell.elf", EM_RISCV, "FE310"),
        ("esp32_blinky/esp32_blinky.elf", EM_XTENSA, "ESP32"),
        ("esp32_blinky/esp32c3_blinky.elf", EM_RISCV, "ESP32-C3"),
    ];
    for (rel, expected, label) in ok {
        let p = fw(rel);
        if !p.exists() {
            continue;
        }
        assert!(
            elf::validate_arch(&p, expected, label).is_ok(),
            "expected {rel} to be accepted as {label}"
        );
    }
}

#[test]
fn the_persona_bug_is_refused_xtensa_image_on_riscv_board() {
    // Exactly the persona scenario: the Xtensa ESP32 app ELF aimed at the
    // RISC-V ESP32-C3 (e_machine RISC-V = 0xF3).
    let p = fw("esp32_blinky/esp32_blinky.elf");
    if !p.exists() {
        eprintln!("skipping: testdata esp32_blinky.elf absent");
        return;
    }
    let err = elf::validate_arch(&p, EM_RISCV, "ESP32-C3 (RISC-V RV32IMC)")
        .expect_err("an Xtensa image on a RISC-V board must be REFUSED");
    let msg = format!("{err}");
    // Names both sides and both e_machine numbers.
    assert!(msg.contains("Xtensa"), "msg: {msg}");
    assert!(msg.contains("0x5E"), "msg: {msg}");
    assert!(msg.contains("ESP32-C3"), "msg: {msg}");
    assert!(msg.contains("RISC-V"), "msg: {msg}");
    assert!(msg.contains("0xF3"), "msg: {msg}");
    eprintln!("refusal message: {msg}");
}

#[test]
fn arm_image_on_avr_backend_is_refused() {
    let p = fw("stm32_blinky/blinky.elf");
    if !p.exists() {
        return;
    }
    let err = elf::validate_arch(&p, EM_AVR, "atmega (AVR)")
        .expect_err("an ARM image on an AVR backend must be REFUSED");
    let msg = format!("{err}");
    assert!(msg.contains("ARM"), "msg: {msg}");
    assert!(msg.contains("AVR"), "msg: {msg}");
}

#[test]
fn raw_merged_flash_bin_is_unchecked_not_false_errored() {
    // The real esp32 merged flash image is raw (starts 0xffffffff): no ELF
    // header, so the gate must SKIP it (Ok), never false-error.
    let p = fw("esp32_blinky/flash.bin");
    if !p.exists() {
        return;
    }
    assert_eq!(elf::read_e_machine(&p).unwrap(), None);
    assert!(elf::validate_arch(&p, EM_RISCV, "ESP32-C3").is_ok());
}

/// End-to-end through the real QEMU backend constructor: a sibling Xtensa ELF
/// beside a raw flash image must make `QemuBackend::new` for the RISC-V
/// ESP32-C3 bail at the arch gate BEFORE any QEMU process is spawned, even on
/// a machine where Espressif QEMU is not installed.
#[cfg(feature = "qemu")]
#[test]
fn qemu_backend_refuses_xtensa_sibling_on_riscv_board_before_spawn() {
    use hauksbee_mcu::{QemuBackend, QemuConfig};

    let src_bin = fw("esp32_blinky/flash.bin"); // raw Xtensa merged image
    let src_elf = fw("esp32_blinky/esp32_blinky.elf"); // Xtensa app ELF (sibling)
    if !src_bin.exists() || !src_elf.exists() {
        eprintln!("skipping: esp32_blinky testdata absent");
        return;
    }

    // Stage a temp dir mirroring the esp-idf build layout: raw flash.bin beside
    // the Xtensa app ELF. Point the RISC-V ESP32-C3 backend at it.
    let dir = std::env::temp_dir().join(format!("hauksbee-archgate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("flash.bin");
    let elf_path = dir.join("esp32_blinky.elf");
    std::fs::copy(&src_bin, &bin).unwrap();
    std::fs::copy(&src_elf, &elf_path).unwrap();

    let res = QemuBackend::new(QemuConfig::esp32c3(), &bin);

    std::fs::remove_dir_all(&dir).ok();

    let err = res
        .err()
        .expect("QemuBackend::new must refuse an Xtensa sibling ELF on the RISC-V ESP32-C3 board");
    let msg = format!("{err:#}");
    assert!(msg.contains("Xtensa"), "msg: {msg}");
    assert!(msg.contains("ESP32-C3"), "msg: {msg}");
    assert!(msg.contains("RISC-V"), "msg: {msg}");
    eprintln!("QEMU-backend refusal: {msg}");
}

/// The matched case must NOT trip the gate at construction: a RISC-V sibling ELF
/// beside the C3 flash image passes the arch check. (It may still fail later for
/// the unrelated reason that Espressif QEMU is not installed; we only assert the
/// failure, if any, is NOT an arch mismatch.)
#[cfg(feature = "qemu")]
#[test]
fn qemu_backend_accepts_riscv_sibling_on_riscv_board_at_gate() {
    use hauksbee_mcu::{QemuBackend, QemuConfig};

    let src_bin = fw("esp32_blinky/flash_c3.bin");
    let src_elf = fw("esp32_blinky/esp32c3_blinky.elf");
    if !src_bin.exists() || !src_elf.exists() {
        eprintln!("skipping: esp32c3 testdata absent");
        return;
    }
    let dir = std::env::temp_dir().join(format!("hauksbee-archgate-ok-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("flash_c3.bin");
    let elf_path = dir.join("esp32c3_blinky.elf");
    std::fs::copy(&src_bin, &bin).unwrap();
    std::fs::copy(&src_elf, &elf_path).unwrap();

    let res = QemuBackend::new(QemuConfig::esp32c3(), &bin);

    std::fs::remove_dir_all(&dir).ok();

    // Whatever happens, it must not be an architecture-mismatch refusal.
    if let Err(e) = res {
        let msg = format!("{e:#}");
        assert!(
            !msg.contains("architecture mismatch"),
            "matched RISC-V sibling must pass the arch gate, got: {msg}"
        );
    }
}
