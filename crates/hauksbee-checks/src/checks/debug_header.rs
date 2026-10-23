//! A programming or serial header whose pin order is not the standard one.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A header meant for a programmer or a USB-to-serial cable mates with an
//! adapter whose connector fixes every pin. A header wired in another order does
//! not work (the board's TX facing the cable's TX, MOSI where MISO belongs) and
//! a supply on a ground pin can destroy the adapter. The pinouts live in one
//! table, `hauksbee_ir::header_pinouts`, each row citing the document that
//! states it.
//!
//! The FTDI cable's pin 3 is a fixed +5 V output, not the target's supply
//! (`FT_000054` Table 4.1: 5 V even on the 3.3 V cable). Wired to a net whose
//! name states a rail that is not 5 V, the cable overdrives it, so that is
//! flagged; a 5 V rail, a name that states no voltage and an open pin are not.
//!
//! Judged only on definite evidence:
//!
//! - **The header is named.** A connector with exactly the pin count of a
//!   standard header, whose value, symbol or footprint name carries a whole word
//!   that names that one header (`FTDI`, `AVR`/`AVRISP`, `SWD`, `Cortex`). A
//!   header with no such word is the designer's own. A bare `ISP` or `JTAG` is
//!   left out (an LPC serial-boot `ISP` header and an Altera or Xilinx `JTAG`
//!   header are other connectors) and so is `ICSP`, because PIC and AVR use
//!   different orders under that name.
//! - **The pin's use is known.** What a pin carries is read from what the net
//!   connects to (and the signal nets one series resistor away; a resistor to a
//!   rail is a pull, not a series part): the programming function of the MCU pad
//!   on it (`MOSI`/`MISO`/`SCK`/`SWDIO`/`SWCLK`/reset), from the pad's card, else
//!   from the pin name of its symbol (`PB3(MOSI/OC2A)`), and only when no pad
//!   states one, from the net's name. A net named for the application's SPI is
//!   not the programming signal of the pad it reaches (an `ATtiny`'s USI data-out
//!   is the ISP `MISO` pad, and the net on it is often called `MOSI`), so a name
//!   never overrules a pad. A supply or ground name is read alongside any of
//!   these. Pads that disagree with each other, or no reading at all, leave the
//!   pin unjudged. The serial pins are judged from the UART pins of the parts on the
//!   net: a pin facing the adapter's `TXD` output must not carry a board
//!   transmitter, and the one facing its `RXD` input must not carry a board
//!   receiver.
//! - **A UART header named by its own pins.** A 6-pin connector whose symbol
//!   names its pins `GND`, a handshake line, a supply, `TXD`, `RXD`, a handshake
//!   line in that order is the FTDI cable's header whatever the part is called,
//!   and a 4-pin connector whose pins are `GND`, a supply, `TX` and `RX`, on a
//!   part named for a USB-serial adapter (`FTDI`, `CH340`, `CP2102`, `USB` with
//!   `serial`/`UART`/`TTL`), is that adapter's connector. Those names are the
//!   adapter's own (the cable's `TXD` is its output), so the board's transmitter
//!   on the `TX`/`TXD` pin or its receiver on the `RX`/`RXD` pin is flagged. A
//!   4-pin header whose pin names say nothing about whose point of view they
//!   are, or that no adapter word names, is not judged: a board's own `TX`
//!   label usually means the board's transmit, and no document fixes that order.
//! - **Two-row numbering is the standard's.** A symbol that counts down the
//!   columns (`Top_Bottom`) numbers its pins differently from the standard, so
//!   it is not judged.

use std::collections::BTreeSet;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_connector_like, is_ground, is_test_point_or_hole, uart_pin_role, Component, ExtractedBoard,
    LintCheck, LintFinding, NetLintReport, Pin, Severity,
};
use hauksbee_ir::header_pinouts::{headers_named, HeaderSignal, StandardHeader};
use hauksbee_ir::pin_functions::{functions_named, PinFunction};
use hauksbee_models::netclass::{is_positive_supply, rail_voltage};
use hauksbee_models::{ComponentKind, ModelLibrary};

/// The lower-case words of `text`, split on everything but letters and digits.
fn words(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The one standard header `comp` is named as, if it is exactly one.
fn named_header(comp: &Component) -> Option<&'static StandardHeader> {
    let pins: BTreeSet<&str> = comp.pins.iter().map(|p| p.number.as_str()).collect();
    if pins.len() != comp.pins.len() || comp.lib_id.to_ascii_lowercase().contains("top_bottom") {
        return None;
    }
    let name = words(&format!(
        "{} {} {}",
        comp.value, comp.lib_id, comp.footprint
    ));
    let mut found = headers_named(pins.len(), &name);
    let one = found.next()?;
    found.next().is_none().then_some(one)
}

/// Nets one hop of two-pin resistor away from `net`, with `net` itself: a
/// series resistor on a programming line does not change what the line is.
pub(super) fn series_nets(board: &ExtractedBoard, net: i64) -> Vec<i64> {
    let mut nets = vec![net];
    for c in &board.components {
        let is_resistor = c.reference.to_ascii_uppercase().starts_with('R') && c.pins.len() == 2;
        if !is_resistor || AssemblyState::of(c).fitted().is_none() {
            continue;
        }
        let (a, b) = (c.pins[0].net, c.pins[1].net);
        for (here, far) in [(a, b), (b, a)] {
            if here == Some(net) {
                if let Some(far) = far.filter(|f| *f != 0 && !nets.contains(f)) {
                    nets.push(far);
                }
            }
        }
    }
    nets
}

/// What one reading of a net says a header pin carries.
fn from_name(name: &str) -> Option<HeaderSignal> {
    if is_ground(name) {
        return Some(HeaderSignal::Ground);
    }
    if is_positive_supply(name) {
        return Some(HeaderSignal::Supply);
    }
    let w = words(name);
    let has = |s: &str| w.iter().any(|x| x == s);
    let functions = functions_named(name);
    let by_word = [
        (has("mosi") || has("copi"), HeaderSignal::Mosi),
        (has("miso") || has("cipo"), HeaderSignal::Miso),
        (has("sck") || has("sclk"), HeaderSignal::Sck),
        (has("swo") || has("tdo"), HeaderSignal::Swo),
        (has("tdi"), HeaderSignal::Tdi),
        (
            functions.contains(&PinFunction::SwdData) || has("tms"),
            HeaderSignal::SwdData,
        ),
        (
            functions.contains(&PinFunction::SwdClock) || has("tck"),
            HeaderSignal::SwdClock,
        ),
        (functions.contains(&PinFunction::Reset), HeaderSignal::Reset),
    ];
    let mut hits = by_word.iter().filter(|(hit, _)| *hit).map(|(_, s)| *s);
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

/// What an MCU pad role says the pad carries (`pb3_mosi_oc2a`).
fn from_role(role: &str) -> Option<HeaderSignal> {
    let w = words(role);
    let has = |s: &str| w.iter().any(|x| x == s);
    let functions = functions_named(role);
    let by_word = [
        (has("mosi"), HeaderSignal::Mosi),
        (has("miso"), HeaderSignal::Miso),
        (has("sck"), HeaderSignal::Sck),
        (
            functions.contains(&PinFunction::SwdData),
            HeaderSignal::SwdData,
        ),
        (
            functions.contains(&PinFunction::SwdClock),
            HeaderSignal::SwdClock,
        ),
        (functions.contains(&PinFunction::Reset), HeaderSignal::Reset),
    ];
    let mut hits = by_word.iter().filter(|(hit, _)| *hit).map(|(_, s)| *s);
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

/// The card role of `pin` on `part`, when its card is an MCU's and fits.
fn mcu_role(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    part: &Component,
    pin: &Pin,
) -> Option<String> {
    let fitted = AssemblyState::of(part).fitted()?;
    let model = hauksbee_bind::binder::resolve(lib, fitted).model?;
    (model.kind == ComponentKind::Mcu && super::card_fits(board, part, &model))
        .then(|| model.pins.get(&pin.number).cloned())
        .flatten()
}

/// The fixed output of an adapter's supply pin, volts.
const ADAPTER_SUPPLY_VOLTS: f64 = 5.0;
/// How far a named rail may sit from the adapter's supply before the two are
/// not one rail (a nominal 5 V rail runs 4.5 to 5.5 V).
const RAIL_TOLERANCE_VOLTS: f64 = 0.5;

/// The rail on `net` when its name states a voltage the adapter's fixed +5 V
/// output must not be wired to (`+3V3`, `+12V`). A name that states no voltage
/// is not judged.
fn rail_clash(board: &ExtractedBoard, net: i64) -> Option<(String, f64)> {
    let name = board.net(net)?.name.clone();
    let volts = rail_voltage(&name)?;
    ((volts - ADAPTER_SUPPLY_VOLTS).abs() > RAIL_TOLERANCE_VOLTS).then_some((name, volts))
}

/// What a header pin carries, and what the reading rests on.
struct Reading {
    signal: HeaderSignal,
    /// The pads the reading comes from (`U1 pin 17 (PB3(MOSI))`); empty when
    /// the reading is only the net's name.
    basis: String,
}

/// Whether `part` is a chip a programming header is wired to: an IC whose own
/// symbol names a reset or a debug pin. Its other pin names (`PB3(MOSI/OC2A)`)
/// then state the programming function of each pad.
fn is_programming_target(part: &Component) -> bool {
    !is_connector_like(part)
        && !is_test_point_or_hole(part)
        && part.pins.len() >= 6
        && part.pins.iter().any(|p| {
            matches!(
                from_role(&p.function),
                Some(HeaderSignal::Reset | HeaderSignal::SwdData | HeaderSignal::SwdClock)
            )
        })
}

/// The one signal every reading in `said` agrees on.
fn agreed(said: &[HeaderSignal]) -> Option<HeaderSignal> {
    let (first, rest) = said.split_first()?;
    rest.iter().all(|s| s == first).then_some(*first)
}

/// What the nets at `net` say a header pin carries, `None` when nothing is said
/// or the readings disagree.
///
/// A signal is decided by what the net connects to, not by what it is called:
/// the programming function of the MCU pad on it, from its card, then from its
/// symbol's pin name. The net's name states a signal only when no pad does (a
/// net named `MOSI` for the application's SPI is the ISP `MISO` pin of a chip
/// whose USI data-out is on that pad). A supply or ground name is a fact about
/// the net and is read alongside any of these.
fn pin_signal(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    header: &Component,
    net: i64,
) -> Option<Reading> {
    let mut rails = Vec::new();
    let mut named = Vec::new();
    let mut card: Vec<(HeaderSignal, String)> = Vec::new();
    let mut symbol: Vec<(HeaderSignal, String)> = Vec::new();
    for n in series_nets(board, net) {
        if let Some(name) = board.net(n).map(|x| &x.name) {
            // A resistor from the line to a rail is a pull-up or pull-down, not a
            // series part: the rail it reaches says nothing about the line.
            match from_name(name) {
                Some(s @ (HeaderSignal::Ground | HeaderSignal::Supply)) => {
                    if n == net {
                        rails.push(s);
                    }
                }
                Some(s) => named.push(s),
                None => {}
            }
        }
        for (part, pin) in board.net_members(n) {
            if std::ptr::eq(part, header) {
                continue;
            }
            let at = format!("{} pin {} ({})", part.reference, pin.number, pin.function);
            if let Some(s) = mcu_role(board, lib, part, pin).and_then(|r| from_role(&r)) {
                card.push((s, at));
            } else if is_programming_target(part) && AssemblyState::of(part).fitted().is_some() {
                if let Some(s) = from_role(&pin.function) {
                    symbol.push((s, at));
                }
            }
        }
    }
    // Pad evidence first (the card, then the symbol), the net's name last.
    let pads = if card.is_empty() { &symbol } else { &card };
    let (mut signals, basis): (Vec<HeaderSignal>, String) = if pads.is_empty() {
        (named, String::new())
    } else {
        (
            pads.iter().map(|(s, _)| *s).collect(),
            pads.iter()
                .map(|(_, at)| at.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
    };
    signals.extend(rails);
    agreed(&signals).map(|signal| Reading { signal, basis })
}

/// The UART direction of the board's parts on the nets at `net`: whether any
/// transmits and whether any receives. The header itself is not counted.
fn board_uart(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    header: &Component,
    net: i64,
) -> (Vec<String>, Vec<String>) {
    let (mut tx, mut rx) = (Vec::new(), Vec::new());
    for n in series_nets(board, net) {
        for (part, pin) in board.net_members(n) {
            if std::ptr::eq(part, header)
                || is_connector_like(part)
                || AssemblyState::of(part).fitted().is_none()
            {
                continue;
            }
            let symbol = uart_pin_role(pin).map(|(t, _)| t);
            let card = mcu_role(board, lib, part, pin).and_then(|r| {
                let as_symbol = Pin {
                    function: r.replace('_', "/"),
                    kind: pin.kind.clone(),
                    ..Pin::default()
                };
                uart_pin_role(&as_symbol).map(|(t, _)| t)
            });
            let dir = match (symbol, card) {
                (Some(a), Some(b)) if a != b => continue,
                (Some(d), _) | (None, Some(d)) => d,
                (None, None) => continue,
            };
            let at = format!("{} pin {} ({})", part.reference, pin.number, pin.function);
            if dir {
                tx.push(at);
            } else {
                rx.push(at);
            }
        }
    }
    (tx, rx)
}

/// A serial header pin (`number`) that faces the board's pin of the same
/// direction: the board's transmitter on the adapter's `TXD`, or its receiver on
/// the adapter's `RXD`.
fn serial_fault(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    header: &Component,
    number: &str,
    net: i64,
    expected: HeaderSignal,
) -> Option<String> {
    let (tx, rx) = board_uart(board, lib, header, net);
    let transmit_side = expected == HeaderSignal::AdapterTransmit;
    let (wrong, kind, other) = if transmit_side {
        (&tx, "transmit", &rx)
    } else {
        (&rx, "receive", &tx)
    };
    (!wrong.is_empty() && other.is_empty()).then(|| {
        let adapter = if transmit_side { "TXD" } else { "RXD" };
        let needed = if transmit_side { "RX" } else { "TX" };
        let name = board.net(net).map_or("", |n| n.name.as_str());
        format!(
            "pin {number} faces the adapter's {adapter}, but net '{name}' carries the \
             board's {kind} pin {}: transmit would meet transmit and receive \
             receive; the board's {needed} belongs here",
            wrong.join(", ")
        )
    })
}

/// The adapter's fixed +5 V output on a rail whose name states another voltage:
/// the pin drives 5 V whatever the board's rail is, so that rail is overdriven.
fn supply_fault(
    board: &ExtractedBoard,
    number: &str,
    net: i64,
    expected: HeaderSignal,
) -> Option<String> {
    rail_clash(board, net).map(|(rail, volts)| {
        format!(
            "pin {number} is {} in the standard, a fixed {ADAPTER_SUPPLY_VOLTS} V output \
             even on the 3.3 V cable, but net '{rail}' is a {volts} V rail: the cable would \
             drive {ADAPTER_SUPPLY_VOLTS} V onto it. Leave the pin unconnected, or feed the \
             rail from it only through a regulator for that rail",
            expected.label(),
        )
    })
}

/// The finding text for a pin that carries a different signal than the standard gives it.
fn wrong_pin_message(
    header: &StandardHeader,
    number: &str,
    expected: HeaderSignal,
    net: &str,
    got: HeaderSignal,
    basis: &str,
) -> String {
    let mut home = header.pins_of(got);
    if got == HeaderSignal::Supply {
        home.extend(header.pins_of(HeaderSignal::AdapterSupply5V));
    }
    let home = if home.is_empty() {
        String::new()
    } else {
        format!(" (it belongs on pin {})", home.join(", "))
    };
    let from = if basis.is_empty() {
        String::new()
    } else {
        format!(", read from the pad {basis}")
    };
    format!(
        "pin {number} is {} in the standard but net '{net}' carries {}{home}{from}",
        expected.label(),
        got.label(),
    )
}

/// A pin name reduced to its letters and digits, upper case (`~{CTS}` is `CTS`,
/// `+5V` is `5V`).
fn pin_word(function: &str) -> String {
    function
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_uppercase()
}

fn is_tx_word(w: &str) -> bool {
    matches!(w, "TX" | "TXD")
}

fn is_rx_word(w: &str) -> bool {
    matches!(w, "RX" | "RXD")
}

fn is_ground_word(w: &str) -> bool {
    matches!(w, "GND" | "GROUND")
}

fn is_supply_word(w: &str) -> bool {
    matches!(
        w,
        "VCC" | "VDD" | "VBUS" | "5V" | "5V0" | "3V3" | "33V" | "V33" | "VIN"
    )
}

fn is_handshake_word(w: &str) -> bool {
    matches!(
        w,
        "CTS" | "RTS" | "DTR" | "DSR" | "CTSN" | "RTSN" | "DTRN" | "DSRN"
    )
}

/// Whether the connector's value, symbol or footprint names a USB-serial
/// adapter (`FTDI`, `CH340`, `CP2102`, `USB` with `serial`, `UART` or `TTL`).
fn names_a_serial_adapter(comp: &Component) -> bool {
    let w = words(&format!(
        "{} {} {}",
        comp.value, comp.lib_id, comp.footprint
    ));
    let has = |x: &str| w.iter().any(|y| y == x);
    [
        "ftdi",
        "ft232",
        "ft232r",
        "ft231x",
        "ft230x",
        "ch340",
        "ch340g",
        "ch341",
        "cp2102",
        "cp2102n",
        "cp2104",
        "cp2105",
        "pl2303",
        "usbserial",
        "usb2ttl",
        "usbttl",
    ]
    .iter()
    .any(|x| has(x))
        || (has("usb") && (has("serial") || has("uart") || has("ttl")))
}

/// The serial pads of a UART header that the symbol's own pin names pin down:
/// each pad number with whether it is the adapter's transmit or receive. `None`
/// for a connector those names do not identify (see the module docs).
fn uart_by_pin_names(comp: &Component) -> Option<Vec<(String, HeaderSignal)>> {
    let pins: BTreeSet<&str> = comp.pins.iter().map(|p| p.number.as_str()).collect();
    if pins.len() != comp.pins.len() || comp.lib_id.to_ascii_lowercase().contains("top_bottom") {
        return None;
    }
    match comp.pins.len() {
        6 => {
            let word = |number: &str| {
                comp.pins
                    .iter()
                    .find(|p| p.number == number)
                    .map(|p| pin_word(&p.function))
            };
            let w: Vec<String> = (1..=6)
                .map(|i| word(&i.to_string()))
                .collect::<Option<_>>()?;
            let order = is_ground_word(&w[0])
                && is_handshake_word(&w[1])
                && is_supply_word(&w[2])
                && is_tx_word(&w[3])
                && is_rx_word(&w[4])
                && is_handshake_word(&w[5]);
            order.then(|| {
                vec![
                    ("4".to_string(), HeaderSignal::AdapterTransmit),
                    ("5".to_string(), HeaderSignal::AdapterReceive),
                ]
            })
        }
        4 => {
            if !names_a_serial_adapter(comp) {
                return None;
            }
            let mut out = Vec::new();
            let (mut ground, mut supply) = (0, 0);
            for p in &comp.pins {
                let w = pin_word(&p.function);
                if is_ground_word(&w) {
                    ground += 1;
                } else if is_supply_word(&w) {
                    supply += 1;
                } else if is_tx_word(&w) {
                    out.push((p.number.clone(), HeaderSignal::AdapterTransmit));
                } else if is_rx_word(&w) {
                    out.push((p.number.clone(), HeaderSignal::AdapterReceive));
                }
            }
            let count = |signal| out.iter().filter(|(_, s)| *s == signal).count();
            let one_each = ground == 1
                && supply == 1
                && count(HeaderSignal::AdapterTransmit) == 1
                && count(HeaderSignal::AdapterReceive) == 1;
            one_each.then_some(out)
        }
        _ => None,
    }
}

/// A UART header the symbol's pin names identify, whose transmit and receive
/// pins face the board's pin of the same direction.
fn uart_by_pin_names_finding(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    comp: &Component,
) -> Option<LintFinding> {
    if AssemblyState::of(comp).fitted().is_none() || !is_connector_like(comp) {
        return None;
    }
    let serial = uart_by_pin_names(comp)?;
    let mut faults = Vec::new();
    let mut nets = Vec::new();
    for (number, expected) in &serial {
        let Some(net) = comp
            .pins
            .iter()
            .find(|p| p.number == *number)
            .and_then(|p| p.net)
            .filter(|id| *id != 0)
        else {
            continue;
        };
        if let Some(f) = serial_fault(board, lib, comp, number, net, *expected) {
            faults.push(f);
            nets.push(board.net(net).map_or_else(String::new, |n| n.name.clone()));
        }
    }
    if faults.is_empty() {
        return None;
    }
    let pad_of = |signal| {
        serial
            .iter()
            .find(|(_, s)| *s == signal)
            .map_or("?", |(n, _)| n.as_str())
    };
    Some(LintFinding {
        check: LintCheck::DebugHeaderPinout,
        severity: Severity::Medium,
        message: format!(
            "{} ({}) is a USB-serial adapter header (its pins are named for the adapter: the \
             pin named TX/TXD is the adapter's transmit output, the one named RX/RXD its receive \
             input) but is not wired for one: {}. An adapter plugged into it would meet transmit \
             with transmit and receive with receive, so no serial data gets through. Cross the \
             lines: put the board's transmit on pin {} (the adapter's RX) and the board's \
             receive on pin {} (the adapter's TX), or, if the pin names are meant from the \
             board's point of view, rename them so they are not read as the adapter's",
            comp.reference,
            comp.value.trim(),
            faults.join("; "),
            pad_of(HeaderSignal::AdapterReceive),
            pad_of(HeaderSignal::AdapterTransmit),
        ),
        refs: vec![comp.reference.clone()],
        nets,
    })
}

/// Flag every named standard header whose pins carry something else.
#[must_use]
pub fn debug_header_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for comp in &board.components {
        if AssemblyState::of(comp).fitted().is_none()
            || !is_connector_like(comp)
            || is_test_point_or_hole(comp)
        {
            continue;
        }
        let Some(header) = named_header(comp) else {
            if let Some(f) = uart_by_pin_names_finding(board, lib, comp) {
                report.findings.push(f);
            }
            continue;
        };
        let name = |id: i64| board.net(id).map_or_else(String::new, |n| n.name.clone());
        let mut faults = Vec::new();
        let mut nets = Vec::new();
        let mut power = false;
        for pin in &comp.pins {
            let (Some(net), Some(expected)) =
                (pin.net.filter(|id| *id != 0), header.signal_at(&pin.number))
            else {
                continue;
            };
            let fault = match expected {
                HeaderSignal::Unused | HeaderSignal::Handshake => None,
                HeaderSignal::AdapterTransmit | HeaderSignal::AdapterReceive => {
                    serial_fault(board, lib, comp, &pin.number, net, expected)
                }
                HeaderSignal::AdapterSupply5V
                    if pin_signal(board, lib, comp, net)
                        .is_some_and(|r| r.signal == HeaderSignal::Supply) =>
                {
                    let clash = supply_fault(board, &pin.number, net, expected);
                    power |= clash.is_some();
                    clash
                }
                _ => pin_signal(board, lib, comp, net)
                    .filter(|r| r.signal != expected)
                    .map(|Reading { signal: got, basis }| {
                        power |= matches!(
                            expected,
                            HeaderSignal::Ground
                                | HeaderSignal::Supply
                                | HeaderSignal::AdapterSupply5V
                        ) || matches!(got, HeaderSignal::Ground | HeaderSignal::Supply);
                        wrong_pin_message(header, &pin.number, expected, &name(net), got, &basis)
                    }),
            };
            if let Some(f) = fault {
                faults.push(f);
                nets.push(name(net));
            }
        }
        if faults.is_empty() {
            continue;
        }
        report.findings.push(LintFinding {
            check: LintCheck::DebugHeaderPinout,
            severity: if power {
                Severity::High
            } else {
                Severity::Medium
            },
            message: format!(
                "{} is the {} but is not wired to its pinout: {}. An adapter plugged into it \
                 meets the wrong signals on those pins, so it will not connect{}. Reorder the \
                 header pins to the standard order ({}, {}), or, if the order is deliberate, name \
                 the header for what it is so it is not read as this one",
                comp.reference,
                header.name,
                faults.join("; "),
                if power {
                    " and a supply or ground on the wrong pin can damage the adapter or the board"
                } else {
                    ""
                },
                header.source.document,
                header.source.location,
            ),
            refs: vec![comp.reference.clone()],
            nets,
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    const GND: i64 = 1;
    const V5: i64 = 2;
    const TX: i64 = 3;
    const RX: i64 = 4;
    const MOSI: i64 = 5;
    const MISO: i64 = 6;
    const SCK: i64 = 7;
    const RST: i64 = 8;

    const NETS: [(i64, &str); 8] = [
        (GND, "GND"),
        (V5, "+5V"),
        (TX, "MCU_TX"),
        (RX, "MCU_RX"),
        (MOSI, "MOSI"),
        (MISO, "MISO"),
        (SCK, "SCK"),
        (RST, "/RESET"),
    ];

    /// `pads` with every other pin of an `n`-pin header left unconnected.
    fn full(pads: &[(&str, i64)], n: usize) -> Vec<(String, i64)> {
        (1..=n)
            .map(|i| {
                let number = i.to_string();
                let net = pads
                    .iter()
                    .find(|(p, _)| *p == number)
                    .map_or(0, |(_, n)| *n);
                (number, net)
            })
            .collect()
    }

    /// A 6-pin header named `value` on `pads`.
    fn header(value: &str, pads: &[(&str, i64)]) -> Component {
        let all = full(pads, 6);
        Component {
            lib_id: "Connector_Generic:Conn_01x06".into(),
            ..part(
                "J1",
                value,
                &all.iter()
                    .map(|(p, n)| (p.as_str(), *n))
                    .collect::<Vec<_>>(),
            )
        }
    }

    /// An MCU with a transmit pin on `TX` and a receive pin on `RX`.
    fn mcu() -> Component {
        let mut u = part("U1", "SomeMcu", &[("1", TX), ("2", RX)]);
        u.pins[0].function = "TXD".into();
        u.pins[1].function = "RXD".into();
        u
    }

    fn run(parts: Vec<Component>) -> Vec<LintFinding> {
        debug_header_lint(&board(&NETS, parts), &ModelLibrary::builtin()).findings
    }

    #[test]
    fn an_ftdi_header_that_faces_the_boards_tx_to_the_cables_tx_is_flagged() {
        // The board's TX on pin 4 (the cable's TXD) and its RX on pin 5.
        let found = run(vec![
            mcu(),
            header("FTDI", &[("1", GND), ("3", V5), ("4", TX), ("5", RX)]),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::DebugHeaderPinout);
        assert_eq!(f.refs, ["J1"]);
        assert_eq!(f.nets, ["MCU_TX", "MCU_RX"]);
        for want in [
            "pin 4 faces the adapter's TXD",
            "U1 pin 1 (TXD)",
            "pin 5 faces the adapter's RXD",
            "FT_000054",
        ] {
            assert!(f.message.contains(want), "{want}: {}", f.message);
        }
        // Through a series resistor the same line is still the board's TX.
        let mut cut = mcu();
        cut.pins[0].net = Some(9);
        let mut nets = NETS.to_vec();
        nets.push((9, "TX_OUT"));
        let found = debug_header_lint(
            &board(
                &nets,
                vec![
                    cut,
                    part("R1", "100", &[("1", 9), ("2", TX)]),
                    header("FTDI", &[("1", GND), ("4", TX)]),
                ],
            ),
            &ModelLibrary::builtin(),
        )
        .findings;
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    /// `c` with its pins named by pad number (pad 1 takes `names[0]`).
    fn with_pin_names(mut c: Component, names: &[&str]) -> Component {
        for pin in &mut c.pins {
            let at = pin.number.parse::<usize>().unwrap_or(0).wrapping_sub(1);
            if let Some(name) = names.get(at) {
                pin.function = (*name).into();
            }
        }
        c
    }

    /// A 6-pin header the symbol names `GND CTS VCC TXD RXD DTR` is the FTDI
    /// cable's whatever the part is called: the board's TX on the pin named
    /// `TXD` is flagged, the crossed wiring is silent.
    #[test]
    fn a_six_pin_header_with_the_ftdi_pin_names_is_judged_without_the_ftdi_name() {
        const NAMES: [&str; 6] = ["GND", "~{CTS}", "VCC", "TXD", "RXD", "~{DTR}"];
        let uart = |tx_pad: &str, rx_pad: &str| {
            let mut pads = vec![("1", GND), ("3", V5)];
            pads.push((if tx_pad == "4" { "4" } else { "5" }, TX));
            pads.push((if rx_pad == "4" { "4" } else { "5" }, RX));
            with_pin_names(header("UART", &pads), &NAMES)
        };
        let found = run(vec![mcu(), uart("4", "5")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::DebugHeaderPinout);
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.nets, ["MCU_TX", "MCU_RX"]);
        for want in [
            "J1 (UART) is a USB-serial adapter header",
            "pin 4 faces the adapter's TXD",
            "pin 5 faces the adapter's RXD",
            "put the board's transmit on pin 5 (the adapter's RX)",
            "receive on pin 4 (the adapter's TX)",
        ] {
            assert!(f.message.contains(want), "{want}: {}", f.message);
        }
        assert!(
            run(vec![mcu(), uart("5", "4")]).is_empty(),
            "crossed is right"
        );
        // Without the handshake names the order is not the cable's: not judged.
        let plain = with_pin_names(
            header("UART", &[("1", GND), ("3", V5), ("4", TX), ("5", RX)]),
            &["GND", "A", "VCC", "TXD", "RXD", "B"],
        );
        assert!(run(vec![mcu(), plain]).is_empty());
    }

    /// A 4-pin header is judged from its pin names only when a USB-serial adapter
    /// word names the part, since a bare `TX` on a board's own header usually
    /// means the board's transmit.
    #[test]
    fn a_four_pin_adapter_header_is_judged_from_its_pin_names_and_an_adapter_word() {
        let four = |value: &str, names: &[&str], tx_pad: &str| {
            let rx_pad = if tx_pad == "3" { "4" } else { "3" };
            let c = part(
                "J1",
                value,
                &[("1", GND), ("2", V5), (tx_pad, TX), (rx_pad, RX)],
            );
            with_pin_names(c, names)
        };
        let names = ["GND", "VCC", "TX", "RX"];
        // The board's TX on the pin named TX of a CH340 adapter's connector.
        let found = run(vec![mcu(), four("CH340 UART", &names, "3")]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Medium);
        assert!(
            found[0].message.contains("pin 3 faces the adapter's TXD"),
            "{}",
            found[0].message
        );
        // Crossed, as an adapter's connector needs: silent.
        assert!(run(vec![mcu(), four("CH340 UART", &names, "4")]).is_empty());
        // No adapter word, or pin names that are not the four: not judged.
        assert!(run(vec![mcu(), four("UART", &names, "3")]).is_empty());
        assert!(run(vec![
            mcu(),
            four("USB-Serial", &["GND", "VCC", "TX", "TX"], "3")
        ])
        .is_empty());
        assert!(run(vec![mcu(), four("FTDI", &["", "", "", ""], "3")]).is_empty());
        // A symbol that counts down the columns numbers its pins its own way.
        let mut columns = four("CH340 UART", &names, "3");
        columns.lib_id = "Connector_Generic:Conn_02x02_Top_Bottom".into();
        assert!(run(vec![mcu(), columns]).is_empty());
    }

    #[test]
    fn supply_and_ground_swapped_on_a_named_header_is_high() {
        let found = run(vec![header("FTDI", &[("1", V5), ("3", GND)])]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert!(found[0].message.contains("pin 1 is GND in the standard"));
    }

    /// Pin 3 of the cable is a +5 V output (`FT_000054` Table 4.1), 5 V even on
    /// the 3.3 V cable: a rail named for another voltage on it is overdriven.
    #[test]
    fn the_ftdi_supply_pin_is_a_5v_output_that_must_not_meet_a_lower_rail() {
        let mut nets = NETS.to_vec();
        nets.push((9, "+3V3"));
        nets.push((10, "VDD"));
        let on = |net| {
            debug_header_lint(
                &board(&nets, vec![header("FTDI", &[("1", GND), ("3", net)])]),
                &ModelLibrary::builtin(),
            )
            .findings
        };
        let found = on(9);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert_eq!(found[0].nets, ["+3V3"]);
        for want in [
            "pin 3 is the adapter's +5 V output (VCC)",
            "even on the 3.3 V cable",
            "'+3V3' is a 3.3 V rail",
        ] {
            assert!(
                found[0].message.contains(want),
                "{want}: {}",
                found[0].message
            );
        }
        // A 5 V rail, or a supply whose name states no voltage, is not judged.
        assert!(on(V5).is_empty());
        assert!(on(10).is_empty());
        // Left unconnected is the usual way to use the cable.
        assert!(run(vec![header("FTDI", &[("1", GND), ("4", RX)])]).is_empty());
    }

    #[test]
    fn an_avr_isp_header_with_mosi_and_miso_exchanged_is_flagged() {
        let isp = |m1, m4| Component {
            lib_id: "Connector_Generic:Conn_02x03_Odd_Even".into(),
            ..part(
                "J2",
                "AVR_ISP",
                &[
                    ("1", m1),
                    ("2", V5),
                    ("3", SCK),
                    ("4", m4),
                    ("5", RST),
                    ("6", GND),
                ],
            )
        };
        let found = run(vec![isp(MOSI, MISO)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Medium);
        assert!(
            found[0].message.contains(
                "pin 1 is MISO in the standard but net 'MOSI' carries MOSI (it belongs on pin 4)"
            ),
            "{}",
            found[0].message
        );
        assert!(run(vec![isp(MISO, MOSI)]).is_empty(), "the standard order");
    }

    #[test]
    fn a_cortex_header_with_swdio_and_swclk_exchanged_is_flagged() {
        let mut nets = NETS.to_vec();
        nets.extend([(9, "SWDIO"), (10, "SWCLK")]);
        let swd = |a, b| Component {
            lib_id: "Connector:Conn_ARM_SWD_TagConnect".into(),
            ..part(
                "J3",
                "SWD",
                &[
                    ("1", V5),
                    ("2", a),
                    ("3", GND),
                    ("4", b),
                    ("5", GND),
                    ("6", 0),
                    ("7", 0),
                    ("8", 0),
                    ("9", GND),
                    ("10", RST),
                ],
            )
        };
        let run = |c| debug_header_lint(&board(&nets, vec![c]), &ModelLibrary::builtin()).findings;
        assert_eq!(run(swd(10, 9)).len(), 1);
        assert!(run(swd(9, 10)).is_empty(), "the standard order");
    }

    #[test]
    fn correct_unnamed_unknown_and_unfitted_headers_are_silent() {
        // The standard order: board TX on pin 5, board RX on pin 4.
        assert!(run(vec![
            mcu(),
            header("FTDI", &[("1", GND), ("3", V5), ("4", RX), ("5", TX)])
        ])
        .is_empty());
        // A header that names no standard is the designer's own.
        assert!(run(vec![
            mcu(),
            header("UART", &[("1", GND), ("3", V5), ("4", TX), ("5", RX)])
        ])
        .is_empty());
        // Nothing known about the pin: a bare net on an FTDI pin.
        let mut nets = NETS.to_vec();
        nets.push((9, "NET_A"));
        let bare = header("FTDI", &[("1", 9), ("4", 9), ("5", 9)]);
        assert!(
            debug_header_lint(&board(&nets, vec![bare]), &ModelLibrary::builtin())
                .findings
                .is_empty()
        );
        // Both a transmit and a receive pin on the net: nothing definite.
        let mut both = mcu();
        both.pins[1].net = Some(TX);
        assert!(run(vec![both, header("FTDI", &[("1", GND), ("4", TX)])]).is_empty());
        // A pin count no standard header has, and a header not fitted.
        let four = part("J1", "FTDI", &[("1", GND), ("2", V5), ("3", TX), ("4", RX)]);
        assert!(run(vec![mcu(), four]).is_empty());
        let mut dnp = header("FTDI", &[("1", V5), ("3", GND)]);
        dnp.dnp = true;
        assert!(run(vec![dnp]).is_empty());
        // A column-counting symbol numbers the pins its own way.
        let mut columns = header("FTDI", &[("1", V5), ("3", GND)]);
        columns.lib_id = "Connector_Generic:Conn_02x03_Top_Bottom".into();
        assert!(run(vec![columns]).is_empty());
    }

    /// A bare `ISP` is an LPC serial-boot header as often as an AVR one, and a
    /// bare `JTAG` is any vendor's: neither names a standard header.
    #[test]
    fn a_bare_isp_or_jtag_name_is_not_a_standard_header() {
        let lpc = header(
            "LPC_ISP",
            &[
                ("1", GND),
                ("2", V5),
                ("3", SCK),
                ("4", MISO),
                ("5", MOSI),
                ("6", RST),
            ],
        );
        assert!(run(vec![lpc]).is_empty());
        let mut nets = NETS.to_vec();
        nets.extend([(9, "TCK"), (10, "TMS")]);
        let jtag = part(
            "J3",
            "JTAG",
            &[
                // TCK on the pin the Cortex header gives to TMS and the reverse:
                // wrong for that header, so only the name keeps this silent.
                ("1", V5),
                ("2", 9),
                ("3", GND),
                ("4", 10),
                ("5", GND),
                ("6", 0),
                ("7", 0),
                ("8", 0),
                ("9", GND),
                ("10", RST),
            ],
        );
        assert!(
            debug_header_lint(&board(&nets, vec![jtag]), &ModelLibrary::builtin())
                .findings
                .is_empty()
        );
    }

    /// A pull-up or pull-down on a debug line is not a series resistor: the rail
    /// it reaches must not be read as what the header pin carries, and the
    /// JTAG names `TCK`/`TMS` are the SWD clock and data pins.
    #[test]
    fn a_pull_up_on_a_debug_line_is_not_what_the_pin_carries() {
        let mut nets = NETS.to_vec();
        nets.extend([(9, "TMS"), (10, "TCK")]);
        let swd = Component {
            lib_id: "Connector:Conn_ARM_SWD_TagConnect".into(),
            ..part(
                "J3",
                "SWD",
                &[
                    ("1", V5),
                    ("2", 9),
                    ("3", GND),
                    ("4", 10),
                    ("5", GND),
                    ("6", 0),
                    ("7", 0),
                    ("8", 0),
                    ("9", GND),
                    ("10", RST),
                ],
            )
        };
        let pulls = vec![
            part("R1", "10k", &[("1", 9), ("2", V5)]),
            part("R2", "10k", &[("1", 10), ("2", GND)]),
        ];
        let run = |extra: Vec<Component>| {
            let mut parts = vec![swd.clone()];
            parts.extend(extra);
            debug_header_lint(&board(&nets, parts), &ModelLibrary::builtin()).findings
        };
        assert!(run(pulls).is_empty());
        // Swapped for real: the clock where the data belongs is still caught.
        let mut crossed = swd.clone();
        crossed.pins[1].net = Some(10);
        crossed.pins[3].net = Some(9);
        let found =
            debug_header_lint(&board(&nets, vec![crossed]), &ModelLibrary::builtin()).findings;
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    // Nets for the programming-function fixtures.
    const APP_MOSI: i64 = 9;
    const APP_MISO: i64 = 10;
    const APP_SCK: i64 = 11;

    /// An ATtiny85-style chip, symbol only (no card): pin names carry the ISP
    /// function of each pad (`PB0` is ISP MOSI and the USI `DI`, `PB1` is ISP
    /// MISO and the USI `DO`). `data_in`/`data_out` are the nets on those pads.
    fn tiny(data_in: i64, data_out: i64) -> Component {
        let mut u = part(
            "U2",
            "FancyTiny",
            &[
                ("1", RST),
                ("2", 0),
                ("3", 0),
                ("4", GND),
                ("5", data_in),
                ("6", data_out),
                ("7", APP_SCK),
                ("8", V5),
            ],
        );
        for (pin, name) in u.pins.iter_mut().zip([
            "PB5(~{RESET}/ADC0)",
            "PB3",
            "PB4",
            "GND",
            "PB0(MOSI/DI/SDA)",
            "PB1(MISO/DO/OC0B)",
            "PB2(SCK/USCK/SCL)",
            "VCC",
        ]) {
            pin.function = name.into();
        }
        u
    }

    /// A 6-pin AVR ISP header with the given nets on MISO (1), MOSI (4).
    fn isp6(miso: i64, mosi: i64) -> Component {
        Component {
            lib_id: "Connector_Generic:Conn_02x03_Odd_Even".into(),
            ..part(
                "J2",
                "AVR_ISP",
                &[
                    ("1", miso),
                    ("2", V5),
                    ("3", APP_SCK),
                    ("4", mosi),
                    ("5", RST),
                    ("6", GND),
                ],
            )
        }
    }

    fn lint_isp(parts: Vec<Component>) -> Vec<LintFinding> {
        let mut nets = NETS.to_vec();
        nets.extend([(APP_MOSI, "MOSI"), (APP_MISO, "MISO"), (APP_SCK, "SCK")]);
        debug_header_lint(&board(&nets, parts), &ModelLibrary::builtin()).findings
    }

    /// The application's SPI is wired through the chip's USI: its data-out
    /// (`DO`, the ISP MISO pad) is the application's MOSI and is named so. The
    /// ISP header is wired right, by the pads, and the net names do not decide.
    #[test]
    fn an_isp_header_is_judged_by_the_pad_it_reaches_not_by_the_nets_name() {
        // ISP MISO (header pin 1) on the pad whose function is MISO, whose net
        // is called MOSI by the application; ISP MOSI (pin 4) on the pad whose
        // function is MOSI, net called MISO.
        let right = lint_isp(vec![tiny(APP_MISO, APP_MOSI), isp6(APP_MOSI, APP_MISO)]);
        assert!(right.is_empty(), "{right:#?}");
        // With no pad evidence at all the names are the fallback, and they
        // disagree with the header.
        let mut bare = tiny(APP_MISO, APP_MOSI);
        for pin in &mut bare.pins {
            pin.function.clear();
        }
        let named = lint_isp(vec![bare, isp6(APP_MOSI, APP_MISO)]);
        assert_eq!(named.len(), 1, "{named:#?}");
    }

    /// Wired wrong for real: the header's MISO pin on the pad whose function is
    /// MOSI. A net name that happens to agree with the header does not excuse it.
    #[test]
    fn an_isp_pin_on_the_wrong_pad_is_flagged_whatever_the_net_is_called() {
        // Header pin 1 (MISO) lands on the MOSI pad (net 10, named MISO).
        let found = lint_isp(vec![tiny(APP_MISO, APP_MOSI), isp6(APP_MISO, APP_MOSI)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.severity, Severity::Medium);
        for want in [
            "pin 1 is MISO in the standard but net 'MISO' carries MOSI",
            "read from the pad U2 pin 5 (PB0(MOSI/DI/SDA))",
            "pin 4 is MOSI in the standard but net 'MOSI' carries MISO",
        ] {
            assert!(f.message.contains(want), "{want}: {}", f.message);
        }
    }

    /// The same by the pad's card: an `ATmega328P`'s pads 17/18/19 are ISP
    /// MOSI/MISO/SCK (DS40002061B Table 14-3).
    #[test]
    fn an_isp_header_is_judged_by_the_mcu_card_when_one_binds() {
        let mcu = || {
            let mut u = part(
                "U1",
                "ATmega328P",
                &[
                    ("1", RST),
                    ("7", V5),
                    ("8", GND),
                    ("17", APP_MOSI),
                    ("18", APP_MISO),
                    ("19", APP_SCK),
                ],
            );
            u.footprint = "Package_DIP:DIP-28_W7.62mm".into();
            u
        };
        // The net on the MOSI pad is named MISO (and the reverse), as an
        // application that views the SPI from the peripheral would name them.
        let mut nets = NETS.to_vec();
        nets.extend([(APP_MOSI, "MISO"), (APP_MISO, "MOSI"), (APP_SCK, "SCK")]);
        let run = |header: Component| {
            debug_header_lint(&board(&nets, vec![mcu(), header]), &ModelLibrary::builtin()).findings
        };
        // Right by the card (pin 1 = MISO on pad 18, pin 4 = MOSI on pad 17).
        let right = run(isp6(APP_MISO, APP_MOSI));
        assert!(right.is_empty(), "{right:#?}");
        // Swapped by the card, with names that agree with the header.
        let swapped = run(isp6(APP_MOSI, APP_MISO));
        assert_eq!(swapped.len(), 1, "{swapped:#?}");
        assert!(
            swapped[0].message.contains("read from the pad U1 pin"),
            "{}",
            swapped[0].message
        );
    }
}
