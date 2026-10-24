//! Firmware intent, end to end through the CLI: the image given with
//! `--firmware` says which peripheral interrupts the program handles, and a
//! handler whose fixed pad the board leaves unconnected is a Medium finding on
//! the static `--lint` and `--check` surfaces.
//!
//! The firmware is a hand-made AVR ELF whose interrupt vector table is the
//! shape avr-gcc links: every unused vector jumps to one shared default routine
//! (`__bad_interrupt`), and a handled vector jumps to its own handler. Vector 11
//! of the `ATmega328P` is `TIMER1_CAPT`, which fires only from `ICP1` (pad 14,
//! PB0). Each flawed board has a control that differs in one thing only: the
//! capture pad is wired, or the firmware does not handle the capture vector.

use std::path::{Path, PathBuf};

use crate::pin_function_reasoning::{netlist, Part, Pin};
use crate::support::{run_board, stdout};

/// Vector numbers are 1-based with reset at 1: `TIMER1_CAPT` is 11.
const TIMER1_CAPT: usize = 11;
/// `TIMER0_OVF`.
const TIMER0_OVF: usize = 17;

/// The 26-slot `jmp` vector table of an `ATmega328P`: every vector to the shared
/// default routine, except the `handled` ones, each to its own handler.
fn vector_table(handled: &[usize]) -> Vec<u8> {
    const DEFAULT: u32 = 0x0070;
    let mut out = Vec::new();
    for n in 1..=26 {
        let target = match handled.iter().position(|&h| h == n) {
            Some(i) => 0x0100 + 0x10 * u32::try_from(i + 1).unwrap(),
            None if n == 1 => 0x0080,
            None => DEFAULT,
        };
        // jmp k: 1001 010k kkkk 110k, then the low 16 bits of the word address.
        let w0 = 0x940C_u16 | u16::try_from((target >> 16) & 1).unwrap();
        out.extend_from_slice(&w0.to_le_bytes());
        out.extend_from_slice(&u16::try_from(target & 0xFFFF).unwrap().to_le_bytes());
    }
    out
}

/// A 32-bit little-endian AVR ELF with `flash` loaded at flash address 0.
fn elf_of(flash: &[u8]) -> Vec<u8> {
    let len = u32::try_from(flash.len()).unwrap();
    let mut e = vec![0x7F, b'E', b'L', b'F', 1, 1, 1, 0];
    e.resize(52, 0);
    e[0x12] = 83; // EM_AVR
    e[0x1C..0x20].copy_from_slice(&52u32.to_le_bytes());
    e[0x2A..0x2C].copy_from_slice(&32u16.to_le_bytes());
    e[0x2C..0x2E].copy_from_slice(&1u16.to_le_bytes());
    for v in [1, 84, 0, 0, len, len, 5, 2] {
        e.extend_from_slice(&u32::to_le_bytes(v));
    }
    e.extend_from_slice(flash);
    e
}

/// The board and firmware files of one scenario in a fresh directory.
struct Scenario {
    dir: tempfile::TempDir,
    board: PathBuf,
}

impl Scenario {
    fn new(parts: &[Part<'_>]) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let board = dir.path().join("fixture.net");
        std::fs::write(&board, netlist(parts)).expect("write the netlist");
        Self { dir, board }
    }

    fn firmware(&self, handled: &[usize]) -> PathBuf {
        let path = self.dir.path().join("fw.elf");
        std::fs::write(&path, elf_of(&vector_table(handled))).expect("write the ELF");
        path
    }

    fn lint(&self, firmware: Option<&Path>) -> String {
        let mut args = vec!["--lint", "--plain"];
        let fw = firmware.map(|p| p.to_str().unwrap().to_owned());
        if let Some(fw) = fw.as_deref() {
            args.extend(["--firmware", fw]);
        }
        stdout(&run_board(&self.board, &args))
    }
}

const SUPPLY: [Pin<'static>; 4] = [
    ("7", "VCC", "power_in", "VCC"),
    ("8", "GND", "power_in", "GND"),
    ("20", "AVCC", "power_in", "VCC"),
    ("22", "GND", "power_in", "GND"),
];

/// The `ATmega328P` with its supply and, optionally, pad 14 wired to a header.
fn board_parts(capture_wired: bool) -> Vec<Part<'static>> {
    static BARE: [Pin<'static>; 4] = SUPPLY;
    static WIRED: [Pin<'static>; 5] = [
        SUPPLY[0],
        SUPPLY[1],
        SUPPLY[2],
        SUPPLY[3],
        ("14", "PB0", "bidirectional", "SENSE"),
    ];
    static HEADER: [Pin<'static>; 2] =
        [("1", "1", "passive", "SENSE"), ("2", "2", "passive", "GND")];
    let mcu: &'static [Pin<'static>] = if capture_wired { &WIRED } else { &BARE };
    let mut parts = vec![("U1", "ATmega328P", "ATmega328P", mcu)];
    if capture_wired {
        parts.push(("J1", "Conn_01x02", "Conn_01x02", &HEADER));
    }
    parts
}

#[test]
fn a_capture_handler_whose_icp1_pad_is_unconnected_is_a_medium_finding() {
    let s = Scenario::new(&board_parts(false));
    let fw = s.firmware(&[TIMER1_CAPT, TIMER0_OVF]);
    let out = s.lint(Some(&fw));
    assert!(out.contains("TIMER1_CAPT"), "{out}");
    assert!(out.contains("ICP1") && out.contains("pad 14"), "{out}");
    // Medium severity prints as a warning in the plain report.
    assert!(
        out.contains("[WARNING] The firmware for U1 handles an interrupt"),
        "{out}"
    );
    assert!(!out.contains("[SERIOUS]"), "{out}");
}

#[test]
fn the_same_firmware_with_icp1_wired_to_a_signal_is_silent() {
    let s = Scenario::new(&board_parts(true));
    let fw = s.firmware(&[TIMER1_CAPT, TIMER0_OVF]);
    let out = s.lint(Some(&fw));
    assert!(!out.contains("TIMER1_CAPT"), "{out}");
}

#[test]
fn firmware_that_does_not_handle_the_capture_vector_is_silent_on_the_same_board() {
    let s = Scenario::new(&board_parts(false));
    let fw = s.firmware(&[TIMER0_OVF]);
    let out = s.lint(Some(&fw));
    assert!(!out.contains("TIMER1_CAPT"), "{out}");
}

#[test]
fn without_firmware_the_board_alone_says_nothing_about_the_capture_pad() {
    let s = Scenario::new(&board_parts(false));
    let out = s.lint(None);
    assert!(!out.contains("TIMER1_CAPT"), "{out}");
}

#[test]
fn the_check_surface_reports_it_too() {
    let s = Scenario::new(&board_parts(false));
    let fw = s.firmware(&[TIMER1_CAPT]);
    let fw = fw.to_str().unwrap();
    let out = stdout(&run_board(
        &s.board,
        &["--check", "--plain", "--firmware", fw],
    ));
    assert!(out.contains("TIMER1_CAPT"), "{out}");
}
