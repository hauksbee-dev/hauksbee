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
//! The QEMU `on_spi` hook is now wired (05-cosim-fidelity §5.2), but through
//! the RAM-mailbox bus contract (`hauksbee-mcu/src/qemu/mod.rs`, `mailbox` v2):
//! Espressif QEMU exposes no host hook for GPSPI transfers, so byte events can
//! only be surfaced for firmware that submits its transactions through the
//! mailbox cells. THIS firmware (`testdata/firmware/esp32_spi_adc`) drives the
//! real SPI2 controller with the esp-idf driver, which the emulator never
//! surfaces to the host, so these tests still FAIL with a FLAG-stuck-LOW
//! assertion. Un-ignore when either (a) the firmware is rebuilt against the
//! mailbox bus contract (needs an esp-idf/Xtensa toolchain), or (b) the QEMU
//! fork grows a real SPI peripheral hook (the §5.3 preferred ending). The
//! backend-level byte-callback path itself is regression-tested for real in
//! `hauksbee-mcu/tests/qemu_bus_mailbox.rs`.

#![cfg(feature = "qemu")]

use std::path::PathBuf;

use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_mcu::qemu::{is_available, QemuArch};

use crate::support::{
    assert_spi_adc_flag_follows_sweep, assert_spi_adc_flag_high_above_threshold,
    assert_spi_adc_flag_low_below_threshold, spi_adc_engine, spi_adc_flag,
};

const DEMO: &str = "esp32_spi_adc_demo";

/// QEMU + firmware preconditions shared by every test below: `Some(path)`
/// when both are satisfied, `None` after printing why (SKIP) it did not run.
fn preconditions() -> Option<PathBuf> {
    if !is_available(QemuArch::Xtensa) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
        return None;
    }
    crate::support::testdata("firmware/esp32_spi_adc/flash.bin").or_else(|| {
        eprintln!("SKIP: flash.bin not built; run `./build.sh` in testdata/firmware/esp32_spi_adc");
        None
    })
}

#[test]
fn esp32_spi_adc_drives_flag_below_threshold() {
    let Some(fw) = preconditions() else {
        return;
    };
    assert_spi_adc_flag_low_below_threshold(|v| spi_adc_flag(DEMO, &fw, v, 500));
}

#[test]
// Known-failing: the firmware drives the real SPI2 controller, which Espressif
// QEMU never surfaces to the host, so the modeled ADC never answers and the
// flag never rises. The on_spi callback path itself IS wired (RAM-mailbox bus
// contract, 05 section 5.2) and tested in hauksbee-mcu/tests/qemu_bus_mailbox.rs;
// un-ignore with mailbox-aware firmware or a QEMU SPI peripheral hook.
#[ignore = "esp32_spi_adc firmware drives the real (host-invisible) SPI2 controller; needs the mailbox bus contract or a QEMU SPI hook; see 05-cosim-fidelity.md section 5.2/5.3"]
fn esp32_spi_adc_drives_flag_above_threshold() {
    let Some(fw) = preconditions() else {
        return;
    };
    assert_spi_adc_flag_high_above_threshold("QEMU", |v| spi_adc_flag(DEMO, &fw, v, 500));
}

#[test]
// Same cause as above: the real SPI2 controller is host-invisible in QEMU.
#[ignore = "esp32_spi_adc firmware drives the real (host-invisible) SPI2 controller; needs the mailbox bus contract or a QEMU SPI hook; see 05-cosim-fidelity.md section 5.2/5.3"]
fn esp32_spi_adc_flag_follows_voltage_sweep() {
    let Some(fw) = preconditions() else {
        return;
    };
    assert_spi_adc_flag_follows_sweep("QEMU", |v| spi_adc_flag(DEMO, &fw, v, 500));
}

#[test]
fn esp32_spi_adc_uart_announces_ready() {
    let Some(fw) = preconditions() else {
        return;
    };

    // Just boot and check the UART banner -- this works even without the SPI bridge.
    let mut engine = spi_adc_engine(DEMO, &fw);
    let frame_dt = 5e-3_f64;
    let n = 300; // 1.5 s of 5 ms frames
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
