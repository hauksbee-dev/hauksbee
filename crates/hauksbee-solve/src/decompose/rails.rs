//! Stiff-rail detection: where a balance tear fragments a fused core.
//!
//! The shape this hunts is the one that defeated the Tarski monolith
//! (`docs/dev-plans/research/tarski-saga.md` §1a): a supply rail fed through
//! one series impedance, loaded by many nonlinear blocks. Every block couples
//! to every other only through the scalar rail voltage, so the system is
//! bordered-block-diagonal and tears *exactly* into per-block solves plus one
//! scalar KCL balance at the rail (the balance tear of
//! `02-tearing-architecture.md` §1; `partition::analyze_with_tears` and its
//! `rail_tear.rs` round-off gate are the proven solver-side mechanics this
//! module decides *when* to use).
//!
//! ## What replaced the magic numbers
//!
//! The first implementation gated tearing on two tuned constants (a nonlinear
//! fanout of 8, a block-size cap of 600, both calibrated on one board). Here
//! the decision is a first-order cost model instead, and the old block-size
//! cap falls out of it: if tearing fails to fragment the core (one block
//! nearly the whole island), the torn cost is the monolithic cost times the
//! outer-loop count and the model refuses on its own. No threshold needed.
//!
//! The model is deliberately crude and says so: per-step Newton cost on an
//! island of `n` devices is estimated as `n^ALPHA` with `ALPHA = 1.4`, the
//! textbook-ish fill exponent for sparse LU on circuit matrices (between the
//! linear ideal of a perfect elimination order and the quadratic of a dense
//! band; the *decision* only needs the ratio to be roughly right, correctness
//! never depends on it). A torn solve pays `OUTER_ITERS` re-solves of every
//! rail-loading block for the scalar balance (the secant loop converges in
//! about three trials on the proven fixtures). Both constants carry their
//! provenance here and are policy fields, not buried literals.
//!
//! ## Two ways in
//!
//! Profitability is not the only trigger. The flagship motivation is a board
//! whose monolith never converges at any cost, so the caller can pass
//! [`TearMotive::ConvergenceEscalation`]: structural guards still apply (an
//! unsound tear stays refused), but the cost gate is bypassed, because a slow
//! answer beats no answer. This is the "decompose" rung of the robustness
//! ladder (`02-tearing-architecture.md` §2.2, §2.6).
//!
//! ## Stacked feeds (the flagship's actual shape)
//!
//! Real supplies cascade: on the Tarski board the path is source -> +5V ->
//! 1k shunt -> ANALOG_VDD, and single-hop detection (feed must be
//! source-pinned) stops one hop short of the rail that fragments the board
//! (the analysis probe's founding finding). Discovery is therefore
//! transitive: a discovered rail is a valid feed for the next hop, walked to
//! a fixpoint. Decisions are then JOINT: every surviving candidate is held
//! as a boundary while each island's fragmentation is computed once, because
//! that is the system the multi-rail balance executor actually solves.
//! Within a cascade, only the deepest accepted rail tears today; its parent
//! refuses as [`TearDecision::RefusedCascadeParent`] (see that variant for
//! the analytic term the executor still lacks), which is exact and merely
//! forfeits the parent's own speedup.
//!
//! ## What can refuse a tear
//!
//! * **A stranded device** (the bypass-cap rule, proven load-bearing by
//!   `rail_with_ground_bypass_cap_is_not_torn`): a device whose conduction
//!   terminals all land in {held rails, pinned, ground} would have its
//!   current dropped from every block's books, silently. Refused, with the
//!   device named in the decision; candidate shunts are exempt (their
//!   currents ARE the balance equations). Stranding is evaluated in the
//!   joint context and re-checked as refusals shrink the held set (a rail
//!   dropping out can un-strand a device).
//! * **An ideal source on the rail**: already pinned, nothing to tear.
//! * **Ambiguous feed**: more than one low-impedance path from shallower
//!   feeds; the scalar-balance bookkeeping assumes one.
//! * **Cascade parent of an accepted tear**: see above.
//! * **No fragmentation / unprofitable**: the cost model above (unless
//!   escalating).
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
    /// A device's current books would silently lose the rail current.
    RefusedStranded { device: DeviceId },
    /// More than one candidate feed path; the scalar balance assumes one.
    RefusedAmbiguousFeed { feeds: usize },
    /// This rail directly feeds another ACCEPTED tear through its shunt. Its
    /// own balance equation would need the analytic inter-rail shunt term
    /// (the child's shunt current appears in the parent's books, but the
    /// child's block set no longer contains that shunt), which the executor
    /// does not carry yet. Refusing the parent is exact: it stays fused in
    /// the feed-side block and merely forfeits its own speedup. Lift this
    /// refusal when `settle_rails` gains the cascade term.
    RefusedCascadeParent { child: NodeId },
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

    // Joint strand guard, iterated: with every kept rail held, a device
    // whose conduction terminals all land in {held rails, pinned, ground}
    // appears in no block's books. It refuses every rail it touches; a
    // refusal frees terminals, so re-check until stable. Candidate shunts
    // are exempt (their currents are balance equations).
    let shunt_devs: std::collections::HashSet<DeviceId> =
        kept.iter().map(|&r| links_of[r][0].1).collect();
    loop {
        let mut held = vec![false; n_nodes + 1];
        for &r in &kept {
            held[r] = true;
        }
        let mut newly_refused: Vec<(usize, DeviceId)> = Vec::new();
        for (id, dev) in circuit.iter() {
            if shunt_devs.contains(&id) || matches!(dev, Device::Vsource { .. }) {
                continue;
            }
            let cond = dev.conduction_nodes();
            let touched: Vec<usize> = cond
                .iter()
                .filter(|n| !n.is_ground() && held[n.0 as usize])
                .map(|n| n.0 as usize)
                .collect();
            if touched.is_empty() {
                continue;
            }
            let all_bounded = cond
                .iter()
                .all(|n| n.is_ground() || pinned[n.0 as usize] || held[n.0 as usize]);
            if all_bounded {
                for r in touched {
                    if !newly_refused.iter().any(|(rr, _)| *rr == r) {
                        newly_refused.push((r, id));
                    }
                }
            }
        }
        if newly_refused.is_empty() {
            break;
        }
        for (rail, device) in newly_refused {
            kept.retain(|&r| r != rail);
            let (rail_node, feed, dev, ohms) = candidate_of(rail);
            out.push(BalanceTearCandidate {
                rail: rail_node,
                feed,
                shunt: dev,
                shunt_ohms: ohms,
                block_sizes: Vec::new(),
                decision: TearDecision::RefusedStranded { device },
            });
        }
    }

    // Cascade parents defer to their deepest surviving child (see the
    // TearDecision variant for the executor term this waits on). Walk
    // deepest-first so a parent whose child refused is itself kept.
    kept.sort_unstable_by_key(|&r| std::cmp::Reverse(depth_of(r)));
    let mut final_kept: Vec<usize> = Vec::new();
    for &r in &kept {
        if let Some(&child) = final_kept
            .iter()
            .find(|&&c| links_of[c][0].0 == r)
        {
            let (rail_node, feed, dev, ohms) = candidate_of(r);
            out.push(BalanceTearCandidate {
                rail: rail_node,
                feed,
                shunt: dev,
                shunt_ohms: ohms,
                block_sizes: Vec::new(),
                decision: TearDecision::RefusedCascadeParent {
                    child: NodeId(child as u32),
                },
            });
        } else {
            final_kept.push(r);
        }
    }

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

/// Sizes of the conduction blocks one island fragments into when every node
/// marked in `bound` (the pinned set plus every held rail) is a boundary
/// (descending). Devices are fused through every non-ground, non-bound
/// conduction terminal, exactly the components a jointly-torn solve would
/// see. A device with NO free terminal (a shunt between two held rails, a
/// source-side chain) is the analytic/known side of the balance equations
/// and is not a block; the strand guard has already refused any rail where
/// such a device's current would actually be lost.
fn fragment_sizes(
    circuit: &Circuit,
    graph: &ConductionGraph,
    island: usize,
    bound: &[bool],
) -> Vec<usize> {
    let n_nodes = circuit.max_node() as usize;
    let mut parent: Vec<usize> = (0..n_nodes + 1).collect();
    fn find(parent: &mut Vec<usize>, mut x: usize) -> usize {
        while parent[x] != x {
            parent[x] = parent[parent[x]];
            x = parent[x];
        }
        x
    }

    let member = |id: DeviceId| -> bool { graph.islands[island].contains(&id) };

    let known_side = |dev: &Device| -> bool {
        dev.conduction_nodes()
            .into_iter()
            .all(|n| n.is_ground() || bound[n.0 as usize])
    };

    for (id, dev) in circuit.iter() {
        if !member(id) || known_side(dev) {
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

    let mut sizes: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (id, dev) in circuit.iter() {
        if !member(id) || known_side(dev) {
            continue;
        }
        if let Some(rep) = dev
            .conduction_nodes()
            .into_iter()
            .find(|n| !n.is_ground() && !bound[n.0 as usize])
        {
            *sizes.entry(find(&mut parent, rep.0 as usize)).or_insert(0) += 1;
        }
    }
    let mut v: Vec<usize> = sizes.into_values().collect();
    v.sort_unstable_by(|a, b| b.cmp(a));
    v
}

/// Ideal-source pinned-ness, propagated across stacked sources: the same rule
/// the proven partitioner uses (`partition.rs`), duplicated at ~20 lines
/// rather than exported, because the two layers must be free to diverge (the
/// decompose layer will learn regulator envelopes and measured stiffness that
/// the basic partitioner never needs).
fn pinned_nodes(circuit: &Circuit, n_nodes: usize) -> Vec<bool> {
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
    use hauksbee_ir::{BjtModel, Polarity, SourceKind};

    /// A shunt-fed PNP mirror array, the `rail_tear.rs` shape: +5V -> 1k shunt
    /// -> ANALOG_VDD rail -> n emitter-coupled blocks of (BJT + load R).
    fn shunt_array(n_blocks: usize) -> (Circuit, NodeId) {
        let mut c = Circuit::new();
        let p5 = c.node("+5V");
        c.add(Device::Vsource {
            name: "V5".into(),
            p: p5,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        let rail = c.node("ANALOG_VDD");
        c.add(Device::Resistor {
            name: "R_shunt".into(),
            a: p5,
            b: rail,
            ohms: 1e3,
            tc1: None,
        });
        let model = BjtModel {
            polarity: Polarity::P,
            ..BjtModel::default()
        };
        for k in 0..n_blocks {
            let base = c.node(&format!("b{k}"));
            let col = c.node(&format!("c{k}"));
            c.add(Device::Bjt {
                name: format!("Q{k}"),
                c: col,
                b: base,
                e: rail,
                model: model.clone(),
            });
            c.add(Device::Resistor {
                name: format!("Rb{k}"),
                a: base,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
            c.add(Device::Resistor {
                name: format!("Rc{k}"),
                a: col,
                b: NodeId::GROUND,
                ohms: 10e3,
                tc1: None,
            });
        }
        (c, rail)
    }

    #[test]
    fn wide_array_tears_profitably() {
        let (c, rail) = shunt_array(24);
        let g = ConductionGraph::analyze(&c);
        let tears = detect_balance_tears(&c, &g, TearMotive::Profit, &RailPolicy::default());
        assert_eq!(tears.len(), 1, "{tears:?}");
        let t = &tears[0];
        assert_eq!(t.rail, rail);
        assert_eq!(t.block_sizes.len(), 24, "{:?}", t.block_sizes);
        match t.decision {
            TearDecision::Tear { est_speedup } => {
                // At toy scale (3-device blocks) the modeled win is slim: the
                // outer loop's 3x re-solve nearly cancels the fragmentation
                // gain, which is honest (per-block overhead dominates small
                // blocks). The margin grows with block size; what this fixture
                // pins is the SIGN of the decision, not its magnitude.
                assert!(est_speedup > 1.0, "24 equal blocks must win: {est_speedup}")
            }
            ref other => panic!("expected Tear, got {other:?}"),
        }
    }

    /// The old TEAR_MAX_BLOCK_DEVICES=600 cap, now emergent: an "array" that
    /// is one giant block plus a couple of trivial ones does not fragment, so
    /// the cost model refuses without any size threshold.
    #[test]
    fn unfragmented_core_refuses_on_cost() {
        let (mut c, rail) = shunt_array(2);
        // Fuse the two blocks into one giant block with base-to-base bridges,
        // leaving the rail's island effectively monolithic after the tear.
        let b0 = c.node("b0");
        let b1 = c.node("b1");
        c.add(Device::Resistor {
            name: "Rbridge".into(),
            a: b0,
            b: b1,
            ohms: 1e3,
            tc1: None,
        });
        let g = ConductionGraph::analyze(&c);
        let tears = detect_balance_tears(&c, &g, TearMotive::Profit, &RailPolicy::default());
        assert_eq!(tears.len(), 1);
        assert_eq!(tears[0].rail, rail);
        assert!(
            matches!(
                tears[0].decision,
                TearDecision::RefusedUnprofitable { .. }
            ),
            "one fused block cannot win the outer loop: {:?}",
            tears[0].decision
        );
        // ... but convergence escalation overrides the cost gate.
        let esc = detect_balance_tears(
            &c,
            &g,
            TearMotive::ConvergenceEscalation,
            &RailPolicy::default(),
        );
        assert!(esc[0].torn(), "{:?}", esc[0].decision);
    }

    /// The load-bearing refusal, ported: a rail-to-ground bypass cap would be
    /// stranded by the tear (its current drops from every block), so the rail
    /// must refuse regardless of motive.
    #[test]
    fn ground_bypass_cap_strands_and_refuses() {
        let (mut c, rail) = shunt_array(24);
        c.add(Device::Capacitor {
            name: "Cbypass".into(),
            a: rail,
            b: NodeId::GROUND,
            farads: 100e-9,
            ic: None,
        });
        let g = ConductionGraph::analyze(&c);
        for motive in [TearMotive::Profit, TearMotive::ConvergenceEscalation] {
            let tears = detect_balance_tears(&c, &g, motive, &RailPolicy::default());
            assert_eq!(tears.len(), 1);
            assert!(
                matches!(tears[0].decision, TearDecision::RefusedStranded { .. }),
                "bypass cap must refuse the tear ({motive:?}): {:?}",
                tears[0].decision
            );
        }
    }

    /// The flagship's founding shape (analysis-probe finding 1): a supply
    /// CASCADE, source -> R1 -> MID -> R2 -> INNER, with the big array on
    /// INNER and a couple of loads on MID. Single-hop detection never
    /// reached INNER; transitive discovery must, and the cascade rule must
    /// tear the deepest rail while the parent defers explainably.
    #[test]
    fn stacked_cascade_reaches_and_tears_the_inner_rail() {
        let mut c = Circuit::new();
        let src = c.node("+5V_SRC");
        c.add(Device::Vsource {
            name: "VS".into(),
            p: src,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        let mid = c.node("MID");
        c.add(Device::Resistor {
            name: "R1".into(),
            a: src,
            b: mid,
            ohms: 500.0,
            tc1: None,
        });
        let inner = c.node("INNER");
        c.add(Device::Resistor {
            name: "R2".into(),
            a: mid,
            b: inner,
            ohms: 1e3,
            tc1: None,
        });
        let model = BjtModel {
            polarity: Polarity::P,
            ..BjtModel::default()
        };
        // Two small loads keep MID above the structural fanout floor.
        for k in 0..2 {
            let b = c.node(&format!("mb{k}"));
            let col = c.node(&format!("mc{k}"));
            c.add(Device::Bjt {
                name: format!("MQ{k}"),
                c: col,
                b,
                e: mid,
                model: model.clone(),
            });
            c.add(Device::Resistor {
                name: format!("MRb{k}"),
                a: b,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
            c.add(Device::Resistor {
                name: format!("MRc{k}"),
                a: col,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
        }
        // The array on INNER is what makes the cascade worth walking.
        for k in 0..24 {
            let b = c.node(&format!("ib{k}"));
            let col = c.node(&format!("ic{k}"));
            c.add(Device::Bjt {
                name: format!("IQ{k}"),
                c: col,
                b,
                e: inner,
                model: model.clone(),
            });
            c.add(Device::Resistor {
                name: format!("IRb{k}"),
                a: b,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
            c.add(Device::Resistor {
                name: format!("IRc{k}"),
                a: col,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
        }

        let g = ConductionGraph::analyze(&c);
        let tears = detect_balance_tears(&c, &g, TearMotive::Profit, &RailPolicy::default());
        let inner_cand = tears
            .iter()
            .find(|t| t.rail == inner)
            .expect("transitive discovery must reach INNER");
        assert!(
            matches!(inner_cand.decision, TearDecision::Tear { .. }),
            "the deep array rail must tear: {:?}",
            inner_cand.decision
        );
        assert_eq!(inner_cand.feed, mid, "INNER is fed from MID");
        let mid_cand = tears
            .iter()
            .find(|t| t.rail == mid)
            .expect("MID is a candidate too");
        assert_eq!(
            mid_cand.decision,
            TearDecision::RefusedCascadeParent { child: inner },
            "the parent defers to its accepted child"
        );
    }

    /// A rail pinned by its own ideal source is not a candidate at all.
    #[test]
    fn source_pinned_rail_is_not_a_candidate() {
        let (mut c, rail) = shunt_array(8);
        c.add(Device::Vsource {
            name: "Vhard".into(),
            p: rail,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        let g = ConductionGraph::analyze(&c);
        let tears = detect_balance_tears(&c, &g, TearMotive::Profit, &RailPolicy::default());
        assert!(tears.is_empty(), "{tears:?}");
    }

    /// Two plausible shunt feeds into one rail: the scalar balance assumes a
    /// single feed, so the detector must refuse rather than pick one
    /// (section 3.4 refusal-completeness: every assumption gets a violating
    /// fixture).
    #[test]
    fn two_feeds_refuse_ambiguous() {
        let (mut c, rail) = {
            let mut c = Circuit::new();
            let p5 = c.node("+5V");
            c.add(Device::Vsource {
                name: "V5".into(),
                p: p5,
                n: NodeId::GROUND,
                kind: SourceKind::Dc(5.0),
            });
            let rail = c.node("ANALOG_VDD");
            c.add(Device::Resistor {
                name: "R_shunt_a".into(),
                a: p5,
                b: rail,
                ohms: 1e3,
                tc1: None,
            });
            (c, rail)
        };
        // Second pinned feed path into the same rail.
        let p3 = c.node("+3V3");
        c.add(Device::Vsource {
            name: "V3".into(),
            p: p3,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.3),
        });
        c.add(Device::Resistor {
            name: "R_shunt_b".into(),
            a: p3,
            b: rail,
            ohms: 1e3,
            tc1: None,
        });
        let model = BjtModel {
            polarity: Polarity::P,
            ..BjtModel::default()
        };
        for k in 0..8 {
            let base = c.node(&format!("b{k}"));
            let col = c.node(&format!("c{k}"));
            c.add(Device::Bjt {
                name: format!("Q{k}"),
                c: col,
                b: base,
                e: rail,
                model: model.clone(),
            });
            c.add(Device::Resistor {
                name: format!("Rb{k}"),
                a: base,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
            c.add(Device::Resistor {
                name: format!("Rc{k}"),
                a: col,
                b: NodeId::GROUND,
                ohms: 10e3,
                tc1: None,
            });
        }
        let g = ConductionGraph::analyze(&c);
        // Even under escalation (structural guards never relax), two feeds
        // must refuse.
        let tears =
            detect_balance_tears(&c, &g, TearMotive::ConvergenceEscalation, &RailPolicy::default());
        let cand = tears.iter().find(|t| t.rail == rail).expect("candidate");
        assert_eq!(cand.decision, TearDecision::RefusedAmbiguousFeed { feeds: 2 });
        assert!(!cand.torn());
    }
}
