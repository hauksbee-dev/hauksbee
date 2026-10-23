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
//! A pad a table lists under `master_only` (the USART's SPI mode, which has
//! no slave role) serves the function only for the bus master: a partner that
//! drives the bus (its `SCK` or `MOSI` typed an output, its `MISO` an input)
//! makes the MCU the slave, and that pad then counts as unlisted.
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
use hauksbee_ir::pin_functions::{
    names_a_uart, peripheral_functions_named, uart_role_direction, PeripheralFunction,
};
use hauksbee_models::schema::FunctionPads;
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use super::signal_direction::{classify, Dir, Member};
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
    /// A partner drives the bus (its `SCK` or `MOSI` is typed an output, its
    /// `MISO` an input), so the MCU is the SPI slave.
    mcu_is_slave: bool,
    /// The serial partners that drive the line (a transmit pin typed output),
    /// as `U2 pin 3 (TXD)`.
    serial_drives: Vec<String>,
    /// The serial partners that only listen to it (a receive pin typed input).
    serial_listens: Vec<String>,
    /// A serial partner whose direction neither its symbol nor its card settles.
    serial_open: bool,
    /// A serial partner pin is a dedicated UART pin: its whole name is the line.
    serial_dedicated: bool,
}

/// Which way the serial partners on a line face the controller's pad.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SerialWay {
    /// Every serial partner transmits into the line: the pad must receive.
    Drives,
    /// Every serial partner listens to the line: the pad must transmit.
    Listens,
}

impl Evidence {
    /// A partner has a serial-port pin.
    const fn is_serial(&self) -> bool {
        self.serial_open || !self.serial_drives.is_empty() || !self.serial_listens.is_empty()
    }

    /// The one direction every serial partner agrees on, when all of them state one.
    const fn serial_way(&self) -> Option<SerialWay> {
        if self.serial_open {
            return None;
        }
        match (
            self.serial_drives.is_empty(),
            self.serial_listens.is_empty(),
        ) {
            (false, true) => Some(SerialWay::Drives),
            (true, false) => Some(SerialWay::Listens),
            _ => None,
        }
    }
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
                let kind = pin.kind.trim().to_ascii_lowercase();
                e.mcu_is_slave |= match f {
                    F::SpiSck | F::SpiMosi => kind == "output",
                    F::SpiMiso => kind == "input",
                    _ => false,
                };
                e.bus.insert(f);
                e.partners.push(at);
            } else if serial_pin(part, pin, role) {
                e.serial_dedicated |= uart_pin_role(pin).is_some_and(|(_, dedicated)| dedicated);
                let card = model.as_ref().filter(|m| super::card_fits(board, part, m));
                match classify(part, pin, card) {
                    Member::Pin(Dir::Out) => e.serial_drives.push(at.clone()),
                    Member::Pin(Dir::In) => e.serial_listens.push(at.clone()),
                    _ => e.serial_open = true,
                }
                e.partners.push(at);
            }
        }
    }
    e
}

/// Whether `pin` of `part` is a serial-port pin (and the part is no bus
/// transceiver, whose `TXD`/`RXD` face the controller the other way round).
pub(super) fn serial_pin(part: &Component, pin: &Pin, card_role: Option<&str>) -> bool {
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

/// Whether `pad` can carry the function of `table`: it is listed, and not for
/// the bus master alone when the MCU is the slave.
fn carries(table: &FunctionPads, pad: &str, mcu_is_slave: bool) -> bool {
    table.pads.contains_key(pad) && !(mcu_is_slave && table.master_only.contains(pad))
}

/// What a card says a pad does on a UART.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UartPad {
    /// The pad is a UART transmit pad and no receive pad.
    Tx,
    /// The pad is a UART receive pad and no transmit pad.
    Rx,
    /// The card gives the pad both a transmit and a receive function.
    Both,
    /// The card gives the pad no UART function (or does not say).
    Neither,
}

/// The UART function the card states for `pad`: its `uart_tx` and `uart_rx`
/// tables and, when `with_roles` is set, the transmit or receive line the pad's
/// own role names (`pd1_txd`, `p01_txd`). The tables are a complete statement
/// about the package, so "no function" is only safe to say from them alone
/// (`with_roles` false); a role is a positive statement and nothing more.
pub(super) fn uart_pad(model: &ModelEntry, pad: &str, with_roles: bool) -> UartPad {
    let listed = |f: PeripheralFunction| {
        model
            .pin_capability
            .table(f)
            .is_some_and(|t| t.pads.contains_key(pad))
    };
    let role = with_roles
        .then(|| model.pins.get(pad))
        .flatten()
        .and_then(|r| uart_role_direction(r));
    let tx = listed(F::UartTx) || role == Some(true);
    let rx = listed(F::UartRx) || role == Some(false);
    match (tx, rx) {
        (true, false) => UartPad::Tx,
        (false, true) => UartPad::Rx,
        (true, true) => UartPad::Both,
        (false, false) => UartPad::Neither,
    }
}

/// The pads of `model` that carry UART receive, as a message lists them (the
/// card's table, else the roles that name a receive line).
pub(super) fn receive_pads(model: &ModelEntry) -> String {
    let mut pads: Vec<String> = Vec::new();
    for pad in model.pins.keys() {
        if matches!(uart_pad(model, pad, true), UartPad::Rx | UartPad::Both) {
            let role = model.pins.get(pad).map_or("", String::as_str);
            let signal = model
                .pin_capability
                .uart_rx
                .as_ref()
                .and_then(|t| t.pads.get(pad))
                .map_or(String::new(), |s| format!(", {s}"));
            pads.push(format!("pad {pad} ({}{signal})", role.to_ascii_uppercase()));
        }
    }
    let more = pads.len().saturating_sub(SHOWN);
    pads.truncate(SHOWN);
    let list = pads.join(", ");
    if more > 0 {
        format!("{list} and {more} more")
    } else {
        list
    }
}

/// The pads of `table` that can carry the function, as a message lists them.
fn alternatives(model: &ModelEntry, functions: &[PeripheralFunction], slave: bool) -> String {
    let mut shown: Vec<String> = Vec::new();
    for f in functions {
        let Some(table) = model.pin_capability.table(*f) else {
            continue;
        };
        for (pad, signal) in table
            .pads
            .iter()
            .filter(|(pad, _)| carries(table, pad, slave))
        {
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

/// What else the card says `pad` is, when it is the pad of another peripheral
/// function than the ones asked for (`" (that pad is this part's I2C data (SDA))"`).
fn also_text(model: &ModelEntry, asked: &[PeripheralFunction], pad: &str) -> String {
    let others: Vec<&str> = F::ALL
        .iter()
        .filter(|g| {
            !asked.contains(g)
                && model
                    .pin_capability
                    .table(**g)
                    .is_some_and(|t| t.pads.contains_key(pad))
        })
        .map(|g| g.label())
        .collect();
    if others.is_empty() {
        String::new()
    } else {
        format!(" (that pad is this part's {})", others.join(" and "))
    }
}

/// What the line is, and the functions whose tables say where it can go; the
/// label, the tables and the severity. `None` when the line is not decided.
fn wanted_functions(
    ev: &Evidence,
    named: &BTreeSet<F>,
    names_serial: bool,
) -> Option<(String, Vec<F>, Severity)> {
    if let [one] = ev.bus.iter().copied().collect::<Vec<_>>()[..] {
        if named.iter().any(|n| *n != one && *n != F::Pwm) {
            return None;
        }
        return Some((one.label().to_owned(), vec![one], Severity::Medium));
    }
    if !ev.bus.is_empty() {
        return None;
    }
    if ev.is_serial()
        && (names_serial || (ev.serial_dedicated && ev.serial_way().is_some() && named.is_empty()))
    {
        return Some(match ev.serial_way() {
            // The partner transmits into the line: the pad must receive.
            Some(SerialWay::Drives) => (
                "UART receive (RX)".to_owned(),
                vec![F::UartRx],
                Severity::Medium,
            ),
            // The partner only listens: the pad must transmit.
            Some(SerialWay::Listens) => (
                "UART transmit (TX)".to_owned(),
                vec![F::UartTx],
                Severity::Medium,
            ),
            None => (
                "a UART pin".to_owned(),
                vec![F::UartTx, F::UartRx],
                Severity::Medium,
            ),
        });
    }
    (named.contains(&F::Pwm) && named.len() == 1)
        .then(|| (F::Pwm.label().to_owned(), vec![F::Pwm], Severity::Low))
}

/// The sentence that says what made the line what it is.
fn evidence_text(ev: &Evidence, directed: bool, net_name: &str) -> String {
    if ev.partners.is_empty() {
        format!("the net is named '{net_name}'")
    } else if directed && ev.serial_way() == Some(SerialWay::Drives) {
        format!(
            "{} transmits into net '{net_name}', so this part has to receive it",
            ev.serial_drives.join(", ")
        )
    } else if directed && ev.serial_way() == Some(SerialWay::Listens) {
        format!(
            "{} only listens to net '{net_name}', so this part has to transmit on it",
            ev.serial_listens.join(", ")
        )
    } else {
        format!("{} on net '{net_name}'", ev.partners.join(", "))
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
            let Some((wanted, tables, strength)) = wanted_functions(&ev, &named, names_serial)
            else {
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
                        .is_some_and(|t| carries(t, &pin.number, ev.mcu_is_slave))
                })
            {
                continue;
            }
            // A pad listed only for the bus master, with a partner that is the master.
            let master_only = tables.iter().any(|f| {
                model
                    .pin_capability
                    .table(*f)
                    .is_some_and(|t| t.master_only.contains(&pin.number))
            });
            let net_name = board.net(net).map_or_else(String::new, |n| n.name.clone());
            let directed = tables.len() == 1 && matches!(tables[0], F::UartRx | F::UartTx);
            let evidence_text = evidence_text(&ev, directed, &net_name);
            let listed = if master_only {
                "that pad for it only as a bus master (a USART's SPI mode has no slave \
                 role), but the partner here drives the bus, so this part is the slave"
            } else {
                "no such function on that pad"
            };
            let also = also_text(&model, &tables, &pin.number);
            report.findings.push(LintFinding {
                check: LintCheck::PinMuxCapability,
                severity: strength,
                message: format!(
                    "{} ({}) pad {} ({}) is wired as {wanted}, but the card '{}' lists {listed}{also}: \
                     {evidence_text}. A hardware peripheral can reach only {}. Move the line to one of those pads; if the firmware will \
                     bit-bang it in software this does not apply, but the hardware peripheral \
                     is then unavailable",
                    mcu.reference,
                    mcu.value,
                    pin.number,
                    role.to_ascii_uppercase(),
                    model.id,
                    alternatives(&model, &tables, ev.mcu_is_slave),
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

    /// The USART's SPI mode is master only (Table 21-3): as the slave of a
    /// master that drives SCK and MOSI, the MCU needs the SPI pads.
    #[test]
    fn the_usart_spi_pads_never_serve_an_spi_slave() {
        let nets = [(GND, "GND"), (A, "SCK"), (B, "MOSI")];
        let typed = |kind: &str| {
            let mut dev = sensor(&[("1", "SCK", A), ("2", "MOSI", B)]);
            for pin in &mut dev.pins {
                pin.kind = kind.into();
            }
            dev
        };
        // A master drives the clock and data (pins typed output); XCK0 (pad 6)
        // and TXD0 (pad 3) are the USART's master-only pads.
        let found = run(&nets, vec![mcu(&[("6", A), ("3", B)]), typed("output")]);
        assert_eq!(found.len(), 2, "{found:#?}");
        for want in [
            "only as a bus master",
            "this part is the slave",
            "pad 19 (PB5_SCK, SCK)",
        ] {
            assert!(
                found[0].message.contains(want),
                "{want}: {}",
                found[0].message
            );
        }
        assert!(!found[0].message.contains("XCK0"), "{}", found[0].message);
        // The same slave on the SPI pads PB5 and PB3 is silent,
        assert!(run(&nets, vec![mcu(&[("19", A), ("17", B)]), typed("output")]).is_empty());
        // and the USART pads are fine when the MCU is the master of a slave
        // device (its SCK and MOSI pins are inputs).
        assert!(run(&nets, vec![mcu(&[("6", A), ("3", B)]), typed("input")]).is_empty());
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

    /// A serial peripheral: `(pad, pin name, electrical type, net)` per pin.
    fn serial_part(reference: &str, pins: &[(&str, &str, &str, i64)]) -> Component {
        let mut u = part(
            reference,
            "GPS-MODULE",
            &pins.iter().map(|(p, _, _, n)| (*p, *n)).collect::<Vec<_>>(),
        );
        for (pin, (_, name, kind, _)) in u.pins.iter_mut().zip(pins) {
            pin.function = (*name).into();
            pin.kind = (*kind).into();
        }
        u
    }

    const LINK: i64 = 5;

    /// `ATmega328P` (DIP-28): PD0 (pad 2) is RXD, PD1 (pad 3) is TXD, PB0 (pad 14) is neither.
    /// A device that transmits (its TXD typed output) must reach a receive pad.
    #[test]
    fn a_transmitting_partner_on_a_pad_with_no_receive_function_is_flagged_with_the_receive_pads() {
        let nets = [(GND, "GND"), (LINK, "GPS_OUT")];
        let gps = || serial_part("U2", &[("1", "TXD", "output", LINK)]);
        let found = run(&nets, vec![mcu(&[("14", LINK)]), gps()]);
        assert_eq!(found.len(), 1, "{found:#?}");
        for want in [
            "wired as UART receive (RX)",
            "U2 pin 1 (TXD) transmits into net 'GPS_OUT'",
            "has to receive it",
            "pad 2 (PD0_RXD, RXD0)",
            "bit-bang",
        ] {
            assert!(
                found[0].message.contains(want),
                "{want}: {}",
                found[0].message
            );
        }
        // On the receive pad the same board is right.
        assert!(run(&nets, vec![mcu(&[("2", LINK)]), gps()]).is_empty());
        // On the transmit pad it is a fight: the pad is named as what it is.
        let on_tx = run(&nets, vec![mcu(&[("3", LINK)]), gps()]);
        assert_eq!(on_tx.len(), 1, "{on_tx:#?}");
        assert!(
            on_tx[0].message.contains("and UART transmit (TX))"),
            "{}",
            on_tx[0].message
        );
    }

    /// The mirror image: a device that only listens (RXD typed input) needs the MCU's transmit pad.
    #[test]
    fn a_listening_partner_needs_the_transmit_pad_and_the_receive_pad_does_not_serve() {
        let nets = [(GND, "GND"), (LINK, "TO_DEVICE")];
        let dev = || serial_part("U2", &[("1", "RXD", "input", LINK)]);
        let off = run(&nets, vec![mcu(&[("14", LINK)]), dev()]);
        assert_eq!(off.len(), 1, "{off:#?}");
        for want in [
            "wired as UART transmit (TX)",
            "only listens to net",
            "pad 3 (PD1_TXD, TXD0)",
        ] {
            assert!(off[0].message.contains(want), "{want}: {}", off[0].message);
        }
        assert!(run(&nets, vec![mcu(&[("3", LINK)]), dev()]).is_empty());
        // RX facing RX: the receive pad is not a transmitter.
        let rx = run(&nets, vec![mcu(&[("2", LINK)]), dev()]);
        assert_eq!(rx.len(), 1, "{rx:#?}");
        assert!(
            rx[0].message.contains("and UART receive (RX))"),
            "{}",
            rx[0].message
        );
    }

    /// Abstentions: a partner whose direction is not stated, a line with a talker and a
    /// listener (a tap), a CAN transceiver, a peer MCU, a card with no table, and a
    /// non-serial name.
    #[test]
    fn serial_direction_abstains_unless_every_partner_states_one() {
        let nets = [(GND, "GND"), (LINK, "GPS_OUT")];
        let at = |kind: &str| serial_part("U2", &[("1", "TXD", kind, LINK)]);
        // Untyped or passive symbol pin: not decided.
        for kind in ["", "passive", "bidirectional"] {
            let found = run(&nets, vec![mcu(&[("14", LINK)]), at(kind)]);
            // A bidirectional pin named TXD is read as the transmit line it is.
            assert_eq!(
                found.len(),
                usize::from(kind == "bidirectional"),
                "{kind}: {found:#?}"
            );
        }
        // A talker and a listener on the same line (a sniffer tap): not decided.
        let tap = serial_part("U3", &[("1", "RXD", "input", LINK)]);
        assert!(run(&nets, vec![mcu(&[("14", LINK)]), at("output"), tap]).is_empty());
        // A CAN transceiver's TXD/RXD face the controller the other way round.
        let can = serial_part(
            "U2",
            &[("1", "RXD", "output", LINK), ("2", "CANH", "passive", 0)],
        );
        assert!(run(&nets, vec![mcu(&[("14", LINK)]), can]).is_empty());
        // A peer MCU is not evidence, and a card with no UART table says nothing.
        let peer = part("U3", "ATmega328P", &[("3", LINK)]);
        assert!(run(&nets, vec![mcu(&[("14", LINK)]), peer]).is_empty());
        assert!(run(
            &nets,
            vec![part("U1", "STM32F4XX", &[("14", LINK)]), at("output")]
        )
        .is_empty());
        // A line the net name says is a different bus is not read as serial.
        let spi = [(GND, "GND"), (LINK, "SPI_SCK")];
        assert!(run(&spi, vec![mcu(&[("14", LINK)]), at("output")]).is_empty());
    }
}
