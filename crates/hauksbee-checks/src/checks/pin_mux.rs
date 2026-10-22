//! A peripheral bus wired to MCU pins that cannot carry it.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A microcontroller's hardware SPI, I2C, UART and PWM outputs reach fixed pads:
//! an `ATmega328P`'s TWI data line is pad 27 and nothing else, its SPI clock is
//! `PB5`. A board that wires the bus to other pads cannot use the peripheral
//! (the firmware either falls back to a slow bit-banged copy or the board needs
//! a respin). The card's `[models.pin_capability.<function>]` tables
//! (`spi_sck`, `spi_mosi`, `spi_miso`, `i2c_sda`, `i2c_scl`, `uart_tx`,
//! `uart_rx`, `pwm`) state which pads of the package can carry each function,
//! read from the datasheet's alternate-function table.
//!
//! Judged only on definite evidence:
//!
//! - **The card has the table.** A table is a complete statement about the
//!   package; a card with none says nothing (every part whose pins reach any pad
//!   through a routing matrix, an `ESP32` or an `RP2040` PIO, declares none).
//! - **The pad is a general-purpose port pad** of the card's pin map, with a
//!   net.
//! - **The intent is known.** For SPI and I2C, a part on the net (one the MCU
//!   is not, not a connector) has a pin the card names, or the symbol names,
//!   `SCK`, `MOSI`, `MISO`, `SDA` or `SCL`, and the net's own name, where it
//!   names a bus function, says the same. For a UART, the net is named for a
//!   serial port (`UART`, `TX`, `RX`...) and a part on it has a serial pin; the
//!   check then asks only whether the pad can be any hardware UART pad, since
//!   which way a name means `TX` is the writer's viewpoint. For PWM, the net is
//!   named for it. Nets one series resistor away count as the same line.
//!
//! The table covers the hardware peripheral. A software (bit-banged) bus can
//! use any pad, so a finding names the pads that could carry the function and
//! leaves the choice to the firmware author. A PWM name alone is weaker evidence
//! than a part's pin, so that finding is Low.

use std::collections::BTreeSet;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_connector_like, uart_pin_role, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Pin, Severity,
};
use hauksbee_ir::pin_functions::{names_a_uart, peripheral_functions_named, PeripheralFunction};
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use PeripheralFunction as F;

/// How many alternative pads a message lists before it says "and N more".
const SHOWN: usize = 8;

/// A general-purpose port pad of a card's pin map (`pb5_sck`, `pa9_tx`, `gpio4`).
fn is_port_role(role: &str) -> bool {
    let r = role.trim().to_ascii_lowercase();
    let head = r.split('_').next().unwrap_or("");
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let port = head.len() >= 3
        && head.starts_with('p')
        && head.as_bytes()[1].is_ascii_lowercase()
        && digits(&head[2..]);
    port || ["gpio", "io", "gp"]
        .iter()
        .any(|p| head.strip_prefix(p).is_some_and(digits))
}

/// The bus function a partner pin states: the card's role first, else the
/// symbol's pin name (`SCK`, `MOSI`, `MISO`, `SDA`, `SCL`).
fn bus_function(card_role: Option<&str>, symbol: &str) -> Option<PeripheralFunction> {
    let from = |name: &str| {
        let found: Vec<_> = peripheral_functions_named(name)
            .into_iter()
            .filter(|f| !matches!(f, F::Pwm))
            .collect();
        match found[..] {
            [one] => Some(one),
            _ => None,
        }
    };
    card_role.and_then(from).or_else(|| from(symbol))
}

/// What the parts on the nets around a pad say the line is.
#[derive(Default)]
struct Evidence {
    /// The bus functions the partners' pins state.
    bus: BTreeSet<PeripheralFunction>,
    /// The partners that state them, as `U2 pin 3 (SDA)`.
    partners: Vec<String>,
    /// A partner has a serial-port pin.
    serial: bool,
}

/// The partner evidence for the line at `net`, one series resistor included.
fn evidence(board: &ExtractedBoard, lib: &ModelLibrary, mcu: &Component, net: i64) -> Evidence {
    let mut e = Evidence::default();
    for n in super::debug_header::series_nets(board, net) {
        for (part, pin) in board.net_members(n) {
            if std::ptr::eq(part, mcu) || is_connector_like(part) {
                continue;
            }
            let Some(fitted) = AssemblyState::of(part).fitted() else {
                continue;
            };
            let model = hauksbee_bind::binder::resolve(lib, fitted).model;
            if model.as_ref().is_some_and(|m| m.kind == ComponentKind::Mcu) {
                continue;
            }
            let role = model
                .as_ref()
                .filter(|m| super::card_fits(board, part, m))
                .and_then(|m| m.pins.get(&pin.number))
                .map(String::as_str);
            let at = format!("{} pin {} ({})", part.reference, pin.number, pin.function);
            if let Some(f) = bus_function(role, &pin.function) {
                e.bus.insert(f);
                e.partners.push(at);
            } else if serial_pin(part, pin, role) {
                e.serial = true;
                e.partners.push(at);
            }
        }
    }
    e
}

/// Whether `pin` of `part` is a serial-port pin (and the part is no bus
/// transceiver, whose `TXD`/`RXD` face the controller the other way round).
fn serial_pin(part: &Component, pin: &Pin, card_role: Option<&str>) -> bool {
    let transceiver = part.pins.iter().any(|p| {
        p.function
            .to_ascii_uppercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| matches!(t, "CANH" | "CANL"))
    });
    if transceiver {
        return false;
    }
    let as_symbol = |function: String| Pin {
        function,
        kind: pin.kind.clone(),
        ..Pin::default()
    };
    uart_pin_role(pin).is_some()
        || card_role.is_some_and(|r| uart_pin_role(&as_symbol(r.replace('_', "/"))).is_some())
}

/// The pads of `table` that can carry the function, as a message lists them.
fn alternatives(model: &ModelEntry, functions: &[PeripheralFunction]) -> String {
    let mut shown: Vec<String> = Vec::new();
    for f in functions {
        let Some(table) = model.pin_capability.table(*f) else {
            continue;
        };
        for (pad, signal) in &table.pads {
            let role = model.pins.get(pad).map_or("", String::as_str);
            let item = format!("pad {pad} ({}, {signal})", role.to_ascii_uppercase());
            if !shown.contains(&item) {
                shown.push(item);
            }
        }
    }
    let more = shown.len().saturating_sub(SHOWN);
    let list = shown.into_iter().take(SHOWN).collect::<Vec<_>>().join(", ");
    if more > 0 {
        format!("{list} and {more} more")
    } else {
        list
    }
}

/// Flag every MCU pad wired for a peripheral function its card says the pad
/// cannot carry.
#[must_use]
pub fn pin_mux_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for (mcu, model) in super::resolved_components(board, lib) {
        if model.kind != ComponentKind::Mcu || model.pin_capability.is_empty() {
            continue;
        }
        for pin in &mcu.pins {
            let (Some(net), Some(role)) = (
                pin.net.filter(|id| *id != 0),
                model.pins.get(&pin.number).filter(|r| is_port_role(r)),
            ) else {
                continue;
            };
            let line_names: Vec<String> = super::debug_header::series_nets(board, net)
                .into_iter()
                .filter_map(|n| board.net(n).map(|x| x.name.clone()))
                .collect();
            let names_serial = line_names.iter().any(|n| names_a_uart(n));
            let named: BTreeSet<_> = line_names
                .iter()
                .flat_map(|n| peripheral_functions_named(n))
                .collect();
            let ev = evidence(board, lib, mcu, net);
            // What the line is, and the functions whose tables say where it can go.
            let (wanted, tables, strength): (String, Vec<F>, Severity) =
                if let [one] = ev.bus.iter().copied().collect::<Vec<_>>()[..] {
                    if named.iter().any(|n| *n != one && *n != F::Pwm) {
                        continue;
                    }
                    (one.label().to_owned(), vec![one], Severity::Medium)
                } else if ev.bus.is_empty() && ev.serial && names_serial {
                    (
                        "a UART pin".to_owned(),
                        vec![F::UartTx, F::UartRx],
                        Severity::Medium,
                    )
                } else if ev.bus.is_empty() && named.contains(&F::Pwm) && named.len() == 1 {
                    (F::Pwm.label().to_owned(), vec![F::Pwm], Severity::Low)
                } else {
                    continue;
                };
            // Every table the line could use must be declared, and the pad in none.
            if tables
                .iter()
                .any(|f| model.pin_capability.table(*f).is_none())
                || tables.iter().any(|f| {
                    model
                        .pin_capability
                        .table(*f)
                        .is_some_and(|t| t.pads.contains_key(&pin.number))
                })
            {
                continue;
            }
            let others: Vec<&str> = F::ALL
                .iter()
                .filter(|g| {
                    !tables.contains(g)
                        && model
                            .pin_capability
                            .table(**g)
                            .is_some_and(|t| t.pads.contains_key(&pin.number))
                })
                .map(|g| g.label())
                .collect();
            let net_name = board.net(net).map_or_else(String::new, |n| n.name.clone());
            let evidence_text = if ev.partners.is_empty() {
                format!("the net is named '{net_name}'")
            } else {
                format!("{} on net '{net_name}'", ev.partners.join(", "))
            };
            let also = if others.is_empty() {
                String::new()
            } else {
                format!(" (that pad is this part's {})", others.join(" and "))
            };
            report.findings.push(LintFinding {
                check: LintCheck::PinMuxCapability,
                severity: strength,
                message: format!(
                    "{} ({}) pad {} ({}) is wired as {wanted}, but the card '{}' lists no such \
                     function on that pad{also}: {evidence_text}. A hardware peripheral can \
                     reach only {}. Move the line to one of those pads; if the firmware will \
                     bit-bang it in software this does not apply, but the hardware peripheral \
                     is then unavailable",
                    mcu.reference,
                    mcu.value,
                    pin.number,
                    role.to_ascii_uppercase(),
                    model.id,
                    alternatives(&model, &tables),
                ),
                refs: vec![mcu.reference.clone()],
                nets: vec![net_name],
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    const GND: i64 = 1;
    const A: i64 = 2;
    const B: i64 = 3;

    /// An `ATmega328P` (DIP-28 card) with `pads` on its pins.
    fn mcu(pads: &[(&str, i64)]) -> Component {
        part("U1", "ATmega328P", pads)
    }

    /// A sensor whose symbol names the pins on `pads`, `(pad, symbol name, net)`.
    fn sensor(pads: &[(&str, &str, i64)]) -> Component {
        let mut u = part(
            "U2",
            "SomeSensor",
            &pads.iter().map(|(p, _, n)| (*p, *n)).collect::<Vec<_>>(),
        );
        for (pin, (_, name, _)) in u.pins.iter_mut().zip(pads) {
            pin.function = (*name).into();
        }
        u
    }

    fn run(nets: &[(i64, &str)], parts: Vec<Component>) -> Vec<LintFinding> {
        pin_mux_lint(&board(nets, parts), &ModelLibrary::builtin()).findings
    }

    #[test]
    fn an_i2c_bus_on_pads_that_are_not_twi_is_flagged_with_the_pads_that_are() {
        // SDA on PD2 (pad 4) and SCL on PD3 (pad 5): neither is a TWI pad.
        let found = run(
            &[(GND, "GND"), (A, "SDA"), (B, "SCL")],
            vec![
                mcu(&[("4", A), ("5", B)]),
                sensor(&[("1", "SDA", A), ("2", "SCL", B)]),
            ],
        );
        assert_eq!(found.len(), 2, "{found:#?}");
        let sda = &found[0];
        assert_eq!(sda.check, LintCheck::PinMuxCapability);
        assert_eq!(sda.severity, Severity::Medium);
        assert_eq!(sda.refs, ["U1"]);
        assert_eq!(sda.nets, ["SDA"]);
        for want in [
            "pad 4 (PD2_INT0)",
            "I2C data (SDA)",
            "pad 27 (PC4_ADC4_SDA, SDA)",
            "U2 pin 1 (SDA)",
            "bit-bang",
        ] {
            assert!(sda.message.contains(want), "{want}: {}", sda.message);
        }
        assert!(found[1].message.contains("pad 28 (PC5_ADC5_SCL, SCL)"));
        // The pad of the other line is named as what it is.
        let swapped = run(
            &[(GND, "GND"), (A, "SDA"), (B, "SCL")],
            vec![
                mcu(&[("27", B), ("28", A)]),
                sensor(&[("1", "SDA", A), ("2", "SCL", B)]),
            ],
        );
        assert_eq!(swapped.len(), 2, "{swapped:#?}");
        assert!(swapped[0]
            .message
            .contains("that pad is this part's I2C data (SDA)"));
    }

    #[test]
    fn spi_on_a_pad_the_card_lists_for_spi_or_the_usart_in_spi_mode_is_silent() {
        let nets = [(GND, "GND"), (A, "SCK"), (B, "MOSI")];
        let dev = || sensor(&[("1", "SCK", A), ("2", "MOSI", B)]);
        // PB5 and PB3: the SPI pins.
        assert!(run(&nets, vec![mcu(&[("19", A), ("17", B)]), dev()]).is_empty());
        // PD4 (XCK) and PD1 (TXD): the USART's SPI mode, a hardware path too.
        assert!(run(&nets, vec![mcu(&[("6", A), ("3", B)]), dev()]).is_empty());
        // SCK on PB0 (pad 14) is not.
        let found = run(&nets, vec![mcu(&[("14", A), ("17", B)]), dev()]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].message.contains("SPI clock (SCK)"));
        assert!(found[0].message.contains("pad 19 (PB5_SCK, SCK)"));
    }

    #[test]
    fn correct_twi_unknown_names_one_sided_evidence_and_no_table_are_silent() {
        let nets = [(GND, "GND"), (A, "SDA"), (B, "SCL"), (4, "NET_A")];
        let bus = || sensor(&[("1", "SDA", A), ("2", "SCL", B)]);
        // PC4 and PC5.
        assert!(run(&nets, vec![mcu(&[("27", A), ("28", B)]), bus()]).is_empty());
        // A net named SDA but nothing on the other end that says it is a bus.
        assert!(run(&nets, vec![mcu(&[("4", A)])]).is_empty());
        // A partner pin that names no bus function.
        assert!(run(&nets, vec![mcu(&[("4", 4)]), sensor(&[("1", "INT", 4)])]).is_empty());
        // The name and the partner disagree: not definite.
        let odd = run(&nets, vec![mcu(&[("4", B)]), sensor(&[("1", "SDA", B)])]);
        assert!(odd.is_empty(), "{odd:#?}");
        // A pad that is not general purpose (VCC, pad 7) is not this check's.
        assert!(run(&nets, vec![mcu(&[("7", A)]), bus()]).is_empty());
        // An MCU whose card has no table, and an unfitted MCU.
        assert!(run(&nets, vec![part("U1", "STM32F4XX", &[("4", A)]), bus()]).is_empty());
        let mut dnp = mcu(&[("4", A), ("5", B)]);
        dnp.dnp = true;
        assert!(run(&nets, vec![dnp, bus()]).is_empty());
        // A second MCU on the line is a peer, not evidence of a bus.
        let peer = part("U3", "ATmega328P", &[("27", A)]);
        assert!(run(&nets, vec![mcu(&[("4", A)]), peer]).is_empty());
    }

    #[test]
    fn a_uart_on_pads_that_are_not_the_usart_and_a_named_pwm_are_flagged_once_each() {
        let nets = [(GND, "GND"), (A, "UART_TX"), (B, "FAN_PWM")];
        // UART_TX with a serial pin on the other end, on PB0 (pad 14).
        let found = run(&nets, vec![mcu(&[("14", A)]), sensor(&[("1", "RXD", A)])]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].message.contains("a UART pin"),
            "{}",
            found[0].message
        );
        assert!(found[0].message.contains("pad 3 (PD1_TXD, TXD0)"));
        // On PD0 or PD1 it is on the USART, whichever way the name means TX.
        assert!(run(&nets, vec![mcu(&[("3", A)]), sensor(&[("1", "RXD", A)])]).is_empty());
        assert!(run(&nets, vec![mcu(&[("2", A)]), sensor(&[("1", "RXD", A)])]).is_empty());
        // A net named for a UART with nothing serial on the other end: no evidence.
        assert!(run(&nets, vec![mcu(&[("14", A)])]).is_empty());
        // FAN_PWM on PB0 is Low; on PB1 (OC1A, pad 15) it is silent.
        let pwm = run(&nets, vec![mcu(&[("14", B)])]);
        assert_eq!(pwm.len(), 1, "{pwm:#?}");
        assert_eq!(pwm[0].severity, Severity::Low);
        assert!(pwm[0].message.contains("pad 15 (PB1_OC1A, OC1A)"));
        assert!(run(&nets, vec![mcu(&[("15", B)])]).is_empty());
    }

    #[test]
    fn a_can_transceiver_is_no_uart_evidence() {
        let nets = [(GND, "GND"), (A, "CAN_TX")];
        let can = sensor(&[("1", "TXD", A), ("2", "CANH", 0)]);
        assert!(run(&nets, vec![mcu(&[("14", A)]), can]).is_empty());
    }
}
