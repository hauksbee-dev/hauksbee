//! Proof: AVR firmware bit-bangs I2C on plain GPIOs and reads a spec-driven
//! MPU-6050 through the synchronous input responder (05 §1.5).
//!
//! The firmware (testdata/firmware/soft_i2c_sensor) toggles SCL/SDA on
//! PD2/PD3, deliberately NOT the ATmega328P's hardware TWI pins, with the
//! push-pull master waveform the [`SoftI2cResponder`] documents, sampling
//! every ACK and read bit with a plain `PIND` read one instruction after the
//! SCL rising edge. The slave is the shipped declarative MPU-6050 spec
//! (docs/hunts/specs/mpu6050.toml) on the existing [`I2cBus`]; the same
//! model class the hardware-TWI path serves, no parallel device.
//!
//! The firmware performs two classic pointered reads with repeated-START
//! framing (WHO_AM_I 0x75; TEMP_OUT 0x41, two-byte burst with a master ACK
//! between) and reports "<A|n>W<who>T<hi><lo>" over UART. The test asserts
//! WHO_AM_I = 0x68, every address byte ACKed, and, transport equality, that
//! the temperature bytes the firmware clocked in over pin edges are exactly
//! the bytes the sensor model holds for the driven temperature.

#![cfg(feature = "avr")]

use std::sync::{Arc, Mutex};

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::{HauksbeeEngine, I2cBus, RegisterMapSensor};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use hauksbee_server::engine::Engine;

/// The shipped declarative MPU-6050 spec (address 0x68, WHO_AM_I = 0x68).
const MPU6050_SPEC: &str = include_str!("../../../docs/hunts/specs/mpu6050.toml");

/// The temperature the test drives into the model.
const TEMP_C: f64 = 30.0;

/// Minimal board: U1 ATmega328P with the soft-I2C GPIOs on PD2/PD3 (DIP-28
/// pads 4, 5), power/ground wired, and pull-up resistors on SCL/SDA the way a
/// real I2C bus is dressed.
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "SOFT_SCL")
  (net 4 "SOFT_SDA")

  (module Package_QFP:TQFP-32_7x7mm_P0.8mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value ATmega328P (at 0 2) (layer F.Fab))
    (pad 7 smd rect (at -3 0) (net 2 "+5V"))
    (pad 8 smd rect (at -3 1) (net 1 "GND"))
    (pad 4 smd rect (at 3 0) (net 3 "SOFT_SCL"))
    (pad 5 smd rect (at 3 1) (net 4 "SOFT_SDA"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 4.7k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "SOFT_SCL"))
    (pad 2 thru_hole circle (at 2 0) (net 2 "+5V"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 102)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 4.7k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 4 "SOFT_SDA"))
    (pad 2 thru_hole circle (at 2 0) (net 2 "+5V"))
  )
)
"#;

fn firmware() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/soft_i2c_sensor/soft_i2c_sensor.hex")
}

#[test]
fn firmware_reads_i2c_sensor_over_bitbanged_gpios() {
    let fw = firmware();
    assert!(
        fw.exists(),
        "build the firmware first: make -C testdata/firmware/soft_i2c_sensor ({fw:?})"
    );

    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    let mut engine = HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("build engine");

    // The bytes the model holds for TEMP_OUT at the driven temperature; the
    // transport-equality oracle (the spec's raw = temp_c*340 - 12420.2 math
    // has its own unit proofs; this test proves the PIN-EDGE TRANSPORT).
    let mut sensor = RegisterMapSensor::from_toml(MPU6050_SPEC).expect("shipped spec validates");
    sensor.set_input("temp_c", TEMP_C);
    let expected_temp = sensor.register_bytes(0x41);
    assert_eq!(expected_temp.len(), 2, "TEMP_OUT is an i16_be register");

    // Wire the soft bus from the BOARD's nets (the 165/595-style trace).
    let sched = engine.scheduler_mut();
    let scl = sched.mcu_pin_for_net("SOFT_SCL").expect("SCL resolves");
    let sda = sched.mcu_pin_for_net("SOFT_SDA").expect("SDA resolves");
    assert_eq!(scl, ('D', 2), "pad 4 is PD2");
    assert_eq!(sda, ('D', 3), "pad 5 is PD3");

    let bus = Arc::new(Mutex::new(I2cBus::new("U2").with_slave(Box::new(sensor))));
    sched
        .attach_soft_i2c(bus, scl, sda)
        .expect("attach soft I2C");

    // Run the co-sim, accumulating the firmware's UART report.
    let mut out = Vec::new();
    for _ in 0..150 {
        let frame = engine.step(1e-3);
        if let Some(bytes) = frame.uart.get("U1") {
            out.extend_from_slice(bytes);
        }
        if out.iter().filter(|&&b| b == b'\n').count() >= 2 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&out);
    let mut lines = text.lines().filter(|l| !l.is_empty());
    let line = lines
        .next()
        .unwrap_or_else(|| panic!("firmware produced no UART report; raw = {text:?}"));

    let expected = format!("AW68T{:02X}{:02X}", expected_temp[0], expected_temp[1]);
    assert_eq!(
        line, expected,
        "soft-I2C readback must ACK and match the model's registers; raw = {text:?}"
    );
    // The report repeats: transaction framing survives STOP after STOP.
    if let Some(second) = lines.next() {
        assert_eq!(second, expected);
    }
}
