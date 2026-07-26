//! End-to-end verification tests for the declarative register-map sensor
//! interpreter (`RegisterMapSensor`).
//!
//! These tests are written against the *design contract* defined in
//! `docs/hunts/DECLARATIVE_SENSOR_DESIGN.md`; the interpreter does NOT exist
//! yet.  Every test here is intentionally RED: it will compile cleanly and fail
//! at runtime (with an explicit "unimplemented" panic) until a real
//! `RegisterMapSensor` is shipped that implements the specified API.
//!
//! ## Tests
//!
//! 1. **`declarative_lm75_equivalence`**, pure model, no firmware.
//!    Builds a declarative LM75 spec via TOML and asserts it returns
//!    byte-identical reads to the hand-coded `Lm75` across several
//!    temperatures (-10, 0, 25, 30.5, 85 °C).  Pins that the interpreter
//!    matches the reference encoding.
//!
//! 2. **`declarative_spi_who_am_i_and_data`**, pure model, no firmware.
//!    Builds a declarative SPI sensor spec with a const WHO_AM_I register and
//!    one i16 data register driven by an input.  Asserts WHO_AM_I reads 0x42
//!    and the data register reflects a swept input over the `SpiSlave`
//!    interface.
//!
//! 3. **`declarative_lm75_i2c_thermostat_cosim`**, Renode co-sim gate.
//!    Attaches the declarative LM75 (NOT the hand-coded `Lm75`) to the I2C bus,
//!    runs `testdata/firmware/stm32_i2c_thermostat/thermostat.elf` on the STM32
//!    Renode backend, sweeps `temperature_c` across the 30 °C threshold, and
//!    asserts the firmware-driven FLAG net follows: LOW below 30, HIGH at/above.
//!    Guarded by `#[cfg(feature = "renode")]`; skips gracefully if Renode or the
//!    firmware are absent.
//!
//! ## Design contract (abbreviated from DECLARATIVE_SENSOR_DESIGN.md)
//!
//! ```
//! RegisterMapSensor::from_toml(toml_str: &str) -> Result<RegisterMapSensor>
//! RegisterMapSensor::set_input(name: &str, value: f64) -> &mut Self
//! RegisterMapSensor: I2cSlave  (for bus = "i2c" sensors)
//! RegisterMapSensor: SpiSlave  (for bus = "spi" sensors)
//! ```
//!
//! Attach to the co-sim the same way as the hand-coded slaves:
//!   `I2cBus::with_slave(Box::new(sensor))` / `SpiBus::new(id, Box::new(sensor))`
//!
//! ## Red-state proof
//!
//! The `RegisterMapSensor` type below is a compile-time placeholder that
//! satisfies the trait bounds but panics on every method call.  As soon as a
//! real implementation is exported from `hauksbee_engine`, replace the local
//! stub with the real import and all three tests should go green together.

// ─────────────────────────────────────────────────────────────────────────────
// Placeholder stub, delete and replace with the real import once the
// interpreter lands in hauksbee-engine.
// ─────────────────────────────────────────────────────────────────────────────

use hauksbee_engine::{I2cBus, Lm75, RegisterMapSensor};
use hauksbee_mcu::I2cEvent;

// ─────────────────────────────────────────────────────────────────────────────
// TOML specs used across tests
// ─────────────────────────────────────────────────────────────────────────────

/// Declarative LM75A spec in the DECLARATIVE_SENSOR_DESIGN.md format.
///
/// Register 0x00: 2-byte temperature, signed Q7.1 (0.5 °C/LSB), big-endian,
/// left-justified into 16 bits (the classic 9-bit LM75 format: top 9 bits,
/// bottom 7 unused).  The classic LM75 uses Q8.1 with `raw = round(T/0.5)`,
/// stored in bits [15:7].  We represent this as encoding = "q8.1_le_lj" or
/// equivalently "q7.1_be" depending on convention; the design doc uses
/// `q7.1_be` (9-bit twos-complement, big-endian, left-justified to 16 bits).
const LM75_TOML: &str = r#"
[sensor]
name        = "lm75_declarative"
bus         = "i2c"
i2c_address = 0x48

[[sensor.input]]
name    = "temperature_c"
default = 25.0

[[sensor.register]]
addr     = 0x00
bytes    = 2
encoding = "q7.1_be"
expr     = "temperature_c"

[[sensor.register]]
addr  = 0x01
const = [0x00]

[[sensor.register]]
addr  = 0x02
const = [0x4B, 0x00]

[[sensor.register]]
addr  = 0x03
const = [0x50, 0x00]

[sensor.protocol]
style = "i2c_pointer"
"#;

/// Declarative SPI sensor spec with a const WHO_AM_I register and one signed
/// 16-bit data register driven by an input.
const SPI_SENSOR_TOML: &str = r#"
[sensor]
name = "declarative_spi_sensor"
bus  = "spi"

[[sensor.input]]
name    = "channel_raw"
default = 0.0

[[sensor.register]]
addr  = 0x75
const = [0x42]

[[sensor.register]]
addr     = 0x3A
bytes    = 2
encoding = "i16_be"
expr     = "channel_raw"

[sensor.protocol]
style           = "spi_reg"
rw_read_is_high = false
"#;

// ─────────────────────────────────────────────────────────────────────────────
// Helper: dispatch a full I2C register-pointer-then-read transaction and return
// the bytes the slave clocked back.  Mirrors what the firmware does.
// ─────────────────────────────────────────────────────────────────────────────

fn i2c_read_register(bus: &mut I2cBus, device_addr: u8, reg: u8, n_bytes: usize) -> Vec<u8> {
    // Write phase: send pointer byte.
    bus.dispatch(I2cEvent::Start {
        addr: device_addr,
        read: false,
    });
    bus.dispatch(I2cEvent::Write {
        addr: device_addr,
        data: reg,
    });
    bus.dispatch(I2cEvent::Stop { addr: device_addr });
    // Read phase: repeated-start then clock N bytes.
    bus.dispatch(I2cEvent::Start {
        addr: device_addr,
        read: true,
    });
    let mut out = Vec::with_capacity(n_bytes);
    for _ in 0..n_bytes {
        if let Some(b) = bus.dispatch(I2cEvent::Read { addr: device_addr }) {
            out.push(b);
        }
    }
    bus.dispatch(I2cEvent::Stop { addr: device_addr });
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 1: Byte-level equivalence between declarative LM75 and hand-coded Lm75
// ─────────────────────────────────────────────────────────────────────────────

/// Assert the declarative LM75 returns byte-identical reads to the hand-coded
/// `Lm75` across a representative temperature sweep.
///
/// This is a pure-model test (no firmware, no Renode, no feature gate) and is
/// the most important correctness pin: it proves the interpreter's encoding path
/// is identical to the reference implementation rather than just plausibly
/// similar.
///
/// ## Why this test goes RED now
///
/// `RegisterMapSensor::from_toml` returns `Err` (unimplemented placeholder).
/// The `.unwrap()` on that result is what panics and marks the test red.  Once
/// the real interpreter is in place, `from_toml` will succeed and the I2C reads
/// will be exercised; any encoding mismatch will then surface here.
#[test]
fn declarative_lm75_equivalence() {
    let temps = [-10.0_f64, 0.0, 25.0, 30.5, 85.0];

    for &t in &temps {
        // ── Reference: hand-coded Lm75 ───────────────────────────────────
        let mut ref_bus = I2cBus::new("REF").with_slave(Box::new(Lm75::new(Lm75::DEFAULT_ADDR, t)));
        let ref_bytes = i2c_read_register(&mut ref_bus, 0x48, 0x00, 2);

        // ── System under test: declarative RegisterMapSensor ─────────────
        //
        // This is where the interpreter must be created.  The `from_toml` call
        // will return Err and the test panics (RED) until the interpreter lands.
        let mut sensor = RegisterMapSensor::from_toml(LM75_TOML)
            .unwrap_or_else(|e| panic!("declarative LM75 from_toml failed: {e}"));
        sensor.set_input("temperature_c", t);

        let mut sut_bus = I2cBus::new("SUT").with_slave(Box::new(sensor));
        let sut_bytes = i2c_read_register(&mut sut_bus, 0x48, 0x00, 2);

        // Bytes must be identical, same encoding, same rounding, same byte order.
        assert_eq!(
            sut_bytes, ref_bytes,
            "at {t} °C: declarative sensor returned {sut_bytes:?}, \
             hand-coded Lm75 returned {ref_bytes:?}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 2: SPI WHO_AM_I + swept data register
// ─────────────────────────────────────────────────────────────────────────────

/// Assert that a declarative SPI sensor correctly exposes a const WHO_AM_I
/// register and a variable data register driven by an input.
///
/// The `spi_reg` protocol is the standard address-then-data SPI model:
///   tx byte 0 = register address (MSB = R/W, rest = addr)
///   tx byte 1..N = data bytes (MISO carries the register contents during these)
///
/// For this test we use the convention where the master clocks address byte first
/// (MISO is don't-care during address phase), then data bytes.
///
/// ## Why this test goes RED now
///
/// Same as test 1: `from_toml` returns Err and the `.unwrap_or_else` panics.
#[test]
fn declarative_spi_who_am_i_and_data() {
    // ── WHO_AM_I read ─────────────────────────────────────────────────────
    {
        let mut sensor = RegisterMapSensor::from_toml(SPI_SENSOR_TOML)
            .unwrap_or_else(|e| panic!("declarative SPI sensor from_toml failed: {e}"));

        use hauksbee_engine::SpiSlave;

        // spi_reg protocol: first byte is address (0x75 = WHO_AM_I), second byte
        // is the register contents clocked back on MISO.
        let _addr_phase = sensor.transfer(0x75); // address byte; MISO during this = don't care
        let who_am_i = sensor.transfer(0x00); // read phase; slave drives MISO
        sensor.deselect();

        assert_eq!(
            who_am_i, 0x42,
            "WHO_AM_I register 0x75 should read 0x42, got 0x{who_am_i:02X}"
        );
    }

    // ── Data register swept across a range ───────────────────────────────
    {
        let test_values: &[f64] = &[-1000.0, -1.0, 0.0, 100.0, 32767.0];

        for &raw in test_values {
            let mut sensor = RegisterMapSensor::from_toml(SPI_SENSOR_TOML)
                .unwrap_or_else(|e| panic!("declarative SPI sensor from_toml failed: {e}"));
            sensor.set_input("channel_raw", raw);

            use hauksbee_engine::SpiSlave;

            // Read register 0x3A: address byte + 2 data bytes.
            let _addr_phase = sensor.transfer(0x3A);
            let msb = sensor.transfer(0x00);
            let lsb = sensor.transfer(0x00);
            sensor.deselect();

            // Reconstruct the i16 from big-endian bytes.
            let decoded = i16::from_be_bytes([msb, lsb]) as f64;
            let expected = raw.clamp(-32768.0, 32767.0).round();

            assert!(
                (decoded - expected).abs() < 1.0,
                "data register at channel_raw={raw}: expected {expected}, got {decoded}"
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Test 3: Full Renode co-sim gate, declarative LM75 drives the FLAG net
// ─────────────────────────────────────────────────────────────────────────────

/// End-to-end Renode co-simulation: the declarative LM75 (NOT the hand-coded
/// `Lm75`) is attached as the I2C slave and the STM32 I2C thermostat firmware
/// drives the FLAG net based on temperature.
///
/// The firmware polls the LM75 at 0x48 and drives a GPIO HIGH when temperature
/// >= 30 °C, LOW otherwise.  We sweep `temperature_c` across the threshold and
/// assert the FLAG net follows.
///
/// Guarded by `#[cfg(feature = "renode")]`.  Skips gracefully when Renode is
/// not installed or the STM32 I2C thermostat ELF is not built.
///
/// ## Why this test goes RED now
///
/// `RegisterMapSensor::from_toml` returns Err before the firmware is even
/// started, so the test panics at sensor construction.  A passing test here
/// proves that:
///   1. The interpreter parsed the TOML spec correctly.
///   2. The interpreter's I2C read path encoded the temperature correctly.
///   3. The firmware correctly interpreted the bytes (same as the hand-coded
///      slave path already proven in `i2c_sensor_cosim.rs`).
#[cfg(feature = "renode")]
#[test]
fn declarative_lm75_i2c_thermostat_cosim() {
    use hauksbee_engine::binder::bind_board;
    use hauksbee_engine::HauksbeeEngine;
    use hauksbee_extract::ExtractedBoard;
    use hauksbee_mcu::renode::is_available;
    use hauksbee_models::ModelLibrary;
    use hauksbee_server::engine::Engine;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }

    let fw = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/stm32_i2c_thermostat/thermostat.elf");
    if !fw.exists() {
        eprintln!(
            "SKIP: STM32 I2C thermostat firmware not built \
             (build testdata/firmware/stm32_i2c_thermostat/thermostat.elf)"
        );
        return;
    }

    /// Minimal STM32F103 board: U1 STM32F103C8T6, PA8 (pad 29) on net "FLAG", PB6/PB7
    /// on SDA/SCL (the STM32 I2C1 pins after REMAP=0).  The LM75 slave is
    /// intercepted at the hardware I2C peripheral, not through these nets.
    const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "FLAG")
  (net 4 "SDA")
  (net 5 "SCL")

  (module Package_QFP:LQFP-48_7x7mm_P0.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F103C8T6 (at 0 2) (layer F.Fab))
    (pad 1   smd rect (at -3 0)  (net 2 "+3V3"))
    (pad 8   smd rect (at -3 1)  (net 1 "GND"))
    (pad 29  smd rect (at  3 0)  (net 3 "FLAG"))
    (pad 42  smd rect (at  3 2)  (net 4 "SDA"))
    (pad 43  smd rect (at  3 3)  (net 5 "SCL"))
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

    // Run the STM32 I2C thermostat firmware for `ms` with a declarative LM75
    // fixed at `temp_c`.  Returns the last sampled FLAG net voltage.
    let run_at_temp = |temp_c: f64, ms: u32| -> f64 {
        let board = ExtractedBoard::from_auto(BOARD).expect("parse STM32 board");
        let lib = ModelLibrary::builtin();
        let bound = bind_board(&board, &lib);

        let mut engine = HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("build engine");
        engine.scheduler_mut().chunk_s = 5e-3;

        // ── The declarative sensor under test (NOT the hand-coded Lm75) ──
        //
        // This is the crucial substitution: the bus carries a `RegisterMapSensor`
        // parsed from TOML, not the hard-coded `Lm75`.  A green test here means
        // the interpreter genuinely drove the firmware through the same path.
        let mut sensor = RegisterMapSensor::from_toml(LM75_TOML)
            .unwrap_or_else(|e| panic!("declarative LM75 from_toml failed: {e}"));
        sensor.set_input("temperature_c", temp_c);

        let bus = Arc::new(Mutex::new(I2cBus::new("U2").with_slave(Box::new(sensor))));
        engine.scheduler_mut().attach_i2c_bus(bus.clone());

        let frame_dt = 5e-3_f64;
        let n = ms as usize;
        let mut last_flag = 0.0_f64;
        for _ in 0..n {
            let frame = engine.step(frame_dt);
            if let Some(&v) = frame.net_voltages.get("FLAG") {
                last_flag = v;
            }
        }
        last_flag
    };

    // Below threshold: firmware should hold FLAG LOW.
    let cold = run_at_temp(20.0, 60);
    assert!(
        cold < 1.0,
        "at 20 °C (declarative LM75) the FLAG net should be LOW, got {cold:.3} V"
    );

    // Above threshold: firmware should drive FLAG HIGH.
    let hot = run_at_temp(40.0, 60);
    assert!(
        hot > 2.0,
        "at 40 °C (declarative LM75) the FLAG net should be HIGH, got {hot:.3} V"
    );

    // Sweep across the 30 °C threshold.
    let sweep = [10.0_f64, 25.0, 29.0, 31.0, 35.0, 50.0, 28.0, 15.0];
    for &t in &sweep {
        let v = run_at_temp(t, 50);
        let high = v > 2.0;
        let expect = t >= 30.0;
        assert_eq!(
            high,
            expect,
            "at {t} °C (declarative LM75): expected FLAG {}, got {:.3} V",
            if expect { "HIGH" } else { "LOW" },
            v
        );
    }

    eprintln!(
        "declarative_lm75_i2c_thermostat_cosim: PASS: \
         declarative LM75 drove the STM32 thermostat firmware correctly"
    );
}
