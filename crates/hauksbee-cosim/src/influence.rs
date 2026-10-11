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
//!
//! One independent source is a wire here: a 0 V source between two non-ground
//! nodes (a current-sense ammeter in series with a feed) holds no node, it
//! carries the same potential across, so influence passes through it in both
//! directions and its terminals are released from `stops` unless something else
//! holds them (a real source, a declared supply). The regulation taint walk
//! that supplies `stops` keeps treating such a source as a stop: the two walks
//! ask different questions.

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, SourceKind};
use std::collections::{HashMap, HashSet, VecDeque};

/// The directed influence graph over one circuit, see the module docs.
pub struct Influence {
    stops: HashSet<NodeId>,
    /// `from -> [(to, forced)]`.
    next: HashMap<NodeId, Vec<(NodeId, bool)>>,
}

/// The two terminals of a 0 V series source, the marker the binder uses for an
/// ammeter (`Vport_<ref>`, a charger's `Vreg_<ref>`): a DC voltage source of
/// exactly 0 V with neither terminal on ground. One referenced to ground holds
/// its node at ground, which is a stop, not a wire.
fn series_ammeter(dev: &Device) -> Option<[NodeId; 2]> {
    match dev {
        Device::Vsource {
            p,
            n,
            kind: SourceKind::Dc(v),
            ..
        } if *v == 0.0 && p != n && !p.is_ground() && !n.is_ground() => Some([*p, *n]),
        _ => None,
    }
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
    ///
    /// `stops` is read as a multiset, one entry per reason a node is held (the
    /// regulation walk lists a node once for each source terminal on it): each
    /// 0 V series source releases one entry on each of its terminals, so a node
    /// only that ammeter held becomes enterable while one a real source or a
    /// declared supply also holds stays held.
    #[must_use]
    pub fn new(circuit: &Circuit, skip: &[DeviceId], stops: &[NodeId]) -> Self {
        let mut held: HashMap<NodeId, usize> = HashMap::new();
        for stop in stops {
            *held.entry(*stop).or_default() += 1;
        }
        let ammeters: Vec<[NodeId; 2]> = circuit
            .iter()
            .filter(|(id, _)| !skip.contains(id))
            .filter_map(|(_, dev)| series_ammeter(dev))
            .collect();
        for end in ammeters.iter().flatten() {
            if let Some(count) = held.get_mut(end) {
                *count = count.saturating_sub(1);
            }
        }
        let mut graph = Self {
            stops: held
                .into_iter()
                .filter_map(|(node, count)| (count > 0).then_some(node))
                .collect(),
            next: HashMap::new(),
        };
        for [p, n] in ammeters {
            graph.link(p, n);
            graph.link(n, p);
        }
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

    /// The stop set the regulation taint walk would hand over for `c`.
    fn walk_stops(c: &Circuit) -> Vec<NodeId> {
        crate::regulation::Walk::new(c, &[], &[], &[])
            .stops()
            .to_vec()
    }

    #[test]
    fn a_zero_volt_series_ammeter_is_a_wire_but_a_real_source_still_holds() {
        let mut c = Circuit::new();
        let [gpio, a, b, load, rail, x] =
            ["GPIO", "A", "B", "LOAD", "RAIL", "X"].map(|n| c.node(n));
        c.add(Device::vsource(
            "Vrail",
            rail,
            NodeId::GROUND,
            SourceKind::Dc(5.0),
        ));
        c.add(Device::resistor("R1", gpio, a, 100.0));
        // The current-sense insertion: 0 V between two live nodes.
        c.add(Device::vsource("Vport_U1", a, b, SourceKind::Dc(0.0)));
        c.add(Device::resistor("R2", b, load, 100.0));
        // A pull-up beside the load reaches a rail a real source holds.
        c.add(Device::resistor("R3", load, rail, 10e3));
        // A 0 V source referenced to ground holds its node: not a wire.
        c.add(Device::vsource(
            "Vzero",
            x,
            NodeId::GROUND,
            SourceKind::Dc(0.0),
        ));
        c.add(Device::resistor("R4", gpio, x, 100.0));
        let stops = walk_stops(&c);
        assert!(
            stops.contains(&a) && stops.contains(&b),
            "the taint walk stops at the ammeter"
        );
        let walk = Influence::new(&c, &[], &stops);
        let mut reached: Vec<NodeId> = walk.reach(&[gpio]).into_iter().map(|(n, _)| n).collect();
        reached.sort();
        let mut want = vec![a, b, load];
        want.sort();
        assert_eq!(reached, want, "through the ammeter, not into RAIL or X");
        // The far side reaches back through it too.
        let back: Vec<NodeId> = walk.reach(&[load]).into_iter().map(|(n, _)| n).collect();
        assert!(back.contains(&a) && back.contains(&gpio), "{back:?}");
    }

    #[test]
    fn an_ammeter_terminal_a_real_source_also_holds_stays_held() {
        let mut c = Circuit::new();
        let [gpio, a, rail] = ["GPIO", "A", "RAIL"].map(|n| c.node(n));
        // The ammeter's far terminal IS the rail a real source holds.
        c.add(Device::vsource(
            "Vrail",
            rail,
            NodeId::GROUND,
            SourceKind::Dc(5.0),
        ));
        c.add(Device::vsource("Vport_U1", a, rail, SourceKind::Dc(0.0)));
        c.add(Device::resistor("R1", gpio, a, 100.0));
        let stops = walk_stops(&c);
        let walk = Influence::new(&c, &[], &stops);
        assert_eq!(walk.reach(&[gpio]), vec![(a, gpio)]);
    }
}
