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

use hauksbee_testkit::avr_firmware::{atmega328p_vector_table, avr_elf};

use crate::pin_function_reasoning::{netlist, Part, Pin};
use crate::support::{run_board, stdout};

/// Vector numbers are 1-based with reset at 1: `TIMER1_CAPT` is 11.
const TIMER1_CAPT: usize = 11;
/// `TIMER0_OVF`.
const TIMER0_OVF: usize = 17;

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
        std::fs::write(&path, avr_elf(&atmega328p_vector_table(handled))).expect("write the ELF");
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
