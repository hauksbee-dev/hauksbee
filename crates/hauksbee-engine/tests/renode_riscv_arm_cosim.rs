//! Renode co-sim proofs for the previously-configured-but-unproven MCU paths:
//! nRF52840 (Cortex-M4) and SiFive FE310 (RISC-V RV32). Both ship a Renode
//! `RenodeConfig` and platform but had never been run end-to-end.
//!
//! These run real, battle-tested Zephyr demo firmware (the upstream Renode demo
//! ELFs) through the identical RenodeBackend code path the STM32F103 proof
//! exercises: spawn headless Renode, bring up the machine, bridge UART over the
//! socket, and lockstep with `RunFor`. The proof standard met here is UART boot:
//! the firmware boots under the backend and its banner arrives through the UART
//! socket bridge.
//!
//! Why UART-boot rather than the full solved-LED-current proof the STM32/ESP32
//! demos meet: those use a custom blinky that drives a known LED net, so the MNA
//! solver computes a specific current. The nRF/FE310 firmware here is the generic
//! Zephyr shell (no fixed LED pin), so it proves the UART path and the backend
//! bring-up, not a specific analog current. The GPIO bridge for these parts is
//! the same ODR-poll code proven for STM32; driving a specific LED would only
//! need a custom firmware ELF.
//!
//! Skips gracefully when Renode or the demo ELFs are absent.

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::is_available;
use hauksbee_mcu::{Mcu, RenodeBackend, RenodeConfig};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn demo_elf(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/renode_demos")
        .join(name);
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

/// Boot `elf` under `config` through the RenodeBackend and collect UART for a
/// short window. Returns the UART text.
fn boot_and_collect_uart(config: RenodeConfig, elf: &PathBuf) -> String {
    let mut be = RenodeBackend::new(config).expect("spawn Renode backend");
    be.load_firmware(elf).expect("load firmware ELF");
    let uart = Arc::new(Mutex::new(Vec::<u8>::new()));
    let sink = uart.clone();
    be.on_uart(Box::new(move |b| {
        sink.lock().unwrap_or_else(|e| e.into_inner()).push(b)
    }));
    be.set_active_ports(&['0', '1']);
    // ~200 ms of virtual time in 5 ms chunks: enough for the Zephyr banner.
    for _ in 0..40 {
        be.run_micros(5000).expect("run chunk");
    }
    let u = uart.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&u).to_string()
}

#[test]
fn nrf52840_boots_and_talks_uart() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let Some(elf) = demo_elf("nrf52840-zephyr_shell.elf") else {
        eprintln!("SKIP: nrf52840 demo ELF not present");
        return;
    };
    let text = boot_and_collect_uart(RenodeConfig::nrf52840(), &elf);
    eprintln!("nRF52840 UART: {text:?}");
    // The Zephyr shell prints its prompt over uart0 through the socket bridge.
    assert!(
        text.contains("uart:~$") || text.to_lowercase().contains("shell"),
        "expected Zephyr shell banner/prompt over UART, got: {text:?}"
    );
}

#[test]
fn sifive_fe310_boots_and_talks_uart() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let Some(elf) = demo_elf("zephyr-fe310-shell.elf") else {
        eprintln!("SKIP: fe310 demo ELF not present");
        return;
    };
    // sifive_fe310() carries the post_load_setup (PRCI tags + `cpu PC vinit`)
    // the FE310 Zephyr demo needs after the ELF load.
    let text = boot_and_collect_uart(RenodeConfig::sifive_fe310(), &elf);
    eprintln!("FE310 UART: {text:?}");
    assert!(
        text.to_uppercase().contains("ZEPHYR") || text.contains("shell>"),
        "expected Zephyr boot banner over UART, got: {text:?}"
    );
}
