//! Proof: AVR firmware reads an I2C LM75 temperature sensor and acts on it.
//!
//! The firmware (testdata/firmware/i2c_thermostat) polls the LM75 at address
//! 0x48 over the ATmega328P hardware TWI peripheral and drives PB0 HIGH when the
//! reported temperature is at or above 30 C, LOW otherwise. This test attaches
//! a hauksbee LM75 slave to the co-sim, sweeps its temperature across the
//! threshold, and asserts the firmware's GPIO (PB0 -> net "FLAG") follows.
//!
//! This exercises the full master-read path that was previously missing on AVR:
//! the firmware clocks bytes OUT of the slave, and the LM75 answers with real
//! datasheet-encoded temperature bytes.

#![cfg(feature = "avr")]

use std::sync::{Arc, Mutex};

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::{HauksbeeEngine, I2cBus, Lm75};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use hauksbee_server::engine::Engine;

/// Minimal board: U1 ATmega328P with PB0 (pad 14) on net "FLAG", power/ground
/// wired, and an SDA/SCL pair on the ADC4/ADC5 pads (the AVR TWI pins). The
/// LM75 is intercepted at the TWI peripheral, not through these nets, but
/// wiring them keeps the board realistic.
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "FLAG")
  (net 4 "SDA")
  (net 5 "SCL")

  (module Package_QFP:TQFP-32_7x7mm_P0.8mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value ATmega328P (at 0 2) (layer F.Fab))
    (pad 7  smd rect (at -3 0) (net 2 "+5V"))
    (pad 8  smd rect (at -3 1) (net 1 "GND"))
    (pad 14 smd rect (at 3 0) (net 3 "FLAG"))
    (pad 27 smd rect (at 3 2) (net 4 "SDA"))
    (pad 28 smd rect (at 3 3) (net 5 "SCL"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "FLAG"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;

fn firmware() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/i2c_thermostat/thermostat.hex")
}

/// Run the firmware for `ms` against an LM75 fixed at `temp_c`, returning the
/// final PB0 / "FLAG" net voltage.
fn run_at_temp(temp_c: f64, ms: u32) -> f64 {
    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    let fw = firmware();
    let mut engine = HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("build engine");

    // Attach the LM75 at its default address and set the swept temperature.
    let bus = Arc::new(Mutex::new(
        I2cBus::new("U2").with_slave(Box::new(Lm75::new(Lm75::DEFAULT_ADDR, temp_c))),
    ));
    engine.scheduler_mut().attach_i2c_bus(bus.clone());

    // Run the co-sim.
    let frame_dt = 1e-3_f64;
    let n = ms as usize;
    let mut last_flag = 0.0;
    for _ in 0..n {
        let frame = engine.step(frame_dt);
        if let Some(&v) = frame.net_voltages.get("FLAG") {
            last_flag = v;
        }
    }
    last_flag
}

#[test]
fn firmware_drives_gpio_from_i2c_temperature() {
    let fw = firmware();
    assert!(
        fw.exists(),
        "build the firmware first: make -C testdata/firmware/i2c_thermostat ({fw:?})"
    );

    // Below threshold (30 C): the firmware should hold PB0 LOW.
    let cold = run_at_temp(20.0, 60);
    assert!(
        cold < 1.0,
        "at 20 C the FLAG net should be LOW, got {cold:.3} V"
    );

    // Above threshold: PB0 HIGH (driven through the 5 V GPIO rail / 10k pull).
    let hot = run_at_temp(40.0, 60);
    assert!(
        hot > 3.0,
        "at 40 C the FLAG net should be HIGH, got {hot:.3} V"
    );
}

#[test]
fn gpio_follows_temperature_sweep() {
    let fw = firmware();
    assert!(fw.exists(), "firmware missing: {fw:?}");

    // Sweep across the 30 C threshold and assert the GPIO tracks it.
    let sweep = [10.0, 25.0, 29.0, 31.0, 35.0, 50.0, 28.0, 15.0];
    let mut highs = Vec::new();
    for &t in &sweep {
        let v = run_at_temp(t, 50);
        let high = v > 2.5;
        highs.push((t, high, v));
    }
    for (t, high, v) in &highs {
        let expect = *t >= 30.0;
        assert_eq!(
            *high,
            expect,
            "at {t} C expected GPIO {}, got {:.3} V",
            if expect { "HIGH" } else { "LOW" },
            v
        );
    }
}
