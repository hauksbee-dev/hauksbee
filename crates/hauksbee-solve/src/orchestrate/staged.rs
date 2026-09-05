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
//! The capture grid is the solver's own accepted-step grid: every accepted
//! step of the upstream solve becomes a PWL breakpoint. Under fixed step
//! control, the only mode this executor accepts, that grid is exactly the
//! uniform `dt` grid, so every replay breakpoint lands exactly on a downstream
//! solve point and interpolation error at solve points is zero. Downstream
//! engines interpolate linearly between breakpoints, which is the same
//! first-order-hold assumption their integrators already make between steps.
//! The certificate's [`ToleranceClaim::CaptureGrid`] carries the breakpoint
//! spacing actually used. This executor refuses adaptive step control rather
//! than silently choosing a grid whose error it cannot state.
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
//! ([`Partition::analyze_imposing_tears`]) and marched by the partitioned
//! engine, whose outer loop is [`super::balance::settle_rails`]. When the torn
//! engine declines to construct OR fails while marching (a per-block Newton
//! death the build could not foresee; the per-block path has none of the
//! monolithic engine's escalation ladder), the group falls back to the
//! whole-group monolithic solve, which is exact and merely forfeits the
//! speedup; [`StagedResult::torn_groups`] says which path each torn group took,
//! so a performance regression is visible instead of silent.
//!
//! Replay pins compose safely with imposed tears. A pin adds a pinned node the
//! full-circuit strand guard never saw, which looks like it could strand a
//! rail device on the extracted sub-circuit; it cannot. The strand condition
//! tests conduction terminals only, and a conduction terminal on a replayed
//! node is a contradiction: conducting that node would have fused this group
//! with its upstream during conduction analysis, so the tear (and therefore
//! the pin) would not exist.
//!
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/orchestrate.md

use std::collections::{BTreeMap, HashMap};

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, PwlPoint, SourceKind};

use crate::decompose::rails::BalanceTearCandidate;
use crate::decompose::verify::{
    Decomposition, Evidence, RefusedAnalysis, TearKind, TearRecord, ToleranceClaim,
};
use crate::options::{DcInit, Partitioning, SolverOptions, StepControl, Strategy};
use crate::orchestrate::capture::{
    execute_composed_group, execute_stiff_group, BoundaryKind, ComposedPolicy, StiffOutcome,
};
use crate::partition::{Partition, RailTear};
use crate::partitioned::PartitionedTransient;
use crate::transient::{Transient, Waveforms};
use crate::{SolveError, SolveResult};

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
/// Per-island ladder selection: trim the CALLER'S
/// ladder down to what this group's sub-circuit could ever use. The caller's
/// grants are the ceiling (an island is never escalated past what was
/// authorized), and every trim below is justified by STRUCTURAL impossibility
/// of the strategy firing, so a trimmed solve is bit-identical to an
/// untrimmed one; heuristic trims ("probably won't need it") are refused on
/// principle, because a strategy that CAN fire changes results when removed.
///
/// Strategies deliberately NOT trimmed, and why the structural bar fails:
/// - TransientDyn: arming is unconditional at march start (branch series
///   regularizers change the stamps of every branch-bearing island), so it
///   always "fires" when granted.
/// - DynamicPivot / DynamicPivotEveryStep: a permission consulted only when
///   the frozen-order LU fails. On a fully-linear island an EXACTLY singular
///   static matrix fails both orderings identically (rank deficiency is
///   order-independent), but the threshold row-pivoting can reject a
///   near-singular pivot under one ordering and accept it under another, so
///   "frozen fails => dynamic fails" is not provable and the trim is refused.
/// - Ptc / ResidualAccept: the staged-DC rescue rungs are meaningful on any
///   island that reaches the staged fallback, regardless of device mix.
fn select_group_ladder(sub: &Circuit, caller: &SolverOptions) -> SolverOptions {
    let mut opts = *caller;
    let has_discrete = sub
        .devices
        .iter()
        .any(|d| matches!(d, Device::Comparator { .. } | Device::VSwitch { .. }));
    // EventFreeze's ONLY consult site (the staged-DC event loop,
    // newton::staged_event_solve) opens by evaluating the comparator and
    // switch decision sets and returns None when both are empty, i.e. on
    // exactly this island class the granted path IS the ungranted path, by
    // the function's own first guard. (The transient event retry is armed by
    // TransientDyn, not by this grant, and has the same empty-state guard.)
    if !has_discrete {
        opts.ladder = opts.ladder.without(Strategy::EventFreeze);
    }
    // The per-step Armijo line search lives behind `ws.linear`'s early return
    // in newton_solve: a fully-linear island is solved exactly by one
    // backsolve and RETURNS before the line-search block exists, so the grant
    // is structurally unreachable. (Uses the SAME predicate Workspace::new
    // uses to set ws.linear, so the two cannot disagree.)
    if sub.devices.iter().all(|d| d.is_linear()) {
        opts.ladder = opts.ladder.without(Strategy::LineSearch);
    }
    opts
}

pub fn run_staged(
    circuit: &Circuit,
    decomp: &Decomposition,
    opts: &SolverOptions,
    tstop: f64,
) -> SolveResult<StagedResult> {
    if !decomp.certificate.sound() {
        return Err(SolveError::refused(format!(
            "staged execution refused: the decomposition is unsound\n{}",
            decomp.certificate.summary(circuit)
        )));
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
        return Err(SolveError::refused(format!(
            "staged execution refused: exogenous boundaries [{}] are certified as \
             environment-driven, and this executor cannot drive them yet; run co-simulated \
             or monolithic",
            names.join(", ")
        )));
    }
    let dt = match opts.step {
        StepControl::Fixed { dt } => dt,
        _ => {
            return Err(SolveError::refused(
                "staged execution requires fixed step control: the capture grid is the step \
                 grid, and an adaptive run has no grid whose error the certificate could state",
            ))
        }
    };

    // Sub-circuits are solved with the reference monolithic engine: Off is
    // bit-identical to the classic solver, so each group's answer carries no
    // partitioning caveats of its own.
    let mut sub_opts = *opts;
    // Per-group ladder observability: each group's fired strategies
    // are drained into this union and re-noted at the end of the run, so the
    // per-group windows are invisible to an outer diagnostics observer.
    let mut fired_union: Vec<Strategy> = Vec::new();
    let dbg_groups = std::env::var("HAUKSBEE_CAPTURE_DEBUG").is_ok();
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
                    SolveError::internal(format!(
                        "stage ordering broke: tear node {} needed by group {} was never captured",
                        circuit.node_name(t.node),
                        g
                    ))
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

            // Round-2 ladder selection: this group's grants, trimmed from the
            // caller's ceiling by what the sub-circuit could ever use.
            let group_opts = select_group_ladder(&sub, &sub_opts);
            // Per-group activation window (drained after the group's solves;
            // the union is re-noted at the end of the run so an outer
            // observer's window sees exactly what it would have seen).
            fired_union.extend(crate::diagnostics::take_strategy_activations());

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
            // supply-integrity refusal: a pinned node cannot sag, so questions
            // about its loading have had their physics removed.
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
                    &ComposedPolicy::default(),
                    &group_opts,
                    tstop,
                    &mut refusals,
                )? {
                    Some(exec) => {
                        // Every composed boundary joins the supply-integrity
                        // refusal (balanced, held, and signal alike: all are
                        // pinned or re-grouped nodes whose loading questions
                        // beyond the record's claim must be refused).
                        let mut refused_nodes = Vec::new();
                        for o in &exec.outcomes {
                            let gnode = NodeId(l2g[&o.node.0]);
                            certificate_stiff.push(composed_tear_record(o, gnode));
                            refused_nodes.push(gnode);
                            stiff_outcomes.push((
                                g,
                                StiffOutcome {
                                    node: gnode,
                                    ..o.clone()
                                },
                            ));
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
                match execute_stiff_group(&sub, &signal_local, &group_opts, tstop, &mut refusals)? {
                    Some(exec) => {
                        // Certificate: one measured record per boundary, and
                        // the pinned nodes join the supply-integrity refusal
                        // (questions about their loading beyond sag_v must be
                        // refused: pinning removed the sag they ask about).
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
                            stiff_outcomes.push((
                                g,
                                StiffOutcome {
                                    node: gnode,
                                    ..o.clone()
                                },
                            ));
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
                None => solve_group(&sub, imposed, &group_opts, tstop).map_err(|e| {
                    // Name the group: a half-hour flagship run whose error
                    // says only "group 3" costs another half-hour run to
                    // learn what group 3 is. Devices and a name sample turn
                    // the next failure into a standalone fixture.
                    let sample: Vec<&str> = sub.devices.iter().take(6).map(|d| d.name()).collect();
                    let stiff_note = if stiff_refusal_note.is_empty() {
                        String::new()
                    } else {
                        format!(" [stiff relaxation refused first: {stiff_refusal_note}]")
                    };
                    let message = format!(
                        "staged group {g} failed ({} devices; sample: {}){stiff_note}: {e}",
                        sub.devices.len(),
                        sample.join(", ")
                    );
                    e.with_message(message)
                })?,
            };
            let fired = crate::diagnostics::take_strategy_activations();
            if dbg_groups {
                eprintln!(
                    "  group {g} ({} devices): ladder granted {:?} fired {:?}",
                    sub.devices.len(),
                    group_opts.ladder.steps().collect::<Vec<_>>(),
                    fired,
                );
            }
            fired_union.extend(fired);
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
                        SolveError::internal(format!(
                            "group {g} owns tear node {} but its sub-circuit never mapped it",
                            circuit.node_name(t.node)
                        ))
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
        let Some(isl) = decomp.graph.node_island.get(node).copied().flatten() else {
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

    // Restore the run-level activation window for outer observers.
    for st in fired_union {
        crate::diagnostics::note(st);
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
) -> SolveResult<(Waveforms, bool, bool)> {
    if !imposed.is_empty() {
        let part = Partition::analyze_imposing_tears(sub, imposed);
        if let Some(mut engine) = PartitionedTransient::try_build_from_partition(sub, opts, part) {
            if let Ok(wf) = super::collect_waveforms(&mut engine, sub, tstop) {
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
        Err(e) if opts.dc_init == DcInit::Solve && e.is_dc_failure() => {
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
            let mut errors = e.to_string();
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
            Err(e.with_message(errors))
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

/// The durable certificate record for one composed outcome, keyed on the
/// STRUCTURED [`BoundaryKind`], never on the prose note (the note once
/// mislabeled a feed-held rail balance-exact; review finding):
///
/// * `BalancedRail`: the exact per-step scalar KCL closed it, so
///   `Balance`/`BalanceEquation`/`RoundOff` is a true claim.
/// * `HeldRail`: the rail was PINNED at its feed voltage; nothing ran and
///   nothing was measured, so the record says `Stiff`/`AssumedFeedHold`/
///   `Unmeasured`, an assumption on the supply leg's stiffness, not a proof.
/// * `Signal`: the relaxation's measured sag, as the stiff executor records.
fn composed_tear_record(o: &StiffOutcome, node: NodeId) -> TearRecord {
    match o.kind {
        BoundaryKind::BalancedRail => TearRecord {
            node,
            kind: TearKind::Balance,
            evidence: Evidence::BalanceEquation,
            tolerance: ToleranceClaim::RoundOff,
            upstream: None,
            downstream: None,
        },
        BoundaryKind::HeldRail => TearRecord {
            node,
            kind: TearKind::Stiff,
            evidence: Evidence::AssumedFeedHold,
            tolerance: ToleranceClaim::Unmeasured,
            upstream: None,
            downstream: None,
        },
        BoundaryKind::Signal => TearRecord {
            node,
            kind: TearKind::Stiff,
            evidence: Evidence::MeasuredStiffness {
                sag_v: o.sag_v,
                tol_v: o.tol_v,
            },
            tolerance: ToleranceClaim::Stiffness { sag_v: o.sag_v },
            upstream: None,
            downstream: None,
        },
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decompose::rails::TearMotive;
    use crate::options::RobustnessLadder;
    use crate::test_fixtures::{
        assert_matches_within_grid, cap, comparator, diode, fixed_opts, max_error, monolith,
        pnp_blocks, res, shunt_array, sw, swing, vdc, vpulse, GND,
    };

    /// The two structurally-justified ladder trims fire on the right island
    /// classes, everything else passes through, and an empty ladder stays empty.
    #[test]
    fn ladder_selection_trims_only_structurally_dead_grants() {
        let full = SolverOptions {
            ladder: RobustnessLadder::full(),
            ..Default::default()
        };
        let mut lin = Circuit::new();
        let (a, b) = (lin.node("a"), lin.node("b"));
        vdc(&mut lin, "V", a, 5.0);
        res(&mut lin, "R1", a, b, 1e3);
        res(&mut lin, "R2", b, GND, 1e3);
        let sel = select_group_ladder(&lin, &full);
        assert!(!sel.ladder.has(Strategy::LineSearch));
        assert!(!sel.ladder.has(Strategy::EventFreeze));
        for s in [
            Strategy::DynamicPivot,
            Strategy::DynamicPivotEveryStep,
            Strategy::Ptc,
            Strategy::ResidualAccept,
            Strategy::TransientDyn,
        ] {
            assert!(sel.ladder.has(s), "{s:?}");
        }

        let mut dio = lin.clone();
        let c = dio.node("c");
        diode(&mut dio, "D", b, c, Default::default());
        res(&mut dio, "R3", c, GND, 1e3);
        let sel = select_group_ladder(&dio, &full);
        assert!(sel.ladder.has(Strategy::LineSearch));
        assert!(!sel.ladder.has(Strategy::EventFreeze));

        let mut cmp = dio.clone();
        let o = cmp.node("o");
        comparator(&mut cmp, "K", o, b, c, 0.1);
        let sel = select_group_ladder(&cmp, &full);
        assert!(sel.ladder.has(Strategy::EventFreeze) && sel.ladder.has(Strategy::LineSearch));

        assert_eq!(
            select_group_ladder(&lin, &SolverOptions::default())
                .ladder
                .steps()
                .count(),
            0
        );
    }

    fn worst_error(c: &Circuit, staged: &StagedResult, mono: &Waveforms) -> f64 {
        max_error(
            c,
            &staged.waveforms.time,
            &staged.waveforms.node_voltages,
            mono,
        )
        .0
    }

    /// A pulsed RC stack (absorbed as a driver), a comparator island conducting
    /// its own output, and a switch island sensing the comparator: one replayed
    /// tear, one absorption, three groups across two stages. Returns the switch
    /// output node.
    fn feedforward_board() -> (Circuit, NodeId) {
        let mut c = Circuit::new();
        let (vin, a) = (c.node("vin"), c.node("a"));
        vpulse(&mut c, "V1", vin, (0.0, 5.0), 2e-6, (1e-6, 1e-6), 30e-6);
        res(&mut c, "R1", vin, a, 1e3);
        cap(&mut c, "C1", a, GND, 1e-9);
        let cmp_out = c.node("cmp_out");
        res(&mut c, "Rc", cmp_out, GND, 10e3);
        comparator(&mut c, "CMP", cmp_out, a, GND, 1e-3);
        let (s, o) = (c.node("s"), c.node("o"));
        vdc(&mut c, "V2", s, 3.3);
        sw(&mut c, "SW", s, o, cmp_out, (2.0, 1.0), 10.0);
        res(&mut c, "RL", o, GND, 10e3);
        (c, o)
    }

    /// The transient gate: staged matches the monolith within the capture-grid
    /// claim; with DC sources every boundary is static and it matches to
    /// round-off.
    #[test]
    fn staged_replay_matches_monolith_within_capture_grid() {
        let (c, o) = feedforward_board();
        let (dt, tstop) = (50e-9, 20e-6);
        let opts = fixed_opts(dt);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert_eq!(d.dag.stages.len(), 3, "{:?}", d.dag.stages);
        assert_eq!(d.drivers.len(), 1, "the RC stack absorbs: {:?}", d.drivers);
        let staged = run_staged(&c, &d, &opts, tstop).expect("staged run");
        let err = worst_error(&c, &staged, &monolith(&c, &opts, tstop));
        assert!(err <= 1e-6, "staged diverged from monolith: {err:.3e}");
        assert!(
            staged.waveforms.node_voltages[o.0 as usize].last().unwrap() > &3.0,
            "switch never closed"
        );
        for r in &staged.certificate.records {
            if r.kind == TearKind::Free {
                assert_eq!(
                    r.tolerance,
                    ToleranceClaim::CaptureGrid { dt: Some(dt) },
                    "{r:?}"
                );
            }
        }

        let (mut c, _) = feedforward_board();
        if let Device::Vsource { kind, .. } = &mut c.devices[0] {
            *kind = SourceKind::Dc(5.0);
        }
        let (dt, tstop) = (100e-9, 5e-6);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged run");
        let err = worst_error(&c, &staged, &monolith(&c, &fixed_opts(dt), tstop));
        assert!(
            err <= 1e-9,
            "DC boundaries must be round-off exact: {err:.3e}"
        );
    }

    /// An unsound decomposition (floating sense net) and adaptive step control
    /// are both refused rather than faked.
    #[test]
    fn unsound_or_adaptive_runs_are_refused() {
        let mut c = Circuit::new();
        let (x, y) = (c.node("x"), c.node("y"));
        vdc(&mut c, "V", x, 1.0);
        let sel = c.node("sel_floating");
        sw(&mut c, "S", x, y, sel, (2.0, 1.0), 10.0);
        res(&mut c, "R", y, GND, 1e3);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert!(run_staged(&c, &d, &fixed_opts(1e-7), 1e-6).is_err());

        let (c2, _) = feedforward_board();
        let d2 = Decomposition::analyze(&c2, TearMotive::Profit);
        assert!(run_staged(&c2, &d2, &SolverOptions::default(), 1e-6).is_err());
    }

    /// A shunt-fed PNP array (balance tear) whose block-0 collector a
    /// comparator senses (free tear), gating a switch island (second free
    /// tear): the array group runs TORN and the pipeline matches the monolith.
    #[test]
    fn balance_torn_group_matches_monolith() {
        let (mut c, rail, _, _, blocks) = shunt_array(24, 1e3);
        let cmp_out = c.node("cmp_out");
        res(&mut c, "Rcmp", cmp_out, GND, 10e3);
        comparator(&mut c, "CMP", cmp_out, blocks[0].1, GND, 1e-3);
        let (s, o) = (c.node("s"), c.node("o"));
        vdc(&mut c, "V2", s, 3.3);
        sw(&mut c, "SW", s, o, cmp_out, (2.0, 1.0), 10.0);
        res(&mut c, "RL", o, GND, 10e3);

        let (dt, tstop) = (100e-9, 5e-6);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        let accepted: Vec<_> = d.balance_tears.iter().filter(|t| t.torn()).collect();
        assert_eq!(accepted.len(), 1, "{:?}", d.balance_tears);
        assert_eq!(accepted[0].rail, rail);
        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged");
        assert_eq!(staged.torn_groups.len(), 1, "{:?}", staged.torn_groups);
        let err = worst_error(&c, &staged, &monolith(&c, &fixed_opts(dt), tstop));
        assert!(err <= 1e-6, "torn staged diverged from monolith: {err:.3e}");
        assert!(
            staged.waveforms.final_node(&c, "o").unwrap() > 3.0,
            "switch never closed"
        );
    }

    /// SRC -> 500R -> MID (two PNP loads) -> 1k -> INNER (24-block array): both
    /// rails tear, and the inter-rail shunt term is carried EXACTLY.
    #[test]
    fn cascaded_rails_match_monolith() {
        let mut c = Circuit::new();
        let src = c.node("+5V_SRC");
        vdc(&mut c, "VS", src, 5.0);
        let (mid, inner) = (c.node("MID"), c.node("INNER"));
        res(&mut c, "R1", src, mid, 500.0);
        res(&mut c, "R2", mid, inner, 1e3);
        pnp_blocks(&mut c, "m", mid, 2, GND, 100e3, 100e3);
        pnp_blocks(&mut c, "i", inner, 24, GND, 100e3, 100e3);

        let (dt, tstop) = (100e-9, 5e-6);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        let accepted: Vec<_> = d.balance_tears.iter().filter(|t| t.torn()).collect();
        assert_eq!(accepted.len(), 2, "{:?}", d.balance_tears);
        assert!(accepted.iter().any(|t| t.rail == mid) && accepted.iter().any(|t| t.rail == inner));
        let opts = fixed_opts(dt);
        let staged = run_staged(&c, &d, &opts, tstop).expect("staged");
        assert!(!staged.torn_groups.is_empty());
        let mono = monolith(&c, &opts, tstop);
        // Per node, the MUTUAL Newton stopping slack: each engine stops within
        // one step tolerance of the root along a different iteration path.
        assert_matches_within_grid(
            &c,
            &staged.waveforms.time,
            &staged.waveforms.node_voltages,
            &mono,
            dt,
            &|mv| 2.0 * (opts.reltol * mv.abs() + opts.vntol),
            "cascaded torn group",
        );
        // DC-settled: the final sample equals the monolith to round-off.
        let last = staged.waveforms.time.len() - 1;
        for node in 1..c.node_count() {
            let sv = staged.waveforms.node_voltages[node][last];
            let mv = lerp_at(
                &mono.time,
                &mono.node_voltages[node],
                staged.waveforms.time[last],
            );
            assert!(
                (sv - mv).abs() <= 1e-9,
                "node {}: {sv:.12} vs {mv:.12}",
                c.node_name(NodeId(node as u32))
            );
        }
        let vmid = staged.waveforms.final_node(&c, "MID").unwrap();
        let vinner = staged.waveforms.final_node(&c, "INNER").unwrap();
        assert!(
            vmid < 4.99 && vinner < vmid - 0.1,
            "MID {vmid} INNER {vinner}"
        );
    }

    /// A shunt-fed 24-block array with block 0's base driven by `drive`
    /// (a pulsed source directly, or a node whose return path is gated).
    fn pulsed_array(block0_base_to: impl FnOnce(&mut Circuit, NodeId)) -> (Circuit, NodeId) {
        let mut c = Circuit::new();
        let p5 = c.node("+5V");
        vdc(&mut c, "V5", p5, 5.0);
        let rail = c.node("ANALOG_VDD");
        res(&mut c, "R_shunt", p5, rail, 1e3);
        let blocks = pnp_blocks(&mut c, "", rail, 24, GND, 100e3, 10e3);
        // Re-route block 0's base return through the caller's network.
        let rb0 = c
            .devices
            .iter()
            .position(|d| d.name() == "Rb0")
            .expect("Rb0");
        c.devices.remove(rb0);
        block0_base_to(&mut c, blocks[0].0);
        (c, rail)
    }

    /// A decoupling cap on a balance-torn rail rides in a boundary-only island
    /// whose current enters the balance books; a pulsed base drive keeps the
    /// cap's current live.
    #[test]
    fn bypass_cap_on_torn_rail_matches_monolith() {
        let (mut c, rail) = pulsed_array(|c, base| {
            let drv = c.node("b0drv");
            vpulse(c, "VB0", drv, (0.0, 3.0), 1.05e-6, (0.5e-6, 0.5e-6), 2e-6);
            res(c, "Rb0", base, drv, 100e3);
        });
        cap(&mut c, "Cbypass", rail, GND, 100e-9);

        let (dt, tstop) = (100e-9, 6e-6);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert!(
            d.balance_tears.iter().any(|t| t.torn()),
            "{:?}",
            d.balance_tears
        );
        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged");
        assert!(!staged.torn_groups.is_empty());
        let mono = monolith(&c, &fixed_opts(dt), tstop);
        // 5e-6: the update-only Newton acceptance spread at a junction knee,
        // present in both formulations; a persistent offset still fails.
        assert_matches_within_grid(
            &c,
            &staged.waveforms.time,
            &staged.waveforms.node_voltages,
            &mono,
            dt,
            &|_| 5e-6,
            "bypass cap current leaked from the balance books",
        );
        assert!(
            swing(staged.waveforms.node(&c, "ANALOG_VDD").unwrap()) > 1e-3,
            "rail never moved"
        );
    }

    /// A group with BOTH an imposed rail tear AND an inbound replay pin: an
    /// upstream pulsed comparator gates a switch in block 0's base return, so
    /// the balance loop tracks a load change arriving through the replay.
    #[test]
    fn torn_group_with_replay_pin_matches_monolith() {
        let (c, rail) = pulsed_array(|c, base| {
            let vin = c.node("vin");
            vpulse(c, "VP", vin, (0.0, 5.0), 1e-6, (0.5e-6, 0.5e-6), 10e-6);
            let cmp_out = c.node("cmp_out");
            res(c, "Rcmp", cmp_out, GND, 10e3);
            comparator(c, "CMP", cmp_out, vin, GND, 1e-3);
            let ret = c.node("b0_ret");
            res(c, "Rb0", base, ret, 100e3);
            sw(c, "SW0", ret, GND, cmp_out, (2.0, 1.0), 10.0);
        });

        let (dt, tstop) = (100e-9, 6e-6);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert!(
            d.balance_tears.iter().any(|t| t.torn() && t.rail == rail),
            "{:?}",
            d.balance_tears
        );
        assert!(!d.dag.free_tears.is_empty(), "{:?}", d.dag.free_tears);
        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged");
        assert!(!staged.torn_groups.is_empty(), "{:?}", staged.torn_groups);
        let mono = monolith(&c, &fixed_opts(dt), tstop);
        assert_matches_within_grid(
            &c,
            &staged.waveforms.time,
            &staged.waveforms.node_voltages,
            &mono,
            dt,
            &|_| 1e-6,
            "replay-pinned torn group",
        );
        assert!(
            swing(staged.waveforms.node(&c, "c0").unwrap()) > 0.05,
            "block 0 never responded"
        );
    }

    /// Analysis nominates cut nodes on a chain of BJT blocks, run_staged runs
    /// the measured relaxation, the certificate carries Stiff records, and the
    /// result matches the monolith within the certificate's numbers.
    #[test]
    fn stiff_cuts_flow_through_run_staged() {
        use crate::decompose::stiff::StiffPolicy;
        let mut c = Circuit::new();
        let vs = c.node("vs");
        vpulse(&mut c, "VS", vs, (3.0, 5.0), 1e-6, (0.5e-6, 0.5e-6), 1.5e-6);
        let model = crate::test_fixtures::pnp();
        let mut prev = vs;
        for k in 0..3 {
            let joint = c.node(&format!("j{k}"));
            res(&mut c, &format!("Rj{k}"), prev, joint, 100.0);
            let b = c.node(&format!("b{k}"));
            crate::test_fixtures::bjt(&mut c, &format!("Q{k}"), GND, b, joint, &model);
            res(&mut c, &format!("Rb{k}"), b, GND, 100e3);
            prev = joint;
        }

        let (dt, tstop) = (100e-9, 4e-6);
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
            "the chain must yield stiff nominations"
        );
        let staged = run_staged(&c, &d, &opts, tstop).expect("staged");
        let accepted: Vec<_> = staged
            .stiff_outcomes
            .iter()
            .filter(|(_, o)| o.accepted)
            .collect();
        assert!(!accepted.is_empty(), "{:?}", staged.stiff_outcomes);
        let stiff_records: Vec<_> = staged
            .certificate
            .records
            .iter()
            .filter(|r| r.kind == TearKind::Stiff)
            .collect();
        assert_eq!(stiff_records.len(), accepted.len());
        for r in &stiff_records {
            assert!(
                matches!(r.tolerance, ToleranceClaim::Stiffness { sag_v } if sag_v.is_finite()),
                "{r:?}"
            );
        }
        assert!(staged
            .certificate
            .permits(crate::decompose::verify::RefusedAnalysis::SupplyIntegrityOnTornRail)
            .is_err());
        let max_sag = accepted.iter().map(|(_, o)| o.sag_v).fold(0.0f64, f64::max);
        let err = worst_error(&c, &staged, &monolith(&c, &opts, tstop));
        assert!(
            err <= (3.0 * max_sag).max(2e-6),
            "staged stiff diverged: {err:.3e} (sag {max_sag:.3e})"
        );
    }

    /// What each [`BoundaryKind`] may claim, keyed on the structured kind and
    /// never the prose note: a held rail is Stiff + AssumedFeedHold +
    /// Unmeasured even with a doctored "balanced rail" note.
    #[test]
    fn composed_records_never_overclaim_a_held_rail() {
        let outcome = |kind, sag_v, note: &'static str| StiffOutcome {
            node: NodeId(7),
            kind,
            sag_v,
            tol_v: 1e-2,
            accepted: true,
            bootstrapped: true,
            capture_growth: 0,
            note,
        };
        let n = NodeId(42);
        let balanced = composed_tear_record(
            &outcome(BoundaryKind::BalancedRail, 0.0, "balanced rail"),
            n,
        );
        assert_eq!(balanced.kind, TearKind::Balance);
        assert_eq!(balanced.evidence, Evidence::BalanceEquation);
        assert_eq!(balanced.tolerance, ToleranceClaim::RoundOff);
        assert_eq!(balanced.node, n);

        for note in ["held rail (stiff-supply feed)", "balanced rail"] {
            let held = composed_tear_record(&outcome(BoundaryKind::HeldRail, 0.0, note), n);
            assert_eq!(held.kind, TearKind::Stiff, "{note}");
            assert_eq!(held.evidence, Evidence::AssumedFeedHold);
            assert_eq!(held.tolerance, ToleranceClaim::Unmeasured);
        }

        let signal = composed_tear_record(&outcome(BoundaryKind::Signal, 3.5e-7, ""), n);
        assert_eq!(signal.kind, TearKind::Stiff);
        assert_eq!(
            signal.evidence,
            Evidence::MeasuredStiffness {
                sag_v: 3.5e-7,
                tol_v: 1e-2
            }
        );
        assert_eq!(
            signal.tolerance,
            ToleranceClaim::Stiffness { sag_v: 3.5e-7 }
        );
    }

    /// A Thevenin driver replicated into two consumers matches the monolith to
    /// round-off (no replay happens at all).
    #[test]
    fn replicated_driver_matches_to_round_off() {
        let mut c = Circuit::new();
        let (vdrv, sel) = (c.node("vdrv"), c.node("sel"));
        vdc(&mut c, "Vdrv", vdrv, 5.0);
        res(&mut c, "Rdrv", vdrv, sel, 1e3);
        for tag in ["x", "y"] {
            let s = c.node(&format!("{tag}_src"));
            let o = c.node(&format!("{tag}_out"));
            vdc(&mut c, &format!("V{tag}"), s, 3.3);
            sw(&mut c, &format!("SW{tag}"), s, o, sel, (2.0, 1.0), 1.0);
            res(&mut c, &format!("RL{tag}"), o, GND, 10e3);
        }
        let (dt, tstop) = (100e-9, 2e-6);
        let d = Decomposition::analyze(&c, TearMotive::Profit);
        assert_eq!(d.drivers.len(), 1);
        assert_eq!(d.drivers[0].consumers.len(), 2);
        let staged = run_staged(&c, &d, &fixed_opts(dt), tstop).expect("staged");
        let err = worst_error(&c, &staged, &monolith(&c, &fixed_opts(dt), tstop));
        assert!(err <= 1e-9, "absorption must be exact: {err:.3e}");
        for tag in ["x_out", "y_out"] {
            assert!(
                staged.waveforms.final_node(&c, tag).unwrap() > 3.0,
                "{tag} never energized"
            );
        }
    }
}
