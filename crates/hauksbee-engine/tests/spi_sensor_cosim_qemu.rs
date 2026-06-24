//! Proof: ESP32 firmware reads an SPI MCP3008 ADC and drives a GPIO on threshold.
//!
//! The firmware (`testdata/firmware/esp32_spi_adc`) configures the HSPI (SPI2)
//! master and reads MCP3008 channel 0 in a loop. When the returned 10-bit count
//! is >= 512 (Vin >= Vref/2 = 1.65 V with a 3.3 V reference), GPIO4 is driven
//! HIGH; otherwise LOW. GPIO4 (pad 26, role "p04") is routed to net "FLAG" on
//! the demo board.
//!
//! This test attaches a hauksbee `Mcp3008` slave via `SpiBus::attach_spi_bus`,
//! sweeps the channel-0 voltage across the threshold, and asserts the FLAG net
//! follows. It is the SPI/QEMU analogue of `i2c_sensor_cosim.rs`.
//!
//! ## Current status
//!
//! The QEMU `on_spi` hook is a documented no-op (see
//! `hauksbee-mcu/src/qemu/mod.rs`): SPI peripheral interception is not yet wired
//! for the QEMU backend. These tests will FAIL with a FLAG-stuck-LOW assertion
//! until the bridge is implemented. That is intentional -- the tests exercise the
//! full co-sim contract so they go green only when the bridge truly works.

#![cfg(feature = "qemu")]

use std::sync::{Arc, Mutex};

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::{HauksbeeEngine, Mcp3008, SpiBus};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use hauksbee_mcu::qemu::{is_available, QemuArch};
use hauksbee_server::engine::Engine;
use std::path::PathBuf;

fn board_text() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/boards/esp32_spi_adc_demo.kicad_pcb");
    std::fs::read_to_string(&p)
        .unwrap_or_else(|_| panic!("read esp32 SPI ADC board from {p:?}"))
}

fn flash_image() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/esp32_spi_adc/flash.bin");
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

/// Run the co-sim for `ms` of virtual time with the MCP3008 channel-0 voltage
/// fixed at `ch0_volts`. Returns the final "FLAG" net voltage.
///
/// The MCP3008 has a 3.3 V Vref. Threshold is counts >= 512, i.e. Vin >= 1.65 V.
/// Below 1.65 V -> FLAG LOW; at/above 1.65 V -> FLAG HIGH.
fn run_at_voltage(ch0_volts: f64, ms: u32) -> f64 {
    let board_str = board_text();
    let board = ExtractedBoard::from_auto(&board_str).expect("parse ESP32 SPI board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    let fw = flash_image().expect("flash.bin present");

    let mut engine = HauksbeeEngine::from_bound(bound, Some(&fw), "/boards/esp32_spi_adc_demo.kicad_pcb")
        .expect("build ESP32 SPI engine");

    // Coarse chunk for QEMU (each cont/stop + mailbox read is a control round-trip).
    engine.scheduler_mut().chunk_s = 5e-3;

    // Attach the MCP3008 slave with a 3.3 V reference and the swept channel voltage.
    let mut adc = Mcp3008::new(3.3);
    adc.set_channel(0, ch0_volts);
    let bus = Arc::new(Mutex::new(SpiBus::new("U2", Box::new(adc))));
    engine.scheduler_mut().attach_spi_bus(bus.clone());

    let frame_dt = 5e-3_f64;
    let n = (ms as f64 / 1000.0 / frame_dt).round() as usize;
    let mut last_flag = 0.0_f64;

    for _ in 0..n {
        let frame = engine.step(frame_dt);
        if let Some(&v) = frame.net_voltages.get("FLAG") {
            last_flag = v;
        }
    }
    last_flag
}

#[test]
fn esp32_spi_adc_drives_flag_below_threshold() {
    if !is_available(QemuArch::Xtensa) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
        return;
    }
    if flash_image().is_none() {
        eprintln!("SKIP: flash.bin not built; run `./build.sh` in testdata/firmware/esp32_spi_adc");
        return;
    }

    // 0.8 V < 1.65 V threshold -> counts < 512 -> FLAG LOW
    let v = run_at_voltage(0.8, 500);
    eprintln!("FLAG at 0.8 V input: {v:.3} V");
    assert!(
        v < 1.0,
        "at 0.8 V ADC input (below threshold), FLAG net should be LOW; got {v:.3} V. \
         If this is ~3.3 V the SPI bridge is working but the threshold logic is wrong. \
         If this is stuck-LOW the on_spi bridge is not yet wired (expected for now)."
    );
}

#[test]
fn esp32_spi_adc_drives_flag_above_threshold() {
    if !is_available(QemuArch::Xtensa) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
        return;
    }
    if flash_image().is_none() {
        eprintln!("SKIP: flash.bin not built; run `./build.sh` in testdata/firmware/esp32_spi_adc");
        return;
    }

    // 2.5 V > 1.65 V threshold -> counts > 512 -> FLAG HIGH
    let v = run_at_voltage(2.5, 500);
    eprintln!("FLAG at 2.5 V input: {v:.3} V");
    assert!(
        v > 2.5,
        "at 2.5 V ADC input (above threshold), FLAG net should be HIGH (~3.3 V); got {v:.3} V. \
         This test is expected to FAIL until the QEMU on_spi bridge is implemented \
         (currently a no-op in hauksbee-mcu/src/qemu/mod.rs). The firmware reads 0x000 \
         from every SPI byte because MISO is never driven, so counts == 0 always, \
         and FLAG stays LOW regardless of the configured Mcp3008 voltage."
    );
}

#[test]
fn esp32_spi_adc_flag_follows_voltage_sweep() {
    if !is_available(QemuArch::Xtensa) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
        return;
    }
    if flash_image().is_none() {
        eprintln!("SKIP: flash.bin not built; run `./build.sh` in testdata/firmware/esp32_spi_adc");
        return;
    }

    // Sweep the MCP3008 channel voltage across the 1.65 V (Vref/2) threshold.
    // With a working SPI bridge: below threshold -> FLAG LOW, at/above -> FLAG HIGH.
    // With the current no-op bridge: FLAG is always LOW (ADC reads 0 every time).
    let sweep: &[(f64, bool)] = &[
        (0.5,  false),  // well below threshold
        (1.0,  false),  // below threshold
        (1.64, false),  // just below threshold
        (1.65, true),   // at threshold (counts == 512)
        (2.0,  true),   // above threshold
        (3.0,  true),   // near Vref
    ];

    let mut failures = Vec::new();
    for &(volts, expect_high) in sweep {
        let v = run_at_voltage(volts, 500);
        let got_high = v > 2.0;
        eprintln!("  {volts:.2} V -> FLAG {v:.3} V ({})", if got_high { "HIGH" } else { "LOW" });
        if got_high != expect_high {
            failures.push((volts, expect_high, v));
        }
    }
    assert!(
        failures.is_empty(),
        "FLAG did not follow ADC voltage sweep (SPI bridge not yet wired for QEMU): {:?}",
        failures
    );
}

#[test]
fn esp32_spi_adc_uart_announces_ready() {
    if !is_available(QemuArch::Xtensa) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
        return;
    }
    let Some(fw) = flash_image() else {
        eprintln!("SKIP: flash.bin not built");
        return;
    };

    // Just boot and check the UART banner -- this works even without the SPI bridge.
    let board_str = board_text();
    let board = ExtractedBoard::from_auto(&board_str).expect("parse ESP32 SPI board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    let mut engine = HauksbeeEngine::from_bound(bound, Some(&fw), "/boards/esp32_spi_adc_demo.kicad_pcb")
        .expect("build ESP32 SPI engine for UART check");
    engine.scheduler_mut().chunk_s = 5e-3;

    let frame_dt = 5e-3_f64;
    let n = (1.5_f64 / frame_dt).round() as usize;
    let mut uart: Vec<u8> = Vec::new();
    for _ in 0..n {
        let frame = engine.step(frame_dt);
        for b in frame.uart.values() {
            uart.extend_from_slice(b);
        }
    }

    let text = String::from_utf8_lossy(&uart).to_string();
    eprintln!("ESP32 SPI ADC UART: {text:?}");
    assert!(
        text.contains("spi adc ready"),
        "expected 'spi adc ready' boot banner; got: {text:?}. \
         Note: the SPI reads will read 0 counts (bridge not yet wired), so \
         FLAG stays LOW, but the UART banner proves the firmware booted and reached app_main."
    );
}
