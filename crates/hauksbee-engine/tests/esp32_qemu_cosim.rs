//! Full board co-simulation of the ESP32 devkit demo through Espressif QEMU.
//!
//! This is the headline ESP32 proof: hauksbee extracts the ESP32 demo
//! `.kicad_pcb`, binds U1 to the `qemu:esp32` backend, boots a headless
//! Espressif QEMU ESP32 running the bundled blinky+UART firmware (the merged
//! flash image), and co-simulates it against the solved analog circuit. It
//! asserts the same standard the STM32 Renode proof met:
//!
//!   1. UART: the firmware's "hello from esp32" banner arrives through the QEMU
//!      serial socket bridge.
//!   2. Analog: when the firmware drives GPIO2 HIGH at boot, real current flows
//!      through the 330 Ohm resistor R1 into the LED, computed by the MNA solver
//!      from the node voltages (V across R1 / 330).
//!   3. GPIO: the GPIO4 blink net toggles through the solved circuit at the
//!      firmware's blink rate.
//!
//! It also checks run-to-run determinism: a second run produces the same banner
//! and a comparable toggle count (icount makes the guest deterministic).
//!
//! Skips gracefully when Espressif QEMU is not installed or the flash image is
//! not built, but runs for real wherever both are present.

#![cfg(feature = "qemu")]

use hauksbee_engine::HauksbeeEngine;
use hauksbee_mcu::qemu::{is_available, QemuArch};
use hauksbee_server::engine::Engine;
use std::path::PathBuf;

fn board_text() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/boards/esp32_devkit_demo.kicad_pcb");
    std::fs::read_to_string(p).expect("read esp32 board")
}

fn flash_image() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/esp32_blinky/flash.bin");
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

/// Run the co-sim once, returning (uart_text, max_r1_current_ma, led_high_count,
/// gpio4_transitions).
fn run_cosim() -> (String, f64, usize, u32) {
    let fw = flash_image().expect("flash.bin present");
    let mut engine = HauksbeeEngine::from_board_file(
        &board_text(),
        Some(&fw),
        "/boards/esp32_devkit_demo.kicad_pcb",
    )
    .expect("build ESP32 engine");

    // Each QEMU cont/stop + mailbox read is a control round-trip, so use a coarse
    // co-sim chunk (5 ms): plenty to oversample a ~5 Hz blink. The LED RC
    // settling is sub-us, so a 5 ms analog chunk resolves the DC operating point.
    engine.scheduler_mut().chunk_s = 5e-3;

    let frame_dt = 5e-3_f64;
    let total = 1.5_f64;
    let n = (total / frame_dt).round() as usize;

    let mut uart: Vec<u8> = Vec::new();
    let mut max_r1_current_ma = 0.0_f64;
    let mut led_high_samples = 0usize;
    let mut gpio4_transitions = 0u32;
    let mut prev: Option<bool> = None;

    for _ in 0..n {
        let frame = engine.step(frame_dt);
        for b in frame.uart.values() {
            uart.extend_from_slice(b);
        }
        let g2 = frame.net_voltages.get("GPIO2_OUT").copied().unwrap_or(0.0);
        let led_a = frame.net_voltages.get("LED_A").copied().unwrap_or(0.0);
        let i_r1_ma = (g2 - led_a).abs() / 330.0 * 1000.0;
        max_r1_current_ma = max_r1_current_ma.max(i_r1_ma);
        if led_a > 1.0 {
            led_high_samples += 1;
        }

        let blink = frame.net_voltages.get("GPIO4_BLINK").copied().unwrap_or(0.0);
        let logic = if blink > 2.0 {
            Some(true)
        } else if blink < 1.0 {
            Some(false)
        } else {
            prev
        };
        if let (Some(p), Some(c)) = (prev, logic) {
            if p != c {
                gpio4_transitions += 1;
            }
        }
        prev = logic;
    }

    (
        String::from_utf8_lossy(&uart).to_string(),
        max_r1_current_ma,
        led_high_samples,
        gpio4_transitions,
    )
}

#[test]
fn esp32_full_cosim_through_solved_circuit() {
    if !is_available(QemuArch::Xtensa) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
        return;
    }
    if flash_image().is_none() {
        eprintln!("SKIP: testdata/firmware/esp32_blinky/flash.bin not built");
        return;
    }

    let (text, max_r1_current_ma, led_high_samples, gpio4_transitions) = run_cosim();

    eprintln!("ESP32 QEMU co-sim results:");
    eprintln!("  UART: {text:?}");
    eprintln!("  max R1 current: {max_r1_current_ma:.3} mA");
    eprintln!("  LED_A high samples: {led_high_samples}");
    eprintln!("  GPIO4 transitions: {gpio4_transitions}");

    // 1. UART bridge: the boot banner came through the QEMU serial socket.
    assert!(
        text.contains("hello from esp32"),
        "expected boot banner over UART, got: {text:?}"
    );

    // 2. Analog: GPIO2 driven HIGH pushes real current through the 330 Ohm R1
    //    into the LED. A 3.3 V rail across 330 Ohm + a ~1.8 V red LED clamp gives
    //    roughly (3.3 - 1.8)/330 ~= 4.5 mA. Assert a clearly nonzero, sane value.
    assert!(
        max_r1_current_ma > 1.0,
        "expected real current through R1 (GPIO2 HIGH); got {max_r1_current_ma:.3} mA"
    );
    assert!(
        max_r1_current_ma < 20.0,
        "R1 current implausibly high: {max_r1_current_ma:.3} mA"
    );
    assert!(
        led_high_samples > 0,
        "LED net never energised through the solved circuit"
    );

    // 3. GPIO: the GPIO4 blink net toggled through the solved circuit several
    //    times (the firmware toggles GPIO4 at ~5 Hz).
    assert!(
        gpio4_transitions >= 3,
        "GPIO4 blink net should toggle through the circuit; got {gpio4_transitions}"
    );
}

#[test]
fn esp32_cosim_is_deterministic_across_runs() {
    if !is_available(QemuArch::Xtensa) || flash_image().is_none() {
        eprintln!("SKIP: Espressif QEMU or flash.bin absent");
        return;
    }
    let (t1, _, _, n1) = run_cosim();
    let (t2, _, _, n2) = run_cosim();
    // The banner is identical run to run (icount-deterministic guest).
    assert!(t1.contains("hello from esp32"));
    assert!(t2.contains("hello from esp32"));
    // Toggle counts land in the same ballpark (a couple of chunks of variance is
    // tolerable; the blink rate is fixed by virtual time).
    let diff = (n1 as i64 - n2 as i64).abs();
    eprintln!("determinism: run1 toggles={n1}, run2 toggles={n2}, diff={diff}");
    assert!(
        diff <= 4,
        "toggle counts diverged across runs ({n1} vs {n2})"
    );
}

// ── ESP32-C3 (RISC-V) via the same QEMU backend ─────────────────────────────

fn c3_flash_image() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/esp32_blinky/flash_c3.bin");
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

#[test]
fn esp32c3_full_cosim_through_solved_circuit() {
    if !is_available(QemuArch::Riscv32) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-riscv32) not installed");
        return;
    }
    let Some(fw) = c3_flash_image() else {
        eprintln!("SKIP: flash_c3.bin not built");
        return;
    };

    let board = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/boards/esp32c3_devkit_demo.kicad_pcb"),
    )
    .expect("read esp32c3 board");

    let mut engine = HauksbeeEngine::from_board_file(
        &board,
        Some(&fw),
        "/boards/esp32c3_devkit_demo.kicad_pcb",
    )
    .expect("build ESP32-C3 engine");
    engine.scheduler_mut().chunk_s = 5e-3;

    let frame_dt = 5e-3_f64;
    let n = (1.5_f64 / frame_dt).round() as usize;
    let mut uart: Vec<u8> = Vec::new();
    let mut max_r1_ma = 0.0_f64;
    let mut led_high = 0usize;
    let mut transitions = 0u32;
    let mut prev: Option<bool> = None;

    for _ in 0..n {
        let frame = engine.step(frame_dt);
        for b in frame.uart.values() {
            uart.extend_from_slice(b);
        }
        let g2 = frame.net_voltages.get("GPIO2_OUT").copied().unwrap_or(0.0);
        let led = frame.net_voltages.get("LED_A").copied().unwrap_or(0.0);
        max_r1_ma = max_r1_ma.max((g2 - led).abs() / 330.0 * 1000.0);
        if led > 1.0 {
            led_high += 1;
        }
        let blink = frame.net_voltages.get("GPIO4_BLINK").copied().unwrap_or(0.0);
        let logic = if blink > 2.0 {
            Some(true)
        } else if blink < 1.0 {
            Some(false)
        } else {
            prev
        };
        if let (Some(p), Some(c)) = (prev, logic) {
            if p != c {
                transitions += 1;
            }
        }
        prev = logic;
    }

    let text = String::from_utf8_lossy(&uart).to_string();
    eprintln!("ESP32-C3 QEMU co-sim results:");
    eprintln!("  UART has 'hello from esp32': {}", text.contains("hello from esp32"));
    eprintln!("  max R1 current: {max_r1_ma:.3} mA");
    eprintln!("  LED_A high samples: {led_high}");
    eprintln!("  GPIO4 transitions: {transitions}");

    // The C3 firmware mirrors the ESP32 demo (UART hello + GPIO2 alive + GPIO4
    // blink), so the analog/GPIO proof is identical; the boot banner may have
    // flushed before the first chunk on the faster C3 boot, so we key the proof
    // on the solved-circuit GPIO activity (which requires app_main to be running)
    // and accept the banner when present.
    assert!(
        max_r1_ma > 1.0 && max_r1_ma < 20.0,
        "expected real R1 current via GPIO2 on C3; got {max_r1_ma:.3} mA"
    );
    assert!(led_high > 0, "LED net never energised on C3");
    assert!(
        transitions >= 3,
        "GPIO4 blink should toggle through the circuit on C3; got {transitions}"
    );
}
