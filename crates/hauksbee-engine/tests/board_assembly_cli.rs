//! A multi-board design, end to end through the CLI: an assembly manifest names
//! the boards that mate through a board-to-board connector, and the static
//! checks run on the one netlist the product is.
//!
//! A carrier holds a potentiometer whose wiper goes to pin 3 of a 10-pin header;
//! the breakout holds the MCU, and its header pin 3 goes to an MCU pad. Neither
//! file alone has an analogue source meeting that pad. Assembled, the wiper
//! meets the pad, and when the pad is not one the card lists as an ADC input
//! (`ATmega328P` pad 4 is PD2) the reading can never be converted. The control
//! lands the same wiper on pad 23 (PC0, ADC0).

use std::path::PathBuf;

use crate::pin_function_reasoning::{netlist, Part, Pin};
use crate::support::{run_board, stdout};

/// The carrier's header: GND, +3V3 and the wiper on pins 1 to 3.
const CARRIER_HEADER: [Pin<'static>; 3] = [
    ("1", "1", "passive", "GND"),
    ("2", "2", "passive", "+3V3"),
    ("3", "3", "passive", "WIPER"),
];
const POT: [Pin<'static>; 3] = [
    ("1", "A", "passive", "+3V3"),
    ("2", "W", "passive", "WIPER"),
    ("3", "B", "passive", "GND"),
];
const BREAKOUT_HEADER: [Pin<'static>; 3] = [
    ("1", "1", "passive", "GND"),
    ("2", "2", "passive", "VCC"),
    ("3", "3", "passive", "A_IN"),
];

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    /// The two boards and the manifest, with the MCU's analogue input on `pad`.
    fn new(pad: &'static str, manifest_extra: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let carrier: Vec<Part<'_>> = vec![
            ("J1", "Conn_02x05", "Conn_02x05", &CARRIER_HEADER),
            ("RV1", "10k", "R_Potentiometer", &POT),
        ];
        let mcu = [
            ("7", "VCC", "power_in", "VCC"),
            ("8", "GND", "power_in", "GND"),
            ("20", "AVCC", "power_in", "VCC"),
            ("22", "GND", "power_in", "GND"),
            (pad, "IO", "bidirectional", "A_IN"),
        ];
        let breakout: Vec<Part<'_>> = vec![
            ("J1", "Conn_02x05", "Conn_02x05", &BREAKOUT_HEADER),
            ("U1", "ATmega328P", "ATmega328P", &mcu),
        ];
        std::fs::write(dir.path().join("carrier.net"), netlist(&carrier)).unwrap();
        std::fs::write(dir.path().join("breakout.net"), netlist(&breakout)).unwrap();
        std::fs::write(
            dir.path().join("product.toml"),
            format!("[assembly]\nboards = [\"carrier.net\", \"breakout.net\"]\n{manifest_extra}"),
        )
        .unwrap();
        Self { dir }
    }

    fn path(&self, file: &str) -> PathBuf {
        self.dir.path().join(file)
    }

    fn lint(&self, file: &str) -> String {
        let out = run_board(&self.path(file), &["--lint", "--plain"]);
        format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr))
    }
}

#[test]
fn each_board_alone_has_nothing_to_flag() {
    let f = Fixture::new("4", "");
    for file in ["carrier.net", "breakout.net"] {
        let out = f.lint(file);
        assert!(!out.contains("lists no ADC"), "{file}: {out}");
    }
}

#[test]
fn a_wiper_reaching_a_non_adc_pad_through_the_header_pair_is_flagged_once_assembled() {
    let f = Fixture::new("4", "");
    let out = f.lint("product.toml");
    assert!(out.contains("lists no ADC"), "{out}");
    assert!(out.contains("U1 pad 4") && out.contains("WIPER"), "{out}");
}

#[test]
fn the_same_assembly_with_the_wiper_on_an_adc_pad_is_silent() {
    let f = Fixture::new("23", "");
    let out = f.lint("product.toml");
    assert!(!out.contains("lists no ADC"), "{out}");
}

#[test]
fn a_manifest_may_name_the_mate_explicitly() {
    let f = Fixture::new(
        "4",
        "[[assembly.mate]]\na = \"carrier:J1\"\nb = \"breakout:J1\"\n",
    );
    let out = f.lint("product.toml");
    assert!(out.contains("lists no ADC"), "{out}");
}

#[test]
fn a_manifest_naming_one_board_is_refused_with_a_reason() {
    let f = Fixture::new("4", "");
    std::fs::write(
        f.path("one.toml"),
        "[assembly]\nboards = [\"carrier.net\"]\n",
    )
    .unwrap();
    let out = f.lint("one.toml");
    assert!(out.contains("fewer than two boards"), "{out}");
}
