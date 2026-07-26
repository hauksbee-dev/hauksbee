//! Regression: loading an Intel-HEX whose records exceed the target MCU's
//! flash must fail loudly instead of memcpy-ing past the simavr-allocated
//! flash buffer (heap corruption / UB). The classic trigger is a mega2560
//! image (flash up to 256 KiB) loaded onto a 328p (32 KiB). The `.elf` loader
//! is arch-gated; this covers the `.hex` path's equivalent bounds gate.

// Drives the in-process simavr core: GPL-gated `avr` feature only.
#![cfg(feature = "avr")]

use hauksbee_mcu::{AvrMcu, Mcu};
use std::fmt::Write as _;
use std::path::PathBuf;

/// Emit one Intel-HEX record with a correct checksum.
fn ihex_record(record_type: u8, addr: u16, data: &[u8]) -> String {
    let mut sum = data.len() as u8;
    sum = sum
        .wrapping_add((addr >> 8) as u8)
        .wrapping_add(addr as u8)
        .wrapping_add(record_type);
    let mut line = format!(":{:02X}{:04X}{:02X}", data.len(), addr, record_type);
    for &b in data {
        sum = sum.wrapping_add(b);
        write!(line, "{b:02X}").unwrap();
    }
    write!(line, "{:02X}", (!sum).wrapping_add(1)).unwrap();
    line
}

fn write_hex(name: &str, records: &[String]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hauksbee-hex-bounds-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut text = records.join("\n");
    text.push('\n');
    text.push_str(":00000001FF\n"); // EOF record
    std::fs::write(&path, text).unwrap();
    path
}

/// A record placed entirely ABOVE the 328p's 32 KiB flash (at 0x10000, i.e. a
/// mega2560-sized image) must be rejected with a clear error, not copied out
/// of bounds.
#[test]
fn hex_beyond_flash_end_errors_cleanly() {
    // Extended-linear-address 0x0001 -> records land at 0x10000.
    let ela = ihex_record(0x04, 0x0000, &[0x00, 0x01]);
    let data = ihex_record(0x00, 0x0000, &[0xCF; 16]);
    let hex = write_hex("oversized.hex", &[ela, data]);

    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    let err = mcu
        .load_firmware(&hex)
        .expect_err("a hex beyond flashend must not load")
        .to_string();
    assert!(
        err.contains("does not fit this MCU's flash"),
        "error should name the flash bounds violation, got: {err}"
    );
    assert!(
        err.contains("larger part"),
        "error should hint at the wrong-part cause, got: {err}"
    );
}

/// A record that STARTS inside flash but runs past the end (straddling
/// `flashend`) is just as much a heap overrun and must also be rejected.
#[test]
fn hex_straddling_flash_end_errors_cleanly() {
    // 32 KiB flash ends at 0x8000; 16 bytes at 0x7FF8 spill 8 bytes past it.
    let data = ihex_record(0x00, 0x7FF8, &[0xCF; 16]);
    let hex = write_hex("straddle.hex", &[data]);

    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    let err = mcu
        .load_firmware(&hex)
        .expect_err("a hex straddling flashend must not load")
        .to_string();
    assert!(
        err.contains("does not fit this MCU's flash"),
        "error should name the flash bounds violation, got: {err}"
    );
}

/// The gate must not reject legitimate images: a small in-bounds hex still
/// loads and runs.
#[test]
fn hex_within_flash_still_loads() {
    // rjmp .-2 at address 0; the tiniest valid firmware.
    let data = ihex_record(0x00, 0x0000, &[0xFF, 0xCF]);
    let hex = write_hex("tiny.hex", &[data]);

    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(&hex).expect("an in-bounds hex must load");
    mcu.run_millis(1).expect("and run");
}
