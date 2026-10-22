//! Official-parts batch motion_power: the register maps of the new cards, run as the
//! co-simulation peripheral that firmware talks to over I2C.
//!
//! Each test resolves a card by its ordering code, builds a [`RegisterMapSensor`] from
//! the card's own `spec_toml`, and drives it through the I2C slave interface: identity
//! reads, write-then-read-back, and data registers answering from the stimulus inputs.

use hauksbee_cosim::peripherals::{I2cSlave, RegisterMapSensor};
use hauksbee_models::schema::PeripheralSpec;
use hauksbee_models::{ComponentQuery, ModelLibrary};

/// The card's sensor, built exactly as the binder builds it.
fn sensor(value: &str, id: &str) -> RegisterMapSensor {
    let model = ModelLibrary::builtin_shared()
        .resolve(&ComponentQuery {
            value: Some(value.into()),
            mpn: Some(value.into()),
            ..Default::default()
        })
        .model
        .unwrap_or_else(|| panic!("{value} did not resolve"));
    assert_eq!(model.id, id);
    match model.peripheral {
        Some(PeripheralSpec::RegisterMap { spec_toml, .. }) => {
            RegisterMapSensor::from_toml(&spec_toml).expect("the card's spec builds")
        }
        other => panic!("{id} carries {other:?}, not a register map"),
    }
}

/// An I2C register read: pointer write, repeated START, then `n` bytes.
fn read(s: &mut RegisterMapSensor, reg: u8, n: usize) -> Vec<u8> {
    s.on_start(false);
    s.on_write(reg);
    s.on_start(true);
    (0..n).map(|_| s.on_read()).collect()
}

fn read_u8(s: &mut RegisterMapSensor, reg: u8) -> u8 {
    read(s, reg, 1)[0]
}

fn read_i16_le(s: &mut RegisterMapSensor, reg: u8) -> i16 {
    let b = read(s, reg, 2);
    i16::from_le_bytes([b[0], b[1]])
}

fn read_i16_be(s: &mut RegisterMapSensor, reg: u8) -> i16 {
    let b = read(s, reg, 2);
    i16::from_be_bytes([b[0], b[1]])
}

fn write(s: &mut RegisterMapSensor, reg: u8, value: u8) {
    s.on_start(false);
    s.on_write(reg);
    s.on_write(value);
}

#[test]
fn adxl345_answers_devid_and_reports_gravity_once_measuring() {
    let mut s = sensor("ADXL345BCCZ", "adxl345");
    assert_eq!(s.address(), 0x53);
    assert_eq!(read_u8(&mut s, 0x00), 0xE5, "DEVID");
    assert_eq!(read_u8(&mut s, 0x2C), 0x0A, "BW_RATE reset");
    // Standby after reset: the data registers read zero.
    assert_eq!(read_i16_le(&mut s, 0x36), 0);
    // Measure bit set (POWER_CTL bit 3), full resolution (DATA_FORMAT bit 3).
    write(&mut s, 0x2D, 0x08);
    write(&mut s, 0x31, 0x08);
    assert_eq!(read_u8(&mut s, 0x2D), 0x08, "POWER_CTL reads back");
    assert_eq!(read_u8(&mut s, 0x31), 0x08, "DATA_FORMAT reads back");
    assert_eq!(
        read_i16_le(&mut s, 0x36),
        256,
        "1 g on Z at 256 LSB/g in full resolution"
    );
    assert_eq!(read_i16_le(&mut s, 0x32), 0);
    s.set_input("accel_x_g", -0.5);
    assert_eq!(read_i16_le(&mut s, 0x32), -128);
    assert_eq!(
        read_u8(&mut s, 0x30) & 0x80,
        0x80,
        "DATA_READY once measuring"
    );
}

#[test]
fn mpu6050_identifies_wakes_and_scales_by_full_scale_range() {
    let mut s = sensor("MPU-6050", "mpu6050");
    assert_eq!(s.address(), 0x68);
    assert_eq!(read_u8(&mut s, 0x75), 0x68, "WHO_AM_I");
    assert_eq!(read_u8(&mut s, 0x6B), 0x40, "PWR_MGMT_1 resets asleep");
    assert_eq!(read_i16_be(&mut s, 0x3F), 0, "asleep: no data");
    // Clear SLEEP; the clock source is left at the internal oscillator.
    write(&mut s, 0x6B, 0x00);
    assert_eq!(read_u8(&mut s, 0x6B), 0x00);
    assert_eq!(read_i16_be(&mut s, 0x3F), 16384, "1 g at +-2 g");
    write(&mut s, 0x1C, 0x18); // AFS_SEL = 3, +-16 g
    assert_eq!(read_u8(&mut s, 0x1C), 0x18);
    assert_eq!(read_i16_be(&mut s, 0x3F), 2048, "1 g at +-16 g");
    s.set_input("gyro_x_dps", 10.0);
    assert_eq!(read_i16_be(&mut s, 0x43), 1310, "10 dps at 131 LSB/(deg/s)");
    // DEVICE_RESET puts the register back to its reset value and the bit reads clear.
    write(&mut s, 0x6B, 0x80);
    assert_eq!(read_u8(&mut s, 0x6B), 0x40);
}

#[test]
fn lsm303agr_accelerometer_left_justifies_by_operating_mode() {
    let mut s = sensor("LSM303AGR", "lsm303agr");
    assert_eq!(s.address(), 0x19);
    assert_eq!(read_u8(&mut s, 0x0F), 0x33, "WHO_AM_I_A");
    assert_eq!(read_u8(&mut s, 0x20), 0x07, "CTRL_REG1_A reset");
    assert_eq!(read_i16_le(&mut s, 0x2C), 0, "power-down");
    // 100 Hz, all axes (ODR = 0101), normal mode (10-bit): 1 g / 3.9 mg x 64.
    write(&mut s, 0x20, 0x57);
    assert_eq!(read_i16_le(&mut s, 0x2C), 16410);
    assert_eq!(read_u8(&mut s, 0x27), 0x0F, "ZYXDA and all axes");
    // High-resolution (12-bit): 1 g / 0.98 mg x 16.
    write(&mut s, 0x23, 0x08);
    assert_eq!(read_i16_le(&mut s, 0x2C), 16327);
    // Z disabled reads zero.
    write(&mut s, 0x20, 0x53);
    assert_eq!(read_i16_le(&mut s, 0x2C), 0);
    assert_eq!(read_i16_le(&mut s, 0x28), 0);
}

#[test]
fn icm20948_wakes_from_reset_state_and_scales_by_bank2_full_scale() {
    let mut s = sensor("ICM-20948", "icm20948");
    assert_eq!(s.address(), 0x68);
    assert_eq!(read_u8(&mut s, 0x00), 0xEA, "WHO_AM_I");
    assert_eq!(read_u8(&mut s, 0x06), 0x41, "PWR_MGMT_1 resets asleep");
    assert_eq!(read_i16_be(&mut s, 0x31), 0, "asleep: no data");
    write(&mut s, 0x06, 0x01); // wake, auto-select clock
    assert_eq!(read_u8(&mut s, 0x06), 0x01);
    assert_eq!(read_i16_be(&mut s, 0x31), 16384, "1 g at +-2 g");
    write(&mut s, 0x14, 0x06); // ACCEL_FS_SEL = 3, +-16 g (bank-2 register)
    assert_eq!(read_i16_be(&mut s, 0x31), 2048, "1 g at +-16 g");
    s.set_input("gyro_x_dps", 10.0);
    assert_eq!(read_i16_be(&mut s, 0x33), 1310, "10 dps at 131 LSB/dps");
    s.set_input("temp_c", 36.0);
    assert_eq!(
        read_i16_be(&mut s, 0x39),
        5008,
        "15 degC above 21 at 333.87"
    );
    // Disabling the gyroscope in PWR_MGMT_2 zeroes it.
    write(&mut s, 0x07, 0x07);
    assert_eq!(read_i16_be(&mut s, 0x33), 0);
    // DEVICE_RESET auto-clears and the chip is back asleep.
    write(&mut s, 0x06, 0x80);
    assert_eq!(read_u8(&mut s, 0x06), 0x41);
    assert_eq!(read_i16_be(&mut s, 0x31), 0);
}

#[test]
fn lsm6dso_and_lsm6dsox_share_an_identity_and_gate_data_on_odr() {
    for (code, id) in [("LSM6DSO", "lsm6dso"), ("LSM6DSOX", "lsm6dsox")] {
        let mut s = sensor(code, id);
        assert_eq!(s.address(), 0x6A, "{id}");
        assert_eq!(read_u8(&mut s, 0x0F), 0x6C, "{id} WHO_AM_I");
        assert_eq!(read_u8(&mut s, 0x12), 0x04, "{id} CTRL3_C reset");
        assert_eq!(read_u8(&mut s, 0x02), 0x3F, "{id} PIN_CTRL reset");
        assert_eq!(read_u8(&mut s, 0x18), 0xE0, "{id} CTRL9_XL reset");
        assert_eq!(read_i16_le(&mut s, 0x2C), 0, "{id}: power-down, no data");
        // 104 Hz accelerometer (ODR_XL = 0100), +-2 g: 1 g / 0.061 mg per LSB.
        write(&mut s, 0x10, 0x40);
        assert_eq!(read_u8(&mut s, 0x10), 0x40);
        assert_eq!(read_i16_le(&mut s, 0x2C), 16393, "{id} 1 g");
        assert_eq!(read_u8(&mut s, 0x1E) & 1, 1, "{id} XLDA");
        // +-4 g (FS_XL = 10) halves the count.
        write(&mut s, 0x10, 0x48);
        assert_eq!(read_i16_le(&mut s, 0x2C), 8197, "{id} 1 g at +-4 g");
        // Gyroscope 104 Hz at 500 dps: 17.5 mdps per LSB.
        write(&mut s, 0x11, 0x44);
        s.set_input("gyro_z_dps", 1.75);
        assert_eq!(read_i16_le(&mut s, 0x26), 100, "{id} 1.75 dps");
        // SW_RESET reads back clear (self-clearing), BOOT likewise.
        write(&mut s, 0x12, 0x05);
        assert_eq!(read_u8(&mut s, 0x12), 0x04, "{id} SW_RESET self-clears");
    }
}

fn read_u16_be(s: &mut RegisterMapSensor, reg: u8) -> u16 {
    let b = read(s, reg, 2);
    u16::from_be_bytes([b[0], b[1]])
}

fn write_u16_be(s: &mut RegisterMapSensor, reg: u8, value: u16) {
    s.on_start(false);
    s.on_write(reg);
    for byte in value.to_be_bytes() {
        s.on_write(byte);
    }
}

#[test]
fn ds3231_resets_to_the_printed_time_and_reports_the_input_temperature() {
    let mut s = sensor("DS3231SN", "ds3231");
    assert_eq!(s.address(), 0x68);
    // Figure 1: 01/01/00 01 00:00:00 after the first power-up.
    assert_eq!(read(&mut s, 0x00, 7), [0, 0, 0, 1, 1, 1, 0]);
    // The time registers read back what firmware writes (BCD 12:34:56).
    write(&mut s, 0x00, 0x56);
    write(&mut s, 0x01, 0x34);
    write(&mut s, 0x02, 0x12);
    assert_eq!(read(&mut s, 0x00, 3), [0x56, 0x34, 0x12]);
    // Control resets to 1Ch; the CONV bit written by a driver reads back clear.
    assert_eq!(read_u8(&mut s, 0x0E), 0x1C);
    write(&mut s, 0x0E, 0x3C);
    assert_eq!(read_u8(&mut s, 0x0E), 0x1C, "CONV self-clears");
    // Control/Status: OSF and EN32kHz set.
    assert_eq!(read_u8(&mut s, 0x0F), 0x88);
    write(&mut s, 0x0F, 0x08);
    assert_eq!(read_u8(&mut s, 0x0F), 0x08, "OSF cleared by firmware");
    // Temperature: 25 degC, then 25.25 and -10.5 degC from the stimulus.
    assert_eq!(read(&mut s, 0x11, 2), [25, 0x00]);
    s.set_input("temp_c", 25.25);
    assert_eq!(read(&mut s, 0x11, 2), [25, 0x40]);
    s.set_input("temp_c", -10.5);
    assert_eq!(read(&mut s, 0x11, 2), [0xF5, 0x80]);
}

#[test]
fn ds1307_starts_halted_and_keeps_the_battery_ram() {
    let mut s = sensor("DS1307Z+", "ds1307");
    assert_eq!(s.address(), 0x68);
    assert_eq!(read_u8(&mut s, 0x00), 0x80, "CH set on first power-up");
    assert_eq!(read(&mut s, 0x03, 3), [1, 1, 1]);
    assert_eq!(read_u8(&mut s, 0x07), 0x03, "control resets to RS = 11");
    write(&mut s, 0x00, 0x00);
    assert_eq!(read_u8(&mut s, 0x00), 0x00, "oscillator started");
    write(&mut s, 0x08, 0xA5);
    write(&mut s, 0x3F, 0x5A);
    assert_eq!(read_u8(&mut s, 0x08), 0xA5);
    assert_eq!(read_u8(&mut s, 0x3F), 0x5A);
    assert_eq!(read_u8(&mut s, 0x09), 0x00);
}

#[test]
fn ina226_identifies_resets_and_scales_shunt_bus_current_and_power() {
    let mut s = sensor("INA226", "ina226");
    assert_eq!(s.address(), 0x40);
    assert_eq!(read_u16_be(&mut s, 0xFE), 0x5449, "manufacturer ID");
    assert_eq!(read_u16_be(&mut s, 0xFF), 0x2260, "die ID");
    assert_eq!(read_u16_be(&mut s, 0x00), 0x4127, "CONFIG reset");
    // Current and power read zero until the calibration register is written.
    s.set_input("shunt_mv", 10.0);
    s.set_input("bus_v", 12.0);
    assert_eq!(read_u16_be(&mut s, 0x01), 4000, "10 mV at 2.5 uV per LSB");
    assert_eq!(read_u16_be(&mut s, 0x02), 9600, "12 V at 1.25 mV per LSB");
    assert_eq!(read_u16_be(&mut s, 0x04), 0);
    assert_eq!(read_u16_be(&mut s, 0x03), 0);
    // Equations 3 and 4 with CAL = 2560 (the datasheet's worked example).
    write_u16_be(&mut s, 0x05, 2560);
    assert_eq!(read_u16_be(&mut s, 0x05), 2560);
    assert_eq!(read_u16_be(&mut s, 0x04), 5000, "4000 x 2560 / 2048");
    assert_eq!(read_u16_be(&mut s, 0x03), 2400, "5000 x 9600 / 20000");
    // A negative shunt voltage is two's complement.
    s.set_input("shunt_mv", -10.0);
    assert_eq!(read_u16_be(&mut s, 0x01), 0xF060);
    s.set_input("shunt_mv", 10.0);
    // Mask/Enable: the function bits read back and CVRF reads set in continuous mode.
    assert_eq!(read_u16_be(&mut s, 0x06), 0x0008);
    write_u16_be(&mut s, 0x06, 0x8000);
    assert_eq!(read_u16_be(&mut s, 0x06), 0x8008);
    write_u16_be(&mut s, 0x07, 0x1234);
    assert_eq!(read_u16_be(&mut s, 0x07), 0x1234);
    // Power-down mode (MODE = 000) reads zero measurements.
    write_u16_be(&mut s, 0x00, 0x4120);
    assert_eq!(read_u16_be(&mut s, 0x01), 0);
    assert_eq!(read_u16_be(&mut s, 0x02), 0);
    // RST self-clears and returns the POR state.
    write_u16_be(&mut s, 0x00, 0x8000);
    assert_eq!(read_u16_be(&mut s, 0x00), 0x4127);
    assert_eq!(read_u16_be(&mut s, 0x05), 0);
    assert_eq!(read_u16_be(&mut s, 0x01), 0);
}
