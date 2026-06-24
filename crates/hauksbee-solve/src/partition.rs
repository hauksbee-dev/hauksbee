//! Circuit partitioning: split the device graph into solvable islands.
//!
//! This is the analysis pass behind the architecture's "partition before
//! solving" principle. It classifies every device (the IR already tags
//! `is_linear` / `is_event_driven`) and groups them into islands that can be
//! advanced independently for one timestep, exchanging only boundary values.
//!
//! ## How the cut is made
//!
//! Nodes are the vertices of a connectivity graph. A device's non-ground nodes
//! are normally tied together (they share an island). Two structures are treated
//! specially:
//!
//! * **Ground never connects.** It is a global reference, not a coupling path,
//!   so two islands both referencing ground are still independent.
//! * **Ideal voltage sources are cut points.** A `Vsource` pins the voltage
//!   between its two nodes to a known time function. Downstream of the source
//!   that node is a *boundary input* (a Thevenin drive), not a path that fuses
//!   the islands on either side. We therefore do not union a voltage source's
//!   two nodes; each appears as a boundary node feeding whatever island uses it.
//!
//! Connected components of the remaining graph are the islands. An island is
//! **linear** iff every device touching it is linear (resistor / capacitor /
//! inductor / current source); a single nonlinear or event-driven device
//! "taints" its whole island, which then goes to the MNA+Newton path. Linear
//! islands are eligible for the state-space matrix-exponential fast path.
//!
//! ## Coupling
//!
//! After the cut we record, per island, which nodes it *drives* and which it
//! merely *reads* as a boundary input. The orchestrator uses that to schedule a
//! Gauss-Seidel exchange at each step boundary. A node is "internal" to an
//! island if every device touching it lives in that island; otherwise it is a
//! shared/boundary node and participates in coupling.

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};

/// A union-find over node indices (1..=n_nodes; ground excluded).
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        UnionFind {
            parent: (0..n).collect(),
        }
    }
    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// One island: a set of devices, the nodes they touch, and a linearity verdict.
#[derive(Debug, Clone)]
pub struct Island {
    /// Devices assigned to this island, by `DeviceId`.
    pub devices: Vec<DeviceId>,
    /// All non-ground nodes any device in this island touches (sorted, unique).
    pub nodes: Vec<NodeId>,
    /// True iff every device in the island is linear (no Newton needed).
    pub linear: bool,
    /// Boundary nodes this island depends on but does not own — driven by an
    /// ideal source or shared with another island. The orchestrator supplies
    /// their voltages each step.
    pub boundary_in: Vec<NodeId>,
}

/// The full partition of a circuit.
#[derive(Debug, Clone)]
pub struct Partition {
    /// All islands. Order is stable (by smallest node index).
    pub islands: Vec<Island>,
    /// Voltage sources, which were cut and are handled as global boundary
    /// drives shared by every island that references their nodes.
    pub sources: Vec<DeviceId>,
    /// Total non-ground node count (for sizing global exchange buffers).
    pub n_nodes: usize,
}

impl Partition {
    /// Analyze a circuit into islands.
    pub fn analyze(circuit: &Circuit) -> Partition {
        let n_nodes = circuit.max_node() as usize;
        let mut uf = UnionFind::new(n_nodes + 1); // index by NodeId.0; 0 = ground

        let mut sources = Vec::new();

        // A node is "pinned" if an ideal voltage source fixes its voltage
        // (one terminal of the source on ground, or relative to another pinned
        // node). Pinned nodes are boundary inputs: a device that merely touches
        // a pinned node does not fuse with other devices through it, which is
        // what lets two legs hanging off the same supply rail become independent
        // islands. We still fuse a device's own internal nodes normally.
        let mut pinned = vec![false; n_nodes + 1];
        pinned[0] = true; // ground is a (trivially pinned) reference
        for (id, dev) in circuit.iter() {
            if let Device::Vsource { p, n, .. } = dev {
                sources.push(id);
                // p is pinned if n is ground/pinned (the common rail-to-ground
                // case). We do a couple of passes to propagate through stacked
                // sources below.
                let _ = (p, n);
            }
        }
        // Propagate pinned-ness across ideal sources (handles stacked supplies).
        for _ in 0..circuit.devices.len().min(64) {
            let mut changed = false;
            for (_, dev) in circuit.iter() {
                if let Device::Vsource { p, n, .. } = dev {
                    let (pi, ni) = (p.0 as usize, n.0 as usize);
                    if pinned[ni] && !pinned[pi] {
                        pinned[pi] = true;
                        changed = true;
                    }
                    if pinned[pi] && !pinned[ni] {
                        pinned[ni] = true;
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }

        // Union every device's non-ground, non-pinned nodes. A device's nodes
        // that are pinned act as boundaries and are not used to fuse islands.
        for (_, dev) in circuit.iter() {
            if matches!(dev, Device::Vsource { .. }) {
                continue;
            }
            let free: Vec<usize> = dev
                .nodes()
                .iter()
                .filter(|n| !n.is_ground() && !pinned[n.0 as usize])
                .map(|n| n.0 as usize)
                .collect();
            for w in free.windows(2) {
                uf.union(w[0], w[1]);
            }
        }

        // Assign every non-ground node a component root.
        // Devices land in the island of their first non-ground node. A device
        // whose only non-ground node is the source-driven boundary still needs
        // an island; voltage sources themselves stay global.
        use std::collections::BTreeMap;
        let mut by_root: BTreeMap<usize, Island> = BTreeMap::new();

        // Helper: pick a representative *free* (non-pinned) node for a device,
        // which decides its island. A device whose only non-ground nodes are
        // pinned has no dynamics of its own; it is dropped from islands (its
        // effect is felt as a boundary current, captured by the resistor on the
        // free side).
        let rep = |dev: &Device| -> Option<usize> {
            dev.nodes()
                .iter()
                .find(|n| !n.is_ground() && !pinned[n.0 as usize])
                .map(|n| n.0 as usize)
        };

        for (id, dev) in circuit.iter() {
            if matches!(dev, Device::Vsource { .. }) {
                continue; // global, not in any island
            }
            let Some(r0) = rep(dev) else { continue };
            let root = uf.find(r0);
            let island = by_root.entry(root).or_insert_with(|| Island {
                devices: Vec::new(),
                nodes: Vec::new(),
                linear: true,
                boundary_in: Vec::new(),
            });
            island.devices.push(id);
            if !dev.is_linear() {
                island.linear = false;
            }
            for n in dev.nodes() {
                if !n.is_ground() {
                    island.nodes.push(n);
                }
            }
        }

        // Finalize node sets. A node is a boundary input to an island if it is
        // pinned (source/ground-driven) or shared with another island; either
        // way its value is supplied from outside the island.
        let roots: Vec<usize> = by_root.keys().copied().collect();
        let mut node_owner: Vec<i64> = vec![-1; n_nodes + 1];
        for (idx, root) in roots.iter().enumerate() {
            let isl = by_root.get_mut(root).unwrap();
            isl.nodes.sort_unstable();
            isl.nodes.dedup();
            for n in &isl.nodes {
                let ni = n.0 as usize;
                if node_owner[ni] == -1 {
                    node_owner[ni] = idx as i64;
                } else if node_owner[ni] != idx as i64 {
                    node_owner[ni] = -2; // shared between islands
                }
            }
        }

        let mut islands: Vec<Island> = roots.iter().map(|r| by_root.remove(r).unwrap()).collect();

        for (idx, isl) in islands.iter_mut().enumerate() {
            let mut bin = Vec::new();
            for n in &isl.nodes {
                let ni = n.0 as usize;
                let is_pinned = pinned[ni] && !n.is_ground();
                let is_shared = node_owner[ni] == -2; // claimed by >1 island
                let _ = idx;
                if is_pinned || is_shared {
                    bin.push(*n);
                }
            }
            bin.sort_unstable();
            bin.dedup();
            isl.boundary_in = bin;
        }

        Partition {
            islands,
            sources,
            n_nodes,
        }
    }

    /// True if any island is purely linear and worth the state-space path.
    pub fn has_linear_island(&self) -> bool {
        self.islands
            .iter()
            .any(|i| i.linear && !i.devices.is_empty())
    }

    /// Number of nonlinear islands.
    pub fn nonlinear_count(&self) -> usize {
        self.islands.iter().filter(|i| !i.linear).count()
    }

    /// A one-line human summary for diagnostics / benchmarks.
    pub fn summary(&self) -> String {
        let lin = self.islands.iter().filter(|i| i.linear).count();
        let nl = self.nonlinear_count();
        format!(
            "{} islands ({} linear, {} nonlinear), {} cut sources, {} nodes",
            self.islands.len(),
            lin,
            nl,
            self.sources.len(),
            self.n_nodes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hauksbee_ir::{Device, NodeId, SourceKind};

    #[test]
    fn rc_ladder_is_one_linear_island() {
        let mut c = Circuit::new();
        let n0 = c.node("n0");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: n0,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        let mut prev = n0;
        for i in 0..5 {
            let next = c.node(&format!("n{}", i + 1));
            c.add(Device::Resistor {
                name: format!("R{}", i + 1),
                a: prev,
                b: next,
                ohms: 1e3,
                tc1: None,
            });
            c.add(Device::Capacitor {
                name: format!("C{}", i + 1),
                a: next,
                b: NodeId::GROUND,
                farads: 1e-9,
                ic: Some(0.0),
            });
            prev = next;
        }
        let p = Partition::analyze(&c);
        assert_eq!(p.islands.len(), 1, "ladder should be one island");
        assert!(p.islands[0].linear);
        assert_eq!(p.sources.len(), 1);
        // n0 is driven by the source -> boundary input to the island.
        assert!(p.islands[0].boundary_in.contains(&n0));
    }

    #[test]
    fn two_isolated_rc_are_two_islands() {
        // Two RC branches sharing only a common source rail are split because
        // the source is a cut point.
        let mut c = Circuit::new();
        let rail = c.node("rail");
        let a = c.node("a");
        let b = c.node("b");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: rail,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::Resistor {
            name: "Ra".into(),
            a: rail,
            b: a,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: "Ca".into(),
            a,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: Some(0.0),
        });
        c.add(Device::Resistor {
            name: "Rb".into(),
            a: rail,
            b,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: "Cb".into(),
            a: b,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: Some(0.0),
        });
        let p = Partition::analyze(&c);
        // rail is shared between the two branches but it's source-driven, so the
        // two RC legs are independent islands.
        assert_eq!(p.islands.len(), 2, "{}", p.summary());
        assert!(p.islands.iter().all(|i| i.linear));
    }

    #[test]
    fn diode_taints_its_island() {
        let mut c = Circuit::new();
        let n = c.node("n");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: n,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        let d = c.node("d");
        c.add(Device::Resistor {
            name: "R".into(),
            a: n,
            b: d,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Diode {
            name: "D".into(),
            a: d,
            k: NodeId::GROUND,
            model: hauksbee_ir::DiodeModel::default(),
        });
        let p = Partition::analyze(&c);
        assert_eq!(p.islands.len(), 1);
        assert!(!p.islands[0].linear, "diode island must be nonlinear");
    }
}
