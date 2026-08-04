//! RP2040 ADC injection, live: an engine-pushed voltage must reach firmware.
//!
//! `Analog.RP2040ADC` from the vendored bundle exposes
//! `SetDefaultVoltageOnChannel(int, double)`, which the descriptor's
//! `[[soc.adc]]` recipes call through the `{volts}` template. This test proves
//! the whole path: `Mcu::set_analog_in` renders the command, Renode accepts it,
//! and stock pico-sdk firmware calling `adc_read()` on channel 0 (GP26) returns
//! the corresponding 12-bit code.
//!
//! It is two-sided in the way that matters for an ADC: the SAME firmware is
//! driven at two different injected voltages and must report two different,
//! correctly-ordered counts. A stuck converter, a dropped injection, or a
//! `{volts}` rendering that lost its scale factor all fail that, whereas a
//! single-point check against a wide tolerance would not.

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::is_available;
use hauksbee_mcu::{Mcu, RenodeBackend, RenodeConfig};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn adc_elf() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/rp2040_adc_probe/adc_probe.elf");
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}

/// Pull the last `adc count=N` value out of the firmware's UART chatter.
fn last_count(text: &str) -> Option<u32> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("adc count="))
        .filter_map(|n| n.trim().parse::<u32>().ok())
        .next_back()
}

#[test]
fn rp2040_adc_injection_reaches_firmware() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let Some(elf) = adc_elf() else {
        eprintln!("SKIP: rp2040_adc_probe/adc_probe.elf not present");
        return;
    };

    let config = RenodeConfig::rp2040();
    assert_eq!(
        config.adc_channels.len(),
        4,
        "the descriptor maps ADC0..ADC3 (GP26..GP29); input 4 is the on-die \
         temperature sensor and is deliberately unmapped"
    );
    let mut mcu = RenodeBackend::new(config).expect("spawn Renode RP2040");

    let uart: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = uart.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));

    mcu.load_firmware(&elf).expect("load pico-sdk ADC ELF");

    // Low point first. 0.5 V of 3.3 V full scale is code round(0.5/3.3 * 4096) =
    // 621; allow a couple of LSB for the model's own decimal rounding.
    mcu.set_analog_in(0, 0.5);
    for _ in 0..8 {
        mcu.run_micros(50_000).expect("run chunk");
    }
    let text = String::from_utf8_lossy(&uart.lock().unwrap()).to_string();
    assert!(
        text.contains("hauksbee rp2040 adc: main reached"),
        "the ADC firmware must boot at all; got: {text:?}"
    );
    let low = last_count(&text).unwrap_or_else(|| panic!("no adc count in: {text:?}"));
    assert!(
        (615..=627).contains(&low),
        "0.5 V of 3.3 V full scale is ~621 counts; firmware read {low}. UART: {text:?}"
    );

    // High point on the same running machine: 2.5 V is round(2.5/3.3 * 4096) =
    // 3103. The count must MOVE, and move the right way.
    uart.lock().unwrap().clear();
    mcu.set_analog_in(0, 2.5);
    for _ in 0..8 {
        mcu.run_micros(50_000).expect("run chunk");
    }
    let text = String::from_utf8_lossy(&uart.lock().unwrap()).to_string();
    let high = last_count(&text).unwrap_or_else(|| panic!("no adc count in: {text:?}"));
    assert!(
        (3097..=3109).contains(&high),
        "2.5 V of 3.3 V full scale is ~3103 counts; firmware read {high}. UART: {text:?}"
    );
    assert!(
        high > low,
        "raising the injected voltage must raise the count ({low} -> {high})"
    );

    // Nothing was dropped: a mapped channel must never take the loud-drop path.
    assert!(
        mcu.adc_dropped_channels().is_empty(),
        "channel 0 is mapped, so no injection may be dropped; dropped {:?}",
        mcu.adc_dropped_channels()
    );
}
