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
//! ## What can refuse a tear
//!
//! * **A stranded device** (the bypass-cap rule, proven load-bearing by
//!   `rail_with_ground_bypass_cap_is_not_torn`): a device whose conduction
//!   terminals all land in {rail, pinned, ground} would have its current
//!   dropped from every block's books, silently. Refused, with the device
//!   named in the decision.
//! * **An ideal source on the rail**: already pinned, nothing to tear.
//! * **Ambiguous feed**: more than one low-impedance path to pinned supply
//!   nodes; the scalar-balance bookkeeping assumes one.
//! * **No fragmentation / unprofitable**: the cost model above (unless
//!   escalating).

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
    /// than the monolithic solve (>= 1.0; exactly 1.0 under escalation when
    /// the model was bypassed).
    Tear { est_speedup: f64 },
    /// Structurally sound but predicted slower than the monolith.
    RefusedUnprofitable { est_speedup: f64 },
    /// A device's current books would silently lose the rail current.
    RefusedStranded { device: DeviceId },
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

    // Structural scan: nonlinear fanout per node, ideal-source touches, and
    // candidate shunt links (resistor from a pinned non-ground supply into an
    // unpinned node).
    let mut nl_fanout = vec![0usize; n_nodes + 1];
    let mut vsource_touch = vec![false; n_nodes + 1];
    let mut shunt_links: Vec<Vec<(usize, DeviceId, f64)>> = vec![Vec::new(); n_nodes + 1];

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
            Device::Resistor { a, b, ohms, .. } if *ohms <= policy.max_shunt_ohms => {
                let (ai, bi) = (a.0 as usize, b.0 as usize);
                if pinned[ai] && !a.is_ground() && !pinned[bi] && !b.is_ground() {
                    shunt_links[bi].push((ai, id, *ohms));
                } else if pinned[bi] && !b.is_ground() && !pinned[ai] && !a.is_ground() {
                    shunt_links[ai].push((bi, id, *ohms));
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    for rail in 1..=n_nodes {
        if pinned[rail] || vsource_touch[rail] {
            continue;
        }
        if shunt_links[rail].is_empty() {
            continue;
        }
        if nl_fanout[rail] < policy.min_nonlinear_fanout {
            continue;
        }
        let rail_node = NodeId(rail as u32);

        if shunt_links[rail].len() > 1 {
            let (feed, dev, ohms) = shunt_links[rail][0];
            out.push(BalanceTearCandidate {
                rail: rail_node,
                feed: NodeId(feed as u32),
                shunt: dev,
                shunt_ohms: ohms,
                block_sizes: Vec::new(),
                decision: TearDecision::RefusedAmbiguousFeed {
                    feeds: shunt_links[rail].len(),
                },
            });
            continue;
        }
        let (feed, shunt_dev, shunt_ohms) = shunt_links[rail][0];

        // Strand guard (the bypass-cap rule). A device is stranded if every
        // conduction terminal it has lands in {this rail, pinned, ground}:
        // torn, its current appears in no block's KCL and is silently lost.
        // The shunt itself is exempt: its current IS the balance equation.
        let mut stranded: Option<DeviceId> = None;
        for (id, dev) in circuit.iter() {
            if id == shunt_dev || matches!(dev, Device::Vsource { .. }) {
                continue;
            }
            let cond = dev.conduction_nodes();
            let touches_rail = cond.iter().any(|n| n.0 as usize == rail);
            if !touches_rail {
                continue;
            }
            let all_bounded = cond.iter().all(|n| {
                n.is_ground() || n.0 as usize == rail || pinned[n.0 as usize]
            });
            if all_bounded {
                stranded = Some(id);
                break;
            }
        }
        if let Some(device) = stranded {
            out.push(BalanceTearCandidate {
                rail: rail_node,
                feed: NodeId(feed as u32),
                shunt: shunt_dev,
                shunt_ohms,
                block_sizes: Vec::new(),
                decision: TearDecision::RefusedStranded { device },
            });
            continue;
        }

        // Fragmentation: re-run the conduction components of the rail's own
        // island with the rail excluded from fusing. Block sizes drive the
        // cost model.
        let block_sizes = fragment_sizes(circuit, graph, rail_node, &pinned);
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

        out.push(BalanceTearCandidate {
            rail: rail_node,
            feed: NodeId(feed as u32),
            shunt: shunt_dev,
            shunt_ohms,
            block_sizes,
            decision,
        });
    }
    out
}

/// Sizes of the conduction blocks the rail's island fragments into when the
/// rail is held as a boundary (descending). Devices are fused through every
/// non-ground conduction terminal EXCEPT the rail, exactly the components a
/// balance-torn solve would see.
fn fragment_sizes(
    circuit: &Circuit,
    graph: &ConductionGraph,
    rail: NodeId,
    pinned: &[bool],
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

    let rail_island = graph
        .node_island
        .get(rail.0 as usize)
        .copied()
        .flatten();
    let member = |id: DeviceId| -> bool {
        match rail_island {
            Some(isl) => graph.islands[isl].contains(&id),
            None => false,
        }
    };

    // Feed-side devices (every non-rail conduction terminal pinned or ground:
    // the source and the shunt itself) are the KNOWN side of the balance
    // equation; the outer loop never re-solves them, so they are not blocks.
    let feed_side = |dev: &Device| -> bool {
        dev.conduction_nodes()
            .into_iter()
            .filter(|n| *n != rail)
            .all(|n| n.is_ground() || pinned[n.0 as usize])
    };

    for (id, dev) in circuit.iter() {
        if !member(id) || feed_side(dev) {
            continue;
        }
        let cond: Vec<usize> = dev
            .conduction_nodes()
            .into_iter()
            .filter(|n| !n.is_ground() && *n != rail)
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
        if !member(id) || feed_side(dev) {
            continue;
        }
        if let Some(rep) = dev
            .conduction_nodes()
            .into_iter()
            .find(|n| !n.is_ground() && *n != rail)
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
}
