//! The `ATtiny85` and `ATmega32u4` cards bind to `simavr:attiny85` /
//! `simavr:atmega32u4`; these check that simavr runs a blink on each part and
//! that the PB0 pin the cards map is the one the core actually drives.
//!
//! avr-gcc is not a build dependency, so the blink is hand-assembled:
//! `ldi r16,1; out DDRB,r16; ldi r17,0; loop: out PORTB,r16; out PORTB,r17;
//! rjmp loop`. DDRB/PORTB sit at I/O 0x17/0x18 on the tiny85 and 0x04/0x05 on
//! the 32u4 (datasheet register summaries), so each image only blinks on its
//! own part.

#![cfg(feature = "avr")]

use crate::support::{ihex_record, write_hex};
use hauksbee_mcu::{AvrMcu, Mcu, PinId};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const TINY85_BLINK: [u8; 12] = [
    0x01, 0xE0, 0x07, 0xBB, 0x10, 0xE0, 0x08, 0xBB, 0x18, 0xBB, 0xFD, 0xCF,
];
const M32U4_BLINK: [u8; 12] = [
    0x01, 0xE0, 0x04, 0xB9, 0x10, 0xE0, 0x05, 0xB9, 0x15, 0xB9, 0xFD, 0xCF,
];

fn blink_hex(name: &str, code: &[u8]) -> PathBuf {
    write_hex("avr-new-parts", name, &[ihex_record(0x00, 0x0000, code)])
}

/// PB0 rising edges seen while `hex` runs for 1 ms on `part`.
fn pb0_rises(part: &str, hex: &Path) -> u32 {
    let mut mcu = AvrMcu::new(part, 16_000_000).expect("simavr knows the part");
    mcu.load_firmware(hex).expect("blink hex loads");
    let rises = Arc::new(Mutex::new(0u32));
    let counter = rises.clone();
    let pb0 = PinId { port: 'B', bit: 0 };
    mcu.on_pin_change(Box::new(move |pin, high, _| {
        if pin == pb0 && high {
            *counter.lock().unwrap() += 1;
        }
    }));
    mcu.run_millis(1).expect("runs");
    let n = *rises.lock().unwrap();
    n
}

#[test]
fn attiny85_blinks_pb0() {
    let hex = blink_hex("tiny85.hex", &TINY85_BLINK);
    assert!(pb0_rises("attiny85", &hex) > 100);
}

#[test]
fn atmega32u4_blinks_pb0() {
    let hex = blink_hex("m32u4.hex", &M32U4_BLINK);
    assert!(pb0_rises("atmega32u4", &hex) > 100);
}

/// Failing input: the 32u4 image on a tiny85 writes I/O 0x04/0x05 (ADC data
/// there), so PB0 must stay still. A part mix-up cannot pass as a blink.
#[test]
fn a_blink_built_for_the_other_part_does_not_drive_pb0() {
    let hex = blink_hex("m32u4-on-tiny.hex", &M32U4_BLINK);
    assert_eq!(pb0_rises("attiny85", &hex), 0);
}

/// Failing input: an image past the tiny85's 8 KiB flash is refused.
#[test]
fn attiny85_refuses_an_image_beyond_8k() {
    let hex = write_hex(
        "avr-new-parts",
        "tiny85-too-big.hex",
        &[ihex_record(0x00, 0x2000, &[0xFF, 0xCF])],
    );
    let mut mcu = AvrMcu::new("attiny85", 16_000_000).unwrap();
    let err = mcu.load_firmware(&hex).unwrap_err().to_string();
    assert!(err.contains("does not fit this MCU's flash"), "{err}");
}
