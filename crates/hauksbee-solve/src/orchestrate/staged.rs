//! The staged executor: capture upstream, replay downstream.
//!
//! Given a sound [`Decomposition`], this runs the stage DAG the way the
//! certificate promised: each solve group becomes its own sub-circuit, solved
//! in dependency order with the reference monolithic engine; every certified
//! free-tear node's waveform is captured from its owning (upstream) group and
//! replayed into the groups that sense it as a piecewise-linear voltage
//! source. Absorbed driver groups are never solved standalone: their devices
//! are copied into each consumer, so those boundaries carry no replay and no
//! capture tolerance at all (the strongest claim in the certificate).
//!
//! ## The capture grid, and why it equals the step grid
//!
//! The original `tarski_decomp` sampled boundary waveforms every 2 us because
//! 2 us worked on one board (the magic constant problem, saga §4). Here the
//! capture grid is the solver's own accepted-step grid: every accepted step
//! of the upstream solve becomes a PWL breakpoint. Under fixed step control,
//! the only mode this executor accepts, that grid is exactly the uniform
//! `dt` grid (the engine's crossing refinement is an adaptive-mode feature;
//! fixed-step event handling resolves discontinuities at the step boundary
//! itself), so every replay breakpoint lands exactly on a downstream solve
//! point and interpolation error at solve points is zero. Downstream engines
//! interpolate linearly between breakpoints, which is exactly the
//! first-order-hold assumption their own integrators already make between
//! steps. The certificate's [`ToleranceClaim::CaptureGrid`] is filled with
//! `dt`: the breakpoint spacing actually used. When adaptive support lands
//! (the tau/10 rule), event-bisected points will ride into the capture
//! automatically, because capture records accepted steps, whatever they are.
//!
//! A coarser grid (the plan's tau/10 rule) is a memory optimization for
//! adaptive-step runs and long captures; it lands with adaptive support.
//! This executor refuses adaptive step control rather than silently choosing
//! a grid whose error it cannot state.
//!
//! ## What the result is
//!
//! Group runs march the same fixed grid but may bisect around their own
//! events, so their accepted-sample times differ. The assembled global
//! [`Waveforms`] therefore samples every node on the uniform fixed grid
//! (linear interpolation from the owning group's accepted samples, the same
//! first-order reading a replay consumer gets). Branch currents are not
//! reassembled; node voltages are the probe surface, as in the partitioned
//! engine.
//!
//! ## Balance tears execute torn
//!
//! A group whose islands contain an accepted balance tear is not solved
//! whole: its sub-circuit is partitioned around the torn rail
//! ([`Partition::analyze_imposing_tears`], the decompose layer's decision
//! imposed on the proven executor) and marched by the partitioned engine,
//! whose outer loop is [`super::balance::settle_rails`]. The legacy
//! magic-constant rail detection never runs on this path: rails.rs decided,
//! this module executes. When the torn engine declines to construct OR
//! fails while marching (a per-block Newton death the build could not
//! foresee; the per-block path has none of the monolithic engine's
//! escalation ladder), the group falls back to the whole-group monolithic
//! solve, which is exact and merely forfeits the speedup;
//! [`StagedResult::torn_groups`] says which path each torn group actually
//! took, so a performance regression is visible instead of silent.
//!
//! Replay pins compose safely with imposed tears. A pin adds a pinned node
//! the full-circuit strand guard never saw, which looks like it could
//! strand a rail device on the extracted sub-circuit; it cannot. The strand
//! condition tests conduction terminals only, and a conduction terminal on
//! a replayed node is a contradiction: conducting that node would have
//! fused this group with its upstream during conduction analysis, so the
//! tear (and therefore the pin) would not exist. The
//! `torn_group_with_replay_pin_matches_monolith` fixture exercises the
//! composition.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/orchestrate.md

use std::collections::{BTreeMap, HashMap};

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, PwlPoint, SourceKind};

use crate::decompose::rails::BalanceTearCandidate;
use crate::decompose::verify::{
    Decomposition, Evidence, RefusedAnalysis, TearKind, TearRecord, ToleranceClaim,
};
use crate::orchestrate::capture::{execute_composed_group, execute_stiff_group, StiffOutcome};
use crate::options::{DcInit, Partitioning, SolverOptions, StepControl};
use crate::partition::{Partition, RailTear};
use crate::partitioned::PartitionedTransient;
use crate::transient::{Transient, Waveforms};

/// What a staged run produced.
#[derive(Debug)]
pub struct StagedResult {
    /// Global node voltages on the uniform fixed grid (see module doc).
    pub waveforms: Waveforms,
    /// The decomposition's certificate with every replayed free tear's
    /// capture grid filled in (no claim left pending).
    pub certificate: crate::decompose::verify::TearCertificate,
    /// Solve groups in the order they were executed (absorbed driver groups
    /// never appear: they were copied, not solved).
    pub executed_groups: Vec<usize>,
    /// Groups whose accepted balance tears actually ran on the torn
    /// (bordered-block-diagonal) engine rather than as a whole-group
    /// monolithic solve. A torn-decision group missing here still solved
    /// exactly, just without the speedup; the gap is visible so a
    /// performance regression cannot hide.
    pub torn_groups: Vec<usize>,
    /// Groups whose fused DC solve was unreachable and which were therefore
    /// carried by a power-ramp retry (every source wrapped in `Ramped`,
    /// `DcInit::FromZero`, no DC solve) rather than aborting the whole run.
    ///
    /// A group listed here means its DC operating point could not be found, so
    /// the group's early window `[0, ramp_window]` is a POWER-ON TRANSIENT: the
    /// sources ramp up from zero and the state integrates from rest. That
    /// window is honest data, not a numerical artifact, but it is NOT a settled
    /// operating point. The certificate's `t = 0` for such a group is the
    /// power-on zero, not a DC solution; downstream consumers reading this
    /// group's early samples should treat them as transient.
    pub ramped_groups: Vec<usize>,
    /// Per group: the stiff boundaries' measured outcomes (accepted runs and
    /// refusals alike), so a refused relaxation is data, not a mystery.
    pub stiff_outcomes: Vec<(usize, StiffOutcome)>,
}

/// Execute a decomposition's stage DAG. Refuses (rather than approximates)
/// when the certificate is unsound or the step control is not fixed.
pub fn run_staged(
    circuit: &Circuit,
    decomp: &Decomposition,
    opts: &SolverOptions,
    tstop: f64,
) -> Result<StagedResult, String> {
    if !decomp.certificate.sound() {
        return Err(format!(
            "staged execution refused: the decomposition is unsound\n{}",
            decomp.certificate.summary(circuit)
        ));
    }
    // Exogenous boundaries are certified BY DECLARATION: the certificate
    // trusts that the run-time environment drives them. This executor has no
    // drive plumbing for them yet (it lands with the co-sim e2e), and running
    // anyway would float exactly the nets the declaration promised were
    // driven: the dead-membrane bug wearing a certificate. Refuse instead.
    if !decomp.certificate.exogenous_boundaries.is_empty() {
        let names: Vec<_> = decomp
            .certificate
            .exogenous_boundaries
            .iter()
            .map(|n| circuit.node_name(*n))
            .collect();
        return Err(format!(
            "staged execution refused: exogenous boundaries [{}] are certified as \
             environment-driven, and this executor cannot drive them yet; run co-simulated \
             or monolithic",
            names.join(", ")
        ));
    }
    let dt = match opts.step {
        StepControl::Fixed { dt } => dt,
        _ => {
            return Err(
                "staged execution requires fixed step control: the capture grid is the step \
                 grid, and an adaptive run has no grid whose error the certificate could state"
                    .into(),
            )
        }
    };

    // Sub-circuits are solved with the reference monolithic engine: Off is
    // bit-identical to the classic solver, so each group's answer carries no
    // partitioning caveats of its own.
    let mut sub_opts = *opts;
    sub_opts.partitioning = Partitioning::Off;

    // Which groups were absorbed, and who receives each one's devices.
    //
    // A BTreeMap, not a HashMap, ON PURPOSE: the loop below extends each
    // consumer group's device list by iterating this map, so its order is the
    // order absorbed devices are pushed into the sub-circuit, which sets node
    // creation order, which the solver SEES (LU pivots, Newton convergence
    // paths, and thus the last-bit accepted values, and on the flagship even
    // WHICH group hits a DC non-convergence). Circuit construction order must
    // be deterministic; HashMap iteration order is not. Keyed by driver group,
    // sorted, so two runs build byte-identical sub-circuits.
    let absorbed: BTreeMap<usize, &[usize]> = decomp
        .drivers
        .iter()
        .map(|a| (a.driver_group, a.consumers.as_slice()))
        .collect();

    // Device inventory per group (islands are device-id lists).
    let group_devices = |g: usize| -> Vec<DeviceId> {
        decomp.dag.groups[g]
            .iter()
            .flat_map(|&isl| decomp.graph.islands[isl].iter().copied())
            .collect()
    };

    // Owner group per island, for routing balance tears to their group here
    // and each node to the run that solved it during assembly.
    let mut island_group = vec![usize::MAX; decomp.graph.islands.len()];
    for (gi, group) in decomp.dag.groups.iter().enumerate() {
        for &isl in group {
            island_group[isl] = gi;
        }
    }
    let group_of_node = |n: NodeId| -> Option<usize> {
        decomp
            .graph
            .node_island
            .get(n.0 as usize)
            .copied()
            .flatten()
            .map(|isl| island_group[isl])
    };

    // Captured tear waveforms: tear node -> (times, values) from its owning
    // group's run. A node is conducted by exactly one group, so one entry.
    let mut captured: HashMap<u32, (Vec<f64>, Vec<f64>)> = HashMap::new();
    // Per executed group: its run plus the global-to-local node map needed to
    // read a global node's series back out.
    let mut runs: HashMap<usize, (Waveforms, HashMap<u32, u32>)> = HashMap::new();
    let mut executed_groups = Vec::new();
    let mut torn_groups = Vec::new();
    let mut ramped_groups = Vec::new();
    let mut stiff_outcomes: Vec<(usize, StiffOutcome)> = Vec::new();
    let mut certificate_stiff: Vec<TearRecord> = Vec::new();
    let mut certificate_refused_nodes: Vec<NodeId> = Vec::new();

    for stage in &decomp.dag.stages {
        for &g in stage {
            if absorbed.contains_key(&g) {
                continue; // copied into consumers, never solved standalone
            }

            // Devices: the group's own, plus copies of every absorbed driver
            // assigned to it.
            let mut devices = group_devices(g);
            for (dg, consumers) in &absorbed {
                if consumers.contains(&g) {
                    devices.extend(group_devices(*dg));
                }
            }

            // Replay pins: every certified free tear into this group whose
            // upstream was actually solved (absorbed upstreams need no pin:
            // their devices are right here).
            let mut replay: Vec<(NodeId, Vec<PwlPoint>)> = Vec::new();
            for t in &decomp.dag.free_tears {
                if t.downstream != g || absorbed.contains_key(&t.upstream) {
                    continue;
                }
                let (times, vals) = captured.get(&t.node.0).ok_or_else(|| {
                    format!(
                        "stage ordering broke: tear node {} needed by group {} was never captured",
                        circuit.node_name(t.node),
                        g
                    )
                })?;
                let points = times
                    .iter()
                    .zip(vals)
                    .map(|(&t, &v)| PwlPoint { t, v })
                    .collect();
                replay.push((t.node, points));
            }
            replay.sort_by_key(|(n, _)| n.0);
            replay.dedup_by_key(|(n, _)| n.0);

            let (sub, g2l) = extract_subcircuit(circuit, &devices, &replay);

            // Accepted balance tears whose rail this group conducts, remapped
            // into the sub-circuit's namespace for the torn engine.
            let imposed: Vec<RailTear> = decomp
                .balance_tears
                .iter()
                .filter(|c| c.torn() && group_of_node(c.rail) == Some(g))
                .filter_map(|c| remap_tear(c, &devices, &g2l))
                .collect();

            // Stiff cuts nominated inside this group: the measured waveform
            // relaxation runs first; a refusal (recorded) falls through to
            // the balance-torn or fused path, which is exact regardless.
            //
            // COMPOSITION, deliberate and here on purpose: when a group's stiff
            // nominations include genuine supply RAILS (a stiff node that is
            // also a balance-tear candidate, ANY decision), those rails want the
            // EXACT scalar KCL balance, not Gauss-Seidel relaxation (they are
            // load-dependent and limit-cycle, the flagship's ANALOG_VDD/+5V).
            // The composed executor hands the rails to the partitioned balance
            // engine and relaxes the plain SIGNAL cuts on top of it. Running
            // plain relaxation on the rails first would burn a full round budget
            // of mega-group solves on a doomed contraction, so a group with
            // rails goes STRAIGHT to the composed path (imposed.is_empty() below
            // keeps the already-balance-torn groups on their own exact path).
            let mut signal_local: Vec<NodeId> = Vec::new();
            let mut composed_rails: Vec<RailTear> = Vec::new();
            for s in &decomp.stiff {
                if group_of_node(s.node) != Some(g) {
                    continue;
                }
                let Some(&ln) = g2l.get(&s.node.0) else {
                    continue;
                };
                // A stiff node that is ALSO a balance candidate (any decision:
                // the candidate carries rail/feed/shunt regardless) is a rail.
                let rail = decomp
                    .balance_tears
                    .iter()
                    .find(|c| c.rail == s.node)
                    .and_then(|c| remap_tear(c, &devices, &g2l));
                match rail {
                    Some(rt) => composed_rails.push(rt),
                    None => signal_local.push(NodeId(ln)),
                }
            }
            let mut stiff_run: Option<Waveforms> = None;
            let mut stiff_refusal_note = String::new();

            // Record a set of measured/refused composed outcomes into the
            // certificate and telemetry. A rail outcome (note "balanced rail")
            // becomes a Balance record (round-off exact); a signal becomes a
            // Stiff record with its measured sag. Every pinned node joins the
            // supply-integrity refusal, lore #12 generalized off the rails.
            // (Built lazily: most groups have no stiff nominations at all.)
            let l2g: HashMap<u32, u32> =
                if imposed.is_empty() && (!composed_rails.is_empty() || !signal_local.is_empty()) {
                    g2l.iter().map(|(&gn, &ln)| (ln, gn)).collect()
                } else {
                    HashMap::new()
                };
            if !composed_rails.is_empty() && imposed.is_empty() {
                let mut refusals = Vec::new();
                match execute_composed_group(
                    &sub,
                    &signal_local,
                    &composed_rails,
                    &sub_opts,
                    tstop,
                    &mut refusals,
                )? {
                    Some(exec) => {
                        let mut refused_nodes = Vec::new();
                        for o in &exec.outcomes {
                            let gnode = NodeId(l2g[&o.node.0]);
                            // A composed rail carries a non-empty note (how it
                            // was held); a signal's note is empty. Rails become
                            // Balance records, signals Stiff records.
                            if !o.note.is_empty() {
                                certificate_stiff.push(TearRecord {
                                    node: gnode,
                                    kind: TearKind::Balance,
                                    evidence: Evidence::BalanceEquation,
                                    tolerance: ToleranceClaim::RoundOff,
                                    upstream: None,
                                    downstream: None,
                                });
                            } else {
                                certificate_stiff.push(TearRecord {
                                    node: gnode,
                                    kind: TearKind::Stiff,
                                    evidence: Evidence::MeasuredStiffness {
                                        sag_v: o.sag_v,
                                        tol_v: o.tol_v,
                                    },
                                    tolerance: ToleranceClaim::Stiffness { sag_v: o.sag_v },
                                    upstream: None,
                                    downstream: None,
                                });
                            }
                            refused_nodes.push(gnode);
                            stiff_outcomes.push((g, StiffOutcome { node: gnode, ..o.clone() }));
                        }
                        certificate_refused_nodes.extend(refused_nodes);
                        stiff_run = Some(exec.waveforms);
                        torn_groups.push(g);
                    }
                    None => {
                        stiff_refusal_note = summarize_stiff_refusal(circuit, &l2g, &refusals);
                        for o in refusals {
                            let gnode = NodeId(l2g[&o.node.0]);
                            stiff_outcomes.push((g, StiffOutcome { node: gnode, ..o }));
                        }
                    }
                }
            } else if !signal_local.is_empty() && imposed.is_empty() {
                let mut refusals = Vec::new();
                match execute_stiff_group(&sub, &signal_local, &sub_opts, tstop, &mut refusals)? {
                    Some(exec) => {
                        // Certificate: one measured record per boundary, and
                        // the pinned nodes join the supply-integrity refusal
                        // (questions about their loading beyond sag_v must be
                        // refused, lore #12 generalized off the rails).
                        let mut refused_nodes = Vec::new();
                        for o in &exec.outcomes {
                            let gnode = NodeId(l2g[&o.node.0]);
                            certificate_stiff.push(TearRecord {
                                node: gnode,
                                kind: TearKind::Stiff,
                                evidence: Evidence::MeasuredStiffness {
                                    sag_v: o.sag_v,
                                    tol_v: o.tol_v,
                                },
                                tolerance: ToleranceClaim::Stiffness { sag_v: o.sag_v },
                                upstream: None,
                                downstream: None,
                            });
                            refused_nodes.push(gnode);
                            stiff_outcomes.push((g, StiffOutcome { node: gnode, ..o.clone() }));
                        }
                        certificate_refused_nodes.extend(refused_nodes);
                        stiff_run = Some(exec.waveforms);
                        torn_groups.push(g);
                    }
                    None => {
                        // The refusal summary rides into any later fused-path
                        // error: three flagship runs could not see WHY the
                        // mega group fell through, because the fused DC error
                        // masked the stiff refusal that caused it.
                        stiff_refusal_note = summarize_stiff_refusal(circuit, &l2g, &refusals);
                        for o in refusals {
                            let gnode = NodeId(l2g[&o.node.0]);
                            stiff_outcomes.push((g, StiffOutcome { node: gnode, ..o }));
                        }
                    }
                }
            }

            let (wf, torn, ramped) = match stiff_run {
                Some(wf) => (wf, false, false),
                None => solve_group(&sub, imposed, &sub_opts, tstop).map_err(|e| {
                    // Name the group: a half-hour flagship run whose error
                    // says only "group 3" costs another half-hour run to
                    // learn what group 3 is. Devices and a name sample turn
                    // the next failure into a standalone fixture.
                    let sample: Vec<&str> =
                        sub.devices.iter().take(6).map(|d| d.name()).collect();
                    let stiff_note = if stiff_refusal_note.is_empty() {
                        String::new()
                    } else {
                        format!(" [stiff relaxation refused first: {stiff_refusal_note}]")
                    };
                    format!(
                        "staged group {g} failed ({} devices; sample: {}){stiff_note}: {e}",
                        sub.devices.len(),
                        sample.join(", ")
                    )
                })?,
            };
            if torn {
                torn_groups.push(g);
            }
            if ramped {
                ramped_groups.push(g);
            }

            // Capture this group's outbound tear nodes for later stages.
            for t in &decomp.dag.free_tears {
                if t.upstream == g && !captured.contains_key(&t.node.0) {
                    let ln = g2l.get(&t.node.0).copied().ok_or_else(|| {
                        format!(
                            "group {g} owns tear node {} but its sub-circuit never mapped it",
                            circuit.node_name(t.node)
                        )
                    })?;
                    captured.insert(
                        t.node.0,
                        (wf.time.clone(), wf.node_voltages[ln as usize].clone()),
                    );
                }
            }

            runs.insert(g, (wf, g2l));
            executed_groups.push(g);
        }
    }

    // Assemble the global result on the uniform fixed grid.
    let grid = uniform_grid(dt, tstop);
    let n_nodes = circuit.node_count();
    let mut waveforms = Waveforms {
        time: grid.clone(),
        node_voltages: vec![vec![0.0; grid.len()]; n_nodes],
        branch_currents: Vec::new(),
    };

    for node in 1..n_nodes {
        let Some(isl) = decomp
            .graph
            .node_island
            .get(node)
            .copied()
            .flatten()
        else {
            continue; // conducted by nobody; sound() proved nothing senses it
        };
        let mut owner = island_group[isl];
        if let Some(consumers) = absorbed.get(&owner) {
            // An absorbed driver's internal waveforms are identical in every
            // consumer (zero current leaves the copies, so they cannot
            // diverge); read the first one.
            owner = consumers[0];
        }
        let (wf, g2l) = &runs[&owner];
        let Some(&ln) = g2l.get(&(node as u32)) else {
            continue;
        };
        let series = &wf.node_voltages[ln as usize];
        for (k, &t) in grid.iter().enumerate() {
            waveforms.node_voltages[node][k] = lerp_at(&wf.time, series, t);
        }
    }

    // Complete the certificate: every replayed free tear now has its grid,
    // and every measured stiff boundary gets its record with the real
    // residual (analysis recorded only nominations; the certificate states
    // what was MEASURED).
    let mut certificate = decomp.certificate.clone();
    for r in &mut certificate.records {
        if r.kind == TearKind::Free {
            if let ToleranceClaim::CaptureGrid { dt: d } = &mut r.tolerance {
                *d = Some(dt);
            }
        }
    }
    certificate.records.extend(certificate_stiff);
    if !certificate_refused_nodes.is_empty() {
        certificate_refused_nodes.sort_unstable();
        certificate_refused_nodes.dedup();
        match certificate
            .refusals
            .iter_mut()
            .find(|(r, _)| *r == RefusedAnalysis::SupplyIntegrityOnTornRail)
        {
            Some((_, nodes)) => {
                nodes.extend(certificate_refused_nodes);
                nodes.sort_unstable();
                nodes.dedup();
            }
            None => certificate.refusals.push((
                RefusedAnalysis::SupplyIntegrityOnTornRail,
                certificate_refused_nodes,
            )),
        }
    }

    Ok(StagedResult {
        waveforms,
        certificate,
        executed_groups,
        torn_groups,
        ramped_groups,
        stiff_outcomes,
    })
}

/// Solve one group's sub-circuit: torn around its imposed balance rails when
/// possible, whole otherwise. Returns the run, whether the torn engine actually
/// hosted it, and whether it was carried by the power-ramp retry (its DC was
/// unreachable).
fn solve_group(
    sub: &Circuit,
    imposed: Vec<RailTear>,
    opts: &SolverOptions,
    tstop: f64,
) -> Result<(Waveforms, bool, bool), String> {
    if !imposed.is_empty() {
        let part = Partition::analyze_imposing_tears(sub, imposed);
        if let Some(mut engine) = PartitionedTransient::try_build_from_partition(sub, opts, part) {
            let n_nodes = sub.node_count();
            let mut wf = Waveforms {
                time: Vec::new(),
                node_voltages: vec![Vec::new(); n_nodes],
                branch_currents: Vec::new(),
            };
            let run = engine.run_streaming(sub, tstop, |s| {
                wf.time.push(s.time);
                for node in 0..n_nodes {
                    let v = if node == 0 {
                        0.0
                    } else {
                        s.x.get(node - 1).copied().unwrap_or(0.0)
                    };
                    wf.node_voltages[node].push(v);
                }
            });
            if run.is_ok() {
                return Ok((wf, true, false));
            }
            // A run-time death (per-block Newton failure the build could not
            // foresee: the per-block path lacks the monolithic engine's
            // escalation ladder). The whole-group solve below is exact and
            // has that ladder; take it, discard the partial waveforms, and
            // let torn_groups show the speedup was forfeited.
        }
        // Construction declined: same fallback, same reasoning.
    }

    // The fused whole-group solve. Exact and carries the escalation ladder.
    match Transient::new(*opts).run(sub, tstop) {
        Ok(wf) => Ok((wf, false, false)),
        // The group has no reachable DC operating point (a self-resetting
        // oscillator, or a fused core that stalls in DC homotopy). Rather than
        // abort the whole staged run the way the bespoke path silently dropped
        // it, retry ONCE the way a real board resolves it: power-on. Every
        // source ramps from zero, the state integrates from zero, and there is
        // no DC solve to fail. Only when the caller asked for a DC-solved start
        // (the retry is a fallback FROM Solve; an explicit FromZero run that
        // failed is a real failure, not a DC-reachability problem).
        //
        // NOTE: the stiff capture path (orchestrate/capture.rs) is deliberately
        // NOT wired to this retry yet: a separate owner is designing that
        // integration, and composing power-on with the capture relaxation needs
        // its own thought. Left untouched on purpose.
        // The retry is gated on the error actually being a DC-reachability
        // failure: retrying an arbitrary mid-march death with a different
        // trajectory would paper over a real bug and mislabel the group as
        // DC-unreachable in ramped_groups (review finding). The DC paths
        // announce themselves in their messages; a typed error is the
        // eventual fix, the substring is the current idiom.
        Err(e)
            if opts.dc_init == DcInit::Solve
                && (e.contains("DC") || e.contains("dc") || e.contains("homotopy")) =>
        {
            let dt = match opts.step {
                StepControl::Fixed { dt } => dt,
                // A non-fixed run reached here only if run_staged's own guard
                // were bypassed; keep the original error rather than invent a
                // window with no grid.
                _ => return Err(e),
            };
            // A LADDER of ramp windows, quasi-static first, step-like last.
            // Provenance: the flagship's group 2 died at t=134us of a 200us
            // ramp: a slow ramp DWELLS in the worst bias region (junctions at
            // their knees, comparators at threshold) and a fixed-step Newton
            // can stall there, while a fast ramp snaps through it and lets
            // the implicit integrator absorb the step (the 10us smoke ramp
            // carried every group). Standard homotopy practice: when a slow
            // continuation stalls, take a bolder step. Windows are clamped
            // to a tenth of the record (a mid-ramp waveform must never be
            // the bulk of a "successful" solve) and floored at 2 steps.
            let mut errors = format!("{e}");
            for scale in [200.0, 20.0, 2.0] {
                let ramp_window = (scale * dt).min(tstop / 10.0);
                if ramp_window < 2.0 * dt {
                    continue;
                }
                let ramped_sub = ramp_all_sources(sub, ramp_window);
                let mut ramp_opts = *opts;
                ramp_opts.dc_init = DcInit::FromZero;
                match Transient::new(ramp_opts).run(&ramped_sub, tstop) {
                    Ok(wf) => return Ok((wf, false, true)),
                    Err(e2) => {
                        errors.push_str(&format!(
                            "; power-ramp retry (window {ramp_window:.3e}s) also failed: {e2}"
                        ));
                    }
                }
            }
            Err(errors)
        }
        Err(e) => Err(e),
    }
}

/// Clone `sub` with every independent source (Vsource/Isource) wrapped in a
/// `Ramped` envelope that reaches full amplitude at `ramp_window`. Used by the
/// power-on retry so a DC-unreachable group starts from zero and ramps up.
pub(crate) fn ramp_all_sources(sub: &Circuit, ramp_window: f64) -> Circuit {
    let mut c = sub.clone();
    for dev in c.devices.iter_mut() {
        match dev {
            Device::Vsource { name, kind, .. } => {
                // Replay pins carry a CERTIFIED upstream waveform: real,
                // causal boundary data solved from the upstream group's own
                // (DC-started) run. Ramping them would drive this group's
                // early window from a boundary that contradicts the upstream
                // certificate (review finding). Power-on applies to the
                // group's own supplies and stimuli, not to its neighbours'
                // already-solved truth.
                if name.starts_with("VREPLAY_") {
                    continue;
                }
                let inner = std::mem::replace(kind, SourceKind::Dc(0.0));
                *kind = inner.ramped(ramp_window);
            }
            Device::Isource { kind, .. } => {
                let inner = std::mem::replace(kind, SourceKind::Dc(0.0));
                *kind = inner.ramped(ramp_window);
            }
            _ => {}
        }
    }
    c
}

/// Join a set of stiff/composed refusal outcomes into one human line, mapping
/// each local node back to its board name, so a later fused-path error carries
/// WHY the relaxation refused instead of masking it behind a DC error.
fn summarize_stiff_refusal(
    circuit: &Circuit,
    l2g: &HashMap<u32, u32>,
    refusals: &[StiffOutcome],
) -> String {
    refusals
        .iter()
        .map(|o| {
            format!(
                "{} sag {:.3e} tol {:.3e}{}{}",
                circuit.node_name(NodeId(l2g[&o.node.0])),
                o.sag_v,
                o.tol_v,
                if o.note.is_empty() { "" } else { ": " },
                o.note
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Remap a balance-tear decision into a sub-circuit's local namespace. `None`
/// when the sub-circuit does not contain the tear's pieces (a candidate on a
/// rail this group only senses), which simply means nothing to impose.
fn remap_tear(
    c: &BalanceTearCandidate,
    devices: &[DeviceId],
    g2l: &HashMap<u32, u32>,
) -> Option<RailTear> {
    let rail = NodeId(*g2l.get(&c.rail.0)?);
    let feed = NodeId(*g2l.get(&c.feed.0)?);
    // Local device ids are positional: extract_subcircuit adds `devices` in
    // order, so the shunt's local id is its index in that list.
    let shunt_pos = devices.iter().position(|&id| id == c.shunt)?;
    Some(RailTear {
        rail,
        feed,
        shunt: DeviceId(shunt_pos as u32),
        r_shunt: c.shunt_ohms,
        extra_loads: Vec::new(),
    })
}

/// Extract `devices` (from the global circuit) into a self-contained
/// sub-circuit, adding one PWL voltage source per replay pin. Returns the
/// sub-circuit and the global-to-local node map. Node names are preserved so
/// diagnostics read like the board, not like `n17`.
fn extract_subcircuit(
    circuit: &Circuit,
    devices: &[DeviceId],
    replay: &[(NodeId, Vec<PwlPoint>)],
) -> (Circuit, HashMap<u32, u32>) {
    let mut sub = Circuit::new();
    sub.temp_c = circuit.temp_c;
    let mut g2l: HashMap<u32, u32> = HashMap::new();

    fn map_node(
        sub: &mut Circuit,
        g2l: &mut HashMap<u32, u32>,
        circuit: &Circuit,
        gn: NodeId,
    ) -> NodeId {
        if gn.is_ground() {
            return NodeId::GROUND;
        }
        if let Some(&ln) = g2l.get(&gn.0) {
            return NodeId(ln);
        }
        let ln = sub.node(circuit.node_name(gn));
        g2l.insert(gn.0, ln.0);
        ln
    }

    for &id in devices {
        let mut d = circuit.devices[id.0 as usize].clone();
        d.map_nodes(&mut |gn| map_node(&mut sub, &mut g2l, circuit, gn));
        sub.add(d);
    }
    for (gn, points) in replay {
        let ln = map_node(&mut sub, &mut g2l, circuit, *gn);
        sub.add(Device::Vsource {
            name: format!("VREPLAY_{}", circuit.node_name(*gn)),
            p: ln,
            n: NodeId::GROUND,
            kind: SourceKind::Pwl(points.clone()),
        });
    }
    (sub, g2l)
}

/// The uniform accepted-step grid a fixed-dt run marches (mirrors the run
/// loop: last step shortens to land exactly on tstop).
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

/// First-order-hold sample of a captured series at time `t` (clamped at the
/// ends, exactly like PWL replay).
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
            let w = (t - t0) / (t1 - t0);
            vals[i - 1] + w * (vals[i] - vals[i - 1])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::rails::TearMotive;
    use crate::options::Integration;
    use hauksbee_ir::SourceKind;

    fn fixed_opts(dt: f64) -> SolverOptions {
        SolverOptions {
            step: StepControl::Fixed { dt },
            integration: Integration::Trapezoidal,
            ..SolverOptions::default()
        }
    }

    fn monolith(circuit: &Circuit, dt: f64, tstop: f64) -> Waveforms {
        let mut opts = fixed_opts(dt);
        opts.partitioning = Partitioning::Off;
        Transient::new(opts).run(circuit, tstop).expect("monolith")
    }

    /// Max |staged - monolith| over every node at every uniform grid point.
    /// The monolith's samples are interpolated to the grid with the same
    /// first-order reading the staged result uses.
    fn max_error(circuit: &Circuit, staged: &StagedResult, mono: &Waveforms) -> f64 {
        let mut worst = 0.0f64;
        for node in 1..circuit.node_count() {
            let m = &mono.node_voltages[node];
            for (k, &t) in staged.waveforms.time.iter().enumerate() {
                let sv = staged.waveforms.node_voltages[node][k];
                let mv = lerp_at(&mono.time, m, t);
                worst = worst.max((sv - mv).abs());
            }
        }
        worst
    }

    /// The full staged shape in one board: a pulsed RC stack (all linear, so
    /// the driver pass absorbs it into the group that senses it), a
    /// comparator whose output is conducted in its own island, and a switch
    /// island that senses the comparator. One replayed tear (cmp_out), one
    /// absorption (the RC stack), three groups across two stages.
    fn feedforward_board() -> (Circuit, NodeId) {
        let mut c = Circuit::new();
        let vin = c.node("vin");
        let a = c.node("a");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Pulse {
                v1: 0.0,
                v2: 5.0,
                delay: 2e-6,
                rise: 1e-6,
                fall: 1e-6,
                width: 30e-6,
                period: 0.0,
            },
        });
        c.add(Device::Resistor {
            name: "R1".into(),
            a: vin,
            b: a,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Capacitor {
            name: "C1".into(),
            a,
            b: NodeId::GROUND,
            farads: 1e-9,
            ic: None,
        });
        // Comparator island: conducts cmp_out, senses the RC node.
        let cmp_out = c.node("cmp_out");
        c.add(Device::Resistor {
            name: "Rc".into(),
            a: cmp_out,
            b: NodeId::GROUND,
            ohms: 10e3,
            tc1: None,
        });
        c.add(Device::Comparator {
            name: "CMP".into(),
            out: cmp_out,
            inp: a,
            inn: NodeId::GROUND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });
        // Switch island: senses cmp_out, conducts its own path.
        let s = c.node("s");
        let o = c.node("o");
        c.add(Device::Vsource {
            name: "V2".into(),
            p: s,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.3),
        });
        c.add(Device::VSwitch {
            name: "SW".into(),
            a: s,
            b: o,
            ctrl_p: cmp_out,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 10.0,
            roff: 1e9,
        });
        c.add(Device::Resistor {
            name: "RL".into(),
            a: o,
            b: NodeId::GROUND,
            ohms: 10e3,
            tc1: None,
        });
        (c, o)
    }

    /// The transient gate: the staged run must match the monolith within the
    /// capture-grid claim. Observed error on this fixture is ~1e-12 away
    /// from switching instants and bounded by one grid interval around them
    /// (both runs place the comparator edge at the same time to round-off,
    /// since the upstream RC is solved from identical equations); the 1e-6
    /// bar is the certificate's own exactness gate with margin.
    #[test]
    fn staged_replay_matches_monolith_within_capture_grid() {
        let (c, o) = feedforward_board();
        let dt = 50e-9;
        let tstop = 20e-6;
        let opts = fixed_opts(dt);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        // Three groups in a chain: the (absorbed) RC stack still occupies
        // stage 0 of the DAG; absorption changes execution, not structure.
        assert_eq!(d.dag.stages.len(), 3, "{:?}", d.dag.stages);
        assert_eq!(d.drivers.len(), 1, "the RC stack absorbs: {:?}", d.drivers);

        let staged = run_staged(&c, &d, &opts, tstop).expect("staged run");
        let mono = monolith(&c, dt, tstop);

        let err = max_error(&c, &staged, &mono);
        assert!(err <= 1e-6, "staged diverged from monolith: {err:.3e}");
        // The switch actually fired (the fixture is not vacuous).
        let vo = staged.waveforms.node_voltages[o.0 as usize].last().copied();
        assert!(vo.unwrap() > 3.0, "switch never closed: {vo:?}");
        // Every replayed free tear's grid is filled: no claim left pending.
        for r in &staged.certificate.records {
            if r.kind == TearKind::Free {
                assert_eq!(
                    r.tolerance,
                    ToleranceClaim::CaptureGrid { dt: Some(dt) },
                    "{r:?}"
                );
            }
        }
    }

    /// The DC-boundary gate: with static sources everything settles, replay
    /// is a constant, and staged must equal the monolith to round-off.
    #[test]
    fn dc_boundaries_match_to_round_off() {
        let (mut c, _) = feedforward_board();
        // Make the ramp source DC so every boundary is static after t=0.
        if let Device::Vsource { kind, .. } = &mut c.devices[0] {
            *kind = SourceKind::Dc(5.0);
        }
        let dt = 100e-9;
        let tstop = 5e-6;
        let opts = fixed_opts(dt);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        let staged = run_staged(&c, &d, &opts, tstop).expect("staged run");
        let mono = monolith(&c, dt, tstop);
        let err = max_error(&c, &staged, &mono);
        assert!(err <= 1e-9, "DC boundaries must be round-off exact: {err:.3e}");
    }

    /// Refuse-rather-than-fake: an unsound decomposition (floating sense
    /// net) must be refused with the certificate's own words, and adaptive
    /// step control must be refused because no capture grid can be claimed.
    #[test]
    fn unsound_or_adaptive_runs_are_refused() {
        let mut c = Circuit::new();
        let x = c.node("x");
        let y = c.node("y");
        c.add(Device::Vsource {
            name: "V".into(),
            p: x,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        let sel = c.node("sel_floating");
        c.add(Device::VSwitch {
            name: "S".into(),
            a: x,
            b: y,
            ctrl_p: sel,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 10.0,
            roff: 1e9,
        });
        c.add(Device::Resistor {
            name: "R".into(),
            a: y,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        let err = run_staged(&c, &d, &fixed_opts(1e-7), 1e-6).unwrap_err();
        assert!(err.contains("unsound"), "{err}");

        let (c2, _) = feedforward_board();
        let d2 = Decomposition::analyze(&c2, TearMotive::Profit);
        let adaptive = SolverOptions::default(); // adaptive step control
        let err2 = run_staged(&c2, &d2, &adaptive, 1e-6).unwrap_err();
        assert!(err2.contains("fixed step"), "{err2}");
    }

    /// The Tarski shape end to end, in miniature: a shunt-fed PNP mirror
    /// array (accepted balance tear) whose block-0 collector a comparator
    /// senses (free tear), whose output gates a switch island (second free
    /// tear). The staged run must put the array group on the TORN engine,
    /// driven by the rails.rs decision rather than the legacy detection, and
    /// the whole three-stage pipeline must match the monolith.
    #[test]
    fn balance_torn_group_matches_monolith() {
        use hauksbee_ir::{BjtModel, Polarity};
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
        let mut c0 = NodeId::GROUND;
        for k in 0..24 {
            let base = c.node(&format!("b{k}"));
            let col = c.node(&format!("c{k}"));
            if k == 0 {
                c0 = col;
            }
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
        // Comparator island: watches block 0's collector.
        let cmp_out = c.node("cmp_out");
        c.add(Device::Resistor {
            name: "Rcmp".into(),
            a: cmp_out,
            b: NodeId::GROUND,
            ohms: 10e3,
            tc1: None,
        });
        c.add(Device::Comparator {
            name: "CMP".into(),
            out: cmp_out,
            inp: c0,
            inn: NodeId::GROUND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });
        // Switch island: gated by the comparator.
        let s = c.node("s");
        let o = c.node("o");
        c.add(Device::Vsource {
            name: "V2".into(),
            p: s,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.3),
        });
        c.add(Device::VSwitch {
            name: "SW".into(),
            a: s,
            b: o,
            ctrl_p: cmp_out,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 10.0,
            roff: 1e9,
        });
        c.add(Device::Resistor {
            name: "RL".into(),
            a: o,
            b: NodeId::GROUND,
            ohms: 10e3,
            tc1: None,
        });

        let dt = 100e-9;
        let tstop = 5e-6;
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        let accepted: Vec<_> = d.balance_tears.iter().filter(|t| t.torn()).collect();
        assert_eq!(accepted.len(), 1, "{:?}", d.balance_tears);
        assert_eq!(accepted[0].rail, rail);

        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged");
        assert_eq!(
            staged.torn_groups.len(),
            1,
            "the array group must run torn: {:?}",
            staged.torn_groups
        );

        let mono = monolith(&c, dt, tstop);
        let err = max_error(&c, &staged, &mono);
        assert!(err <= 1e-6, "torn staged diverged from monolith: {err:.3e}");
        // The pipeline end actually energized (the fixture is not vacuous).
        let vo = staged.waveforms.node(&c, "o").unwrap().last().copied();
        assert!(vo.unwrap() > 3.0, "switch never closed: {vo:?}");
    }

    /// The stacked-feed cascade end to end (rails.rs's `stacked_cascade`
    /// shape): SRC -> 500R -> MID (two PNP loads) -> 1k -> INNER (24-block PNP
    /// array). BOTH rails are accepted balance tears now that the executor
    /// carries the inter-rail shunt term, so this is the gate that proves the
    /// carry is EXACT rather than merely permitted. The R2 shunt between the
    /// two torn rails lives in no block: INNER books it as its feed term and
    /// MID books it as an analytic child draw. If either side dropped it, MID
    /// would sit ~0.7 V off and this two-sided compare would fail.
    #[test]
    fn cascaded_rails_match_monolith() {
        use hauksbee_ir::{BjtModel, Polarity};
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
        // Two small loads on MID (keep it above the fanout floor).
        for k in 0..2 {
            let b = c.node(&format!("mb{k}"));
            let col = c.node(&format!("mc{k}"));
            c.add(Device::Bjt {
                name: format!("MQ{k}"),
                c: col,
                b,
                e: mid,
                model,
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
        // The array on INNER.
        for k in 0..24 {
            let b = c.node(&format!("ib{k}"));
            let col = c.node(&format!("ic{k}"));
            c.add(Device::Bjt {
                name: format!("IQ{k}"),
                c: col,
                b,
                e: inner,
                model,
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

        let dt = 100e-9;
        let tstop = 5e-6;
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        let accepted: Vec<_> = d.balance_tears.iter().filter(|t| t.torn()).collect();
        assert_eq!(
            accepted.len(),
            2,
            "both cascade rails must tear: {:?}",
            d.balance_tears
        );
        assert!(
            accepted.iter().any(|t| t.rail == mid) && accepted.iter().any(|t| t.rail == inner),
            "the accepted set must be exactly {{MID, INNER}}: {:?}",
            d.balance_tears
        );

        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged");
        assert!(
            !staged.torn_groups.is_empty(),
            "the cascade group must run torn: {:?}",
            staged.torn_groups
        );

        let mono = monolith(&c, dt, tstop);
        // Two-sided capture-grid compare, copied from
        // `torn_group_with_replay_pin_matches_monolith`. This board is all
        // DC, so away from any (nonexistent) edge the runs agree flatly; the
        // two-sided form is kept for uniformity with the suite.
        let tol = 1e-6;
        for node in 1..c.node_count() {
            for (k, &t) in staged.waveforms.time.iter().enumerate() {
                let sv = staged.waveforms.node_voltages[node][k];
                let m = &mono.node_voltages[node];
                if (sv - lerp_at(&mono.time, m, t)).abs() <= tol {
                    continue;
                }
                let edge_hit = (0..=8).any(|j| {
                    let tt = t - dt + (j as f64) * (dt / 4.0);
                    (lerp_at(&mono.time, m, tt) - sv).abs() <= tol
                });
                assert!(
                    edge_hit,
                    "cascaded torn group diverged from the monolith: node {} t={t:.3e} \
                     staged {sv:.9} vs mono {:.9}",
                    c.node_name(NodeId(node as u32)),
                    lerp_at(&mono.time, m, t)
                );
            }
        }

        // DC-settled variant: with static sources the whole board is a DC
        // operating point at every step, so the final sample must equal the
        // monolith to round-off (the stronger claim). Both rails moved off
        // their unloaded feed values, so the tear is not vacuous.
        let last = staged.waveforms.time.len() - 1;
        for node in 1..c.node_count() {
            let sv = staged.waveforms.node_voltages[node][last];
            let mv = lerp_at(&mono.time, &mono.node_voltages[node], staged.waveforms.time[last]);
            assert!(
                (sv - mv).abs() <= 1e-9,
                "DC-settled cascade must be round-off exact: node {} staged {sv:.12} vs mono {mv:.12}",
                c.node_name(NodeId(node as u32))
            );
        }
        let vmid = *staged.waveforms.node(&c, "MID").unwrap().last().unwrap();
        let vinner = *staged.waveforms.node(&c, "INNER").unwrap().last().unwrap();
        assert!(
            vmid < 4.99 && vinner < vmid - 0.1,
            "the cascade must actually drop across both shunts: MID {vmid} INNER {vinner}"
        );
    }

    /// The bypass-cap exactness gate: a decoupling cap on a balance-torn
    /// rail rides in a boundary-only island whose current enters the
    /// balance books. This is the shape the old detector REFUSED because
    /// its island analysis dropped the cap; the refusal is lifted, and this
    /// gate is the proof the lift is exact rather than merely permitted.
    /// A pulsed base drive keeps dv/dt nonzero so the cap's current is a
    /// live term, not a settled zero.
    #[test]
    fn bypass_cap_on_torn_rail_matches_monolith() {
        use hauksbee_ir::{BjtModel, Polarity};
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
        c.add(Device::Capacitor {
            name: "Cbypass".into(),
            a: rail,
            b: NodeId::GROUND,
            farads: 100e-9,
            ic: None,
        });
        let model = BjtModel {
            polarity: Polarity::P,
            ..BjtModel::default()
        };
        for k in 0..24 {
            let base = c.node(&format!("b{k}"));
            let col = c.node(&format!("c{k}"));
            c.add(Device::Bjt {
                name: format!("Q{k}"),
                c: col,
                b: base,
                e: rail,
                model: model.clone(),
            });
            if k == 0 {
                // Pulse block 0's bias THROUGH its base resistor so the rail
                // (and the cap) sees a transient: the C*dv/dt term goes live.
                // Driving the base with an ideal source directly would
                // forward-bias the EB junction by volts, which is not a
                // circuit, it is a dead transistor.
                let drv = c.node("b0drv");
                c.add(Device::Vsource {
                    name: "VB0".into(),
                    p: drv,
                    n: NodeId::GROUND,
                    kind: SourceKind::Pulse {
                        v1: 0.0,
                        v2: 3.0,
                        delay: 1.05e-6,
                        rise: 0.5e-6,
                        fall: 0.5e-6,
                        width: 2e-6,
                        period: 0.0,
                    },
                });
                c.add(Device::Resistor {
                    name: "Rb0".into(),
                    a: base,
                    b: drv,
                    ohms: 100e3,
                    tc1: None,
                });
            } else {
                c.add(Device::Resistor {
                    name: format!("Rb{k}"),
                    a: base,
                    b: NodeId::GROUND,
                    ohms: 100e3,
                    tc1: None,
                });
            }
            c.add(Device::Resistor {
                name: format!("Rc{k}"),
                a: col,
                b: NodeId::GROUND,
                ohms: 10e3,
                tc1: None,
            });
        }

        let dt = 100e-9;
        let tstop = 6e-6;
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert!(
            d.balance_tears.iter().any(|t| t.torn()),
            "{:?}",
            d.balance_tears
        );
        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged");
        assert!(!staged.torn_groups.is_empty(), "must actually run torn");
        let mono = monolith(&c, dt, tstop);
        // Two-sided compare at 5e-6. The bar is wider than the suite's usual
        // 1e-6 for a root-caused reason: Newton acceptance is update-based
        // only (newton.rs converged(): |dx| <= reltol*|x| + vntol, no
        // residual test), so at a junction-knee step each engine's path
        // stops at a different point inside the reltol*|x| band (~4.3 mV at
        // this base node), and the accepted value is the quadratic Newton
        // image of the last step: a ~1.2e-6 V acceptance spread, present in
        // BOTH formulations, insensitive to vntol by construction. Verified
        // by a 400-warm-start repro (spread quadratic in reltol, unmoved by
        // vntol; pnjlim inactive at termination step sizes, hypothesis
        // refuted). The designed fix, a node-row residual gate reusing the
        // line-search matvec behind a strategy flag, collapses the band to
        // ~1e-12 and lets this bar return to 1e-6; it lands with the
        // strategy ladder because it changes accepted values bitwise
        // everywhere. A persistent offset, the failure this gate hunts,
        // still fails at this bar (flat regions agree to 1e-15).
        let tol = 5e-6;
        for node in 1..c.node_count() {
            for (k, &t) in staged.waveforms.time.iter().enumerate() {
                let sv = staged.waveforms.node_voltages[node][k];
                let m = &mono.node_voltages[node];
                if (sv - lerp_at(&mono.time, m, t)).abs() <= tol {
                    continue;
                }
                let edge_hit = (0..=8).any(|j| {
                    let tt = t - dt + (j as f64) * (dt / 4.0);
                    (lerp_at(&mono.time, m, tt) - sv).abs() <= tol
                });
                assert!(
                    edge_hit,
                    "bypass cap current leaked from the balance books: node {} t={t:.3e} \
                     staged {sv:.9} vs mono {:.9}",
                    c.node_name(NodeId(node as u32)),
                    lerp_at(&mono.time, m, t)
                );
            }
        }
        // The transient really moved the rail (the cap term was live).
        let vr = staged.waveforms.node(&c, "ANALOG_VDD").unwrap();
        let swing = vr.iter().cloned().fold(f64::MIN, f64::max)
            - vr.iter().cloned().fold(f64::MAX, f64::min);
        assert!(swing > 1e-3, "rail never moved; the gate is vacuous: {swing}");
    }

    /// The composition the review flagged as unexercised: a group with BOTH
    /// an imposed rail tear AND an inbound replay pin. An upstream pulsed
    /// comparator gates a switch inside the torn array (block 0's base
    /// return path), so the balance loop must track a load change that
    /// arrives THROUGH the replay boundary mid-run. The replay pin becomes
    /// a cut source inside the partitioned engine (evaluated per step) and
    /// must not disturb the tear's exactness.
    #[test]
    fn torn_group_with_replay_pin_matches_monolith() {
        use hauksbee_ir::{BjtModel, Polarity};
        let mut c = Circuit::new();
        // Upstream island: pulsed source, sensed by the comparator.
        let vin = c.node("vin");
        c.add(Device::Vsource {
            name: "VP".into(),
            p: vin,
            n: NodeId::GROUND,
            kind: SourceKind::Pulse {
                v1: 0.0,
                v2: 5.0,
                delay: 1e-6,
                rise: 0.5e-6,
                fall: 0.5e-6,
                width: 10e-6,
                period: 0.0,
            },
        });
        let cmp_out = c.node("cmp_out");
        c.add(Device::Resistor {
            name: "Rcmp".into(),
            a: cmp_out,
            b: NodeId::GROUND,
            ohms: 10e3,
            tc1: None,
        });
        c.add(Device::Comparator {
            name: "CMP".into(),
            out: cmp_out,
            inp: vin,
            inn: NodeId::GROUND,
            out_lo: 0.0,
            out_hi: 5.0,
            hysteresis: 1e-3,
        });
        // The array: shunt-fed rail, 24 PNP blocks; block 0's base return
        // runs through a switch gated by the upstream comparator.
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
        for k in 0..24 {
            let base = c.node(&format!("b{k}"));
            let col = c.node(&format!("c{k}"));
            c.add(Device::Bjt {
                name: format!("Q{k}"),
                c: col,
                b: base,
                e: rail,
                model: model.clone(),
            });
            if k == 0 {
                // Base return via the gated switch: the block's bias, and
                // with it the rail current, changes when the replayed edge
                // arrives.
                let ret = c.node("b0_ret");
                c.add(Device::Resistor {
                    name: "Rb0".into(),
                    a: base,
                    b: ret,
                    ohms: 100e3,
                    tc1: None,
                });
                c.add(Device::VSwitch {
                    name: "SW0".into(),
                    a: ret,
                    b: NodeId::GROUND,
                    ctrl_p: cmp_out,
                    ctrl_n: NodeId::GROUND,
                    von: 2.0,
                    voff: 1.0,
                    ron: 10.0,
                    roff: 1e9,
                });
            } else {
                c.add(Device::Resistor {
                    name: format!("Rb{k}"),
                    a: base,
                    b: NodeId::GROUND,
                    ohms: 100e3,
                    tc1: None,
                });
            }
            c.add(Device::Resistor {
                name: format!("Rc{k}"),
                a: col,
                b: NodeId::GROUND,
                ohms: 10e3,
                tc1: None,
            });
        }

        let dt = 100e-9;
        let tstop = 6e-6;
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert!(
            d.balance_tears.iter().any(|t| t.torn() && t.rail == rail),
            "{:?}",
            d.balance_tears
        );
        assert!(
            !d.dag.free_tears.is_empty(),
            "the comparator boundary must be a free tear: {:?}",
            d.dag.free_tears
        );

        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged");
        assert!(
            !staged.torn_groups.is_empty(),
            "the array group must run torn despite the replay pin: {:?}",
            staged.torn_groups
        );
        let mono = monolith(&c, dt, tstop);
        // Two-sided compare per the capture-grid claim (same semantics as
        // tests/staged_property.rs): a replayed edge may arrive up to one
        // grid interval late downstream, so a point passes when the values
        // agree OR the reference attains the same value within +/- dt. This
        // fixture's worst raw pointwise error (6.6e-6 at c0, t=1.1us, the
        // switching instant, gain-amplified through Q0) is exactly that
        // transient; away from the edge the runs agree below 1e-6.
        let tol = 1e-6;
        for node in 1..c.node_count() {
            for (k, &t) in staged.waveforms.time.iter().enumerate() {
                let sv = staged.waveforms.node_voltages[node][k];
                let m = &mono.node_voltages[node];
                if (sv - lerp_at(&mono.time, m, t)).abs() <= tol {
                    continue;
                }
                let edge_hit = (0..=8).any(|j| {
                    let tt = t - dt + (j as f64) * (dt / 4.0);
                    (lerp_at(&mono.time, m, tt) - sv).abs() <= tol
                });
                assert!(
                    edge_hit,
                    "replay-pinned torn group diverged beyond the capture-grid claim: \
                     node {} t={t:.3e} staged {sv:.9} vs mono {:.9}",
                    c.node_name(NodeId(node as u32)),
                    lerp_at(&mono.time, m, t)
                );
            }
        }
        // The mid-run load change actually happened: block 0's collector
        // moved when the comparator fired (the fixture is not vacuous).
        let c0 = staged.waveforms.node(&c, "c0").unwrap();
        let swing = c0.iter().cloned().fold(f64::MIN, f64::max)
            - c0.iter().cloned().fold(f64::MAX, f64::min);
        assert!(swing > 0.05, "block 0 never responded to the edge: {swing}");
    }

    /// End-to-end stiff wiring: analysis nominates cut nodes on a chain of
    /// BJT blocks, run_staged executes the measured waveform relaxation,
    /// the certificate carries Stiff records with real residuals, and the
    /// result matches the fused monolith within the certificate's own
    /// numbers.
    #[test]
    fn stiff_cuts_flow_through_run_staged() {
        use crate::decompose::stiff::StiffPolicy;
        use hauksbee_ir::{BjtModel, Polarity};
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
        let model = BjtModel {
            polarity: Polarity::P,
            ..BjtModel::default()
        };
        // A chain of three BJT blocks joined through low-impedance nets.
        let mut prev = vs;
        for k in 0..3 {
            let joint = c.node(&format!("j{k}"));
            c.add(Device::Resistor {
                name: format!("Rj{k}"),
                a: prev,
                b: joint,
                ohms: 100.0,
                tc1: None,
            });
            let b = c.node(&format!("b{k}"));
            c.add(Device::Bjt {
                name: format!("Q{k}"),
                c: NodeId::GROUND,
                b,
                e: joint,
                model: model.clone(),
            });
            c.add(Device::Resistor {
                name: format!("Rb{k}"),
                a: b,
                b: NodeId::GROUND,
                ohms: 100e3,
                tc1: None,
            });
            prev = joint;
        }

        let dt = 100e-9;
        let tstop = 4e-6;
        let opts = fixed_opts(dt);
        let d = Decomposition::analyze_with_boundaries(
            &c,
            TearMotive::Profit,
            Default::default(),
            Default::default(),
            StiffPolicy {
                min_block_devices: 2,
                max_probes_per_block: 8,
            },
            &[],
        );
        assert!(
            !d.stiff.is_empty(),
            "the chain must yield stiff nominations: {:?}",
            d.balance_tears
        );

        let staged = run_staged(&c, &d, &opts, tstop).expect("staged");
        let accepted: Vec<_> = staged
            .stiff_outcomes
            .iter()
            .filter(|(_, o)| o.accepted)
            .collect();
        assert!(
            !accepted.is_empty(),
            "the relaxation must certify at least one boundary: {:?}",
            staged.stiff_outcomes
        );
        let stiff_records: Vec<_> = staged
            .certificate
            .records
            .iter()
            .filter(|r| r.kind == TearKind::Stiff)
            .collect();
        assert_eq!(stiff_records.len(), accepted.len());
        for r in &stiff_records {
            match r.tolerance {
                ToleranceClaim::Stiffness { sag_v } => {
                    assert!(sag_v.is_finite(), "{r:?}");
                }
                ref other => panic!("stiff record with wrong claim: {other:?}"),
            }
        }
        // The pinned nodes joined the supply-integrity refusal.
        assert!(staged
            .certificate
            .permits(crate::decompose::verify::RefusedAnalysis::SupplyIntegrityOnTornRail)
            .is_err());

        let max_sag = accepted
            .iter()
            .map(|(_, o)| o.sag_v)
            .fold(0.0f64, f64::max);
        let mono = monolith(&c, dt, tstop);
        let tol = (3.0 * max_sag).max(2e-6);
        for node in 1..c.node_count() {
            for (k, &t) in staged.waveforms.time.iter().enumerate() {
                let sv = staged.waveforms.node_voltages[node][k];
                let mv = lerp_at(&mono.time, &mono.node_voltages[node], t);
                assert!(
                    (sv - mv).abs() <= tol,
                    "staged stiff diverged at {} t={t:.3e}: {sv:.6} vs {mv:.6} (sag {max_sag:.3e})",
                    c.node_name(NodeId(node as u32))
                );
            }
        }
    }

    /// Absorption exactness: the drivers.rs Thevenin shape, replicated into
    /// two consumers, must match the monolith to round-off (no replay
    /// happens at all, so not even a capture grid separates them).
    #[test]
    fn replicated_driver_matches_to_round_off() {
        let mut c = Circuit::new();
        let vdrv = c.node("vdrv");
        let sel = c.node("sel");
        c.add(Device::Vsource {
            name: "Vdrv".into(),
            p: vdrv,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        c.add(Device::Resistor {
            name: "Rdrv".into(),
            a: vdrv,
            b: sel,
            ohms: 1e3,
            tc1: None,
        });
        for tag in ["x", "y"] {
            let s = c.node(&format!("{tag}_src"));
            let o = c.node(&format!("{tag}_out"));
            c.add(Device::Vsource {
                name: format!("V{tag}"),
                p: s,
                n: NodeId::GROUND,
                kind: SourceKind::Dc(3.3),
            });
            c.add(Device::VSwitch {
                name: format!("SW{tag}"),
                a: s,
                b: o,
                ctrl_p: sel,
                ctrl_n: NodeId::GROUND,
                von: 2.0,
                voff: 1.0,
                ron: 1.0,
                roff: 1e9,
            });
            c.add(Device::Resistor {
                name: format!("RL{tag}"),
                a: o,
                b: NodeId::GROUND,
                ohms: 10e3,
                tc1: None,
            });
        }
        let dt = 100e-9;
        let tstop = 2e-6;
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert_eq!(d.drivers.len(), 1);
        assert_eq!(d.drivers[0].consumers.len(), 2);
        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged");
        let mono = monolith(&c, dt, tstop);
        let err = max_error(&c, &staged, &mono);
        assert!(err <= 1e-9, "absorption must be exact: {err:.3e}");
        // Both switches closed from the shared, replicated driver.
        for tag in ["x_out", "y_out"] {
            let series = staged
                .waveforms
                .node(&c, tag)
                .unwrap_or_else(|| panic!("{tag} missing"));
            assert!(
                series.last().copied().unwrap() > 3.0,
                "{tag} never energized"
            );
        }
    }
}

/// One group's DC health, from [`probe_groups_dc`].
#[derive(Debug)]
pub struct GroupDcProbe {
    /// Group index (the same numbering staged errors use).
    pub group: usize,
    /// Device count of the extracted sub-circuit (absorbed copies included).
    pub devices: usize,
    /// A few device names, enough to recognize the group on a schematic.
    pub sample: Vec<String>,
    /// Whether the group's own DC operating point converges. `None` when the
    /// group was skipped (over the size cap, or absorbed).
    pub dc_ok: Option<bool>,
}

/// Diagnostic: enumerate the solve groups exactly as [`run_staged`] would and
/// try ONLY each group's DC operating point. Exists because a failing group
/// deep inside a half-hour staged run is unidentifiable at acceptable cost
/// without it (the flagship burned two runs learning that). Groups larger
/// than `size_cap` are enumerated but not solved (the flagship's mega group
/// is the known DC-collapse case and takes minutes to fail; probing it tells
/// nothing new).
pub fn probe_groups_dc(
    circuit: &Circuit,
    decomp: &Decomposition,
    opts: &SolverOptions,
    size_cap: usize,
) -> Vec<GroupDcProbe> {
    use crate::newton::{dc_operating_point, Workspace};

    let absorbed: HashMap<usize, &[usize]> = decomp
        .drivers
        .iter()
        .map(|a| (a.driver_group, a.consumers.as_slice()))
        .collect();
    let group_devices = |g: usize| -> Vec<DeviceId> {
        decomp.dag.groups[g]
            .iter()
            .flat_map(|&isl| decomp.graph.islands[isl].iter().copied())
            .collect()
    };

    let mut out = Vec::new();
    for stage in &decomp.dag.stages {
        for &g in stage {
            if absorbed.contains_key(&g) {
                continue;
            }
            let mut devices = group_devices(g);
            let mut sorted_absorbed: Vec<(&usize, &&[usize])> = absorbed.iter().collect();
            sorted_absorbed.sort_by_key(|(k, _)| **k);
            for (dg, consumers) in sorted_absorbed {
                if consumers.contains(&g) {
                    devices.extend(group_devices(*dg));
                }
            }
            let (sub, _) = extract_subcircuit(circuit, &devices, &[]);
            let sample: Vec<String> = sub
                .devices
                .iter()
                .take(6)
                .map(|d| d.name().to_string())
                .collect();
            let dc_ok = if sub.devices.len() > size_cap {
                None
            } else {
                let mut ws = Workspace::new(&sub);
                Some(dc_operating_point(&mut ws, &sub, opts).is_ok())
            };
            out.push(GroupDcProbe {
                group: g,
                devices: sub.devices.len(),
                sample,
                dc_ok,
            });
        }
    }
    out
}
