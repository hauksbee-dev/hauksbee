//! Stiff-node tear detection: the fused cores that free tears cannot see.
//!
//! The rail pass ([`super::rails`]) finds nodes that are stiff because a supply
//! feeds them through one series impedance; the feedforward pass
//! ([`super::feedforward`]) finds boundaries that carry information but no
//! current. Between the two sits a shape neither catches. On the flagship
//! Tarski board the ten output columns fuse through nine hidden `V_out` nets,
//! and the route-4 mechanics come straight off that shape. Those nets are
//! CONDUCTED on both sides: the neuron drives them and the synapse ladders
//! draw from them. So
//! they are not sense-only (free-tear discovery walks right past them) and they
//! are not shunt-fed rails (no single series feed pins them). They are the
//! canonical stiff node: a high-fanout conducted node whose voltage barely
//! moves, and holding it fragments an otherwise-monolithic block into the
//! per-column solves the bespoke `tarski_decomp` stage A/B achieved by hand.
//!
//! ## What this pass does
//!
//! Analysis-time only, NO solves. It nominates candidates: a non-pinned,
//! non-held node with high conduction fanout whose holding (jointly with the
//! rails and stiff nodes already accepted) fragments a large block into two or
//! more pieces the cost model says are worth solving apart. Search is confined
//! to blocks big enough to repay a capture/replay boundary, the top
//! conduction-fanout nodes of each are probed, and the reuse is deliberate:
//! `super::rails::fragment_blocks` already fragments an island under an
//! arbitrary held set, so this is a new caller, not a new engine. No magic
//! fanout constant survives; the probe budget is bounded by policy and the
//! candidates are ranked by the same joint cost model the balance tears use.
//!
//! ## What this pass does NOT claim
//!
//! It does not claim any node is stiff. Stiffness is a measured property, and
//! it is measured by the ORCHESTRATOR at capture time, not here: capture the
//! candidate's waveform with its load attached, probe the small-signal output
//! impedance, and certify `sag_v = max_t |I_replay - I_capture| * Z_out`
//! against `stiff_tol`, escalating to a fused or balance treatment when the
//! bound is exceeded. Candidates that couple to each other are re-captured
//! against each other's waveforms, so the certified sag is a round-to-round
//! residual rather than an artifact of pinning them at rest. Detection only
//! nominates the nodes worth
//! that measurement. A candidate this pass returns is a hypothesis about where
//! the board fragments cheaply, nothing more; every honesty claim is deferred
//! to the certificate the orchestrator fills in.
//!

use std::collections::HashMap;

use hauksbee_ir::{Circuit, NodeId};

use super::conduction::ConductionGraph;
use super::rails::{fragment_blocks, pinned_nodes, Fragmentation, RailPolicy};

/// Tunable policy for stiff-node detection. Every field exists so a calibration
/// pass can move it from measurements instead of edits.
#[derive(Debug, Clone, Copy)]
pub struct StiffPolicy {
    /// Candidates are searched only inside blocks at least this large
    /// (devices); a smaller block cannot repay a capture/replay boundary, so
    /// probing it is wasted work. Default 64: below this a stiff-tear boundary
    /// (one capture solve of the fan-out, one replay per downstream block) does
    /// not amortize against just solving the block whole, and the flagship's
    /// worth-tearing blocks are the ~436-device output columns, orders of
    /// magnitude above the floor. This is the emergent successor to the
    /// bespoke path's tuned `TEAR_MAX_BLOCK_DEVICES`: a size the cost model
    /// respects, not a threshold it obeys.
    pub min_block_devices: usize,
    /// Try at most this many top-conduction-fanout nodes per block. The cut
    /// node is a high-fanout node by construction (many devices conduct into
    /// the fused net), so ranking by fanout puts it near the front; the budget
    /// bounds the per-block probe work without a magic fanout constant.
    /// Default 16.
    pub max_probes_per_block: usize,
}

impl Default for StiffPolicy {
    fn default() -> Self {
        StiffPolicy {
            min_block_devices: 64,
            max_probes_per_block: 16,
        }
    }
}

/// One nominated stiff-node tear candidate. Explainable on purpose: this is
/// what the eventual certificate and `--json` surface, so a user can see which
/// node the analysis thinks fragments their board and by how much.
#[derive(Debug, Clone)]
pub struct StiffCandidate {
    /// The node to hold as a boundary.
    pub node: NodeId,
    /// Conduction fanout (devices conducting the node).
    pub fanout: usize,
    /// Block sizes (device counts, descending) the node's island fragments into
    /// when this candidate is held JOINTLY with everything already accepted
    /// (the passed-in `held` set plus every earlier candidate in the returned
    /// list). Composed, not double-counted: two candidates returned together
    /// achieve the product of their gains, and the later one's sizes already
    /// reflect the earlier one being held.
    pub block_sizes: Vec<usize>,
    /// Cost-model speedup of splitting THIS candidate's parent block versus
    /// solving that block whole (the marginal ratio that ranked it). Strictly
    /// greater than 1.0 for every returned candidate. Note the scope: this is
    /// per-block marginal and pays no outer loop, so it is NOT directly
    /// comparable to a balance tear's est_speedup, which is whole-island and
    /// divided by outer_iters; a consumer ranking the two together would be
    /// comparing different formulas.
    pub est_speedup: f64,
}

/// Nominate stiff-node tear candidates: non-pinned, non-held conducted nodes
/// whose holding fragments a large block into two or more cheaper pieces.
///
/// `graph` must be the [`ConductionGraph`] of the same circuit. `held` are the
/// pinned-equivalent boundaries already decided (accepted balance rails and the
/// like); detection holds them too, because the fragmentation it must reason
/// about is the one the multi-boundary executor actually solves. `rails`
/// supplies the shared cost exponent (`alpha`); see the cost note below for why
/// its `outer_iters` is deliberately not applied here.
///
/// Returned candidates are sorted by `est_speedup` descending, and every one
/// both scores above 1.0 and fragments its block into at least two pieces.
///
/// ## Cost note
///
/// The score is the balance tear's ratio with one term removed. A balance tear
/// pays `outer_iters * sum(b^alpha)` because it reconciles the torn node with a
/// scalar KCL balance loop; a stiff tear pins the node to its settled voltage
/// with NO coupling equation at all, so it pays only `sum(b^alpha)`. Applying
/// `rails.outer_iters` here would charge the cheaper tear for a loop it never
/// runs and would wrongly refuse every two-way split (`2^alpha < outer_iters`).
/// The `alpha` exponent is still read from [`RailPolicy`] so the fill model
/// stays a single source of truth across the two passes.
pub fn detect_stiff_candidates(
    circuit: &Circuit,
    graph: &ConductionGraph,
    held: &[NodeId],
    rails: &RailPolicy,
    policy: &StiffPolicy,
) -> Vec<StiffCandidate> {
    let n_nodes = circuit.max_node() as usize;
    if n_nodes == 0 {
        return Vec::new();
    }

    // The held-boolean vec: source-pinned nodes (the same propagation the rail
    // pass uses, reused not copied) plus the caller's already-decided
    // boundaries. Detection holds all of them.
    let mut base_held = pinned_nodes(circuit, n_nodes);
    for h in held {
        let i = h.0 as usize;
        if i <= n_nodes {
            base_held[i] = true;
        }
    }

    // Conduction fanout per node: how many devices conduct into it. This is the
    // ranking key, computed once.
    let mut fanout = vec![0usize; n_nodes + 1];
    for (_, dev) in circuit.iter() {
        for nd in dev.conduction_nodes() {
            if !nd.is_ground() {
                fanout[nd.0 as usize] += 1;
            }
        }
    }

    let alpha = rails.alpha;
    let mut out: Vec<StiffCandidate> = Vec::new();

    for island in 0..graph.islands.len() {
        // Greedy set growth within the island: each round holds the best
        // remaining probe and re-fragments with it added, so the returned
        // candidates COMPOSE (the second is scored and sized with the first
        // already held) instead of each re-claiming the same split. Bounded by
        // the node count: every round adds one node to `chosen`.
        let mut chosen: Vec<usize> = Vec::new();
        for _ in 0..=n_nodes {
            let mut held_set = base_held.clone();
            for &c in &chosen {
                held_set[c] = true;
            }
            let frag = fragment_blocks(circuit, graph, island, &held_set);

            // Blocks big enough to be worth searching inside.
            let big: Vec<usize> = frag
                .block_devices
                .iter()
                .filter(|(_, &cnt)| cnt >= policy.min_block_devices)
                .map(|(&root, _)| root)
                .collect();
            if big.is_empty() {
                break;
            }

            // Best fragmenting probe across every big block, chosen
            // deterministically (higher speedup, then lower node id on a tie).
            let mut best: Option<(usize, f64, Fragmentation)> = None;
            for &root in &big {
                let mut nodes: Vec<usize> = frag
                    .node_block
                    .iter()
                    .filter(|(_, &r)| r == root)
                    .map(|(&n, _)| n)
                    .collect();
                nodes.sort_unstable_by(|&a, &b| fanout[b].cmp(&fanout[a]).then(a.cmp(&b)));
                nodes.truncate(policy.max_probes_per_block);

                let parent_devs: Vec<_> = frag
                    .device_block
                    .iter()
                    .filter(|(_, &r)| r == root)
                    .map(|(&d, _)| d)
                    .collect();

                for &p in &nodes {
                    let mut probe_held = held_set.clone();
                    probe_held[p] = true;
                    let frag2 = fragment_blocks(circuit, graph, island, &probe_held);

                    // How the parent block alone splits (other blocks are
                    // unchanged; holding a node can only split, never fuse).
                    let mut sub: HashMap<usize, usize> = HashMap::new();
                    for &d in &parent_devs {
                        if let Some(&r) = frag2.device_block.get(&d) {
                            *sub.entry(r).or_insert(0) += 1;
                        }
                    }
                    if sub.len() < 2 {
                        continue; // fragmentation, not fanout, is the criterion
                    }

                    let mut sub_sizes: Vec<usize> = sub.into_values().collect();
                    sub_sizes.sort_unstable_by(|a, b| b.cmp(a));
                    let total: usize = sub_sizes.iter().sum();
                    let mono = (total.max(1) as f64).powf(alpha);
                    let torn: f64 = sub_sizes
                        .iter()
                        .map(|&b| (b.max(1) as f64).powf(alpha))
                        .sum();
                    let speedup = mono / torn.max(f64::MIN_POSITIVE);
                    if speedup <= 1.0 {
                        continue;
                    }

                    let better = match &best {
                        None => true,
                        Some((bn, bs, _)) => {
                            speedup > *bs + 1e-12 || ((speedup - *bs).abs() <= 1e-12 && p < *bn)
                        }
                    };
                    if better {
                        // Keep the probe's whole fragmentation: the winner's
                        // probe_held IS the final held set for this round, so
                        // its block sizes are the candidate's block_sizes and
                        // recomputing them below would be pure waste.
                        best = Some((p, speedup, frag2));
                    }
                }
            }

            let Some((node, speedup, frag_win)) = best else {
                break;
            };
            chosen.push(node);

            // The candidate's own sizes are the WHOLE island's fragmentation
            // with every accepted node held (base + all chosen, this one
            // included): exactly the winning probe's fragmentation, cached.
            let block_sizes = frag_win.sizes();
            out.push(StiffCandidate {
                node: NodeId(node as u32),
                fanout: fanout[node],
                block_sizes,
                est_speedup: speedup,
            });
        }
    }

    out.sort_unstable_by(|a, b| {
        b.est_speedup
            .partial_cmp(&a.est_speedup)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.node.0.cmp(&b.node.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::conduction::ConductionGraph;
    use hauksbee_ir::{BjtModel, Device, Polarity, SourceKind};

    /// A 4-cycle of BJTs with grounded base resistors, returning the four cycle
    /// nodes. A cycle has no internal articulation point, so holding any single
    /// one of its nodes leaves the rest connected: this makes the *only* way to
    /// cut such a block off from its neighbours the shared node that joins them,
    /// which is exactly the stiff-node shape under test (fanout without an
    /// internal cut is not a tear).
    fn add_cycle(c: &mut Circuit, prefix: &str, model: &BjtModel) -> Vec<NodeId> {
        let a: Vec<NodeId> = (0..4).map(|i| c.node(&format!("{prefix}_a{i}"))).collect();
        for i in 0..4 {
            let base = c.node(&format!("{prefix}_base{i}"));
            c.add(Device::Bjt {
                name: format!("{prefix}_Q{i}"),
                c: a[i],
                b: base,
                e: a[(i + 1) % 4],
                model: model.clone(),
            });
            c.add(Device::Resistor {
                name: format!("{prefix}_Rb{i}"),
                a: base,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
        }
        a
    }

    fn mesh(c: &mut Circuit, name: &str, a: NodeId, b: NodeId) {
        c.add(Device::Resistor {
            name: name.into(),
            a,
            b,
            ohms: 1e3,
            tc1: None,
        });
    }

    fn source(c: &mut Circuit, name: &str, node: NodeId) {
        c.add(Device::Vsource {
            name: name.into(),
            p: node,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
    }

    fn pnp() -> BjtModel {
        BjtModel {
            polarity: Polarity::P,
            ..BjtModel::default()
        }
    }

    /// Two ~10-device nonlinear blocks fused through ONE driven node `vout`
    /// (a resistor mesh joins each block to `vout`, itself fed from a source
    /// through a small resistor). With the rail NOT held, holding `vout` is the
    /// only cut that separates the two blocks, so it must be nominated with two
    /// blocks and a speedup above one.
    fn fused_pair() -> (Circuit, NodeId) {
        let mut c = Circuit::new();
        let pwr = c.node("pwr");
        source(&mut c, "V5", pwr);
        let vout = c.node("vout");
        // The small series feed: vout is driven but not source-pinned.
        c.add(Device::Resistor {
            name: "Rfeed".into(),
            a: pwr,
            b: vout,
            ohms: 10.0,
            tc1: None,
        });
        let model = pnp();
        let a = add_cycle(&mut c, "A", &model);
        let b = add_cycle(&mut c, "B", &model);
        // Two contacts per block so no internal block node is itself a cut:
        // only vout separates block A from block B.
        mesh(&mut c, "RvA0", vout, a[0]);
        mesh(&mut c, "RvA2", vout, a[2]);
        mesh(&mut c, "RvB0", vout, b[0]);
        mesh(&mut c, "RvB2", vout, b[2]);
        (c, vout)
    }

    #[test]
    fn fused_blocks_nominate_the_driven_node() {
        let (c, vout) = fused_pair();
        let g = ConductionGraph::analyze(&c);
        let policy = StiffPolicy {
            min_block_devices: 8,
            max_probes_per_block: 16,
        };
        let cands = detect_stiff_candidates(&c, &g, &[], &RailPolicy::default(), &policy);
        assert_eq!(cands.len(), 1, "only vout cuts the fused pair: {cands:?}");
        let t = &cands[0];
        assert_eq!(t.node, vout);
        assert!(
            t.block_sizes.len() >= 2,
            "vout must fragment into 2+ blocks: {:?}",
            t.block_sizes
        );
        assert!(
            t.est_speedup > 1.0,
            "must be a modeled win: {}",
            t.est_speedup
        );
    }

    /// The same board, but with the floor above the block size: the fused
    /// island is smaller than `min_block_devices`, so it is never searched and
    /// nothing is nominated. The floor is respected, not overridden by fanout.
    #[test]
    fn blocks_below_the_floor_yield_nothing() {
        let (c, _vout) = fused_pair();
        let g = ConductionGraph::analyze(&c);
        // Default floor is 64; the fused island is ~21 devices.
        let cands =
            detect_stiff_candidates(&c, &g, &[], &RailPolicy::default(), &StiffPolicy::default());
        assert!(
            cands.is_empty(),
            "a sub-floor block cannot repay a boundary: {cands:?}"
        );
    }

    /// A high-conduction-fanout node whose holding does NOT fragment: every
    /// device also connects to a second shared node, so removing the first
    /// leaves the block fused through the second. Fanout alone must not
    /// nominate; only a node whose removal actually splits a block qualifies.
    #[test]
    fn high_fanout_without_a_cut_is_not_nominated() {
        let mut c = Circuit::new();
        let pwr = c.node("pwr");
        source(&mut c, "V5", pwr);
        let n1 = c.node("n1");
        let n2 = c.node("n2");
        // n1 looks like a stiff rail (fed through a shunt, high fanout) but is
        // not a cut: n2 re-fuses everything the moment n1 is held.
        c.add(Device::Resistor {
            name: "Rshunt".into(),
            a: pwr,
            b: n1,
            ohms: 1e3,
            tc1: None,
        });
        let model = pnp();
        for k in 0..6 {
            let base = c.node(&format!("base{k}"));
            c.add(Device::Bjt {
                name: format!("Q{k}"),
                c: n1,
                b: base,
                e: n2,
                model: model.clone(),
            });
            c.add(Device::Resistor {
                name: format!("Rb{k}"),
                a: base,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
        }
        let g = ConductionGraph::analyze(&c);
        let policy = StiffPolicy {
            min_block_devices: 4,
            max_probes_per_block: 16,
        };
        let cands = detect_stiff_candidates(&c, &g, &[], &RailPolicy::default(), &policy);
        assert!(
            cands.is_empty(),
            "fanout without fragmentation is not a tear: {cands:?}"
        );
    }

    /// Composition: three blocks chained through two cut nodes `x` (A|B) and
    /// `y` (B|C). Both must be nominated, and because detection grows the held
    /// set greedily, the candidate held SECOND is scored and sized with the
    /// first already held: holding both yields the fully composed three-block
    /// fragmentation, not two independent two-block claims.
    #[test]
    fn chained_blocks_compose() {
        let mut c = Circuit::new();
        let pwr = c.node("pwr");
        source(&mut c, "V5", pwr);
        let x = c.node("x");
        let y = c.node("y");
        c.add(Device::Resistor {
            name: "Rfx".into(),
            a: pwr,
            b: x,
            ohms: 10.0,
            tc1: None,
        });
        c.add(Device::Resistor {
            name: "Rfy".into(),
            a: pwr,
            b: y,
            ohms: 10.0,
            tc1: None,
        });
        let model = pnp();
        let a = add_cycle(&mut c, "A", &model);
        let b = add_cycle(&mut c, "B", &model);
        let d = add_cycle(&mut c, "C", &model);
        // A joins x by two contacts; C joins y by two; B bridges both, two
        // contacts each. Two contacts keep every internal node off the cut set,
        // so only x and y fragment.
        mesh(&mut c, "RAx0", a[0], x);
        mesh(&mut c, "RAx2", a[2], x);
        mesh(&mut c, "RBx0", b[0], x);
        mesh(&mut c, "RBx2", b[2], x);
        mesh(&mut c, "RBy1", b[1], y);
        mesh(&mut c, "RBy3", b[3], y);
        mesh(&mut c, "RCy0", d[0], y);
        mesh(&mut c, "RCy2", d[2], y);

        let g = ConductionGraph::analyze(&c);
        let policy = StiffPolicy {
            min_block_devices: 8,
            max_probes_per_block: 16,
        };
        let cands = detect_stiff_candidates(&c, &g, &[], &RailPolicy::default(), &policy);
        assert_eq!(
            cands.len(),
            2,
            "both cut nodes must be nominated: {cands:?}"
        );
        assert!(
            cands.iter().any(|k| k.node == x),
            "x must be nominated: {cands:?}"
        );
        assert!(
            cands.iter().any(|k| k.node == y),
            "y must be nominated: {cands:?}"
        );

        // The first cut splits the single island in two; the second, scored
        // with the first held, splits it in three. So exactly one candidate
        // carries three blocks and one carries two, whichever order they were
        // held in.
        let composed = cands
            .iter()
            .find(|k| k.block_sizes.len() == 3)
            .expect("holding both cuts yields three blocks");
        let first = cands
            .iter()
            .find(|k| k.block_sizes.len() == 2)
            .expect("the first cut yields two blocks");
        assert_eq!(
            composed.block_sizes,
            vec![12, 10, 10],
            "composed fragmentation is A|B|C"
        );
        assert_eq!(
            first.block_sizes,
            vec![23, 10],
            "the first cut leaves one end block and the fused remainder"
        );
    }
}
