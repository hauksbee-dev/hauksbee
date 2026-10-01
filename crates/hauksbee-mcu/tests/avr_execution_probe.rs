//! simavr execution evidence: `main` watched against the PC from the ELF
//! symbol table, and a crashed core reported as a fault.
//!
//! Two-sided: a healthy ELF reaches `main` with no fault; a hand-assembled
//! image that jumps past the end of flash crashes the core and carries no
//! symbol table, so its entry reads unknown rather than a guessed yes or no.

#![cfg(feature = "avr")]

use hauksbee_mcu::{AvrMcu, Mcu};

#[test]
fn healthy_elf_reaches_main_without_a_fault() {
    let Some(elf) = crate::support::firmware("avr_watchdog/nowdt.elf") else {
        return;
    };
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(&elf).expect("load firmware");
    for _ in 0..5 {
        mcu.run_micros(1_000).expect("chunk runs");
    }
    let probe = mcu.execution_probe();
    assert_eq!(probe.reached_entry, Some(true), "{probe:?}");
    assert_eq!(probe.entry_symbol.as_deref(), Some("main"));
    assert!(probe.faults.is_empty(), "{probe:?}");
    assert!(probe.instructions.unwrap_or(0) >= 5 * 16_000, "{probe:?}");
    assert_eq!(probe.pc_samples, 5, "{probe:?}");
}

#[test]
fn jump_past_flash_crashes_and_entry_is_unknown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let hex = dir.path().join("crash.hex");
    // `jmp 0x10000` (word 0x8000): past the 32 KiB flash of an ATmega328P.
    std::fs::write(&hex, ":040000000C940080DC\n:00000001FF\n").expect("write hex");
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(&hex).expect("load firmware");
    mcu.run_micros(1_000).expect("chunk runs");
    let probe = mcu.execution_probe();
    assert_eq!(probe.reached_entry, None, "a .hex has no symbol table");
    assert!(
        probe.faults.iter().any(|f| f.contains("crashed")),
        "{probe:?}"
    );
}
