//! Official-parts batch env2 (pressure, gas, light and IR sensors): the bus models the cards carry,
//! driven byte by byte through the same I2C slave interface the firmware sees.
//!
//! Loading each card's spec through `RegisterMapSensor::from_toml` also compiles every expression
//! with `evalexpr`, so a card whose arithmetic the interpreter cannot parse fails here.

use hauksbee_cosim::peripherals::{I2cSlave, RegisterMapSensor};
use hauksbee_models::{ComponentQuery, ModelLibrary, PeripheralSpec};

/// The bus model of the card an ordering code resolves to.
fn sensor(code: &str) -> RegisterMapSensor {
    let res = ModelLibrary::builtin_shared().resolve(&ComponentQuery {
        value: Some(code.into()),
        mpn: Some(code.into()),
        ..Default::default()
    });
    let model = res
        .model
        .unwrap_or_else(|| panic!("{code} resolves to a card"));
    let Some(PeripheralSpec::RegisterMap { spec_toml, .. }) = model.peripheral else {
        panic!("{code}: no register_map peripheral");
    };
    RegisterMapSensor::from_toml(&spec_toml)
        .unwrap_or_else(|e| panic!("{code}: the spec does not load: {e}"))
}

/// A pointer write then a repeated-start read of `n` bytes.
fn read(s: &mut RegisterMapSensor, pointer: u8, n: usize) -> Vec<u8> {
    s.on_start(false);
    s.on_write(pointer);
    s.on_start(true);
    (0..n).map(|_| s.on_read()).collect()
}

/// One write transaction: pointer (or command) then payload.
fn write(s: &mut RegisterMapSensor, bytes: &[u8]) {
    s.on_start(false);
    for b in bytes {
        s.on_write(*b);
    }
}

/// The streamed result of a frame-reading device (no pointer).
fn frame(s: &mut RegisterMapSensor, n: usize) -> Vec<u8> {
    s.on_start(true);
    (0..n).map(|_| s.on_read()).collect()
}

#[test]
fn every_env2_card_spec_loads() {
    for code in [
        "BMP388",
        "BMP390",
        "BME680",
        "BME688",
        "TMP117AIDRVR",
        "TMP117AIYBGR",
        "TSL2591",
        "TSL2561CS",
        "TSL2561CL",
        "BH1750FVI",
        "MLX90614ESF-BCC-000-TU",
        "MLX90614ESF-ACC-000-TU",
    ] {
        let _ = sensor(code);
    }
}

#[test]
fn bosch_identity_and_readback() {
    let mut s = sensor("BMP388");
    assert_eq!(s.address(), 0x76);
    assert_eq!(read(&mut s, 0x00, 1), [0x50]);
    let mut s = sensor("BMP390");
    assert_eq!(read(&mut s, 0x00, 2), [0x60, 0x01]);
    let mut s = sensor("BME680");
    assert_eq!(read(&mut s, 0xD0, 1), [0x61]);
    // ctrl_meas (0x74) is writable and reads back what was written.
    write(&mut s, &[0x74, 0x25]);
    assert_eq!(read(&mut s, 0x74, 1), [0x25]);
    let mut s = sensor("BME688");
    assert_eq!(read(&mut s, 0xD0, 1), [0x61]);
    assert_eq!(read(&mut s, 0xF0, 1), [0x01]);
}

#[test]
fn tmp117_temperature_and_registers() {
    let mut s = sensor("TMP117AIDRVR");
    assert_eq!(s.address(), 0x48);
    assert_eq!(read(&mut s, 0x0F, 2), [0x01, 0x17], "device ID");
    // 7.8125 m degC per LSB: 25 C = 3200 = 0x0C80.
    s.set_input("temp_c", 25.0);
    assert_eq!(read(&mut s, 0x00, 2), [0x0C, 0x80]);
    s.set_input("temp_c", -25.0);
    assert_eq!(read(&mut s, 0x00, 2), [0xF3, 0x80]);
    // Temperature offset register adds 128 LSB = 1 C.
    s.set_input("temp_c", 25.0);
    write(&mut s, &[0x07, 0x00, 0x80]);
    assert_eq!(read(&mut s, 0x07, 2), [0x00, 0x80]);
    assert_eq!(read(&mut s, 0x00, 2), [0x0D, 0x00]);
    // Reset limits, then a write reads back.
    assert_eq!(read(&mut s, 0x02, 2), [0x60, 0x00], "THigh reset");
    assert_eq!(read(&mut s, 0x03, 2), [0x80, 0x00], "TLow reset");
    write(&mut s, &[0x02, 0x12, 0x34]);
    assert_eq!(read(&mut s, 0x02, 2), [0x12, 0x34]);
    // The read-only status bits 15:12 never come from the written value; data-ready (bit 13) is set.
    write(&mut s, &[0x01, 0xF4, 0x00]);
    assert_eq!(read(&mut s, 0x01, 2), [0x24, 0x00]);
}

#[test]
fn tmp117_wafer_package_has_the_same_bus_model() {
    let mut s = sensor("TMP117AIYBGR");
    assert_eq!(read(&mut s, 0x0F, 2), [0x01, 0x17]);
    s.set_input("temp_c", 0.0);
    assert_eq!(read(&mut s, 0x00, 2), [0x00, 0x00]);
}

#[test]
fn tsl2591_counts_follow_enable_and_integration_time() {
    let mut s = sensor("TSL2591");
    assert_eq!(s.address(), 0x29);
    assert_eq!(read(&mut s, 0xB2, 1), [0x50], "ID");
    s.set_input("ch0_counts", 1000.0);
    s.set_input("ch1_counts", 300.0);
    // Powered off: both channels read zero.
    assert_eq!(read(&mut s, 0xB4, 4), [0, 0, 0, 0]);
    // PON | AEN.
    write(&mut s, &[0xA0, 0x03]);
    assert_eq!(read(&mut s, 0xA0, 1), [0x03]);
    assert_eq!(read(&mut s, 0xB3, 1), [0x01], "STATUS AVALID");
    assert_eq!(read(&mut s, 0xB4, 4), [0xE8, 0x03, 0x2C, 0x01]);
    // ATIME 100 ms saturates at 37888 counts; 600 ms at 65535.
    write(&mut s, &[0xA1, 0x00]);
    s.set_input("ch0_counts", 50000.0);
    assert_eq!(read(&mut s, 0xB4, 2), [0x00, 0x94]);
    write(&mut s, &[0xA1, 0x05]);
    s.set_input("ch0_counts", 70000.0);
    assert_eq!(read(&mut s, 0xB4, 2), [0xFF, 0xFF]);
    assert_eq!(read(&mut s, 0xA1, 1), [0x05]);
}

#[test]
fn tsl2561_byte_and_word_framing() {
    let mut s = sensor("TSL2561FN");
    assert_eq!(s.address(), 0x39, "floating ADDR SEL default");
    assert_eq!(read(&mut s, 0x8A, 1), [0x10], "ID: part 0001, REVNO 0");
    s.set_input("ch0_counts", 1000.0);
    s.set_input("ch1_counts", 300.0);
    assert_eq!(read(&mut s, 0x8C, 4), [0, 0, 0, 0], "powered down");
    write(&mut s, &[0x80, 0x03]);
    assert_eq!(read(&mut s, 0x80, 1), [0x03]);
    // Byte framing reads the data registers a byte at a time; word framing reads them as words.
    assert_eq!(read(&mut s, 0x8C, 4), [0xE8, 0x03, 0x2C, 0x01]);
    assert_eq!(read(&mut s, 0xAC, 2), [0xE8, 0x03]);
    assert_eq!(read(&mut s, 0xAE, 2), [0x2C, 0x01]);
    // 13.7 ms integration saturates at 5047 counts.
    write(&mut s, &[0x81, 0x00]);
    s.set_input("ch0_counts", 60000.0);
    assert_eq!(read(&mut s, 0x8C, 2), [0xB7, 0x13]);
    assert_eq!(read(&mut s, 0x81, 1), [0x00]);
    // A threshold word written with word framing reads back through both framings.
    write(&mut s, &[0xA2, 0x34, 0x12]);
    assert_eq!(read(&mut s, 0x82, 2), [0x34, 0x12]);
    assert_eq!(read(&mut s, 0xA2, 2), [0x34, 0x12]);
    let mut cl = sensor("TSL2561CL");
    assert_eq!(read(&mut cl, 0x8A, 1), [0x10]);
}

#[test]
fn bh1750_opcodes_and_result_frame() {
    let mut s = sensor("BH1750FVI");
    assert_eq!(s.address(), 0x23);
    s.set_input("illuminance_lx", 100.0);
    // No measurement started: the result reads zero.
    assert_eq!(frame(&mut s, 2), [0x00, 0x00]);
    // Continuous H-resolution: lx x 1.2 x (MTreg / 69) counts.
    write(&mut s, &[0x01]);
    write(&mut s, &[0x10]);
    assert_eq!(frame(&mut s, 2), [0x00, 120]);
    // H-resolution mode 2 doubles the count resolution.
    write(&mut s, &[0x11]);
    assert_eq!(frame(&mut s, 2), [0x00, 240]);
    // MTreg 138: high bits 0b100 (0x44), low bits 0b01010 (0x6A); the count doubles.
    write(&mut s, &[0x10]);
    write(&mut s, &[0x44]);
    write(&mut s, &[0x6A]);
    assert_eq!(frame(&mut s, 2), [0x00, 240]);
    // Reset clears the data register while powered on.
    write(&mut s, &[0x07]);
    assert_eq!(frame(&mut s, 2), [0x00, 0x00]);
    // One-time measurement; the count saturates at 65535.
    write(&mut s, &[0x20]);
    s.set_input("illuminance_lx", 100_000.0);
    assert_eq!(frame(&mut s, 2), [0xFF, 0xFF]);
}

/// The 16-bit value of a little-endian word.
fn word(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

#[test]
fn mlx90614_temperature_words_and_eeprom() {
    for code in ["MLX90614ESF-BCC-000-TU", "MLX90614ESF-ACC-000-TU"] {
        let mut s = sensor(code);
        assert_eq!(s.address(), 0x5A);
        assert_eq!(read(&mut s, 0xF0, 2), [0x10, 0x00], "flags: INIT");
        // 0.02 K per LSB: 26.85 C = 300.00 K = 15000.
        s.set_input("ta_c", 26.85);
        s.set_input("to_c", 26.85);
        assert!(
            word(&read(&mut s, 0x06, 2)).abs_diff(15000) <= 1,
            "{code}: Ta"
        );
        assert!(
            word(&read(&mut s, 0x07, 2)).abs_diff(15000) <= 1,
            "{code}: To"
        );
        // Ta saturates at the linearisation limits, +125 C and -38.2 C.
        s.set_input("ta_c", 200.0);
        assert_eq!(read(&mut s, 0x06, 2), [0xC4, 0x4D]);
        s.set_input("ta_c", -100.0);
        assert_eq!(read(&mut s, 0x06, 2), [0xE4, 0x2D]);
        // To saturates at -70 C and 382.19 C.
        s.set_input("to_c", 1000.0);
        assert_eq!(read(&mut s, 0x07, 2), [0xFF, 0x7F]);
        s.set_input("to_c", -200.0);
        assert_eq!(read(&mut s, 0x07, 2), [0xAD, 0x27]);
        // Emissivity EEPROM word: 1.0 at the factory, writable.
        assert_eq!(read(&mut s, 0x24, 2), [0xFF, 0xFF]);
        write(&mut s, &[0x24, 0x00, 0x80]);
        assert_eq!(read(&mut s, 0x24, 2), [0x00, 0x80]);
    }
}
