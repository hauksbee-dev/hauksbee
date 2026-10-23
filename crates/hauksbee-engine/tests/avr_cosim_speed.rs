//! AVR co-sim speed on a mid-size board: an `ATmega328P` toggling fourteen GPIO
//! at about 5 kHz, each into a 330R + LED + 100nF load, with supply
//! decoupling and a reset pull-up (46 parts).
//!
//! A real board of this shape took hours of wall time per simulated second,
//! so co-sim timed out. The default test runs a short window and checks the
//! answer (every LED net toggles at the firmware's rate); the ignored one
//! measures wall seconds per simulated second (`HAUKSBEE_BENCH_SIM_MS`
//! simulated milliseconds, default 50):
//! `cargo nextest run -p hauksbee-engine --run-ignored only avr_cosim_speed`.

// The board binds a `simavr:` part, whose in-process core needs the `avr` feature.
#![cfg(feature = "avr")]

use std::fmt::Write as _;

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_models::ModelLibrary;
use hauksbee_testkit::avr_firmware::{intel_hex, port_toggle_image, port_toggle_period_cycles};

/// `(pad, GPIO net)` for the 14 GPIO the firmware toggles: PB0-PB5 and
/// PD0-PD7 on the `ATmega328P`'s TQFP-32 (PB6/PB7 are the crystal pins).
const GPIO: [(u32, &str); 14] = [
    (12, "PB0"),
    (13, "PB1"),
    (14, "PB2"),
    (15, "PB3"),
    (16, "PB4"),
    (17, "PB5"),
    (30, "PD0"),
    (31, "PD1"),
    (32, "PD2"),
    (1, "PD3"),
    (2, "PD4"),
    (9, "PD5"),
    (10, "PD6"),
    (11, "PD7"),
];

/// Toggle delay: 773 cycles per loop at 8 MHz, so each pin toggles every
/// 96.6 us, a 5.2 kHz square wave.
const DELAY: u8 = 255;

fn two_pad(lib: &str, reference: &str, value: &str, a: &str, b: &str, x: u32) -> String {
    format!(
        "  (module {lib} (layer F.Cu)\n    (at {x} 120)\n    \
         (fp_text reference {reference} (at 0 0) (layer F.SilkS))\n    \
         (fp_text value {value} (at 0 2) (layer F.Fab))\n    \
         (pad 1 smd rect (at 0 0) (size 1 1) (net {} \"{a}\"))\n    \
         (pad 2 smd rect (at 1 0) (size 1 1) (net {} \"{b}\"))\n  )\n",
        net_index(a),
        net_index(b)
    )
}

fn nets() -> Vec<String> {
    let mut n = vec![String::new(), "GND".into(), "+5V".into(), "RESET".into()];
    for (_, gpio) in GPIO {
        n.push(gpio.to_string());
        n.push(format!("{gpio}_LED"));
    }
    n
}

fn net_index(name: &str) -> usize {
    nets().iter().position(|n| n == name).expect("known net")
}

fn board() -> ExtractedBoard {
    let mut pcb = String::from("(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n");
    for (i, name) in nets().iter().enumerate() {
        writeln!(pcb, "  (net {i} \"{name}\")").unwrap();
    }
    let mut pads: Vec<(u32, &str)> = vec![
        (3, "GND"),
        (5, "GND"),
        (21, "GND"),
        (4, "+5V"),
        (6, "+5V"),
        (18, "+5V"),
        (20, "+5V"),
        (29, "RESET"),
    ];
    pads.extend(GPIO);
    pcb.push_str(
        "  (module Package_QFP:TQFP-32_7x7mm_P0.8mm (layer F.Cu)\n    (at 100 100)\n    \
         (fp_text reference U1 (at 0 0) (layer F.SilkS))\n    \
         (fp_text value ATmega328P-AU (at 0 2) (layer F.Fab))\n",
    );
    for (pad, name) in pads {
        writeln!(
            pcb,
            "    (pad {pad} smd rect (at 0 {pad}) (size 1 1) (net {} \"{name}\"))",
            net_index(name)
        )
        .unwrap();
    }
    pcb.push_str("  )\n");
    let res = "Resistor_SMD:R_0603_1608Metric";
    let cap_fp = "Capacitor_SMD:C_0603_1608Metric";
    let led = "LED_SMD:LED_0603_1608Metric";
    pcb.push_str(&two_pad(res, "R1", "10k", "RESET", "+5V", 150));
    for (i, cap) in ["C1", "C2", "C3"].iter().enumerate() {
        let x = 155 + 5 * u32::try_from(i).unwrap();
        pcb.push_str(&two_pad(cap_fp, cap, "100n", "+5V", "GND", x));
    }
    for (i, (_, gpio)) in GPIO.iter().enumerate() {
        let x = 200 + 10 * u32::try_from(i).unwrap();
        let n = i + 2;
        let lednet = format!("{gpio}_LED");
        pcb.push_str(&two_pad(res, &format!("R{n}"), "330", gpio, &lednet, x));
        // LED pad 1 cathode, pad 2 anode.
        pcb.push_str(&two_pad(
            led,
            &format!("D{n}"),
            "LED",
            "GND",
            &lednet,
            x + 3,
        ));
        pcb.push_str(&two_pad(
            cap_fp,
            &format!("C{}", n + 2),
            "100n",
            gpio,
            "GND",
            x + 6,
        ));
    }
    pcb.push_str(")\n");
    ExtractedBoard::from_auto(&pcb).expect("parse board")
}

fn engine(dir: &tempfile::TempDir) -> HauksbeeEngine {
    let fw = dir.path().join("toggle.hex");
    std::fs::write(&fw, intel_hex(&port_toggle_image(DELAY, 0))).unwrap();
    let bound = bind_board(&board(), &ModelLibrary::builtin());
    assert_eq!(bound.mcus.len(), 1, "the ATmega328P binds");
    HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("co-sim builds")
}

/// Rising crossings of `threshold` in a sampled trace.
fn rising(samples: &[f64], threshold: f64) -> usize {
    samples
        .windows(2)
        .filter(|w| w[0] < threshold && w[1] >= threshold)
        .count()
}

/// Every LED net follows the firmware at its rate: a 5.2 kHz square wave
/// counted at the 330R/100nF node's mid-rail crossings over a 10 ms window.
#[test]
fn every_led_net_toggles_at_the_firmware_rate() {
    // Sixteen samples per toggle period: about 2 ms of warm-up, then a
    // window of about 10 ms.
    const PER_PERIOD: u32 = 16;
    const WARMUP_STEPS: u32 = 11 * PER_PERIOD;
    const WINDOW_STEPS: u32 = 52 * PER_PERIOD;
    let dir = tempfile::tempdir().unwrap();
    let mut e = engine(&dir);
    let cycles = u32::try_from(port_toggle_period_cycles(DELAY)).expect("a short loop");
    let period_s = 2.0 * f64::from(cycles) / 8e6;
    let dt = period_s / f64::from(PER_PERIOD);
    for _ in 0..WARMUP_STEPS {
        e.step(dt);
    }
    let mut traces: Vec<Vec<f64>> = vec![Vec::new(); GPIO.len()];
    for _ in 0..WINDOW_STEPS {
        let frame = e.step(dt);
        for (trace, (_, gpio)) in traces.iter_mut().zip(GPIO) {
            trace.push(frame.net_voltages.get(gpio).copied().unwrap_or(0.0));
        }
    }
    let expected = f64::from(WINDOW_STEPS / PER_PERIOD);
    for (trace, (_, gpio)) in traces.iter().zip(GPIO) {
        let edges = f64::from(u32::try_from(rising(trace, 2.5)).expect("few edges"));
        assert!(
            (edges - expected).abs() <= expected.mul_add(0.1, 1.0),
            "{gpio}: {edges} rising edges in the window, expected about {expected:.1}"
        );
    }
}

/// An unsigned benchmark knob from the environment, or `default`.
fn knob(name: &str, default: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Step `e` for `HAUKSBEE_BENCH_SIM_MS` simulated milliseconds (after one
/// warm-up millisecond) and print wall seconds per simulated second.
fn bench(label: &str, e: &mut HauksbeeEngine) {
    let ms = knob("HAUKSBEE_BENCH_SIM_MS", 50);
    e.step(1e-3);
    let start = std::time::Instant::now();
    for _ in 0..ms {
        e.step(1e-3);
    }
    let wall = start.elapsed().as_secs_f64();
    let sim = f64::from(ms) * 1e-3;
    println!(
        "BENCH {label}: {wall:.3} wall s for {sim:.3} sim s = {:.1} wall s per sim s",
        wall / sim
    );
}

/// Wall seconds per simulated second for this board (printed; ignored by
/// default since it measures rather than checks).
#[test]
#[ignore = "benchmark: run with --run-ignored only"]
fn wall_seconds_per_simulated_second() {
    let dir = tempfile::tempdir().unwrap();
    bench("avr_cosim_speed", &mut engine(&dir));
}

/// The same measurement on the ultrasonic-cane fixture: an `ATmega328P` with an
/// LDO, a Li-ion charger, a MOSFET motor driver with flyback diode, buttons
/// and an I2C IMU, all of port B and D toggled by the firmware.
#[test]
#[ignore = "benchmark: run with --run-ignored only"]
fn wall_seconds_per_simulated_second_on_the_cane_board() {
    let dir = tempfile::tempdir().unwrap();
    let fw = dir.path().join("toggle.hex");
    let delay = u8::try_from(knob("HAUKSBEE_BENCH_DELAY", DELAY.into())).expect("a u8 delay");
    std::fs::write(&fw, intel_hex(&port_toggle_image(delay, 0))).unwrap();
    let pcb = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/boards/ultrasonic_cane/ultrasonic_cane.kicad_pcb");
    let board = ExtractedBoard::from_auto(&std::fs::read_to_string(pcb).unwrap()).unwrap();
    let bound = bind_board(&board, &ModelLibrary::builtin());
    let mut e = HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("co-sim builds");
    bench(&format!("cane delay={delay}"), &mut e);
}
