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

/// A shunt-fed rail tear: an internal supply rail node (e.g. `ANALOG_VDD`) that
/// is fed from a pinned source rail (e.g. `+5V`) through a *single* series
/// resistor — a current-sense shunt or supply impedance — and is shared by many
/// otherwise-independent nonlinear blocks (the current-mirror emitters).
///
/// Because the rail couples the blocks only through one scalar (its voltage),
/// the system is bordered-block-diagonal: it decomposes EXACTLY into the per-
/// block islands plus a single scalar KCL balance at the rail. The driver solves
/// that balance by tearing — pin the rail to a trial voltage, solve every block,
/// sum the block currents drawn from the rail, and adjust the rail voltage until
/// the shunt current matches. At convergence this reproduces the monolithic
/// solution bit-for-bit within Newton tolerance; nothing is approximated.
#[derive(Debug, Clone)]
pub struct RailTear {
    /// The torn rail node (shared boundary across all blocks).
    pub rail: NodeId,
    /// The pinned source node feeding the rail (e.g. `+5V`).
    pub feed: NodeId,
    /// The single series resistor between `feed` and `rail` (the shunt).
    pub shunt: DeviceId,
    /// Series resistance of the shunt (Ω).
    pub r_shunt: f64,
    /// Other linear devices that also tie to the rail and have a free (non-rail,
    /// non-pinned) terminal — e.g. membrane pull-up resistors. These couple only
    /// through the rail voltage and are accounted for via their islands' boundary
    /// currents. A rail load with NO free terminal (e.g. a rail-to-ground bypass
    /// capacitor) would be dropped by the island analysis and its current lost,
    /// so its presence makes [`detect_rail_tears`] reject the tear entirely
    /// (conservative fallback to the exact monolithic path).
    pub extra_loads: Vec<DeviceId>,
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
    /// Detected shunt-fed rail tears. When non-empty, the orchestrator must
    /// solve each tear's scalar rail balance (see [`RailTear`]); the islands are
    /// already split as if the rail were a boundary input.
    pub tears: Vec<RailTear>,
}

impl Partition {
    /// Analyze a circuit into islands, with no rail tearing (legacy behaviour).
    pub fn analyze(circuit: &Circuit) -> Partition {
        Self::analyze_inner(circuit, &[])
    }

    /// Analyze a circuit into islands, additionally detecting and cutting
    /// shunt-fed supply-rail tear nodes (see [`RailTear`]). This is what unlocks
    /// the Tarski synapse array: every current mirror shares one `ANALOG_VDD`
    /// rail fed through a 1 kΩ sense shunt, which otherwise fuses the whole
    /// array into one giant nonlinear island.
    pub fn analyze_with_tears(circuit: &Circuit) -> Partition {
        let tears = detect_rail_tears(circuit);
        let tear_nodes: Vec<NodeId> = tears.iter().map(|t| t.rail).collect();
        let mut p = Self::analyze_inner(circuit, &tear_nodes);
        p.tears = tears;
        p
    }

    /// Core analysis. `extra_pinned` nodes are treated as boundary inputs (not
    /// unioned through), exactly like ideal-source-pinned nodes — used for rail
    /// tear nodes.
    fn analyze_inner(circuit: &Circuit, extra_pinned: &[NodeId]) -> Partition {
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
        // Rail tear nodes are treated as boundary inputs: a device touching the
        // rail does not fuse with other blocks through it. The rail's voltage is
        // resolved by the orchestrator's scalar balance, not by an ideal source.
        for n in extra_pinned {
            if !n.is_ground() {
                pinned[n.0 as usize] = true;
            }
        }
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

        let mut islands: Vec<Island> = roots
            .iter()
            .map(|r| by_root.remove(r).unwrap())
            .collect();

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
            tears: Vec::new(),
        }
    }

    /// True if any island is purely linear and worth the state-space path.
    pub fn has_linear_island(&self) -> bool {
        self.islands.iter().any(|i| i.linear && !i.devices.is_empty())
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
            "{} islands ({} linear, {} nonlinear), {} cut sources, {} tears, {} nodes",
            self.islands.len(),
            lin,
            nl,
            self.sources.len(),
            self.tears.len(),
            self.n_nodes
        )
    }
}

/// Minimum number of *nonlinear* devices that must share a rail before we treat
/// it as a tear (and not just an incidental two-terminal node). Below this, the
/// monolithic solve of the fused block is already cheap and tearing buys nothing.
const TEAR_MIN_NONLINEAR_FANOUT: usize = 8;

/// A series resistor at or below this resistance (Ω) is treated as a *stiff*
/// supply-leg connection: the two nodes it joins are effectively the same rail
/// for tear-detection purposes. This catches the binder's supply representation,
/// where an ideal `Vsource` sits behind a ~1 mΩ series resistor (`STIFF_R_OHMS`)
/// feeding the named rail net. It is deliberately far smaller than a real sense
/// shunt (the Tarski analog rail is fed through 1 kΩ), so a genuine shunt is
/// never mistaken for a stiff leg.
const STIFF_SUPPLY_R: f64 = 1.0;

/// Compute which nodes are held at a (near-)fixed supply potential: pinned by an
/// ideal voltage source, OR reachable from such a node through only *stiff*
/// (<= [`STIFF_SUPPLY_R`]) series resistors. The latter is what makes the
/// binder's "Vsource behind a 1 mΩ leg" rails register as proper supply feeds.
fn pinned_nodes(circuit: &Circuit, n_nodes: usize) -> Vec<bool> {
    let mut pinned = vec![false; n_nodes + 1];
    pinned[0] = true;
    for _ in 0..circuit.devices.len().min(128) {
        let mut changed = false;
        for (_, dev) in circuit.iter() {
            match dev {
                Device::Vsource { p, n, .. } => {
                    let (pi, ni) = (p.0 as usize, n.0 as usize);
                    if (n.is_ground() || pinned[ni]) && !pinned[pi] {
                        pinned[pi] = true;
                        changed = true;
                    }
                    if (p.is_ground() || pinned[pi]) && !pinned[ni] {
                        pinned[ni] = true;
                        changed = true;
                    }
                }
                // Stiff supply leg: propagate the rail across a near-zero R.
                Device::Resistor { a, b, ohms, .. } if *ohms <= STIFF_SUPPLY_R => {
                    let (ai, bi) = (a.0 as usize, b.0 as usize);
                    if pinned[ai] && !pinned[bi] && !b.is_ground() {
                        pinned[bi] = true;
                        changed = true;
                    }
                    if pinned[bi] && !pinned[ai] && !a.is_ground() {
                        pinned[ai] = true;
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        if !changed {
            break;
        }
    }
    pinned
}

/// Detect shunt-fed supply-rail tear nodes (see [`RailTear`]).
///
/// A node `rail` qualifies as a tear iff ALL of:
///   * it is NOT itself pinned by an ideal source;
///   * exactly one resistor connects it to a *pinned* node (the sense shunt /
///     supply impedance) — call that node `feed`;
///   * no ideal voltage source touches `rail` (it is genuinely internal);
///   * at least [`TEAR_MIN_NONLINEAR_FANOUT`] nonlinear devices touch `rail`
///     (it is the shared rail of a current-mirror array, not an incidental net).
///
/// This is an EXACT structural condition: when it holds, the only coupling among
/// the blocks hanging off `rail` is the single scalar `v(rail)`, and the system
/// is bordered-block-diagonal. The orchestrator's scalar balance reproduces the
/// monolithic answer; no device behaviour is abstracted away.
fn detect_rail_tears(circuit: &Circuit) -> Vec<RailTear> {
    let n_nodes = circuit.max_node() as usize;
    if n_nodes == 0 {
        return Vec::new();
    }
    let pinned = pinned_nodes(circuit, n_nodes);

    // Per node: count nonlinear-device touches, list resistors to a pinned node,
    // and flag if an ideal Vsource touches it.
    let mut nl_fanout = vec![0usize; n_nodes + 1];
    let mut vsource_touch = vec![false; n_nodes + 1];
    // resistor links from a non-pinned node to a pinned node: (rail) -> (feed, dev, ohms)
    let mut shunt_links: Vec<Vec<(usize, DeviceId, f64)>> = vec![Vec::new(); n_nodes + 1];

    for (id, dev) in circuit.iter() {
        if !dev.is_linear() {
            for n in dev.nodes() {
                if !n.is_ground() {
                    nl_fanout[n.0 as usize] += 1;
                }
            }
        }
        match dev {
            Device::Vsource { p, n, .. } => {
                if !p.is_ground() {
                    vsource_touch[p.0 as usize] = true;
                }
                if !n.is_ground() {
                    vsource_touch[n.0 as usize] = true;
                }
            }
            Device::Resistor { a, b, ohms, .. } => {
                let (ai, bi) = (a.0 as usize, b.0 as usize);
                // A candidate supply shunt feeds the unpinned (rail) side from a
                // pinned SUPPLY node. The feed must NOT be ground: a resistor to
                // ground is a leak / load, not a supply shunt, and tearing there
                // would wrongly treat the node as supply-driven.
                if pinned[ai] && !a.is_ground() && !pinned[bi] && !b.is_ground() {
                    shunt_links[bi].push((ai, id, *ohms));
                } else if pinned[bi] && !b.is_ground() && !pinned[ai] && !a.is_ground() {
                    shunt_links[ai].push((bi, id, *ohms));
                }
            }
            _ => {}
        }
    }

    let mut tears = Vec::new();
    for rail in 1..=n_nodes {
        if pinned[rail] || vsource_touch[rail] {
            continue;
        }
        if nl_fanout[rail] < TEAR_MIN_NONLINEAR_FANOUT {
            continue;
        }
        // Exactly one resistor to a pinned node => an unambiguous series shunt.
        if shunt_links[rail].len() != 1 {
            continue;
        }
        let (feed, shunt, r_shunt) = shunt_links[rail][0];
        // Other linear loads tied to the rail (e.g. membrane pull-up resistors)
        // are recorded for diagnostics; they live in their blocks' islands and
        // are accounted for through the rail voltage automatically — UNLESS a
        // device
        // connects ONLY between the rail and already-pinned/ground nodes. Such a
        // device (e.g. a rail-to-ground decoupling capacitor) has no free node,
        // so `analyze_inner` drops it from every island and its current would be
        // silently excluded from the rail balance — diverging from the monolithic
        // engine, which stamps it on the single rail node. We CANNOT account for
        // its current with the boundary-source bookkeeping, so we reject the tear
        // entirely and fall back to the monolithic path (exact, just not torn).
        // This is the conservative-correctness guard: only tear where every rail
        // load has a free node whose island reports its current.
        let mut droppable_load = false;
        let mut extra_loads = Vec::new();
        for (id, dev) in circuit.iter() {
            if id == shunt {
                continue;
            }
            let touches_rail = dev.nodes().iter().any(|n| n.0 as usize == rail);
            if !touches_rail {
                continue;
            }
            // Does this device have any non-ground node that is NOT the rail and
            // NOT already pinned? If not, it is dropped and its current is lost.
            let has_free = dev.nodes().iter().any(|n| {
                !n.is_ground() && n.0 as usize != rail && !pinned[n.0 as usize]
            });
            if !has_free {
                droppable_load = true;
                break;
            }
            if dev.is_linear() {
                extra_loads.push(id);
            }
        }
        if droppable_load {
            continue;
        }
        tears.push(RailTear {
            rail: NodeId(rail as u32),
            feed: NodeId(feed as u32),
            shunt,
            r_shunt,
            extra_loads,
        });
    }
    tears
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
        c.add(Device::Resistor { name: "Ra".into(), a: rail, b: a, ohms: 1e3, tc1: None });
        c.add(Device::Capacitor { name: "Ca".into(), a, b: NodeId::GROUND, farads: 1e-9, ic: Some(0.0) });
        c.add(Device::Resistor { name: "Rb".into(), a: rail, b, ohms: 1e3, tc1: None });
        c.add(Device::Capacitor { name: "Cb".into(), a: b, b: NodeId::GROUND, farads: 1e-9, ic: Some(0.0) });
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
        c.add(Device::Resistor { name: "R".into(), a: n, b: d, ohms: 1e3, tc1: None });
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
