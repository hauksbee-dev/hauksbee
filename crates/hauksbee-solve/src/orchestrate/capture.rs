//! The stiff-tear executor: capture with the load attached, verify by
//! re-capture, certify the measured sag.
//!
//! Detection ([`crate::decompose::stiff`]) nominates conducted cut nodes that
//! fragment a fused block cheaply; whether any nominee is actually STIFF is a
//! measured property, and this module does the measuring. The mechanism
//! generalizes the bespoke `tarski_decomp` stage A/B, with one simplification
//! the Gauss-Seidel structure turns out to imply:
//!
//! * **Round 0 (capture with the load attached).** Each candidate's waveform
//!   is solved on its CAPTURE GROUP: the fragment blocks adjacent to it, with
//!   the candidate free and every OTHER candidate pinned at its rest estimate.
//!   The load's draw is therefore in the captured waveform from the start,
//!   which is the part naive stiffness pinning misses.
//! * **Rounds 1 and 2 (train-driven, the second is the verification).**
//!   Round 1 re-captures each candidate with the others pinned at their
//!   round-0 trains; round 2 repeats against the round-1 trains. The
//!   certified `sag_v = max_t |v2(t) - v1(t)|` is the Gauss-Seidel
//!   fixed-point residual between two consecutive TRAIN-DRIVEN iterates.
//!   Comparing round 1 against round 0 instead would conflate the residual
//!   with legitimate signal propagation: when one candidate's drive arrives
//!   THROUGH another (a chain), the rest assumption suppresses the signal
//!   itself and the difference is volts of physics, not error. The chain
//!   fixture below caught exactly that design mistake.
//! * **No separate replay pass.** Every fragment block borders at least one
//!   candidate (that is what made it a fragment), so the round-2 captures
//!   already contain every block's waveforms with all trains live. Assembly
//!   reads each block from one adjacent candidate's round-2 solve; the
//!   recorded sag bounds the disagreement between the possible choices.
//!
//! What this iteration actually is: waveform relaxation (Gauss-Seidel over
//! sub-circuits), and that upgrades the claim beyond the plan's stiffness
//! framing. If the iteration CONVERGES, the assembled answer is a fixed
//! point of the true equations, full stop; physical stiffness only sets the
//! convergence RATE (a passive chain with soft impedances still contracts
//! and lands on the exact answer, where one-shot rest-pinned capture, the
//! bespoke stage-A concept, would have been visibly wrong). What refuses is
//! NON-CONVERGENCE within the round budget, and the canonical non-converger
//! is a cut through an ACTIVE feedback loop with gain above one, which is
//! precisely the cut nobody should be allowed to fake through: the group
//! falls back to the exact fused solve and the outcome carries the residual
//! that refused it. The tolerance follows the plan's `10 x reltol x Vnom`
//! with `Vnom` from the candidate's own rest level, floored at 1 V.
//!
//! ## Rest estimates without a converging monolith
//!
//! Round 0 needs a rest value per candidate, and on the flagship the fused DC
//! does not converge, so "solve the whole group once" is not available. The
//! bootstrap: try the whole-group DC; if it fails, solve each candidate's
//! capture group DC with the others at 0 V and use the resulting candidate
//! voltages as the rest set (one extra Gauss-Seidel half-round, recorded in
//! the outcome as `bootstrapped`). Rest estimates only seed round 0; their
//! error is measured out by the round-1 sag like every other assumption here.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/orchestrate.md

use std::collections::{HashMap, HashSet};

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, PwlPoint, SourceKind};

use crate::decompose::conduction::ConductionGraph;
use crate::decompose::rails::fragment_blocks;
use crate::newton::{dc_operating_point, Workspace};
use crate::options::{Partitioning, SolverOptions, StepControl};
use crate::partition::{Partition, RailTear};
use crate::partitioned::PartitionedTransient;
use crate::transient::{Transient, Waveforms};
use crate::{SolveError, SolveResult};

/// What KIND of boundary an outcome describes: the structured discriminator
/// callers (certificate construction above all) must branch on. The prose
/// `note` field is telemetry for humans; discriminating on note STRINGS is how
/// a feed-held rail once shipped a balance-exact certificate record (review
/// finding on the first composed commit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    /// An ordinary signal cut: relaxed, with a measured sag.
    Signal,
    /// A rail closed by the exact per-step scalar KCL balance.
    BalancedRail,
    /// A rail PINNED at its feed voltage: no balance ran, no sag was
    /// measured; an assumption on the stiffness of the supply leg. Must
    /// never be certified balance-exact.
    HeldRail,
}

/// What happened at one stiff boundary.
#[derive(Debug, Clone)]
pub struct StiffOutcome {
    /// The cut node (in the caller's node space).
    pub node: NodeId,
    /// The structured boundary kind (see [`BoundaryKind`]): what the
    /// certificate may claim hangs off this, never off `note`.
    pub kind: BoundaryKind,
    /// The relaxation residual between the final two train-driven iterates
    /// at this boundary, time-shift-tolerant by one grid step (each sample
    /// compares against the other iterate's neighbouring cells): the
    /// capture-grid claim's own metric, and the reason spiking trains can
    /// converge instead of reading a one-cell spike shift as volts.
    pub sag_v: f64,
    /// The tolerance it was judged against (10 x reltol x Vnom).
    pub tol_v: f64,
    /// Whether this boundary's residual met its tolerance. The group is
    /// accepted only when every boundary converged.
    pub accepted: bool,
    /// True when rest estimates came from the per-group DC bootstrap rather
    /// than a converging whole-group DC.
    pub bootstrapped: bool,
    /// How many blocks the generator-inclusive capture pre-pass ADDED to this
    /// candidate's plain-adjacency capture group (0 = plain adjacency was
    /// enough). Nonzero means the capture reached past the load to pull in the
    /// generation of the captured node (a sense-coupled drive loop, a nearby
    /// source): the fix for the dead-membrane / quiet-basin hazard. Telemetry
    /// only; the certificate claim still hangs off `kind`/`sag_v`.
    pub capture_growth: usize,
    /// One line of provenance: why this outcome says what it says (an
    /// out-of-scope disqualification, a dead capture, or empty for a
    /// normally-measured boundary). Refusals must never be wordless.
    pub note: &'static str,
}

/// One capture run and its global-to-local node map.
type CaptureRun = (Waveforms, HashMap<u32, u32>);

/// Boundary trains for the pins: per-candidate series over the shared grid.
type BoundaryTrains<'a> = (&'a HashMap<u32, Vec<f64>>, &'a [f64]);

/// A stiff-torn group execution: the group's waveforms plus the per-boundary
/// honesty numbers.
#[derive(Debug)]
pub struct StiffExecution {
    pub waveforms: Waveforms,
    pub outcomes: Vec<StiffOutcome>,
}

/// Tunables for the generator-inclusive capture pre-pass, with provenance.
///
/// A candidate's capture group is, by the plain rule, the fragment blocks
/// ADJACENT to it (its load). That is exactly enough for a node whose waveform
/// is a passive function of its neighbours, and exactly WRONG for a node whose
/// waveform is GENERATED by an active loop it reaches only through sense edges
/// (the flagship's hidden `V_out`: driven by an output stage that SENSES a
/// membrane living in a non-adjacent block, so the plain capture holds a static
/// membrane and the output relaxes flat). This pre-pass grows the group past the
/// load to pull that generation in; the field below bounds the walk so it can
/// never re-fuse a whole island.
#[derive(Debug, Clone, Copy)]
pub struct CapturePolicy {
    /// Largest number of blocks the sense-follow may PULL beyond plain
    /// adjacency, per candidate. `0` is the pre-fix behaviour (adjacency only).
    /// The default is generous relative to a real generator loop (a membrane
    /// block, a comparator block, a reset-switch block, their references: a
    /// handful) yet finite, so a pathological walk truncates rather than running
    /// away. Sized well below a fused island so a capped capture stays honestly
    /// small.
    pub max_growth_blocks: usize,
    /// Largest block (devices) the walk may pull. A generator loop is made of
    /// SMALL blocks (a real per-neuron block is ~65 devices on the Tarski
    /// board, the same provenance as [`ComposedPolicy::max_balance_block`]); a
    /// block an order of magnitude larger is the fused core wearing a sense
    /// edge, and pulling it re-fuses the very island the tear exists to avoid
    /// (measured on the flagship: pulling it ballooned every OUTPUT_I capture
    /// to 3600 devices and pushed each solve onto the adaptive rescue). An
    /// oversized block is skipped, deterministically, and the growth continues
    /// past it; whether skipping cost anything is measured by the same sag the
    /// relaxation always carries.
    pub max_block_devices: usize,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        CapturePolicy {
            max_growth_blocks: 64,
            max_block_devices: 300,
        }
    }
}

/// Execute one solve group by stiff tearing at `candidates`, or refuse.
///
/// `sub` is the group's self-contained circuit (the staged executor's
/// extraction, replay pins already added); `candidates` are cut nodes in
/// `sub`'s node space, in detection's choice order. `Ok(None)` means an
/// honest refusal, with the caller expected to run the exact fused solve
/// instead: non-convergence within the round budget, an out-of-scope shape,
/// or a capture solve that died (a boundary the executor cannot solve is a
/// boundary it cannot certify; the dead candidate carries an infinite
/// residual in `refusal_report`). `Err` is reserved for structural
/// mis-use (adaptive step control).
pub fn execute_stiff_group(
    sub: &Circuit,
    candidates: &[NodeId],
    opts: &SolverOptions,
    tstop: f64,
    refusal_report: &mut Vec<StiffOutcome>,
) -> SolveResult<Option<StiffExecution>> {
    execute_stiff_group_held(
        sub,
        candidates,
        &HashMap::new(),
        opts,
        tstop,
        refusal_report,
    )
}

/// [`execute_stiff_group`] with a set of EXTRA nodes HELD (not relaxed): each
/// `held` node is treated as a boundary during fragmentation (so the candidate
/// capture groups stay small even when the held node is a heavily-connected
/// hub) and pinned at its supplied train in every capture solve. The composed
/// executor uses this to relax signal cuts while holding the rails at their
/// balance-solved trains: a signal captured on its small adjacent-block group
/// with the rails held stays small, where freeing it in the whole rail-torn
/// group would re-fuse the hub it belongs to.
pub fn execute_stiff_group_held(
    sub: &Circuit,
    candidates: &[NodeId],
    held: &HashMap<u32, Vec<f64>>,
    opts: &SolverOptions,
    tstop: f64,
    refusal_report: &mut Vec<StiffOutcome>,
) -> SolveResult<Option<StiffExecution>> {
    execute_stiff_group_held_capped(
        sub,
        candidates,
        held,
        &CapturePolicy::default(),
        opts,
        tstop,
        refusal_report,
    )
}

/// [`execute_stiff_group_held`] with an explicit [`CapturePolicy`] governing the
/// generator-inclusive capture pre-pass. The convenience wrappers pass the
/// default; a caller (or a gate) that needs to size or disable the growth calls
/// this directly.
pub fn execute_stiff_group_held_capped(
    sub: &Circuit,
    candidates: &[NodeId],
    held: &HashMap<u32, Vec<f64>>,
    capture_policy: &CapturePolicy,
    opts: &SolverOptions,
    tstop: f64,
    refusal_report: &mut Vec<StiffOutcome>,
) -> SolveResult<Option<StiffExecution>> {
    let dt = match opts.step {
        StepControl::Fixed { dt } => dt,
        _ => {
            return Err(SolveError::refused(
                "stiff execution requires fixed step control",
            ))
        }
    };
    if candidates.is_empty() {
        return Ok(None);
    }

    let graph = ConductionGraph::analyze(sub);
    // Scope: the CANDIDATES must share one conduction island (the fused core
    // being cut). The sub-circuit legitimately holds OTHER islands too:
    // absorbed-driver copies and replay-pin islands, whose devices drive the
    // core's sense nets. Requiring the whole sub to be one island (the first
    // version of this check) silently disqualified every real group on the
    // flagship, because absorption creates companion islands by design.
    let mut islands: HashSet<usize> = HashSet::new();
    for c in candidates {
        match graph.node_island.get(c.0 as usize).copied().flatten() {
            Some(i) => {
                islands.insert(i);
            }
            None => {
                refusal_report.extend(out_of_scope_outcomes(
                    candidates,
                    "a candidate nobody conducts",
                ));
                return Ok(None);
            }
        }
    }
    if islands.len() != 1 {
        refusal_report.extend(out_of_scope_outcomes(
            candidates,
            "candidates span multiple conduction islands",
        ));
        return Ok(None);
    }
    let island = *islands.iter().next().unwrap();
    // Companion devices: everything outside the candidates' island rides
    // into every capture (they hold the sense boundaries; without them the
    // dead-membrane bug returns wearing a stiff certificate).
    let companions: Vec<DeviceId> = (0..sub.devices.len() as u32)
        .map(DeviceId)
        .filter(|id| !graph.islands[island].contains(id))
        .collect();

    let n_nodes = sub.max_node() as usize;
    let mut bound = vec![false; n_nodes + 1];
    for c in candidates {
        bound[c.0 as usize] = true;
    }
    // Held nodes (the composed executor's rails) are boundaries too: fragmenting
    // with them held keeps a candidate's capture group small even when the
    // candidate is a hub that would otherwise re-fuse a large region.
    for &h in held.keys() {
        if (h as usize) <= n_nodes {
            bound[h as usize] = true;
        }
    }
    let frag = fragment_blocks(sub, &graph, island, &bound);

    // Which blocks touch each candidate: via the devices that conduct it.
    let mut adjacent: HashMap<u32, Vec<usize>> = HashMap::new(); // cand -> block reps
    let mut conducts_cand: HashMap<DeviceId, Vec<u32>> = HashMap::new();
    for (id, dev) in sub.iter() {
        for n in dev.conduction_nodes() {
            if !n.is_ground() && bound[n.0 as usize] {
                conducts_cand.entry(id).or_default().push(n.0);
            }
        }
    }
    // Determinism sweep: iterate the conductor devices in DEVICE-ID order, not
    // HashMap order. The order blocks are pushed into `adjacent[cand]` seeds the
    // block-owner assignment below (and, via capture_circuit, the device set of
    // each capture sub-circuit); circuit construction order is solver-visible
    // (pivots, convergence paths), so it must be deterministic. HashMap
    // iteration order is not. (The rest of capture.rs already constructs from
    // `sub.iter()` device order and the `candidates` slice, both deterministic.)
    let mut conductors: Vec<DeviceId> = conducts_cand.keys().copied().collect();
    conductors.sort_unstable();
    for id in conductors {
        let cands = &conducts_cand[&id];
        if let Some(&b) = frag.device_block.get(&id) {
            for &cn in cands {
                let e = adjacent.entry(cn).or_default();
                if !e.contains(&b) {
                    e.push(b);
                }
            }
        }
    }
    for c in candidates {
        if adjacent.get(&c.0).is_none_or(|v| v.is_empty()) {
            refusal_report.extend(out_of_scope_outcomes(
                candidates,
                "a candidate with no adjacent block to capture against",
            ));
            return Ok(None);
        }
    }

    // ---- generator-inclusive capture growth (the fix) --------------------
    // Plain adjacency captures a candidate's LOAD. A node whose waveform is
    // GENERATED by an active loop it reaches only through sense edges (a driven
    // output stage sensing a membrane in a non-adjacent block) then relaxes flat
    // on that load-only group, because the generation is pinned/absent. This
    // pre-pass grows each candidate's block set past the load to pull that
    // generation in, generalizing the reference tear's two-seed island walk
    // (a hidden-capture BFS seeded from both the drive net and the output net)
    // to an arbitrary decomposition.
    // Two coupling kinds pull a block B (not yet in the set):
    //   (b) SENSE: a device already IN the capture senses a node of B, OR a
    //       device of B senses the candidate or a node already in the set. This
    //       assembles the whole drive loop (the output comparator senses the
    //       membrane; the reset switch senses the comparator; ...), and it is
    //       the workhorse for the flagship.
    //   (a) SOURCE: B holds a real independent source within `max_source_hops`
    //       conduction block-hops of the set (bounded insurance for a generator
    //       reached purely by conduction; rails never couple blocks here).
    // Sorted iteration throughout; capped by `max_growth_blocks`.
    let capture_blocks = grow_capture_blocks(
        sub,
        &graph,
        &frag,
        &adjacent,
        &conducts_cand,
        candidates,
        capture_policy,
    );
    let grew: HashMap<u32, usize> = candidates
        .iter()
        .map(|c| {
            let base = adjacent.get(&c.0).map(|v| v.len()).unwrap_or(0);
            (
                c.0,
                capture_blocks.get(&c.0).map(|v| v.len()).unwrap_or(base) - base,
            )
        })
        .collect();
    if std::env::var("HAUKSBEE_CAPTURE_DEBUG").is_ok() {
        let ngrew = grew.values().filter(|&&g| g > 0).count();
        eprintln!(
            "GROW: {} candidates, {} grew; blocks per grown cand: {:?}",
            candidates.len(),
            ngrew,
            candidates
                .iter()
                .filter(|c| grew.get(&c.0).copied().unwrap_or(0) > 0)
                .map(|c| (sub.node_name(*c), capture_blocks[&c.0].len()))
                .collect::<Vec<_>>()
        );
        let mut block_size: HashMap<usize, usize> = HashMap::new();
        for &b in frag.device_block.values() {
            *block_size.entry(b).or_insert(0) += 1;
        }
        for c in candidates {
            if grew.get(&c.0).copied().unwrap_or(0) == 0 {
                continue;
            }
            let adj: HashSet<usize> = adjacent
                .get(&c.0)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect();
            let pulled: Vec<(usize, usize, String)> = capture_blocks[&c.0]
                .iter()
                .filter(|b| !adj.contains(b))
                .map(|&b| {
                    let name = frag
                        .node_block
                        .iter()
                        .find(|(_, &bb)| bb == b)
                        .map(|(&n, _)| sub.node_name(NodeId(n as u32)).to_string())
                        .unwrap_or_default();
                    (b, block_size.get(&b).copied().unwrap_or(0), name)
                })
                .collect();
            eprintln!("  GROW {} pulled: {:?}", sub.node_name(*c), pulled);
        }
    }

    // ---- capture-cluster merge ---------------------------------------------
    // Grown captures OVERLAP on the flagship: every column capture pulls the
    // same generator core, so relaxing them separately solves N near-identical
    // sub-circuits per round, each seeing last round's REPLAY of the shared
    // core instead of the core itself. That is the measured cost blowout
    // (~137s x 16 candidates x up to 7 rounds) AND the quiet-basin risk: a
    // capture pinned to a stale train of its own generator can relax onto the
    // wrong fixed point. Candidates whose capture-block sets intersect are
    // merged into one CLUSTER and solved JOINTLY: the intra-cluster tears are
    // never torn (exact where relaxation only approximated), and only
    // cluster-external candidates remain replayed pin boundaries. Union-find
    // over pairwise GENERATION overlap, transitive by design (c1~c2, c2~c3
    // merges all three); `candidates` order seeds representatives, so the
    // construction stays deterministic.
    //
    // The criterion is deliberately NARROWER than any block-set intersection:
    // two chain neighbours legitimately share a LOAD block (the cut's own
    // impedance chain) and relax fine on separate small captures: merging them
    // would only bloat the solves and rewrite behaviour the fixtures certify.
    // A shared block triggers a merge only when it is a GROWN block (pulled by
    // the generator walk, not plain adjacency) for at least one side: that is
    // the "we both captured the same generator" signature, and it leaves every
    // ungrown group's execution bit-identical to the pre-merge code.
    let (clusters, cluster_blocks, cand_rep) = {
        let n = candidates.len();
        let sets: Vec<HashSet<usize>> = candidates
            .iter()
            .map(|c| capture_blocks[&c.0].iter().copied().collect())
            .collect();
        let plain: Vec<HashSet<usize>> = candidates
            .iter()
            .map(|c| {
                adjacent
                    .get(&c.0)
                    .map(|v| v.iter().copied().collect())
                    .unwrap_or_default()
            })
            .collect();
        let mut parent: Vec<usize> = (0..n).collect();
        fn find(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }
        for i in 0..n {
            for j in (i + 1)..n {
                let entangled = sets[i]
                    .intersection(&sets[j])
                    .any(|b| !plain[i].contains(b) || !plain[j].contains(b));
                if entangled {
                    let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                    if ri != rj {
                        // Lower index wins: the representative is always the
                        // EARLIEST member in `candidates` order.
                        parent[ri.max(rj)] = ri.min(rj);
                    }
                }
            }
        }
        let mut clusters: Vec<Vec<NodeId>> = Vec::new();
        let mut root_slot: HashMap<usize, usize> = HashMap::new();
        for i in 0..n {
            let r = find(&mut parent, i);
            let slot = *root_slot.entry(r).or_insert_with(|| {
                clusters.push(Vec::new());
                clusters.len() - 1
            });
            clusters[slot].push(candidates[i]);
        }
        let mut cluster_blocks: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut cand_rep: HashMap<u32, u32> = HashMap::new();
        for cl in &clusters {
            let rep = cl[0];
            let mut union: HashSet<usize> = HashSet::new();
            for c in cl {
                union.extend(capture_blocks[&c.0].iter().copied());
                cand_rep.insert(c.0, rep.0);
            }
            let mut blocks: Vec<usize> = union.into_iter().collect();
            blocks.sort_unstable();
            cluster_blocks.insert(rep.0, blocks);
        }
        (clusters, cluster_blocks, cand_rep)
    };
    if std::env::var("HAUKSBEE_CAPTURE_DEBUG").is_ok() {
        eprintln!(
            "CLUSTERS: {} from {} candidates: {:?}",
            clusters.len(),
            candidates.len(),
            clusters
                .iter()
                .map(|cl| (
                    sub.node_name(cl[0]),
                    cl.len(),
                    cluster_blocks[&cl[0].0].len()
                ))
                .collect::<Vec<_>>()
        );
    }

    // ---- rest estimates ---------------------------------------------------
    let mut rest: HashMap<u32, f64> = HashMap::new();
    let mut bootstrapped = false;
    let whole_dc = {
        let mut ws = Workspace::new(sub);
        dc_operating_point(&mut ws, sub, opts).ok().map(|_| ws)
    };
    match whole_dc {
        Some(ws) => {
            for c in candidates {
                let v = ws.layout.node(*c).map(|i| ws.x[i]).unwrap_or(0.0);
                rest.insert(c.0, v);
            }
        }
        None => {
            // Bootstrap: per-cluster capture-group DC with the external
            // candidates at 0; every member's rest reads from the joint point.
            bootstrapped = true;
            for cluster in &clusters {
                let members: HashSet<u32> = cluster.iter().map(|c| c.0).collect();
                // Pins default to Dc(0.0): exactly the zeros bootstrap, except
                // held rails, which start at their train's t=0 estimate.
                let (mut cap, g2l) = capture_circuit(
                    sub,
                    &frag,
                    &cluster_blocks[&cluster[0].0],
                    &members,
                    &conducts_cand,
                    &companions,
                    candidates,
                    held,
                );
                for dev in cap.devices.iter_mut() {
                    if let Device::Vsource { name, kind, .. } = dev {
                        if let Some(net) = name.strip_prefix("VRAIL_") {
                            if let Some((_, train)) =
                                held.iter().find(|(k, _)| sub.node_name(NodeId(**k)) == net)
                            {
                                *kind = SourceKind::Dc(train.first().copied().unwrap_or(0.0));
                            }
                        }
                    }
                }
                let mut ws = Workspace::new(&cap);
                let solved = dc_operating_point(&mut ws, &cap, opts).is_ok();
                for c in cluster {
                    let v = if solved {
                        g2l.get(&c.0)
                            .and_then(|ln| ws.layout.node(NodeId(*ln)))
                            .map(|i| ws.x[i])
                            .unwrap_or(0.0)
                    } else {
                        0.0
                    };
                    rest.insert(c.0, v);
                }
            }
        }
    }

    // ---- waveform relaxation: rest-seeded, then train-driven rounds -------
    // True Gauss-Seidel: within a round each candidate is solved against the
    // LATEST trains (in-place update), so a feedforward chain converges in
    // about one round instead of one round per chain link. Convergence is
    // per candidate: max |train_new - train_old| against its tolerance; the
    // LAST round's residual is what the certificate carries.
    let grid = uniform_grid(dt, tstop);
    // The convergence bar scales with the CALLER'S requested accuracy: the
    // certified tol_v must be a function of the tolerance the user selected,
    // never a literal that happens to match the default (review finding).
    let reltol = opts.reltol;
    let max_rounds = 6usize;
    let dbg = std::env::var("HAUKSBEE_CAPTURE_DEBUG").is_ok();

    let mut trains: HashMap<u32, Vec<f64>> = HashMap::new();
    let mut runs: HashMap<u32, CaptureRun> = HashMap::new();
    // Round 0: rest-seeded. Solved in order with in-place updates, so later
    // candidates already see earlier candidates' round-0 trains instead of
    // bare rest values.
    let rest_trains: HashMap<u32, Vec<f64>> = candidates
        .iter()
        .map(|c| {
            (
                c.0,
                vec![rest.get(&c.0).copied().unwrap_or(0.0); grid.len()],
            )
        })
        .collect();
    for cluster in &clusters {
        let rep = cluster[0];
        let members: HashSet<u32> = cluster.iter().map(|c| c.0).collect();
        let cluster_grown = cluster
            .iter()
            .any(|c| grew.get(&c.0).copied().unwrap_or(0) > 0);
        // Merge: already-captured candidates by train, the rest by rest.
        let mut boundary = rest_trains.clone();
        for (k, v) in &trains {
            boundary.insert(*k, v.clone());
        }
        let t_solve = std::time::Instant::now();
        let wf = match solve_capture(
            sub,
            &frag,
            &cluster_blocks[&rep.0],
            &members,
            &conducts_cand,
            &companions,
            rep,
            candidates,
            &rest,
            held,
            Some((&boundary, &grid)),
            None,
            cluster_grown,
            opts,
            tstop,
        ) {
            Ok(wf) => wf,
            Err(_) => {
                refusal_report.extend(dead_capture_outcomes(
                    candidates,
                    rep,
                    &rest,
                    opts.reltol,
                    bootstrapped,
                ));
                return Ok(None);
            }
        };
        if dbg {
            eprintln!(
                "  ROUND 0 {} ({} joint): {:.2}s",
                sub.node_name(rep),
                cluster.len(),
                t_solve.elapsed().as_secs_f64()
            );
        }
        for c in cluster {
            let s = sample_node(&wf.0, &wf.1, *c, &grid);
            trains.insert(c.0, s);
        }
        runs.insert(rep.0, wf);
    }

    let mut sag: HashMap<u32, f64> = HashMap::new();
    let mut converged = false;
    // A SINGLE cluster covering every candidate has no replayed candidate
    // boundaries at all: the only inputs to its solve are the held trains,
    // which are fixed for the whole call. The relaxation map is therefore
    // CONSTANT (no member's train feeds back into any solve), so round 0
    // already sits at the fixed point EXACTLY, not approximately: a train
    // round would re-solve the identical problem (the flagship measured the
    // round-1 residual at exactly 0.0 for all 16 members, ~750s of pure
    // repetition). Zero sag is the structural truth here, the same class of
    // claim as balance's round-off: nothing was replayed, so nothing sags.
    if clusters.len() == 1 {
        converged = true;
        for c in candidates {
            sag.insert(c.0, 0.0);
        }
    }
    for _round in 0..max_rounds {
        if converged {
            break;
        }
        converged = true;
        for cluster in &clusters {
            let rep = cluster[0];
            let members: HashSet<u32> = cluster.iter().map(|c| c.0).collect();
            let cluster_grown = cluster
                .iter()
                .any(|c| grew.get(&c.0).copied().unwrap_or(0) > 0);
            let t_solve = std::time::Instant::now();
            let wf = match solve_capture(
                sub,
                &frag,
                &cluster_blocks[&rep.0],
                &members,
                &conducts_cand,
                &companions,
                rep,
                candidates,
                &rest,
                held,
                Some((&trains, &grid)),
                runs.get(&rep.0),
                cluster_grown,
                opts,
                tstop,
            ) {
                Ok(wf) => wf,
                Err(_) => {
                    refusal_report.extend(dead_capture_outcomes(
                        candidates,
                        rep,
                        &rest,
                        opts.reltol,
                        bootstrapped,
                    ));
                    return Ok(None);
                }
            };
            for c in cluster {
                let new = sample_node(&wf.0, &wf.1, *c, &grid);
                let old = &trains[&c.0];
                // The residual metric is time-shift-tolerant by ONE grid step:
                // each sample compares against the other iterate's neighbouring
                // cells and keeps the smallest difference. This is the
                // capture-grid claim's own metric (a replayed boundary is exact
                // up to the grid), and it is what lets spiking trains converge:
                // a spike whose timing shifts by one cell between iterates is
                // the same physics, but a pointwise metric reads it as volts of
                // divergence, which is exactly how the flagship's mega group
                // refused at the full window while accepting at smoke scale.
                let diff = shifted_residual(old, &new);
                if dbg {
                    eprintln!(
                        "  ROUND {} {}: {:.2}s, residual {:.3e}",
                        _round + 1,
                        sub.node_name(*c),
                        t_solve.elapsed().as_secs_f64(),
                        diff
                    );
                }
                sag.insert(c.0, diff);
                let vnom = rest.get(&c.0).map(|v| v.abs()).unwrap_or(0.0).max(1.0);
                if diff > 10.0 * reltol * vnom {
                    converged = false;
                }
                trains.insert(c.0, new);
            }
            runs.insert(rep.0, wf);
        }
        if converged {
            break;
        }
    }

    // ---- the honesty gate --------------------------------------------------
    let mut outcomes = Vec::new();
    for c in candidates {
        let s = sag.get(&c.0).copied().unwrap_or(f64::INFINITY);
        let vnom = rest.get(&c.0).map(|v| v.abs()).unwrap_or(0.0).max(1.0);
        let tol = 10.0 * reltol * vnom;
        let g = grew.get(&c.0).copied().unwrap_or(0);
        let joint = cand_rep
            .get(&c.0)
            .and_then(|r| clusters.iter().find(|cl| cl[0].0 == *r))
            .is_some_and(|cl| cl.len() > 1);
        outcomes.push(StiffOutcome {
            node: *c,
            kind: BoundaryKind::Signal,
            sag_v: s,
            tol_v: tol,
            accepted: s <= tol,
            bootstrapped,
            capture_growth: g,
            note: if joint {
                "merged capture: solved jointly with overlapping candidates (intra-cluster boundary exact, not replayed)"
            } else if g > 0 {
                "generator-inclusive capture (grew beyond plain adjacency)"
            } else {
                ""
            },
        });
    }
    if !converged {
        refusal_report.extend(outcomes);
        return Ok(None);
    }

    // ---- assembly: every block reads from one owning final-round run ------
    // Ownership follows the GROWN capture set, so a pulled-in generator block is
    // read from the candidate that pulled it (its membrane now carries the real
    // spiking waveform, not a static rest value). Runs are keyed by CLUSTER
    // representative: a merged block reads from the joint run that solved it.
    let mut block_owner: HashMap<usize, u32> = HashMap::new();
    for cluster in &clusters {
        for &b in &cluster_blocks[&cluster[0].0] {
            block_owner.entry(b).or_insert(cluster[0].0);
        }
    }
    // Any block adjacent to NO candidate would be unreadable; fragmentation
    // guarantees there are none WITHOUT held rails, but refuse rather than emit
    // zeros if the guarantee is ever broken. With held rails present, holding
    // them as boundaries can create blocks adjacent only to a rail (never to a
    // signal), which is expected: the composed caller assembles the whole group
    // itself and reads only the signal series from here, so those blocks are
    // skipped, not fatal.
    if held.is_empty() {
        for &b in frag.block_devices.keys() {
            if !block_owner.contains_key(&b) {
                refusal_report.extend(outcomes);
                return Ok(None);
            }
        }
    }

    let n_out = sub.node_count();
    let mut waveforms = Waveforms {
        time: grid.clone(),
        node_voltages: vec![vec![0.0; grid.len()]; n_out],
        branch_currents: Vec::new(),
    };
    for node in 1..n_out {
        let series: Vec<f64> = if held.contains_key(&(node as u32)) {
            held[&(node as u32)].clone()
        } else if bound[node] {
            trains[&(node as u32)].clone()
        } else if let Some(&blk) = frag.node_block.get(&node) {
            let Some(&owner) = block_owner.get(&blk) else {
                continue; // rail-only block (held case); composed ignores it
            };
            let (wf, g2l) = &runs[&owner];
            match g2l.get(&(node as u32)) {
                Some(&ln) => grid
                    .iter()
                    .map(|&t| lerp_at(&wf.time, &wf.node_voltages[ln as usize], t))
                    .collect(),
                None => continue,
            }
        } else {
            // Companion-island nodes (absorbed copies, replay-pin islands)
            // ride in every capture; read them from the first cluster's
            // final run. Known-side nodes of the core island with no free
            // block land here too and stay zero only if no capture mapped
            // them.
            let owner = clusters[0][0].0;
            let (wf, g2l) = &runs[&owner];
            match g2l.get(&(node as u32)) {
                Some(&ln) => grid
                    .iter()
                    .map(|&t| lerp_at(&wf.time, &wf.node_voltages[ln as usize], t))
                    .collect(),
                None => continue,
            }
        };
        waveforms.node_voltages[node] = series;
    }

    Ok(Some(StiffExecution {
        waveforms,
        outcomes,
    }))
}

/// Execute one solve group by COMPOSING balance-torn rails with stiff-cut
/// waveform relaxation, or refuse.
///
/// This is the sibling of [`execute_stiff_group`] for a group whose stiff
/// nominations include genuine supply RAILS (load-dependent nets on which
/// Gauss-Seidel limit-cycles instead of contracting, the flagship's ANALOG_VDD
/// and +5V) alongside ordinary SIGNAL cuts (which relax fine). The rails are
/// handed to the exact scalar KCL balance (`analyze_imposing_tears` +
/// `PartitionedTransient`), and the signals are relaxed on top of it.
///
/// `sub` is the group's self-contained circuit; `signal_cuts` are signal stiff
/// nodes in `sub`'s space; `rails` are the balance tears already remapped into
/// `sub`'s space by the caller (like `remap_tear`).
///
/// ## The composition is an alternation of two exact solves
///
/// A signal cut relaxes fine on a SMALL capture group; a supply rail does not
/// relax at all (it limit-cycles), it wants the exact scalar KCL balance. The
/// naive "pin every signal, solve once, resample" measures a signal's residual
/// as identically zero (a pinned node reads back its pin), and freeing a signal
/// in the WHOLE rail-torn group re-fuses the large hub it belongs to (the
/// flagship's OUTPUT_I nets fuse thousands of devices when freed). So neither a
/// whole-group free-solve nor a whole-group pinned-solve works.
///
/// The composition instead ALTERNATES:
///
/// 1. **Rail balance.** Pin every signal at its current train, tear the rails,
///    and march the fully-fragmented partitioned engine (its seed decomposes
///    when the whole-group DC collapses). Sample each rail's voltage train from
///    that solve: the balance closes each rail's KCL exactly per step.
/// 2. **Signal relaxation.** Hold the rails at those trains and relax the
///    signals with [`execute_stiff_group_held`]: each signal is captured on its
///    small adjacent-block group (rails held keep it small even for a hub),
///    exactly the proven stiff mechanism. Sample the new signal trains.
///
/// Iterate until the signal trains stop moving between outer passes; the rails
/// carry no sag (their exactness is the balance claim), the signals carry the
/// inner relaxation's measured sag. The final assembly is one rail-balance solve
/// at the converged signal trains, read for every node.
///
/// ## The fragmentation gate, and the stiff-supply feed-hold fallback
///
/// Step 1 requires the balance engine to actually FRAGMENT the fully-pinned
/// group into marchable blocks. It fragments by `nodes()`, coarser than the
/// conduction-based fragmentation the signal captures use, so a group whose
/// signals fragment finely can still leave a large nonlinear island for the
/// balance engine (the flagship's 5808-device mega group does, even with every
/// signal pinned: a 4000-device block survives and its per-island Newton cannot
/// march). When `balance_group_fragments` reports this (a [`ComposedPolicy`]
/// cap), the rails are treated as the STIFF supplies they are (fed through mΩ
/// legs) and HELD at a fixed estimate (the whole-group DC value when one
/// exists, else the feed voltage) instead of balanced on the whole group; only
/// the signals relax, on their small captures, and the assembly is the held
/// relaxation's own (signals from trains, blocks from captures, rails from the
/// hold; blocks adjacent to no signal are not read). A held rail's outcome
/// carries [`BoundaryKind::HeldRail`], which the certificate maps to
/// `Stiff`/`AssumedFeedHold`/`Unmeasured`: an assumption on the supply leg's
/// stiffness, never the balance-exact claim (that record is reserved for
/// [`BoundaryKind::BalancedRail`], where the scalar KCL actually ran). This is
/// the honest degradation for a group the balance engine cannot fragment; the
/// exact whole-group balance stands for groups that do fragment (the mini
/// fixtures, real synapse arrays).
///
/// `Ok(None)` is an honest refusal (a signal that would not contract, or a solve
/// that died); rails then carry no exactness claim.
pub fn execute_composed_group(
    sub: &Circuit,
    signal_cuts: &[NodeId],
    rails: &[RailTear],
    policy: &ComposedPolicy,
    opts: &SolverOptions,
    tstop: f64,
    refusal_report: &mut Vec<StiffOutcome>,
) -> SolveResult<Option<StiffExecution>> {
    let dt = match opts.step {
        StepControl::Fixed { dt } => dt,
        _ => {
            return Err(SolveError::refused(
                "composed execution requires fixed step control",
            ))
        }
    };
    if rails.is_empty() {
        return Err(SolveError::refused(
            "composed execution needs at least one rail tear",
        ));
    }
    let debug = std::env::var("HAUKSBEE_COMPOSED_DEBUG").is_ok();

    let grid = uniform_grid(dt, tstop);
    let reltol = opts.reltol;
    let mut sub_opts = *opts;
    sub_opts.partitioning = Partitioning::Off;

    // ---- estimates: signal rest, rail vnom, rail feed seed ----------------
    // The whole-group DC usually FAILS (the collapse this exists for). When it
    // does, signals seed at 0 and rails seed at their feed voltage; the first
    // rail-balance pass corrects both.
    let mut rest: HashMap<u32, f64> = HashMap::new();
    let mut rail_vnom: HashMap<u32, f64> = HashMap::new();
    let mut rail_seed: HashMap<u32, f64> = HashMap::new();
    let mut bootstrapped = true;
    {
        let mut ws = Workspace::new(sub);
        if dc_operating_point(&mut ws, sub, opts).is_ok() {
            bootstrapped = false;
            for s in signal_cuts {
                rest.insert(s.0, ws.layout.node(*s).map(|i| ws.x[i]).unwrap_or(0.0));
            }
            for rt in rails {
                let v = ws.layout.node(rt.rail).map(|i| ws.x[i]).unwrap_or(0.0);
                rail_vnom.insert(rt.rail.0, v.abs().max(1.0));
                rail_seed.insert(rt.rail.0, v);
            }
        } else {
            for s in signal_cuts {
                rest.insert(s.0, 0.0);
            }
            for rt in rails {
                // Feed-voltage seed: the feed node's source value if it is
                // ideally driven, else 0 for now (cascade fix-up below).
                let vfeed = feed_source_value(sub, rt.feed).unwrap_or(0.0);
                rail_vnom.insert(rt.rail.0, vfeed.abs().max(1.0));
                rail_seed.insert(rt.rail.0, vfeed);
            }
            // Cascade fix-up: a rail whose feed is ANOTHER rail (ANALOG_VDD fed
            // from the +5V rail) inherits its parent's seed. Iterate to a
            // fixpoint (bounded by the rail count).
            for _ in 0..rails.len().max(1) {
                for rt in rails {
                    if rail_seed.get(&rt.rail.0).copied().unwrap_or(0.0) == 0.0 {
                        if let Some(&pv) = rail_seed.get(&rt.feed.0) {
                            rail_seed.insert(rt.rail.0, pv);
                            rail_vnom.insert(rt.rail.0, pv.abs().max(1.0));
                        }
                    }
                }
            }
        }
    }

    // Signal trains, rail trains (constant seeds to start).
    let mut signal_trains: HashMap<u32, Vec<f64>> = signal_cuts
        .iter()
        .map(|s| {
            (
                s.0,
                vec![rest.get(&s.0).copied().unwrap_or(0.0); grid.len()],
            )
        })
        .collect();
    let mut rail_trains: HashMap<u32, Vec<f64>> = rails
        .iter()
        .map(|rt| {
            (
                rt.rail.0,
                vec![rail_seed.get(&rt.rail.0).copied().unwrap_or(0.0); grid.len()],
            )
        })
        .collect();

    // ---- does the group fragment for the partitioned BALANCE engine? ------
    // The balance engine ([`PartitionedTransient`]) fragments by `nodes()`,
    // which is COARSER than the conduction-based fragmentation the stiff
    // detector (and the signal captures below) use. A group the detector
    // fragments into small signal-capture blocks can therefore still leave a
    // large nonlinear island for the balance engine to march (the flagship's
    // 5808-device mega group does exactly this, even with every signal pinned).
    // When it does, we do NOT march the whole group: the rails are STIFF
    // supplies, so they are held at their feed voltage (a cascade seed, near-
    // exact for a mΩ supply leg) and only the signals relax on their small
    // captures. When the group DOES fragment (the mini fixtures, real synapse
    // arrays), the exact whole-group scalar balance runs and the composition is
    // exact up to the inner relaxation's sag.
    let whole_group_balances = balance_group_fragments(sub, rails, signal_cuts, policy);
    // The kind every rail outcome (success or refusal) will carry: it is a
    // property of HOW this run closes the rails, decided here, once.
    let rail_kind = if whole_group_balances {
        BoundaryKind::BalancedRail
    } else {
        BoundaryKind::HeldRail
    };
    if debug {
        eprintln!("COMPOSED whole-group balance fragments: {whole_group_balances}");
    }

    // ---- outer alternation (rail balance <-> signal relaxation) -----------
    // With the whole-group balance the rail<->signal coupling is block
    // Gauss-Seidel, contracting LINEARLY at the coupling's spectral radius
    // (weak on a stiff rail: a couple of passes). The ceiling is generous;
    // convergence breaks early. When the group does not fragment, the rails are
    // held fixed at their feed voltage, so a single signal pass suffices.
    let max_outer = if whole_group_balances { 20 } else { 1 };
    let mut sag: HashMap<u32, f64> = HashMap::new();
    let mut growth: HashMap<u32, usize> = HashMap::new();
    let mut outer_converged = signal_cuts.is_empty();
    let mut held_waveforms: Option<Waveforms> = None;
    for outer in 0..max_outer {
        // 1. Rail balance on the whole group (exact) when it fragments.
        if whole_group_balances {
            let rail_wf = match solve_composed(
                sub,
                rails,
                signal_cuts,
                &signal_trains,
                &grid,
                &sub_opts,
                tstop,
            ) {
                Ok(Some(wf)) => wf,
                other => {
                    if debug {
                        eprintln!("COMPOSED rail balance died: {:?}", other.as_ref().err());
                    }
                    refusal_report.extend(composed_refusal_outcomes(
                        signal_cuts,
                        rails,
                        rail_kind,
                        None,
                        &rest,
                        &rail_vnom,
                        reltol,
                        bootstrapped,
                    ));
                    return Ok(None);
                }
            };
            for rt in rails {
                let s: Vec<f64> = grid
                    .iter()
                    .map(|&t| lerp_at(&rail_wf.time, &rail_wf.node_voltages[rt.rail.0 as usize], t))
                    .collect();
                rail_trains.insert(rt.rail.0, s);
            }
        }
        // (else: rails stay held at their feed-voltage seed in rail_trains.)

        // 2. Signal relaxation with rails HELD at their current trains.
        if signal_cuts.is_empty() {
            outer_converged = true;
            break;
        }
        let mut inner_refusals = Vec::new();
        let exec = match execute_stiff_group_held(
            sub,
            signal_cuts,
            &rail_trains,
            &sub_opts,
            tstop,
            &mut inner_refusals,
        )? {
            Some(exec) => exec,
            None => {
                if debug {
                    eprintln!("COMPOSED signal relaxation refused: {inner_refusals:?}");
                }
                refusal_report.extend(inner_refusals);
                for rt in rails {
                    let vnom = rail_vnom.get(&rt.rail.0).copied().unwrap_or(1.0);
                    refusal_report.push(StiffOutcome {
                        node: rt.rail,
                        kind: rail_kind,
                        sag_v: f64::NAN,
                        tol_v: 10.0 * reltol * vnom,
                        accepted: false,
                        bootstrapped,
                        capture_growth: 0,
                        note: "composed relaxation refused; rail balance not certified",
                    });
                }
                return Ok(None);
            }
        };

        let mut outer_move = 0.0f64;
        for c in signal_cuts {
            let new: Vec<f64> = grid
                .iter()
                .map(|&t| {
                    lerp_at(
                        &exec.waveforms.time,
                        &exec.waveforms.node_voltages[c.0 as usize],
                        t,
                    )
                })
                .collect();
            let old = &signal_trains[&c.0];
            outer_move = outer_move.max(shifted_residual(old, &new));
            signal_trains.insert(c.0, new);
        }
        for o in &exec.outcomes {
            sag.insert(o.node.0, o.sag_v);
            growth.insert(o.node.0, o.capture_growth);
        }
        held_waveforms = Some(exec.waveforms);
        if debug {
            eprintln!("COMPOSED outer {outer}: signal move {outer_move:.3e}");
        }
        if !whole_group_balances {
            outer_converged = true; // rails held fixed: one signal pass is the answer
            break;
        }
        let bar = signal_cuts
            .iter()
            .map(|c| {
                let vnom = rest.get(&c.0).map(|v| v.abs()).unwrap_or(0.0).max(1.0);
                (10.0 * reltol * vnom).max(1e-6 * vnom)
            })
            .fold(0.0f64, f64::max);
        if outer_move <= bar {
            outer_converged = true;
            break;
        }
    }

    if !outer_converged && !signal_cuts.is_empty() {
        refusal_report.extend(composed_refusal_outcomes(
            signal_cuts,
            rails,
            rail_kind,
            None,
            &rest,
            &rail_vnom,
            reltol,
            bootstrapped,
        ));
        return Ok(None);
    }

    // ---- final assembly ---------------------------------------------------
    let n_out = sub.node_count();
    let mut waveforms = Waveforms {
        time: grid.clone(),
        node_voltages: vec![vec![0.0; grid.len()]; n_out],
        branch_currents: Vec::new(),
    };
    if whole_group_balances {
        // One exact whole-group rail-balance solve at the converged trains.
        let full = match solve_composed(
            sub,
            rails,
            signal_cuts,
            &signal_trains,
            &grid,
            &sub_opts,
            tstop,
        ) {
            Ok(Some(wf)) => wf,
            other => {
                if debug {
                    eprintln!("COMPOSED final assembly died: {:?}", other.as_ref().err());
                }
                refusal_report.extend(composed_refusal_outcomes(
                    signal_cuts,
                    rails,
                    rail_kind,
                    None,
                    &rest,
                    &rail_vnom,
                    reltol,
                    bootstrapped,
                ));
                return Ok(None);
            }
        };
        for node in 1..n_out {
            if node >= full.node_voltages.len() {
                continue;
            }
            for (k, &t) in grid.iter().enumerate() {
                waveforms.node_voltages[node][k] =
                    lerp_at(&full.time, &full.node_voltages[node], t);
            }
        }
    } else {
        // Feed-hold: the held signal relaxation's OWN assembly is the group's
        // waveforms (signals from their trains, blocks from their captures, the
        // held rails from their feed-voltage trains; rail-only blocks stay 0).
        if let Some(wf) = held_waveforms {
            for node in 1..n_out.min(wf.node_voltages.len()) {
                for (k, &t) in grid.iter().enumerate() {
                    waveforms.node_voltages[node][k] =
                        lerp_at(&wf.time, &wf.node_voltages[node], t);
                }
            }
        } else {
            // No signals to relax: just the held rails.
            for rt in rails {
                if let Some(tr) = rail_trains.get(&rt.rail.0) {
                    waveforms.node_voltages[rt.rail.0 as usize] = tr.clone();
                }
            }
        }
    }

    // ---- outcomes ---------------------------------------------------------
    let mut outcomes = Vec::new();
    for c in signal_cuts {
        let s = sag.get(&c.0).copied().unwrap_or(0.0);
        let vnom = rest.get(&c.0).map(|v| v.abs()).unwrap_or(0.0).max(1.0);
        // Quiet-basin telemetry (pure diagnostics, no behavior change): a
        // converged train that is essentially FLAT may have relaxed into a
        // quiet basin instead of the true spiking answer (the flagship's O2
        // hazard); flag it so the outcome table names the candidates.
        let train = &signal_trains[&c.0];
        let ptp = train.iter().cloned().fold(f64::MIN, f64::max)
            - train.iter().cloned().fold(f64::MAX, f64::min);
        outcomes.push(StiffOutcome {
            node: *c,
            kind: BoundaryKind::Signal,
            sag_v: s,
            tol_v: 10.0 * reltol * vnom,
            accepted: true,
            bootstrapped,
            capture_growth: growth.get(&c.0).copied().unwrap_or(0),
            note: if ptp < 0.01 * vnom {
                "converged flat (quiet-basin candidate)"
            } else {
                ""
            },
        });
    }
    for rt in rails {
        let vnom = rail_vnom.get(&rt.rail.0).copied().unwrap_or(1.0);
        outcomes.push(StiffOutcome {
            node: rt.rail,
            kind: rail_kind,
            sag_v: 0.0,
            tol_v: 10.0 * reltol * vnom,
            accepted: true,
            bootstrapped,
            capture_growth: 0,
            // A rail carries no sag either way; `kind` is the structured claim
            // (BalancedRail: exact per-step KCL; HeldRail: feed-voltage pin,
            // nothing measured) and the note is its human reading.
            note: if whole_group_balances {
                "balanced rail"
            } else {
                "held rail (stiff-supply feed)"
            },
        });
    }

    Ok(Some(StiffExecution {
        waveforms,
        outcomes,
    }))
}

/// Tunables for the composed executor, with provenance. Policy fields, not
/// buried literals, so gates can exercise both paths at toy scale.
#[derive(Debug, Clone, Copy)]
pub struct ComposedPolicy {
    /// Largest nonlinear island (devices) the whole-group balance engine is
    /// asked to march; above this the rails degrade to the feed hold. The
    /// default mirrors `TEAR_MAX_BLOCK_DEVICES` in `partitioned.rs`: sized
    /// well above a real per-neuron block (~65 devices on the Tarski board)
    /// but far below the fused 5k-device island, so the whole-group balance
    /// is attempted exactly when its per-block Newton has a chance.
    pub max_balance_block: usize,
}

impl Default for ComposedPolicy {
    fn default() -> Self {
        ComposedPolicy {
            max_balance_block: 600,
        }
    }
}

/// Whether the fully-signal-pinned, rail-torn group fragments into blocks the
/// partitioned balance engine can march (largest nonlinear island under the
/// policy cap). The balance engine fragments by `nodes()`, coarser than the
/// conduction-based signal fragmentation, so this can be false even when the
/// signal captures fragment finely.
fn balance_group_fragments(
    sub: &Circuit,
    rails: &[RailTear],
    signal_cuts: &[NodeId],
    policy: &ComposedPolicy,
) -> bool {
    let mut subp = sub.clone();
    for s in signal_cuts {
        subp.add(Device::Vsource {
            name: format!("VSTIFF_{}", sub.node_name(*s)),
            p: *s,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(0.0), // topology only; value irrelevant here
        });
    }
    let part = Partition::analyze_imposing_tears(&subp, rails.to_vec());
    let largest_nl = part
        .islands
        .iter()
        .filter(|i| !i.linear)
        .map(|i| i.devices.len())
        .max()
        .unwrap_or(0);
    largest_nl <= policy.max_balance_block
}

/// The DC value an ideal source drives onto `feed`, if one does (used to seed a
/// torn rail at its feed voltage before the first balance pass). `None` when the
/// feed is not directly source-pinned.
fn feed_source_value(sub: &Circuit, feed: NodeId) -> Option<f64> {
    if feed.is_ground() {
        return Some(0.0);
    }
    for dev in &sub.devices {
        if let Device::Vsource { p, n, kind, .. } = dev {
            if *p == feed && n.is_ground() {
                return Some(kind.eval(0.0));
            }
            if *n == feed && p.is_ground() {
                return Some(-kind.eval(0.0));
            }
        }
    }
    None
}

/// One composed solve: `sub` cloned, one VSTIFF PWL pin per node in
/// `pin_signals` at its current train, torn on `rails`, marched by the
/// partitioned engine (whose seed decomposes when the whole-group DC collapses).
/// Returns the whole group's waveforms (node voltages on the engine grid), or
/// `Ok(None)` if the engine declined to build.
fn solve_composed(
    sub: &Circuit,
    rails: &[RailTear],
    pin_signals: &[NodeId],
    trains: &HashMap<u32, Vec<f64>>,
    grid: &[f64],
    opts: &SolverOptions,
    tstop: f64,
) -> SolveResult<Option<Waveforms>> {
    // Clone so node AND device ids are preserved: the rails' shunt DeviceIds and
    // feed NodeIds (chosen by the caller in `sub`'s space) stay valid, and the
    // VSTIFF pins add no new nodes (the signal nodes already exist).
    let mut subp = sub.clone();
    for s in pin_signals {
        let train = &trains[&s.0];
        let points: Vec<PwlPoint> = grid
            .iter()
            .zip(train)
            .map(|(&t, &v)| PwlPoint { t, v })
            .collect();
        subp.add(Device::Vsource {
            name: format!("VSTIFF_{}", sub.node_name(*s)),
            p: *s,
            n: NodeId::GROUND,
            kind: SourceKind::Pwl(points),
        });
    }

    let part = Partition::analyze_imposing_tears(&subp, rails.to_vec());
    let Some(mut engine) = PartitionedTransient::try_build_from_partition(&subp, opts, part) else {
        return Ok(None);
    };
    let n_nodes = subp.node_count();
    let mut wf = Waveforms {
        time: Vec::new(),
        node_voltages: vec![Vec::new(); n_nodes],
        branch_currents: Vec::new(),
    };
    engine.run_streaming(&subp, tstop, |s| {
        wf.time.push(s.time);
        for node in 0..n_nodes {
            let v = if node == 0 {
                0.0
            } else {
                s.x.get(node - 1).copied().unwrap_or(0.0)
            };
            wf.node_voltages[node].push(v);
        }
    })?;
    Ok(Some(wf))
}

/// Refusal outcomes for the composed executor: signals carry their sag (INF for
/// the one whose solve died), rails carry no certified balance.
fn composed_refusal_outcomes(
    signal_cuts: &[NodeId],
    rails: &[RailTear],
    rail_kind: BoundaryKind,
    dead: Option<NodeId>,
    rest: &HashMap<u32, f64>,
    rail_vnom: &HashMap<u32, f64>,
    reltol: f64,
    bootstrapped: bool,
) -> Vec<StiffOutcome> {
    let mut out = Vec::new();
    for c in signal_cuts {
        let vnom = rest.get(&c.0).map(|v| v.abs()).unwrap_or(0.0).max(1.0);
        let tol = 10.0 * reltol * vnom;
        let sag = match dead {
            Some(d) if d.0 == c.0 => f64::INFINITY,
            _ => f64::NAN,
        };
        out.push(StiffOutcome {
            node: *c,
            kind: BoundaryKind::Signal,
            sag_v: sag,
            tol_v: tol,
            accepted: false,
            bootstrapped,
            capture_growth: 0,
            note: match dead {
                Some(d) if d.0 == c.0 => "composed capture died at this signal",
                Some(_) => "unmeasured: a sibling signal solve died first",
                None => "composed relaxation did not contract within the round budget",
            },
        });
    }
    for rt in rails {
        let vnom = rail_vnom.get(&rt.rail.0).copied().unwrap_or(1.0);
        out.push(StiffOutcome {
            node: rt.rail,
            kind: rail_kind,
            sag_v: f64::NAN,
            tol_v: 10.0 * reltol * vnom,
            accepted: false,
            bootstrapped,
            capture_growth: 0,
            note: "composed relaxation refused; rail balance not certified",
        });
    }
    out
}

/// Outcomes for an out-of-scope disqualification: no measurement exists,
/// but the caller still learns exactly why nothing was attempted (the
/// flagship smoke found the silent form of this: zero outcomes, zero
/// explanation).
fn out_of_scope_outcomes(candidates: &[NodeId], note: &'static str) -> Vec<StiffOutcome> {
    candidates
        .iter()
        .map(|c| StiffOutcome {
            node: *c,
            kind: BoundaryKind::Signal,
            sag_v: f64::NAN,
            tol_v: f64::NAN,
            accepted: false,
            bootstrapped: false,
            capture_growth: 0,
            note,
        })
        .collect()
}

/// Outcomes for a refusal caused by a capture solve dying at `dead`: the
/// dead boundary carries an infinite residual, the others are unknown.
fn dead_capture_outcomes(
    candidates: &[NodeId],
    dead: NodeId,
    rest: &HashMap<u32, f64>,
    reltol: f64,
    bootstrapped: bool,
) -> Vec<StiffOutcome> {
    candidates
        .iter()
        .map(|c| {
            let vnom = rest.get(&c.0).map(|v| v.abs()).unwrap_or(0.0).max(1.0);
            StiffOutcome {
                node: *c,
                kind: BoundaryKind::Signal,
                sag_v: if c.0 == dead.0 {
                    f64::INFINITY
                } else {
                    f64::NAN
                },
                tol_v: 10.0 * reltol * vnom,
                accepted: false,
                bootstrapped,
                capture_growth: 0,
                note: if c.0 == dead.0 {
                    "capture solve died (power-ramp retry included)"
                } else {
                    "unmeasured: a sibling capture died first"
                },
            }
        })
        .collect()
}

/// Grow each candidate's plain-adjacency block set to include the GENERATION of
/// the captured node, not only its load. Returns cand -> block reps (the
/// plain-adjacency blocks plus any pulled generator blocks), sorted.
///
/// This generalizes the reference tear's two-seed island walk (a
/// hidden-capture BFS seeded from BOTH the drive net and the output net) to an
/// arbitrary conduction decomposition. The walk is
/// a targeted SENSE-follow, seeded from the devices that DRIVE `c` (its
/// conductors: the output stage, plus any orphan output comparator whose only
/// conducted terminal is `c`). It pulls in every block those drivers SENSE, then
/// continues from the pulled block's own devices, assembling the active drive
/// loop one sense-hop at a time: the output stage senses the membrane, the
/// membrane block's reset switch senses the comparator, the comparator senses the
/// membrane, and the loop closes. Load resistors that merely conduct `c` sense
/// nothing and add nothing, and the walk deliberately never starts from the
/// adjacency (load) blocks, so it reaches the generation without dragging in the
/// downstream consumers that only sense `c` (the whole-island over-pull the first
/// version produced). A source that drives `c` purely by conduction is already an
/// adjacency block, and a generator's internal conduction is one whole block, so
/// this sense-follow needs no separate source rule.
///
/// The growth iterates to a fixpoint, is capped at `max_growth_blocks` pulled
/// blocks per candidate, and iterates sorted collections throughout so the
/// resulting capture circuits (device order is solver-visible) are deterministic.
fn grow_capture_blocks(
    sub: &Circuit,
    graph: &ConductionGraph,
    frag: &crate::decompose::rails::Fragmentation,
    adjacent: &HashMap<u32, Vec<usize>>,
    conducts_cand: &HashMap<DeviceId, Vec<u32>>,
    candidates: &[NodeId],
    policy: &CapturePolicy,
) -> HashMap<u32, Vec<usize>> {
    use std::collections::BTreeSet;

    // No growth requested: hand back plain adjacency (sorted for determinism).
    if policy.max_growth_blocks == 0 {
        return candidates
            .iter()
            .map(|c| {
                let mut v: Vec<usize> = adjacent.get(&c.0).cloned().unwrap_or_default();
                v.sort_unstable();
                (c.0, v)
            })
            .collect();
    }

    // Block -> its member devices, once: expanding a pulled generator block adds
    // its devices to the frontier so the walk follows the loop one hop further.
    let mut block_devs: HashMap<usize, Vec<DeviceId>> = HashMap::new();
    for (&id, &b) in &frag.device_block {
        block_devs.entry(b).or_default().push(id);
    }

    let mut out: HashMap<u32, Vec<usize>> = HashMap::new();
    for c in candidates {
        let mut set: BTreeSet<usize> = adjacent
            .get(&c.0)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        // Frontier devices: every device already in c's plain capture, i.e. the
        // adjacency blocks' members plus everything that conducts c (the orphan
        // conductors whose terminals are all bound). The output stage that
        // produces c senses the generator (the membrane), and on the real board
        // it is one passive hop inside an adjacency block (the stretcher's
        // comparator), NOT a direct conductor of c, so seeding from conductors
        // alone misses it. What keeps this from the whole-island over-pull is the
        // DIRECTION: the walk only follows what capture devices SENSE (their
        // inputs, pointing INTO the generation), never who senses c (the
        // downstream consumers). That is the bespoke two-seed walk's discipline.
        let mut frontier_devs: BTreeSet<DeviceId> = sub
            .iter()
            .filter_map(|(id, _)| {
                conducts_cand
                    .get(&id)
                    .is_some_and(|v| v.contains(&c.0))
                    .then_some(id)
            })
            .collect();
        for b in &set {
            if let Some(devs) = block_devs.get(b) {
                frontier_devs.extend(devs.iter().copied());
            }
        }
        let mut pulled = 0usize;
        // Oversized blocks the walk refused (see CapturePolicy::max_block_devices):
        // remembered so the fixpoint loop does not re-propose them forever.
        let mut rejected: BTreeSet<usize> = BTreeSet::new();

        loop {
            if pulled >= policy.max_growth_blocks {
                break;
            }
            // Blocks sensed by any frontier device: the generation those drivers
            // read (and, once a generator block is in, the loop it closes onto).
            let mut newblocks: BTreeSet<usize> = BTreeSet::new();
            for e in &graph.sense_edges {
                if frontier_devs.contains(&e.device) {
                    if let Some(&nb) = frag.node_block.get(&(e.node.0 as usize)) {
                        if !set.contains(&nb) && !rejected.contains(&nb) {
                            newblocks.insert(nb);
                        }
                    }
                }
            }
            if newblocks.is_empty() {
                break;
            }
            let mut changed = false;
            for b in newblocks {
                if pulled >= policy.max_growth_blocks {
                    break;
                }
                // A core-scale block is not a generator: skip it (policy doc).
                if block_devs.get(&b).map(|v| v.len()).unwrap_or(0) > policy.max_block_devices {
                    rejected.insert(b);
                    continue;
                }
                set.insert(b);
                pulled += 1;
                changed = true;
                // The pulled block's devices join the frontier: their sense
                // inputs continue the drive loop (the reset switch senses the
                // comparator, the comparator senses the membrane, ...).
                if let Some(devs) = block_devs.get(&b) {
                    frontier_devs.extend(devs.iter().copied());
                }
            }
            if !changed {
                break;
            }
        }
        out.insert(c.0, set.into_iter().collect());
    }
    out
}

/// Build a capture CLUSTER's circuit: the cluster's (merged, possibly
/// generator-grown) capture blocks' devices plus every device that conducts a
/// member candidate (they carry the load current), with pin sources for the
/// candidates OUTSIDE the cluster those devices or blocks touch. Returns the
/// circuit and the global-to-local node map. Pin sources are added as DC zero
/// and set by the caller (DC rest or PWL trains). Member candidates get NO
/// pin: they are interior, solved jointly and exactly.
fn capture_circuit(
    sub: &Circuit,
    frag: &crate::decompose::rails::Fragmentation,
    blocks: &[usize],
    members: &HashSet<u32>,
    conducts_cand: &HashMap<DeviceId, Vec<u32>>,
    companions: &[DeviceId],
    candidates: &[NodeId],
    held: &HashMap<u32, Vec<f64>>,
) -> (Circuit, HashMap<u32, u32>) {
    let my_blocks: HashSet<usize> = blocks.iter().copied().collect();
    let companion_set: HashSet<DeviceId> = companions.iter().copied().collect();
    let mut devices: Vec<DeviceId> = Vec::new();
    for (id, _) in sub.iter() {
        let in_block = frag
            .device_block
            .get(&id)
            .is_some_and(|b| my_blocks.contains(b));
        // Devices conducting a member that belong to no block (all terminals
        // held: between two candidates) still carry load current: include them.
        let orphan_on_c = !frag.device_block.contains_key(&id)
            && conducts_cand
                .get(&id)
                .is_some_and(|v| v.iter().any(|x| members.contains(x)));
        // Companion devices (absorbed drivers, replay pins: everything
        // outside the candidates' island) hold the sense boundaries.
        if in_block || orphan_on_c || companion_set.contains(&id) {
            devices.push(id);
        }
    }

    let mut cap = Circuit::new();
    cap.temp_c = sub.temp_c;
    let mut g2l: HashMap<u32, u32> = HashMap::new();
    for &id in &devices {
        let mut d = sub.devices[id.0 as usize].clone();
        d.map_nodes(&mut |gn| {
            if gn.is_ground() {
                return NodeId::GROUND;
            }
            if let Some(&ln) = g2l.get(&gn.0) {
                return NodeId(ln);
            }
            let ln = cap.node(sub.node_name(gn));
            g2l.insert(gn.0, ln.0);
            ln
        });
        cap.add(d);
    }
    // Pin every candidate OUTSIDE the cluster this capture touches.
    for o in candidates {
        if members.contains(&o.0) {
            continue;
        }
        if let Some(&ln) = g2l.get(&o.0) {
            cap.add(Device::Vsource {
                name: format!("VSTIFF_{}", sub.node_name(*o)),
                p: NodeId(ln),
                n: NodeId::GROUND,
                kind: SourceKind::Dc(0.0),
            });
        }
    }
    // Pin every HELD node (the composed executor's rails) this capture touches.
    // Value is set by the caller from the held train (VRAIL_, so ramp_all_sources
    // treats it like a rampable stimulus, not a certified VREPLAY_ boundary).
    // SORTED keys: device insertion order is solver-visible (pivots, Newton
    // paths), and HashMap iteration order is not deterministic; with several
    // held rails an unsorted walk varies the capture circuit run to run.
    let mut held_keys: Vec<u32> = held.keys().copied().collect();
    held_keys.sort_unstable();
    for h in held_keys {
        if let Some(&ln) = g2l.get(&h) {
            cap.add(Device::Vsource {
                name: format!("VRAIL_{}", sub.node_name(NodeId(h))),
                p: NodeId(ln),
                n: NodeId::GROUND,
                kind: SourceKind::Dc(0.0),
            });
        }
    }
    (cap, g2l)
}

/// Solve one capture CLUSTER: cluster-external candidates pinned at rest DC
/// (round 0) or at PWL trains (round 1+); member candidates interior. `c` is
/// the cluster representative (naming, error messages). Returns the run plus
/// its node map.
#[allow(clippy::too_many_arguments)]
fn solve_capture(
    sub: &Circuit,
    frag: &crate::decompose::rails::Fragmentation,
    blocks: &[usize],
    members: &HashSet<u32>,
    conducts_cand: &HashMap<DeviceId, Vec<u32>>,
    companions: &[DeviceId],
    c: NodeId,
    candidates: &[NodeId],
    rest: &HashMap<u32, f64>,
    held: &HashMap<u32, Vec<f64>>,
    trains: Option<BoundaryTrains<'_>>,
    seed: Option<&CaptureRun>,
    grown: bool,
    opts: &SolverOptions,
    tstop: f64,
) -> SolveResult<CaptureRun> {
    let (mut cap, g2l) = capture_circuit(
        sub,
        frag,
        blocks,
        members,
        conducts_cand,
        companions,
        candidates,
        held,
    );
    // Set the pins: PWL trains when supplied, else the DC rest values.
    for dev in cap.devices.iter_mut() {
        if let Device::Vsource { name, kind, .. } = dev {
            if let Some(net) = name.strip_prefix("VSTIFF_") {
                let other = candidates
                    .iter()
                    .find(|o| sub.node_name(**o) == net)
                    .ok_or_else(|| {
                        SolveError::internal(format!("stiff pin lost its candidate: {net}"))
                    })?;
                *kind = match trains {
                    Some((r0, grid)) => SourceKind::Pwl(
                        grid.iter()
                            .zip(&r0[&other.0])
                            .map(|(&t, &v)| PwlPoint { t, v })
                            .collect(),
                    ),
                    None => SourceKind::Dc(rest.get(&other.0).copied().unwrap_or(0.0)),
                };
            } else if let Some(net) = name.strip_prefix("VRAIL_") {
                // A held rail: pin it at its supplied train (constant DC of the
                // train's t=0 value when no grid is available, e.g. the rest
                // bootstrap).
                let train = held
                    .iter()
                    .find(|(k, _)| sub.node_name(NodeId(**k)) == net)
                    .map(|(_, v)| v)
                    .ok_or_else(|| {
                        SolveError::internal(format!("held rail pin lost its node: {net}"))
                    })?;
                *kind = match trains {
                    Some((_, grid)) => SourceKind::Pwl(
                        grid.iter()
                            .zip(train)
                            .map(|(&t, &v)| PwlPoint { t, v })
                            .collect(),
                    ),
                    None => SourceKind::Dc(train.first().copied().unwrap_or(0.0)),
                };
            }
        }
    }
    let mut sub_opts = *opts;
    sub_opts.partitioning = Partitioning::Off;

    // Warm-start the DC from the previous round's EARLY-TRANSIENT state (the
    // t=dt sample, which one honest Newton step already corrected), because
    // the cold homotopy on an extracted sub-circuit can accept a nonphysical
    // operating point that a warm start walks straight out of. The engine
    // falls back cold when a seed does not fit.
    let dc_seed: Option<Vec<f64>> = seed.and_then(|(pwf, pg2l)| {
        if pwf.time.len() < 2 {
            return None;
        }
        let ws = Workspace::new(&cap);
        let mut x = vec![0.0f64; ws.layout.size];
        for (gn, ln) in &g2l {
            // The previous round's circuit was built by the same construction
            // over the same device list, so its local ids coincide; read via
            // ITS map defensively anyway.
            let Some(pln) = pg2l.get(gn) else { continue };
            if let Some(i) = ws.layout.node(NodeId(*ln)) {
                x[i] = pwf.node_voltages[*pln as usize][1];
            }
        }
        Some(x)
    });

    let n_nodes = cap.node_count();
    let run_collect = |circuit: &Circuit, ropts: &SolverOptions| -> SolveResult<Waveforms> {
        let mut wf = Waveforms {
            time: Vec::new(),
            node_voltages: vec![Vec::new(); n_nodes],
            branch_currents: Vec::new(),
        };
        Transient::new(*ropts).run_streaming_seeded(circuit, tstop, dc_seed.as_deref(), |s| {
            wf.time.push(s.time);
            for node in 0..n_nodes {
                let v = if node == 0 { 0.0 } else { s.x[node - 1] };
                wf.node_voltages[node].push(v);
            }
        })?;
        Ok(wf)
    };
    // The FIXED-step power-on ramp ladder: quasi-static window first, step-like
    // last (a slow ramp can stall dwelling at bad biases; a fast one snaps
    // through). Fixed marches fail fast at an unresolvable event, so trying all
    // three is cheap. The stiff pins (rest values or previous iterates, internal
    // estimates, not certified data) ramp with the rest; VREPLAY_ pins stay
    // unramped inside ramp_all_sources.
    let fixed_ramp_ladder = || -> Option<Waveforms> {
        let StepControl::Fixed { dt } = sub_opts.step else {
            return None;
        };
        for scale in [200.0, 20.0, 2.0] {
            let ramp_window = (scale * dt).min(tstop / 10.0);
            if ramp_window < 2.0 * dt {
                continue;
            }
            let ramped = super::staged::ramp_all_sources(&cap, ramp_window);
            let mut ramp_opts = sub_opts;
            ramp_opts.dc_init = crate::options::DcInit::FromZero;
            if let Ok(wf) = run_collect(&ramped, &ramp_opts) {
                return Some(wf);
            }
        }
        None
    };

    // A SINGLE adaptive march for a grown capture the fixed grid cannot carry: a
    // generator-inclusive capture pulls in a self-resetting relaxation loop whose
    // reset is a sub-dt event, and only an adaptive resolution (the bespoke
    // stage-A recipe: one ramped power-on window, dt_min small) marches it. Just
    // ONE attempt (not the three-window ladder): an adaptive march is expensive,
    // and this fires once per grown capture per round on the flagship. The result
    // is sampled onto the group grid by lerp, so the non-uniform steps cost
    // nothing downstream. dt_max = dt, the CAPTURE GRID ITSELF: the certificate
    // claims CaptureGrid{dt}, and a march allowed to step coarser than the grid
    // it certifies undersamples its own claim. The bespoke precedent agrees:
    // stage A ran dt_max == sample_dt (both 2 us), not a multiple.
    let adaptive_once = || -> Option<Waveforms> {
        let StepControl::Fixed { dt } = sub_opts.step else {
            return None;
        };
        let ramp_window = (20.0 * dt).min(tstop / 10.0);
        let ramped = super::staged::ramp_all_sources(&cap, ramp_window);
        let mut aopts = sub_opts;
        aopts.dc_init = crate::options::DcInit::FromZero;
        aopts.step = StepControl::Adaptive {
            dt_initial: dt,
            dt_min: 1e-12,
            dt_max: dt,
        };
        let dbg = std::env::var("HAUKSBEE_CAPTURE_DEBUG").is_ok();
        let t0 = std::time::Instant::now();
        let r = run_collect(&ramped, &aopts).ok();
        if dbg {
            // Step census from the accepted grid: where the march spent its
            // steps (the power-on bring-up inside the ramp window vs the
            // cruise) and how small it had to go. Pure readout, no behaviour.
            let census = r.as_ref().map(|wf| {
                let n = wf.time.len();
                let bring_up = wf.time.iter().filter(|&&t| t <= ramp_window).count();
                let mut min_dt = f64::INFINITY;
                for w in wf.time.windows(2) {
                    min_dt = min_dt.min(w[1] - w[0]);
                }
                (n, bring_up, min_dt)
            });
            eprintln!(
                "  adaptive_once at {}: {} devices, {} nodes, {:.2}s, ok={}, steps={:?} (total, in ramp window {:.1e}s, min dt)",
                sub.node_name(c),
                cap.devices.len(),
                cap.node_count(),
                t0.elapsed().as_secs_f64(),
                r.is_some(),
                census,
                ramp_window,
            );
        }
        r
    };

    let dbg = std::env::var("HAUKSBEE_CAPTURE_DEBUG").is_ok();

    // GROWN captures march ADAPTIVELY FIRST, not as a rescue. A grown capture
    // contains a self-resetting generator loop, and a fixed grid does not just
    // risk failing on its reset events: it can "succeed" while MANUFACTURING
    // spikes. Measured on the flagship's torn O2 column under identical
    // imposed trains: fixed dt=1e-6 counts 15 spikes, fixed dt=5e-7 counts 1,
    // the adaptive march counts 2. The fixed-grid count is grid chatter, not
    // physics, so a fixed "success" on a generator loop cannot be trusted
    // even when Newton accepts every step. Plain (ungrown) captures keep the
    // fixed-first path bit-unchanged: no generator inside, no chatter, and
    // their refusal semantics must not be touched (review invariant).
    if grown {
        if let Some(wf) = adaptive_once() {
            return Ok((wf, g2l));
        }
        // Adaptive died: fall through to the fixed chain as the rescue,
        // inverting the original order. (Note: a grown capture that only the
        // fixed chain can carry keeps its chatter risk; the alternative is a
        // refusal, and the certificate's sag measurement still gates it.)
    }

    let t_fixed = std::time::Instant::now();
    match run_collect(&cap, &sub_opts) {
        Ok(wf) => Ok((wf, g2l)),
        // A DC-class death gets the fixed-step power-on ramp the staged fused
        // path has (ORIGINAL behaviour, every capture). A plain-adjacency
        // capture that dies past the ladder is a legitimate refusal signal (a
        // cut through an active loop, say) that must NOT be papered over: the
        // adaptive march never runs for it.
        Err(e) if e.is_dc_failure() => {
            if dbg {
                eprintln!(
                    "  fixed at {} DC-died after {:.2}s: {e}",
                    sub.node_name(c),
                    t_fixed.elapsed().as_secs_f64()
                );
            }
            let t_ladder = std::time::Instant::now();
            if let Some(wf) = fixed_ramp_ladder() {
                if dbg {
                    eprintln!(
                        "  ladder at {} ok in {:.2}s",
                        sub.node_name(c),
                        t_ladder.elapsed().as_secs_f64()
                    );
                }
                return Ok((wf, g2l));
            }
            if dbg {
                eprintln!(
                    "  ladder at {} died in {:.2}s",
                    sub.node_name(c),
                    t_ladder.elapsed().as_secs_f64()
                );
            }
            let message = format!("stiff capture at {} failed: {e}", sub.node_name(c));
            Err(e.with_message(message))
        }
        Err(e) => {
            if dbg {
                eprintln!(
                    "  fixed at {} died after {:.2}s: {e}",
                    sub.node_name(c),
                    t_fixed.elapsed().as_secs_f64()
                );
            }
            let message = format!("stiff capture at {} failed: {e}", sub.node_name(c));
            Err(e.with_message(message))
        }
    }
}

/// Sample one (global) node from a capture run onto the uniform grid.
fn sample_node(wf: &Waveforms, g2l: &HashMap<u32, u32>, node: NodeId, grid: &[f64]) -> Vec<f64> {
    match g2l.get(&node.0) {
        Some(&ln) => grid
            .iter()
            .map(|&t| lerp_at(&wf.time, &wf.node_voltages[ln as usize], t))
            .collect(),
        None => vec![0.0; grid.len()],
    }
}

/// One-grid-step time-shift-tolerant residual between two iterates of a
/// boundary train: max over samples of the smallest difference against the
/// other iterate's neighbouring cells. See the call site for why pointwise
/// comparison is the wrong metric on spiking trains.
fn shifted_residual(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len().min(b.len());
    let mut worst = 0.0f64;
    for i in 0..n {
        let lo = i.saturating_sub(1);
        let hi = (i + 1).min(n - 1);
        let mut best = f64::INFINITY;
        for j in lo..=hi {
            best = best.min((a[i] - b[j]).abs());
        }
        worst = worst.max(best);
    }
    worst
}

fn uniform_grid(dt: f64, tstop: f64) -> Vec<f64> {
    let mut grid = vec![0.0];
    let mut t = 0.0;
    let eps = dt * 1e-9;
    while t < tstop - eps {
        let h = dt.min(tstop - t);
        t += h;
        grid.push(t);
    }
    grid
}

fn lerp_at(times: &[f64], vals: &[f64], t: f64) -> f64 {
    if times.is_empty() {
        return 0.0;
    }
    match times.binary_search_by(|x| x.partial_cmp(&t).expect("non-finite sample time")) {
        Ok(i) => vals[i],
        Err(0) => vals[0],
        Err(i) if i >= times.len() => *vals.last().unwrap(),
        Err(i) => {
            let (t0, t1) = (times[i - 1], times[i]);
            vals[i - 1] + (t - t0) / (t1 - t0) * (vals[i] - vals[i - 1])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_fixtures::{
        assert_matches_within_grid, bjt, cap, comparator, fixed_opts, idc, max_error, monolith,
        pnp, pnp_blocks, res, sw, swing, up_crossings, vdc, vpulse, GND,
    };
    use hauksbee_ir::BjtModel;

    /// feed -> Rd -> X -> [block M] -> Y -> [block R], BJT loads in both
    /// blocks, a pulsed feed. `r_scale` sets the chain impedances: low = stiff
    /// boundaries, high = boundaries whose waveforms depend on the neighbours.
    fn two_cut_chain(r_scale: f64) -> (Circuit, NodeId, NodeId) {
        let mut c = Circuit::new();
        let vs = c.node("vs");
        vpulse(&mut c, "VS", vs, (3.0, 5.0), 1e-6, (0.5e-6, 0.5e-6), 1.5e-6);
        let x = c.node("x");
        res(&mut c, "Rd", vs, x, r_scale);
        let model = pnp();
        let (m, y, r) = (c.node("m"), c.node("y"), c.node("r"));
        res(&mut c, "Rm1", x, m, 2.0 * r_scale);
        let mb = c.node("mb");
        bjt(&mut c, "QM", GND, mb, m, &model);
        res(&mut c, "Rmb", mb, GND, 100e3);
        res(&mut c, "Rm2", m, y, 2.0 * r_scale);
        res(&mut c, "Rr1", y, r, 2.0 * r_scale);
        let rb = c.node("rb");
        bjt(&mut c, "QR", GND, rb, r, &model);
        res(&mut c, "Rrb", rb, GND, 100e3);
        (c, x, y)
    }

    fn max_sag(outcomes: &[StiffOutcome]) -> f64 {
        outcomes.iter().map(|o| o.sag_v).fold(0.0f64, f64::max)
    }

    fn assert_matches(c: &Circuit, exec: &StiffExecution, mono: &Waveforms, tol: f64, what: &str) {
        let (err, node) = max_error(c, &exec.waveforms.time, &exec.waveforms.node_voltages, mono);
        assert!(
            err <= tol,
            "{what}: diverged {err:.3e} at {} (tol {tol:.3e})",
            c.node_name(node)
        );
    }

    /// Stiff boundaries are accepted with a small measured sag; SOFT but
    /// contracting boundaries still converge; both match the monolith within
    /// the certificate's own sag.
    #[test]
    fn stiff_and_soft_contracting_boundaries_match_the_monolith() {
        for (r_scale, what) in [(50.0, "stiff"), (50e3, "soft")] {
            let (c, x, y) = two_cut_chain(r_scale);
            let (dt, tstop) = (100e-9, 4e-6);
            let opts = fixed_opts(dt);
            let mut refusals = Vec::new();
            let exec = execute_stiff_group(&c, &[x, y], &opts, tstop, &mut refusals)
                .expect("mechanical success")
                .unwrap_or_else(|| panic!("{what} boundaries must be accepted: {refusals:?}"));
            assert!(refusals.is_empty());
            assert_eq!(exec.outcomes.len(), 2);
            for o in &exec.outcomes {
                assert!(o.accepted && !o.bootstrapped, "{o:?}");
            }
            let sag = max_sag(&exec.outcomes);
            assert!(sag > 0.0, "cross-coupling must be measurably nonzero");
            let mono = monolith(&c, &opts, tstop);
            assert_matches(&c, &exec, &mono, (3.0 * sag).max(2e-6), what);
        }
    }

    /// A shunt-fed rail feeding four PNP blocks with pulsed bases, plus a
    /// low-impedance SIGNAL cut `MID` between blocks 0 and 1. Optionally a
    /// comparator relaxation astable loads the rail so no whole-group DC
    /// exists. Returns `(circuit, rail, shunt, feed, mid)`.
    fn composed_fixture(with_astable: bool) -> (Circuit, NodeId, DeviceId, NodeId, NodeId) {
        let mut c = Circuit::new();
        let p5 = c.node("+5V");
        vdc(&mut c, "V5", p5, 5.0);
        let rail = c.node("ANALOG_VDD");
        let shunt = res(&mut c, "R_shunt", p5, rail, 100.0);
        let vd = c.node("vd");
        vpulse(&mut c, "VD", vd, (0.0, 2.0), 1e-6, (1e-6, 1e-6), 2e-6);
        let blocks = pnp_blocks(&mut c, "", rail, 4, vd, 100e3, 10e3);
        let mid = c.node("MID");
        res(&mut c, "Rl0", blocks[0].1, mid, 1e3);
        res(&mut c, "Rl1", mid, blocks[1].1, 1e3);
        if with_astable {
            let vref = c.node("vref");
            vdc(&mut c, "VREF", vref, 2.5);
            let (osc, vc) = (c.node("osc"), c.node("vc"));
            comparator(&mut c, "CMP_AST", osc, vref, vc, 0.5);
            res(&mut c, "Rosc", osc, GND, 10e3);
            res(&mut c, "Rf", osc, vc, 10e3);
            cap(&mut c, "Cf", vc, GND, 1e-9); // tau = 10 us
            let astl = c.node("ast_load");
            sw(&mut c, "SW_AST", rail, astl, osc, (2.5, 2.0), 50.0);
            res(&mut c, "R_ast", astl, GND, 2e3);
        }
        (c, rail, shunt, p5, mid)
    }

    fn tight_opts(dt: f64) -> SolverOptions {
        SolverOptions {
            reltol: 1e-9,
            vntol: 1e-9,
            max_newton: 200,
            ..fixed_opts(dt)
        }
    }

    fn run_composed(
        c: &Circuit,
        signals: &[NodeId],
        rails: &[RailTear],
        policy: &ComposedPolicy,
        opts: &SolverOptions,
        tstop: f64,
    ) -> StiffExecution {
        let mut refusals = Vec::new();
        execute_composed_group(c, signals, rails, policy, opts, tstop, &mut refusals)
            .expect("mechanical success")
            .unwrap_or_else(|| panic!("composed execution must succeed: {refusals:?}"))
    }

    fn assert_deterministic(c: &Circuit, a: &StiffExecution, b: &StiffExecution) {
        for node in 0..c.node_count() {
            assert_eq!(
                a.waveforms.node_voltages[node],
                b.waveforms.node_voltages[node],
                "waveform at {} drifted between runs",
                c.node_name(NodeId(node as u32))
            );
        }
    }

    /// Composed execution (rail balanced + signal relaxed) matches the fused
    /// monolith where the monolith's DC converges.
    #[test]
    fn composed_rail_and_signal_matches_monolith() {
        let (c, rail, shunt, feed, mid) = composed_fixture(false);
        let (dt, tstop) = (100e-9, 4e-6);
        let opts = tight_opts(dt);
        let mono = monolith(&c, &opts, tstop);
        let rails = vec![RailTear {
            rail,
            feed,
            shunt,
            r_shunt: 100.0,
            extra_loads: Vec::new(),
        }];
        let exec = run_composed(&c, &[mid], &rails, &ComposedPolicy::default(), &opts, tstop);
        assert!(exec
            .outcomes
            .iter()
            .any(|o| o.kind == BoundaryKind::BalancedRail
                && o.node == rail
                && o.accepted
                && o.sag_v == 0.0));
        let sig = exec
            .outcomes
            .iter()
            .find(|o| o.node == mid)
            .expect("signal outcome");
        assert!(sig.accepted, "{sig:?}");
        let sag = exec
            .outcomes
            .iter()
            .filter(|o| o.kind == BoundaryKind::Signal)
            .map(|o| o.sag_v)
            .fold(0.0f64, f64::max);
        let tol = (3.0 * sag).max(1e-4);
        assert_matches_within_grid(
            &c,
            &exec.waveforms.time,
            &exec.waveforms.node_voltages,
            &mono,
            dt,
            &|_| tol,
            "composed",
        );
        let vr = &exec.waveforms.node_voltages[rail.0 as usize];
        let vr_min = vr.iter().cloned().fold(f64::MAX, f64::min);
        assert!(
            vr.iter().all(|&v| v <= 5.0 + 1e-9) && vr_min < 4.999,
            "rail must sag: {vr_min}"
        );
    }

    /// With an astable on the rail the whole-group DC has no solution, so the
    /// engine must take its decomposed seed: runs, deterministic, rail live,
    /// astable oscillating. No monolith exists to compare against.
    #[test]
    fn composed_seed_fallback_runs_without_whole_group_dc() {
        let (c, rail, shunt, feed, mid) = composed_fixture(true);
        let (dt, tstop) = (100e-9, 60e-6);
        let opts = tight_opts(dt);
        let mut mono_opts = opts;
        mono_opts.partitioning = Partitioning::Off;
        assert!(
            Transient::new(mono_opts).run(&c, tstop).is_err(),
            "fixture must have no whole-group DC"
        );
        let rails = vec![RailTear {
            rail,
            feed,
            shunt,
            r_shunt: 100.0,
            extra_loads: Vec::new(),
        }];
        let run = || run_composed(&c, &[mid], &rails, &ComposedPolicy::default(), &opts, tstop);
        let (a, b) = (run(), run());
        assert_deterministic(&c, &a, &b);
        assert!(a
            .outcomes
            .iter()
            .any(|o| o.kind == BoundaryKind::BalancedRail && o.node == rail));
        assert!(a.outcomes.iter().any(|o| o.node == mid && o.accepted));
        assert!(
            swing(&a.waveforms.node_voltages[rail.0 as usize]) > 1e-3,
            "rail never moved"
        );
        let osc = a.waveforms.node(&c, "osc").unwrap();
        let crossings = osc
            .windows(2)
            .filter(|w| (w[0] - 2.5).signum() != (w[1] - 2.5).signum())
            .count();
        assert!(
            crossings >= 3,
            "astable must oscillate (saw {crossings} crossings)"
        );
    }

    /// The HELD (feed-hold) degradation on a two-rail fixture: deterministic,
    /// both rails HeldRail, and the assembly matches the monolith of the same
    /// circuit with both rails pinned at their hold value.
    #[test]
    fn held_rails_run_deterministically_and_match_the_pinned_monolith() {
        let mut c = Circuit::new();
        let p5 = c.node("+5V");
        vdc(&mut c, "V5", p5, 5.0);
        let vd = c.node("vd");
        vpulse(&mut c, "VD", vd, (0.0, 2.0), 1e-6, (1e-6, 1e-6), 2e-6);
        let mut rails = Vec::new();
        let mut rail_nodes = Vec::new();
        let mut cols = Vec::new();
        for tag in ["A", "B"] {
            let rail = c.node(&format!("RAIL_{tag}"));
            let shunt = res(&mut c, &format!("Rsh{tag}"), p5, rail, 100.0);
            let blocks = pnp_blocks(&mut c, tag, rail, 1, vd, 100e3, 10e3);
            rails.push(RailTear {
                rail,
                feed: p5,
                shunt,
                r_shunt: 100.0,
                extra_loads: Vec::new(),
            });
            rail_nodes.push(rail);
            cols.push(blocks[0].1);
        }
        let mid = c.node("MID");
        res(&mut c, "Rl0", cols[0], mid, 1e3);
        res(&mut c, "Rl1", mid, cols[1], 1e3);

        let (dt, tstop) = (100e-9, 4e-6);
        let opts = tight_opts(dt);
        let policy = ComposedPolicy {
            max_balance_block: 1,
        };
        let run = || run_composed(&c, &[mid], &rails, &policy, &opts, tstop);
        let (a, b) = (run(), run());
        assert_deterministic(&c, &a, &b);
        for rn in &rail_nodes {
            let o = a
                .outcomes
                .iter()
                .find(|o| o.node == *rn)
                .expect("rail outcome");
            assert_eq!(o.kind, BoundaryKind::HeldRail, "{o:?}");
            assert!(o.accepted && o.sag_v == 0.0, "{o:?}");
        }
        let sig = a
            .outcomes
            .iter()
            .find(|o| o.node == mid)
            .expect("signal outcome");
        assert!(sig.kind == BoundaryKind::Signal && sig.accepted, "{sig:?}");

        let mut pinned = c.clone();
        for rn in &rail_nodes {
            let hold_v = a.waveforms.node_voltages[rn.0 as usize][0];
            vdc(
                &mut pinned,
                &format!("VPIN_{}", c.node_name(*rn)),
                *rn,
                hold_v,
            );
        }
        let mono = monolith(&pinned, &opts, tstop);
        let sag = a
            .outcomes
            .iter()
            .filter(|o| o.kind == BoundaryKind::Signal)
            .map(|o| o.sag_v)
            .fold(0.0f64, f64::max);
        let tol = (3.0 * sag).max(1e-4);
        for name in ["MID", "RAIL_A", "RAIL_B", "Ac0", "Bc0", "Ab0", "Bb0"] {
            let series = a.waveforms.node(&c, name).unwrap();
            let mseries = mono.node(&c, name).unwrap();
            for (k, &t) in a.waveforms.time.iter().enumerate() {
                let mv = lerp_at(&mono.time, mseries, t);
                assert!(
                    (series[k] - mv).abs() <= tol,
                    "{name} t={t:.3e}: {} vs {mv}",
                    series[k]
                );
            }
        }
    }

    /// A self-resetting relaxation oscillator (IDAC into a membrane cap, a
    /// comparator-driven reset switch) whose spiking output `vspk` is produced
    /// by an OUTPUT comparator that only SENSES the membrane, through a
    /// passive hop. The membrane is same-island-but-not-adjacent to `vspk`
    /// (tied in via a weak bias to VDD). Returns `(circuit, vspk, vdd, hold_v)`.
    fn generator_capture_fixture() -> (Circuit, NodeId, NodeId, f64) {
        let mut c = Circuit::new();
        let vdd = c.node("VDD");
        vdc(&mut c, "V_VDD", vdd, 5.0);
        let vmem = c.node("vmem");
        idc(&mut c, "IDRV", GND, vmem, 2e-3);
        cap(&mut c, "Cmem", vmem, GND, 2e-9);
        res(&mut c, "Rbias", vdd, vmem, 1e6);
        let vref = c.node("vref");
        vdc(&mut c, "Vref", vref, 1.0);
        let vcmp = c.node("vcmp");
        comparator(&mut c, "Kcmp", vcmp, vmem, vref, 0.5);
        sw(&mut c, "SWr", vmem, GND, vcmp, (2.5, 1.5), 200.0);
        let vref2 = c.node("vref2");
        vdc(&mut c, "Vref2", vref2, 0.6);
        let vko = c.node("vko");
        comparator(&mut c, "Kout", vko, vmem, vref2, 0.05);
        let vspk = c.node("vspk");
        res(&mut c, "R_str", vko, vspk, 1e3);
        let vload = c.node("vload");
        res(&mut c, "Rload", vspk, vload, 1e3);
        res(&mut c, "RloadG", vload, vdd, 10e3);
        let vref3 = c.node("vref3");
        vdc(&mut c, "Vref3", vref3, 2.5);
        let vsyn = c.node("vsyn");
        comparator(&mut c, "Ksyn", vsyn, vspk, vref3, 0.05);
        res(&mut c, "Rsyn", vsyn, GND, 1e3);
        (c, vspk, vdd, 5.0)
    }

    fn generator_opts(dt: f64) -> SolverOptions {
        SolverOptions {
            reltol: 1e-7,
            vntol: 1e-7,
            max_newton: 100,
            dc_init: crate::options::DcInit::FromZero,
            ..fixed_opts(dt)
        }
    }

    fn held_vdd(vdd: NodeId, hold_v: f64, dt: f64, tstop: f64) -> HashMap<u32, Vec<f64>> {
        [(vdd.0, vec![hold_v; uniform_grid(dt, tstop).len()])]
            .into_iter()
            .collect()
    }

    /// Plain adjacency leaves the sense-driven output FLAT; the
    /// generator-inclusive capture makes it spike, matching the monolith's
    /// spike count on the output net and the sense-gated load.
    #[test]
    fn generator_inclusive_capture_makes_the_output_spike() {
        let (c, vspk, vdd, hold_v) = generator_capture_fixture();
        let (dt, tstop) = (50e-9, 20e-6);
        let opts = generator_opts(dt);
        let mono = monolith(&c, &opts, tstop);
        let mono_vspk = up_crossings(mono.node(&c, "vspk").unwrap(), 2.5);
        let mono_vsyn = up_crossings(mono.node(&c, "vsyn").unwrap(), 2.5);
        assert!(
            mono_vspk >= 3,
            "oscillator must spike in the monolith (saw {mono_vspk})"
        );
        assert_eq!(mono_vspk, mono_vsyn);
        let held = held_vdd(vdd, hold_v, dt, tstop);

        let off = CapturePolicy {
            max_growth_blocks: 0,
            ..CapturePolicy::default()
        };
        let mut neg_refusals = Vec::new();
        let neg = execute_stiff_group_held_capped(
            &c,
            &[vspk],
            &held,
            &off,
            &opts,
            tstop,
            &mut neg_refusals,
        )
        .expect("mechanical success");
        if let Some(exec) = &neg {
            let ptp = swing(&exec.waveforms.node_voltages[vspk.0 as usize]);
            assert!(
                ptp < 0.5,
                "load-only capture must relax FLAT (saw ptp {ptp:.3} V)"
            );
        }

        let mut refusals = Vec::new();
        let exec = execute_stiff_group_held_capped(
            &c,
            &[vspk],
            &held,
            &CapturePolicy::default(),
            &opts,
            tstop,
            &mut refusals,
        )
        .expect("mechanical success")
        .unwrap_or_else(|| panic!("generator-inclusive capture must succeed: {refusals:?}"));
        let o = exec
            .outcomes
            .iter()
            .find(|o| o.node == vspk)
            .expect("vspk outcome");
        assert!(o.capture_growth > 0 && o.accepted, "{o:?}");
        let vspk_wf = &exec.waveforms.node_voltages[vspk.0 as usize];
        assert!(swing(vspk_wf) > 1.0);
        assert_eq!(up_crossings(vspk_wf, 2.5), mono_vspk);
        assert_eq!(
            up_crossings(exec.waveforms.node(&c, "vsyn").unwrap(), 2.5),
            mono_vsyn
        );
    }

    /// Two sense-driven columns behind the SAME generator: the grown block
    /// sets overlap, so the executor must solve them jointly as one cluster.
    #[test]
    fn overlapping_grown_captures_merge_and_match() {
        let (mut c, vspk, vdd, hold_v) = generator_capture_fixture();
        let vmem = c.node("vmem");
        let vref4 = c.node("vref4");
        vdc(&mut c, "Vref4", vref4, 0.6);
        let vko2 = c.node("vko2");
        comparator(&mut c, "Kout2", vko2, vmem, vref4, 0.05);
        let vspk2 = c.node("vspk2");
        res(&mut c, "R_str2", vko2, vspk2, 1e3);
        let vload2 = c.node("vload2");
        res(&mut c, "Rload2", vspk2, vload2, 1e3);
        res(&mut c, "RloadG2", vload2, vdd, 10e3);

        let (dt, tstop) = (50e-9, 20e-6);
        let opts = generator_opts(dt);
        let mono = monolith(&c, &opts, tstop);
        let mono_vspk = up_crossings(mono.node(&c, "vspk").unwrap(), 2.5);
        assert!(mono_vspk >= 3);
        assert_eq!(
            mono_vspk,
            up_crossings(mono.node(&c, "vspk2").unwrap(), 2.5)
        );
        let held = held_vdd(vdd, hold_v, dt, tstop);

        let mut refusals = Vec::new();
        let exec = execute_stiff_group_held_capped(
            &c,
            &[vspk, vspk2],
            &held,
            &CapturePolicy::default(),
            &opts,
            tstop,
            &mut refusals,
        )
        .expect("mechanical success")
        .unwrap_or_else(|| panic!("merged generator captures must succeed: {refusals:?}"));
        for cand in [vspk, vspk2] {
            let o = exec
                .outcomes
                .iter()
                .find(|o| o.node == cand)
                .expect("outcome");
            assert!(o.accepted && o.capture_growth > 0, "{o:?}");
            assert!(o.note.contains("merged capture"), "{o:?}");
            assert_eq!(
                up_crossings(&exec.waveforms.node_voltages[cand.0 as usize], 2.5),
                mono_vspk
            );
        }
    }

    /// A cut through an ACTIVE feedback loop (cross-coupled bistable) must
    /// either refuse with its residual on record, or converge to a TRUE root
    /// (checked against a monolith nodeset at the torn answer).
    #[test]
    fn cut_through_an_active_loop_is_refused_or_true() {
        let mut c = Circuit::new();
        let vcc = c.node("vcc");
        vdc(&mut c, "VCC", vcc, 5.0);
        let model = BjtModel::default();
        let (c1, c2, b1, b2) = (c.node("c1"), c.node("c2"), c.node("b1"), c.node("b2"));
        for (tag, col, base, xbase) in [("1", c1, b1, b2), ("2", c2, b2, b1)] {
            res(&mut c, &format!("Rc{tag}"), vcc, col, 4.7e3);
            bjt(&mut c, &format!("Q{tag}"), col, base, GND, &model);
            res(&mut c, &format!("Rx{tag}"), col, xbase, 10e3);
            res(&mut c, &format!("Rb{tag}"), base, GND, 47e3);
        }
        let opts = fixed_opts(100e-9);
        let mut refusals = Vec::new();
        let out = execute_stiff_group(&c, &[c1, c2], &opts, 2e-6, &mut refusals)
            .unwrap_or_else(|e| panic!("mechanical: {e}"));
        match &out {
            Some(exec) => {
                let mut mono_c = c.clone();
                for node in 1..c.node_count() {
                    mono_c
                        .nodesets
                        .push((NodeId(node as u32), exec.waveforms.node_voltages[node][0]));
                }
                let mono = monolith(&mono_c, &opts, 2e-6);
                let tol = (3.0 * max_sag(&exec.outcomes)).max(1e-4);
                assert_matches(&c, exec, &mono, tol, "converged-but-wrong");
            }
            None => assert!(
                refusals.iter().any(|o| !o.accepted),
                "a refusal carries its residual: {refusals:?}"
            ),
        }
    }
}
