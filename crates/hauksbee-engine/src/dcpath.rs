//! Is a net's DC level defined by the modeled circuit, or a numerical
//! artifact?
//!
//! The solver assigns every node a voltage, including a node nothing drives:
//! GMIN pins an isolated node near zero, and 0.000 V then looks exactly like
//! a healthy logic low. That number is a convention, not a claim about the
//! board. This module answers, from the built [`Circuit`] alone, whether a
//! node's DC operating point is entitled to be read as evidence:
//!
//! - [`NetDcDefinition::Floating`]: no device with DC conductance touches
//!   the node (nothing at all, or only capacitors). Whatever the solver
//!   prints, the modeled board does not define this level. A voltage
//!   assertion here must not pass numerically; the honest verdict is "the
//!   net floats: add a pull or a model for the parts that would drive it".
//! - [`NetDcDefinition::Defined`]: a chain of resistors/inductors reaches
//!   ground or an independent source, so the level stands on modeled
//!   elements even if every unmodelled (open) part is high-impedance. Open
//!   parts on such a net downgrade from verdict-blocking to a stated caveat
//!   ("holds unless an unmodelled part actively drives the net") via
//!   [`hauksbee_ir::evidence::EvidenceMap::assuming_open_parts_high_impedance`].
//! - [`NetDcDefinition::Indeterminate`]: devices touch the node but no
//!   passive DC path reaches a reference (an island of resistors, an
//!   active-only neighborhood). No entitlement either way; callers keep the
//!   default undermined semantics.
//!
//! The distinction is what lets one user assertion discriminate a real
//! defect pair: "SWCLK stays low" on a board whose pull-down is missing is
//! Floating (red, traced to the absent resistor), and on the fixed board is
//! Defined through that resistor (green, with the open-MCU caveat stated).
//! Same stated assumption set, opposite verdicts, no new lint rule.

use hauksbee_ir::{Circuit, Device, NodeId};
use std::collections::{HashMap, HashSet, VecDeque};

/// How a node's DC level relates to the modeled circuit. See the module doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetDcDefinition {
    /// Nothing with DC conductance touches the node.
    Floating,
    /// The node is itself a terminal of an independent source (or ground).
    /// Its level comes from the harness, not from board topology, so an
    /// assertion here tests the source setting; open parts beside it stay
    /// fully critical (their hidden load is exactly what is untested).
    DefinedBySource,
    /// A passive DC path THROUGH at least one modeled board element reaches
    /// ground or an independent source; `via` names the first element for
    /// the human explanation. The assertion genuinely tests board topology
    /// (remove the element and the verdict flips), which is what entitles
    /// downgrading open parts to a stated caveat.
    DefinedThroughBoard { via: String },
    /// DC conductance exists but never reaches a reference.
    Indeterminate,
}

/// Classify one node. Pure function of the circuit; the caller maps net
/// names to [`NodeId`]s (`BoundBoard::net_nodes`).
pub fn net_dc_definition(circuit: &Circuit, start: NodeId) -> NetDcDefinition {
    // Reference nodes: ground, plus every terminal of an independent source.
    // A source terminal defines its node's level (or ties it, through the
    // source, to a defined partner), which is exactly what "the level stands
    // on modeled elements" means here.
    let mut reference: HashSet<NodeId> = HashSet::new();
    reference.insert(NodeId::GROUND);
    // Passive DC edges: resistors and inductors conduct at DC. Capacitors do
    // not; every other device is deliberately NOT an edge (a transistor
    // channel's conductance depends on operating point, which is the very
    // thing in question).
    let mut edges: HashMap<NodeId, Vec<(NodeId, &str)>> = HashMap::new();
    let mut has_dc_device: HashSet<NodeId> = HashSet::new();
    let mut has_any_noncap_device: HashSet<NodeId> = HashSet::new();
    for device in &circuit.devices {
        match device {
            Device::Resistor { name, a, b, .. } | Device::Inductor { name, a, b, .. } => {
                edges.entry(*a).or_default().push((*b, name));
                edges.entry(*b).or_default().push((*a, name));
                has_dc_device.insert(*a);
                has_dc_device.insert(*b);
                has_any_noncap_device.insert(*a);
                has_any_noncap_device.insert(*b);
            }
            Device::Capacitor { .. } => {}
            Device::Vsource { p, n, .. } | Device::Isource { p, n, .. } => {
                reference.insert(*p);
                reference.insert(*n);
                has_any_noncap_device.insert(*p);
                has_any_noncap_device.insert(*n);
            }
            other => {
                for node in device_nodes(other) {
                    has_any_noncap_device.insert(node);
                }
            }
        }
    }

    if reference.contains(&start) {
        return NetDcDefinition::DefinedBySource;
    }
    if !has_dc_device.contains(&start) && !has_any_noncap_device.contains(&start) {
        return NetDcDefinition::Floating;
    }

    // BFS over the passive DC edges toward any reference node.
    let mut seen: HashSet<NodeId> = HashSet::from([start]);
    let mut queue: VecDeque<(NodeId, Option<&str>)> = VecDeque::from([(start, None)]);
    while let Some((node, first_hop)) = queue.pop_front() {
        let Some(neighbors) = edges.get(&node) else {
            continue;
        };
        for (next, name) in neighbors {
            let hop = first_hop.or(Some(*name));
            if reference.contains(next) {
                return NetDcDefinition::DefinedThroughBoard {
                    via: hop.unwrap_or(name).to_string(),
                };
            }
            if seen.insert(*next) {
                queue.push_back((*next, hop));
            }
        }
    }

    // DC conductance without a reference, or only non-passive devices: no
    // entitlement either way.
    if has_dc_device.contains(&start) {
        NetDcDefinition::Indeterminate
    } else if has_any_noncap_device.contains(&start) {
        NetDcDefinition::Indeterminate
    } else {
        NetDcDefinition::Floating
    }
}

/// Every node a device touches, for the "something non-capacitive is here"
/// bookkeeping of device kinds that are not DC edges.
fn device_nodes(device: &Device) -> Vec<NodeId> {
    match device {
        Device::Resistor { a, b, .. }
        | Device::Capacitor { a, b, .. }
        | Device::Inductor { a, b, .. } => vec![*a, *b],
        Device::Vsource { p, n, .. } | Device::Isource { p, n, .. } => vec![*p, *n],
        Device::Diode { a, k, .. } => vec![*a, *k],
        Device::Bjt { c, b, e, .. } => vec![*c, *b, *e],
        Device::Mosfet { d, g, s, b, .. } => {
            let mut nodes = vec![*d, *g, *s];
            if let Some(b) = b {
                nodes.push(*b);
            }
            nodes
        }
        Device::VSwitch {
            a, b, ctrl_p, ctrl_n, ..
        } => vec![*a, *b, *ctrl_p, *ctrl_n],
        other => other_nodes_conservative(other),
    }
}

/// Fallback for device variants added after this module: read no nodes,
/// which errs toward `Floating`, the direction that REFUSES a numeric pass
/// rather than inventing one.
fn other_nodes_conservative(_device: &Device) -> Vec<NodeId> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn circuit() -> Circuit {
        Circuit::new()
    }

    #[test]
    fn untouched_node_is_floating() {
        let mut c = circuit();
        let swclk = c.node("SWCLK");
        // A capacitor alone provides no DC path: still floating at DC.
        let gnd = NodeId::GROUND;
        c.devices.push(Device::Capacitor {
            name: "C1".into(),
            a: swclk,
            b: gnd,
            farads: 22e-12,
            ic: None,
        });
        assert_eq!(net_dc_definition(&c, swclk), NetDcDefinition::Floating);
    }

    #[test]
    fn pulldown_to_ground_defines() {
        let mut c = circuit();
        let swclk = c.node("SWCLK");
        c.devices.push(Device::Resistor {
            name: "R33".into(),
            a: swclk,
            b: NodeId::GROUND,
            ohms: 100_000.0,
            tc1: None,
        });
        assert_eq!(
            net_dc_definition(&c, swclk),
            NetDcDefinition::DefinedThroughBoard { via: "R33".into() }
        );
    }

    #[test]
    fn chain_through_damper_to_source_defines() {
        let mut c = circuit();
        let a = c.node("SWCLK");
        let b = c.node("SWCLK_MCU");
        let rail = c.node("+3V3");
        c.devices.push(Device::Resistor {
            name: "R5".into(),
            a,
            b,
            ohms: 22.0,
            tc1: None,
        });
        c.devices.push(Device::Resistor {
            name: "R9".into(),
            a: b,
            b: rail,
            ohms: 47_000.0,
            tc1: None,
        });
        c.devices.push(Device::Vsource {
            name: "V3V3".into(),
            p: rail,
            n: NodeId::GROUND,
            kind: hauksbee_ir::SourceKind::Dc(3.3),
        });
        assert_eq!(
            net_dc_definition(&c, a),
            NetDcDefinition::DefinedThroughBoard { via: "R5".into() }
        );
    }

    #[test]
    fn resistor_island_is_indeterminate() {
        let mut c = circuit();
        let a = c.node("A");
        let b = c.node("B");
        c.devices.push(Device::Resistor {
            name: "R1".into(),
            a,
            b,
            ohms: 10_000.0,
            tc1: None,
        });
        assert_eq!(net_dc_definition(&c, a), NetDcDefinition::Indeterminate);
    }
}
