//! A two-wire bus crossed between a standard connector and the part behind it.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A USB receptacle and a Qwiic, STEMMA QT or Grove port fix which pin carries
//! which line of a pair (`D+`/`D-`, `SDA`/`SCL`). A board that lands the
//! connector's `D+` on the controller's `D-` pad, or its `SDA` pin on the
//! controller's `SCL` pad, never enumerates or never answers, and nothing about
//! the supply or the levels looks wrong. The roles of the connector's pins come
//! from `hauksbee_ir::connector_pinouts`, each row citing its document and
//! sha256.
//!
//! Judged only on definite evidence:
//!
//! - **The connector's pin has a known line.** A USB Type-C receptacle's pads
//!   `A6`/`B6` (`D+`) and `A7`/`B7` (`D-`) (USB Type-C spec R2.0 Table 3-4); a
//!   connector named for Qwiic or STEMMA QT with its four numbered pads (pin 3
//!   `SDA`, pin 4 `SCL`); a Grove socket (pin 1 `SCL`, pin 2 `SDA`); or any
//!   connector whose symbol names the pin `D+`, `D-`, `SDA` or `SCL`. Where the
//!   table and the symbol's own pin name disagree the pin is not judged.
//! - **The far pad's line is known.** The pad of a part behind the connector
//!   (through a series resistor or ferrite of at most 1 kΩ, or the windings of
//!   a common-mode choke) whose card, or failing that symbol pin name, states
//!   the line. An MCU card that declares hardware pad tables for both I2C lines
//!   states it from those tables alone (a pad that can carry only `SCL`), and a
//!   card that routes I2C to any pad states nothing, so a controller that can
//!   put `SDA` anywhere is never judged.
//! - **Every far pad says the opposite.** The connector pin's net, with the nets
//!   one series part away, carries only pads of the opposite line. A pad that
//!   agrees, or a net with pads of both lines, is a short or a mix-up and is
//!   another check's finding. A connector on the far side of the net is not a
//!   pad: it is a header, not a controller.
//! - **The other pin of the pair does not agree.** If one line of the pair
//!   reaches its own pad the board is wired straight and the odd net is a short.
//!   A Grove socket carries I2C, UART and analogue under one name, so it is
//!   judged only when both of its pins are crossed.
//!
//! A USB hub, a bridge or a USB-C switch splits the net into two segments, each
//! judged on its own side. DNP parts take no part. USB swaps and I2C swaps are
//! High: no firmware can swap a USB PHY's polarity, and an I2C bus with
//! its lines crossed has no master and slave in agreement; software I2C on
//! pads the card calls the other line is the one escape, which the message says.

use std::collections::BTreeSet;

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_connector_like, is_ground, is_plain_resistor, is_test_point_or_hole, Component,
    ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin, Severity,
};
use hauksbee_ir::connector_pinouts::{
    bus_connectors_named, bus_line_named, usb_c_line_at, BusLine, ConnectorSource,
    USB_C_SOURCE,
};
use hauksbee_ir::pin_functions::PeripheralFunction;
use hauksbee_models::netclass::is_positive_supply;
use hauksbee_models::value::parse_value;
use hauksbee_models::ModelLibrary;

/// The largest series resistance a line is followed through. A termination
/// network or a divider is not a series part of the line.
const SERIES_MAX_OHMS: f64 = 1_000.0;

/// How many nets a line's group may grow to before it is not followed further.
const GROUP_MAX_NETS: usize = 12;

/// The lower-case words of `text`, split on everything but letters and digits.
fn words(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Maps a pad number to the line a connector standard puts on it.
type LineAt = Box<dyn Fn(&str) -> Option<BusLine>>;

/// A pin of a connector with the line it carries and why that is known.
struct NearPin<'c> {
    pin: &'c Pin,
    line: BusLine,
    /// What states the line, as `pad A6 (Dp1)`.
    basis: String,
}

/// The standard that fixed a connector's pin roles, when one did.
struct Standard {
    name: String,
    /// The connector shares its name with ones that carry other buses.
    shared_name: bool,
    source: ConnectorSource,
}

/// The pins of `comp` whose line is known, and the standard behind them.
fn near_pins(comp: &Component) -> (Vec<NearPin<'_>>, Option<Standard>) {
    let numbered: BTreeSet<&str> = comp
        .pins
        .iter()
        .map(|p| p.number.trim())
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    let named = words(&format!(
        "{} {} {}",
        comp.value, comp.lib_id, comp.footprint
    ));
    let pads: BTreeSet<String> = comp
        .pins
        .iter()
        .map(|p| p.number.trim().to_ascii_uppercase())
        .collect();
    let is_usb_c = ["A5", "B5", "A6", "A7", "B6", "B7"]
        .iter()
        .all(|p| pads.contains(*p));
    let table: Option<(Standard, LineAt)> = if is_usb_c {
        Some((
            Standard {
                name: "USB Type-C receptacle".into(),
                shared_name: false,
                source: USB_C_SOURCE,
            },
            Box::new(usb_c_line_at),
        ))
    } else {
        let mut found = bus_connectors_named(numbered.len(), &named);
        found.next().filter(|_| found.next().is_none()).map(|c| {
            (
                Standard {
                    name: c.name.to_string(),
                    shared_name: c.shared_name,
                    source: c.source,
                },
                Box::new(move |pad: &str| c.line_at(pad)) as LineAt,
            )
        })
    };
    let mut pins = Vec::new();
    for pin in &comp.pins {
        if pin.net.is_none_or(|n| n == 0) {
            continue;
        }
        let by_table = table.as_ref().and_then(|(_, f)| f(&pin.number));
        let by_name = bus_line_named(&pin.function);
        let (line, basis) = match (by_table, by_name) {
            (Some(t), Some(n)) if t != n => continue,
            (Some(t), _) => (t, format!("pad {}", pin.number)),
            (None, Some(n)) => (
                n,
                format!("pad {} (symbol pin '{}')", pin.number, pin.function),
            ),
            (None, None) => continue,
        };
        pins.push(NearPin { pin, line, basis });
    }
    (pins, table.map(|(s, _)| s))
}

/// Whether net `id` is a supply or a ground: a pull or a decoupling, never
/// part of a data line.
fn is_rail(board: &ExtractedBoard, id: i64) -> bool {
    board
        .net(id)
        .is_some_and(|n| is_ground(&n.name) || is_positive_supply(&n.name))
}

/// Whether `comp` is a series part of a line: a resistor of at most
/// [`SERIES_MAX_OHMS`], a ferrite bead or an inductor.
fn is_series_part(comp: &Component) -> bool {
    if comp.pins.len() != 2 {
        return false;
    }
    if is_plain_resistor(comp) {
        return parse_value(&comp.value).is_some_and(|v| v.si <= SERIES_MAX_OHMS);
    }
    let r = comp.reference.to_ascii_uppercase();
    let lib = comp.lib_id.to_ascii_lowercase();
    r.starts_with("FB")
        || lib.contains("ferrite")
        || (r.starts_with('L') && r[1..].chars().all(|c| c.is_ascii_digit()) && r.len() > 1)
}

/// Whether `comp` is a four-pad common-mode choke, whose windings join pad 1 to
/// pad 4 and pad 2 to pad 3 (the numbering of the SMD chokes of Murata DLW, TDK
/// ACM and Wurth WE-CNSW, counted round the package).
fn is_common_mode_choke(comp: &Component) -> bool {
    if comp.pins.len() != 4 {
        return false;
    }
    let w = words(&format!(
        "{} {} {}",
        comp.value, comp.lib_id, comp.footprint
    ));
    w.iter().any(|x| {
        matches!(x.as_str(), "choke" | "cmc" | "cmf" | "dlw5bsn")
            || x.starts_with("acm")
            || x.starts_with("dlw")
            || x.starts_with("act45")
    }) || w.windows(2).any(|p| p[0] == "common" && p[1] == "mode")
}

/// The nets `net` is joined to through series parts, with `net` first.
fn line_nets(board: &ExtractedBoard, net: i64) -> Vec<i64> {
    let mut group = vec![net];
    let mut i = 0;
    while i < group.len() && group.len() < GROUP_MAX_NETS {
        let here = group[i];
        i += 1;
        for comp in &board.components {
            if AssemblyState::of(comp).fitted().is_none() {
                continue;
            }
            let far: Vec<i64> = if is_series_part(comp) {
                let (a, b) = (comp.pins[0].net, comp.pins[1].net);
                match (a, b) {
                    (Some(a), Some(b)) if a == here => vec![b],
                    (Some(a), Some(b)) if b == here => vec![a],
                    _ => vec![],
                }
            } else if is_common_mode_choke(comp) {
                let net_of = |n: &str| {
                    comp.pins
                        .iter()
                        .find(|p| p.number.trim() == n)
                        .and_then(|p| p.net)
                };
                [("1", "4"), ("2", "3")]
                    .iter()
                    .filter_map(|(x, y)| match (net_of(x), net_of(y)) {
                        (Some(a), Some(b)) if a == here => Some(b),
                        (Some(a), Some(b)) if b == here => Some(a),
                        _ => None,
                    })
                    .collect()
            } else {
                vec![]
            };
            for f in far {
                if f != 0 && !group.contains(&f) && !is_rail(board, f) {
                    group.push(f);
                }
            }
        }
    }
    group
}

/// The line a pad of `comp` carries on the bus `like` belongs to, with how the
/// board states it.
fn far_line(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    comp: &Component,
    pin: &Pin,
    like: BusLine,
) -> Option<(BusLine, String)> {
    let fitted = AssemblyState::of(comp).fitted()?;
    let model = hauksbee_bind::binder::resolve(lib, fitted)
        .model
        .filter(|m| super::card_fits(board, comp, m));
    if like.bus() == "I2C" {
        if let Some(m) = &model {
            let pc = &m.pin_capability;
            if pc.routed.as_ref().is_some_and(|r| {
                r.functions.contains(PeripheralFunction::I2cSda.key())
                    || r.functions.contains(PeripheralFunction::I2cScl.key())
            }) {
                return None;
            }
            if let (Some(sda), Some(scl)) = (
                pc.table(PeripheralFunction::I2cSda),
                pc.table(PeripheralFunction::I2cScl),
            ) {
                // A card with both tables is a complete statement: its pads
                // are judged by the tables alone.
                let in_sda = sda.pads.contains_key(&pin.number);
                let in_scl = scl.pads.contains_key(&pin.number);
                return match (in_sda, in_scl) {
                    (true, false) => Some((BusLine::I2cSda, "its card's I2C data pads".into())),
                    (false, true) => Some((BusLine::I2cScl, "its card's I2C clock pads".into())),
                    _ => None,
                };
            }
        }
    }
    let from_card = model
        .as_ref()
        .and_then(|m| m.pins.get(&pin.number))
        .and_then(|role| bus_line_named(role).map(|l| (l, format!("its card's role '{role}'"))));
    let stated = from_card.or_else(|| {
        bus_line_named(&pin.function)
            .map(|l| (l, format!("its symbol's pin name '{}'", pin.function)))
    })?;
    (stated.0.bus() == like.bus()).then_some(stated)
}

/// The pads of the parts around `group` (not `conn`, not connectors, not
/// test points) that state a line of `like`'s bus.
fn far_pads<'b>(
    board: &'b ExtractedBoard,
    lib: &ModelLibrary,
    conn: &Component,
    group: &[i64],
    like: BusLine,
) -> Vec<(&'b Component, &'b Pin, BusLine, String)> {
    let mut out = Vec::new();
    for &net in group {
        for (comp, pin) in board.net_members(net) {
            if std::ptr::eq(comp, conn) || is_connector_like(comp) || is_test_point_or_hole(comp) {
                continue;
            }
            if let Some((line, why)) = far_line(board, lib, comp, pin, like) {
                out.push((comp, pin, line, why));
            }
        }
    }
    out
}

/// What the check found of one connector pin.
enum Verdict {
    /// Every far pad states the opposite line: the pin is crossed.
    Crossed(Vec<String>, Vec<String>),
    /// A far pad states the pin's own line.
    Straight,
    /// Nothing, or a mix: not judged.
    Unjudged,
}

/// One connector pin against the pads behind it.
fn judge(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    conn: &Component,
    near: &NearPin<'_>,
) -> Verdict {
    let Some(net) = near.pin.net else {
        return Verdict::Unjudged;
    };
    let group = line_nets(board, net);
    let far = far_pads(board, lib, conn, &group, near.line);
    if far.is_empty() {
        return Verdict::Unjudged;
    }
    if far.iter().any(|(_, _, l, _)| *l == near.line) {
        return Verdict::Straight;
    }
    let refs = far.iter().map(|(c, ..)| c.reference.clone()).collect();
    let said = far
        .iter()
        .map(|(c, p, l, why)| {
            format!(
                "{} pin {} states {} ({why})",
                c.reference,
                p.number,
                l.label()
            )
        })
        .collect();
    Verdict::Crossed(refs, said)
}

/// Whether the far pads of a crossed I2C pin are hardware-pad evidence only
/// (a general-purpose port pad), so that software I2C could still drive it.
fn software_could_cross(said: &[String]) -> bool {
    said.iter().any(|s| s.contains("card's I2C"))
}

/// The swapped-lines findings of `board`.
#[must_use]
pub fn bus_swap_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    for conn in &board.components {
        if !is_connector_like(conn)
            || is_test_point_or_hole(conn)
            || AssemblyState::of(conn).fitted().is_none()
        {
            continue;
        }
        let (pins, standard) = near_pins(conn);
        if pins.is_empty() {
            continue;
        }
        for bus in ["USB", "I2C"] {
            let mut crossed: Vec<(&NearPin<'_>, Vec<String>, Vec<String>)> = Vec::new();
            let mut straight = false;
            let mut seen_nets = BTreeSet::new();
            for near in pins.iter().filter(|n| n.line.bus() == bus) {
                match judge(board, lib, conn, near) {
                    Verdict::Crossed(refs, said) => {
                        if seen_nets.insert(near.pin.net) {
                            crossed.push((near, refs, said));
                        }
                    }
                    Verdict::Straight => straight = true,
                    Verdict::Unjudged => {}
                }
            }
            if crossed.is_empty() || straight {
                continue;
            }
            let lines: BTreeSet<BusLine> = crossed.iter().map(|(n, ..)| n.line).collect();
            // A name that serves several buses is judged only when both lines of
            // the pair are crossed.
            if standard.as_ref().is_some_and(|s| s.shared_name) && lines.len() < 2 {
                continue;
            }
            report.findings.push(finding(board, conn, standard.as_ref(), &crossed, bus));
        }
    }
    report
}

/// The finding for a connector whose `bus` lines are crossed.
fn finding(
    board: &ExtractedBoard,
    conn: &Component,
    standard: Option<&Standard>,
    crossed: &[(&NearPin<'_>, Vec<String>, Vec<String>)],
    bus: &str,
) -> LintFinding {
    let mut refs = vec![conn.reference.clone()];
    let mut nets = Vec::new();
    let mut parts = Vec::new();
    let mut said_all = Vec::new();
    for (near, far_refs, said) in crossed {
        let net = near
            .pin
            .net
            .and_then(|id| board.net(id))
            .map_or_else(String::new, |n| n.name.clone());
        parts.push(format!(
            "{} {} ({}) is on net \"{net}\", where {}",
            conn.reference,
            near.line.label(),
            near.basis,
            said.join(", ")
        ));
        for r in far_refs {
            if !refs.contains(r) {
                refs.push(r.clone());
            }
        }
        if !nets.contains(&net) {
            nets.push(net);
        }
        said_all.extend(said.iter().cloned());
    }
    let basis = standard.map_or_else(
        || "the connector symbol's own pin names".to_string(),
        |s| {
            format!(
                "{} per {}, {}",
                s.name, s.source.document, s.source.location
            )
        },
    );
    let escape = if bus == "I2C" && software_could_cross(&said_all) {
        " If the firmware bit-bangs the bus in software it can drive each line on \
         the pad the card calls the other one; a hardware I2C peripheral cannot."
    } else {
        ""
    };
    let effect = if bus == "USB" {
        "A USB device with D+ and D- exchanged never enumerates; no firmware can swap a USB PHY's polarity"
    } else {
        "An I2C bus with SDA and SCL exchanged between the connector and the controller never \
         answers: the data pin is driven as a clock and the clock as data"
    };
    LintFinding {
        check: LintCheck::BusLineSwap,
        severity: Severity::High,
        message: format!(
            "{bus} lines are crossed: {}. {effect}. The connector's pins are {basis}.{escape} \
             Cross the two nets (or the pins) so each line reaches the pad of its own name",
            parts.join("; ")
        ),
        refs,
        nets,
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    const GND: i64 = 1;
    const V33: i64 = 2;
    const A: i64 = 3;
    const B: i64 = 4;
    const MA: i64 = 5;
    const MB: i64 = 6;

    const NETS: [(i64, &str); 6] = [
        (GND, "GND"),
        (V33, "+3V3"),
        (A, "BUS_A"),
        (B, "BUS_B"),
        (MA, "MCU_A"),
        (MB, "MCU_B"),
    ];

    /// A USB Type-C receptacle with its USB 2.0 pairs on `dp` and `dm`.
    fn usb_c(dp: i64, dm: i64) -> Component {
        let mut j = part(
            "J1",
            "USB4105-GF-A",
            &[
                ("A5", 0),
                ("B5", 0),
                ("A6", dp),
                ("B6", dp),
                ("A7", dm),
                ("B7", dm),
            ],
        );
        j.lib_id = "Connector:USB_C_Receptacle_USB2.0".into();
        j
    }

    /// An MCU whose symbol names pad 1 `d1` and pad 2 `d2`, on `n1` and `n2`.
    fn bridge(d1: &str, n1: i64, d2: &str, n2: i64) -> Component {
        let mut u = part("U1", "SomeBridge", &[("1", n1), ("2", n2)]);
        u.pins[0].function = d1.into();
        u.pins[1].function = d2.into();
        u
    }

    fn run(parts: Vec<Component>) -> Vec<LintFinding> {
        bus_swap_lint(&board(&NETS, parts), &ModelLibrary::builtin()).findings
    }

    #[test]
    fn usb_data_lines_crossed_between_the_receptacle_and_the_bridge_are_flagged() {
        // The receptacle's D+ reaches the bridge pad named D-, and the reverse.
        let found = run(vec![usb_c(A, B), bridge("D-", A, "D+", B)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::BusLineSwap);
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["J1", "U1"]);
        assert_eq!(f.nets, ["BUS_A", "BUS_B"]);
        for want in [
            "USB lines are crossed",
            "J1 D+ (pad A6)",
            "U1 pin 1 states D-",
            "J1 D- (pad A7)",
            "USB Type-C Cable and Connector Specification",
            "Table 3-4",
        ] {
            assert!(f.message.contains(want), "{want}: {}", f.message);
        }
    }

    #[test]
    fn usb_through_a_series_resistor_and_a_common_mode_choke_is_followed() {
        // D+ -> R1 -> bridge D-; D- -> R2 -> bridge D+.
        let mut r1 = part("R1", "22", &[("1", A), ("2", MA)]);
        let mut r2 = part("R2", "22", &[("1", B), ("2", MB)]);
        r1.lib_id = "Device:R".into();
        r2.lib_id = "Device:R".into();
        let found = run(vec![usb_c(A, B), r1, r2, bridge("D-", MA, "D+", MB)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        // The same through a choke (pad 1 to 4 is D+, pad 2 to 3 is D-).
        let mut l = part("FL1", "ACM2012", &[("1", A), ("2", B), ("3", MB), ("4", MA)]);
        l.lib_id = "Device:L_Coupled".into();
        let found = run(vec![usb_c(A, B), l, bridge("D-", MA, "D+", MB)]);
        assert_eq!(found.len(), 1, "{found:#?}");
    }

    #[test]
    fn usb_wired_straight_is_silent() {
        assert!(run(vec![usb_c(A, B), bridge("D+", A, "D-", B)]).is_empty());
        // Through a resistor pair, too.
        let mut r1 = part("R1", "22", &[("1", A), ("2", MA)]);
        let mut r2 = part("R2", "22", &[("1", B), ("2", MB)]);
        r1.lib_id = "Device:R".into();
        r2.lib_id = "Device:R".into();
        assert!(run(vec![usb_c(A, B), r1, r2, bridge("D+", MA, "D-", MB)]).is_empty());
    }

    #[test]
    fn a_hub_between_the_connector_and_the_controller_splits_the_net() {
        // Receptacle -> hub upstream pads; hub downstream pads -> bridge. Each
        // side is wired straight, so nothing is flagged even though the hub's
        // pads carry the lines under other names.
        let hub = {
            let mut u = part("U2", "Hub", &[("1", A), ("2", B), ("3", MA), ("4", MB)]);
            u.pins[0].function = "USBDP_UP".into();
            u.pins[1].function = "USBDM_UP".into();
            u.pins[2].function = "USBDP_DN1".into();
            u.pins[3].function = "USBDM_DN1".into();
            u
        };
        assert!(run(vec![usb_c(A, B), hub, bridge("D+", MA, "D-", MB)]).is_empty());
    }

    #[test]
    fn a_pair_with_one_straight_line_or_no_known_far_pad_is_not_judged() {
        // D+ straight, D- onto a pad that also says D+: a short, not a swap.
        let mut u = bridge("D+", A, "D+", B);
        u.pins[1].function = "D+".into();
        assert!(run(vec![usb_c(A, B), u]).is_empty());
        // Pads that state nothing.
        assert!(run(vec![usb_c(A, B), bridge("PA11", A, "PA12", B)]).is_empty());
        // A DNP bridge takes no part.
        let mut dnp = bridge("D-", A, "D+", B);
        dnp.dnp = true;
        assert!(run(vec![usb_c(A, B), dnp]).is_empty());
    }

    /// A 4-pad connector named `value` on SDA/SCL nets `(sda, scl)` at pins 3, 4.
    fn qwiic(value: &str, sda: i64, scl: i64) -> Component {
        let mut j = part("J2", value, &[("1", GND), ("2", V33), ("3", sda), ("4", scl)]);
        j.lib_id = "Connector_Generic:Conn_01x04".into();
        j
    }

    #[test]
    fn a_qwiic_port_with_sda_and_scl_swapped_to_the_controller_is_flagged() {
        // Qwiic pin 3 is SDA but lands on the controller pad named SCL.
        let found = run(vec![qwiic("Qwiic", A, B), bridge("SCL", A, "SDA", B)]);
        assert_eq!(found.len(), 1, "{found:#?}");
        let f = &found[0];
        assert_eq!(f.check, LintCheck::BusLineSwap);
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["J2", "U1"]);
        assert!(f.message.contains("I2C lines are crossed"), "{}", f.message);
        assert!(f.message.contains("J2 SDA (pad 3)"), "{}", f.message);
        assert!(f.message.contains("STEMMA"), "{}", f.message);
        // A STEMMA QT connector reads the same.
        assert_eq!(
            run(vec![qwiic("STEMMA_QT", A, B), bridge("SCL", A, "SDA", B)]).len(),
            1
        );
    }

    #[test]
    fn a_qwiic_port_wired_straight_or_not_named_so_is_silent() {
        assert!(run(vec![qwiic("Qwiic", A, B), bridge("SDA", A, "SCL", B)]).is_empty());
        // The same wiring on a connector no standard names, whose symbol names
        // no pins, is the designer's own order.
        assert!(run(vec![qwiic("Conn_01x04", A, B), bridge("SCL", A, "SDA", B)]).is_empty());
    }

    #[test]
    fn a_grove_socket_is_judged_only_when_both_lines_are_crossed() {
        // Grove pin 1 is SCL and pin 2 SDA: pins 1 and 2 on SDA and SCL nets.
        let grove = |p1: i64, p2: i64| {
            let mut j = part("J3", "Grove", &[("1", p1), ("2", p2), ("3", V33), ("4", GND)]);
            j.lib_id = "Connector_Generic:Conn_01x04".into();
            j
        };
        assert_eq!(
            run(vec![grove(A, B), bridge("SDA", A, "SCL", B)]).len(),
            1,
            "both crossed"
        );
        assert!(run(vec![grove(A, B), bridge("SCL", A, "SDA", B)]).is_empty());
        // Only one crossed line: a Grove socket may be UART or analogue.
        assert!(run(vec![grove(A, B), bridge("SDA", A, "PB3", B)]).is_empty());
    }

    #[test]
    fn a_symbol_that_names_the_connector_pins_is_judged_by_those_names() {
        let mut j = part("J4", "Conn", &[("1", A), ("2", B)]);
        j.pins[0].function = "SDA".into();
        j.pins[1].function = "SCL".into();
        assert_eq!(run(vec![j.clone(), bridge("SCL", A, "SDA", B)]).len(), 1);
        assert!(run(vec![j, bridge("SDA", A, "SCL", B)]).is_empty());
    }

    #[test]
    fn pull_ups_do_not_join_the_line_to_the_supply() {
        let mut r = part("R5", "4.7k", &[("1", A), ("2", V33)]);
        r.lib_id = "Device:R".into();
        let found = run(vec![
            qwiic("Qwiic", A, B),
            r,
            bridge("SCL", A, "SDA", B),
        ]);
        assert_eq!(found.len(), 1, "{found:#?}");
    }
}
