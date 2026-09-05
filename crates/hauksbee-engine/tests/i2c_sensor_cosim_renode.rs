//! I2C peripheral interception proof: STM32 / Renode backend.
//!
//! This is the Renode analogue of `i2c_sensor_cosim.rs`, which already passes
//! on AVR/simavr. THE FEATURE UNDER TEST IS NOT YET BUILT: today the Renode
//! backend's `on_i2c` hook is a documented no-op, so these tests are expected
//! to FAIL at the I2C dispatch level (the firmware's I2C reads return 0xFF,
//! the temperature decodes as -0.125 C, the FLAG stays LOW regardless of the
//! temperature we inject). They will go GREEN the moment the Renode I2C bridge
//! is wired up.
//!
//! ## What is being tested
//!
//! The firmware `testdata/firmware/stm32_i2c_thermostat/thermostat.elf` runs on
//! a Renode-emulated STM32F103. It polls an LM75 at I2C address 0x48 over the
//! STM32 hardware I2C1 peripheral (PB6=SCL, PB7=SDA) and drives PA8 HIGH when
//! the reported temperature is >= 30 C, LOW otherwise. hauksbee attaches an
//! in-process `Lm75` slave to an `I2cBus` and registers it with the co-sim.
//! When the bridge is complete:
//!   - At 20 C the FLAG net must be LOW  (< 1 V).
//!   - At 40 C the FLAG net must be HIGH (> 3 V on the 3.3 V rail with 4k7
//!     pull-down: driven HIGH by the STM32 GPIO driver).
//!   - Across the sweep [10, 25, 29, 31, 35, 50, 28, 15] the GPIO must track
//!     the 30 C threshold exactly.
//!
//! ## Board layout
//!
//! The board is a minimal extension of the STM32 bluepill demo: the same U1
//! STM32F103, power/ground, and PA5/PC13 nets from that demo, PLUS an explicit
//! I2C bus pair on PB6/PB7, and a FLAG net on PA8 (pad 29) with a 4k7 pulldown. The LM75
//! is intercepted at the I2C peripheral, not through these nets, but wiring them
//! keeps the board realistic (and consistent with how the AVR test wires SDA/SCL).
//!
//! ## Firmware
//!
//! `testdata/firmware/stm32_i2c_thermostat/thermostat.elf`, built with
//! `make -C testdata/firmware/stm32_i2c_thermostat`. Source in that directory.
//!
//! ## Running
//!
//! ```
//! cargo test -p hauksbee-engine --features renode i2c_renode -- --nocapture
//! ```
//! Requires Renode installed at `~/renode-portable`.

#![cfg(feature = "renode")]

use std::sync::{Arc, Mutex};

use hauksbee_engine::{HauksbeeEngine, I2cBus, Lm75};
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_mcu::renode::is_available;
use std::path::PathBuf;

/// Minimal board: U1 STM32F103, power/ground on the usual pads, I2C bus on
/// PB6/PB7 (pads 37/38 on the LQFP-48), FLAG on PA8 (pad 29), with a 4k7
/// pull-down to ground so the net has a well-defined logic level when the GPIO
/// is not driving it. The LM75 is not placed on the board (it is intercepted
/// in-process); the SDA/SCL pads are wired for board completeness only.
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "FLAG")
  (net 4 "SDA")
  (net 5 "SCL")
  (net 6 "PA5_OUT")

  (module Package_QFP:LQFP-48_7x7mm_P0.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F103C8T6 (at 0 2) (layer F.Fab))
    (pad 8  smd rect (at -3 1) (net 1 "GND"))
    (pad 9  smd rect (at -3 2) (net 2 "+3V3"))
    (pad 15 smd rect (at 3 0) (net 6 "PA5_OUT"))
    (pad 29 smd rect (at 3 1) (net 3 "FLAG"))
    (pad 23 smd rect (at 3 2) (net 1 "GND"))
    (pad 24 smd rect (at 3 3) (net 2 "+3V3"))
    (pad 35 smd rect (at -3 3) (net 1 "GND"))
    (pad 36 smd rect (at -3 4) (net 2 "+3V3"))
    (pad 37 smd rect (at -3 5) (net 5 "SCL"))
    (pad 38 smd rect (at -3 6) (net 4 "SDA"))
    (pad 44 smd rect (at -3 7) (net 1 "GND"))
    (pad 48 smd rect (at -3 8) (net 2 "+3V3"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 4k7 (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "FLAG"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;

fn firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/stm32_i2c_thermostat/thermostat.elf")
}

/// Run the firmware for `ms` milliseconds against an LM75 fixed at `temp_c`.
/// Returns the final "FLAG" net voltage.
fn run_at_temp(temp_c: f64, ms: u32) -> f64 {
    let fw = firmware();
    let mut engine = HauksbeeEngine::from_board_file(BOARD, Some(&fw), "/ci")
        .expect("build STM32 I2C thermostat engine");

    // Use coarse chunks for the external Renode backend (5 ms).
    // Fine chunks (100 us default) would mean thousands of TCP round-trips.
    engine.scheduler_mut().chunk_s = 5e-3;

    // Attach the LM75 at address 0x48.
    let bus = Arc::new(Mutex::new(
        I2cBus::new("U2").with_slave(Box::new(Lm75::new(Lm75::DEFAULT_ADDR, temp_c))),
    ));
    engine.scheduler_mut().attach_i2c_bus(bus.clone());

    let frame_dt = 5e-3_f64;
    let n = (ms as f64 / (frame_dt * 1e3)).round() as usize;
    let n = n.max(1);

    let mut last_flag = 0.0;
    for _ in 0..n {
        let frame = engine.step(frame_dt);
        if let Some(&v) = frame.net_voltages.get("FLAG") {
            last_flag = v;
        }
    }
    last_flag
}

/// The core proof: the firmware reads the LM75 temperature via I2C and drives
/// PA8 / "FLAG" HIGH when >= 30 C, LOW otherwise.
///
/// TODAY this test FAILS because the Renode `on_i2c` hook is a no-op: the
/// firmware gets 0xFF bytes back, decodes them as ~-0.125 C, and never sets the
/// flag. The test is deliberately written to fail with a clear, diagnosable
/// assertion message when the bridge is absent, and to go GREEN the instant the
/// bridge delivers real bytes.
#[test]
fn stm32_i2c_firmware_drives_gpio_from_temperature() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let fw = firmware();
    assert!(
        fw.exists(),
        "build the firmware first: make -C testdata/firmware/stm32_i2c_thermostat ({fw:?})"
    );

    // Below threshold: FLAG must be LOW.
    let cold = run_at_temp(20.0, 500);
    assert!(
        cold < 1.0,
        "STM32 at 20 C: FLAG net should be LOW (< 1 V); got {cold:.3} V. \
         If it reads ~0 V but the test still passes at 40 C (below), \
         the bridge is probably not yet wired (firmware saw 0xFF -> -0 C -> never hot)."
    );

    // Above threshold: FLAG must be HIGH.
    let hot = run_at_temp(40.0, 500);
    assert!(
        hot > 2.0,
        "STM32 at 40 C: FLAG net should be HIGH (> 2 V); got {hot:.3} V. \
         This is the primary proof: the Renode I2C bridge must deliver real \
         LM75 bytes to the firmware so it drives PA8 HIGH."
    );
}

/// Sweep the temperature across the threshold and assert the GPIO tracks it.
/// This is the strict proof-of-tracking test: any crossing point misfire
/// (firmware reports wrong polarity) fails here.
#[test]
fn stm32_i2c_gpio_follows_temperature_sweep() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let fw = firmware();
    assert!(fw.exists(), "firmware missing: {fw:?}");

    let sweep = [10.0_f64, 25.0, 29.0, 31.0, 35.0, 50.0, 28.0, 15.0];
    let mut results = Vec::new();
    for &t in &sweep {
        let v = run_at_temp(t, 400);
        let high = v > 1.5;
        results.push((t, high, v));
    }

    eprintln!("STM32 I2C thermostat sweep:");
    for (t, high, v) in &results {
        eprintln!(
            "  {t:.1} C -> FLAG = {:.3} V ({})",
            v,
            if *high { "HIGH" } else { "LOW" }
        );
    }

    for (t, high, v) in &results {
        let expect = *t >= 30.0;
        assert_eq!(
            *high,
            expect,
            "STM32 at {t} C: expected FLAG {}, got {v:.3} V",
            if expect { "HIGH" } else { "LOW" }
        );
    }
}

/// Incremental temperature sweep: single engine, mutate the LM75 temperature
/// between steps. This test probes whether the I2C bridge delivers fresh bytes
/// on every transaction (not cached from boot), which is the harder requirement.
#[test]
fn stm32_i2c_temperature_change_is_live() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let fw = firmware();
    assert!(fw.exists(), "firmware missing: {fw:?}");

    let mut engine =
        HauksbeeEngine::from_board_file(BOARD, Some(&fw), "/ci").expect("build engine");
    engine.scheduler_mut().chunk_s = 5e-3;

    let bus = Arc::new(Mutex::new(
        I2cBus::new("U2").with_slave(Box::new(Lm75::new(Lm75::DEFAULT_ADDR, 20.0))),
    ));
    engine.scheduler_mut().attach_i2c_bus(bus.clone());

    let frame_dt = 5e-3_f64;

    // Run 200 ms cold.
    let mut last_flag = 0.0_f64;
    for _ in 0..40 {
        let frame = engine.step(frame_dt);
        if let Some(&v) = frame.net_voltages.get("FLAG") {
            last_flag = v;
        }
    }
    assert!(
        last_flag < 1.0,
        "after 200 ms at 20 C, FLAG should be LOW; got {last_flag:.3} V"
    );

    // Raise to 40 C and run another 200 ms.
    {
        let mut b = bus.lock().unwrap();
        if let Some(lm75) = b.slave_mut_t::<Lm75>(Lm75::DEFAULT_ADDR) {
            lm75.set_temp_c(40.0);
        }
    }
    for _ in 0..40 {
        let frame = engine.step(frame_dt);
        if let Some(&v) = frame.net_voltages.get("FLAG") {
            last_flag = v;
        }
    }
    assert!(
        last_flag > 2.0,
        "after raising to 40 C, FLAG should go HIGH; got {last_flag:.3} V. \
         This fails if the Renode I2C bridge does not deliver updated bytes."
    );
}
