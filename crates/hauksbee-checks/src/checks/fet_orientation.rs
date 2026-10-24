//! A power FET wired backwards: its body diode bypasses the switch.
//!
//! A MOSFET is a switch only in one direction. Its body diode runs drain to
//! source in a P-channel part and source to drain in an N-channel one, and it
//! conducts whenever the supply side sits on its anode. Two board shapes put
//! the diode the wrong way round, and both read from topology and the card's
//! electrode roles alone (the binder's own resolver, so a KiCad pin function,
//! an Eagle pad named `G`/`D`/`S` and a bare SOT-23 pad number all resolve):
//!
//! - **A driven switch** (a gate held by an MCU pin or a driver): the supply
//!   side must be on the cathode. With the diode pointing from the supply to
//!   the load the load is powered through the diode with the gate off, so the
//!   rail can never be switched off.
//! - **Reverse-polarity protection** (a gate biased only by passives, so the
//!   FET is on): the drain must face the connector. A connector reached
//!   through a fuse, ferrite or other low-value series part counts, which the
//!   extract layer's direct-neighbour rule does not see.
//!
//! The supply side is the one that shows a source: a regulator output, a
//! battery or a power-output pin, then a connector, then (only when neither
//! side shows one) the one on a named rail. When the two sides show equal
//! evidence (a power-path FET between two sources, an ideal diode) the check
//! abstains. Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.

use std::collections::BTreeSet;

use hauksbee_bind::binder::role_nets;
use hauksbee_extract::{
    is_connector_like, is_ground, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport,
    Pin, Severity,
};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, ModelLibrary};

use super::node_drive::{is_gpio_role, Index};
use super::regulator_enable::{pin_drives, resistor_ohms};

/// A series resistor at or below this reads as a wire between a connector
/// and the FET (a fuse substitute, a sense resistor), in ohms.
const WIRE_OHMS: f64 = 10.0;
/// Series hops walked from a net to find its source or connector.
const HOPS: usize = 2;

fn same(a: &Component, b: &Component) -> bool {
    a.reference == b.reference
}

/// Words that name a cell or its holder wherever they sit in a value,
/// footprint or symbol name, once split on anything that is not a letter or
/// digit (`BatteryHolder_Keystone_103`, `Battery_Cell`, `BAT`, `CR2032`).
/// Whole words only, so a `BAT54` Schottky diode is not a cell.
const CELL_WORDS: &[&str] = &[
    "bat",
    "batt",
    "battery",
    "batteries",
    "batteryholder",
    "batteryclip",
    "coincell",
    "cr2032",
    "cr2025",
    "cr2016",
    "cr1220",
    "cr1632",
    "cr927",
    "18650",
    "lipo",
    "liion",
];

fn names_a_cell(text: &str) -> bool {
    text.to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .any(|w| CELL_WORDS.contains(&w))
}

/// A battery or cell by what the part is, not by what its net is called: a
/// `BT`/`BAT` designator, a `Device:Battery*` symbol, a battery-holder or
/// clip footprint, or a small non-semiconductor part whose value, symbol or
/// footprint names a cell (`BAT`, `CR2032`, `18650`). A cell on a net KiCad
/// named `Net-(BT1-+)` is as much a source as one on `+BATT`.
fn is_battery_part(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let lib = c.lib_id.to_ascii_lowercase();
    let fp = c.footprint.to_ascii_lowercase();
    if r.starts_with("BT")
        || r.starts_with("BAT")
        || lib.starts_with("device:battery")
        || fp.contains("batteryholder")
        || fp.contains("battery_holder")
    {
        return true;
    }
    // A holder or clip carries two to four pads (the extra two are mounting
    // tabs). A diode, transistor or IC whose name merely contains the word is
    // not a cell.
    let passive_or_active = matches!(
        RefClass::of(&c.reference),
        RefClass::Resistor
            | RefClass::ResistorArray
            | RefClass::Varistor
            | RefClass::Thermistor
            | RefClass::Capacitor
            | RefClass::Inductor
            | RefClass::FerriteBead
            | RefClass::Diode
            | RefClass::Led
            | RefClass::Transistor
            | RefClass::ActiveIc
    );
    !passive_or_active
        && (2..=4).contains(&c.pins.len())
        && [&c.value, &c.lib_id, &c.footprint]
            .into_iter()
            .any(|t| names_a_cell(t))
}

/// A part the board's supply comes in through: a connector or a battery.
fn is_supply_source(c: &Component) -> bool {
    is_connector_like(c) || is_battery_part(c)
}

/// A two-pin part that is a wire at DC for this question: a fuse, a ferrite
/// bead, an inductor or a resistor of a few ohms or less.
fn is_series_element(c: &Component) -> bool {
    if c.pins.len() != 2 {
        return false;
    }
    match RefClass::of(&c.reference) {
        RefClass::FerriteBead | RefClass::Inductor => true,
        RefClass::Resistor => resistor_ohms(c).is_some_and(|r| r <= WIRE_OHMS),
        _ => {
            let r = c.reference.to_ascii_uppercase();
            let lib = c.lib_id.to_ascii_lowercase();
            r.starts_with('F') && !r.starts_with("FB") && !r.starts_with("FID")
                || lib.contains("fuse")
                || lib.contains("polyfuse")
        }
    }
}

/// `net` and every net reached from it through up to [`HOPS`] series
/// elements, never crossing the FET `q`.
fn cluster(idx: &Index<'_>, net: i64, q: &Component) -> Vec<i64> {
    let mut seen = vec![net];
    // Ground is the reference every sense resistor and bead returns to, not a
    // source: walking out of it would lend it another rail's evidence.
    let mut frontier = if is_ground(idx.name(net)) {
        Vec::new()
    } else {
        vec![net]
    };
    for _ in 0..HOPS {
        let mut reached = Vec::new();
        for n in frontier {
            for (o, _) in idx.members(n) {
                if same(o, q) || !is_series_element(o) {
                    continue;
                }
                for far in o.pins.iter().filter_map(|p| p.net).filter(|id| *id != 0) {
                    if !seen.contains(&far) {
                        seen.push(far);
                        reached.push(far);
                    }
                }
            }
        }
        frontier = reached;
    }
    seen
}

/// How strongly the nets of a cluster show a power source: 2 for a regulator
/// output, a battery or a power-output pin, 1 for a connector, else 0.
fn source_tier(idx: &Index<'_>, nets: &[i64], q: &Component) -> u8 {
    let mut tier = 0;
    for &n in nets {
        if is_ground(idx.name(n)) {
            continue;
        }
        for (o, p) in idx.members(n) {
            if same(o, q) {
                continue;
            }
            let reg_out = idx.model(o).zip(idx.role(o, p)).is_some_and(|(m, r)| {
                m.kind == ComponentKind::Vreg && matches!(r, "out" | "vout" | "sw")
            });
            if reg_out || p.kind.eq_ignore_ascii_case("power_out") || is_battery_part(o) {
                tier = tier.max(2);
            } else if is_connector_like(o) {
                tier = tier.max(1);
            }
        }
    }
    tier
}

fn cluster_level(idx: &Index<'_>, nets: &[i64]) -> Option<f64> {
    nets.iter()
        .filter_map(|&n| idx.level(n))
        .filter(|v| *v > 0.0)
        .max_by(f64::total_cmp)
}

/// Which of the FET's two channel nets `first` and `second` is the supply side,
/// when the evidence is not a tie.
fn supply_side(idx: &Index<'_>, q: &Component, (first, second): (i64, i64)) -> Option<i64> {
    let (near_first, near_second) = (cluster(idx, first, q), cluster(idx, second, q));
    let tier_first = source_tier(idx, &near_first, q);
    let tier_second = source_tier(idx, &near_second, q);
    if tier_first != tier_second {
        return Some(if tier_first > tier_second {
            first
        } else {
            second
        });
    }
    let levels = (
        cluster_level(idx, &near_first),
        cluster_level(idx, &near_second),
    );
    match levels {
        (Some(volts_first), Some(volts_second)) if (volts_first - volts_second).abs() > 0.5 => {
            Some(if volts_first > volts_second {
                first
            } else {
                second
            })
        }
        // The named-rail fallback: the side on a named rail is the supply.
        // It is wrong when the other side is an auto-named net that reaches
        // a two-pin part which is not a passive (an unrecognised battery
        // holder or clip): that side may well be the input, so abstain.
        (Some(_), None) if tier_first == 0 && !opaque_source_side(idx, second, q) => Some(first),
        (None, Some(_)) if tier_first == 0 && !opaque_source_side(idx, first, q) => Some(second),
        _ => None,
    }
}

/// A pin that drives its net: an output of any type, a GPIO, a logic output,
/// or the collector of a driver transistor.
fn drives(idx: &Index<'_>, o: &Component, p: &Pin) -> bool {
    if is_connector_like(o) {
        return false;
    }
    if p.kind.to_ascii_lowercase().contains("power") {
        return false;
    }
    if pin_drives(p) || p.kind.trim().eq_ignore_ascii_case("bidirectional") {
        return true;
    }
    let (Some(m), Some(role)) = (idx.model(o), idx.role(o, p)) else {
        return false;
    };
    match m.kind {
        ComponentKind::Mcu => is_gpio_role(role),
        ComponentKind::Digital | ComponentKind::ShiftRegister => {
            m.logic.outputs.iter().any(|x| x == role)
        }
        ComponentKind::BjtNpn
        | ComponentKind::BjtPnp
        | ComponentKind::Nmos
        | ComponentKind::Pmos => {
            matches!(role, "collector" | "drain")
        }
        _ => false,
    }
}

/// The first pin that drives the gate net, or one series gate resistor away:
/// the part it belongs to and a "REF pin N" label for it.
fn gate_driver<'a>(idx: &Index<'a>, q: &Component, gate: i64) -> Option<(&'a Component, String)> {
    // A gate tied to a rail or ground is not switched by anything.
    if idx.level(gate).is_some() {
        return None;
    }
    let mut nets = vec![gate];
    for (o, _) in idx.members(gate) {
        if !same(o, q) && o.pins.len() == 2 && RefClass::of(&o.reference) == RefClass::Resistor {
            nets.extend(
                o.pins
                    .iter()
                    .filter_map(|p| p.net)
                    .filter(|n| *n != 0 && *n != gate && idx.level(*n).is_none()),
            );
        }
    }
    nets.iter().find_map(|&n| {
        idx.members(n)
            .iter()
            .find(|(o, p)| !same(o, q) && drives(idx, o, p))
            .map(|(o, p)| (*o, format!("{} pin {}", o.reference, p.number)))
    })
}

/// The nets a controller can sense a channel side on: the side's cluster plus
/// every net one resistor of any value further out (a sense pin sits behind a
/// filter resistor of 100 ohms or so, which the cluster's wire-like hop does
/// not cross).
fn sensed_nets(idx: &Index<'_>, net: i64, q: &Component, gate: i64) -> Vec<i64> {
    let mut nets = cluster(idx, net, q);
    let first = nets.clone();
    for n in first.into_iter().filter(|n| !is_ground(idx.name(*n))) {
        for (o, _) in idx.members(n) {
            if same(o, q) || o.pins.len() != 2 || RefClass::of(&o.reference) != RefClass::Resistor {
                continue;
            }
            for far in o.pins.iter().filter_map(|p| p.net).filter(|f| *f != 0) {
                if far != gate && !nets.contains(&far) {
                    nets.push(far);
                }
            }
        }
    }
    nets.retain(|n| *n != gate);
    nets
}

/// True when the part driving the gate is a controller that also has a sense
/// or terminal pin on each side of the channel: a power-path, ideal-diode,
/// hot-swap or charger IC watching the battery on one side and its own
/// converter output on the other (the LTC4020's BAT and CSN/CSP around its
/// `PowerPath` FET). Such a controller servos the FET to its documented
/// topology (the body diode carrying the battery or the input to the output
/// is usually the point), and the schematic alone cannot say which side is
/// the load, so the check abstains. A microcontroller is excluded: it
/// switches a rail, it does not own the power path. Supply pins do not count
/// as sensing, and neither does the gate net itself (a pull-up there is bias).
fn controller_spans_channel(
    idx: &Index<'_>,
    q: &Component,
    driver: &Component,
    gate: i64,
    (s, d): (i64, i64),
) -> bool {
    if idx
        .model(driver)
        .is_some_and(|m| m.kind == ComponentKind::Mcu)
    {
        return false;
    }
    let touches = |near: &[i64]| {
        driver.pins.iter().any(|p| {
            !p.kind.to_ascii_lowercase().contains("power")
                && p.net
                    .is_some_and(|n| n != 0 && near.contains(&n) && !is_ground(idx.name(n)))
        })
    };
    touches(&sensed_nets(idx, s, q, gate)) && touches(&sensed_nets(idx, d, q, gate))
}

/// A two-pin resistor, capacitor or diode: biases a node, never drives one.
fn is_bias_part(c: &Component) -> bool {
    c.pins.len() == 2
        && matches!(
            RefClass::of(&c.reference),
            RefClass::Resistor | RefClass::Capacitor | RefClass::Diode
        )
}

/// The gate's only bias runs to the FET's own source (`ends` is source then
/// drain), so Vgs is held at zero and the FET is off: a pull-down on an
/// N-FET that sits on ground, or a pull-up on a P-FET wired to its own
/// supply, is a switch held open, not an always-on part.
fn gate_is_held_off(idx: &Index<'_>, q: &Component, gate: i64, [source, _]: [i64; 2]) -> bool {
    let mut bias = idx
        .members(gate)
        .iter()
        .filter(|(o, _)| !same(o, q))
        .flat_map(|(o, _)| o.pins.iter().filter_map(|p| p.net))
        .filter(|n| *n != 0 && *n != gate)
        .peekable();
    bias.peek().is_some() && bias.all(|n| n == source)
}

/// True when the gate net, and every net one bias part further out, holds
/// only bias parts (or a ground), ending on ground or the FET's own channel
/// nets: nothing drives the gate, so the FET sits permanently on.
fn gate_is_passive(idx: &Index<'_>, q: &Component, gate: i64, ends: [i64; 2]) -> bool {
    let returns = supply_returns(idx, q, (ends[0], ends[1]));
    let stop = |n: i64| ends.contains(&n) || is_ground(idx.name(n)) || returns.contains(&n);
    if gate_is_held_off(idx, q, gate, ends) {
        return false;
    }
    let mut frontier = vec![gate];
    let mut seen = vec![gate];
    for depth in 0..2 {
        let mut next = Vec::new();
        for net in frontier {
            let others: Vec<_> = idx
                .members(net)
                .iter()
                .filter(|(o, _)| !same(o, q))
                .collect();
            if depth == 0 && others.is_empty() && !is_ground(idx.name(net)) {
                return false;
            }
            if others.iter().any(|(o, _)| !is_bias_part(o)) {
                return false;
            }
            for (o, _) in others {
                for far in o.pins.iter().filter_map(|p| p.net).filter(|n| *n != 0) {
                    if !seen.contains(&far) && !stop(far) {
                        seen.push(far);
                        next.push(far);
                    }
                }
            }
        }
        frontier = next;
    }
    frontier.is_empty()
}

/// A net KiCad (or another tool) named itself, `Net-(J1-Pin_1)`, rather than
/// a person: its name says nothing about what the net carries.
fn is_auto_named(name: &str) -> bool {
    let leaf = name.rsplit('/').next().unwrap_or(name);
    leaf.starts_with("Net-(") || leaf.starts_with("unconnected-(")
}

/// An auto-named net whose cluster reaches a two-pin part that is neither a
/// passive nor a semiconductor: a supply the binder could not name (a battery
/// holder, clip or module drawn with a generic symbol). Its side of a FET may
/// be the input, so a rail-name fallback must not call the other side the
/// supply.
fn opaque_source_side(idx: &Index<'_>, net: i64, q: &Component) -> bool {
    if !is_auto_named(idx.name(net)) {
        return false;
    }
    cluster(idx, net, q).into_iter().any(|n| {
        idx.members(n).iter().any(|(o, _)| {
            !same(o, q)
                && o.pins.len() == 2
                && matches!(
                    RefClass::of(&o.reference),
                    RefClass::Other | RefClass::Switch
                )
        })
    })
}

/// The gate net has a bias part to a ground net: a pull-down, as a
/// reverse-polarity P-FET's gate has.
fn gate_pulled_to_ground(idx: &Index<'_>, q: &Component, gate: i64) -> bool {
    idx.members(gate)
        .iter()
        .filter(|(o, _)| !same(o, q) && is_bias_part(o))
        .flat_map(|(o, _)| o.pins.iter().filter_map(|p| p.net))
        .any(|n| n != 0 && is_ground(idx.name(n)))
}

/// Whether a connector or a battery sits on the cluster of `net`: the board's
/// supply comes in there, whatever the net is called (`Net-(J1-Pin_1)` from a
/// battery connector has no name to read a rail from).
fn reaches_connector(idx: &Index<'_>, net: i64, q: &Component) -> bool {
    cluster(idx, net, q).into_iter().any(|n| {
        idx.members(n)
            .iter()
            .any(|(o, _)| !same(o, q) && is_supply_source(o))
    })
}

/// The return net of a two-terminal supply (a battery, or a two-net connector)
/// on the clusters of the FET's two channel nets. A board that names no ground
/// (a bare battery and a load) has this net where `GND` would be. A ground
/// net is skipped (every header on the board sits on it, and a FET whose
/// channel is on ground is a low-side switch, not the supply's protection),
/// and so is a connector with more than two nets: its other pins are signals.
fn supply_returns(idx: &Index<'_>, q: &Component, (s, d): (i64, i64)) -> Vec<i64> {
    let mut out = Vec::new();
    for net in cluster(idx, s, q).into_iter().chain(cluster(idx, d, q)) {
        if is_ground(idx.name(net)) {
            continue;
        }
        for (o, _) in idx.members(net) {
            if same(o, q) || !is_supply_source(o) {
                continue;
            }
            let mut nets: Vec<i64> = o
                .pins
                .iter()
                .filter_map(|p| p.net)
                .filter(|n| *n != 0)
                .collect();
            nets.sort_unstable();
            nets.dedup();
            if nets.len() != 2 {
                continue;
            }
            for far in nets.into_iter().filter(|n| *n != net) {
                if !out.contains(&far) {
                    out.push(far);
                }
            }
        }
    }
    out
}

/// A switching-converter power stage: an inductor sits on a channel net, so
/// the half-bridge's body diodes are part of the topology and the supply side
/// cannot be read from where the sources are.
fn on_inductor(idx: &Index<'_>, net: i64, q: &Component) -> bool {
    idx.members(net)
        .iter()
        .any(|(o, _)| !same(o, q) && RefClass::of(&o.reference) == RefClass::Inductor)
}

/// A second source feeds `net` through a diode (power-path OR-ing, an ideal
/// diode): the body diode conducting from the other side is the design.
fn diode_or_fed(idx: &Index<'_>, net: i64, q: &Component) -> bool {
    idx.members(net).iter().any(|(o, p)| {
        !same(o, q)
            && o.pins.len() == 2
            && RefClass::of(&o.reference) == RefClass::Diode
            && o.pins
                .iter()
                .filter(|x| !std::ptr::eq(*x, *p))
                .filter_map(|x| x.net)
                .filter(|&far| far != 0 && far != net && !is_ground(idx.name(far)))
                .any(|far| {
                    let near = cluster(idx, far, q);
                    source_tier(idx, &near, q) > 0 || cluster_level(idx, &near).is_some()
                })
    })
}

/// A resistor straight across the FET's channel (a pre-charge or soft-start
/// path): the part is not a bare protection element.
fn bypassed(idx: &Index<'_>, q: &Component, (s, d): (i64, i64)) -> bool {
    idx.members(s).iter().any(|(o, _)| {
        !same(o, q)
            && o.pins.len() == 2
            && RefClass::of(&o.reference) == RefClass::Resistor
            && o.pins.iter().any(|x| x.net == Some(d))
    })
}

/// A second FET of the same polarity stands cathode to cathode with `q`: its
/// body diode's cathode sits on `q`'s cathode net and its anode on a net other
/// than `q`'s anode net. The two diodes then oppose in series (a common-drain
/// N-FET pair, a common-source P-FET pair), so the pair blocks in both
/// directions with both gates off: the back-to-back switch of a battery
/// charge/discharge protection stage or a bidirectional load switch. Neither
/// FET alone is wired backwards. Two FETs in parallel (the anode nets shared)
/// are not such a pair. The shared node must be neither ground nor a named
/// rail.
fn series_opposed_partner(idx: &Index<'_>, q: &Component, pmos: bool, (s, d): (i64, i64)) -> bool {
    let (anode_role, cathode_role, anode, cathode) = if pmos {
        ("drain", "source", d, s)
    } else {
        ("source", "drain", s, d)
    };
    let Some(kind) = idx.model(q).map(|m| m.kind) else {
        return false;
    };
    // The shared node is the pair's midpoint, not a rail or ground that many
    // unrelated FETs reach.
    if is_ground(idx.name(cathode)) || idx.level(cathode).is_some() {
        return false;
    }
    idx.members(cathode).iter().any(|(o, p)| {
        !same(o, q)
            && idx.model(o).is_some_and(|m| m.kind == kind)
            && idx.role(o, p) == Some(cathode_role)
            && o.pins.iter().any(|x| {
                idx.role(o, x) == Some(anode_role)
                    && x.net.is_some_and(|n| n != 0 && n != anode && n != cathode)
            })
    })
}

/// Why a reverse-polarity FET's source is the board's input, if it is: the
/// source reaches the connector and the drain does not, or (no side shows a
/// recognised source) the gate is pulled to ground and the source is the
/// auto-named side that reaches an unrecognised two-pin part while the drain
/// sits on a named rail.
fn protection_input_note(
    idx: &Index<'_>,
    q: &Component,
    gate: i64,
    (s, d): (i64, i64),
) -> Option<&'static str> {
    if reaches_connector(idx, s, q) {
        (!reaches_connector(idx, d, q)).then_some("reaches the connector net")
    } else if !reaches_connector(idx, d, q)
        && gate_pulled_to_ground(idx, q, gate)
        && opaque_source_side(idx, s, q)
        && idx.level(d).is_some_and(|v| v > 0.0)
    {
        Some("reaches the unnamed input net")
    } else {
        None
    }
}

const fn diode_note(pmos: bool) -> &'static str {
    if pmos {
        "drain to source"
    } else {
        "source to drain"
    }
}

/// Run the check.
#[must_use]
pub fn fet_orientation_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let idx = Index::new(board, lib);
    let mut report = NetLintReport::default();
    let mut done = BTreeSet::new();
    for q in &board.components {
        let Some(model) = idx.model(q) else { continue };
        let pmos = match model.kind {
            ComponentKind::Pmos => true,
            ComponentKind::Nmos => false,
            _ => continue,
        };
        let roles = role_nets(q, model, lib.pin_rules());
        let (Some(&g), Some(&s), Some(&d)) =
            (roles.get("gate"), roles.get("source"), roles.get("drain"))
        else {
            continue;
        };
        if s == d || g == 0 || s == 0 || d == 0 || !done.insert(q.reference.clone()) {
            continue;
        }
        let name = |n: i64| idx.name(n).to_string();
        let anode = if pmos { d } else { s };
        let (anode_pin, cathode_pin) = if pmos {
            ("drain", "source")
        } else {
            ("source", "drain")
        };
        let channel = if pmos { "P" } else { "N" };
        if series_opposed_partner(&idx, q, pmos, (s, d)) {
            continue;
        }
        if let Some((driver_part, driver)) = gate_driver(&idx, q, g) {
            if controller_spans_channel(&idx, q, driver_part, g, (s, d)) {
                continue;
            }
            // Driven switch: the supply must not sit on the diode's anode.
            let supply = supply_side(&idx, q, (s, d))
                .filter(|n| *n == anode && !is_ground(idx.name(*n)))
                .filter(|_| !on_inductor(&idx, s, q) && !on_inductor(&idx, d, q));
            if let Some(supply) = supply {
                let load = if supply == s { d } else { s };
                if diode_or_fed(&idx, load, q) {
                    continue;
                }
                report.findings.push(LintFinding {
                    check: LintCheck::PinFunction,
                    severity: Severity::High,
                    message: format!(
                        "{} is a {channel}-channel FET switched by {driver} but wired backwards: its {anode_pin} is on '{}', the supply side, and its {cathode_pin} on '{}'. The body diode ({}) then conducts from '{}' to '{}' whenever the supply is up, so '{}' stays powered with the gate off and the switch cannot turn it off. {}swap source and drain so the body diode points from the load back to the supply",
                        q.reference, name(supply), name(load), diode_note(pmos), name(supply), name(load), name(load), hauksbee_extract::REMEDY_MARKER
                    ),
                    refs: vec![q.reference.clone()],
                    nets: vec![name(s), name(d)],
                });
            } else if !pmos
                && is_ground(idx.name(d))
                && !is_ground(idx.name(s))
                && idx.level(s).is_none()
            {
                // A low-side N-FET with drain on ground: the diode clamps the
                // load node to ground whenever it is above it.
                report.findings.push(LintFinding {
                    check: LintCheck::PinFunction,
                    severity: Severity::Medium,
                    message: format!(
                        "{} is an N-channel FET switched by {driver} but its drain is on ground '{}' and its source on '{}': the body diode (source to drain) then holds '{}' within a diode drop of ground whatever the gate does. {}swap source and drain so the source is on ground",
                        q.reference, name(d), name(s), name(s), hauksbee_extract::REMEDY_MARKER
                    ),
                    refs: vec![q.reference.clone()],
                    nets: vec![name(s), name(d)],
                });
            }
            continue;
        }
        // Reverse-polarity protection: drain faces the connector.
        // A source on ground is not a supply input the body diode must block:
        // every header on the board sits on that net, and a FET whose source
        // is on ground is a low-side switch.
        if is_ground(idx.name(s))
            || !gate_is_passive(&idx, q, g, [s, d])
            || bypassed(&idx, q, (s, d))
        {
            continue;
        }
        if let Some(input_note) = protection_input_note(&idx, q, g, (s, d)) {
            report.findings.push(LintFinding {
                check: LintCheck::PinFunction,
                severity: Severity::Medium,
                message: format!(
                    "{} is an always-on FET (its gate is biased only by passives), as used for reverse-polarity protection, but its source {input_note} '{}' (directly or through a fuse, bead or low-value resistor) and its drain '{}': the body diode then points from the load back to the input and a reversed supply is not blocked. {}swap source and drain so the drain faces the input",
                    q.reference, name(s), name(d), hauksbee_extract::REMEDY_MARKER
                ),
                refs: vec![q.reference.clone()],
                nets: vec![name(s), name(d)],
            });
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{board, pin_fn};

    const GND: i64 = 1;
    const V5: i64 = 2;
    const SW: i64 = 10;
    const GATE: i64 = 11;

    fn part(reference: &str, value: &str, lib_id: &str, pins: &[(&str, i64)]) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: lib_id.into(),
            footprint: "X".into(),
            pins: pins.iter().map(|(n, net)| pin_fn(n, *net, "")).collect(),
            ..Default::default()
        }
    }

    fn fet(value: &str, lib_id: &str, (s, d, g): (i64, i64, i64)) -> Component {
        let mut q = part("Q1", value, lib_id, &[]);
        q.pins = vec![
            pin_fn("1", g, "G"),
            pin_fn("2", s, "S"),
            pin_fn("3", d, "D"),
        ];
        q
    }

    /// A P-FET with `s` on the source net, `d` on the drain net.
    fn pfet(s: i64, d: i64, g: i64) -> Component {
        fet("AO3401A", "Device:Q_PMOS_GSD", (s, d, g))
    }

    fn nfet(s: i64, d: i64, g: i64) -> Component {
        fet("2N7002", "Device:Q_NMOS_GSD", (s, d, g))
    }

    fn header(net: i64) -> Component {
        part(
            "J1",
            "Conn",
            "Connector:Conn_01x02",
            &[("1", net), ("2", GND)],
        )
    }

    fn mcu(gate: i64) -> Component {
        part(
            "U2",
            "ATmega328P",
            "MCU:ATmega328P",
            &[("2", gate), ("7", V5), ("8", GND)],
        )
    }

    fn load(net: i64) -> Component {
        part("R2", "1k", "Device:R", &[("1", net), ("2", GND)])
    }

    fn lint(parts: Vec<Component>, extra: &[(i64, &str)]) -> Vec<LintFinding> {
        let mut nets = vec![(GND, "GND"), (V5, "+5V"), (SW, "SW_OUT"), (GATE, "GATE")];
        nets.extend_from_slice(extra);
        let b = board("t", &nets, parts);
        fet_orientation_lint(&b, &ModelLibrary::builtin()).findings
    }

    #[test]
    fn a_driven_high_side_pfet_with_drain_on_the_supply_cannot_switch_off() {
        let found = lint(
            vec![header(V5), pfet(SW, V5, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        assert_eq!(found[0].refs, ["Q1"]);
        assert!(
            found[0].message.contains("U2 pin 2")
                && found[0].message.contains("cannot turn it off"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn a_pulled_up_pfet_gate_released_by_an_nfet_with_drain_on_the_supply_is_flagged() {
        // The mito LED-rail switch: a 10k from the P-FET's gate to the supply,
        // an N-FET pulling the gate down, the P-FET's drain on the supply and
        // its source on the load. With the N-FET off the gate sits at the
        // supply but the body diode still feeds the load.
        const EN: i64 = 12;
        let mut driver = nfet(GND, GATE, EN);
        driver.reference = "Q2".into();
        let pull_up = part("R1", "10k", "Device:R", &[("1", V5), ("2", GATE)]);
        let found = lint(
            vec![
                header(V5),
                pfet(SW, V5, GATE),
                driver,
                pull_up,
                mcu(EN),
                load(SW),
            ],
            &[(EN, "EN")],
        );
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].refs, ["Q1"]);
        assert!(
            found[0].message.contains("switched by Q2 pin 3")
                && found[0].message.contains("cannot turn it off"),
            "{}",
            found[0].message
        );
    }

    /// A power-path or charger controller: a gate-driver output on `GATE`
    /// plus a battery-sense pin on `bat` and an output-sense pin on `out`.
    fn controller(bat: Option<i64>, out: Option<i64>) -> Component {
        let mut gate = pin_fn("21", GATE, "BGATE");
        gate.kind = "output".into();
        let mut pins = vec![gate];
        pins.extend(bat.map(|n| pin_fn("20", n, "BAT")));
        pins.extend(out.map(|n| pin_fn("22", n, "CSN")));
        Component {
            reference: "U3".into(),
            value: "PowerPathCtl".into(),
            lib_id: "Power:PowerPathCtl".into(),
            footprint: "X".into(),
            pins,
            ..Default::default()
        }
    }

    #[test]
    fn a_power_path_controller_that_senses_both_sides_of_its_fet_is_left_alone() {
        // The LTC4020 shape: the PowerPath P-FET's drain is the battery, its
        // source the converter output, and the controller drives the gate and
        // senses both. The body diode carrying the battery to the output is the
        // vendor topology, so nothing is reported. The output sense pin sits
        // behind a 100 ohm filter resistor, as the datasheet draws it.
        const SNS: i64 = 13;
        let board = |ctl: Component| {
            let filter = part("R9", "100", "Device:R", &[("1", SW), ("2", SNS)]);
            lint(
                vec![header(V5), pfet(SW, V5, GATE), ctl, load(SW), filter],
                &[(SNS, "SNS")],
            )
        };
        let found = board(controller(Some(V5), Some(SNS)));
        assert!(found.is_empty(), "{found:#?}");
        // The same gate driver with no pin on the load side is a plain switch.
        let found = board(controller(Some(V5), None));
        assert_eq!(found.len(), 1, "{found:#?}");
        assert_eq!(found[0].refs, ["Q1"]);
    }

    #[test]
    fn a_correctly_oriented_or_undriven_or_tied_fet_is_silent() {
        // Source on the supply, drain on the load: the diode points back at the supply.
        let ok = lint(
            vec![header(V5), pfet(V5, SW, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert!(ok.is_empty(), "{ok:#?}");
        // Reversed wiring, but nothing drives the gate and no connector
        // touches the source: not a switch and not protection.
        let undriven = lint(
            vec![
                header(V5),
                pfet(SW, V5, GATE),
                part("R1", "100k", "Device:R", &[("1", GATE), ("2", V5)]),
                load(SW),
            ],
            &[],
        );
        assert!(undriven.is_empty(), "{undriven:#?}");
        // A power path between two sources shows equal evidence on both
        // sides: the check abstains.
        let two_sources = lint(
            vec![
                header(V5),
                part(
                    "J2",
                    "Conn",
                    "Connector:Conn_01x02",
                    &[("1", SW), ("2", GND)],
                ),
                pfet(SW, V5, GATE),
                mcu(GATE),
            ],
            &[],
        );
        assert!(two_sources.is_empty(), "{two_sources:#?}");
    }

    #[test]
    fn a_driven_nfet_high_side_with_source_on_the_supply_is_flagged_and_the_good_one_is_not() {
        let bad = lint(
            vec![header(V5), nfet(V5, SW, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert_eq!(bad.len(), 1, "{bad:#?}");
        let good = lint(
            vec![header(V5), nfet(SW, V5, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert!(good.is_empty(), "{good:#?}");
        // A low-side N-FET (source on ground) is the ordinary orientation.
        let low = lint(
            vec![header(V5), nfet(GND, SW, GATE), mcu(GATE), load(SW)],
            &[],
        );
        assert!(low.is_empty(), "{low:#?}");
    }

    const MIDPT: i64 = 80;

    /// The second FET of a pair, as `Q2`.
    fn second(mut q: Component) -> Component {
        q.reference = "Q2".into();
        q
    }

    #[test]
    fn a_back_to_back_pair_is_silent_and_the_lone_reversed_fet_is_still_flagged() {
        let nets = [(MIDPT, "Net-(Q1-D)")];
        // N-FETs, drains joined: the source of Q1 faces the supply, the source
        // of Q2 faces the load, so the body diodes oppose in series.
        let pair = vec![
            header(V5),
            nfet(V5, MIDPT, GATE),
            second(nfet(SW, MIDPT, GATE)),
            mcu(GATE),
            load(SW),
        ];
        let found = lint(pair, &nets);
        assert!(found.is_empty(), "common-drain pair: {found:#?}");
        // P-FETs, sources joined.
        let pair = vec![
            header(V5),
            pfet(MIDPT, V5, GATE),
            second(pfet(MIDPT, SW, GATE)),
            mcu(GATE),
            load(SW),
        ];
        let found = lint(pair, &nets);
        assert!(found.is_empty(), "common-source pair: {found:#?}");
        // The same Q1 alone, wired straight to the load, is reversed.
        let lone = vec![header(V5), nfet(V5, SW, GATE), mcu(GATE), load(SW)];
        let found = lint(lone, &nets);
        assert_eq!(found.len(), 1, "lone reversed FET: {found:#?}");
        assert_eq!(found[0].severity, Severity::High);
        // Two FETs in parallel (both sources on the supply) are not a series
        // pair: each is still reversed.
        let parallel = vec![
            header(V5),
            nfet(V5, SW, GATE),
            second(nfet(V5, SW, GATE)),
            mcu(GATE),
            load(SW),
        ];
        let found = lint(parallel, &nets);
        assert_eq!(found.len(), 2, "parallel FETs: {found:#?}");
        // A partner whose drain is on ground does not make ground a midpoint.
        let on_ground = vec![
            header(V5),
            nfet(SW, GND, GATE),
            second(nfet(V5, GND, GATE)),
            mcu(GATE),
            load(SW),
        ];
        let found = lint(on_ground, &nets);
        assert!(!found.is_empty(), "drains on ground: {found:#?}");
    }

    #[test]
    fn a_low_side_nfet_with_drain_on_ground_clamps_its_load() {
        let bad = lint(vec![nfet(SW, GND, GATE), mcu(GATE)], &[]);
        assert_eq!(bad.len(), 1, "{bad:#?}");
        assert_eq!(bad[0].severity, Severity::Medium);
    }

    const VIN: i64 = 20;
    const VFUSED: i64 = 21;
    const VPROT: i64 = 22;
    const RPP_GATE: i64 = 23;
    const RAILS: &[(i64, &str)] = &[
        (VIN, "VIN"),
        (VFUSED, "VIN_FUSED"),
        (VPROT, "VPROT"),
        (RPP_GATE, "RPPG"),
    ];

    fn rpp(s: i64, d: i64) -> Vec<Component> {
        vec![
            part(
                "J1",
                "Conn",
                "Connector:Conn_01x02",
                &[("1", VIN), ("2", GND)],
            ),
            part("F1", "1A", "Device:Fuse", &[("1", VIN), ("2", VFUSED)]),
            pfet(s, d, RPP_GATE),
            part("R1", "100k", "Device:R", &[("1", RPP_GATE), ("2", GND)]),
            part("R2", "1k", "Device:R", &[("1", VPROT), ("2", GND)]),
        ]
    }

    #[test]
    fn a_reverse_polarity_pfet_with_drain_and_source_swapped_behind_a_fuse_is_flagged() {
        let found = lint(rpp(VFUSED, VPROT), RAILS);
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].message.contains("always-on FET"),
            "{}",
            found[0].message
        );
        assert_eq!(found[0].refs, ["Q1"]);
    }

    #[test]
    fn a_correct_reverse_polarity_pfet_behind_a_fuse_is_silent() {
        let found = lint(rpp(VPROT, VFUSED), RAILS);
        assert!(found.is_empty(), "{found:#?}");
    }

    const OTHER: i64 = 30;
    const SWNODE: i64 = 31;

    /// The shapes that read as reversed but are not: each stays silent, and
    /// the same wiring without the extra part is still flagged.
    #[test]
    fn converter_stages_or_ing_level_shifters_and_sense_returns_are_silent() {
        let extra: &[(i64, &str)] = &[(OTHER, "VBUS_B"), (SWNODE, "SWNODE")];
        let reversed = vec![header(V5), pfet(SW, V5, GATE), mcu(GATE), load(SW)];
        assert_eq!(lint(reversed.clone(), extra).len(), 1);
        // An inductor on a channel net: a converter half-bridge.
        let mut stage = reversed.clone();
        stage.push(part("L1", "10u", "Device:L", &[("1", SW), ("2", SWNODE)]));
        let found = lint(stage, extra);
        assert!(found.is_empty(), "inductor: {found:#?}");
        // A second source feeds the load net through a diode: power-path OR-ing.
        let mut or_ing = reversed;
        or_ing.push(part(
            "J2",
            "Conn",
            "Connector:Conn_01x02",
            &[("1", OTHER), ("2", GND)],
        ));
        or_ing.push(part("D1", "SS14", "Device:D", &[("1", OTHER), ("2", SW)]));
        let found = lint(or_ing, extra);
        assert!(found.is_empty(), "diode OR: {found:#?}");
        // A gate held by a regulator's power output is tied, not switched.
        let mut tied = vec![header(V5), pfet(SW, V5, GATE), load(SW)];
        let mut reg = part("U4", "LDO", "Regulator:LDO", &[("1", GATE)]);
        reg.pins[0].kind = "power_output".into();
        tied.push(reg);
        let found = lint(tied, extra);
        assert!(found.is_empty(), "tied gate: {found:#?}");
        // A sense resistor from ground to a supply does not make ground the
        // supply side of a low-side N-FET.
        let mut low = vec![header(V5), nfet(GND, SW, GATE), mcu(GATE), load(SW)];
        low.push(part("R9", "0.01", "Device:R", &[("1", GND), ("2", V5)]));
        let found = lint(low, extra);
        assert!(found.is_empty(), "sense return: {found:#?}");
    }

    const NA: i64 = 40;
    const NB: i64 = 41;
    const NLOAD: i64 = 42;
    const NGATE: i64 = 43;
    const UNNAMED: &[(i64, &str)] = &[
        (NA, "Net-(J1-Pin_1)"),
        (NB, "Net-(J1-Pin_2)"),
        (NLOAD, "Net-(Q1-D)"),
        (NGATE, "Net-(Q1-G)"),
    ];

    /// A battery board with no named net at all: the supply part `src` on
    /// `Net-(J1-Pin_1)` / `Net-(J1-Pin_2)`, a P-FET from it to the load, its
    /// gate pulled to the supply's return through 100k.
    fn unnamed_battery_board(src: Component, s: i64, d: i64) -> Vec<LintFinding> {
        let parts = vec![
            src,
            pfet(s, d, NGATE),
            part("R1", "100k", "Device:R", &[("1", NGATE), ("2", NB)]),
            part("R2", "1k", "Device:R", &[("1", NLOAD), ("2", NB)]),
        ];
        let b = board("t", UNNAMED, parts);
        fet_orientation_lint(&b, &ModelLibrary::builtin()).findings
    }

    #[test]
    fn a_reversed_protection_fet_on_an_unnamed_battery_net_is_flagged_and_the_right_one_is_not() {
        let connector = || {
            part(
                "J1",
                "Conn_01x02",
                "Connector:Conn_01x02",
                &[("1", NA), ("2", NB)],
            )
        };
        let cell = || {
            part(
                "BT1",
                "Battery_Cell",
                "Device:Battery_Cell",
                &[("1", NA), ("2", NB)],
            )
        };
        for (what, src) in [("connector", connector()), ("battery part", cell())] {
            let bad = unnamed_battery_board(src.clone(), NA, NLOAD);
            assert_eq!(bad.len(), 1, "{what}: {bad:#?}");
            assert!(
                bad[0].message.contains("always-on FET")
                    && bad[0].message.contains("Net-(J1-Pin_1)"),
                "{what}: {}",
                bad[0].message
            );
            let good = unnamed_battery_board(src, NLOAD, NA);
            assert!(good.is_empty(), "{what}: {good:#?}");
        }
    }

    #[test]
    fn a_ground_referenced_signal_switch_is_not_read_through_a_signal_header() {
        // A BSS138 boot-select switch: source on ground, drain to a signal,
        // gate through 1k to the reset net, which a 4-pin debug header also
        // carries. The header's other pins are signals, not a supply return.
        const RESET: i64 = 50;
        const BOOT: i64 = 51;
        const SPARE: i64 = 52;
        let nets: &[(i64, &str)] = &[
            (GND, "GND"),
            (GATE, "Net-(Q8-G)"),
            (SW, "Net-(Q8-D)"),
            (RESET, "/MCU/~RESET"),
            (BOOT, "/MCU/BOOT0"),
            (SPARE, "/MCU/SWDIO"),
        ];
        let parts = vec![
            nfet(GND, SW, GATE),
            part("R1", "1k", "Device:R", &[("1", RESET), ("2", GATE)]),
            part("R2", "1k", "Device:R", &[("1", SW), ("2", BOOT)]),
            part(
                "J2",
                "Conn_01x04",
                "Connector:Conn_01x04",
                &[("1", RESET), ("2", SPARE), ("3", GND), ("4", GND)],
            ),
        ];
        let b = board("t", nets, parts);
        let found = fet_orientation_lint(&b, &ModelLibrary::builtin()).findings;
        assert!(found.is_empty(), "signal switch: {found:#?}");
    }

    #[test]
    fn a_resistor_across_the_channel_makes_it_a_soft_start_not_protection() {
        let mut parts = rpp(VFUSED, VPROT);
        parts.push(part("R9", "62", "Device:R", &[("1", VFUSED), ("2", VPROT)]));
        let found = lint(parts, RAILS);
        assert!(found.is_empty(), "{found:#?}");
    }

    const V12: i64 = 60;
    const LOAD_EN: i64 = 61;
    const DRAIN: i64 = 62;
    const LEVEL: i64 = 63;
    const LOAD_OUT: i64 = 64;
    const VIN_GATE: i64 = 65;
    const MID: i64 = 66;
    const CTRL: i64 = 67;
    const SHIFT_NETS: &[(i64, &str)] = &[
        (V12, "+12V"),
        (LOAD_EN, "LOAD_EN"),
        (DRAIN, "Net-(Q1-D)"),
        (LEVEL, "Net-(Q2-G)"),
        (LOAD_OUT, "LOAD_OUT"),
        (VIN_GATE, "Net-(Q3-G)"),
        (MID, "Net-(R7-Pad2)"),
        (CTRL, "Net-(Q4-G)"),
    ];

    /// A gate-drive level shifter: a low-side N-FET (source on the board
    /// ground, gate on an enable that another sheet drives, with a 100k to
    /// ground) pulls the gate node of a high-side P-FET load switch low
    /// through 100k, a pull-up and a cap holding that node at the rail.
    fn level_shifter() -> Vec<Component> {
        let mut q2 = fet("AO3401A", "Device:Q_PMOS_GSD", (V12, LOAD_OUT, LEVEL));
        q2.reference = "Q2".into();
        vec![
            part(
                "J1",
                "Conn_01x06",
                "Connector:Conn_01x06",
                &[
                    ("1", V12),
                    ("2", GND),
                    ("3", SW),
                    ("4", MID),
                    ("5", CTRL),
                    ("6", GND),
                ],
            ),
            nfet(GND, DRAIN, LOAD_EN),
            part("R1", "100k", "Device:R", &[("1", LOAD_EN), ("2", GND)]),
            part("R2", "100k", "Device:R", &[("1", DRAIN), ("2", LEVEL)]),
            part("R3", "100k", "Device:R", &[("1", LEVEL), ("2", V12)]),
            part("C1", "100n", "Device:C", &[("1", LEVEL), ("2", V12)]),
            q2,
            part("R4", "1k", "Device:R", &[("1", LOAD_OUT), ("2", GND)]),
        ]
    }

    #[test]
    fn a_low_side_gate_drive_level_shifter_is_not_reverse_polarity_protection() {
        let found = lint(level_shifter(), SHIFT_NETS);
        assert!(found.is_empty(), "level shifter: {found:#?}");
    }

    /// Each of the two reasons the level shifter was misread (a gate held to
    /// its own source, a source on ground) is enough on its own to keep the
    /// check silent, and the same wiring with the reason removed is still the
    /// reversed protection FET it looked like.
    #[test]
    fn held_off_gates_and_ground_sources_each_abstain_alone() {
        let nets = [SHIFT_NETS, RAILS].concat();
        let rpp_with = |gate: i64, bias_to: i64| {
            let mut parts = rpp(VFUSED, VPROT);
            parts.retain(|p| p.reference != "R1");
            parts[2] = pfet(VFUSED, VPROT, gate);
            parts.push(part(
                "R1",
                "100k",
                "Device:R",
                &[("1", gate), ("2", bias_to)],
            ));
            parts
        };
        // Control: an auto-named gate pulled to ground is the protection FET.
        let found = lint(rpp_with(VIN_GATE, GND), &nets);
        assert_eq!(found.len(), 1, "{found:#?}");
        // (a) The same gate pulled to the FET's own source is held off.
        let found = lint(rpp_with(VIN_GATE, VFUSED), &nets);
        assert!(found.is_empty(), "held off: {found:#?}");
        // A hand-named gate net is still the protection FET: a name is not
        // evidence that something drives the gate.
        let found = lint(rpp_with(LOAD_EN, GND), &nets);
        assert_eq!(found.len(), 1, "named gate: {found:#?}");
        // (c) A source on ground behind an auto-named two-resistor bias chain
        // is not a supply input, however many headers share ground.
        let low = |source: i64| {
            vec![
                header(V5),
                nfet(source, DRAIN, CTRL),
                part("R5", "100k", "Device:R", &[("1", CTRL), ("2", MID)]),
                part("R6", "100k", "Device:R", &[("1", MID), ("2", GND)]),
                part("R8", "100k", "Device:R", &[("1", DRAIN), ("2", GND)]),
            ]
        };
        let found = lint(low(GND), &nets);
        assert!(found.is_empty(), "ground source: {found:#?}");
        let found = lint(low(V5), &nets);
        assert_eq!(found.len(), 1, "source on the connector rail: {found:#?}");
    }

    const CELL: i64 = 70;
    const CELL_GATE: i64 = 71;
    const VCC: i64 = 72;
    const CELL_NETS: &[(i64, &str)] = &[
        (CELL, "Net-(X1-Pad1)"),
        (CELL_GATE, "Net-(Q1-G)"),
        (VCC, "VCC"),
    ];

    /// A protection P-FET between `Net-(X1-Pad1)` (the 2-pad part `x1`) and the
    /// named `VCC` rail, its gate to ground through 10k; no connector anywhere.
    fn cell_board(x1: Component, (s, d): (i64, i64)) -> Vec<LintFinding> {
        lint(
            vec![
                x1,
                pfet(s, d, CELL_GATE),
                part("R1", "10k", "Device:R", &[("1", CELL_GATE), ("2", GND)]),
                part("R2", "1k", "Device:R", &[("1", VCC), ("2", GND)]),
            ],
            CELL_NETS,
        )
    }

    fn two_pad(reference: &str, value: &str, lib_id: &str, footprint: &str) -> Component {
        let mut c = part(reference, value, lib_id, &[("1", CELL), ("2", GND)]);
        c.footprint = footprint.into();
        c
    }

    #[test]
    fn a_battery_holder_named_by_value_or_footprint_is_a_source_wherever_its_net_goes() {
        for (what, x1) in [
            (
                "value BAT",
                two_pad("X1", "BAT", "Device:Generic", "Keystone_3034"),
            ),
            (
                "value CR2032",
                two_pad("X1", "CR2032", "Device:Generic", "Holder_THT"),
            ),
            (
                "footprint",
                two_pad("X1", "Cell", "Device:Generic", "Battery_Clip_2xAAA"),
            ),
            (
                "coin-cell footprint",
                two_pad("X1", "Hold", "Device:Generic", "Keystone_CoinCell_1"),
            ),
        ] {
            assert!(is_battery_part(&x1), "{what} should read as a cell");
            let bad = cell_board(x1.clone(), (CELL, VCC));
            assert_eq!(bad.len(), 1, "{what}: {bad:#?}");
            assert!(bad[0].message.contains("always-on FET"), "{what}");
            let good = cell_board(x1, (VCC, CELL));
            assert!(good.is_empty(), "{what}: {good:#?}");
        }
    }

    #[test]
    fn a_part_that_merely_has_bat_in_its_name_is_not_a_cell() {
        let mut schottky = part(
            "D1",
            "BAT54",
            "Device:D_Schottky",
            &[("1", CELL), ("2", GND)],
        );
        schottky.footprint = "SOT-23".into();
        assert!(!is_battery_part(&schottky));
        let reg = two_pad("U1", "BAT", "Device:Generic", "SOT-23");
        assert!(!is_battery_part(&reg), "an IC is not a cell");
        let named_bat54 = two_pad("X1", "BAT54", "Device:Generic", "SOD-323");
        assert!(!is_battery_part(&named_bat54), "BAT54 is a diode, not BAT");
    }

    #[test]
    fn the_unnamed_input_side_of_a_gate_to_ground_pfet_is_not_read_as_the_load() {
        // Nothing here is recognisable as a source: a generic 2-pad part on an
        // auto-named net, a named VCC rail on the other side. The input is
        // the side that is not the named rail.
        let module = || two_pad("X1", "Module", "Device:Generic", "Foo");
        let bad = cell_board(module(), (CELL, VCC));
        assert_eq!(bad.len(), 1, "{bad:#?}");
        assert!(
            bad[0].message.contains("unnamed input net"),
            "{}",
            bad[0].message
        );
        // Drain on the input side is the right way round.
        let good = cell_board(module(), (VCC, CELL));
        assert!(good.is_empty(), "{good:#?}");
        // The auto-named side reaching only a passive (a pull-down resistor)
        // says nothing about being an input: silent either way.
        for (s, d) in [(CELL, VCC), (VCC, CELL)] {
            let passive = cell_board(
                part("R9", "1k", "Device:R", &[("1", CELL), ("2", GND)]),
                (s, d),
            );
            assert!(passive.is_empty(), "passive only: {passive:#?}");
        }
    }

    #[test]
    fn the_named_rail_fallback_abstains_when_the_other_side_is_an_unnamed_two_pin_part() {
        let driven = |x1: Option<Component>, (s, d): (i64, i64)| {
            let mut parts = vec![pfet(s, d, GATE), mcu(GATE)];
            parts.push(
                x1.unwrap_or_else(|| part("R9", "1k", "Device:R", &[("1", CELL), ("2", GND)])),
            );
            parts.push(part("R2", "1k", "Device:R", &[("1", VCC), ("2", GND)]));
            lint(parts, CELL_NETS)
        };
        // Control: the auto-named side carries only a resistor, so the named
        // rail is still the supply and a drain on it cannot switch off.
        let found = driven(None, (CELL, VCC));
        assert_eq!(found.len(), 1, "{found:#?}");
        // The auto-named side reaches an unrecognised 2-pad part: abstain.
        let module = two_pad("X1", "Module", "Device:Generic", "Foo");
        let found = driven(Some(module), (CELL, VCC));
        assert!(found.is_empty(), "{found:#?}");
    }
}
