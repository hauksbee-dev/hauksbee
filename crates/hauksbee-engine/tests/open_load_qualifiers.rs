//! A rating claim whose current path runs into a part with no working model
//! is unverified, not met.
//!
//! The DC stress pass reports only parts it finds over a rating, so a driver
//! pin feeding a load the binder left open read as inside its rating: the
//! solver carried no current there at all. Each such claim is now an
//! abstention naming the open part; a fully modelled path stays silent.

use std::fmt::Write as _;

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, Severity};
use hauksbee_models::ModelLibrary;

/// Footprint, reference, value, and `(pad, net)` pairs.
type Part<'a> = (&'a str, &'a str, &'a str, &'a [(&'a str, &'a str)]);

/// U1, a 74HCT1G125 with OE# grounded and A high, so Y drives net "Y" high;
/// plus `parts`.
fn board(parts: &[Part<'_>]) -> String {
    let buffer: Part<'_> = (
        "Package_TO_SOT_SMD:SOT-353_SC-70-5",
        "U1",
        "74HCT1G125GW",
        &[
            ("1", "GND"),
            ("2", "+5V"),
            ("3", "GND"),
            ("4", "Y"),
            ("5", "+5V"),
        ],
    );
    let all: Vec<&Part<'_>> = std::iter::once(&buffer).chain(parts).collect();
    let mut nets: Vec<&str> = Vec::new();
    for (.., pads) in &all {
        for (_, net) in *pads {
            if !nets.contains(net) {
                nets.push(net);
            }
        }
    }
    let id = |net: &str| nets.iter().position(|n| *n == net).unwrap() + 1;
    let mut pcb =
        String::from("(kicad_pcb (version 20171130) (host pcbnew 5.1.0)\n  (net 0 \"\")\n");
    for net in &nets {
        writeln!(pcb, "  (net {} \"{net}\")", id(net)).unwrap();
    }
    for (x, (footprint, reference, value, pads)) in all.iter().enumerate() {
        write!(
            pcb,
            "  (module {footprint} (layer F.Cu)\n    (at {} 100)\n    (fp_text reference {reference} (at 0 0) (layer F.SilkS))\n    (fp_text value {value} (at 0 2) (layer F.Fab))\n",
            100 + 10 * x
        )
        .unwrap();
        for (i, (pad, net)) in pads.iter().enumerate() {
            writeln!(
                pcb,
                "    (pad {pad} smd rect (at {i} 0) (net {} \"{net}\"))",
                id(net)
            )
            .unwrap();
        }
        pcb.push_str("  )\n");
    }
    pcb.push_str(")\n");
    pcb
}

fn findings(parts: &[Part<'_>]) -> Vec<LintFinding> {
    let board = ExtractedBoard::from_auto(&board(parts)).expect("parse board");
    let lint = hauksbee_engine::checks::engine_lint(&board, &ModelLibrary::builtin());
    lint.of_check(LintCheck::DcOverstress).cloned().collect()
}

/// The one abstention on `part`, naming `open`.
fn qualified(found: &[LintFinding], part: &str, open: &str) {
    let f = found
        .iter()
        .find(|f| f.refs == [part])
        .unwrap_or_else(|| panic!("{part}'s rating is qualified: {found:#?}"));
    assert_eq!(f.severity, Severity::Low, "{}", f.message);
    assert!(
        f.is_abstention(),
        "an abstention, not a finding: {}",
        f.message
    );
    assert!(f.message.contains(open), "names {open}: {}", f.message);
    assert!(f.message.contains("unverified"), "{}", f.message);
}

const R0603: &str = "Resistor_SMD:R_0603_1608Metric";
const R2512: &str = "Resistor_SMD:R_2512_6332Metric";
const LED: &str = "LED_SMD:LED_0603_1608Metric";
const SOIC8: &str = "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm";

#[test]
fn a_pin_into_an_unmodelled_load_through_a_series_resistor_is_unverified() {
    let found = findings(&[
        (R0603, "R1", "1k", &[("1", "Y"), ("2", "LOAD")]),
        (
            SOIC8,
            "U2",
            "XQZ9031",
            &[("1", "LOAD"), ("4", "GND"), ("8", "+5V")],
        ),
    ]);
    qualified(&found, "U1", "U2");
}

#[test]
fn a_pin_into_a_non_numeric_resistor_is_unverified() {
    let found = findings(&[(R0603, "R1", "PROG", &[("1", "Y"), ("2", "GND")])]);
    qualified(&found, "U1", "R1 (PROG");
}

#[test]
fn a_pin_into_a_valueless_shunt_through_a_resistor_is_unverified() {
    let found = findings(&[
        (R0603, "R1", "330", &[("1", "Y"), ("2", "SENSE")]),
        (R2512, "R2", "SHUNT", &[("1", "SENSE"), ("2", "GND")]),
    ]);
    qualified(&found, "U1", "R2 (SHUNT");
}

#[test]
fn a_pin_into_a_trimmer_is_unverified() {
    let found = findings(&[(
        "Potentiometer_SMD:Potentiometer_Bourns_3314J_Vertical",
        "RV1",
        "10k",
        &[("1", "Y"), ("2", "Y"), ("3", "GND")],
    )]);
    qualified(&found, "U1", "RV1");
}

#[test]
fn an_led_in_series_with_a_non_numeric_resistor_is_unverified() {
    let found = findings(&[
        (LED, "D1", "RED_LED", &[("1", "CATH"), ("2", "+5V")]),
        (R0603, "R1", "PROG", &[("1", "CATH"), ("2", "GND")]),
    ]);
    qualified(&found, "D1", "R1 (PROG");
}

/// Control: every part on the paths is modelled, and an unmodelled IC that
/// sits only on the rails is on no part's signal path.
#[test]
fn a_fully_modelled_path_is_silent() {
    let found = findings(&[
        (R0603, "R1", "1k", &[("1", "Y"), ("2", "ANODE")]),
        (LED, "D1", "RED_LED", &[("1", "GND"), ("2", "ANODE")]),
        (SOIC8, "U2", "XQZ9031", &[("4", "GND"), ("8", "+5V")]),
    ]);
    assert!(found.is_empty(), "{found:#?}");
}

/// A header is the board's edge, not an unmodelled load.
#[test]
fn a_pin_into_a_header_is_silent() {
    let found = findings(&[(
        "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical",
        "J1",
        "HDR",
        &[("1", "Y"), ("2", "GND")],
    )]);
    assert!(found.is_empty(), "{found:#?}");
}

/// An LED on a net four other parts share is on a bus, not a series path.
#[test]
fn an_led_on_a_crowded_net_is_silent() {
    let ic = |reference| {
        (
            SOIC8,
            reference,
            "XQZ9031",
            &[("1", "BUS"), ("4", "GND")][..],
        )
    };
    let found = findings(&[
        (LED, "D1", "RED_LED", &[("1", "BUS"), ("2", "+5V")]),
        ic("U2"),
        ic("U3"),
        ic("U4"),
        ic("U5"),
    ]);
    assert!(found.is_empty(), "{found:#?}");
}

/// Parts behind the same open part share one note.
#[test]
fn leds_behind_one_unmodelled_driver_share_a_note() {
    let found = findings(&[
        (LED, "D1", "RED_LED", &[("1", "CA"), ("2", "+5V")]),
        (LED, "D2", "RED_LED", &[("1", "CB"), ("2", "+5V")]),
        (
            SOIC8,
            "U2",
            "XQZ9031",
            &[("1", "CA"), ("2", "CB"), ("4", "GND")],
        ),
    ]);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].refs, ["D1", "D2"], "{found:#?}");
    assert!(found[0].is_abstention() && found[0].message.contains("U2"));
}
