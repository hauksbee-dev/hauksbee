//! Integration tests for the Renode STM32 backend.
//!
//! These spawn a real headless Renode process, bring up an STM32F103 machine,
//! load the bundled blinky+UART firmware, and drive it through the generic
//! [`Mcu`] trait. They skip gracefully when Renode is not installed (so
//! `cargo test --workspace` stays green on a machine without it) but run for
//! real wherever Renode is present.

#![cfg(feature = "renode")]

use galvani_mcu::renode::is_available;
use galvani_mcu::{Mcu, PinId, RenodeBackend};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Path to the bundled STM32F103 demo firmware ELF.
fn blinky_elf() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/stm32_blinky/blinky.elf");
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

/// Bring up an STM32F103 with the demo firmware loaded, or skip.
macro_rules! stm32_or_skip {
    ($name:ident) => {
        if !is_available() {
            eprintln!("SKIP: Renode not installed");
            return;
        }
        let Some(elf) = blinky_elf() else {
            eprintln!("SKIP: blinky.elf not built (run make in testdata/firmware/stm32_blinky)");
            return;
        };
        let mut $name = RenodeBackend::stm32f103().expect("spawn Renode STM32F103");
        $name.load_firmware(&elf).expect("load blinky.elf");
    };
}

#[test]
fn stm32_uart_says_hello() {
    stm32_or_skip!(mcu);

    let uart_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = uart_buf.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));

    // Boot the firmware: a few 50 ms chunks is plenty for the banner.
    for _ in 0..6 {
        mcu.run_micros(50_000).expect("run chunk");
    }

    let text = String::from_utf8_lossy(&uart_buf.lock().unwrap()).to_string();
    assert!(
        text.contains("hello from stm32"),
        "expected boot banner in UART output, got: {text:?}"
    );
}

#[test]
fn stm32_pc13_led_toggles() {
    stm32_or_skip!(mcu);

    let pc13 = PinId { port: 'C', bit: 13 };
    let toggles: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let counter = toggles.clone();
    mcu.on_pin_change(Box::new(move |pin, _high| {
        if pin == pc13 {
            *counter.lock().unwrap() += 1;
        }
    }));

    // Run 1.0 s of virtual time in 50 ms chunks.
    for _ in 0..20 {
        mcu.run_micros(50_000).expect("run chunk");
    }

    let n = *toggles.lock().unwrap();
    // The firmware toggles PC13 roughly every 100 ms (~5 Hz), so ~8-12 edges in
    // 1 s. Assert a generous band: it must clearly be blinking, not stuck.
    assert!(
        (4..=20).contains(&n),
        "PC13 should toggle several times in 1 s; got {n} edges"
    );
}

#[test]
fn stm32_uart_command_response() {
    stm32_or_skip!(mcu);

    let uart_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = uart_buf.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));

    // Boot.
    for _ in 0..6 {
        mcu.run_micros(50_000).expect("run chunk");
    }
    uart_buf.lock().unwrap().clear();

    // 'i' asks the firmware to reprint its ident string.
    mcu.uart_write(b"i");
    for _ in 0..6 {
        mcu.run_micros(50_000).expect("run chunk");
    }

    let text = String::from_utf8_lossy(&uart_buf.lock().unwrap()).to_string();
    assert!(
        text.contains("hello from stm32"),
        "expected ident response to 'i', got: {text:?}"
    );
}
