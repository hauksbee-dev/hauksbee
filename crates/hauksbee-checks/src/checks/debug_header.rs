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
//! Judged only on definite evidence:
//!
//! - **The header is named.** A connector with exactly the pin count of a
//!   standard header, whose value, symbol or footprint name carries a whole word
//!   that names that one header (`FTDI`, `ISP`, `SWD`, `JTAG`, `Cortex`). A
//!   header with no such word is the designer's own, and `ICSP` is left out
//!   because PIC and AVR use different orders under that name.
//! - **The pin's use is known.** What a pin carries is read from the net on it
//!   (and the nets one series resistor away): its supply or ground name, a
//!   `MOSI`/`MISO`/`SCK`/`SWDIO`/`SWCLK`/reset name, or the card pad of an MCU on
//!   it. A name and a card that disagree, or no reading at all, leave the pin
//!   unjudged. The serial pins are judged from the UART pins of the parts on the
//!   net: a pin facing the adapter's `TXD` output must not carry a board
//!   transmitter, and the one facing its `RXD` input must not carry a board
//!   receiver.
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
use hauksbee_models::netclass::is_positive_supply;
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
        (has("swo"), HeaderSignal::Swo),
        (has("tdi"), HeaderSignal::Tdi),
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

/// What the nets at `net` say a header pin carries, `None` when nothing is said
/// or the readings disagree.
fn pin_signal(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    header: &Component,
    net: i64,
) -> Option<HeaderSignal> {
    let mut said = Vec::new();
    for n in series_nets(board, net) {
        if let Some(name) = board.net(n).map(|x| &x.name) {
            if let Some(s) = from_name(name) {
                said.push(s);
            }
        }
        for (part, pin) in board.net_members(n) {
            if std::ptr::eq(part, header) {
                continue;
            }
            if let Some(s) = mcu_role(board, lib, part, pin).and_then(|r| from_role(&r)) {
                said.push(s);
            }
        }
    }
    let (first, rest) = said.split_first()?;
    rest.iter().all(|s| s == first).then_some(*first)
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
                    let (tx, rx) = board_uart(board, lib, comp, net);
                    let transmit_side = expected == HeaderSignal::AdapterTransmit;
                    let (wrong, kind, other) = if transmit_side {
                        (&tx, "transmit", &rx)
                    } else {
                        (&rx, "receive", &tx)
                    };
                    (!wrong.is_empty() && other.is_empty()).then(|| {
                        let adapter = if transmit_side { "TXD" } else { "RXD" };
                        let needed = if transmit_side { "RX" } else { "TX" };
                        format!(
                            "pin {} faces the adapter's {adapter}, but net '{}' carries the \
                             board's {kind} pin {}: transmit would meet transmit and receive \
                             receive; the board's {needed} belongs here",
                            pin.number,
                            name(net),
                            wrong.join(", ")
                        )
                    })
                }
                _ => pin_signal(board, lib, comp, net)
                    .filter(|got| *got != expected)
                    .map(|got| {
                        power |= matches!(expected, HeaderSignal::Ground | HeaderSignal::Supply)
                            || matches!(got, HeaderSignal::Ground | HeaderSignal::Supply);
                        let home = header.pins_of(got);
                        let home = if home.is_empty() {
                            String::new()
                        } else {
                            format!(" (it belongs on pin {})", home.join(", "))
                        };
                        format!(
                            "pin {} is {} in the standard but net '{}' carries {}{home}",
                            pin.number,
                            expected.label(),
                            name(net),
                            got.label(),
                        )
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

    #[test]
    fn supply_and_ground_swapped_on_a_named_header_is_high() {
        let found = run(vec![header("FTDI", &[("1", V5), ("3", GND)])]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert!(found[0].message.contains("pin 1 is GND in the standard"));
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
}
