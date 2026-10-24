//! A signal net wired against its pins' directions.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A signal wire carries a level from a pin that drives it to the pins that
//! listen. Two kinds of net cannot do that, whatever the signal is called:
//!
//! - **Nothing listens.** Every pin on it is an output (or a tri-state or
//!   open-drain pin beside at least one push-pull output) and no pin reads it.
//!   Typical of a crossed pair: a UART TX wired to the other side's TX, or a
//!   control bus whose outputs were wired to the other part's outputs.
//! - **Nothing drives.** Every pin on it is an input and nothing can drive it:
//!   no output, no bidirectional pin, no tri-state or open-drain pin, no
//!   connector, no pull resistor. The other half of the same crossed pair
//!   (RX to RX, input to input).
//!
//! The directions come from two places that must agree. A symbol pin's
//! electrical type (`output`, `input`, `bidirectional`, `tri_state`,
//! `open_collector`/`open_emitter`) gives the first reading. A part whose
//! model card states it refines it: a digital card's `logic.inputs` and
//! `logic.outputs` (an output under a `tristate` group may be released), a
//! microcontroller's port pad (bidirectional whatever the symbol says). When
//! the symbol and the card give different directions the pin is not judged. A
//! bidirectional pin whose own name is a dedicated UART `TX`/`RX` is read as
//! that output or input. Any other pin, passive, power, untyped or unnamed, is
//! not judged, and one such pin (a connector, a resistor, a switch, a
//! transistor, a diode, a part the extraction refused) on the net stops the net
//! being judged: only a net the check can read to the end is called wrong.
//! Capacitors, test points and parts not fitted are not pins on the net.
//!
//! A single small resistor between two signal nets, or one element of a resistor
//! network, is a wire for this question (see the `series` module): a crossed pair behind a
//! 22 ohm or 1 kilohm series resistor is judged as the line it is. A pull, a
//! divider, a filter or two resistors in series are not wires and stop the line as
//! any resistor pin does. A microcontroller port pad the card binds to a UART or
//! Ethernet transmit function is read as that output (a receive function as that
//! input) when the part facing it has a card that lists the facing pin as the
//! opposite direction: the card pair states the direction the firmware cannot
//! change.
//!
//! A net needs pins of two different parts. The nets the other checks already
//! report (two push-pull outputs, a CMOS input with nothing on its net, two
//! UART transmit pins) are theirs, and a finding on such a net is not repeated.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::pin_mux::{uart_pad, UartPad};

pub(super) mod series;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_connector_like, is_ground, is_test_point_or_hole, is_unconnected_net,
    pin_is_no_connect, uart_pin_role, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Pin, Severity,
};
use hauksbee_ir::pin_functions::uart_token_direction;
use hauksbee_models::netclass::may_be_supply;
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

/// What a pin does to the net it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Dir {
    /// A push-pull output: drives the net.
    Out,
    /// A tri-state or open-drain pin: may drive the net low (or either way)
    /// and may release it.
    Open,
    /// An input: reads the net.
    In,
    /// A bidirectional pin: may do either, as the firmware or the mode says.
    Bidi,
}

/// The direction a schematic pin's electrical type states, `None` for a pin
/// that is passive, power, untyped or of a type this check does not read.
fn symbol_dir(kind: &str) -> Option<Dir> {
    match kind
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' '], "_")
        .as_str()
    {
        "output" | "out" => Some(Dir::Out),
        "input" | "in" => Some(Dir::In),
        "bidirectional" | "io" => Some(Dir::Bidi),
        "tri_state" | "tristate" | "hiz" | "open_collector" | "open_emitter" | "open_drain"
        | "oc" => Some(Dir::Open),
        _ => None,
    }
}

/// A symbol whose pins carry almost all one electrical type states no
/// directions: a library that types every pin of a many-pin part `input` (or
/// `bidirectional`, or `passive`) never assigned them, and a peripheral pin of a
/// microcontroller (`TXD`, `PWM`, `SCK`) typed that way is not an input. "Almost
/// all" is every pin of a part with six or more, or four in five of the pins of
/// a part with sixteen or more (the few left are its supply and port pins).
fn untyped_symbol(comp: &Component) -> bool {
    const FEWEST_PINS: usize = 6;
    const FEWEST_PINS_DOMINANT: usize = 16;
    let total = comp.pins.len();
    let mut counts = std::collections::BTreeMap::new();
    for pin in &comp.pins {
        *counts
            .entry(pin.kind.trim().to_ascii_lowercase())
            .or_insert(0_usize) += 1;
    }
    let most = counts.values().copied().max().unwrap_or(0);
    (total >= FEWEST_PINS && most == total)
        || (total >= FEWEST_PINS_DOMINANT && most * 5 >= total * 4)
}

/// A card role that names a port or general-purpose I/O pad (`pa9`, `gpio4`,
/// `io12`): the firmware sets its direction.
fn is_port_role(role: &str) -> bool {
    let b = role.as_bytes();
    role.starts_with("gpio")
        || (role.starts_with("io") && b.get(2).is_some_and(u8::is_ascii_digit))
        || (b.len() >= 3 && b[0] == b'p' && b[1].is_ascii_lowercase() && b[2].is_ascii_digit())
}

/// The direction the part's card states for `pad`, when it states one.
fn card_dir(model: &ModelEntry, pad: &str) -> Option<Dir> {
    let role = model.pins.get(pad)?.to_ascii_lowercase();
    match model.kind {
        ComponentKind::Digital | ComponentKind::ShiftRegister if !model.logic.is_empty() => {
            let logic = &model.logic;
            if logic.sense.contains_key(&role) {
                return Some(Dir::Bidi);
            }
            if logic.outputs.contains(&role) {
                let tri = logic.tristate.keys().any(|g| {
                    logic
                        .expand_tristate_group(g)
                        .is_ok_and(|n| n.contains(&role))
                });
                // A group that cannot be read is a part that may release.
                let unreadable = logic
                    .tristate
                    .keys()
                    .any(|g| logic.expand_tristate_group(g).is_err());
                return Some(if tri || unreadable {
                    Dir::Open
                } else {
                    Dir::Out
                });
            }
            logic.inputs.contains(&role).then_some(Dir::In)
        }
        ComponentKind::Mcu if is_port_role(&role) => Some(Dir::Bidi),
        _ => None,
    }
}

/// A bus transceiver (a part with a `CANH`/`CANL` bus pin): its `TXD` is an
/// input and its `RXD` an output, whatever a name says.
fn is_bus_transceiver(c: &Component) -> bool {
    c.pins.iter().any(|p| {
        p.function
            .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
            .any(|t| matches!(t.to_ascii_uppercase().as_str(), "CANH" | "CANL"))
    })
}

/// One pin of a net, as the check reads it.
pub(super) enum Member {
    /// Takes no part: a capacitor, a test point, a part not fitted, a pin
    /// marked no-connect.
    Ignored,
    /// A pin with a known direction.
    Pin(Dir),
    /// Anything the check cannot read: the net is not judged.
    Unknown,
}

pub(super) fn classify(comp: &Component, pin: &Pin, model: Option<&ModelEntry>) -> Member {
    match AssemblyState::of(comp) {
        AssemblyState::DnpAbsent(_) => return Member::Ignored,
        AssemblyState::IdentityUnknown(_) => return Member::Unknown,
        AssemblyState::Present(_) => {}
    }
    if pin_is_no_connect(pin) || is_test_point_or_hole(comp) || is_capacitor(comp) {
        return Member::Ignored;
    }
    if is_connector_like(comp) {
        return Member::Unknown;
    }
    let symbol = symbol_dir(&pin.kind).filter(|_| !untyped_symbol(comp));
    let card = model.and_then(|m| card_dir(m, &pin.number));
    let dir = match (symbol, card) {
        (Some(s), Some(c)) if s == c => Some(s),
        // The symbol only says "may do either": the card's statement stands.
        (Some(Dir::Bidi) | None, Some(c)) => Some(c),
        (Some(s), None) => Some(s),
        (Some(_), Some(_)) | (None, None) => None,
    };
    let dir = match (dir, uart_pin_role(pin)) {
        // A bidirectional pad whose whole name is a UART line is that line.
        (Some(Dir::Bidi), Some((transmit, true))) if !is_bus_transceiver(comp) => {
            Some(if transmit { Dir::Out } else { Dir::In })
        }
        (d, _) => d,
    };
    dir.map_or(Member::Unknown, Member::Pin)
}

/// The pins of a net as words, `U1.2 (TXD) [output]`. A pad read from its card's
/// binding (see [`alt_function_dir`]) says so.
fn describe(pins: &[(&Component, &Pin, Dir)], bound: &HashSet<(String, String)>) -> Vec<String> {
    pins.iter()
        .map(|(c, p, d)| {
            let name = if p.function.trim().is_empty() {
                String::new()
            } else {
                format!(" ({})", p.function.trim())
            };
            let kind = match d {
                Dir::Out => "output",
                Dir::Open => "open-drain or tri-state",
                Dir::In => "input",
                Dir::Bidi => "bidirectional",
            };
            let why = if bound.contains(&(c.reference.clone(), p.number.clone())) {
                ", the card binds this pad to a peripheral function"
            } else {
                ""
            };
            format!("{}.{}{name} [{kind}{why}]", c.reference, p.number)
        })
        .collect()
}

/// A peripheral function a pad is wired to on the card, as a direction at the
/// pad: `Some(Dir::Out)` for a transmit function, `Some(Dir::In)` for a receive
/// one.
///
/// A port pad of a microcontroller is bidirectional to the netlist: the firmware
/// picks. Its card can bind it to one peripheral function all the same: a UART
/// transmit or receive pad (the card's `uart_tx` or `uart_rx` table, or a pad role
/// that names the line), an Ethernet RMII line (a role such as `pb11_eth_tx_en`),
/// or a symbol pin name that states exactly one such function (`PD1/TXD`,
/// `PB11/ETH_TX_EN`; a pad name that lists several alternates states none). Where
/// the sources disagree, or the card gives the pad both directions, the pad stays
/// bidirectional.
fn alt_function_dir(pin: &Pin, model: &ModelEntry) -> Option<Dir> {
    if model.kind != ComponentKind::Mcu {
        return None;
    }
    let role = model.pins.get(&pin.number)?.to_ascii_lowercase();
    if !is_port_role(&role) {
        return None;
    }
    let mut found: Vec<bool> = Vec::new();
    match uart_pad(model, &pin.number, true) {
        UartPad::Tx => found.push(true),
        UartPad::Rx => found.push(false),
        UartPad::Both => return None,
        UartPad::Neither => {}
    }
    found.extend(ethernet_direction(&role));
    // A symbol pin name that states one function, once the port name is set aside.
    let named: Vec<&str> = pin
        .function
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|t| !t.is_empty() && !is_port_role(&t.to_ascii_lowercase()))
        .collect();
    if let [one] = named.as_slice() {
        found.extend(uart_token_direction(one).or_else(|| ethernet_direction(one)));
    }
    let first = *found.first()?;
    found
        .iter()
        .all(|d| *d == first)
        .then_some(if first { Dir::Out } else { Dir::In })
}

/// The direction an Ethernet MII/RMII line has at the controller: `Some(true)`
/// for a transmit line (`TX_EN`, `TXD0`), `Some(false)` for a receive one
/// (`RXD1`, `CRS_DV`, `RX_ER`). The name must state the interface (`eth`,
/// `rmii`, `mii`, `emac`, `rgmii`), so a bare `TXD` stays a UART's.
fn ethernet_direction(name: &str) -> Option<bool> {
    let lower = name.to_ascii_lowercase();
    let words: Vec<&str> = lower.split('_').filter(|w| !w.is_empty()).collect();
    if !words
        .iter()
        .any(|w| matches!(*w, "eth" | "rmii" | "mii" | "emac" | "rgmii"))
    {
        return None;
    }
    let joined = words.join("");
    // `stem` followed by a digit: `txd0`, `rxd1`.
    let numbered = |stem: &str| {
        joined
            .split(stem)
            .skip(1)
            .any(|r| r.chars().next().is_some_and(|c| c.is_ascii_digit()))
    };
    let transmit = ["txen", "txctl"].iter().any(|k| joined.contains(k)) || numbered("txd");
    let receive = ["crsdv", "rxer", "rxdv"].iter().any(|k| joined.contains(k)) || numbered("rxd");
    (transmit != receive).then_some(transmit)
}

/// Why a net was called wrong.
enum Fault {
    /// Outputs only: nothing reads the net.
    NoListener,
    /// Inputs only: nothing drives the net.
    NoDriver,
}

/// The two nets-of-fault lists of a pair of parts: no-listener nets, no-driver nets.
type PairFaults = BTreeMap<(String, String), (Vec<String>, Vec<String>)>;

/// One line judged wrong: what is wrong, how the message opens, its nets and
/// the parts and pins on it.
struct Faulty {
    fault: Fault,
    /// The phrase that opens the message: `net 'A'`, or the two nets a series
    /// resistor joins.
    lead: String,
    nets: Vec<String>,
    refs: Vec<String>,
    listed: Vec<String>,
}

/// The finding for one faulty line; a pair of parts faulted both ways is crossed.
fn finding(faulty: Faulty, pairs: &PairFaults) -> LintFinding {
    let Faulty {
        fault,
        lead,
        nets,
        refs,
        listed,
    } = faulty;
    let pair = match refs.as_slice() {
        [a, b] => pairs.get(&(a.clone(), b.clone())),
        _ => None,
    };
    let crossed = pair.filter(|(outs, ins)| !outs.is_empty() && !ins.is_empty());
    let crossed_note = crossed.map_or_else(String::new, |(outs, ins)| {
        format!(
            " {} and {} are wired this way on both {} and {}: the two buses are crossed, \
             and swapping the pair of connections at one end fixes both",
            refs[0],
            refs[1],
            hauksbee_ir::vocabulary::names_summary(outs),
            hauksbee_ir::vocabulary::names_summary(ins),
        )
    });
    let listed = hauksbee_ir::vocabulary::names_summary(&listed);
    let message = match fault {
        Fault::NoListener => format!(
            "{lead} joins only outputs and nothing that reads it: {listed}. The \
             parts drive the same wire and neither hears the other, so the signal never \
             arrives (an output must go to the other side's input, TX to RX); wire each \
             output to the matching input pin.{crossed_note}"
        ),
        Fault::NoDriver => format!(
            "{lead} joins only inputs and nothing that drives it: {listed}. No \
             output, bidirectional or open-drain pin, connector or pull resistor sets its \
             level, so the inputs float and read nothing real; wire each input to the \
             output that is meant to drive it (RX to the other side's TX), or tie the \
             line to a defined level.{crossed_note}"
        ),
    };
    LintFinding {
        check: LintCheck::SignalDirection,
        severity: if crossed.is_some() {
            Severity::High
        } else {
            Severity::Medium
        },
        message,
        refs,
        nets,
    }
}

/// Flag every signal net that joins pins of two or more parts and cannot carry
/// a signal.
///
/// It has drivers and no reader, or readers and no driver. A net joined to another
/// through one small series resistor is judged as one line with it.
#[must_use]
pub fn signal_direction_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let models: HashMap<&str, ModelEntry> = super::resolved_components(board, lib)
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    let links = series::SeriesLinks::of(board);
    let mut by_line: BTreeMap<i64, Vec<(&Component, &Pin)>> = BTreeMap::new();
    for c in &board.components {
        for p in &c.pins {
            let Some(id) = p.net.filter(|id| *id != 0) else {
                continue;
            };
            if !links.is_link_pin(c, p) {
                // The resistor is the wire, not a pin on the line.
                by_line.entry(links.line(id)).or_default().push((c, p));
            }
        }
    }
    // Per pair of parts: the nets of each fault, to tell a crossed pair.
    let mut pairs = PairFaults::new();
    let mut found = Vec::new();
    for (line, members) in &by_line {
        let names = links.names(board, *line);
        if names.is_empty()
            || names
                .iter()
                .any(|n| is_unconnected_net(n) || is_ground(n) || may_be_supply(n))
        {
            continue;
        }
        let mut pins: Vec<(&Component, &Pin, Dir)> = Vec::new();
        let mut readable = true;
        for (c, p) in members {
            match classify(c, p, models.get(c.reference.as_str())) {
                Member::Ignored => {}
                Member::Pin(d) => pins.push((c, p, d)),
                Member::Unknown => {
                    readable = false;
                    break;
                }
            }
        }
        if !readable {
            continue;
        }
        let bound = bind_alternate_functions(&mut pins, &models);
        let parts: BTreeSet<&str> = pins.iter().map(|(c, _, _)| c.reference.as_str()).collect();
        if parts.len() < 2 {
            continue;
        }
        let has = |d: Dir| pins.iter().any(|(_, _, x)| *x == d);
        let fault = if has(Dir::Out) && !has(Dir::In) && !has(Dir::Bidi) {
            Fault::NoListener
        } else if has(Dir::In) && !has(Dir::Out) && !has(Dir::Open) && !has(Dir::Bidi) {
            Fault::NoDriver
        } else {
            continue;
        };
        let name = names[0].clone();
        let refs: Vec<String> = parts.iter().map(|r| (*r).to_string()).collect();
        if let [a, b] = refs.as_slice() {
            let entry = pairs.entry((a.clone(), b.clone())).or_default();
            match fault {
                Fault::NoListener => entry.0.push(name.clone()),
                Fault::NoDriver => entry.1.push(name.clone()),
            }
        }
        let lead = match (links.link(*line), names.as_slice()) {
            (Some(link), [first, second]) => format!(
                "net '{first}' (wired through the series resistor {} to net '{second}')",
                link.describe()
            ),
            _ => format!("net '{name}'"),
        };
        found.push(Faulty {
            fault,
            lead,
            nets: names,
            refs,
            listed: describe(&pins, &bound),
        });
    }
    for f in found {
        report.findings.push(finding(f, &pairs));
    }
    report
}

/// Read each microcontroller port pad its card binds to a peripheral function as
/// that function's direction, where a part facing it states the same: a transmit
/// pad is an output when a facing pin is listed as an output by its own card, a
/// receive pad an input when one is listed as an input. Returns the pads changed,
/// as (reference, pad).
fn bind_alternate_functions(
    pins: &mut [(&Component, &Pin, Dir)],
    models: &HashMap<&str, ModelEntry>,
) -> HashSet<(String, String)> {
    let mut bound = HashSet::new();
    let wanted: Vec<Option<Dir>> = pins
        .iter()
        .map(|(c, p, d)| {
            models
                .get(c.reference.as_str())
                .filter(|_| *d == Dir::Bidi)
                .and_then(|m| alt_function_dir(p, m))
        })
        .collect();
    for (i, want) in wanted.iter().enumerate() {
        let Some(want) = *want else { continue };
        // A UART table lists what a pad can do, not what the firmware picked: a
        // pad read as a UART line faces a UART line, never a plain logic input
        // that a GPIO or timer output on the same pad would drive.
        let pad = pins[i].1;
        let ethernet = models
            .get(pins[i].0.reference.as_str())
            .and_then(|m| m.pins.get(&pad.number))
            .is_some_and(|r| ethernet_direction(&r.to_ascii_lowercase()).is_some())
            || pad
                .function
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .any(|t| ethernet_direction(t).is_some());
        let facing = pins.iter().any(|(c, p, _)| {
            let model = models.get(c.reference.as_str());
            c.reference != pins[i].0.reference
                && model.and_then(|m| card_dir(m, &p.number)) == Some(want)
                && (ethernet
                    || uart_pin_role(p).is_some()
                    || model
                        .is_some_and(|m| !matches!(uart_pad(m, &p.number, true), UartPad::Neither)))
        });
        if facing {
            pins[i].2 = want;
            bound.insert((pins[i].0.reference.clone(), pins[i].1.number.clone()));
        }
    }
    bound
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    fn ic(reference: &str, pads: &[(&str, i64, &str, &str)]) -> Component {
        let mut c = part(reference, "SomeIC", &[]);
        c.pins = pads
            .iter()
            .map(|(n, net, function, kind)| Pin {
                number: (*n).into(),
                net: Some(*net),
                function: (*function).into(),
                kind: (*kind).into(),
                ..Default::default()
            })
            .collect();
        c
    }

    fn run(nets: &[(i64, &str)], parts: Vec<Component>) -> Vec<LintFinding> {
        signal_direction_lint(&board(nets, parts), &ModelLibrary::builtin()).findings
    }

    /// A control bus wired output-to-output and input-to-input: both nets are
    /// wrong, and the message says the two are one crossed pair.
    #[test]
    fn a_crossed_pair_of_buses_is_flagged_on_both_nets() {
        let found = run(
            &[(1, "CTRL_A"), (2, "CTRL_B")],
            vec![
                ic("U1", &[("1", 1, "REQ", "output"), ("2", 2, "ACK", "input")]),
                ic("U2", &[("1", 1, "REQ", "output"), ("2", 2, "ACK", "input")]),
            ],
        );
        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(found.iter().all(|f| f.severity == Severity::High));
        let out = found.iter().find(|f| f.nets == ["CTRL_A"]).unwrap();
        assert!(out.message.contains("only outputs"), "{}", out.message);
        assert!(out.message.contains("crossed"), "{}", out.message);
        let inp = found.iter().find(|f| f.nets == ["CTRL_B"]).unwrap();
        assert!(inp.message.contains("only inputs"), "{}", inp.message);
        assert_eq!(inp.refs, ["U1", "U2"]);
    }

    #[test]
    fn a_uart_rx_to_rx_with_no_driver_is_flagged() {
        let found = run(
            &[(1, "UART_RX")],
            vec![
                ic("U1", &[("1", 1, "RXD", "bidirectional")]),
                ic("U2", &[("1", 1, "RXD", "bidirectional")]),
            ],
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].message.contains("only inputs"));
    }

    #[test]
    fn an_output_with_open_drain_and_no_reader_is_flagged() {
        let found = run(
            &[(1, "FLAG")],
            vec![
                ic("U1", &[("1", 1, "OUT", "output")]),
                ic("U2", &[("1", 1, "FLG", "open_collector")]),
            ],
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::Medium);
    }

    #[test]
    fn a_wire_with_a_driver_and_a_reader_is_silent() {
        // Output to input, and TX to RX through UART names.
        assert!(run(
            &[(1, "A"), (2, "B")],
            vec![
                ic(
                    "U1",
                    &[("1", 1, "OUT", "output"), ("2", 2, "TXD", "bidirectional")]
                ),
                ic(
                    "U2",
                    &[("1", 1, "IN", "input"), ("2", 2, "RXD", "bidirectional")]
                ),
            ],
        )
        .is_empty());
        // Bidirectional pins either end, and a fan-out of readers with a driver.
        assert!(run(
            &[(1, "SDA")],
            vec![
                ic("U1", &[("1", 1, "SDA", "bidirectional")]),
                ic("U2", &[("1", 1, "SDA", "bidirectional")]),
            ],
        )
        .is_empty());
    }

    /// A library that types every pin of a many-pin part `input` assigned no
    /// directions: a peripheral output on it is not an input, so the net is
    /// not judged. The same two parts with real types are flagged.
    #[test]
    fn a_symbol_with_one_type_on_every_pin_states_no_direction() {
        let mcu = |kind: &str| {
            let mut pads: Vec<(String, i64, String, String)> = (2..=8)
                .map(|i| (i.to_string(), 0, format!("PA{i}"), kind.to_owned()))
                .collect();
            pads.insert(0, ("1".into(), 1, "PWML0".into(), kind.to_owned()));
            let pads: Vec<(&str, i64, &str, &str)> = pads
                .iter()
                .map(|(n, net, f, k)| (n.as_str(), *net, f.as_str(), k.as_str()))
                .collect();
            ic("U1", &pads)
        };
        let buffer = || ic("U2", &[("1", 1, "2A", "input")]);
        assert!(run(&[(1, "HEATER0")], vec![mcu("input"), buffer()]).is_empty());
        // Four in five of twenty pins one type: the few others are supply and port
        // pins, and the part is still untyped.
        let mut big: Vec<(String, i64, String, String)> = (2..=19)
            .map(|i| (i.to_string(), 0, format!("PA{i}"), "input".to_owned()))
            .collect();
        big.insert(0, ("1".into(), 1, "PWML0".into(), "input".into()));
        big.push(("20".into(), 0, "VDD".into(), "power_in".into()));
        let big: Vec<(&str, i64, &str, &str)> = big
            .iter()
            .map(|(n, net, f, k)| (n.as_str(), *net, f.as_str(), k.as_str()))
            .collect();
        assert!(run(&[(1, "HEATER0")], vec![ic("U1", &big), buffer()]).is_empty());
        // The same net with a pin that really is typed an input is a fault.
        let one = ic("U1", &[("1", 1, "PWML0", "input")]);
        assert_eq!(run(&[(1, "HEATER0")], vec![one, buffer()]).len(), 1);
    }

    #[test]
    fn a_net_the_check_cannot_read_to_the_end_is_silent() {
        let nets = [(1, "N"), (2, "VCC")];
        // Inputs only, with a connector that may drive them.
        let conn = Component {
            lib_id: "Connector:Conn_01x02".into(),
            ..ic("J1", &[("1", 1, "1", "passive")])
        };
        assert!(run(
            &nets,
            vec![
                ic("U1", &[("1", 1, "IN", "input")]),
                ic("U2", &[("1", 1, "IN", "input")]),
                conn,
            ],
        )
        .is_empty());
        // Inputs only, with a pull-up resistor: a deliberate default.
        assert!(run(
            &nets,
            vec![
                ic("U1", &[("1", 1, "IN", "input")]),
                ic("U2", &[("1", 1, "IN", "input")]),
                part("R1", "10k", &[("1", 1), ("2", 2)]),
            ],
        )
        .is_empty());
        // Outputs only, one pin of unknown type.
        assert!(run(
            &nets,
            vec![
                ic("U1", &[("1", 1, "OUT", "output")]),
                ic("U2", &[("1", 1, "X", "passive")]),
            ],
        )
        .is_empty());
        // One part alone, a supply name, a part not fitted, a bus transceiver.
        assert!(run(
            &nets,
            vec![ic("U1", &[("1", 1, "A", "input"), ("2", 1, "B", "input")])]
        )
        .is_empty());
        assert!(run(
            &nets,
            vec![
                ic("U1", &[("1", 2, "IN", "input")]),
                ic("U2", &[("1", 2, "IN", "input")]),
            ],
        )
        .is_empty());
        let mut dnp = ic("U2", &[("1", 1, "IN", "input")]);
        dnp.dnp = true;
        assert!(run(&nets, vec![ic("U1", &[("1", 1, "IN", "input")]), dnp]).is_empty());
        assert!(run(
            &nets,
            vec![
                ic("U1", &[("1", 1, "TXD", "bidirectional")]),
                ic(
                    "U2",
                    &[
                        ("1", 1, "RXD", "bidirectional"),
                        ("2", 1, "CANH", "passive")
                    ]
                ),
            ],
        )
        .is_empty());
    }

    // ---- A series resistor and a card-bound pad ----------------------------------

    /// The built-in library plus the cards in `text`.
    fn library(tag: &str, text: &str) -> ModelLibrary {
        let dir = hauksbee_testkit::fresh_scratch_dir("signal_direction", tag);
        std::fs::write(dir.join("models.toml"), text).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "fixture cards load: {errors:?}");
        lib
    }

    /// A USB-UART bridge (`TXD` out, `RXD` in), an Ethernet PHY (`CRS_DV` out,
    /// `TX_EN` in) and a Cortex-M controller whose pads the card binds to RMII
    /// transmit (`PB11`) and receive (`PA7`) lines.
    const CARDS: &str = r#"
[[models]]
id = "bridge_x"
kind = "digital"
description = "synthetic USB-UART bridge"
[models.match]
value_re = "^BRIDGE_X$"
[models.pins]
"1" = "txd"
"2" = "rxd"
"3" = "gnd"
"4" = "vcc"
[models.logic]
inputs = ["rxd"]
outputs = ["txd"]
[models.logic.comb]
"txd" = "rxd"

[[models]]
id = "phy_x"
kind = "digital"
description = "synthetic Ethernet PHY"
[models.match]
value_re = "^PHY_X$"
[models.pins]
"1" = "crs_dv"
"2" = "tx_en"
"3" = "gnd"
"4" = "vcc"
[models.logic]
inputs = ["tx_en"]
outputs = ["crs_dv"]
[models.logic.comb]
"crs_dv" = "tx_en"

[[models]]
id = "cortex_x"
kind = "mcu"
description = "synthetic Cortex-M controller"
[models.match]
value_re = "^CORTEX_X$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "pb11_eth_tx_en"
"2" = "pa7_rmii_crs_dv"
"3" = "pa0"
"4" = "gnd"
"5" = "vdd"
"6" = "pa10_usart1_rx"
[models.pin_capability.uart_rx]
basis = "synthetic"
[models.pin_capability.uart_rx.pads]
"6" = "USART1_RX"

[[models]]
id = "buffer_x"
kind = "digital"
description = "synthetic non-inverting buffer"
[models.match]
value_re = "^BUFFER_X$"
[models.pins]
"1" = "a"
"2" = "y"
"3" = "gnd"
"4" = "vcc"
[models.logic]
inputs = ["a"]
outputs = ["y"]
[models.logic.comb]
"y" = "a"
"#;

    fn card_part(reference: &str, value: &str, pads: &[(&str, i64, &str)]) -> Component {
        let pads_only: Vec<(&str, i64)> = pads.iter().map(|(n, net, _)| (*n, *net)).collect();
        let mut c = part(reference, value, &pads_only);
        for (pin, (_, _, function)) in c.pins.iter_mut().zip(pads) {
            pin.function = (*function).into();
        }
        c
    }

    fn resistor(reference: &str, value: &str, a: i64, b: i64) -> Component {
        Component {
            lib_id: "Device:R".into(),
            ..part(reference, value, &[("1", a), ("2", b)])
        }
    }

    /// An isolated four-resistor package (`R_Pack04`): pin `i` with pin `9 - i`.
    fn pack(reference: &str, value: &str, pins: [i64; 8]) -> Component {
        let pads: Vec<(&str, i64)> = ["1", "2", "3", "4", "5", "6", "7", "8"]
            .into_iter()
            .zip(pins)
            .collect();
        Component {
            lib_id: "Device:R_Pack04".into(),
            footprint: "Resistor_SMD:R_Array_Convex_4x0603".into(),
            ..part(reference, value, &pads)
        }
    }

    fn run_with(
        lib: &ModelLibrary,
        nets: &[(i64, &str)],
        parts: Vec<Component>,
    ) -> Vec<LintFinding> {
        signal_direction_lint(&board(nets, parts), lib).findings
    }

    const AVR_NETS: [(i64, &str); 7] = [
        (1, "MCU_TX"),
        (2, "BRIDGE_TX"),
        (3, "MCU_RX"),
        (4, "BRIDGE_RX"),
        (5, "+3V3"),
        (6, "MID"),
        (7, "GND"),
    ];

    /// The AVR's UART pads (PD1/TXD pad 3, PD0/RXD pad 2) and the bridge's `TXD`
    /// and `RXD`, on the nets `tx_net` and `rx_net` give (the AVR's, the
    /// bridge's), with `between` the parts that join them.
    fn avr_and_bridge(
        tx_net: (i64, i64),
        rx_net: (i64, i64),
        between: Vec<Component>,
    ) -> Vec<Component> {
        let mut parts = vec![
            card_part(
                "U1",
                "ATmega328P",
                &[("3", tx_net.0, "PD1/TXD"), ("2", rx_net.0, "PD0/RXD")],
            ),
            card_part(
                "U2",
                "BRIDGE_X",
                &[("1", tx_net.1, "TXD"), ("2", rx_net.1, "RXD")],
            ),
        ];
        parts.extend(between);
        parts
    }

    /// An 8-bit AVR's UART and a USB-UART bridge wired TX-to-TX and RX-to-RX, each
    /// through one element of a resistor network: a crossed pair, however many
    /// ohms the elements are.
    #[test]
    fn a_crossed_uart_pair_behind_resistor_network_elements_is_flagged() {
        let lib = library("avr", CARDS);
        // Elements 1-8 and 2-7 of RN1 carry the two lines.
        let rn = pack("RN1", "1k", [1, 3, 0, 0, 0, 0, 4, 2]);
        let found = run_with(&lib, &AVR_NETS, avr_and_bridge((1, 2), (3, 4), vec![rn]));
        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(found.iter().all(|f| f.severity == Severity::High));
        assert!(found.iter().all(|f| f.refs == ["U1", "U2"]));
        let tx = found.iter().find(|f| f.nets[0] == "MCU_TX").unwrap();
        assert_eq!(tx.nets, ["MCU_TX", "BRIDGE_TX"]);
        assert!(tx.message.contains("only outputs"), "{}", tx.message);
        assert!(tx.message.contains("RN1"), "{}", tx.message);
        assert!(tx.message.contains("crossed"), "{}", tx.message);
        // The controller's pad says why it is read as an output.
        assert!(tx.message.contains("binds this pad"), "{}", tx.message);
        let rx = found.iter().find(|f| f.nets[0] == "MCU_RX").unwrap();
        assert!(rx.message.contains("only inputs"), "{}", rx.message);
    }

    #[test]
    fn two_pin_series_resistors_hide_the_same_crossed_pair() {
        let lib = library("avr_r", CARDS);
        let rs = vec![resistor("R1", "1k", 1, 2), resistor("R2", "22", 3, 4)];
        let found = run_with(&lib, &AVR_NETS, avr_and_bridge((1, 2), (3, 4), rs));
        assert_eq!(found.len(), 2, "{found:#?}");
    }

    /// The control: the correctly crossed copy (TX to RX, RX to TX) behind the
    /// same elements is silent. The same crossed wires with no resistor are not.
    #[test]
    fn the_correctly_wired_pair_behind_the_same_elements_is_silent() {
        let lib = library("avr_ok", CARDS);
        let rn = pack("RN1", "1k", [1, 3, 0, 0, 0, 0, 4, 2]);
        // The AVR's TXD (net 1) to the bridge's RXD, its RXD (net 3) to the bridge's TXD.
        let ok = vec![
            card_part(
                "U1",
                "ATmega328P",
                &[("3", 1, "PD1/TXD"), ("2", 3, "PD0/RXD")],
            ),
            card_part("U2", "BRIDGE_X", &[("1", 4, "TXD"), ("2", 2, "RXD")]),
            rn,
        ];
        assert!(run_with(&lib, &AVR_NETS, ok).is_empty());
        let bare = avr_and_bridge((1, 1), (3, 3), vec![]);
        assert_eq!(run_with(&lib, &AVR_NETS, bare).len(), 2);
    }

    /// Everything that is not one small series resistor between two signals
    /// leaves the two halves alone.
    #[test]
    fn a_resistor_that_is_not_a_series_hop_does_not_join_the_nets() {
        let lib = library("avr_no_hop", CARDS);
        let flagged = |between: Vec<Component>| {
            run_with(&lib, &AVR_NETS, avr_and_bridge((1, 2), (3, 4), between))
                .iter()
                .any(|f| f.nets.iter().any(|n| n == "MCU_TX"))
        };
        // Controls: a 1 kilohm hop does, and so does 4.7 kilohms, the limit.
        assert!(flagged(vec![resistor("R1", "1k", 1, 2)]));
        assert!(flagged(vec![resistor("R1", "4.7k", 1, 2)]));
        // Too large for a series element.
        assert!(!flagged(vec![resistor("R1", "10k", 1, 2)]));
        // A pull-up or pull-down is a resistor to a rail, not a hop.
        assert!(!flagged(vec![
            resistor("R1", "1k", 1, 5),
            resistor("R2", "1k", 2, 7)
        ]));
        // A divider: the midpoint holds the second leg, which the line reads.
        assert!(!flagged(vec![
            resistor("R1", "1k", 1, 6),
            resistor("R2", "1k", 6, 7)
        ]));
        // Two resistors in series are not traced.
        assert!(!flagged(vec![
            resistor("R1", "1k", 1, 6),
            resistor("R2", "1k", 6, 2)
        ]));
        // A resistor with a capacitor to ground that make a filter (1 k, 1 uF),
        // and a small damping capacitor that leaves it a wire.
        let with_cap = |c: &str| {
            let mut cap = part("C1", c, &[("1", 1), ("2", 7)]);
            cap.lib_id = "Device:C".into();
            flagged(vec![resistor("R1", "1k", 1, 2), cap])
        };
        assert!(!with_cap("1uF"));
        assert!(with_cap("10pF"));
        // A resistor not fitted joins nothing.
        let mut dnp = resistor("R1", "1k", 1, 2);
        dnp.dnp = true;
        assert!(!flagged(vec![dnp]));
    }

    /// A bussed or part-used network is not paired by pin position.
    #[test]
    fn a_network_with_a_pin_on_a_rail_is_not_paired_by_position() {
        let lib = library("avr_bussed", CARDS);
        // Pin 1 on +3V3 (a common pin): pins 2 and 7 are not an element.
        let rp = pack("RP1", "1k", [5, 1, 0, 0, 0, 0, 2, 0]);
        let found = run_with(&lib, &AVR_NETS, avr_and_bridge((1, 2), (3, 4), vec![rp]));
        assert!(found.is_empty(), "{found:#?}");
    }

    /// A pad the card binds to a peripheral function is read as it only where
    /// the part facing it has a card that says so.
    #[test]
    fn a_bound_pad_needs_a_facing_pin_the_card_lists() {
        let lib = library("avr_facing", CARDS);
        // The bridge's TXD typed output by its symbol, with no card behind it.
        let mut other = card_part("U2", "UNCARDED_BRIDGE", &[("1", 2, "TXD")]);
        other.pins[0].kind = "output".into();
        let parts = vec![
            card_part("U1", "ATmega328P", &[("3", 1, "PD1/TXD")]),
            other,
            resistor("R1", "1k", 1, 2),
        ];
        assert!(run_with(&lib, &AVR_NETS, parts).is_empty());
    }

    /// A pad the card can use as a UART receive line, wired to a buffer's input:
    /// a GPIO or timer output on the same pad drives it, so nothing is wrong.
    /// The same pad facing the bridge's `RXD` is RX to RX.
    #[test]
    fn a_uart_capable_pad_driving_a_plain_logic_input_is_not_read_as_uart() {
        let lib = library("uart_gpio", CARDS);
        let nets = [(1, "BTM_LIGHT")];
        let mcu = || card_part("U1", "CORTEX_X", &[("6", 1, "PA10")]);
        let buffer = card_part("U2", "BUFFER_X", &[("1", 1, "A")]);
        assert!(run_with(&lib, &nets, vec![mcu(), buffer]).is_empty());
        let bridge = card_part("U2", "BRIDGE_X", &[("2", 1, "RXD")]);
        let found = run_with(&lib, &nets, vec![mcu(), bridge]);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].message.contains("only inputs"),
            "{}",
            found[0].message
        );
    }

    /// A Cortex-M's RMII transmit-enable pad through 22 ohms into a pin the PHY's
    /// card lists as an output (`CRS_DV`): the PHY's own input is left undriven.
    #[test]
    fn an_rmii_transmit_pad_facing_a_phy_output_through_a_resistor_is_flagged() {
        let lib = library("rmii", CARDS);
        let nets = [(1, "ETH_TXEN"), (2, "PHY_CRS"), (3, "PHY_TXEN")];
        let wired = |mcu: Component, phy_pad: &str, far: i64| {
            run_with(
                &lib,
                &nets,
                vec![
                    mcu,
                    card_part("U2", "PHY_X", &[(phy_pad, far, "X")]),
                    resistor("R1", "22", 1, far),
                ],
            )
        };
        let bound = || card_part("U1", "CORTEX_X", &[("1", 1, "PB11")]);
        let found = wired(bound(), "1", 2);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].message.contains("only outputs"));
        assert_eq!(found[0].nets, ["ETH_TXEN", "PHY_CRS"]);
        // Control: TX_EN to the PHY's TX_EN input is silent.
        assert!(wired(bound(), "2", 3).is_empty());
        // The pad named by the symbol alone, the card calling it a port pad.
        let named = card_part("U1", "CORTEX_X", &[("3", 1, "PA0/ETH_TX_EN")]);
        assert_eq!(wired(named, "1", 2).len(), 1);
        // A pad name that lists several functions states none.
        let several = card_part("U1", "CORTEX_X", &[("3", 1, "PA0/ETH_TX_EN/I2C1_SDA")]);
        assert!(wired(several, "1", 2).is_empty());
        // A plain port pad with no binding is the firmware's.
        let plain = card_part("U1", "CORTEX_X", &[("3", 1, "PA0")]);
        assert!(wired(plain, "1", 2).is_empty());
    }
}
