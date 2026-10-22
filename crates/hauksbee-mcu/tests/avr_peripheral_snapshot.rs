//! The AVR peripheral snapshot and the clock-prescaler follow.
//!
//! simavr does not implement `CLKPR`, so without `AvrMcu::follow_clock` a
//! firmware that divides the system clock would keep running at the crystal
//! rate. The image here is hand-assembled (no toolchain dependency): it sets
//! CLKPS to /8, a double-speed USART0 at UBRR0 = 103, an SPI master at /8 and a
//! TWI bit rate, then spins. Register addresses and formulas are from Microchip
//! DS40002061A (ATmega48A/PA/88A/PA/168A/PA/328/P); the working is below.

#![cfg(feature = "avr")]

use hauksbee_mcu::periph_snapshot::Decoded;
use hauksbee_mcu::{AvrMcu, Mcu};

/// `ldi r16, k` : 1110 KKKK dddd KKKK with d = r16 (dddd = 0).
fn ldi(k: u8) -> u16 {
    0xE000 | (u16::from(k & 0xF0) << 4) | u16::from(k & 0x0F)
}

/// Words of `sts addr, r16`: 0x9300 then the 16-bit data address.
fn sts(addr: u16) -> [u16; 2] {
    [0x9300, addr]
}

fn write(program: &mut Vec<u16>, addr: u16, value: u8) {
    program.push(ldi(value));
    program.extend(sts(addr));
}

/// One Intel HEX data record at byte address `at`.
fn record(at: u16, bytes: &[u8]) -> String {
    let mut line = vec![u8::try_from(bytes.len()).expect("short record")];
    line.extend(at.to_be_bytes());
    line.push(0);
    line.extend(bytes);
    let sum = line.iter().fold(0u8, |a, b| a.wrapping_add(*b));
    line.push(sum.wrapping_neg());
    format!(
        ":{}\n",
        line.iter().map(|b| format!("{b:02X}")).collect::<String>()
    )
}

fn hex(program: &[u16]) -> String {
    let bytes: Vec<u8> = program.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut out = String::new();
    for (i, chunk) in bytes.chunks(16).enumerate() {
        out.push_str(&record(u16::try_from(i * 16).expect("small image"), chunk));
    }
    out.push_str(":00000001FF\n");
    out
}

fn configured_image() -> Vec<u16> {
    let mut p = Vec::new();
    // CLKPR: CLKPCE then CLKPS3:0 = 0011 (/8), Table 9-17.
    write(&mut p, 0x61, 0x80);
    write(&mut p, 0x61, 0x03);
    // USART0: U2X0 (UCSR0A bit 1), UCSZ01:00 = 11 (8 bits, UCSR0C bits 2:1),
    // UBRR0L = 103.
    write(&mut p, 0xC0, 0x02);
    write(&mut p, 0xC2, 0x06);
    write(&mut p, 0xC4, 103);
    // SPI master: SPE | MSTR | CPOL | SPR0 = 0x59, SPI2X = 1: /8.
    write(&mut p, 0x4C, 0x59);
    write(&mut p, 0x4D, 0x01);
    // TWI: TWBR = 72, TWPS = 01 (prescaler 4).
    write(&mut p, 0xB8, 72);
    write(&mut p, 0xB9, 0x01);
    p.push(0xCFFF); // rjmp .-2
    p
}

fn load(mcu: &mut AvrMcu, program: &[u16]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.hex");
    std::fs::write(&path, hex(program)).expect("write hex");
    mcu.load_firmware(&path).expect("load firmware");
    dir
}

#[track_caller]
fn close(got: &Decoded<f64>, want: f64) {
    let v = got
        .known()
        .unwrap_or_else(|| panic!("expected {want}, got {got}"));
    assert!(
        (v - want).abs() <= want.abs() * 1e-9,
        "expected {want}, got {v}"
    );
}

/// Reset state: no CLKPR write, so the part runs at the oscillator; the zeroed
/// USART divider is a valid (UBRR + 1 = 1) setting and UCSR0C resets to 0x06
/// (UCSZ01:00 = 11, 8 data bits; DS40002061A USART0 register description).
#[test]
fn reset_state_is_the_oscillator_and_register_defaults() {
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    let snap = mcu.peripheral_config().expect("328P decodes");
    close(&snap.clock().expect("clock").core_hz, 16e6);
    let u = snap.uarts().into_iter().next().expect("usart0");
    close(&u.baud, 16e6 / 16.0);
    close(&u.data_bits, 8.0);
    close(&u.stop_bits, 1.0);
}

#[test]
fn firmware_configuration_is_decoded_from_the_data_space() {
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    let _keep = load(&mut mcu, &configured_image());
    mcu.run_micros(1_000).expect("run");
    // The data-space accessor sees what the firmware stored.
    assert_eq!(mcu.read_data(0x61), Some(0x03));
    assert_eq!(mcu.read_data(0xC4), Some(103));
    assert_eq!(mcu.read_data(0xFFFF), None);

    let snap = mcu.peripheral_config().expect("328P decodes");
    // 16 MHz / 8.
    close(&snap.clock().expect("clock").core_hz, 2e6);
    // U2X0: 2e6 / (8 x (103 + 1)).
    let u = snap.uarts().into_iter().next().expect("usart0");
    close(&u.baud, 2e6 / 832.0);
    close(&u.data_bits, 8.0);
    close(&u.stop_bits, 1.0);
    // SPI2X with SPR = 01: fosc/8; CPOL set, CPHA clear.
    let s = snap.spis().into_iter().next().expect("spi");
    close(&s.sck_hz, 250e3);
    close(&s.cpol, 1.0);
    close(&s.cpha, 0.0);
    // 2e6 / (16 + 2 x 72 x 4).
    let t = snap.i2cs().into_iter().next().expect("twi");
    close(&t.scl_hz, 2e6 / 592.0);
}

/// The simulated clock follows CLKPR: after the firmware divides by 8, a
/// millisecond of simulated time is 2000 cycles, not 16000.
#[test]
fn simulated_time_follows_the_clock_prescaler() {
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    let _keep = load(&mut mcu, &configured_image());
    mcu.run_micros(1_000).expect("run configures the clock");
    assert_eq!(mcu.frequency(), 2_000_000);
    let before = mcu.current_cycle();
    mcu.run_micros(1_000).expect("run a millisecond");
    let ran = mcu.current_cycle() - before;
    assert!(
        (2_000..2_000 + 8).contains(&ran),
        "1 ms at 2 MHz is 2000 cycles (plus instruction-boundary overshoot), ran {ran}"
    );
}

/// A reserved CLKPS code is not a clock: the decode abstains and the last good
/// frequency stays.
#[test]
fn reserved_clkps_is_unknown_and_keeps_the_last_clock() {
    let mut p = Vec::new();
    write(&mut p, 0x61, 0x80);
    write(&mut p, 0x61, 0x09); // CLKPS = 1001: reserved (Table 9-17)
    p.push(0xCFFF);
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    let _keep = load(&mut mcu, &p);
    mcu.run_micros(1_000).expect("run");
    let snap = mcu.peripheral_config().expect("328P decodes");
    assert!(snap.clock().expect("clock").core_hz.is_unknown());
    assert_eq!(mcu.frequency(), 16_000_000);
}

/// A firmware that never touches CLKPR.
fn idle_image() -> Vec<u16> {
    vec![0xCFFF] // rjmp .-2
}

/// DS40002061A p.46: "The device is shipped with the CKDIV8 Fuse programmed", so
/// CLKPS resets to 0011 and a bare chip runs at fosc / 8. With the fuse stated
/// as programmed the core starts at 2 MHz from a 16 MHz oscillator, the
/// snapshot agrees, and no assumption is left to report.
#[test]
fn programmed_ckdiv8_starts_the_core_at_one_eighth() {
    let mut mcu = AvrMcu::atmega328p_16mhz()
        .expect("create MCU")
        .with_ckdiv8_programmed(true);
    assert_eq!(
        mcu.frequency(),
        2_000_000,
        "follows CLKPR before any firmware"
    );
    let _keep = load(&mut mcu, &idle_image());
    assert_eq!(mcu.read_data(0x61), Some(0x03));
    mcu.run_micros(1_000).expect("run");
    assert_eq!(mcu.frequency(), 2_000_000);
    let snap = mcu.peripheral_config().expect("328P decodes");
    close(&snap.clock().expect("clock").core_hz, 2e6);
    assert!(mcu.clock_assumption().is_none());
    // 1 ms at 2 MHz is 2000 cycles (plus instruction-boundary overshoot).
    let before = mcu.current_cycle();
    mcu.run_micros(1_000).expect("run a millisecond");
    let ran = mcu.current_cycle() - before;
    assert!((2_000..2_000 + 8).contains(&ran), "ran {ran} cycles");
}

/// The firmware's own CLKPR write (Table 9-17: CLKPS = 0000 is /1) overrides
/// the fuse's reset value, and a machine reset brings the fuse value back.
#[test]
fn clkpr_write_overrides_the_fuse_and_reset_restores_it() {
    let mut p = Vec::new();
    write(&mut p, 0x61, 0x80);
    write(&mut p, 0x61, 0x00);
    p.push(0xCFFF);
    let mut mcu = AvrMcu::atmega328p_16mhz()
        .expect("create MCU")
        .with_ckdiv8_programmed(true);
    let _keep = load(&mut mcu, &p);
    mcu.run_micros(1_000).expect("run");
    assert_eq!(mcu.frequency(), 16_000_000);
    mcu.reset().expect("reset");
    assert_eq!(
        mcu.frequency(),
        2_000_000,
        "CLKPS is 0011 again after reset"
    );
    let snap = mcu.peripheral_config().expect("328P decodes");
    close(&snap.clock().expect("clock").core_hz, 2e6);
}

/// Unstated, the fuse is an ASSUMPTION (unprogrammed, the Arduino state) and the
/// backend says so; stated as unprogrammed it is a fact and nothing is reported.
/// Parts with no clock decode assume nothing.
#[test]
fn the_ckdiv8_assumption_is_reported_until_stated() {
    let mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    let note = mcu.clock_assumption().expect("assumption reported");
    assert!(
        note.contains("CKDIV8") && note.contains("16000000"),
        "{note}"
    );
    assert_eq!(mcu.frequency(), 16_000_000);

    let stated = AvrMcu::atmega328p_16mhz()
        .expect("create MCU")
        .with_ckdiv8_programmed(false);
    assert!(stated.clock_assumption().is_none());
    assert_eq!(stated.frequency(), 16_000_000);

    let other = AvrMcu::new("atmega2560", 16_000_000).expect("create MCU");
    assert!(other.clock_assumption().is_none());
}

/// Parts without a verified register map decode nothing rather than guess.
#[test]
fn other_avr_parts_decode_nothing() {
    let mut mcu = AvrMcu::new("atmega2560", 16_000_000).expect("create MCU");
    assert!(mcu.peripheral_config().is_none());
}
