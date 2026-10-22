//! Watchdog bite through the Renode backend.
//!
//! `testdata/firmware/watchdog/wdt.c` pulses one pin on every boot of the core
//! and then arms a 50 ms watchdog that is either never fed or fed forever. The
//! number of rising edges on the pin is therefore the number of boots. These
//! tests print the edge train so a failure shows what the platform did, and
//! assert the hardware behaviour: a starved watchdog reboots the core about
//! every 50 ms, a fed one never does.

#![cfg(feature = "renode")]

use hauksbee_mcu::{Mcu, PinId, RenodeBackend, RenodeConfig};
use std::sync::{Arc, Mutex};

const CHUNK_US: u32 = 1_000;
const WINDOW_MS: u32 = 400;

/// Rising edges of `pin` seen over `WINDOW_MS`, with the chunk index of each.
fn rising_edges(cfg: RenodeConfig, elf: &str, pin: PinId) -> Option<Vec<u32>> {
    if !crate::support::renode_or_skip() {
        return None;
    }
    let path = crate::support::firmware(&format!("watchdog/{elf}"))?;
    let mut mcu = RenodeBackend::new(cfg).ok()?;
    mcu.load_firmware(&path).ok()?;
    mcu.set_active_ports(&[pin.port]);
    let edges: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let chunk = Arc::new(Mutex::new(0u32));
    let (sink, now) = (edges.clone(), chunk.clone());
    mcu.on_pin_change(Box::new(move |p, high, _cycle| {
        if p == pin && high {
            sink.lock().unwrap().push(*now.lock().unwrap());
        }
    }));
    for i in 1..=WINDOW_MS {
        mcu.run_micros(CHUNK_US.into()).ok()?;
        *chunk.lock().unwrap() = i;
    }
    let seen = edges.lock().unwrap().clone();
    println!("{elf}: rising edges at chunks {seen:?}");
    Some(seen)
}

#[test]
fn nrf52840_starved_watchdog_reboots_the_core() {
    let pin = PinId { port: '0', bit: 13 };
    let Some(seen) = rising_edges(RenodeConfig::nrf52840(), "nrf52840_wdt.elf", pin) else {
        return;
    };
    assert!(
        seen.len() >= 4,
        "a starved 50 ms nRF52840 WDT must reboot the core repeatedly in {WINDOW_MS} ms: {seen:?}"
    );
}

#[test]
fn nrf52840_fed_watchdog_does_not_reboot() {
    let pin = PinId { port: '0', bit: 13 };
    let Some(seen) = rising_edges(RenodeConfig::nrf52840(), "nrf52840_wdt_fed.elf", pin) else {
        return;
    };
    assert_eq!(seen.len(), 1, "a fed watchdog must not reboot: {seen:?}");
}

#[test]
fn stm32f103_starved_watchdog_reboots_the_core() {
    let pin = PinId { port: 'C', bit: 13 };
    let Some(seen) = rising_edges(RenodeConfig::stm32f103(), "stm32f103_wdt.elf", pin) else {
        return;
    };
    assert!(
        seen.len() >= 4,
        "a starved 50 ms IWDG must reboot the core repeatedly in {WINDOW_MS} ms: {seen:?}"
    );
}

#[test]
fn stm32f103_fed_watchdog_does_not_reboot() {
    let pin = PinId { port: 'C', bit: 13 };
    let Some(seen) = rising_edges(RenodeConfig::stm32f103(), "stm32f103_wdt_fed.elf", pin) else {
        return;
    };
    assert_eq!(seen.len(), 1, "a fed watchdog must not reboot: {seen:?}");
}
