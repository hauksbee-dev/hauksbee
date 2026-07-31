//! Regression: the hardware-TWI (I2C) bridge must ACK only modeled slave
//! addresses.
//!
//! A bridge that discards the `on_i2c` callback's return value and raises
//! `TWI_COND_ACK` unconditionally lets a firmware bus scanner "find" a device
//! at every address, and leaves firmware NACK-handling paths untestable. The fix
//! gates the address ACK on the known-address set installed through
//! [`Mcu::set_i2c_slave_addresses`] (the engine populates it from the attached
//! bus), NACKs unknown addresses, and never fabricates ACKed read data for
//! them, mirroring the `SoftI2cResponder`'s honest no-answer.
//!
//! The probe firmware (testdata/firmware/twi_scan) is the classic i2c_scanner:
//! START + SLA+W for every address in 0x08..=0x77, reporting each address whose
//! TWSR status came back TW_MT_SLA_ACK as one raw UART byte, then a 0xFE
//! terminator.

// Drives the in-process simavr core: GPL-gated `avr` feature only.
#![cfg(feature = "avr")]

use hauksbee_mcu::{AvrMcu, I2cEvent, Mcu};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn scan_firmware() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/firmware/twi_scan/scan.hex")
}

/// Run the scanner and return the addresses it found (the UART bytes before
/// the 0xFE terminator). Panics if the sweep never completes.
fn run_scan(mcu: &mut AvrMcu, uart: &Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    for _ in 0..50 {
        mcu.run_millis(10).expect("run_millis");
        let buf = uart.lock().unwrap();
        if let Some(end) = buf.iter().position(|&b| b == 0xFE) {
            return buf[..end].to_vec();
        }
    }
    panic!(
        "scan never completed (no 0xFE terminator); UART so far: {:02X?}",
        uart.lock().unwrap()
    );
}

fn scanner_mcu() -> (AvrMcu, Arc<Mutex<Vec<u8>>>) {
    let fw = scan_firmware();
    assert!(
        fw.exists(),
        "build the fixture first: make -C testdata/firmware/twi_scan ({fw:?})"
    );
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(&fw).expect("load scan.hex");
    let uart: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = uart.clone();
    mcu.on_uart(Box::new(move |b| sink.lock().unwrap().push(b)));
    (mcu, uart)
}

/// With a known-address set installed, the scanner finds EXACTLY the modeled
/// slaves, every other address is NACKed.
#[test]
fn twi_scanner_finds_only_modeled_addresses() {
    let (mut mcu, uart) = scanner_mcu();

    // Model two slaves, an LM75 at 0x48 and an EEPROM at 0x50. The handler
    // is a plain event log; the ACK decision must come from the address set,
    // not from the handler's return value shape.
    let events: Arc<Mutex<Vec<I2cEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let ev_sink = events.clone();
    mcu.set_i2c_slave_addresses(&[0x48, 0x50]);
    mcu.on_i2c(Box::new(move |ev| {
        ev_sink.lock().unwrap().push(ev);
        None
    }));

    let found = run_scan(&mut mcu, &uart);
    assert_eq!(
        found,
        vec![0x48, 0x50],
        "the scan must find exactly the modeled slaves, got {found:02X?}"
    );
}

/// Without `set_i2c_slave_addresses` (a standalone `on_i2c` user modelling the
/// whole bus itself), the legacy ACK-everything behaviour is preserved.
#[test]
fn twi_scanner_acks_everything_without_an_address_set() {
    let (mut mcu, uart) = scanner_mcu();
    mcu.on_i2c(Box::new(|_| None));

    let found = run_scan(&mut mcu, &uart);
    let expected: Vec<u8> = (0x08..=0x77).collect();
    assert_eq!(
        found, expected,
        "with no address set installed every address still ACKs (legacy)"
    );
}
