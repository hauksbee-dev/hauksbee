//! Official-parts batch env1: the register-map specs shipped inside the temperature and humidity
//! sensor cards, driven as an I2C slave. The spec is taken from the resolved built-in card, so the
//! test exercises the bytes a board would actually bind.

use hauksbee_cosim::{I2cBus, RegisterMapSensor};
use hauksbee_mcu::I2cEvent;
use hauksbee_models::{ComponentQuery, ModelLibrary, PeripheralSpec};

/// The embedded sensor spec of the card a part number resolves to.
fn card_spec(mpn: &str) -> String {
    let m = ModelLibrary::builtin_shared()
        .resolve(&ComponentQuery {
            value: Some(mpn.into()),
            mpn: Some(mpn.into()),
            ..Default::default()
        })
        .model
        .unwrap_or_else(|| panic!("{mpn} did not resolve"));
    match m.peripheral {
        Some(PeripheralSpec::RegisterMap { spec_toml, .. }) => spec_toml,
        other => panic!("{mpn} ({}) has no register-map peripheral: {other:?}", m.id),
    }
}

fn sensor(mpn: &str) -> RegisterMapSensor {
    RegisterMapSensor::from_toml(&card_spec(mpn)).unwrap_or_else(|e| panic!("{mpn}: {e:?}"))
}

fn bus(s: RegisterMapSensor) -> I2cBus {
    I2cBus::new("I2C").with_slave(Box::new(s))
}

/// START (write), the bytes, STOP.
fn write(bus: &mut I2cBus, addr: u8, bytes: &[u8]) {
    bus.dispatch(I2cEvent::Start { addr, read: false });
    for &data in bytes {
        bus.dispatch(I2cEvent::Write { addr, data });
    }
    bus.dispatch(I2cEvent::Stop { addr });
}

/// START (read), `n` bytes, STOP.
fn read(bus: &mut I2cBus, addr: u8, n: usize) -> Vec<u8> {
    bus.dispatch(I2cEvent::Start { addr, read: true });
    let out = (0..n)
        .map(|_| bus.dispatch(I2cEvent::Read { addr }).expect("slave byte"))
        .collect();
    bus.dispatch(I2cEvent::Stop { addr });
    out
}

/// Whether the bus models a device at `addr`.
fn acks(bus: &I2cBus, addr: u8) -> bool {
    bus.addresses().contains(&addr)
}

/// Reference CRC-8: polynomial 0x31, MSB first, no reflection, no final XOR.
fn crc8(data: &[u8], init: u8) -> u8 {
    let mut crc = init;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x31
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[test]
fn crc8_reference_matches_the_datasheet_vectors() {
    assert_eq!(crc8(&[0xBE, 0xEF], 0xFF), 0x92); // SHT4x Table 7
    assert_eq!(crc8(&[0x68, 0x3A], 0x00), 0x7C); // SHT21 datasheet example
    assert_eq!(crc8(&[0xDC], 0x00), 0x79); // HTU21D datasheet example
    assert_eq!(crc8(&[0x4E, 0x85], 0x00), 0x6B); // HTU21D datasheet example
}

fn sht4x_raw_t(t: f64) -> u16 {
    ((t + 45.0) * 65535.0 / 175.0).round().clamp(0.0, 65535.0) as u16
}

fn sht4x_raw_rh(rh: f64) -> u16 {
    ((rh + 6.0) * 65535.0 / 125.0).round().clamp(0.0, 65535.0) as u16
}

fn assert_sht4x_frame(frame: &[u8], t: f64, rh: f64) {
    let st = sht4x_raw_t(t).to_be_bytes();
    let srh = sht4x_raw_rh(rh).to_be_bytes();
    assert_eq!(
        frame,
        [
            st[0],
            st[1],
            crc8(&st, 0xFF),
            srh[0],
            srh[1],
            crc8(&srh, 0xFF)
        ],
        "at {t} C / {rh} %RH"
    );
}

#[test]
fn sht4x_measurement_follows_the_inputs_with_crc() {
    for mpn in ["SHT40-AD1B-R2", "SHT41-AD1B-R2", "SHT45-AD1B-R3"] {
        for &(t, rh) in &[
            (25.0, 50.0),
            (-40.0, 0.0),
            (125.0, 100.0),
            (0.0, 10.0),
            (36.6, 63.2),
        ] {
            let mut s = sensor(mpn);
            s.set_input("temperature_c", t);
            s.set_input("humidity_pct", rh);
            let mut b = bus(s);
            for cmd in [0xFD_u8, 0xF6, 0xE0] {
                write(&mut b, 0x44, &[cmd]);
                let frame = read(&mut b, 0x44, 6);
                assert_sht4x_frame(&frame, t, rh);
            }
        }
    }
}

#[test]
fn sht4x_defaults_are_25c_50pct_and_match_the_formula() {
    let mut b = bus(sensor("SHT40-AD1B-R2"));
    write(&mut b, 0x44, &[0xFD]);
    let frame = read(&mut b, 0x44, 6);
    assert_eq!(frame[0..2], [0x66, 0x66]);
    assert_eq!(frame[3..5], [0x72, 0xB0]);
    assert_sht4x_frame(&frame, 25.0, 50.0);
}

#[test]
fn sht4x_heater_commands_return_a_measurement_and_reset_clears_it() {
    let mut b = bus(sensor("SHT45-AD1B-R2"));
    for cmd in [0x39_u8, 0x32, 0x2F, 0x24, 0x1E, 0x15] {
        write(&mut b, 0x44, &[cmd]);
        assert_sht4x_frame(&read(&mut b, 0x44, 6), 25.0, 50.0);
    }
    write(&mut b, 0x44, &[0x94]);
    assert_eq!(read(&mut b, 0x44, 6), vec![0xFF; 6]);
}

#[test]
fn sht4x_address_letter_sets_the_i2c_address() {
    for (mpn, addr, other) in [
        ("SHT40-AD1B-R2", 0x44, 0x45),
        ("SHT40-BD1B-R2", 0x45, 0x44),
        ("SHT40-CD1B-R3", 0x46, 0x44),
    ] {
        let mut b = bus(sensor(mpn));
        assert!(acks(&b, addr), "{mpn} must answer at {addr:#x}");
        assert!(!acks(&b, other), "{mpn} must not answer at {other:#x}");
        write(&mut b, addr, &[0xFD]);
        assert_sht4x_frame(&read(&mut b, addr, 6), 25.0, 50.0);
    }
}
