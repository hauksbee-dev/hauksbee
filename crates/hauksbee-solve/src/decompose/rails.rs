//! Stiff-rail detection: where a balance tear fragments a fused core.
//!
//! The shape this hunts is the one that defeats a monolithic solve: a supply
//! rail fed through one series impedance, loaded by many nonlinear blocks.
//! Every block couples to every other only through the scalar rail voltage, so
//! the system is bordered-block-diagonal and tears *exactly* into per-block
//! solves plus one scalar KCL balance at the rail. This module decides *when*
//! to use the solver-side mechanics in `partition::analyze_with_tears`.
//!
//! ## Why a cost model and not thresholds
//!
//! Gating the tear on tuned constants (a nonlinear fanout bar, a block-size
//! cap) only ever calibrates to the board it was measured on. The decision
//! here is a first-order cost model, and a block-size cap falls out of it for
//! free: if tearing fails to fragment the core (one block nearly the whole
//! island), the torn cost is the monolithic cost times the outer-loop count
//! and the model refuses on its own.
//!
//! The model is deliberately crude: per-step Newton cost on an island of `n`
//! devices is estimated as `n^ALPHA` with `ALPHA = 1.4`, the textbook fill
//! exponent for sparse LU on circuit matrices (between the linear ideal of a
//! perfect elimination order and the quadratic of a dense band; the decision
//! only needs the ratio to be roughly right, correctness never depends on it).
//! A torn solve pays `OUTER_ITERS` re-solves of every rail-loading block for
//! the scalar balance (the secant loop converges in about three trials on the
//! fixtures). Both constants are policy fields, not buried literals.
//!
//! ## Two ways in
//!
//! Profitability is not the only trigger. A board whose monolith never
//! converges at any cost passes [`TearMotive::ConvergenceEscalation`]:
//! structural guards still apply (an unsound tear stays refused), but the cost
//! gate is bypassed, because a slow answer beats no answer. This is the
//! "decompose" rung of the robustness ladder.
//!
//! ## Stacked feeds
//!
//! Real supplies cascade (source -> +5V -> 1k shunt -> ANALOG_VDD), and
//! single-hop detection (feed must be source-pinned) stops one hop short of
//! the rail that fragments the board. Discovery is therefore transitive: a
//! discovered rail is a valid feed for the next hop, walked to a fixpoint.
//! Decisions are then JOINT: every surviving candidate is held as a boundary
//! while each island's fragmentation is computed once, because that is the
//! system the multi-rail balance executor actually solves. Within a cascade
//! every accepted rail tears, parent and child alike: the balance executor
//! carries the inter-rail shunt term. A parent's KCL subtracts the current
//! leaving through the shunt toward each accepted child, whose shunt belongs
//! to no block because it sits between two held rails (see
//! `orchestrate::balance::RailChannel::children`).
//!
//! ## What can refuse a tear
//!
//! * **An ideal source on the rail**: already pinned, nothing to tear.
//! * **Ambiguous feed**: more than one low-impedance path from shallower
//!   feeds; the scalar-balance bookkeeping assumes one.
//! * **No fragmentation / unprofitable**: the cost model above (unless
//!   escalating).
//!
//! Stranding (a device whose conduction terminals all land in {held rails,
//! pinned, ground} losing its current from every block's books) is not a
//! refusal here: the island analysis gives such devices boundary-only islands
//! whose currents the balance reads like any block's (`partition.rs`). The
//! refusal survives only on the `detect_rail_tears` path, whose executor does
//! not carry those boundary-only currents.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/decompose.md

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};

use super::conduction::ConductionGraph;

/// Why the caller wants tears: shapes how aggressive the decision is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TearMotive {
    /// Normal path: tear only where the cost model predicts a win.
    Profit,
    /// The monolithic solve failed (DC homotopy exhausted or transient died
    /// at dt_min): structural guards still apply, the cost gate does not.
    ConvergenceEscalation,
}

/// Tunable policy for the detector. Defaults carry their provenance in the
/// module doc; every field exists so a calibration pass can adjust them from
/// measurements instead of edits.
#[derive(Debug, Clone, Copy)]
pub struct RailPolicy {
    /// Sparse-LU cost exponent: per-step island cost is modeled as n^alpha.
    pub alpha: f64,
    /// Expected outer (secant) iterations of the scalar rail balance.
    pub outer_iters: f64,
    /// Structural floor: a "rail" loaded by fewer nonlinear devices than this
    /// is not an array shape at all (it is a signal node with a pull-up).
    pub min_nonlinear_fanout: usize,
    /// A feed resistor above this is not a supply shunt (it is a divider or
    /// a pull-up); the rail is then simply not stiff-fed.
    pub max_shunt_ohms: f64,
}

impl Default for RailPolicy {
    fn default() -> Self {
        RailPolicy {
            alpha: 1.4,
            outer_iters: 3.0,
            min_nonlinear_fanout: 2,
            max_shunt_ohms: 10.0e3,
        }
    }
}

/// The decision for one candidate rail, kept explainable on purpose: this
/// struct is what the tear certificate and `--json` surface, so a user can
/// see why their board did or did not tear.
#[derive(Debug, Clone, PartialEq)]
pub enum TearDecision {
    /// Tear it: the balance loop is predicted `est_speedup` times cheaper
    /// than the monolithic solve (>= 1.0; clamped up to 1.0 under escalation when
    /// the model was bypassed).
    Tear { est_speedup: f64 },
    /// Structurally sound but predicted slower than the monolith.
    RefusedUnprofitable { est_speedup: f64 },
    /// More than one candidate feed path; the scalar balance assumes one.
    RefusedAmbiguousFeed { feeds: usize },
}

/// One candidate balance tear, with everything the orchestrator and the
/// certificate need.
#[derive(Debug, Clone)]
pub struct BalanceTearCandidate {
    /// The rail node to tear.
    pub rail: NodeId,
    /// The pinned supply node feeding it.
    pub feed: NodeId,
    /// The series feed resistor (the shunt) and its value.
    pub shunt: DeviceId,
    pub shunt_ohms: f64,
    /// Block sizes (device counts) the island fragments into with the rail
    /// held as a boundary. Sorted descending.
    pub block_sizes: Vec<usize>,
    /// The verdict and its reasoning.
    pub decision: TearDecision,
}

impl BalanceTearCandidate {
    /// Convenience: was this candidate accepted?
    pub fn torn(&self) -> bool {
        matches!(self.decision, TearDecision::Tear { .. })
    }
}

/// Find every shunt-fed rail in the circuit and decide each one.
///
/// `graph` must be the [`ConductionGraph`] of the same circuit. Pinned-ness
/// (ideal-source-driven, propagated through stacked sources) is recomputed
/// here the same way the proven partitioner computes it.
pub fn detect_balance_tears(
    circuit: &Circuit,
    graph: &ConductionGraph,
    motive: TearMotive,
    policy: &RailPolicy,
) -> Vec<BalanceTearCandidate> {
    let n_nodes = circuit.max_node() as usize;
    if n_nodes == 0 {
        return Vec::new();
    }
    let pinned = pinned_nodes(circuit, n_nodes);

    // Structural scan (once): nonlinear fanout, ideal-source touches, and
    // every non-ground resistor at or under the shunt ceiling. Which end is
    // the feed is decided per discovery round, not here.
    let mut nl_fanout = vec![0usize; n_nodes + 1];
    let mut vsource_touch = vec![false; n_nodes + 1];
    let mut rlinks: Vec<(usize, usize, DeviceId, f64)> = Vec::new();

    for (id, dev) in circuit.iter() {
        if !dev.is_linear() {
            for n in dev.conduction_nodes() {
                if !n.is_ground() {
                    nl_fanout[n.0 as usize] += 1;
                }
            }
        }
        match dev {
            Device::Vsource { p, n, .. } => {
                for t in [p, n] {
                    if !t.is_ground() {
                        vsource_touch[t.0 as usize] = true;
                    }
                }
            }
            Device::Resistor { a, b, ohms, .. }
                if *ohms <= policy.max_shunt_ohms && !a.is_ground() && !b.is_ground() =>
            {
                rlinks.push((a.0 as usize, b.0 as usize, id, *ohms));
            }
            _ => {}
        }
    }

    // ---- Phase 1: transitive discovery. ---------------------------------
    // A node is a candidate rail when a shunt links it to a FEEDABLE node:
    // pinned, or a rail discovered earlier. This walks supply cascades to
    // the rail that actually fragments the board. Promotion is monotone, so
    // the fixpoint loop terminates.
    let mut feedable = pinned.clone();
    let mut is_candidate = vec![false; n_nodes + 1];
    let mut discovered: Vec<usize> = Vec::new();
    loop {
        let mut promoted = false;
        for &(a, b, _, _) in &rlinks {
            for (feed, rail) in [(a, b), (b, a)] {
                if !feedable[feed] || feedable[rail] {
                    continue;
                }
                if vsource_touch[rail] || nl_fanout[rail] < policy.min_nonlinear_fanout {
                    continue;
                }
                feedable[rail] = true;
                is_candidate[rail] = true;
                discovered.push(rail);
                promoted = true;
            }
        }
        if !promoted {
            break;
        }
    }

    // Feed links per candidate, oriented outward-in: a valid feed is
    // strictly shallower (pinned counts as depth 0, discovery order after),
    // so a child's shunt is never mistaken for a second feed of its parent.
    let mut depth = vec![usize::MAX; n_nodes + 1];
    for (k, &r) in discovered.iter().enumerate() {
        depth[r] = k + 1;
    }
    let depth_of = |n: usize| if pinned[n] { 0 } else { depth[n] };
    let mut links_of: Vec<Vec<(usize, DeviceId, f64)>> = vec![Vec::new(); n_nodes + 1];
    for &(a, b, id, ohms) in &rlinks {
        for (feed, rail) in [(a, b), (b, a)] {
            if is_candidate[rail] && feedable[feed] && depth_of(feed) < depth_of(rail) {
                links_of[rail].push((feed, id, ohms));
            }
        }
    }

    let candidate_of = |rail: usize| -> (NodeId, NodeId, DeviceId, f64) {
        let (feed, dev, ohms) = links_of[rail][0];
        (NodeId(rail as u32), NodeId(feed as u32), dev, ohms)
    };

    // ---- Phase 2: refusals, then a joint decision. -----------------------
    let mut out = Vec::new();
    let mut kept: Vec<usize> = Vec::new();
    for &rail in &discovered {
        if links_of[rail].len() > 1 {
            let (rail_node, feed, dev, ohms) = candidate_of(rail);
            out.push(BalanceTearCandidate {
                rail: rail_node,
                feed,
                shunt: dev,
                shunt_ohms: ohms,
                block_sizes: Vec::new(),
                decision: TearDecision::RefusedAmbiguousFeed {
                    feeds: links_of[rail].len(),
                },
            });
        } else {
            kept.push(rail);
        }
    }

    // No strand guard, by construction. Its old hazard (a device whose every
    // conduction terminal lands in {held rails, pinned, ground} loses its
    // current from every block's books) is closed at the source: the island
    // analysis gives such devices BOUNDARY-ONLY islands whose currents the
    // balance reads like any block's (`partition.rs`, the analysis-probe
    // finding about the flagship's rail decoupling). Tear shunts remain the
    // one exception and are excluded from that pass because their currents
    // are the analytic balance terms themselves.

    // Cascade parents join the joint decision like everyone else: the
    // balance executor carries the inter-rail shunt term (a parent's KCL
    // subtracts the current leaving toward each accepted child, whose shunt
    // is in no block; see `orchestrate::balance::RailChannel::children`), so a
    // parent does not have to stay fused to keep the books straight. Every
    // surviving candidate is held jointly and fragments once.
    let final_kept: Vec<usize> = kept;

    // Joint fragmentation and cost, per conduction island: every kept rail
    // is held while the island fragments once, because that is the system
    // the multi-rail balance loop solves. All of an island's kept rails
    // share the verdict (they win or lose as a set; per-subset search is a
    // calibration refinement, not a correctness need).
    let mut bound = pinned.clone();
    for &r in &final_kept {
        bound[r] = true;
    }
    let mut by_island: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for &r in &final_kept {
        if let Some(isl) = graph.node_island.get(r).copied().flatten() {
            by_island.entry(isl).or_default().push(r);
        }
    }
    for (isl, rails) in by_island {
        let block_sizes = fragment_sizes(circuit, graph, isl, &bound);
        let total: usize = block_sizes.iter().sum();
        let mono_cost = (total.max(1) as f64).powf(policy.alpha);
        let torn_cost: f64 = policy.outer_iters
            * block_sizes
                .iter()
                .map(|&b| (b.max(1) as f64).powf(policy.alpha))
                .sum::<f64>();
        let est_speedup = mono_cost / torn_cost.max(f64::MIN_POSITIVE);

        let decision = match motive {
            TearMotive::ConvergenceEscalation => TearDecision::Tear {
                est_speedup: est_speedup.max(1.0),
            },
            TearMotive::Profit if est_speedup > 1.0 => TearDecision::Tear { est_speedup },
            TearMotive::Profit => TearDecision::RefusedUnprofitable { est_speedup },
        };

        for rail in rails {
            let (rail_node, feed, dev, ohms) = candidate_of(rail);
            out.push(BalanceTearCandidate {
                rail: rail_node,
                feed,
                shunt: dev,
                shunt_ohms: ohms,
                block_sizes: block_sizes.clone(),
                decision: decision.clone(),
            });
        }
    }
    // Stable order for callers and reports.
    out.sort_unstable_by_key(|c| c.rail.0);
    out
}

/// One island's conduction fragmentation under a held set: which block each
/// free node and each block device lands in, plus the per-block device count.
///
/// This is the SINGLE fragmentation implementation. [`fragment_sizes`] (the
/// balance-tear cost model's input) and the stiff-node detector
/// ([`super::stiff`], which must search *inside* a block, not just size it)
/// both read it, so the two passes can never drift apart on what "one block"
/// means. Devices are fused through every non-ground, non-`bound` conduction
/// terminal, exactly the components a jointly-torn solve would see. A device
/// with NO free terminal (a shunt between two held rails, a source-side chain)
/// is the analytic/known side of the balance equations and is not a block; the
/// strand guard has already refused any rail where such a device's current
/// would actually be lost.
pub(crate) struct Fragmentation {
    /// Block representative (a node index) for each free node that lands in a
    /// block. Two free nodes share a value iff they are in the same block.
    pub(crate) node_block: std::collections::HashMap<usize, usize>,
    /// Block representative for each block device (a member that is not on the
    /// known side).
    pub(crate) device_block: std::collections::HashMap<DeviceId, usize>,
    /// Device count per block representative.
    pub(crate) block_devices: std::collections::HashMap<usize, usize>,
}

impl Fragmentation {
    /// Block device counts, descending: the balance-tear cost model's input.
    pub(crate) fn sizes(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.block_devices.values().copied().collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v
    }
}

/// Fragment one island with every node marked in `bound` (the pinned set plus
/// every held rail or stiff node) treated as a boundary. See [`Fragmentation`]
/// for the fusion rule and the known-side note.
pub(crate) fn fragment_blocks(
    circuit: &Circuit,
    graph: &ConductionGraph,
    island: usize,
    bound: &[bool],
) -> Fragmentation {
    let n_nodes = circuit.max_node() as usize;
    let mut parent: Vec<usize> = (0..n_nodes + 1).collect();
    fn find(parent: &mut [usize], mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }

    let known_side = |dev: &Device| -> bool {
        dev.conduction_nodes()
            .into_iter()
            .all(|n| n.is_ground() || bound[n.0 as usize])
    };

    // Iterate the island's own device list: the ids are right there, and a
    // whole-circuit scan with a linear membership test made every call
    // O(circuit x island). The stiff-node probe loop calls this hundreds of
    // times on the flagship's fused island, where that product was minutes
    // of analysis time (review finding on the detection commit).
    for &id in &graph.islands[island] {
        let dev = &circuit.devices[id.0 as usize];
        if known_side(dev) {
            continue;
        }
        let cond: Vec<usize> = dev
            .conduction_nodes()
            .into_iter()
            .filter(|n| !n.is_ground() && !bound[n.0 as usize])
            .map(|n| n.0 as usize)
            .collect();
        for w in cond.windows(2) {
            let (ra, rb) = (find(&mut parent, w[0]), find(&mut parent, w[1]));
            if ra != rb {
                parent[ra] = rb;
            }
        }
    }

    let mut block_devices: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::new();
    let mut device_block: std::collections::HashMap<DeviceId, usize> =
        std::collections::HashMap::new();
    let mut node_block: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for &id in &graph.islands[island] {
        let dev = &circuit.devices[id.0 as usize];
        if known_side(dev) {
            continue;
        }
        let free: Vec<usize> = dev
            .conduction_nodes()
            .into_iter()
            .filter(|n| !n.is_ground() && !bound[n.0 as usize])
            .map(|n| n.0 as usize)
            .collect();
        if let Some(&first) = free.first() {
            let root = find(&mut parent, first);
            device_block.insert(id, root);
            *block_devices.entry(root).or_insert(0) += 1;
            for n in free {
                let r = find(&mut parent, n);
                node_block.insert(n, r);
            }
        }
    }
    Fragmentation {
        node_block,
        device_block,
        block_devices,
    }
}

/// Sizes of the conduction blocks one island fragments into (descending). Thin
/// reader over [`fragment_blocks`]; see it for the fusion rule and the
/// known-side note.
fn fragment_sizes(
    circuit: &Circuit,
    graph: &ConductionGraph,
    island: usize,
    bound: &[bool],
) -> Vec<usize> {
    fragment_blocks(circuit, graph, island, bound).sizes()
}

/// Ideal-source pinned-ness, propagated across stacked sources: the same rule
/// the proven partitioner uses (`partition.rs`), kept as its own ~20 lines
/// rather than reaching into the partitioner, because the two layers must be
/// free to diverge (the decompose layer will learn regulator envelopes and
/// measured stiffness that the basic partitioner never needs). Shared
/// `pub(crate)` across the decompose passes: the stiff-node detector
/// ([`super::stiff`]) holds the same pinned set the rail pass does, so it reads
/// this rather than growing a second copy.
pub(crate) fn pinned_nodes(circuit: &Circuit, n_nodes: usize) -> Vec<bool> {
    let mut pinned = vec![false; n_nodes + 1];
    pinned[0] = true;
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
    pinned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::conduction::ConductionGraph;
    use crate::test_fixtures::{cap, pnp_blocks, res, shunt_array, vdc, GND};

    fn tears(c: &Circuit, motive: TearMotive) -> Vec<BalanceTearCandidate> {
        let g = ConductionGraph::analyze(c);
        detect_balance_tears(c, &g, motive, &RailPolicy::default())
    }

    /// A 24-block array tears profitably (the SIGN of the decision is what the
    /// toy pins); a ground bypass cap on the rail no longer refuses.
    #[test]
    fn wide_array_tears_profitably_even_with_a_bypass_cap() {
        let (mut c, rail, _, _, _) = shunt_array(24, 1e3);
        let t = tears(&c, TearMotive::Profit);
        assert_eq!(t.len(), 1, "{t:?}");
        assert_eq!(t[0].rail, rail);
        assert_eq!(t[0].block_sizes.len(), 24, "{:?}", t[0].block_sizes);
        assert!(
            matches!(t[0].decision, TearDecision::Tear { est_speedup } if est_speedup > 1.0),
            "{:?}",
            t[0].decision
        );

        cap(&mut c, "Cbypass", rail, GND, 100e-9);
        let t = tears(&c, TearMotive::Profit);
        assert_eq!(t.len(), 1);
        assert!(t[0].rail == rail && t[0].torn(), "{:?}", t[0].decision);
    }

    /// One giant block plus a trivial one does not fragment, so the cost model
    /// refuses; convergence escalation overrides the cost gate.
    #[test]
    fn unfragmented_core_refuses_on_cost() {
        let (mut c, rail, _, _, blocks) = shunt_array(2, 1e3);
        res(&mut c, "Rbridge", blocks[0].0, blocks[1].0, 1e3);
        let t = tears(&c, TearMotive::Profit);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].rail, rail);
        assert!(
            matches!(t[0].decision, TearDecision::RefusedUnprofitable { .. }),
            "{:?}",
            t[0].decision
        );
        let esc = tears(&c, TearMotive::ConvergenceEscalation);
        assert!(esc[0].torn(), "{:?}", esc[0].decision);
    }

    /// source -> R1 -> MID (two loads) -> R2 -> INNER (24-block array):
    /// transitive discovery reaches INNER, and both rails tear jointly.
    #[test]
    fn stacked_cascade_reaches_and_tears_the_inner_rail() {
        let mut c = Circuit::new();
        let src = c.node("+5V_SRC");
        vdc(&mut c, "VS", src, 5.0);
        let (mid, inner) = (c.node("MID"), c.node("INNER"));
        res(&mut c, "R1", src, mid, 500.0);
        res(&mut c, "R2", mid, inner, 1e3);
        pnp_blocks(&mut c, "m", mid, 2, GND, 100e3, 100e3);
        pnp_blocks(&mut c, "i", inner, 24, GND, 100e3, 100e3);
        let t = tears(&c, TearMotive::Profit);
        let inner_cand = t
            .iter()
            .find(|t| t.rail == inner)
            .expect("transitive discovery reaches INNER");
        assert!(
            matches!(inner_cand.decision, TearDecision::Tear { .. }),
            "{t:?}"
        );
        assert_eq!(inner_cand.feed, mid);
        let mid_cand = t
            .iter()
            .find(|t| t.rail == mid)
            .expect("MID is a candidate too");
        assert!(
            matches!(mid_cand.decision, TearDecision::Tear { .. }),
            "{t:?}"
        );
        assert_eq!(mid_cand.feed, src);
    }

    /// A rail pinned by its own ideal source is not a candidate; two plausible
    /// shunt feeds refuse as ambiguous even under escalation.
    #[test]
    fn pinned_and_ambiguously_fed_rails_are_not_torn() {
        let (mut c, rail, _, _, _) = shunt_array(8, 1e3);
        vdc(&mut c, "Vhard", rail, 5.0);
        assert!(tears(&c, TearMotive::Profit).is_empty());

        let (mut c, rail, _, _, _) = shunt_array(8, 1e3);
        let p3 = c.node("+3V3");
        vdc(&mut c, "V3", p3, 3.3);
        res(&mut c, "R_shunt_b", p3, rail, 1e3);
        let t = tears(&c, TearMotive::ConvergenceEscalation);
        let cand = t.iter().find(|t| t.rail == rail).expect("candidate");
        assert_eq!(
            cand.decision,
            TearDecision::RefusedAmbiguousFeed { feeds: 2 }
        );
        assert!(!cand.torn());
    }
}
