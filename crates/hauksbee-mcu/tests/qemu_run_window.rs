//! Regression proof for R8 #5 against a REAL Espressif QEMU guest: the run
//! window's 8 ms floor is BOOT-ONLY.
//!
//! Before the fix, `run_micros(100)` always advanced the guest ~8 ms, an ~80x
//! over-advance versus the scheduler's requested chunk, desyncing the QEMU MCU
//! from the analog solve and the lockstep peers. The floor exists so the ROM +
//! 2nd-stage bootloader clear boot in a few hundred chunks; once the firmware
//! raises the mailbox MAGIC handshake (the first thing `app_main` does), the
//! requested interval must be honored exactly.
//!
//! The observable here is the backend's cycle credit, which is derived from the
//! window it ACTUALLY ran: a post-boot 100 µs chunk must credit 100 µs worth of
//! cycles (24,000 at 240 MHz), where the pre-fix backend credited 8 ms worth
//! (1,920,000). The pure floor/cap mapping itself is pinned by unit tests in
//! `src/qemu/mod.rs`; this test proves the boot handshake flips the regime on a
//! live guest.
//!
//! Skips gracefully (reason printed) when Espressif QEMU or the blinky flash
//! image is absent.

#![cfg(feature = "qemu")]

use hauksbee_mcu::qemu::{is_available, QemuArch};
use hauksbee_mcu::{Mcu, QemuBackend};
use std::path::PathBuf;

fn flash_image() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/esp32_blinky/flash.bin");
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

#[test]
fn boot_floor_lifts_once_firmware_raises_magic() {
    if !is_available(QemuArch::Xtensa) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
        return;
    }
    let Some(fw) = flash_image() else {
        eprintln!("SKIP: flash.bin not built; run ./build.sh in testdata/firmware/esp32_blinky");
        return;
    };
    let mut mcu = QemuBackend::esp32(&fw).expect("boot Espressif QEMU esp32");

    // Drive boot with scheduler-shaped chunks. Each 5 ms request is floored to
    // 8 ms while booting; the ROM + bootloader take ~1-2 s of virtual time, so
    // a few hundred chunks suffice. 800 is a generous ceiling (~6.4 s guest).
    let mut booted_after = None;
    for i in 0..800 {
        mcu.run_micros(5_000).expect("run boot chunk");
        if mcu.boot_complete() {
            booted_after = Some(i + 1);
            break;
        }
    }
    let booted_after = booted_after.expect(
        "firmware never raised the mailbox MAGIC within 800 chunks; boot is \
         broken (the boot floor must still apply pre-MAGIC)",
    );
    eprintln!("boot complete after {booted_after} chunks");

    // THE REGRESSION: a steady-state 100 µs chunk must advance ~100 µs, not
    // the 8 ms boot floor. The credited cycles are exactly the run window
    // times the configured clock, so the assertion is deterministic.
    let freq = mcu.frequency() as f64;
    let before = mcu.current_cycle();
    mcu.run_micros(100).expect("run steady-state chunk");
    let credited = mcu.current_cycle() - before;
    let expected = (100e-6 * freq).round() as u64;
    let floored = (8e-3 * freq).round() as u64;
    assert_eq!(
        credited, expected,
        "post-boot 100 µs chunk must credit {expected} cycles; got {credited} \
         (the pre-fix unconditional floor credited {floored})"
    );

    // And a long post-boot chunk is no longer truncated by the old 50 ms cap.
    let before = mcu.current_cycle();
    mcu.run_micros(80_000).expect("run long chunk");
    let credited = mcu.current_cycle() - before;
    let expected = (80e-3 * freq).round() as u64;
    assert_eq!(
        credited, expected,
        "post-boot 80 ms chunk must credit its full length (old cap: 50 ms)"
    );
}
