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

// ---- HTU21D / SHT21 (shared command set, user register and CRC-8 init 0x00) ------------------------

const HTU_PARTS: [&str; 4] = ["HTU21D", "HTU21DF", "HPP845E031R4", "SHT21"];

/// Code of a measurement at the code step of the chosen resolution (2^(16 - bits)).
fn htu_word(x: f64, step: f64) -> u16 {
    ((x / step).round() * step).clamp(0.0, 65536.0 - step) as u16
}

fn htu_t_word(t: f64, step: f64) -> u16 {
    htu_word((t + 46.85) * 65536.0 / 175.72, step)
}

fn htu_rh_word(rh: f64, step: f64) -> u16 {
    htu_word((rh + 6.0) * 65536.0 / 125.0, step)
}

/// The three bytes a read returns: MSB, LSB with the status bits, CRC-8 (init 0) over both.
fn htu_frame(word: u16, status: u8) -> Vec<u8> {
    let b = word.to_be_bytes();
    let lsb = b[1] | status;
    vec![b[0], lsb, crc8(&[b[0], lsb], 0x00)]
}

#[test]
fn htu21d_sht21_datasheet_worked_examples() {
    // HTU21D p.11: SRH = 0x7C80 -> 54.8 %RH, LSB with status 10 = 0x82, checksum 1001'0111.
    // SHT21 Fig. 15: SRH = 0x6350 -> 42.5 %RH, LSB 0x52, checksum 0110'0100.
    for (mpn, rh, expect) in [
        ("HTU21D", 54.8, [0x7C, 0x82, 0x97]),
        ("HTU21DF", 54.8, [0x7C, 0x82, 0x97]),
        ("SHT21", 42.5, [0x63, 0x52, 0x64]),
    ] {
        let mut s = sensor(mpn);
        s.set_input("humidity_pct", rh);
        let mut b = bus(s);
        write(&mut b, 0x40, &[0xE5]);
        assert_eq!(read(&mut b, 0x40, 3), expect, "{mpn} at {rh} %RH");
    }
}

#[test]
fn htu21d_sht21_measurements_follow_the_inputs_for_every_trigger() {
    for mpn in HTU_PARTS {
        for &(t, rh) in &[
            (25.0, 50.0),
            (-40.0, 0.0),
            (125.0, 100.0),
            (7.04, 44.8),
            (36.6, 63.2),
        ] {
            let mut s = sensor(mpn);
            s.set_input("temperature_c", t);
            s.set_input("humidity_pct", rh);
            let mut b = bus(s);
            for cmd in [0xE3_u8, 0xF3] {
                write(&mut b, 0x40, &[cmd]);
                assert_eq!(
                    read(&mut b, 0x40, 3),
                    htu_frame(htu_t_word(t, 4.0), 0),
                    "{mpn} T {t} via {cmd:#x}"
                );
            }
            for cmd in [0xE5_u8, 0xF5] {
                write(&mut b, 0x40, &[cmd]);
                assert_eq!(
                    read(&mut b, 0x40, 3),
                    htu_frame(htu_rh_word(rh, 16.0), 2),
                    "{mpn} RH {rh} via {cmd:#x}"
                );
            }
        }
    }
}

#[test]
fn htu21d_sht21_user_register_sets_the_resolution() {
    // Table 8: resolution bits 7 and 0: 00 RH 12 / T 14, 01 RH 8 / T 12, 10 RH 10 / T 13, 11 RH 11 / T 11.
    for mpn in HTU_PARTS {
        let mut s = sensor(mpn);
        s.set_input("temperature_c", 21.3);
        s.set_input("humidity_pct", 37.7);
        let mut b = bus(s);
        write(&mut b, 0x40, &[0xE7]);
        assert_eq!(
            read(&mut b, 0x40, 1),
            vec![0x02],
            "{mpn} user register default"
        );
        for (reg, rh_step, t_step) in [
            (0x03_u8, 256.0, 16.0),
            (0x82, 64.0, 8.0),
            (0x83, 32.0, 32.0),
            (0x02, 16.0, 4.0),
        ] {
            write(&mut b, 0x40, &[0xE6, reg]);
            write(&mut b, 0x40, &[0xE7]);
            assert_eq!(
                read(&mut b, 0x40, 1),
                vec![reg],
                "{mpn} user register readback"
            );
            write(&mut b, 0x40, &[0xE5]);
            assert_eq!(
                read(&mut b, 0x40, 3),
                htu_frame(htu_rh_word(37.7, rh_step), 2),
                "{mpn} RH at reg {reg:#x}"
            );
            write(&mut b, 0x40, &[0xE3]);
            assert_eq!(
                read(&mut b, 0x40, 3),
                htu_frame(htu_t_word(21.3, t_step), 0),
                "{mpn} T at reg {reg:#x}"
            );
        }
    }
}

#[test]
fn htu21d_sht21_soft_reset_keeps_only_the_heater_bit() {
    for mpn in HTU_PARTS {
        let mut b = bus(sensor(mpn));
        write(&mut b, 0x40, &[0xE6, 0x07]); // resolution 01, heater on, OTP reload disabled
        write(&mut b, 0x40, &[0xFE]);
        write(&mut b, 0x40, &[0xE7]);
        assert_eq!(
            read(&mut b, 0x40, 1),
            vec![0x06],
            "{mpn}: default 0x02 plus the heater bit"
        );
        write(&mut b, 0x40, &[0xE5]);
        assert_eq!(
            read(&mut b, 0x40, 3),
            htu_frame(htu_rh_word(50.0, 16.0), 2),
            "{mpn}: resolution back to default"
        );
    }
}

#[test]
fn htu21d_sht21_answer_at_0x40_only() {
    for mpn in HTU_PARTS {
        let b = bus(sensor(mpn));
        assert!(acks(&b, 0x40), "{mpn}");
        assert!(!acks(&b, 0x44), "{mpn}");
        assert!(!acks(&b, 0x38), "{mpn}");
    }
}

// ---- AHT10 / AHT20 / AHT21 (trigger 0xAC 0x33 0x00, status + 5 data bytes, CRC-8 init 0xFF on AHT20/21) ----

fn aht_words(t: f64, rh: f64) -> (u32, u32) {
    let srh = (rh / 100.0 * 1_048_576.0).round().clamp(0.0, 1_048_575.0) as u32;
    let st = ((t + 50.0) / 200.0 * 1_048_576.0)
        .round()
        .clamp(0.0, 1_048_575.0) as u32;
    (srh, st)
}

/// Status, then humidity [19:12], [11:4], humidity [3:0] + temperature [19:16], temperature [15:8], [7:0].
fn aht_body(status: u8, t: f64, rh: f64) -> Vec<u8> {
    let (srh, st) = aht_words(t, rh);
    vec![
        status,
        (srh >> 12) as u8,
        (srh >> 4) as u8,
        (((srh & 0xF) << 4) | (st >> 16)) as u8,
        (st >> 8) as u8,
        st as u8,
    ]
}

fn aht_frame(mpn: &str, status: u8, t: f64, rh: f64) -> Vec<u8> {
    let mut f = aht_body(status, t, rh);
    if mpn != "AHT10" {
        f.push(crc8(&f, 0xFF));
    }
    f
}

fn aht_len(mpn: &str) -> usize {
    if mpn == "AHT10" {
        6
    } else {
        7
    }
}

fn aht_frame_zero(status: u8) -> Vec<u8> {
    let mut f = vec![status, 0, 0, 0, 0, 0];
    f.push(crc8(&f, 0xFF));
    f
}

#[test]
fn aht_measurement_follows_the_inputs() {
    for mpn in ["AHT10", "AHT20", "AHT21"] {
        for &(t, rh) in &[
            (25.0, 50.0),
            (-40.0, 0.0),
            (85.0, 100.0),
            (-50.0, 0.0),
            (36.6, 63.2),
            (7.04, 44.8),
        ] {
            let mut s = sensor(mpn);
            s.set_input("temperature_c", t);
            s.set_input("humidity_pct", rh);
            let mut b = bus(s);
            let status = if mpn == "AHT21" { 0x18 } else { 0x08 };
            write(&mut b, 0x38, &[0xAC, 0x33, 0x00]);
            assert_eq!(
                read(&mut b, 0x38, aht_len(mpn)),
                aht_frame(mpn, status, t, rh),
                "{mpn} at {t} C / {rh} %RH"
            );
        }
    }
}

#[test]
fn aht_default_conversion_is_25c_50pct() {
    // 50 %RH -> 0x80000, 25 C -> (75 / 200) * 2^20 = 0x60000
    let mut b = bus(sensor("AHT20"));
    write(&mut b, 0x38, &[0xAC, 0x33, 0x00]);
    let f = read(&mut b, 0x38, 7);
    assert_eq!(f[..6], [0x08, 0x80, 0x00, 0x06, 0x00, 0x00]);
    assert_eq!(f[6], crc8(&f[..6], 0xFF));
}

#[test]
fn aht_crc_is_present_on_aht20_and_aht21_only() {
    for (mpn, len) in [("AHT10", 6), ("AHT20", 7), ("AHT21", 7)] {
        let mut b = bus(sensor(mpn));
        write(&mut b, 0x38, &[0xAC, 0x33, 0x00]);
        // The frame wraps: reading twice the length returns the same bytes again.
        let f = read(&mut b, 0x38, len * 2);
        assert_eq!(f[..len], f[len..], "{mpn}");
    }
}

#[test]
fn aht_status_before_a_trigger_and_after_init_and_reset() {
    // AHT20: init 0xBE 0x08 0x00, soft reset 0xBA.
    let mut b = bus(sensor("AHT20"));
    assert_eq!(read(&mut b, 0x38, 7), aht_frame_zero(0x08));
    write(&mut b, 0x38, &[0xBE, 0x08, 0x00]);
    assert_eq!(read(&mut b, 0x38, 1), vec![0x08]);
    write(&mut b, 0x38, &[0xAC, 0x33, 0x00]);
    assert_ne!(read(&mut b, 0x38, 7), aht_frame_zero(0x08));
    write(&mut b, 0x38, &[0xBA]);
    assert_eq!(read(&mut b, 0x38, 7), aht_frame_zero(0x08));

    // AHT10: init 0xE1 (parameters not printed, two accepted), soft reset 0xBA.
    let mut b = bus(sensor("AHT10"));
    write(&mut b, 0x38, &[0xE1, 0x08, 0x00]);
    write(&mut b, 0x38, &[0xAC, 0x33, 0x00]);
    assert_eq!(read(&mut b, 0x38, 6), aht_frame("AHT10", 0x08, 25.0, 50.0));
    write(&mut b, 0x38, &[0xBA]);
    assert_eq!(read(&mut b, 0x38, 6), vec![0x08, 0, 0, 0, 0, 0]);

    // AHT21: status satisfies the printed check (status & 0x18 == 0x18) from power-on.
    let mut b = bus(sensor("AHT21"));
    assert_eq!(read(&mut b, 0x38, 1)[0] & 0x18, 0x18);
}

#[test]
fn aht_answers_at_0x38_only() {
    for mpn in ["AHT10", "AHT20", "AHT21"] {
        let b = bus(sensor(mpn));
        assert!(acks(&b, 0x38), "{mpn}");
        assert!(!acks(&b, 0x40), "{mpn}");
        assert!(!acks(&b, 0x44), "{mpn}");
    }
}

// ---- Si7021 (the HTU21D command family with the Silicon Labs differences) ---------------------------

#[test]
fn si7021_measurements_follow_the_inputs_for_every_trigger() {
    for &(t, rh) in &[
        (25.0, 50.0),
        (-40.0, 0.0),
        (125.0, 100.0),
        (7.04, 44.8),
        (36.6, 63.2),
    ] {
        let mut s = sensor("Si7021-A20-GM");
        s.set_input("temperature_c", t);
        s.set_input("humidity_pct", rh);
        let mut b = bus(s);
        for cmd in [0xE3_u8, 0xF3] {
            write(&mut b, 0x40, &[cmd]);
            assert_eq!(
                read(&mut b, 0x40, 3),
                htu_frame(htu_t_word(t, 4.0), 0),
                "T {t} via {cmd:#x}"
            );
        }
        for cmd in [0xE5_u8, 0xF5] {
            write(&mut b, 0x40, &[cmd]);
            assert_eq!(
                read(&mut b, 0x40, 3),
                htu_frame(htu_rh_word(rh, 16.0), 2),
                "RH {rh} via {cmd:#x}"
            );
        }
    }
}

#[test]
fn si7021_reads_the_temperature_of_the_previous_rh_measurement() {
    let mut s = sensor("Si7021");
    s.set_input("temperature_c", 31.5);
    s.set_input("humidity_pct", 40.0);
    let mut b = bus(s);
    write(&mut b, 0x40, &[0xE0]);
    assert_eq!(read(&mut b, 0x40, 2), vec![0, 0], "no RH measurement yet");
    write(&mut b, 0x40, &[0xE5]);
    let _ = read(&mut b, 0x40, 3);
    write(&mut b, 0x40, &[0xE0]);
    assert_eq!(
        read(&mut b, 0x40, 2),
        htu_t_word(31.5, 4.0).to_be_bytes().to_vec()
    );
}

#[test]
fn si7021_user_register_resets_to_0x3a_and_sets_the_resolution() {
    let mut s = sensor("Si7021-A20-IM1R");
    s.set_input("temperature_c", 21.3);
    s.set_input("humidity_pct", 37.7);
    let mut b = bus(s);
    write(&mut b, 0x40, &[0xE7]);
    assert_eq!(read(&mut b, 0x40, 1), vec![0x3A], "reset setting 0011_1010");
    // (written, read back with the reserved bits at 1 and VDDS at 0, RH step, T step)
    for (written, back, rh_step, t_step) in [
        (0x01_u8, 0x3B_u8, 256.0, 16.0),
        (0x80, 0xBA, 64.0, 8.0),
        (0x81, 0xBB, 32.0, 32.0),
        (0xFF, 0xBF, 32.0, 32.0),
        (0x00, 0x3A, 16.0, 4.0),
    ] {
        write(&mut b, 0x40, &[0xE6, written]);
        write(&mut b, 0x40, &[0xE7]);
        assert_eq!(
            read(&mut b, 0x40, 1),
            vec![back],
            "user register after writing {written:#x}"
        );
        write(&mut b, 0x40, &[0xE5]);
        assert_eq!(
            read(&mut b, 0x40, 3),
            htu_frame(htu_rh_word(37.7, rh_step), 2),
            "RH at {written:#x}"
        );
        write(&mut b, 0x40, &[0xE3]);
        assert_eq!(
            read(&mut b, 0x40, 3),
            htu_frame(htu_t_word(21.3, t_step), 0),
            "T at {written:#x}"
        );
    }
}

#[test]
fn si7021_heater_register_firmware_revision_and_reset() {
    let mut b = bus(sensor("Si7021"));
    write(&mut b, 0x40, &[0x11]);
    assert_eq!(
        read(&mut b, 0x40, 1),
        vec![0x00],
        "heater register reset setting"
    );
    write(&mut b, 0x40, &[0x51, 0x0F]);
    write(&mut b, 0x40, &[0x11]);
    assert_eq!(read(&mut b, 0x40, 1), vec![0x0F]);
    write(&mut b, 0x40, &[0x51, 0xF5]);
    write(&mut b, 0x40, &[0x11]);
    assert_eq!(
        read(&mut b, 0x40, 1),
        vec![0x05],
        "only HEATER[3:0] is stored"
    );

    write(&mut b, 0x40, &[0x84, 0xB8]);
    assert_eq!(read(&mut b, 0x40, 1), vec![0x20], "firmware 2.0");

    write(&mut b, 0x40, &[0xE6, 0x81]);
    write(&mut b, 0x40, &[0xFE]);
    write(&mut b, 0x40, &[0xE7]);
    assert_eq!(read(&mut b, 0x40, 1), vec![0x3A]);
    write(&mut b, 0x40, &[0x11]);
    assert_eq!(read(&mut b, 0x40, 1), vec![0x00]);
    write(&mut b, 0x40, &[0xE5]);
    assert_eq!(
        read(&mut b, 0x40, 3),
        htu_frame(htu_rh_word(50.0, 16.0), 2),
        "resolution back to default"
    );
}

#[test]
fn si7021_answers_at_0x40_only() {
    let b = bus(sensor("Si7021-A20-GM"));
    assert!(acks(&b, 0x40));
    assert!(!acks(&b, 0x44));
    assert!(!acks(&b, 0x38));
}

// ---- VL53L0X (datasheet Table 5 reference registers, 7-bit address 0x29) -----------------------------

#[test]
fn vl53l0x_reference_registers_read_the_fresh_reset_values() {
    for mpn in ["VL53L0X", "VL53L0CXV0DH/1", "VL53L0CXV9DH/1"] {
        let mut b = bus(sensor(mpn));
        assert!(acks(&b, 0x29), "{mpn}");
        assert!(
            !acks(&b, 0x52),
            "{mpn}: 0x52 is the 8-bit write byte, not a 7-bit address"
        );
        for (reg, expect) in [
            (0xC0_u8, vec![0xEE_u8]),
            (0xC1, vec![0xAA]),
            (0xC2, vec![0x10]),
            (0x51, vec![0x00, 0x99]),
            (0x61, vec![0x00, 0x00]),
        ] {
            write(&mut b, 0x29, &[reg]);
            assert_eq!(
                read(&mut b, 0x29, expect.len()),
                expect,
                "{mpn} register {reg:#x}"
            );
        }
    }
}
