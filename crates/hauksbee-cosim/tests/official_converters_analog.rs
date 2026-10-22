//! Official-parts batch `converters_analog`: the register maps shipped inside the cards
//! (ADS1115, ADS1015, MCP4725) executed over a modelled I2C bus, from the card's own
//! `spec_toml` rather than a copy.

use hauksbee_cosim::peripherals::{I2cBus, RegisterMapSensor};
use hauksbee_mcu::I2cEvent;
use hauksbee_models::{ComponentQuery, ModelLibrary, PeripheralSpec};

/// The register-map sensor a card ships, resolved by ordering code.
fn card_sensor(code: &str) -> RegisterMapSensor {
    let resolution = ModelLibrary::builtin_shared().resolve(&ComponentQuery {
        value: Some(code.into()),
        mpn: Some(code.into()),
        ..Default::default()
    });
    let model = resolution
        .model
        .unwrap_or_else(|| panic!("{code} did not resolve"));
    match model.peripheral {
        Some(PeripheralSpec::RegisterMap { spec_toml, .. }) => {
            RegisterMapSensor::from_toml(&spec_toml)
                .unwrap_or_else(|e| panic!("{code}: card spec_toml does not compile: {e:?}"))
        }
        other => panic!("{code}: expected a register map, got {other:?}"),
    }
}

fn bus_with(sensor: RegisterMapSensor) -> I2cBus {
    I2cBus::new("I2C").with_slave(Box::new(sensor))
}

/// START, `bytes`, STOP.
fn write(bus: &mut I2cBus, addr: u8, bytes: &[u8]) {
    bus.dispatch(I2cEvent::Start { addr, read: false });
    for &data in bytes {
        bus.dispatch(I2cEvent::Write { addr, data });
    }
    bus.dispatch(I2cEvent::Stop { addr });
}

/// Pointer write then repeated-START read of `n` bytes.
fn read_pointered(bus: &mut I2cBus, addr: u8, reg: u8, n: usize) -> Vec<u8> {
    bus.dispatch(I2cEvent::Start { addr, read: false });
    bus.dispatch(I2cEvent::Write { addr, data: reg });
    bus.dispatch(I2cEvent::Start { addr, read: true });
    let out = (0..n)
        .map(|_| bus.dispatch(I2cEvent::Read { addr }).expect("byte"))
        .collect();
    bus.dispatch(I2cEvent::Stop { addr });
    out
}

/// A plain read of `n` bytes (command-framed devices stream a frame).
fn read_stream(bus: &mut I2cBus, addr: u8, n: usize) -> Vec<u8> {
    bus.dispatch(I2cEvent::Start { addr, read: true });
    let out = (0..n)
        .map(|_| bus.dispatch(I2cEvent::Read { addr }).expect("byte"))
        .collect();
    bus.dispatch(I2cEvent::Stop { addr });
    out
}

fn sensor_mut(bus: &mut I2cBus, addr: u8) -> &mut RegisterMapSensor {
    bus.slave_mut_t::<RegisterMapSensor>(addr)
        .expect("sensor at address")
}

fn reg16(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[0], bytes[1]])
}

#[test]
fn ads1115_reset_values_and_pga_steered_conversion() {
    for code in ["ADS1115IDGSR", "ADS1115IRUGR", "ADS1115IDYNR"] {
        let mut bus = bus_with(card_sensor(code));
        // Config register reset 8583h, Lo_thresh 8000h, Hi_thresh 7FFFh.
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x01, 2)),
            0x8583,
            "{code}"
        );
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x02, 2)),
            0x8000,
            "{code}"
        );
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x03, 2)),
            0x7FFF,
            "{code}"
        );

        // Reset MUX = 000 (AIN0 - AIN1), PGA = 010 (+-2.048 V): 1.024 V is half scale.
        sensor_mut(&mut bus, 0x48).set_input("a0", 1.024);
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x00, 2)),
            0x4000,
            "{code}"
        );

        // MUX = 100 (AIN0 - GND), PGA = 001 (+-4.096 V): config C383h.
        write(&mut bus, 0x48, &[0x01, 0xC3, 0x83]);
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x01, 2)),
            0xC383,
            "{code}"
        );
        sensor_mut(&mut bus, 0x48).set_input("a0", 2.048);
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x00, 2)),
            0x4000,
            "{code}"
        );

        // Over full scale clips at 7FFFh.
        sensor_mut(&mut bus, 0x48).set_input("a0", 5.0);
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x00, 2)),
            0x7FFF,
            "{code}"
        );
    }
}

#[test]
fn ads1115_pins_select_the_four_addresses() {
    // The card's spec_toml lists 0x48..0x4B; the binder picks one from the ADDR strap.
    let sensor = card_sensor("ADS1115IDGSR");
    let bus = bus_with(sensor);
    assert_eq!(bus.addresses(), vec![0x48]);
}

#[test]
fn ads1015_is_a_12_bit_left_justified_result() {
    for code in ["ADS1015IDGSR", "ADS1015IRUGR"] {
        let mut bus = bus_with(card_sensor(code));
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x01, 2)),
            0x8583,
            "{code}"
        );
        // 1.024 V at +-2.048 V is code 1024 in the 12-bit field, left-justified: 4000h.
        sensor_mut(&mut bus, 0x48).set_input("a0", 1.024);
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x00, 2)),
            0x4000,
            "{code}"
        );
        // Clips at the 12-bit positive full scale 7FFh, left-justified: 7FF0h.
        write(&mut bus, 0x48, &[0x01, 0xC3, 0x83]);
        sensor_mut(&mut bus, 0x48).set_input("a0", 5.0);
        assert_eq!(
            reg16(&read_pointered(&mut bus, 0x48, 0x00, 2)),
            0x7FF0,
            "{code}"
        );
    }
}

#[test]
fn mcp4725_options_sit_at_their_hard_wired_addresses() {
    for (code, lo) in [
        ("MCP4725A0T-E/CH", 0x60u8),
        ("MCP4725A1T-E/CH", 0x62),
        ("MCP4725A2T-E/CH", 0x64),
        ("MCP4725A3T-E/CH", 0x66),
    ] {
        let bus = bus_with(card_sensor(code));
        assert_eq!(bus.addresses(), vec![lo], "{code}");
    }
}

#[test]
fn mcp4725_write_commands_drive_vout_and_read_back() {
    let mut bus = bus_with(card_sensor("MCP4725A0T-E/CH"));
    let addr = 0x60;
    // Factory EEPROM default: mid-scale, normal mode; the read frame is
    // [RDY/BSY .. , D11..D4, D3..D0 xxxx, x PD PD x D11..D8, D7..D0].
    assert_eq!(
        read_stream(&mut bus, addr, 5),
        vec![0x80, 0x80, 0x00, 0x08, 0x00]
    );

    sensor_mut(&mut bus, addr).set_input("vdd", 5.0);
    // Fast write: 00 PD1 PD0 D11..D8 | D7..D0, code 1000 (3E8h).
    write(&mut bus, addr, &[0x03, 0xE8]);
    let vout = bus
        .slave::<RegisterMapSensor>(addr)
        .and_then(|s| s.output_volts("vout"))
        .expect("vout");
    let expected = 1000.0 / 4096.0 * 5.0;
    assert!((vout - expected).abs() < 1e-9, "fast write: {vout}");
    // A fast write does not touch the EEPROM image.
    assert_eq!(
        read_stream(&mut bus, addr, 5),
        vec![0x80, 0x3E, 0x80, 0x08, 0x00]
    );

    // Write DAC register: 010 xx PD1 PD0 x | D11..D4 | D3..D0 xxxx, code 1234 (4D2h).
    write(&mut bus, addr, &[0x40, 0x4D, 0x20]);
    assert_eq!(
        read_stream(&mut bus, addr, 5),
        vec![0x80, 0x4D, 0x20, 0x08, 0x00]
    );

    // Write DAC register and EEPROM with PD = 01 (1 k to ground): VOUT goes to 0 V
    // and the EEPROM image takes the code and the power-down bits.
    write(&mut bus, addr, &[0x62, 0xBB, 0x80]); // code BB8h = 3000
    let vout = bus
        .slave::<RegisterMapSensor>(addr)
        .and_then(|s| s.output_volts("vout"))
        .expect("vout");
    assert!(vout.abs() < 1e-12, "power-down output {vout}");
    assert_eq!(
        read_stream(&mut bus, addr, 5),
        vec![0x82, 0xBB, 0x80, 0x2B, 0xB8]
    );

    // Normal mode again: VOUT = code / 4096 x VDD.
    write(&mut bus, addr, &[0x40, 0xBB, 0x80]);
    let vout = bus
        .slave::<RegisterMapSensor>(addr)
        .and_then(|s| s.output_volts("vout"))
        .expect("vout");
    let expected = 3000.0 / 4096.0 * 5.0;
    assert!((vout - expected).abs() < 1e-9, "write DAC: {vout}");
}
