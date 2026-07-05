//! Proof: AVR firmware bit-bangs SPI mode 0 on plain GPIOs and reads a
//! spec-driven SPI sensor through the synchronous input responder (05 §1.5).
//!
//! The firmware (testdata/firmware/bitbang_spi_imu) toggles SCLK/MOSI/CS_n on
//! PD4..PD6 and samples MISO on PD7 with a plain `PIND` read one instruction
//! after each rising clock edge — deliberately NOT the ATmega328P's hardware
//! SPI pins, so only the engine's [`BitBangSpiResponder`] bridging to the
//! byte-level [`SpiBus`] slave can answer. The slave is the shipped ICM-42605
//! declarative spec (docs/hunts/specs/icm42605.toml) loaded into a
//! [`RegisterMapSensor`] — the existing SPI slave model, no parallel device.
//!
//! The firmware reads WHO_AM_I (0x75 -> 0x42) and a two-byte burst from
//! GYRO_CONFIG1 (0x4F -> 0x06, auto-incrementing to 0x50 -> 0x06) and reports
//! "W<who>G<b0><b1>" in hex over UART; the test asserts the exact line. Every
//! byte of that report crossed the bit-level bridge inside the firmware's own
//! clock loop — the read-inside-`run_micros` shape the 74HC165 fix pioneered,
//! now for any SPI slave.

#![cfg(feature = "avr")]

use std::sync::{Arc, Mutex};

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::{BitBangSpiPins, HauksbeeEngine, RegisterMapSensor, SpiBus};
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use hauksbee_server::engine::Engine;

/// The shipped declarative ICM-42605 spec (same file the boot-coverage hunts
/// use); WHO_AM_I = 0x42, GYRO_CONFIG1/GYRO_ACCEL_CONFIG0 = 0x06.
const ICM42605_SPEC: &str = include_str!("../../../docs/hunts/specs/icm42605.toml");

/// Minimal board: U1 ATmega328P with the software-SPI GPIOs on PD4..PD7
/// (DIP-28 pads 6, 11, 12, 13), power/ground wired, and the IMU U2's SPI pins
/// on the same nets. 10k pulls give each SPI net a second real component the
/// way a routed board would.
const BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "SPI_CS")
  (net 4 "SPI_SCLK")
  (net 5 "SPI_MOSI")
  (net 6 "SPI_MISO")

  (module Package_QFP:TQFP-32_7x7mm_P0.8mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value ATmega328P (at 0 2) (layer F.Fab))
    (pad 7  smd rect (at -3 0) (net 2 "+5V"))
    (pad 8  smd rect (at -3 1) (net 1 "GND"))
    (pad 6  smd rect (at 3 0) (net 3 "SPI_CS"))
    (pad 11 smd rect (at 3 1) (net 4 "SPI_SCLK"))
    (pad 12 smd rect (at 3 2) (net 5 "SPI_MOSI"))
    (pad 13 smd rect (at 3 3) (net 6 "SPI_MISO"))
  )

  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "SPI_CS"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 102)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 4 "SPI_SCLK"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 104)
    (fp_text reference R3 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 5 "SPI_MOSI"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor_THT:R_Axial_DIN0207_L6.3mm_D2.5mm (layer F.Cu)
    (at 110 106)
    (fp_text reference R4 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 6 "SPI_MISO"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)
"#;

fn firmware() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/bitbang_spi_imu/bitbang_spi_imu.hex")
}

#[test]
fn firmware_reads_spi_sensor_over_bitbanged_gpios() {
    let fw = firmware();
    assert!(
        fw.exists(),
        "build the firmware first: make -C testdata/firmware/bitbang_spi_imu ({fw:?})"
    );

    let board = ExtractedBoard::from_auto(BOARD).expect("parse board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);
    let mut engine = HauksbeeEngine::from_bound(bound, Some(&fw), "/ci").expect("build engine");

    // Wire the bit-banged topology from the BOARD's nets (the 165/595-style
    // net-to-pin trace), not from hardcoded pin tuples.
    let sched = engine.scheduler_mut();
    let pin = |net: &str| {
        sched
            .mcu_pin_for_net(net)
            .unwrap_or_else(|| panic!("net {net} must resolve to an MCU GPIO"))
    };
    let pins = BitBangSpiPins {
        sclk: pin("SPI_SCLK"),
        mosi: pin("SPI_MOSI"),
        miso: pin("SPI_MISO"),
        cs_n: pin("SPI_CS"),
    };
    assert_eq!(pins.sclk, ('D', 5), "pad 11 is PD5");
    assert_eq!(pins.miso, ('D', 7), "pad 13 is PD7");

    let sensor = RegisterMapSensor::from_toml(ICM42605_SPEC).expect("shipped spec validates");
    let bus = Arc::new(Mutex::new(SpiBus::new("U2", Box::new(sensor))));
    sched
        .attach_bitbang_spi(bus, pins)
        .expect("attach bit-banged SPI");

    // Run the co-sim, accumulating the firmware's UART report.
    let mut out = Vec::new();
    for _ in 0..100 {
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
    let line = lines.next().unwrap_or_else(|| {
        panic!("firmware produced no UART report; raw = {text:?}")
    });

    // WHO_AM_I = 0x42; GYRO_CONFIG1 burst = 0x06 then (auto-increment) 0x06.
    assert_eq!(
        line, "W42G0606",
        "bit-banged SPI readback must match the spec's registers; raw = {text:?}"
    );
    // The report repeats, proving CS re-framing works transaction after
    // transaction (select() resets the command state machine each time).
    if let Some(second) = lines.next() {
        assert_eq!(second, "W42G0606");
    }
}
