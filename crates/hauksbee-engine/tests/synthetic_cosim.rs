//! Test 1: a minimal synthetic board, bound with the builtin library and run
//! as a live co-sim against the demo firmware for 1.2 simulated seconds.
//!
//! Asserts the three coupling paths actually move:
//!   - GPIO out: the LED net toggles ~5×/sec (firmware blinks at 100 ms).
//!   - analog: the LED-net high voltage is a sensible diode-clamped level.
//!   - ADC in + UART: the firmware's 'v' command reports the RC divider
//!     voltage (2.5 V) within 10%.

mod common;

use hauksbee_engine::binder::bind_board;
#[cfg(feature = "avr")]
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
#[cfg(feature = "avr")]
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_models::ModelLibrary;

#[test]
fn synthetic_board_binds() {
    let board = ExtractedBoard::from_auto(common::SYNTH_BOARD).expect("parse synth board");
    assert_eq!(board.components.len(), 5, "U1, R1, D1, R2, R3");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    // The MCU and the LED diode must bind.
    assert_eq!(bound.report.mcu_count(), 1, "one ATmega328P");
    assert_eq!(bound.mcus.len(), 1);
    // PB5 (port B, bit 5) must have a GPIO driver stamped.
    assert!(
        bound.mcus[0].gpio_drivers.contains_key(&('B', 5)),
        "PB5 driver present: {:?}",
        bound.mcus[0].gpio_drivers.keys().collect::<Vec<_>>()
    );
    // ADC0 must be wired for injection.
    assert!(
        bound.mcus[0].adc_nets.contains_key(&0),
        "ADC0 wired: {:?}",
        bound.mcus[0].adc_nets.keys().collect::<Vec<_>>()
    );
    // The diode (LED) must be an analog device in the circuit.
    let has_diode = bound
        .circuit
        .devices
        .iter()
        .any(|d| matches!(d, hauksbee_ir::Device::Diode { .. }));
    assert!(has_diode, "LED stamped as a diode");
    // No unresolved analog parts on this clean board.
    let unresolved: Vec<_> = bound
        .report
        .rows
        .iter()
        .filter(|r| r.confidence == hauksbee_models::Confidence::Unresolved)
        .map(|r| r.reference.clone())
        .collect();
    assert!(unresolved.is_empty(), "all parts resolve: {unresolved:?}");
}

// Boots the AVR demo firmware on the synthetic Nano board, so it needs the
// GPL-gated `avr` feature (the GPL-free renode/qemu build refuses AVR
// firmware by design). `synthetic_board_binds` above stays feature-free.
#[cfg(feature = "avr")]
#[test]
fn synthetic_cosim_runs() {
    let firmware = common::demo_firmware();
    assert!(firmware.exists(), "demo firmware at {firmware:?}");

    let mut engine = HauksbeeEngine::from_board_file(
        common::SYNTH_BOARD,
        Some(&firmware),
        "/boards/synth.kicad_pcb",
    )
    .expect("build engine");

    // Deterministic chunked stepping: 1 ms frames for 1.2 s.
    let frame_dt = 1e-3_f64;
    let total = 1.2_f64;
    let n = (total / frame_dt).round() as usize;

    let mut led_high_samples: Vec<f64> = Vec::new();
    let mut uart: Vec<u8> = Vec::new();
    let mut led_transitions = 0u32;
    let mut prev_led: Option<bool> = None;

    for _ in 0..n {
        let frame = engine.step(frame_dt);
        let led_v = *frame.net_voltages.get("LED_A").expect("LED_A net present");
        // Logic decision with hysteresis around the diode clamp (~1 V mid).
        let logic = if led_v > 1.4 {
            Some(true)
        } else if led_v < 0.6 {
            Some(false)
        } else {
            prev_led
        };
        if let (Some(p), Some(c)) = (prev_led, logic) {
            if p != c {
                led_transitions += 1;
            }
        }
        if logic == Some(true) {
            led_high_samples.push(led_v);
        }
        prev_led = logic;
        for b in frame.uart.values() {
            uart.extend_from_slice(b);
        }
    }

    // ── GPIO toggle rate. The demo firmware flips PB5 every 100 ms (see
    // testdata/firmware/demo/main.c: ticks reaches 10 at ~10 ms/tick), i.e.
    // ~5 full on/off cycles per second = ~5 "toggles/sec". That is ~10
    // edges/sec, so ~12 voltage transitions over 1.2 s. (The brief's "~6
    // transitions" assumed a 100 ms *full period*; the firmware actually uses
    // a 100 ms half-period, so we assert against the firmware's real rate.)
    // We allow generous slack for emulator timing and chunk granularity.
    let toggles_per_sec = led_transitions as f64 / 2.0 / total; // cycles/sec
    assert!(
        (3.0..=7.0).contains(&toggles_per_sec),
        "LED ~{toggles_per_sec:.1} toggles/sec (want ~5; {led_transitions} edges in 1.2s)"
    );
    assert!(
        (8..=15).contains(&led_transitions),
        "LED transitions in 1.2s = {led_transitions} (firmware toggles every 100 ms)"
    );

    // ── Analog level: LED-net high voltage is a sensible diode-clamped level.
    assert!(
        !led_high_samples.is_empty(),
        "LED net reached a high state at least once"
    );
    let led_high = median(&mut led_high_samples);
    assert!(
        (1.5..=5.0).contains(&led_high),
        "LED high voltage = {led_high:.3} V (want 1.5..=5.0)"
    );

    // ── ADC readback over UART: ask for the divider voltage with 'v'.
    engine.serial("U1", b"v");
    // Pump a little more time so the firmware processes the request and replies.
    for _ in 0..200 {
        let frame = engine.step(frame_dt);
        for b in frame.uart.values() {
            uart.extend_from_slice(b);
        }
    }
    let text = String::from_utf8_lossy(&uart);
    let mv = last_millivolts(&text)
        .unwrap_or_else(|| panic!("no 'NNNNmV' reading in UART output:\n{text}"));
    let volts = mv as f64 / 1000.0;
    // The 10k/10k divider from +5V is 2.5 V; allow 10%.
    assert!(
        (volts - 2.5).abs() <= 0.25,
        "ADC readback {volts:.3} V not within 10% of 2.5 V divider"
    );
}

/// Median of a sample vector (mutates by sorting).
#[cfg(feature = "avr")]
fn median(xs: &mut [f64]) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = xs.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        xs[n / 2]
    } else {
        0.5 * (xs[n / 2 - 1] + xs[n / 2])
    }
}

/// Parse the last "<number>mV" reading out of the demo firmware's UART stream.
#[cfg(feature = "avr")]
fn last_millivolts(s: &str) -> Option<u32> {
    let mut last = None;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            // Followed by "mV"?
            if s[i..].starts_with("mV") {
                if let Ok(v) = s[start..i].parse::<u32>() {
                    last = Some(v);
                }
            }
        } else {
            i += 1;
        }
    }
    last
}
