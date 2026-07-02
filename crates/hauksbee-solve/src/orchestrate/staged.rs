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

use std::collections::HashMap;

use hauksbee_ir::{Circuit, Device, DeviceId, NodeId, PwlPoint, SourceKind};

use crate::decompose::rails::BalanceTearCandidate;
use crate::decompose::verify::{Decomposition, TearKind, ToleranceClaim};
use crate::options::{Partitioning, SolverOptions, StepControl};
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
    let absorbed: HashMap<usize, &[usize]> = decomp
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

            let (wf, torn) = solve_group(&sub, imposed, &sub_opts, tstop)
                .map_err(|e| format!("staged group {g} failed: {e}"))?;
            if torn {
                torn_groups.push(g);
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

    // Complete the certificate: every replayed free tear now has its grid.
    let mut certificate = decomp.certificate.clone();
    for r in &mut certificate.records {
        if r.kind == TearKind::Free {
            if let ToleranceClaim::CaptureGrid { dt: d } = &mut r.tolerance {
                *d = Some(dt);
            }
        }
    }

    Ok(StagedResult {
        waveforms,
        certificate,
        executed_groups,
        torn_groups,
    })
}

/// Solve one group's sub-circuit: torn around its imposed balance rails when
/// possible, whole otherwise. Returns the run and whether the torn engine
/// actually hosted it.
fn solve_group(
    sub: &Circuit,
    imposed: Vec<RailTear>,
    opts: &SolverOptions,
    tstop: f64,
) -> Result<(Waveforms, bool), String> {
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
                return Ok((wf, true));
            }
            // A run-time death (per-block Newton failure the build could not
            // foresee: the per-block path lacks the monolithic engine's
            // escalation ladder). The whole-group solve below is exact and
            // has that ladder; take it, discard the partial waveforms, and
            // let torn_groups show the speedup was forfeited.
        }
        // Construction declined: same fallback, same reasoning.
    }
    Ok((Transient::new(*opts).run(sub, tstop)?, false))
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
        // 1e-6 for a documented reason, found the hard way: at the junction
        // KNEE step of the pulse fall the two engines' accepted solutions
        // differ by ~2.5e-6 at b0 for ONE sample, and the difference is
        // path-dependent Newton acceptance, not bookkeeping. Evidence: flat
        // regions agree to 1e-15 (no leak); the rail balance closes to 7e-9
        // (the cap's current IS in the books, which is what this gate
        // exists to prove); tightening the balance target 10x and vntol
        // 100x changes nothing bitwise (each engine's accepted point is its
        // own machine-stable fixed point); the blocks are static, so no
        // history is involved. Root-causing the knee acceptance band
        // (junction limiting at termination) is punch-listed with the
        // Newton robustness work. A persistent offset, the failure this
        // gate hunts, would still fail at this bar.
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
