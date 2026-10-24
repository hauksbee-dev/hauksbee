//! FET gates whose level nothing in the DC netlist sets.
//!
//! A bound card stamps only the pins its behaviour needs. A pin it leaves
//! out of `[models.pins]`, or lists with a role that carries no drive (an
//! identity-only row), lands on its net as nothing at all: no source, no
//! conductance, no clamp. A MOSFET whose gate net holds nothing but such pins
//! is then solved with its gate held by the solver's gmin alone, at 0 V, which
//! is a level the board never chose: a P-FET from a 30 V rail reads fully on
//! with 30 V across its gate, and the paths it closes read as near-shorts.
//! On the real board the pin drives the gate, to a level the model does not
//! give.
//!
//! [`undetermined_gates`] finds those gates from the bound circuit alone, so
//! it is the same rule for every part: walk the gate's DC cluster (the gate
//! node and every node a resistor or inductor joins it to); when no stamped
//! device sets any node of it (ground, a source, a junction, a channel, a
//! driven output) and a fitted part's pin that could drive lands on it, the
//! gate's level is undetermined, and those pins are what decide it.
//! [`hold_off`] holds each such FET off (gate tied to its source) for a pass
//! that judges ratings at rest, and drops its own ratings from the pass, so
//! neither the FET's gate-source rating nor any current path it would close
//! is judged at a level the board does not set; [`undetermined_gate_note`]
//! says so, naming the pins.

use std::collections::{BTreeSet, HashSet};

use hauksbee_ir::{BOutput, Device, DeviceId, NodeId};

use crate::bind_report::PartPin;
use crate::binder::BoundBoard;

/// The words every undetermined-gate note carries, so a surface can tell it
/// from other abstentions.
pub const UNDETERMINED_GATE_MARKER: &str = "so its gate level is undetermined";

/// The hold resistor's resistance: any value ties the gate to its source,
/// since nothing else on the cluster draws current; a kilohm keeps the
/// matrix well conditioned.
const HOLD_OHMS: f64 = 1.0e3;

/// One MOSFET whose gate level nothing in the DC netlist sets.
#[derive(Debug, Clone)]
pub struct UndeterminedGate {
    /// The FET's reference designator.
    pub reference: String,
    /// The FET's device in the bound circuit.
    pub device: DeviceId,
    /// The gate's net name.
    pub net: String,
    /// The pins of fitted parts on the gate's cluster that could drive it
    /// and to which the DC netlist gives no behaviour.
    pub pins: Vec<PartPin>,
}

/// The nodes `device` holds at a DC level or passes DC current through:
/// every terminal but the ones that only sense (a gate, a switch control, an
/// amplifier input, a controlled source's control pair) and a capacitor's.
fn dc_terminals(device: &Device) -> Vec<NodeId> {
    match device {
        Device::Capacitor { .. } => Vec::new(),
        Device::Mosfet { d, s, b, .. } => [*d, *s].into_iter().chain(*b).collect(),
        Device::VSwitch { a, b, .. } => vec![*a, *b],
        Device::OpAmp {
            out,
            reference,
            rail_lo,
            rail_hi,
            supply,
            ..
        } => {
            let mut nodes = vec![*out, rail_lo.pin, rail_hi.pin];
            nodes.extend(*reference);
            nodes.extend(supply.nodes());
            nodes
        }
        Device::Comparator { out, supply, .. } => {
            let mut nodes = vec![*out];
            nodes.extend(supply.nodes());
            nodes
        }
        Device::Vcvs { p, n, .. } | Device::Vccs { p, n, .. } | Device::Behavioral { p, n, .. } => {
            vec![*p, *n]
        }
        other => other.nodes(),
    }
}

/// Whether a device named `name` belongs to the part `reference` (its own
/// name, or a sub-device such as `U1:3` or `U1.A`).
fn of_part(name: &str, reference: &str) -> bool {
    name.strip_prefix(reference)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '.', '_', '#']))
}

/// The nodes something other than a resistor or inductor holds or loads at
/// DC, and each node's resistive links.
fn dc_graph(
    devices: &[Device],
) -> (
    HashSet<NodeId>,
    std::collections::HashMap<NodeId, Vec<NodeId>>,
) {
    let mut set: HashSet<NodeId> = HashSet::new();
    let mut links: std::collections::HashMap<NodeId, Vec<NodeId>> =
        std::collections::HashMap::new();
    for dev in devices {
        match dev {
            Device::Resistor { a, b, .. } | Device::Inductor { a, b, .. } => {
                links.entry(*a).or_default().push(*b);
                links.entry(*b).or_default().push(*a);
            }
            other => set.extend(dc_terminals(other)),
        }
    }
    (set, links)
}

/// The nodes of `bound` whose DC level the netlist holds.
///
/// Ground, or a node whose resistive cluster (every node a resistor, inductor
/// or switch joins it to) holds ground or a source, an amplifier or comparator
/// output, or a controlled voltage. The solver puts every other node wherever
/// gmin and leakage leave it, which is a level the board never chose: a judge
/// that reads such a node must abstain, never take the solved figure as a
/// measurement.
///
/// A source of current (an `Isource`, a current-output behavioural pass
/// element) holds no level either: it only forces a current into whatever the
/// node is wired to.
///
/// A junction (a diode, a transistor's channel or base) does not hold a level
/// on its own: a diode from a rail into a node with no load leaves the node
/// wherever its tiny forward current balances gmin, not at the rail less a
/// drop, so a junction neither sets a node nor joins two clusters here.
#[must_use]
pub fn dc_determined_nodes(bound: &BoundBoard) -> HashSet<NodeId> {
    let mut set: HashSet<NodeId> = HashSet::new();
    let mut links: std::collections::HashMap<NodeId, Vec<NodeId>> =
        std::collections::HashMap::new();
    for dev in &bound.circuit.devices {
        match dev {
            Device::Resistor { a, b, .. }
            | Device::Inductor { a, b, .. }
            | Device::VSwitch { a, b, .. } => {
                links.entry(*a).or_default().push(*b);
                links.entry(*b).or_default().push(*a);
            }
            Device::Vsource { p, n, .. }
            | Device::Vcvs { p, n, .. }
            | Device::Behavioral {
                p,
                n,
                output: BOutput::Voltage,
                ..
            } => set.extend([*p, *n]),
            Device::OpAmp { out, .. } | Device::Comparator { out, .. } => {
                set.insert(*out);
            }
            _ => {}
        }
    }
    bound
        .net_nodes
        .values()
        .copied()
        .filter(|&node| floating_cluster(node, &set, &links).is_none())
        .collect()
}

/// The MOSFETs on `bound` whose gate level nothing in its DC netlist sets
/// while a fitted part's pin that could drive it lands on the gate's cluster.
#[must_use]
pub fn undetermined_gates(bound: &BoundBoard) -> Vec<UndeterminedGate> {
    let devices = &bound.circuit.devices;
    let (set, links) = dc_graph(devices);
    let mut found = Vec::new();
    for (i, dev) in devices.iter().enumerate() {
        let Device::Mosfet { name, g, .. } = dev else {
            continue;
        };
        let Some(cluster) = floating_cluster(*g, &set, &links) else {
            continue;
        };
        // Parts with any device on the cluster are stamped there (a
        // capacitor, a resistor, another gate): they set nothing, and they
        // are not what drives it either.
        let stamped: Vec<&str> = (devices.iter())
            .filter(|d| d.nodes().iter().any(|n| cluster.contains(n)))
            .map(Device::name)
            .collect();
        let device = DeviceId(u32::try_from(i).unwrap_or(u32::MAX));
        let reference = (bound.device_meta.iter())
            .find(|m| m.device == device)
            .map_or_else(|| name.clone(), |m| m.reference.clone());
        let pins: Vec<PartPin> = (bound.report.part_pins.iter())
            .filter(|p| p.can_drive && cluster.contains(&p.node))
            .filter(|p| !stamped.iter().any(|s| of_part(s, &p.reference)))
            .cloned()
            .collect();
        if pins.is_empty() {
            continue;
        }
        let net = bound.circuit.node_name(*g).to_string();
        found.push(UndeterminedGate {
            reference,
            device,
            net,
            pins,
        });
    }
    found
}

/// The DC cluster of `gate` (every node a resistor or inductor joins it to)
/// when no node of it is ground or set by anything else; `None` otherwise.
fn floating_cluster(
    gate: NodeId,
    set: &HashSet<NodeId>,
    links: &std::collections::HashMap<NodeId, Vec<NodeId>>,
) -> Option<BTreeSet<NodeId>> {
    let mut cluster = BTreeSet::new();
    let mut todo = vec![gate];
    while let Some(n) = todo.pop() {
        if n.is_ground() || set.contains(&n) {
            return None;
        }
        if cluster.insert(n) {
            todo.extend(links.get(&n).into_iter().flatten().copied());
        }
    }
    Some(cluster)
}

/// Hold each of `gates` off for a pass judging ratings at rest.
///
/// Its gate is tied to its source through a hold resistor and its own
/// ratings are dropped from the pass, so nothing is judged at a gate level
/// the board does not set.
pub fn hold_off(bound: &mut BoundBoard, gates: &[UndeterminedGate]) {
    for gate in gates {
        let Some(Device::Mosfet { g, s, .. }) = bound.circuit.devices.get(gate.device.0 as usize)
        else {
            continue;
        };
        let (g, s) = (*g, *s);
        bound.circuit.add(Device::Resistor {
            name: format!("{}:gate-hold", gate.reference),
            a: g,
            b: s,
            ohms: HOLD_OHMS,
            tc1: None,
        });
        bound.device_meta.retain(|m| m.reference != gate.reference);
    }
}

/// One pin as the note names it: part, pad, its card's role for the pad, and
/// the card.
fn pin_phrase(pin: &PartPin) -> String {
    match (&pin.card, &pin.role) {
        (Some(card), Some(role)) => format!(
            "{} pin {} ('{role}' on card {card}, which stamps no behaviour for it)",
            pin.reference, pin.pin
        ),
        (Some(card), None) => format!(
            "{} pin {} (a pin card {card} leaves unmodelled)",
            pin.reference, pin.pin
        ),
        (None, _) => format!("{} pin {} (a part with no model)", pin.reference, pin.pin),
    }
}

/// What the ratings check says about `gate`: what drives it, that its level
/// is undetermined, and what it therefore held and did not judge. The caller
/// marks it as an abstention.
#[must_use]
pub fn undetermined_gate_note(gate: &UndeterminedGate) -> String {
    let pins: Vec<String> = gate.pins.iter().map(pin_phrase).collect();
    format!(
        "{}'s gate net '{}' carries nothing the DC model drives but {}, {UNDETERMINED_GATE_MARKER}: \
         the DC ratings check held {} off and abstained on its gate-source rating, its other \
         ratings and the current paths through it",
        gate.reference,
        gate.net,
        hauksbee_ir::vocabulary::list_summary(&pins, "; "),
        gate.reference,
    )
}
