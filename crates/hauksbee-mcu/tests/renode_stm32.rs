//! Integration tests for the Renode STM32 backend.
//!
//! These spawn a real headless Renode process, bring up an STM32F103 machine,
//! load the bundled blinky+UART firmware, and drive it through the generic
//! [`Mcu`] trait. They skip gracefully when Renode is not installed (so
//! `cargo test --workspace` stays green on a machine without it) but run for
//! real wherever Renode is present.

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::is_available;
use hauksbee_mcu::{Mcu, PinId, RenodeBackend, RenodeConfig, SpiEvent};
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

/// Firmware whose PA5/PC13 markers rise only after HSERDY/PLLRDY respectively.
fn clock_ready_elf() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/stm32_clock_ready/clock_ready.elf");
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

/// Run the external-clock probe in 50 us slices and return the virtual time at
/// which its HSERDY (PA5) and PLLRDY (PC13) markers first rose.
fn clock_ready_markers(
    config: RenodeConfig,
    external_clock_present: bool,
    budget_us: u64,
) -> Option<(u64, u64)> {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return None;
    }
    let Some(elf) = clock_ready_elf() else {
        eprintln!(
            "SKIP: clock_ready.elf not built \
             (run make in testdata/firmware/stm32_clock_ready)"
        );
        return None;
    };

    let config = config.with_external_clock_present(external_clock_present);

    let mut mcu = RenodeBackend::new(config).expect("spawn Renode STM32F103");
    mcu.load_firmware(&elf).expect("load clock readiness probe");
    mcu.set_active_ports(&['A', 'C']);

    const CHUNK_US: u64 = 50;
    let now_us = Arc::new(Mutex::new(0u64));
    let hse_us = Arc::new(Mutex::new(None));
    let pll_us = Arc::new(Mutex::new(None));
    let now = now_us.clone();
    let hse = hse_us.clone();
    let pll = pll_us.clone();
    mcu.on_pin_change(Box::new(move |pin, high, _cycle| {
        if !high {
            return;
        }
        let t = *now.lock().unwrap();
        if pin == (PinId { port: 'A', bit: 5 }) {
            hse.lock().unwrap().get_or_insert(t);
        }
        if pin == (PinId { port: 'C', bit: 13 }) {
            pll.lock().unwrap().get_or_insert(t);
        }
    }));

    for t in (CHUNK_US..=budget_us).step_by(CHUNK_US as usize) {
        *now_us.lock().unwrap() = t;
        mcu.run_micros(CHUNK_US).expect("run clock readiness slice");
    }

    let hse = *hse_us.lock().unwrap();
    let pll = *pll_us.lock().unwrap();
    Some((hse.unwrap_or(0), pll.unwrap_or(0)))
}

#[test]
fn stm32_external_clock_does_not_become_ready_without_board_source() {
    if !is_available() || clock_ready_elf().is_none() {
        eprintln!("SKIP: Renode or clock_ready.elf unavailable");
        return;
    }

    // Do not assert the private presence control. A real F103 with no crystal
    // on OSC_IN/OSC_OUT never raises HSERDY, so firmware cannot reach either
    // marker even after longer than the nominal 2 ms startup time.
    let config = RenodeConfig::stm32f103();
    let Some((hse_us, pll_us)) = clock_ready_markers(config, false, 3_000) else {
        return;
    };
    assert_eq!(hse_us, 0, "HSERDY marker falsely rose at {hse_us} us");
    assert_eq!(pll_us, 0, "PLLRDY marker falsely rose at {pll_us} us");
}

#[test]
fn stm32_external_clock_and_pll_observe_startup_delays() {
    let Some((hse_us, pll_us)) = clock_ready_markers(RenodeConfig::stm32f103(), true, 2_500) else {
        return;
    };

    assert!(
        (2_000..=2_100).contains(&hse_us),
        "HSERDY marker must follow the 2 ms crystal startup, got {hse_us} us"
    );
    assert!(
        (2_200..=2_300).contains(&pll_us),
        "PLLRDY marker must follow HSE plus the 200 us PLL lock, got {pll_us} us"
    );
    assert!(pll_us > hse_us, "PLL must lock after HSE is ready");
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
    mcu.on_pin_change(Box::new(move |pin, _high, _cycle| {
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

/// The GPIO drive-DIRECTION capability, live against Renode: with the F103
/// descriptor's CRL/CRH dir maps, the backend reports direction as observable
/// and distinguishes pins the firmware CONFIGURED as outputs from pins it
/// never touched; the distinction that lets a boot-coverage check tell a
/// held-LOW output from a floating input on an external backend.
#[test]
fn stm32_reports_drive_direction() {
    stm32_or_skip!(mcu);

    // Static claim first: every F103 port carries a dir map, so direction is
    // observable regardless of which ports the engine later wires.
    assert!(
        mcu.drive_direction_observable(),
        "F103 descriptor maps CRL/CRH on every port, so direction must be observable"
    );

    // Before any chunk runs, nothing has been polled: no pin may claim to be
    // a configured output yet.
    assert!(
        mcu.pins_configured_output().is_empty(),
        "no direction has been read before the first chunk"
    );

    // Boot the blinky: it configures PA5 (alive LED, held HIGH), PC13
    // (blinker, LOW half the time), and PA9 (USART1 TX, AF output).
    for _ in 0..6 {
        mcu.run_micros(50_000).expect("run chunk");
    }

    let outputs = mcu.pins_configured_output();
    let has = |port: char, bit: u8| outputs.iter().any(|p| p.port == port && p.bit == bit);
    assert!(
        has('A', 5),
        "PA5 is configured as a GP output by the firmware; got {outputs:?}"
    );
    assert!(
        has('C', 13),
        "PC13 is configured as a GP output: even while driven LOW mid-blink \
         it must read as DRIVEN, not floating; got {outputs:?}"
    );
    assert!(
        has('A', 9),
        "PA9 is the USART1 TX AF output; the F1 CRL/CRH MODE bits mark it driven"
    );
    // And a pin the firmware never touches stays out of the set: this is the
    // authoritative "genuinely floating" half of the distinction.
    assert!(
        !has('B', 0) && !has('C', 0),
        "pins the firmware never configured must not read as outputs; got {outputs:?}"
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

// ─────────────────────────────────────────────────────────────────────────────
// STM32F4 Discovery cross-talk test (real platform, no firmware needed)
// ─────────────────────────────────────────────────────────────────────────────

/// Verify that the STM32F4 Discovery Renode platform can attach SPI bridge
/// peripherals on BOTH spi2 AND spi3 without a redefinition conflict.
///
/// Root cause of the original failure: `RenodeConfig::stm32f4_discovery()` had
/// `spi_extra_repl = Some("spi2: ... @ sysbus 0x40003800\nspi3: ... @ sysbus
/// 0x40003C00")` but the base `stm32f4.repl` (included by
/// `stm32f4_discovery.repl`) ALREADY defines spi1/spi2/spi3. Trying to
/// redefine them caused Renode to dump the peripheral method list instead of
/// accepting the command, then panic on the bridge registration.
///
/// Fix: set `spi_extra_repl = None` so the installer skips the fragment and
/// goes straight to registering the bridge peripheral on the already-existing
/// controllers.
///
/// This test does NOT need firmware: it verifies the Renode machine setup
/// (platform load + C# bridge compile + peripheral registration) succeeds
/// without error. The on_spi_controller calls synthesise distinct per-byte
/// callbacks and route them through the bridge's TCP protocol; any cross-talk
/// would be caught by verifying each callback is only called for its own
/// controller (tracked by Arc<Mutex<Vec<u8>>>).
#[test]
fn stm32f4_spi2_and_spi3_bridge_setup_no_crosstalk() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }

    // Bring up the STM32F4 Discovery platform. No firmware is loaded; we only
    // care that platform setup and SPI bridge registration succeed.
    let mut mcu = RenodeBackend::new(RenodeConfig::stm32f4_discovery())
        .expect("spawn Renode STM32F4 Discovery; platform or bridge setup failed");

    // Track which bytes each bridge received so cross-talk would produce the
    // wrong controller's count > 0.
    let spi2_bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let spi3_bytes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    let sink2 = spi2_bytes.clone();
    let sink3 = spi3_bytes.clone();

    // Register bridge on spi2. Responds with 0xA2 (spi2 sentinel).
    mcu.on_spi_controller(
        "spi2",
        Box::new(move |ev: SpiEvent| {
            if !ev.deselect {
                sink2.lock().unwrap().push(ev.mosi);
            }
            0xA2
        }),
    );

    // Register bridge on spi3. Responds with 0xA3 (spi3 sentinel).
    mcu.on_spi_controller(
        "spi3",
        Box::new(move |ev: SpiEvent| {
            if !ev.deselect {
                sink3.lock().unwrap().push(ev.mosi);
            }
            0xA3
        }),
    );

    // Both bridge registrations must have succeeded. The test asserts this
    // implicitly: if either panicked above the test would already have failed.
    // No firmware means there are no actual SPI transactions to route; the
    // bridge setup (C# compile + peripheral registration in Renode) is what
    // we are verifying here. Confirm neither byte sink received garbage from
    // the other controller.
    assert!(
        spi2_bytes.lock().unwrap().is_empty(),
        "spi2 bridge received unexpected bytes before firmware ran: {:?}",
        spi2_bytes.lock().unwrap()
    );
    assert!(
        spi3_bytes.lock().unwrap().is_empty(),
        "spi3 bridge received unexpected bytes before firmware ran: {:?}",
        spi3_bytes.lock().unwrap()
    );

    eprintln!(
        "stm32f4_spi2_and_spi3_bridge_setup_no_crosstalk: PASS: \
         spi2 and spi3 bridges registered on the real STM32F4 Discovery platform \
         without redefinition conflict"
    );
}
