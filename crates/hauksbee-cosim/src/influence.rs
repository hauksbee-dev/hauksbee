//! Which nodes firmware can move without an MCU pin driving them.
//!
//! A directed walk from the nets the firmware drove. Each circuit device
//! carries influence from every terminal it touches to the terminals its stamp
//! drives current through (`Device::conduction_nodes`): a gate or base into a
//! channel, a switch's control pair into its contacts, a passive end to end, a
//! controlled source's control pair into its output port. An op-amp or
//! comparator drives only its output: its supply pins carry that output's
//! current, which is a load on the rail, not a way to move it. Independent
//! sources are not paths (a voltage source holds its terminals, a current
//! source forces its current whatever is wired to it), and a
//! current-controlled current source is a supply-current mirror (a regulator's
//! pass draw, a supply leg), not a control path.
//!
//! Parts the scheduler evaluates outside the circuit add their own edges
//! ([`Influence::link`]), and an edge a control pin owns on the real board
//! may enter a node something else holds in the simulation
//! ([`Influence::force`]): a regulator's enable switches the rail its ideal
//! source holds.
//!
//! The walk never enters a node in `stops` except through a forced edge, and
//! never enters ground: the nodes an independent source holds stay where it
//! holds them whatever a firmware-driven net does beside them.

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};
use std::collections::{HashMap, HashSet, VecDeque};

/// The directed influence graph over one circuit, see the module docs.
pub struct Influence {
    stops: HashSet<NodeId>,
    /// `from -> [(to, forced)]`.
    next: HashMap<NodeId, Vec<(NodeId, bool)>>,
}

/// The terminals `dev` moves when any terminal it touches moves, or `None` when
/// it is not a path at all.
fn driven_terminals(dev: &Device) -> Option<Vec<NodeId>> {
    match dev {
        Device::Vsource { .. } | Device::Isource { .. } | Device::Cccs { .. } => None,
        Device::OpAmp { out, .. } | Device::Comparator { out, .. } => Some(vec![*out]),
        _ => Some(dev.conduction_nodes()),
    }
}

impl Influence {
    /// The graph of `circuit`'s own devices, leaving out `skip` (stamp
    /// helpers that read a branch rather than a node), with `stops` held.
    #[must_use]
    pub fn new(circuit: &Circuit, skip: &[DeviceId], stops: &[NodeId]) -> Self {
        let mut graph = Self {
            stops: stops.iter().copied().collect(),
            next: HashMap::new(),
        };
        for (id, dev) in circuit.iter() {
            if skip.contains(&id) {
                continue;
            }
            if let Device::Coupling { l1, l2, .. } = dev {
                // Mutual flux: either winding moves the other.
                let winding = |w: &DeviceId| {
                    circuit
                        .devices
                        .get(w.0 as usize)
                        .map(Device::nodes)
                        .unwrap_or_default()
                };
                let (a, b) = (winding(l1), winding(l2));
                for (from, to) in [(&a, &b), (&b, &a)] {
                    for x in from {
                        for y in to {
                            graph.link(*x, *y);
                        }
                    }
                }
                continue;
            }
            let Some(driven) = driven_terminals(dev) else {
                continue;
            };
            for from in dev.nodes() {
                for to in &driven {
                    graph.link(from, *to);
                }
            }
        }
        graph
    }

    /// `to` moves when `from` does, unless `to` is held.
    pub fn link(&mut self, from: NodeId, to: NodeId) {
        if from != to {
            self.next.entry(from).or_default().push((to, false));
        }
    }

    /// `to` moves when `from` does, even if the simulation holds `to` (a
    /// control pin the stamp does not read, switching the rail it holds).
    pub fn force(&mut self, from: NodeId, to: NodeId) {
        if from != to {
            self.next.entry(from).or_default().push((to, true));
        }
    }

    /// Every node reached from `seeds`, each paired with the seed fewest edges
    /// away (the earlier one in `seeds` on a tie). The seeds themselves and
    /// ground are not listed.
    #[must_use]
    pub fn reach(&self, seeds: &[NodeId]) -> Vec<(NodeId, NodeId)> {
        let mut origin: HashMap<NodeId, NodeId> = seeds.iter().map(|s| (*s, *s)).collect();
        let mut queue: VecDeque<NodeId> = seeds.iter().copied().collect();
        let mut out = Vec::new();
        while let Some(at) = queue.pop_front() {
            let from = origin[&at];
            for &(to, forced) in self.next.get(&at).into_iter().flatten() {
                let held = !forced && self.stops.contains(&to);
                if to.is_ground() || held || origin.contains_key(&to) {
                    continue;
                }
                origin.insert(to, from);
                out.push((to, from));
                queue.push_back(to);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::SourceKind;

    #[test]
    fn a_gate_reaches_the_channel_but_a_held_rail_stops_the_walk() {
        let mut c = Circuit::new();
        let [gpio, gate, load, rail] = ["GPIO", "GATE", "LOAD", "RAIL"].map(|n| c.node(n));
        c.add(Device::vsource(
            "V1",
            rail,
            NodeId::GROUND,
            SourceKind::Dc(5.0),
        ));
        c.add(Device::resistor("R1", gpio, gate, 100.0));
        c.add(Device::resistor("Rpu", gate, rail, 10e3));
        c.add(Device::resistor("Rload", rail, load, 100.0));
        let nmos = hauksbee_ir::MosfetModel::default();
        c.add(Device::Mosfet {
            name: "Q1".into(),
            d: load,
            g: gate,
            s: NodeId::GROUND,
            b: None,
            model: nmos,
        });
        let walk = Influence::new(&c, &[], &[NodeId::GROUND, rail]);
        let reached = walk.reach(&[gpio]);
        assert_eq!(reached, vec![(gate, gpio), (load, gpio)]);
    }

    #[test]
    fn a_forced_edge_enters_a_held_node_and_continues_from_it() {
        let mut c = Circuit::new();
        let [en, out, led] = ["EN", "OUT", "LED"].map(|n| c.node(n));
        c.add(Device::vsource(
            "Vreg",
            out,
            NodeId::GROUND,
            SourceKind::Dc(3.3),
        ));
        c.add(Device::resistor("R1", out, led, 1e3));
        let mut walk = Influence::new(&c, &[], &[NodeId::GROUND, out]);
        assert!(walk.reach(&[en]).is_empty());
        walk.force(en, out);
        assert_eq!(walk.reach(&[en]), vec![(out, en), (led, en)]);
    }
}
