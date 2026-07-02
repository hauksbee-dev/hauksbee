//! Conduction-graph analysis: who is electrically fused to whom.
//!
//! The single most important primitive of the tearing engine, generalized from
//! the hand-written `conduction_terminals` that made the Tarski feedforward
//! decomposition exact (`docs/dev-plans/research/tarski-saga.md` §2b).
//!
//! ## Why two kinds of terminal
//!
//! A device terminal either *conducts* (its node's KCL row receives current
//! from the device's stamp: a resistor end, a BJT collector) or it only
//! *senses* (it steers the stamp but its own row receives nothing: a VSwitch
//! control pin, a comparator input). The distinction is load-bearing for
//! decomposition: two blocks joined only through sense terminals exchange no
//! current, so cutting that wire and replaying its voltage as a source is
//! electrically exact. Reachability computed over *all* terminals would fuse
//! them and hide the tear; reachability computed over conduction terminals
//! only is what lets a 6,400-device board fall apart into per-block solves.
//!
//! The classification itself lives on the IR ([`Device::conduction_nodes`] /
//! [`Device::sense_nodes`]) next to each variant. It is a claim about the
//! stamp as implemented, and claims drift, so this module carries the
//! cross-check test that stamps every example device at multiple operating
//! points and fails if a declared sense node's row ever receives a matrix
//! entry or RHS contribution. A new device cannot ship a classification that
//! disagrees with its stamp.
//!
//! ## What this module computes
//!
//! [`ConductionGraph::analyze`] returns the connected components of the
//! circuit over conduction edges only (each component a *conduction island*),
//! plus every sense edge (device, sensed node) crossing anywhere in the
//! circuit. Downstream passes consume both: the feedforward pass turns
//! sense edges between islands into candidate free tears; the rail pass looks
//! for stiff nodes whose removal fragments an island further. Ground never
//! joins islands (it is the global reference, not a coupling path), matching
//! the rule the basic partitioner already uses.

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};

/// A device reading a node it does not conduct into: the raw material of a
/// free tear. If `node` is owned by one island and `device` lives in another,
/// the coupling between those islands through this edge carries zero current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenseEdge {
    /// The sensing device.
    pub device: DeviceId,
    /// The node it senses.
    pub node: NodeId,
}

/// Connected components of a circuit over conduction terminals only, plus all
/// sense edges. See the module doc for why the two are kept separate.
#[derive(Debug, Clone)]
pub struct ConductionGraph {
    /// Devices of each island, by island index. Order is stable across runs
    /// (indexed by first appearance in device order). Devices with no
    /// non-ground conduction terminal (e.g. a rail-to-ground source whose
    /// nodes are all ground) land in the island of their first conduction
    /// node, or nowhere if they have none.
    pub islands: Vec<Vec<DeviceId>>,
    /// Island index of each non-ground node, by `NodeId.0`; `None` for ground,
    /// for nodes only ever sensed, and for unused node ids.
    pub node_island: Vec<Option<usize>>,
    /// Every (device, sensed node) pair in the circuit.
    pub sense_edges: Vec<SenseEdge>,
}

impl ConductionGraph {
    /// Analyze a circuit into conduction islands and sense edges.
    ///
    /// This is deliberately more primitive than [`crate::Partition::analyze`]:
    /// no source-pinning, no linearity verdicts. Those are policy; this is
    /// topology. The tear passes need the raw conduction components first and
    /// apply rail/source reasoning on top.
    pub fn analyze(circuit: &Circuit) -> ConductionGraph {
        let n_nodes = circuit.max_node() as usize;
        let mut uf = UnionFind::new(n_nodes + 1);

        for (_, dev) in circuit.iter() {
            let cond: Vec<usize> = dev
                .conduction_nodes()
                .into_iter()
                .filter(|n| !n.is_ground())
                .map(|n| n.0 as usize)
                .collect();
            for w in cond.windows(2) {
                uf.union(w[0], w[1]);
            }
        }

        // Assign island indices by first-appearance of each root, walking
        // devices in order so the numbering is stable and human-followable.
        let mut root_to_island: Vec<Option<usize>> = vec![None; n_nodes + 1];
        let mut islands: Vec<Vec<DeviceId>> = Vec::new();
        let mut node_island: Vec<Option<usize>> = vec![None; n_nodes + 1];
        let mut sense_edges = Vec::new();

        for (id, dev) in circuit.iter() {
            let rep = dev
                .conduction_nodes()
                .into_iter()
                .find(|n| !n.is_ground())
                .map(|n| uf.find(n.0 as usize));
            if let Some(root) = rep {
                let island = *root_to_island[root].get_or_insert_with(|| {
                    islands.push(Vec::new());
                    islands.len() - 1
                });
                islands[island].push(id);
                for n in dev.conduction_nodes() {
                    if !n.is_ground() {
                        node_island[n.0 as usize] = Some(island);
                    }
                }
            }
            for n in dev.sense_nodes() {
                if !n.is_ground() {
                    sense_edges.push(SenseEdge { device: id, node: n });
                }
            }
        }

        ConductionGraph {
            islands,
            node_island,
            sense_edges,
        }
    }

    /// The island a device was assigned to, if any.
    pub fn island_of_device(&self, id: DeviceId) -> Option<usize> {
        self.islands
            .iter()
            .position(|isl| isl.contains(&id))
    }

    /// Sense edges that cross islands: `device` lives in one island while the
    /// node it senses is conducted by another (or by none). These are the
    /// candidate free-tear boundaries the feedforward pass will prove or
    /// refuse one-directionality for.
    pub fn cross_island_sense_edges(&self) -> Vec<SenseEdge> {
        self.sense_edges
            .iter()
            .copied()
            .filter(|e| {
                let dev_island = self.island_of_device(e.device);
                let node_island = self
                    .node_island
                    .get(e.node.0 as usize)
                    .copied()
                    .flatten();
                match (dev_island, node_island) {
                    (Some(d), Some(n)) => d != n,
                    // A sensed node nobody conducts into is exogenous (a
                    // floating control net): still a boundary, still crossing.
                    (Some(_), None) => true,
                    (None, _) => false,
                }
            })
            .collect()
    }
}

/// Path-halving union-find over node indices (same shape as the partitioner's;
/// kept private to each because the two must be able to evolve separately).
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::newton::Workspace;
    use crate::options::SolverOptions;
    use crate::stamp::{stamp_all, IntegCoeffs, StampCtx};
    use crate::system::ReactiveState;
    use hauksbee_ir::SourceKind;

    /// THE cross-check: every declared sense node's KCL row must receive
    /// nothing from the device's stamp, matrix entries AND RHS, at every
    /// probed operating point. This is the property the Tarski STEP-0 proof
    /// rests on (a select pin that drew current would re-fuse the torn
    /// columns), asserted mechanically so [`Device::sense_nodes`] can never
    /// silently disagree with the stamp in `stamp.rs`. If this test fails
    /// after a device or stamp change, the fix is to move the terminal to
    /// `conduction_nodes`, and every tear across it stops being offered.
    #[test]
    fn declared_sense_rows_receive_no_current() {
        // Operating points chosen to exercise the stamps' regions: all-zero,
        // everything high, a mixed pattern with sense terminals driven hard
        // (a lazy stamp that leaks current into its control row does so most
        // visibly when the control is at a rail), and a negative pattern for
        // the polarity folds.
        let probes: [[f64; 4]; 4] = [
            [0.0, 0.0, 0.0, 0.0],
            [5.0, 5.0, 5.0, 5.0],
            [0.3, 4.8, -0.7, 2.5],
            [-2.0, -0.5, 3.3, -4.0],
        ];

        let mut c = hauksbee_ir::Circuit::new();
        let n = [c.node("n1"), c.node("n2"), c.node("n3"), c.node("n4")];

        for dev in hauksbee_ir::Device::examples(n) {
            let name = dev.name().to_string();
            let mut circuit = hauksbee_ir::Circuit::new();
            let m = [
                circuit.node("n1"),
                circuit.node("n2"),
                circuit.node("n3"),
                circuit.node("n4"),
            ];
            let mut d = dev.clone();
            // examples() was built against `c`'s nodes; ids are identical in
            // the fresh circuit (same insertion order), remap for hygiene.
            d.map_nodes(&mut |old| m[(old.0 - 1) as usize]);
            let sense = d.sense_nodes();
            let conduction = d.conduction_nodes();

            // Partition property: every terminal is exactly one of the two.
            let mut all: Vec<_> = d.nodes();
            all.sort_unstable();
            all.dedup();
            let mut both: Vec<_> = conduction
                .iter()
                .chain(sense.iter())
                .copied()
                .collect();
            both.sort_unstable();
            both.dedup();
            assert_eq!(
                all, both,
                "{name}: conduction ∪ sense must cover every terminal exactly"
            );
            for s in &sense {
                assert!(
                    !conduction.contains(s),
                    "{name}: node {s:?} declared both conduction and sense"
                );
            }

            circuit.add(d);
            let mut ws = Workspace::new(&circuit);
            let opts = SolverOptions::default();
            let state = ReactiveState::new(circuit.devices.len());

            for (dc, first) in [(true, true), (false, true), (false, false)] {
                let coeffs = IntegCoeffs::for_step(opts.integration, 1e-6, first);
                for probe in probes {
                    for (i, v) in probe.iter().enumerate() {
                        if let Some(row) = ws.layout.node(m[i]) {
                            ws.x[row] = *v;
                        }
                    }
                    ws.matrix.clear_values();
                    ws.rhs.iter_mut().for_each(|v| *v = 0.0);
                    let empty_siblings = std::collections::HashMap::new();
                    let ctx = StampCtx {
                        circuit: &circuit,
                        layout: &ws.layout,
                        opts: &opts,
                        x: &ws.x,
                        x_prev: &ws.x,
                        time: 0.0,
                        coeffs,
                        state: &state,
                        dc,
                        use_ic: false,
                        // gmin = 0: the diagonal shunt is solver
                        // regularization, not device current, and would mask
                        // a zero-row check.
                        gmin: 0.0,
                        src_scale: 1.0,
                        // Baseline path: no staged-DC regularization, no
                        // frozen event decisions. The cross-check verifies
                        // the reference stamps; the staged variants reuse the
                        // same row-writing helpers, so a sense leak there
                        // would surface here too.
                        branch_reg: 0.0,
                        cmp_freeze: None,
                        switch_freeze: None,
                        spdt_sibling: &empty_siblings,
                    };
                    stamp_all(&ctx, &mut ws.matrix, &mut ws.rhs);

                    for sn in &sense {
                        let row = ws
                            .layout
                            .node(*sn)
                            .expect("sense node has an unknown row");
                        for &(col, val) in ws.matrix.row(row) {
                            assert!(
                                val == 0.0,
                                "{name}: sense node {sn:?} row has matrix entry \
                                 ({row},{col})={val} at probe {probe:?} (dc={dc}); \
                                 the stamp conducts into a declared sense terminal"
                            );
                        }
                        assert!(
                            ws.rhs[row] == 0.0,
                            "{name}: sense node {sn:?} row has RHS {} at probe \
                             {probe:?} (dc={dc})",
                            ws.rhs[row]
                        );
                    }
                }
            }
        }
    }

    /// Two RC blocks joined only by a comparator (out in block A, inputs
    /// sensing block B... inverted: inputs sense A, output drives B): the
    /// conduction graph must keep them separate islands with the coupling
    /// visible as cross-island sense edges. Add a resistor bridge and they
    /// must fuse. This is the tearing story in one fixture.
    #[test]
    fn sense_only_coupling_does_not_fuse_islands() {
        let mut c = hauksbee_ir::Circuit::new();
        let a1 = c.node("a1");
        let a2 = c.node("a2");
        let b1 = c.node("b1");
        let b2 = c.node("b2");
        c.add(hauksbee_ir::Device::Vsource {
            name: "VA".into(),
            p: a1,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        c.add(hauksbee_ir::Device::Resistor {
            name: "RA".into(),
            a: a1,
            b: a2,
            ohms: 1e3,
            tc1: None,
        });
        // Comparator lives electrically in block B (drives b1) while its
        // inputs only watch block A.
        c.add(hauksbee_ir::Device::Comparator {
            name: "CMP".into(),
            out: b1,
            inp: a2,
            inn: a1,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });
        c.add(hauksbee_ir::Device::Resistor {
            name: "RB".into(),
            a: b1,
            b: b2,
            ohms: 1e3,
            tc1: None,
        });

        let g = ConductionGraph::analyze(&c);
        assert_eq!(g.islands.len(), 2, "sense coupling must not fuse: {g:?}");
        let crossing = g.cross_island_sense_edges();
        assert_eq!(
            crossing.len(),
            2,
            "both comparator inputs cross islands: {crossing:?}"
        );

        // A real (conducting) bridge fuses the blocks.
        c.add(hauksbee_ir::Device::Resistor {
            name: "RBRIDGE".into(),
            a: a2,
            b: b1,
            ohms: 1e6,
            tc1: None,
        });
        let g2 = ConductionGraph::analyze(&c);
        assert_eq!(g2.islands.len(), 1, "a conducting bridge fuses the blocks");
    }
}
