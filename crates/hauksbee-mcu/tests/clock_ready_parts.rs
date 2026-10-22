//! External-clock readiness, per part: the crystal-present / crystal-absent
//! contract that `renode_stm32.rs` proves for the STM32F103, for the other
//! parts whose descriptors carry `[[soc.clock_ready]]` entries.
//!
//! A board with a crystal must raise HSERDY after the part's startup time and
//! PLLRDY after the lock time on top of that; a board without one must never
//! raise either, so firmware that waits on the flag (the CubeMX and pico-sdk
//! bring-up loops have no timeout) stays blocked, as on silicon. The STM32F072
//! rows double as the regression test for its old RCC stub, which returned
//! alternating values on every read and so hung any firmware that waited.
//!
//! The STM32 images are `testdata/firmware/clock_truth/ready_probe.c`: PA5
//! rises on HSERDY, PA6 on PLLRDY. The RP2040 case is the unmodified pico-sdk
//! blink image, whose `runtime_init` blocks on `XOSC_STATUS.STABLE`.

#![cfg(feature = "renode")]

use hauksbee_mcu::{Mcu, PinId, RenodeBackend, RenodeConfig};
use std::sync::{Arc, Mutex};

const CHUNK_US: u64 = 50;

/// Virtual microsecond at which each marker pin first rose, or `None`.
fn marker_times(
    part: &str,
    config: RenodeConfig,
    elf: &str,
    crystal: bool,
    budget_us: u64,
) -> Option<(Option<u64>, Option<u64>)> {
    if !crate::support::renode_or_skip() {
        return None;
    }
    let Some(elf) = crate::support::firmware(&format!("clock_truth/{elf}")) else {
        eprintln!("SKIP: {elf} not built (run make in testdata/firmware/clock_truth)");
        return None;
    };
    let mut mcu = RenodeBackend::new(config.with_external_clock_present(crystal))
        .unwrap_or_else(|e| panic!("spawn Renode for {part}: {e:#}"));
    mcu.load_firmware(&elf).expect("load ready probe");
    mcu.set_active_ports(&['A']);

    let now = Arc::new(Mutex::new(0u64));
    let hse = Arc::new(Mutex::new(None));
    let pll = Arc::new(Mutex::new(None));
    let (n, h, p) = (now.clone(), hse.clone(), pll.clone());
    mcu.on_pin_change(Box::new(move |pin, high, _| {
        if !high {
            return;
        }
        let t = *n.lock().unwrap();
        if pin == (PinId { port: 'A', bit: 5 }) {
            h.lock().unwrap().get_or_insert(t);
        }
        if pin == (PinId { port: 'A', bit: 6 }) {
            p.lock().unwrap().get_or_insert(t);
        }
    }));
    for t in (1..=budget_us / CHUNK_US).map(|i| i * CHUNK_US) {
        *now.lock().unwrap() = t;
        mcu.run_micros(CHUNK_US).expect("run readiness slice");
    }
    let (h, p) = (*hse.lock().unwrap(), *pll.lock().unwrap());
    Some((h, p))
}

/// `hse_us` is the datasheet startup time and `lock_us` the PLL lock time the
/// descriptor carries (cited there). The marker is seen at the first slice
/// boundary after the event, and the firmware needs a few microseconds to write
/// the pin, so the window is `[expected, expected + 150 us]`. Both markers are
/// quantized to a slice, so the lock window opens one slice early.
fn check_part(part: &str, config: fn() -> RenodeConfig, elf: &str, hse_us: u64, lock_us: u64) {
    if let Some((h, p)) = marker_times(part, config(), elf, true, 2_800) {
        let h = h.unwrap_or_else(|| panic!("{part}: HSERDY marker never rose with a crystal"));
        let p = p.unwrap_or_else(|| panic!("{part}: PLLRDY marker never rose after HSE"));
        assert!(
            (hse_us..=hse_us + 150).contains(&h),
            "{part}: HSERDY at {h} us, datasheet startup is {hse_us} us"
        );
        assert!(
            (h + lock_us - CHUNK_US..=h + lock_us + 150).contains(&p),
            "{part}: PLLRDY at {p} us, HSE ready at {h} us plus {lock_us} us lock"
        );
    }
    if let Some((h, p)) = marker_times(part, config(), elf, false, 3_000) {
        assert_eq!(h, None, "{part}: HSERDY rose with no crystal on the board");
        assert_eq!(p, None, "{part}: PLLRDY rose with no HSE source");
    }
}

// Startup and lock times are the descriptors' cited datasheet numbers:
// F4 DS8626 p.103/p.106 (2 ms, 300 us), L0 DS10685 p.81/p.86 (2 ms, 160 us),
// F072 DS9826 p.68/p.74 (2 ms, 200 us), G4 DS12288 p.117/p.123 (2 ms, 40 us).
#[test]
fn stm32f4_crystal_gates_hse_and_pll() {
    check_part(
        "stm32f4",
        RenodeConfig::stm32f4_discovery,
        "stm32f4_ready_probe.elf",
        2_000,
        300,
    );
}

#[test]
fn stm32l0_crystal_gates_hse_and_pll() {
    check_part(
        "stm32l0",
        RenodeConfig::stm32l0,
        "stm32l0_ready_probe.elf",
        2_000,
        160,
    );
}

/// With a crystal the F072 reaches both markers: the old RCC stub hung.
#[test]
fn stm32f072_crystal_gates_hse_and_pll_and_does_not_hang() {
    check_part(
        "stm32f072",
        RenodeConfig::stm32f072,
        "stm32f072_ready_probe.elf",
        2_000,
        200,
    );
}

#[test]
fn stm32g4_crystal_gates_hse_and_pll() {
    check_part(
        "stm32g4",
        RenodeConfig::stm32g4,
        "stm32g4_ready_probe.elf",
        2_000,
        40,
    );
}

/// RP2040: the pico-sdk image blocks in `xosc_init` without a crystal, so GP25
/// never toggles and nothing reaches the UART; with one it boots, no earlier
/// than the 1 ms the SDK's `STARTUP.DELAY` encodes.
#[test]
fn rp2040_crystal_gates_xosc_stable() {
    if !crate::support::renode_or_skip() {
        return;
    }
    let Some(elf) = crate::support::firmware("rp2040_blink_uart/blink_uart.elf") else {
        eprintln!("SKIP: rp2040_blink_uart/blink_uart.elf not present");
        return;
    };
    let run = |crystal: bool| -> (Option<u64>, String) {
        let mut mcu =
            RenodeBackend::new(RenodeConfig::rp2040().with_external_clock_present(crystal))
                .expect("spawn Renode RP2040");
        mcu.load_firmware(&elf).expect("load pico-sdk ELF");
        let uart = crate::support::capture_uart(&mut mcu);
        let now = Arc::new(Mutex::new(0u64));
        let first = Arc::new(Mutex::new(None));
        let (n, f) = (now.clone(), first.clone());
        mcu.on_pin_change(Box::new(move |pin, _high, _| {
            if pin == (PinId { port: '0', bit: 25 }) {
                f.lock().unwrap().get_or_insert(*n.lock().unwrap());
            }
        }));
        for i in 1..=100u64 {
            *now.lock().unwrap() = i * 1_000;
            mcu.run_micros(1_000).expect("run chunk");
        }
        let text = String::from_utf8_lossy(&uart.lock().unwrap()).to_string();
        let first = *first.lock().unwrap();
        (first, text)
    };

    let (edge, text) = run(true);
    let edge = edge.expect("GP25 must toggle with a crystal fitted");
    assert!(
        edge >= 1_000,
        "boot cannot finish inside the 1 ms XOSC startup: {edge} us"
    );
    assert!(
        text.contains("main reached"),
        "the SDK must reach main: {text:?}"
    );

    let (edge, text) = run(false);
    assert_eq!(
        edge, None,
        "no crystal: runtime_init must stay blocked in xosc_init"
    );
    assert!(
        text.is_empty(),
        "no crystal: nothing may reach the UART, got {text:?}"
    );
}
