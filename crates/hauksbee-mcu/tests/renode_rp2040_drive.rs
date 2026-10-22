//! The RP2040 pad drive-strength field, read back from a running Renode
//! machine. The scheduler tunes a pin's output resistance from this field
//! (`Mcu::read_register`), so the vendored PADS model has to keep what the
//! firmware wrote and show the datasheet's reset value when it wrote nothing.
//!
//! `PADS_BANK0` GPIO25 is at `0x4001c068`, DRIVE is bits 5:4 and resets to 0x1
//! (4 mA), RP2040 datasheet Table 341.
//!
//! The fixtures carry the pico-sdk second stage because the boot ROM only hands
//! over to flash code whose first 256 bytes pass its CRC check.

use hauksbee_mcu::{Mcu, PinId, RenodeBackend, RenodeConfig};
use std::sync::{Arc, Mutex};

const PADS_GPIO25: u32 = 0x4001_c068;
const LED_PIN: PinId = PinId { port: '0', bit: 25 };

const fn drive_field(word: u32) -> u32 {
    (word >> 4) & 3
}

/// Boot `dir/elf`, run 100 ms, and return the DRIVE field and whether GP25 rose.
fn boot_and_read(dir: &str, elf: &str) -> Option<(u32, bool)> {
    if !crate::support::renode_or_skip() {
        return None;
    }
    let Some(path) = crate::support::firmware(&format!("{dir}/{elf}")) else {
        eprintln!("SKIP: {dir}/{elf} not present");
        return None;
    };
    let mut mcu = RenodeBackend::new(RenodeConfig::rp2040()).expect("spawn Renode RP2040");
    mcu.load_firmware(&path).expect("load firmware");
    let rose = Arc::new(Mutex::new(false));
    let seen = rose.clone();
    mcu.on_pin_change(Box::new(move |pin, high, _| {
        if pin == LED_PIN && high {
            *seen.lock().unwrap() = true;
        }
    }));
    for _ in 0..5 {
        mcu.run_micros(20_000).expect("run chunk");
    }
    let word = mcu
        .read_register(PADS_GPIO25)
        .expect("PADS register readable");
    let rose = *rose.lock().unwrap();
    Some((drive_field(word), rose))
}

#[test]
fn a_firmware_that_programs_12_ma_reads_back_12_ma() {
    let Some((field, rose)) = boot_and_read("rp2040_drive", "drive_12ma.elf") else {
        return;
    };
    assert!(rose, "the fixture drives GP25 high");
    assert_eq!(field, 3, "DRIVE = 0x3 (12 mA) as written");
}

#[test]
fn a_firmware_that_programs_2_ma_reads_back_2_ma() {
    let Some((field, rose)) = boot_and_read("rp2040_drive", "drive_2ma.elf") else {
        return;
    };
    assert!(rose, "the fixture drives GP25 high");
    assert_eq!(field, 0, "DRIVE = 0x0 (2 mA) as written");
}

#[test]
fn a_firmware_that_never_touches_drive_shows_the_reset_default() {
    let Some((field, _)) = boot_and_read("rp2040_blink_uart", "blink_uart.elf") else {
        return;
    };
    assert_eq!(field, 1, "DRIVE resets to 0x1 (4 mA), Table 341");
}
