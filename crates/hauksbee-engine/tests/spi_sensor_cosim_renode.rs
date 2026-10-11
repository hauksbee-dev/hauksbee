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
//! ## What the tests check
//!
//! The Renode backend's `on_spi` hook is implemented (see `hauksbee-mcu/src/renode/mod.rs`):
//! it routes the first configured SPI controller through a bridge that forwards each
//! firmware transfer to the attached `SpiBus`, so the modelled MCP3008 answers the
//! firmware's reads. The tests check that FLAG follows the ADC input: LOW at 0.8 V,
//! HIGH at 2.5 V, and switching across the 1.65 V threshold in a sweep. A FLAG stuck
//! LOW at every input points at the bridge not reaching the ADC, not at the threshold
//! logic. When Renode or the firmware ELF is missing, each test prints a `SKIP:` line
//! and returns without asserting.

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
