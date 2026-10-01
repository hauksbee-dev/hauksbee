//! Proof: STM32F103 firmware reads an SPI MCP3008 ADC and drives a GPIO on threshold.
//!
//! The firmware (`testdata/firmware/stm32_spi_adc`) configures SPI1 as a master and
//! reads MCP3008 channel 0 in a loop. When the returned 10-bit count is >= 512
//! (Vref/2 = 1.65 V with a 3.3 V reference), PA8 is driven HIGH; otherwise LOW.
//! PA8 is routed to net "FLAG" on the demo board.
//!
//! This test attaches a hauksbee `Mcp3008` slave via `SpiBus::attach_spi_bus`,
//! sweeps the channel-0 voltage across the threshold, and asserts the FLAG net
//! follows. It is the SPI analogue of `i2c_sensor_cosim.rs`.
//!
//! ## Current status
//!
//! The Renode `on_spi` hook is a documented no-op (see `hauksbee-mcu/src/renode/mod.rs`):
//! SPI peripheral interception is not yet wired for the Renode backend. These tests
//! will FAIL with a FLAG-stuck-LOW assertion until the bridge is implemented. That
//! is intentional -- the tests exercise the full co-sim contract so they go green
//! only when the bridge truly works.

#![cfg(feature = "renode")]

use std::path::PathBuf;

use hauksbee_mcu::renode::is_available;

use crate::support::{
    assert_spi_adc_flag_follows_sweep, assert_spi_adc_flag_high_above_threshold,
    assert_spi_adc_flag_low_below_threshold, spi_adc_flag,
};

const DEMO: &str = "stm32_spi_adc_demo";

/// Renode + firmware preconditions shared by every test below: `Some(path)`
/// when both are satisfied, `None` after printing why (SKIP) it did not run.
fn preconditions() -> Option<PathBuf> {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return None;
    }
    crate::support::testdata("firmware/stm32_spi_adc/spi_adc.elf").or_else(|| {
        eprintln!("SKIP: spi_adc.elf not built; run `make -C testdata/firmware/stm32_spi_adc`");
        None
    })
}

#[test]
fn stm32_spi_adc_drives_flag_below_threshold() {
    let Some(fw) = preconditions() else {
        return;
    };
    assert_spi_adc_flag_low_below_threshold(|v| spi_adc_flag(DEMO, &fw, v, 200));
}

#[test]
fn stm32_spi_adc_drives_flag_above_threshold() {
    let Some(fw) = preconditions() else {
        return;
    };
    assert_spi_adc_flag_high_above_threshold("Renode", |v| spi_adc_flag(DEMO, &fw, v, 200));
}

#[test]
fn stm32_spi_adc_flag_follows_voltage_sweep() {
    let Some(fw) = preconditions() else {
        return;
    };
    assert_spi_adc_flag_follows_sweep("Renode", |v| spi_adc_flag(DEMO, &fw, v, 200));
}
