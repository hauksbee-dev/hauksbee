//! Pin-function reasoning, end to end through a KiCad netlist: a peripheral
//! alternate function is read from the MCU card, not from the pad's port name.
//!
//! Each class has a flawed board that must be flagged and a correct twin that
//! must stay silent on the same net:
//!
//! - a UART transmit pad (`ATmega328P` PD1/TXD, pad 3) facing another transmitter
//!   is contention, though the pad is a GPIO-capable port pad;
//! - a UART device's TX or RX wired to an MCU pad that cannot carry the matching
//!   UART function is wrong, and the message names the pads that would work;
//! - an analogue sensor output wired to an MCU pad with no ADC channel is wrong,
//!   and the message names the ADC pads.
//!
//! The card values come from `crates/hauksbee-models/db/mcu.toml` (`atmega328p`: the
//! `pin_capability` `uart_tx` / `uart_rx` / `adc` tables, from Microchip
//! DS40002061B Table 14-9 for PD0 RXD / PD1 TXD; the card's `adc` table for PC0-PC5).

use std::fmt::Write as _;

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, Severity};
use hauksbee_models::ModelLibrary;

/// `(pad, pin name, electrical type, net)`.
pub type Pin<'a> = (&'a str, &'a str, &'a str, &'a str);
/// `(reference, value, library part, pins)`.
pub type Part<'a> = (&'a str, &'a str, &'a str, &'a [Pin<'a>]);

/// The KiCad netlist text of `parts`.
pub fn netlist(parts: &[Part<'_>]) -> String {
    let mut nets: Vec<&str> = Vec::new();
    for (.., pins) in parts {
        for (.., net) in *pins {
            if !nets.contains(net) {
                nets.push(net);
            }
        }
    }
    let mut out = String::from("(export (version \"E\")\n  (components\n");
    for (reference, value, part, _) in parts {
        writeln!(
            out,
            "    (comp (ref \"{reference}\") (value \"{value}\") (libsource (lib \"X\") (part \"{part}\")))"
        )
        .unwrap();
    }
    out.push_str("  )\n  (nets\n");
    for (i, net) in nets.iter().enumerate() {
        write!(out, "    (net (code \"{}\") (name \"{net}\")", i + 1).unwrap();
        for (reference, .., pins) in parts {
            for (pad, name, kind, n) in *pins {
                if n == net {
                    write!(
                        out,
                        " (node (ref \"{reference}\") (pin \"{pad}\") (pinfunction \"{name}\") (pintype \"{kind}\"))"
                    )
                    .unwrap();
                }
            }
        }
        out.push_str(")\n");
    }
    out.push_str("  )\n)\n");
    out
}

/// Every finding of the engine lint on `parts` that sits on `net`, other than the
/// rating abstentions for parts that have no model (this fixture has no GPS model).
fn findings_on(net: &str, parts: &[Part<'_>]) -> Vec<LintFinding> {
    let board = ExtractedBoard::from_kicad_netlist(&netlist(parts)).expect("parse netlist");
    hauksbee_engine::checks::engine_lint(&board, &ModelLibrary::builtin())
        .findings
        .into_iter()
        .filter(|f| f.check != LintCheck::DcOverstress && f.nets.iter().any(|n| n == net))
        .collect()
}

/// The `ATmega328P` (DIP-28) with the given port pads wired.
const fn mcu<'a>(pads: &'a [Pin<'a>]) -> Part<'a> {
    ("U1", "ATmega328P", "ATmega328P", pads)
}

const GPS_TX: Pin<'static> = ("1", "TXD", "output", "GPS_TX");
const GPS: Part<'static> = ("U2", "NEO-GPS", "GPS-MODULE", &[GPS_TX]);

/// A UART transmit pad facing a GPS module's TXD: both drive the line.
#[test]
fn a_uart_transmit_pad_tied_to_a_transmitting_peer_is_contention() {
    let flawed = findings_on(
        "GPS_TX",
        &[mcu(&[("3", "PD1", "bidirectional", "GPS_TX")]), GPS],
    );
    let contention: Vec<_> = flawed
        .iter()
        .filter(|f| f.check == LintCheck::OutputContention)
        .collect();
    assert_eq!(contention.len(), 1, "{flawed:#?}");
    assert_eq!(contention[0].severity, Severity::High);
    assert_eq!(contention[0].refs, ["U1", "U2"]);
    // One finding for the one fault: the pin-mux check does not repeat it.
    assert_eq!(flawed.len(), 1, "{flawed:#?}");

    // Correct: the GPS TXD goes to the receive pad, PD0 (pad 2).
    let correct = findings_on(
        "GPS_TX",
        &[mcu(&[("2", "PD0", "bidirectional", "GPS_TX")]), GPS],
    );
    assert!(correct.is_empty(), "{correct:#?}");
}

/// Two MCUs' TXD pads on one line.
#[test]
fn two_mcu_transmit_pads_on_one_uart_line_are_contention() {
    let pads: &[Pin<'_>] = &[("3", "PD1", "bidirectional", "UART_TX")];
    let flawed = findings_on(
        "UART_TX",
        &[mcu(pads), ("U3", "ATmega328P", "ATmega328P", pads)],
    );
    assert!(
        flawed
            .iter()
            .any(|f| f.check == LintCheck::OutputContention),
        "{flawed:#?}"
    );
    // Correct: TX of one into RX of the other.
    let correct = findings_on(
        "UART_TX",
        &[
            mcu(pads),
            (
                "U3",
                "ATmega328P",
                "ATmega328P",
                &[("2", "PD0", "bidirectional", "UART_TX")],
            ),
        ],
    );
    assert!(correct.is_empty(), "{correct:#?}");
}

/// A device that transmits must reach a pad the card gives a receive function.
#[test]
fn a_uart_device_tx_on_a_pad_with_no_receive_function_is_flagged_with_the_pads_that_work() {
    // PB0 (pad 14) has no UART function on the ATmega328P.
    let flawed = findings_on(
        "GPS_TX",
        &[mcu(&[("14", "PB0", "bidirectional", "GPS_TX")]), GPS],
    );
    assert_eq!(flawed.len(), 1, "{flawed:#?}");
    for want in ["UART receive (RX)", "pad 2 (PD0_RXD", "U2 pin 1 (TXD)"] {
        assert!(
            flawed[0].message.contains(want),
            "{want}: {}",
            flawed[0].message
        );
    }
    let correct = findings_on(
        "GPS_TX",
        &[mcu(&[("2", "PD0", "bidirectional", "GPS_TX")]), GPS],
    );
    assert!(correct.is_empty(), "{correct:#?}");
}

/// A device that only listens must be fed from a transmit-capable pad.
#[test]
fn a_uart_device_rx_on_a_pad_with_no_transmit_function_is_flagged_with_the_pads_that_work() {
    let dev: Part<'_> = (
        "U2",
        "BT-MODULE",
        "BT-MODULE",
        &[("1", "RXD", "input", "TO_DEVICE")],
    );
    let flawed = findings_on(
        "TO_DEVICE",
        &[mcu(&[("14", "PB0", "bidirectional", "TO_DEVICE")]), dev],
    );
    assert_eq!(flawed.len(), 1, "{flawed:#?}");
    for want in ["UART transmit (TX)", "pad 3 (PD1_TXD"] {
        assert!(
            flawed[0].message.contains(want),
            "{want}: {}",
            flawed[0].message
        );
    }
    let correct = findings_on(
        "TO_DEVICE",
        &[mcu(&[("3", "PD1", "bidirectional", "TO_DEVICE")]), dev],
    );
    assert!(correct.is_empty(), "{correct:#?}");
}

/// A sensor's analogue output must reach an ADC channel.
#[test]
fn an_analogue_sensor_output_on_a_pad_with_no_adc_channel_is_flagged_with_the_adc_pads() {
    let sensor: Part<'_> = (
        "U2",
        "TMP-SENSOR",
        "TMP-SENSOR",
        &[("1", "AOUT", "output", "TEMP")],
    );
    // PD2 (pad 4) has no ADC channel; PC0 (pad 23) is ADC0.
    let flawed = findings_on(
        "TEMP",
        &[mcu(&[("4", "PD2", "bidirectional", "TEMP")]), sensor],
    );
    assert_eq!(flawed.len(), 1, "{flawed:#?}");
    for want in ["lists no ADC channel", "ADC-capable pads", "PC0_ADC0"] {
        assert!(
            flawed[0].message.contains(want),
            "{want}: {}",
            flawed[0].message
        );
    }
    let correct = findings_on(
        "TEMP",
        &[mcu(&[("23", "PC0", "bidirectional", "TEMP")]), sensor],
    );
    assert!(correct.is_empty(), "{correct:#?}");
}
