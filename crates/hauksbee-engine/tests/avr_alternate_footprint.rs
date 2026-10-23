//! A board that carries an AVR footprint plus an alternate footprint for a
//! smaller-flash variant of the same family (`ATmega328P` with an `ATmega168`
//! alternative on the same nets), co-simulated with firmware sized for the
//! bigger part.
//!
//! The co-sim loads one firmware image for the board. Two invariants keep that
//! from going wrong:
//! - alternate footprints of one MCU (every connected GPIO on the same net as
//!   the same GPIO of another MCU) are one processor, not two cores running the
//!   same firmware and fighting on every shared pin;
//! - an image bigger than the part's flash is refused BEFORE any loader sees
//!   it, with the sizes named, and never reaches the emulator (an overflowing
//!   load used to take the whole process down).

// The board binds `simavr:` parts, whose in-process core needs the `avr` feature.
#![cfg(feature = "avr")]

use std::fmt::Write as _;

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_models::ModelLibrary;
use hauksbee_testkit::avr_firmware::{avr_elf, intel_hex, port_toggle_image};

/// One 32-pin AVR footprint: power, reset, and five GPIO (PB0, PB1, PB5, PD2,
/// PD5) on the board's named nets. Pads follow the TQFP-32 map both the
/// `ATmega328P` and `ATmega168` cards carry.
fn avr_footprint(reference: &str, value: &str, x: u32) -> String {
    let pads = [
        (3, 1, "GND"),
        (5, 1, "GND"),
        (21, 1, "GND"),
        (4, 2, "+5V"),
        (6, 2, "+5V"),
        (18, 2, "+5V"),
        (29, 3, "RESET"),
        (12, 4, "D8"),
        (13, 5, "D9"),
        (17, 6, "D13"),
        (32, 7, "D2"),
        (9, 8, "D5"),
    ];
    let mut s = format!(
        "  (module Package_QFP:TQFP-32_7x7mm_P0.8mm (layer F.Cu)\n    (at {x} 100)\n    \
         (fp_text reference {reference} (at 0 0) (layer F.SilkS))\n    \
         (fp_text value {value} (at 0 2) (layer F.Fab))\n"
    );
    for (pad, net, name) in pads {
        writeln!(
            s,
            "    (pad {pad} smd rect (at 0 {pad}) (size 1 1) (net {net} \"{name}\"))"
        )
        .unwrap();
    }
    s.push_str("  )\n");
    s
}

fn resistor(reference: &str, value: &str, a: (u32, &str), b: (u32, &str), x: u32) -> String {
    format!(
        "  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)\n    (at {x} 120)\n    \
         (fp_text reference {reference} (at 0 0) (layer F.SilkS))\n    \
         (fp_text value {value} (at 0 2) (layer F.Fab))\n    \
         (pad 1 smd rect (at 0 0) (size 1 1) (net {} \"{}\"))\n    \
         (pad 2 smd rect (at 1 0) (size 1 1) (net {} \"{}\"))\n  )\n",
        a.0, a.1, b.0, b.1
    )
}

/// A board with the given AVR footprints sharing every net, a reset pull-up and
/// a 10k load on each GPIO net.
fn board(mcus: &[(&str, &str)]) -> ExtractedBoard {
    let mut s = String::from("(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n");
    for (i, name) in ["", "GND", "+5V", "RESET", "D8", "D9", "D13", "D2", "D5"]
        .iter()
        .enumerate()
    {
        writeln!(s, "  (net {i} \"{name}\")").unwrap();
    }
    for (i, (reference, value)) in mcus.iter().enumerate() {
        s.push_str(&avr_footprint(
            reference,
            value,
            100 + 20 * u32::try_from(i).unwrap(),
        ));
    }
    s.push_str(&resistor("R1", "10k", (3, "RESET"), (2, "+5V"), 150));
    for (i, (net, name)) in [(4, "D8"), (5, "D9"), (6, "D13"), (7, "D2"), (8, "D5")]
        .iter()
        .enumerate()
    {
        let r = format!("R{}", i + 2);
        s.push_str(&resistor(
            &r,
            "10k",
            (*net, name),
            (1, "GND"),
            160 + 5 * u32::try_from(i).unwrap(),
        ));
    }
    s.push_str(")\n");
    ExtractedBoard::from_auto(&s).expect("parse board")
}

/// 20 KiB: bigger than an `ATmega168`'s 16 KiB flash, inside an `ATmega328P`'s 32 KiB.
const IMAGE_BYTES: usize = 20 * 1024;

fn write_firmware(dir: &tempfile::TempDir, elf: bool) -> std::path::PathBuf {
    let image = port_toggle_image(50, IMAGE_BYTES);
    if elf {
        let path = dir.path().join("big.elf");
        std::fs::write(&path, avr_elf(&image)).unwrap();
        path
    } else {
        let path = dir.path().join("big.hex");
        std::fs::write(&path, intel_hex(&image)).unwrap();
        path
    }
}

fn engine(mcus: &[(&str, &str)], fw: &std::path::Path) -> anyhow::Result<HauksbeeEngine> {
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board(mcus), &lib);
    let backends: Vec<String> = bound.mcus.iter().map(|m| m.backend.clone()).collect();
    assert_eq!(
        backends.len(),
        mcus.len(),
        "every footprint binds as an MCU: {backends:?}"
    );
    HauksbeeEngine::from_bound(bound, Some(fw), "/ci")
}

const ALTERNATES: [(&str, &str); 2] = [("U1", "ATmega328P"), ("U2", "ATmega168-20AU")];

#[test]
fn an_alternate_smaller_footprint_gets_no_processor_and_the_fitted_part_runs() {
    for elf in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let fw = write_firmware(&dir, elf);
        let mut e = engine(&ALTERNATES, &fw).unwrap_or_else(|err| {
            panic!("the 328P fits the image, so the co-sim must build ({fw:?}): {err:#}")
        });
        assert_eq!(
            e.scheduler().mcu_refs(),
            vec!["U1".to_string()],
            "one processor for the two footprints of one MCU, the one the image fits"
        );
        let notes = e.scheduler().alternate_footprint_notes().join("\n");
        assert!(
            notes.contains("U2") && notes.contains("U1") && notes.contains("alternate"),
            "the run says which footprint was left without a processor and why: {notes}"
        );
        let mut toggled = false;
        for _ in 0..5 {
            let frame = e.step(1e-3);
            let v = frame.net_voltages.get("D13").copied().unwrap_or(0.0);
            toggled |= v > 2.5;
        }
        assert!(toggled, "U1 runs the firmware and drives D13 ({fw:?})");
    }
}

#[test]
fn an_image_bigger_than_the_only_parts_flash_is_refused_naming_the_sizes() {
    for elf in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let fw = write_firmware(&dir, elf);
        let Err(err) = engine(&[("U2", "ATmega168-20AU")], &fw) else {
            panic!("a 20 KiB image cannot run on a 16 KiB part ({fw:?})");
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("U2") && msg.contains("20480") && msg.contains("16384"),
            "the refusal names the part and both sizes: {msg}"
        );
    }
}

#[test]
fn two_footprints_the_image_fits_still_run_one_core() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("small.hex");
    std::fs::write(&path, intel_hex(&port_toggle_image(50, 4096))).unwrap();
    let e = engine(&ALTERNATES, &path).expect("both parts fit a 4 KiB image");
    assert_eq!(e.scheduler().mcu_refs(), vec!["U1".to_string()]);
}
