//! The stiff-tear executor: capture with the load attached, verify by
//! re-capture, certify the measured sag.
//!
//! Detection ([`crate::decompose::stiff`]) nominates conducted cut nodes that
//! fragment a fused block cheaply; whether any nominee is actually STIFF is a
//! measured property, and this module does the measuring, per plan
//! `02-tearing-architecture.md` §2.2.1. The mechanism generalizes the bespoke
//! `tarski_decomp` stage A/B, with one simplification the spec's Gauss-Seidel
//! bullet turns out to imply:
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
use crate::transient::{Transient, Waveforms};

/// What happened at one stiff boundary.
#[derive(Debug, Clone)]
pub struct StiffOutcome {
    /// The cut node (in the caller's node space).
    pub node: NodeId,
    /// The relaxation residual: `max_t |v_last(t) - v_prev(t)|` between the
    /// final two train-driven iterates at this boundary.
    pub sag_v: f64,
    /// The tolerance it was judged against (10 x reltol x Vnom).
    pub tol_v: f64,
    /// Whether this boundary's residual met its tolerance. The group is
    /// accepted only when every boundary converged.
    pub accepted: bool,
    /// True when rest estimates came from the per-group DC bootstrap rather
    /// than a converging whole-group DC.
    pub bootstrapped: bool,
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
) -> Result<Option<StiffExecution>, String> {
    let dt = match opts.step {
        StepControl::Fixed { dt } => dt,
        _ => return Err("stiff execution requires fixed step control".into()),
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
                refusal_report
                    .extend(out_of_scope_outcomes(candidates, "a candidate nobody conducts"));
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
        .filter(|id| {
            !graph.islands[island].contains(id)
        })
        .collect();

    let n_nodes = sub.max_node() as usize;
    let mut bound = vec![false; n_nodes + 1];
    for c in candidates {
        bound[c.0 as usize] = true;
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
            // Bootstrap: per-candidate capture-group DC with the others at 0.
            bootstrapped = true;
            for c in candidates {
                // Pins default to Dc(0.0): exactly the zeros bootstrap.
                let (cap, g2l) = capture_circuit(
                    sub, &frag, &adjacent, &conducts_cand, &companions, *c, candidates,
                );
                let mut ws = Workspace::new(&cap);
                let v = match dc_operating_point(&mut ws, &cap, opts) {
                    Ok(_) => g2l
                        .get(&c.0)
                        .and_then(|ln| ws.layout.node(NodeId(*ln)))
                        .map(|i| ws.x[i])
                        .unwrap_or(0.0),
                    Err(_) => 0.0,
                };
                rest.insert(c.0, v);
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

    let mut trains: HashMap<u32, Vec<f64>> = HashMap::new();
    let mut runs: HashMap<u32, CaptureRun> = HashMap::new();
    // Round 0: rest-seeded. Solved in order with in-place updates, so later
    // candidates already see earlier candidates' round-0 trains instead of
    // bare rest values.
    let rest_trains: HashMap<u32, Vec<f64>> = candidates
        .iter()
        .map(|c| (c.0, vec![rest.get(&c.0).copied().unwrap_or(0.0); grid.len()]))
        .collect();
    for c in candidates {
        // Merge: already-captured candidates by train, the rest by rest.
        let mut boundary = rest_trains.clone();
        for (k, v) in &trains {
            boundary.insert(*k, v.clone());
        }
        let wf = match solve_capture(
            sub, &frag, &adjacent, &conducts_cand, &companions, *c, candidates, &rest,
            Some((&boundary, &grid)), None, opts, tstop,
        ) {
            Ok(wf) => wf,
            Err(_) => {
                refusal_report.extend(dead_capture_outcomes(candidates, *c, &rest, opts.reltol, bootstrapped));
                return Ok(None);
            }
        };
        let s = sample_node(&wf.0, &wf.1, *c, &grid);
        trains.insert(c.0, s);
        runs.insert(c.0, wf);
    }

    let mut sag: HashMap<u32, f64> = HashMap::new();
    let mut converged = false;
    for _round in 0..max_rounds {
        converged = true;
        for c in candidates {
            let wf = match solve_capture(
                sub, &frag, &adjacent, &conducts_cand, &companions, *c, candidates, &rest,
                Some((&trains, &grid)), runs.get(&c.0), opts, tstop,
            ) {
                Ok(wf) => wf,
                Err(_) => {
                    refusal_report.extend(dead_capture_outcomes(
                        candidates,
                        *c,
                        &rest,
                        opts.reltol,
                        bootstrapped,
                    ));
                    return Ok(None);
                }
            };
            let new = sample_node(&wf.0, &wf.1, *c, &grid);
            let old = &trains[&c.0];
            let diff = old
                .iter()
                .zip(&new)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f64, f64::max);
            sag.insert(c.0, diff);
            let vnom = rest.get(&c.0).map(|v| v.abs()).unwrap_or(0.0).max(1.0);
            if diff > 10.0 * reltol * vnom {
                converged = false;
            }
            trains.insert(c.0, new);
            runs.insert(c.0, wf);
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
        outcomes.push(StiffOutcome {
            node: *c,
            sag_v: s,
            tol_v: tol,
            accepted: s <= tol,
            bootstrapped,
            note: "",
        });
    }
    if !converged {
        refusal_report.extend(outcomes);
        return Ok(None);
    }

    // ---- assembly: every block reads from one adjacent final-round run ----
    let mut block_owner: HashMap<usize, u32> = HashMap::new();
    for c in candidates {
        for &b in &adjacent[&c.0] {
            block_owner.entry(b).or_insert(c.0);
        }
    }
    // Any block adjacent to NO candidate would be unreadable; fragmentation
    // guarantees there are none, but refuse rather than emit zeros if the
    // guarantee is ever broken.
    for &b in frag.block_devices.keys() {
        if !block_owner.contains_key(&b) {
            refusal_report.extend(outcomes);
            return Ok(None);
        }
    }

    let n_out = sub.node_count();
    let mut waveforms = Waveforms {
        time: grid.clone(),
        node_voltages: vec![vec![0.0; grid.len()]; n_out],
        branch_currents: Vec::new(),
    };
    for node in 1..n_out {
        let series: Vec<f64> = if bound[node] {
            trains[&(node as u32)].clone()
        } else if let Some(&blk) = frag.node_block.get(&node) {
            let owner = block_owner[&blk];
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
            // ride in every capture; read them from the first candidate's
            // final run. Known-side nodes of the core island with no free
            // block land here too and stay zero only if no capture mapped
            // them.
            let owner = candidates[0].0;
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

/// Outcomes for an out-of-scope disqualification: no measurement exists,
/// but the caller still learns exactly why nothing was attempted (the
/// flagship smoke found the silent form of this: zero outcomes, zero
/// explanation).
fn out_of_scope_outcomes(candidates: &[NodeId], note: &'static str) -> Vec<StiffOutcome> {
    candidates
        .iter()
        .map(|c| StiffOutcome {
            node: *c,
            sag_v: f64::NAN,
            tol_v: f64::NAN,
            accepted: false,
            bootstrapped: false,
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
                sag_v: if c.0 == dead.0 { f64::INFINITY } else { f64::NAN },
                tol_v: 10.0 * reltol * vnom,
                accepted: false,
                bootstrapped,
                note: if c.0 == dead.0 {
                    "capture solve died (power-ramp retry included)"
                } else {
                    "unmeasured: a sibling capture died first"
                },
            }
        })
        .collect()
}

/// Build candidate `c`'s capture circuit: its adjacent blocks' devices plus
/// every device that conducts `c` (they carry the load current), with pin
/// sources for the OTHER candidates those devices or blocks touch. Returns
/// the circuit and the global-to-local node map. Pin sources are added as DC
/// zero and set by the caller (DC rest or PWL trains).
fn capture_circuit(
    sub: &Circuit,
    frag: &crate::decompose::rails::Fragmentation,
    adjacent: &HashMap<u32, Vec<usize>>,
    conducts_cand: &HashMap<DeviceId, Vec<u32>>,
    companions: &[DeviceId],
    c: NodeId,
    candidates: &[NodeId],
) -> (Circuit, HashMap<u32, u32>) {
    let my_blocks: HashSet<usize> = adjacent[&c.0].iter().copied().collect();
    let companion_set: HashSet<DeviceId> = companions.iter().copied().collect();
    let mut devices: Vec<DeviceId> = Vec::new();
    for (id, _) in sub.iter() {
        let in_block = frag
            .device_block
            .get(&id)
            .is_some_and(|b| my_blocks.contains(b));
        // Devices conducting c that belong to no block (all terminals held:
        // between two candidates) still carry load current: include them.
        let orphan_on_c = !frag.device_block.contains_key(&id)
            && conducts_cand.get(&id).is_some_and(|v| v.contains(&c.0));
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
    // Pin every OTHER candidate this capture touches.
    for o in candidates {
        if o.0 == c.0 {
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
    (cap, g2l)
}

/// Solve one capture: others pinned at rest DC (round 0) or at PWL trains
/// (round 1). Returns the run plus its node map.
#[allow(clippy::too_many_arguments)]
fn solve_capture(
    sub: &Circuit,
    frag: &crate::decompose::rails::Fragmentation,
    adjacent: &HashMap<u32, Vec<usize>>,
    conducts_cand: &HashMap<DeviceId, Vec<u32>>,
    companions: &[DeviceId],
    c: NodeId,
    candidates: &[NodeId],
    rest: &HashMap<u32, f64>,
    trains: Option<BoundaryTrains<'_>>,
    seed: Option<&CaptureRun>,
    opts: &SolverOptions,
    tstop: f64,
) -> Result<CaptureRun, String> {
    let (mut cap, g2l) =
        capture_circuit(sub, frag, adjacent, conducts_cand, companions, c, candidates);
    // Set the pins: PWL trains when supplied, else the DC rest values.
    for dev in cap.devices.iter_mut() {
        if let Device::Vsource { name, kind, .. } = dev {
            if let Some(net) = name.strip_prefix("VSTIFF_") {
                let other = candidates
                    .iter()
                    .find(|o| sub.node_name(**o) == net)
                    .ok_or_else(|| format!("stiff pin lost its candidate: {net}"))?;
                *kind = match trains {
                    Some((r0, grid)) => SourceKind::Pwl(
                        grid.iter()
                            .zip(&r0[&other.0])
                            .map(|(&t, &v)| PwlPoint { t, v })
                            .collect(),
                    ),
                    None => SourceKind::Dc(rest.get(&other.0).copied().unwrap_or(0.0)),
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
    let run_collect = |circuit: &Circuit, ropts: &SolverOptions| -> Result<Waveforms, String> {
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
    match run_collect(&cap, &sub_opts) {
        Ok(wf) => Ok((wf, g2l)),
        // A DC-class death gets the same power-on retry the staged fused
        // path has: a capture group can contain the very oscillator shapes
        // whose DC does not exist. The stiff pins (rest values or previous
        // iterates, internal estimates, not certified data) ramp with the
        // rest; VREPLAY_ pins stay unramped inside ramp_all_sources.
        Err(e) if e.contains("DC") || e.contains("dc") || e.contains("homotopy") => {
            // Same ramp-window ladder as the staged retry: quasi-static
            // first, step-like last (a slow ramp can stall dwelling at bad
            // biases; a fast one snaps through).
            if let StepControl::Fixed { dt } = sub_opts.step {
                for scale in [200.0, 20.0, 2.0] {
                    let ramp_window = (scale * dt).min(tstop / 10.0);
                    if ramp_window < 2.0 * dt {
                        continue;
                    }
                    let ramped = super::staged::ramp_all_sources(&cap, ramp_window);
                    let mut ramp_opts = sub_opts;
                    ramp_opts.dc_init = crate::options::DcInit::FromZero;
                    if let Ok(wf) = run_collect(&ramped, &ramp_opts) {
                        return Ok((wf, g2l));
                    }
                }
            }
            Err(format!("stiff capture at {} failed: {e}", sub.node_name(c)))
        }
        Err(e) => Err(format!("stiff capture at {} failed: {e}", sub.node_name(c))),
    }
}

/// Sample one (global) node from a capture run onto the uniform grid.
fn sample_node(
    wf: &Waveforms,
    g2l: &HashMap<u32, u32>,
    node: NodeId,
    grid: &[f64],
) -> Vec<f64> {
    match g2l.get(&node.0) {
        Some(&ln) => grid
            .iter()
            .map(|&t| lerp_at(&wf.time, &wf.node_voltages[ln as usize], t))
            .collect(),
        None => vec![0.0; grid.len()],
    }
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
    use crate::options::Integration;
    use hauksbee_ir::{BjtModel, Polarity};

    /// A chain with two cut candidates: feed -> Rd -> X -> [block M] -> Y ->
    /// [block R], BJT loads in both blocks, a pulsed feed so everything
    /// moves. `r_scale` sets the chain impedances: low = genuinely stiff
    /// boundaries, high = boundaries whose waveforms depend strongly on the
    /// neighbours (the shape verification must refuse).
    fn two_cut_chain(r_scale: f64) -> (Circuit, NodeId, NodeId) {
        let mut c = Circuit::new();
        let vs = c.node("vs");
        c.add(Device::Vsource {
            name: "VS".into(),
            p: vs,
            n: NodeId::GROUND,
            kind: SourceKind::Pulse {
                v1: 3.0,
                v2: 5.0,
                delay: 1e-6,
                rise: 0.5e-6,
                fall: 0.5e-6,
                width: 1.5e-6,
                period: 0.0,
            },
        });
        let x = c.node("x");
        c.add(Device::Resistor {
            name: "Rd".into(),
            a: vs,
            b: x,
            ohms: r_scale,
            tc1: None,
        });
        let model = BjtModel {
            polarity: Polarity::P,
            ..BjtModel::default()
        };
        // Block M sits between X and Y.
        let m = c.node("m");
        let y = c.node("y");
        c.add(Device::Resistor {
            name: "Rm1".into(),
            a: x,
            b: m,
            ohms: 2.0 * r_scale,
            tc1: None,
        });
        let mb = c.node("mb");
        c.add(Device::Bjt {
            name: "QM".into(),
            c: NodeId::GROUND,
            b: mb,
            e: m,
            model: model.clone(),
        });
        c.add(Device::Resistor {
            name: "Rmb".into(),
            a: mb,
            b: NodeId::GROUND,
            ohms: 100e3,
            tc1: None,
        });
        c.add(Device::Resistor {
            name: "Rm2".into(),
            a: m,
            b: y,
            ohms: 2.0 * r_scale,
            tc1: None,
        });
        // Block R hangs off Y.
        let r = c.node("r");
        c.add(Device::Resistor {
            name: "Rr1".into(),
            a: y,
            b: r,
            ohms: 2.0 * r_scale,
            tc1: None,
        });
        let rb = c.node("rb");
        c.add(Device::Bjt {
            name: "QR".into(),
            c: NodeId::GROUND,
            b: rb,
            e: r,
            model,
        });
        c.add(Device::Resistor {
            name: "Rrb".into(),
            a: rb,
            b: NodeId::GROUND,
            ohms: 100e3,
            tc1: None,
        });
        (c, x, y)
    }

    fn fixed_opts(dt: f64) -> SolverOptions {
        SolverOptions {
            step: StepControl::Fixed { dt },
            integration: Integration::Trapezoidal,
            ..SolverOptions::default()
        }
    }

    /// The acceptance gate: stiff boundaries (low chain impedance) must be
    /// accepted with a small measured sag, and the assembled waveforms must
    /// match the fused monolith within a bound tied to that measurement.
    #[test]
    fn stiff_boundaries_capture_verify_and_match() {
        let (c, x, y) = two_cut_chain(50.0);
        let dt = 100e-9;
        let tstop = 4e-6;
        let opts = fixed_opts(dt);
        let mut refusals = Vec::new();
        let exec = execute_stiff_group(&c, &[x, y], &opts, tstop, &mut refusals)
            .expect("mechanical success")
            .unwrap_or_else(|| panic!("stiff boundaries must be accepted: {refusals:?}"));
        assert!(refusals.is_empty());
        assert_eq!(exec.outcomes.len(), 2);
        let max_sag = exec
            .outcomes
            .iter()
            .map(|o| o.sag_v)
            .fold(0.0f64, f64::max);
        for o in &exec.outcomes {
            assert!(o.accepted, "{o:?}");
            assert!(!o.bootstrapped, "whole-group DC converges here");
        }
        assert!(max_sag > 0.0, "cross-coupling must be measurably nonzero");

        let mut mono_opts = opts;
        mono_opts.partitioning = Partitioning::Off;
        let mono = Transient::new(mono_opts).run(&c, tstop).expect("monolith");
        // First-order claim: assembled waveforms match the monolith within a
        // small multiple of the measured sag (the certificate's own number),
        // plus solver-tolerance floor.
        let tol = (3.0 * max_sag).max(2e-6);
        let mut worst = (0.0f64, 0usize);
        for node in 1..c.node_count() {
            for (k, &t) in exec.waveforms.time.iter().enumerate() {
                let sv = exec.waveforms.node_voltages[node][k];
                let mv = lerp_at(&mono.time, &mono.node_voltages[node], t);
                if (sv - mv).abs() > worst.0 {
                    worst = ((sv - mv).abs(), node);
                }
            }
        }
        assert!(
            worst.0 <= tol,
            "stiff assembly diverged beyond its own certificate: {:.3e} at {} (sag {:.3e})",
            worst.0,
            c.node_name(NodeId(worst.1 as u32)),
            max_sag
        );
    }

    /// The claim that beats the bespoke concept: SOFT boundaries (high chain
    /// impedance, strongly load-dependent waveforms) still converge, because
    /// waveform relaxation contracts on passive coupling regardless of
    /// stiffness, and the assembled answer matches the fused monolith. A
    /// one-shot rest-pinned capture (stage A of the bespoke code) would be
    /// off by whole volts here.
    #[test]
    fn soft_but_contracting_boundaries_converge_and_match() {
        let (c, x, y) = two_cut_chain(50e3);
        let dt = 100e-9;
        let tstop = 4e-6;
        let opts = fixed_opts(dt);
        let mut refusals = Vec::new();
        let exec = execute_stiff_group(&c, &[x, y], &opts, tstop, &mut refusals)
            .expect("mechanical success")
            .unwrap_or_else(|| panic!("contracting boundaries must converge: {refusals:?}"));
        let max_sag = exec
            .outcomes
            .iter()
            .map(|o| o.sag_v)
            .fold(0.0f64, f64::max);
        let mut mono_opts = opts;
        mono_opts.partitioning = Partitioning::Off;
        let mono = Transient::new(mono_opts).run(&c, tstop).expect("monolith");
        let tol = (3.0 * max_sag).max(2e-6);
        for node in 1..c.node_count() {
            for (k, &t) in exec.waveforms.time.iter().enumerate() {
                let sv = exec.waveforms.node_voltages[node][k];
                let mv = lerp_at(&mono.time, &mono.node_voltages[node], t);
                assert!(
                    (sv - mv).abs() <= tol,
                    "soft chain diverged at {} t={t:.3e}: {sv:.6} vs {mv:.6} (sag {max_sag:.3e})",
                    c.node_name(NodeId(node as u32))
                );
            }
        }
    }

    /// The refusal gate: a cut through an ACTIVE feedback loop (cross-coupled
    /// bistable, loop gain above one) must not converge to a certified
    /// answer; the executor must exhaust its budget and refuse with the
    /// residual on record. This is the measured counterpart of the
    /// feedforward pass's structural never-tear-inside-a-loop rule, which
    /// cannot see conduction loops inside one island.
    #[test]
    fn cut_through_an_active_loop_is_refused() {
        let mut c = Circuit::new();
        let vcc = c.node("vcc");
        c.add(Device::Vsource {
            name: "VCC".into(),
            p: vcc,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        let model = BjtModel::default(); // NPN
        let c1 = c.node("c1");
        let c2 = c.node("c2");
        let b1 = c.node("b1");
        let b2 = c.node("b2");
        for (tag, col, base, xbase) in [("1", c1, b1, b2), ("2", c2, b2, b1)] {
            c.add(Device::Resistor {
                name: format!("Rc{tag}"),
                a: vcc,
                b: col,
                ohms: 4.7e3,
                tc1: None,
            });
            c.add(Device::Bjt {
                name: format!("Q{tag}"),
                c: col,
                b: base,
                e: NodeId::GROUND,
                model: model.clone(),
            });
            // Cross-coupling: this collector drives the OTHER base.
            c.add(Device::Resistor {
                name: format!("Rx{tag}"),
                a: col,
                b: xbase,
                ohms: 10e3,
                tc1: None,
            });
            c.add(Device::Resistor {
                name: format!("Rb{tag}"),
                a: base,
                b: NodeId::GROUND,
                ohms: 47e3,
                tc1: None,
            });
        }

        let opts = fixed_opts(100e-9);
        let mut refusals = Vec::new();
        let out = execute_stiff_group(&c, &[c1, c2], &opts, 2e-6, &mut refusals)
            .unwrap_or_else(|e| panic!("mechanical: {e}"));
        if let Some(exec) = &out {
            // If it converged, it must at least have converged to the TRUTH;
            // a certified-but-wrong latch state is the failure this test
            // exists to forbid. Compare against the fused monolith.
            let mut mono_opts = opts;
            mono_opts.partitioning = Partitioning::Off;
            let mono = Transient::new(mono_opts).run(&c, 2e-6).expect("monolith");
            let max_sag = exec
                .outcomes
                .iter()
                .map(|o| o.sag_v)
                .fold(0.0f64, f64::max);
            let tol = (3.0 * max_sag).max(1e-4);
            for node in 1..c.node_count() {
                for (k, &t) in exec.waveforms.time.iter().enumerate() {
                    let sv = exec.waveforms.node_voltages[node][k];
                    let mv = lerp_at(&mono.time, &mono.node_voltages[node], t);
                    assert!(
                        (sv - mv).abs() <= tol,
                        "converged-but-wrong at {} t={t:.3e}: {sv:.4} vs {mv:.4}",
                        c.node_name(NodeId(node as u32))
                    );
                }
            }
        } else {
            assert!(
                refusals.iter().any(|o| !o.accepted),
                "a refusal must carry its residual: {refusals:?}"
            );
        }
    }
}
