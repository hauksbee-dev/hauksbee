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
//! A net needs pins of two different parts. The nets the other checks already
//! report (two push-pull outputs, a CMOS input with nothing on its net, two
//! UART transmit pins) are theirs, and a finding on such a net is not repeated.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_connector_like, is_ground, is_test_point_or_hole, is_unconnected_net,
    pin_is_no_connect, uart_pin_role, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Pin, Severity,
};
use hauksbee_models::netclass::may_be_supply;
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

/// What a pin does to the net it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
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
enum Member {
    /// Takes no part: a capacitor, a test point, a part not fitted, a pin
    /// marked no-connect.
    Ignored,
    /// A pin with a known direction.
    Pin(Dir),
    /// Anything the check cannot read: the net is not judged.
    Unknown,
}

fn classify(comp: &Component, pin: &Pin, model: Option<&ModelEntry>) -> Member {
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

/// Why a net was called wrong.
enum Fault {
    /// Outputs only: nothing reads the net.
    NoListener,
    /// Inputs only: nothing drives the net.
    NoDriver,
}

/// The pins of a net as words, `U1.2 (TXD) [output]`.
fn describe(pins: &[(&Component, &Pin, Dir)]) -> Vec<String> {
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
            format!("{}.{}{name} [{kind}]", c.reference, p.number)
        })
        .collect()
}

/// The two nets-of-fault lists of a pair of parts: no-listener nets, no-driver nets.
type PairFaults = BTreeMap<(String, String), (Vec<String>, Vec<String>)>;

/// The finding for one faulty net; a pair of parts faulted both ways is crossed.
fn finding(
    fault: &Fault,
    name: String,
    refs: Vec<String>,
    listed: &[String],
    pairs: &PairFaults,
) -> LintFinding {
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
    let listed = hauksbee_ir::vocabulary::names_summary(listed);
    let message = match fault {
        Fault::NoListener => format!(
            "net '{name}' joins only outputs and nothing that reads it: {listed}. The \
             parts drive the same wire and neither hears the other, so the signal never \
             arrives (an output must go to the other side's input, TX to RX); wire each \
             output to the matching input pin.{crossed_note}"
        ),
        Fault::NoDriver => format!(
            "net '{name}' joins only inputs and nothing that drives it: {listed}. No \
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
        nets: vec![name],
    }
}

/// Flag every signal net that joins pins of two or more parts and cannot carry
/// a signal: it has drivers and no reader, or readers and no driver.
#[must_use]
pub fn signal_direction_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let models: HashMap<&str, ModelEntry> = super::resolved_components(board, lib)
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    let mut by_net: BTreeMap<i64, Vec<(&Component, &Pin)>> = BTreeMap::new();
    for c in &board.components {
        for p in &c.pins {
            if let Some(id) = p.net.filter(|id| *id != 0) {
                by_net.entry(id).or_default().push((c, p));
            }
        }
    }
    // Per pair of parts: the nets of each fault, to tell a crossed pair.
    let mut pairs = PairFaults::new();
    let mut found = Vec::new();
    for (id, members) in &by_net {
        let Some(net) = board.net(*id) else { continue };
        if is_unconnected_net(&net.name) || is_ground(&net.name) || may_be_supply(&net.name) {
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
        let parts: BTreeSet<&str> = pins.iter().map(|(c, _, _)| c.reference.as_str()).collect();
        if !readable || parts.len() < 2 {
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
        let refs: Vec<String> = parts.iter().map(|r| (*r).to_string()).collect();
        if let [a, b] = refs.as_slice() {
            let entry = pairs.entry((a.clone(), b.clone())).or_default();
            match fault {
                Fault::NoListener => entry.0.push(net.name.clone()),
                Fault::NoDriver => entry.1.push(net.name.clone()),
            }
        }
        let listed = describe(&pins);
        found.push((fault, net.name.clone(), refs, listed));
    }
    for (fault, name, refs, listed) in found {
        report
            .findings
            .push(finding(&fault, name, refs, &listed, &pairs));
    }
    report
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
}
