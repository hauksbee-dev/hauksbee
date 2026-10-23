//! Regression: loading an Intel-HEX whose records exceed the target MCU's
//! flash must fail loudly instead of memcpy-ing past the simavr-allocated
//! flash buffer (heap corruption / UB). The classic trigger is a mega2560
//! image (flash up to 256 KiB) loaded onto a 328p (32 KiB). The `.elf` loader
//! is arch-gated; this covers the `.hex` path's equivalent bounds gate.

// Drives the in-process simavr core: GPL-gated `avr` feature only.
#![cfg(feature = "avr")]

use crate::support::{ihex_record, write_hex};
use hauksbee_mcu::{AvrMcu, Mcu};

/// A record placed entirely ABOVE the 328p's 32 KiB flash (at 0x10000, i.e. a
/// mega2560-sized image) must be rejected with a clear error, not copied out
/// of bounds.
#[test]
fn hex_beyond_flash_end_errors_cleanly() {
    // Extended-linear-address 0x0001 -> records land at 0x10000.
    let ela = ihex_record(0x04, 0x0000, &[0x00, 0x01]);
    let data = ihex_record(0x00, 0x0000, &[0xCF; 16]);
    let hex = write_hex("hex-bounds", "oversized.hex", &[ela, data]);

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
    let hex = write_hex("hex-bounds", "straddle.hex", &[data]);

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
    let hex = write_hex("hex-bounds", "tiny.hex", &[data]);

    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(&hex).expect("an in-bounds hex must load");
    mcu.run_millis(1).expect("and run");
}

/// The flash every part reports is the silicon's: what a caller compares an
/// image against before choosing which footprint's part runs it.
#[test]
fn part_capacity_is_the_parts_flash() {
    let flash = |p: &str| hauksbee_mcu::avr_part_capacity(p).map(|c| c.flash);
    assert_eq!(flash("atmega168"), Some(16 * 1024));
    assert_eq!(flash("atmega328p"), Some(32 * 1024));
    assert_eq!(flash("atmega2560"), Some(256 * 1024));
    assert_eq!(flash("no-such-part"), None);
}

/// An ELF image bigger than the part's flash used to reach simavr's ELF
/// loader, which clips the copy but then zero-fills the remainder past the
/// flash buffer: heap corruption, then a `free()`/`realloc()` abort that took
/// the whole process down. It must be refused before the loader runs, naming
/// both sizes, and the core must stay usable.
#[test]
fn elf_bigger_than_flash_is_refused_before_the_loader_sees_it() {
    use hauksbee_testkit::avr_firmware::{avr_elf, port_toggle_image};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.elf");
    std::fs::write(&path, avr_elf(&port_toggle_image(50, 20 * 1024))).unwrap();
    for _ in 0..3 {
        let mut mcu = AvrMcu::new("atmega168", 8_000_000).expect("create MCU");
        let err = format!(
            "{:#}",
            mcu.load_firmware(&path).expect_err("20 KiB > 16 KiB")
        );
        assert!(
            err.contains("20480") && err.contains("16384") && err.contains("atmega168"),
            "the refusal names the part and both sizes: {err}"
        );
        drop(mcu);
    }
    // The same image loads and runs on the part it was built for.
    let mut big = AvrMcu::new("atmega328p", 8_000_000).expect("create MCU");
    big.load_firmware(&path).expect("20 KiB fits 32 KiB");
    big.run_millis(1).expect("and runs");
}

/// A HEX with no data records (or a malformed one) used to reach simavr's
/// reader, which dereferences a null chunk on it; it is refused instead.
#[test]
fn hex_with_no_program_is_refused_not_crashed_on() {
    let empty = write_hex("hex-bounds", "empty.hex", &[]);
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    let err = format!("{:#}", mcu.load_firmware(&empty).expect_err("no program"));
    assert!(err.contains("no data records"), "{err}");
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("bad.hex");
    std::fs::write(&bad, ":0400000001020304FF\n:00000001FF\n").unwrap();
    assert!(
        mcu.load_firmware(&bad).is_err(),
        "a bad checksum is refused"
    );
}

/// Every data run of a HEX loads, not just the first (simavr's reader kept
/// only the first contiguous run, silently dropping code placed apart from
/// the reset vector).
#[test]
fn hex_with_separate_runs_loads_every_run() {
    // Reset vector: rjmp to word 0x80 (byte 0x100).
    let vector = ihex_record(0x00, 0x0000, &[0x7F, 0xC0]);
    // At 0x100: ldi r16, 0xFF; out DDRB, r16; rjmp .-2.
    let body = ihex_record(0x00, 0x0100, &[0x0F, 0xEF, 0x04, 0xB9, 0xFF, 0xCF]);
    let hex = write_hex("hex-bounds", "two-runs.hex", &[vector, body]);
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(&hex).expect("both runs fit");
    mcu.run_millis(1).expect("runs");
    // DDRB sits at data address 0x24 on every ATmega48/88/168/328.
    assert_eq!(
        mcu.read_data(0x24),
        Some(0xFF),
        "the code in the second run executed and made port B an output"
    );
}
