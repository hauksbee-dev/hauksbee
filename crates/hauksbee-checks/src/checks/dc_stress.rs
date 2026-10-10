//! Part ratings judged at the board's DC operating point.
//!
//! The stress monitor ([`hauksbee_bind::stress`]) holds every bound part to its
//! card's ratings, but only while a co-simulation runs. A static check never
//! got that far, and on a large board the co-simulation may not either, so an
//! overstress the board sits in from the moment it powers up went unreported.
//! The Tarski inhibitory cell is the case that found it: a weight switch
//! drives a transistor's base from a 5 V rail through nothing but its own
//! channel, so about 250 mA flows through the base-emitter junction in either
//! switch state. A solved cell flagged it; `check` printed a clean verdict.
//!
//! This check binds the board, solves its DC operating point once, and runs
//! the same monitor over that state as a steady state (see
//! [`StressMonitor::evaluate_steady`]). Nothing here is a pattern: any part
//! whose card carries a rating (a junction's current, a driver pin's or switch
//! channel's current, a resistor's wattage, a package's junction temperature)
//! is judged by the physics the solver settled on. A board whose DC solve
//! fails gets one low-severity note saying the pass did not run, not silence.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.

use std::collections::{BTreeMap, HashMap};

use hauksbee_bind::binder::{bind_board, BoundBoard};
use hauksbee_bind::stress::{
    bjt_currents, strip_unit_suffix, FaultEvent, FaultKind, GateDrive, RegulatorHeat, StressMonitor,
};
use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_ir::{DeviceId, NodeId};
use hauksbee_models::ModelLibrary;
use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};

/// Bind `board`, solve its DC operating point, and report every part held
/// past a rating there, at every assumed corner ([`dc_stress_at_corners`]).
#[must_use]
pub fn dc_stress_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let bound = bind_board(board, lib);
    dc_stress_at_corners(board, lib, bound)
}

/// [`dc_stress_findings`] at every ASSUMED corner of the class-only parts.
///
/// Class-only parts are a generic LED (forward-voltage corners) or a generic
/// MOSFET (threshold corners); each is judged at every corner, not just at the
/// stand-in corner `stand_in` was bound with.
///
/// The corners move together: the k-th pass binds every corner set at its
/// k-th member (a set with fewer members stays at its last), so the extremes
/// of the board's guesses are each judged once. A part overstressed at every
/// corner is reported as the stand-in pass reports it. A part overstressed at
/// only some corners is a conditional finding, at medium severity, naming the
/// corner it holds at: whether it is real depends on the fitted part, which
/// the board does not say.
#[must_use]
pub fn dc_stress_at_corners(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    stand_in: BoundBoard,
) -> NetLintReport {
    use hauksbee_models::variants::SWEEP_PROPERTY;
    // (reference, member ids) of every undetermined corner set.
    let sets: Vec<(String, Vec<String>)> = (stand_in.report.selections.iter())
        .filter(|(_, s)| s.corners && s.determined.is_none())
        .filter_map(|(&row, s)| {
            let reference = stand_in.report.rows.get(row)?.reference.clone();
            let ids: Vec<String> = s.survivors().map(|m| m.id.clone()).collect();
            (ids.len() > 1).then_some((reference, ids))
        })
        .collect();
    let corners = sets.iter().map(|(_, ids)| ids.len()).max().unwrap_or(1);
    if corners < 2 {
        return dc_stress_findings(stand_in);
    }
    let pick = |k: usize| -> Vec<(&str, &str)> {
        (sets.iter())
            .map(|(r, ids)| (r.as_str(), ids[k.min(ids.len() - 1)].as_str()))
            .collect()
    };
    let mut passes = vec![dc_stress_findings(stand_in).findings];
    for k in 1..corners {
        let mut swept = board.clone();
        for (reference, id) in pick(k) {
            if let Some(c) = (swept.components.iter_mut()).find(|c| c.reference == reference) {
                c.properties.push((SWEEP_PROPERTY.into(), id.into()));
            }
        }
        passes.push(dc_stress_findings(bind_board(&swept, lib)).findings);
    }
    // part -> the corners it is overstressed at, and its first finding.
    let mut by_part: BTreeMap<String, (Vec<usize>, LintFinding)> = BTreeMap::new();
    let mut notes: Vec<LintFinding> = Vec::new();
    for (k, findings) in passes.into_iter().enumerate() {
        for f in findings {
            match f.refs.first() {
                // A gate under-drive already names its corner, and the swept
                // lint ([`super::engine_lint_swept`]) judges the others: only
                // the stand-in pass reports it, whole.
                _ if f.check == LintCheck::GateUnderdrive && k > 0 => {}
                Some(part)
                    if f.severity == Severity::High && f.check == LintCheck::DcOverstress =>
                {
                    let entry = by_part.entry(part.clone()).or_insert((Vec::new(), f));
                    entry.0.push(k);
                }
                _ if !notes.iter().any(|n| n.message == f.message) => notes.push(f),
                _ => {}
            }
        }
    }
    let describe = |k: usize| {
        let picked = pick(k);
        let shown: Vec<String> = (picked.iter().take(4))
            .map(|(r, id)| format!("{r} as {id}"))
            .collect();
        let more = picked.len().saturating_sub(4);
        let more = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        format!("{}{more}", shown.join(", "))
    };
    let mut findings: Vec<LintFinding> = (by_part.into_values())
        .map(|(at, mut f)| {
            if at.len() < corners {
                let held: Vec<String> = at.iter().map(|&k| describe(k)).collect();
                f.severity = Severity::Medium;
                f.message = format!(
                    "{} (CONDITIONAL: only at the assumed corner {}; the board names no part \
                     for these, so the fitted parts decide whether it holds)",
                    f.message,
                    held.join(" / ")
                );
            }
            f
        })
        .collect();
    findings.extend(notes);
    NetLintReport { findings }
}

/// The two resting states the pass judges. Combinational logic is settled
/// in both; what differs is the clocked state firmware or a bit stream sets.
const STATES: [(&str, bool); 2] = [
    ("at power-up (registers cleared)", false),
    ("with every register output set high", true),
];

/// Logic sweeps allowed for combinational outputs to settle against the
/// analog solve; a chain of gates settles one stage per sweep.
const SETTLE_SWEEPS: usize = 8;

/// One part's faults across the judged states: the state labels it tripped
/// in, the clauses describing the trip, and the nets named.
type PartFaults = (Vec<&'static str>, Vec<String>, Vec<String>);

/// [`dc_stress_lint`] over an already bound board.
#[must_use]
pub fn dc_stress_findings(mut bound: BoundBoard) -> NetLintReport {
    if bound.device_meta.is_empty() {
        return NetLintReport::default();
    }
    // part -> (state labels, clauses); one finding per physical part.
    let mut by_part: BTreeMap<String, PartFaults> = BTreeMap::new();
    let mut unsolved = Vec::new();
    let mut under: BTreeMap<String, (GateDrive, Vec<String>)> = BTreeMap::new();
    for (label, registers_high) in STATES {
        // The states run in order on the one bound board: the second starts
        // from the first's settled circuit and re-settles after its change.
        let state = &mut bound;
        if registers_high && !set_registers_high(state) {
            continue; // no clocked state: the second pass would repeat the first
        }
        let Some(x) = settle(state) else {
            unsolved.push(label);
            continue;
        };
        let (overstressed, under_driven) = judge(state, &x);
        for (drive, nets) in under_driven {
            let worst = under
                .entry(drive.reference.clone())
                .or_insert_with(|| (drive.clone(), nets));
            if drive.vgs < worst.0.vgs {
                worst.0 = drive;
            }
        }
        for (part, clauses, nets) in overstressed {
            let entry = by_part.entry(part).or_default();
            entry.0.push(label);
            if entry.1.is_empty() {
                entry.1 = clauses;
            }
            entry.2.extend(nets);
        }
    }
    let mut findings: Vec<LintFinding> = (by_part.into_iter())
        .map(|(part, (states, clauses, mut nets))| {
            nets.sort();
            nets.dedup();
            LintFinding {
                check: LintCheck::DcOverstress,
                severity: Severity::High,
                message: format!(
                    "{part} is overstressed with the board at rest, {}: {}",
                    states.join(" and "),
                    clauses.join("; ")
                ),
                refs: vec![part],
                nets,
            }
        })
        .collect();
    findings.extend(
        (under.into_values()).filter_map(|(drive, nets)| gate_underdrive(&bound, &drive, nets)),
    );
    if !unsolved.is_empty() {
        findings.push(LintFinding {
            check: LintCheck::DcOverstress,
            severity: Severity::Low,
            message: format!(
                "the board's DC operating point did not solve {}, so no part was checked \
                 against its ratings there",
                unsolved.join(" or ")
            ),
            refs: Vec::new(),
            nets: Vec::new(),
        });
    }
    NetLintReport { findings }
}

/// Load every clocked register of every logic part with ones and drive the
/// outputs from them. False when the board has no register to set.
fn set_registers_high(bound: &mut BoundBoard) -> bool {
    let BoundBoard {
        circuit, digital, ..
    } = bound;
    let mut any = false;
    for d in digital.iter_mut() {
        let Some(logic) = d.logic.as_mut() else {
            continue;
        };
        let names: Vec<String> = logic.registers().map(|(n, _)| n.to_string()).collect();
        for name in &names {
            any |= logic.set_register(name, u64::MAX);
        }
        if !names.is_empty() {
            d.drive_from_registers(circuit);
        }
    }
    any
}

/// Solve DC, let the logic parts react to it, and repeat until no driven
/// output moves. Returns the solution vector of the last solve.
fn settle(bound: &mut BoundBoard) -> Option<(Workspace, Vec<f64>)> {
    let drive_state = |b: &BoundBoard| -> Vec<(bool, u64)> {
        (b.digital.iter())
            .flat_map(|d| d.drivers.values())
            .map(|drv| {
                let v = match b.circuit.devices.get(drv.vsource.0 as usize) {
                    Some(hauksbee_ir::Device::Vsource {
                        kind: hauksbee_ir::SourceKind::Dc(v),
                        ..
                    }) => v.to_bits(),
                    _ => 0,
                };
                (drv.enabled, v)
            })
            .collect()
    };
    let mut ws = Workspace::new(&bound.circuit);
    for _ in 0..SETTLE_SWEEPS {
        dc_operating_point(&mut ws, &bound.circuit, &SolverOptions::default()).ok()?;
        let before = drive_state(bound);
        let x = ws.x.clone();
        let layout = &ws.layout;
        let node_v = |n: NodeId| {
            layout
                .node(n)
                .and_then(|i| x.get(i).copied())
                .unwrap_or(0.0)
        };
        let BoundBoard {
            circuit, digital, ..
        } = &mut *bound;
        for d in digital.iter_mut() {
            d.tick(circuit, &node_v);
        }
        if drive_state(bound) == before {
            return Some((ws, x));
        }
    }
    // Still moving (an oscillator, say): judge the last solved state anyway.
    dc_operating_point(&mut ws, &bound.circuit, &SolverOptions::default()).ok()?;
    let x = ws.x.clone();
    Some((ws, x))
}

/// The finding for a class-only MOSFET (bound to an assumed corner set) that
/// conducts with its gate below the class's maximum threshold: the corner
/// bound here turns on, a part nearer another corner is not guaranteed to.
/// Limited to corner sets on purpose: a named part's card is its own, and one
/// biased near threshold (a linear stage) is the designer's choice, already
/// disclosed as a gate-drive caveat on the co-sim run.
fn gate_underdrive(
    bound: &BoundBoard,
    drive: &GateDrive,
    nets: Vec<String>,
) -> Option<LintFinding> {
    let th = drive.th.filter(|_| drive.below_threshold())?;
    let reference = strip_unit_suffix(&drive.reference);
    let report = &bound.report;
    let corners = (report.selections.iter())
        .find(|(&i, s)| s.corners && report.rows.get(i).is_some_and(|r| r.reference == reference))
        .map(|(_, s)| s)?;
    let at = corners.bound()?;
    let others: Vec<&str> = (corners.members.iter())
        .map(|m| m.id.as_str())
        .filter(|id| *id != at)
        .collect();
    let (vgs, spec) = (drive.vgs, drive.spec);
    Some(LintFinding {
        check: LintCheck::GateUnderdrive,
        severity: Severity::High,
        message: format!(
            "{reference} has no part number, so it is bounded by assumed corners; at the corner \
             {at} it conducts with |VGS| = {vgs:.2} V, below the {th:.2} V maximum VGS(th) of \
             its class, so a part nearer {} is not guaranteed to turn on: put the MPN on \
             {reference} to remove this assumption, or drive its gate to at least {spec:.2} V",
            others.join(" or ")
        ),
        refs: vec![reference.to_string()],
        nets,
    })
}

/// What one settled state shows: the parts held past a rating there.
type Judged = (
    Vec<(String, Vec<String>, Vec<String>)>,
    Vec<(GateDrive, Vec<String>)>,
);

/// Each part the monitor saw conducting under-driven, with its gate net.
fn under_driven_gates(
    bound: &BoundBoard,
    monitor: &StressMonitor,
) -> Vec<(GateDrive, Vec<String>)> {
    let gate_net = |reference: &str| {
        let meta = bound
            .device_meta
            .iter()
            .find(|m| m.reference == reference)?;
        match bound.circuit.devices.get(meta.device.0 as usize) {
            Some(hauksbee_ir::Device::Mosfet { g, .. }) => (bound.net_nodes.iter())
                .find(|(_, node)| *node == g)
                .map(|(name, _)| name.clone()),
            _ => None,
        }
    };
    (monitor.under_driven().into_iter())
        .map(|d| {
            let nets = gate_net(&d.reference).into_iter().collect();
            (d, nets)
        })
        .collect()
}

/// Run the stress monitor over one settled state: `(part, clauses, nets)` for
/// every part that trips a rating, and every MOSFET conducting under-driven
/// with its gate net.
fn judge(bound: &mut BoundBoard, solved: &(Workspace, Vec<f64>)) -> Judged {
    let (ws, x) = solved;
    let layout = &ws.layout;
    let node_v = |n: NodeId| -> f64 {
        layout
            .node(n)
            .and_then(|i| x.get(i).copied())
            .unwrap_or(0.0)
    };
    let branch =
        |id: DeviceId| -> Option<f64> { layout.branch(id).and_then(|i| x.get(i).copied()) };

    let mut monitor = StressMonitor::new(bound.device_meta.clone());
    for r in &bound.report.regulators {
        monitor.watch_regulator_heat(r.source, RegulatorHeat::Linear { input: r.input });
    }
    for &(source, input) in &bound.report.linear_heat {
        monitor.watch_regulator_heat(source, RegulatorHeat::Linear { input });
    }
    for &(source, input, port) in &bound.report.port_heat {
        monitor.watch_regulator_heat(source, RegulatorHeat::LinearWithPort { input, port });
    }
    for (source, heat) in bound
        .behavioral
        .iter()
        .filter_map(hauksbee_bind::BehavioralDevice::converter_heat)
    {
        monitor.watch_regulator_heat(source, heat);
    }
    let faults = monitor.evaluate_steady(&mut bound.circuit, &node_v, &branch);
    let under = under_driven_gates(bound, &monitor);

    let device_of: HashMap<&str, DeviceId> = (bound.device_meta.iter())
        .map(|m| (m.reference.as_str(), m.device))
        .collect();
    let net_of: HashMap<NodeId, &str> = (bound.net_nodes.iter())
        .map(|(name, node)| (*node, name.as_str()))
        .collect();
    let mut by_part: BTreeMap<String, Vec<&FaultEvent>> = BTreeMap::new();
    for f in &faults {
        let part = strip_unit_suffix(f.component.split(':').next().unwrap_or(&f.component));
        by_part.entry(part.to_string()).or_default().push(f);
    }
    // One clause per kind of rating per part: a junction over both its
    // continuous and its surge current, or a package pool echoed onto each
    // transistor it holds, is one fact. Keep the worst-ratio event, and on a
    // tie the device actually carrying current, so a pooled package's heat is
    // named on the transistor making it.
    let bjt = |f: &FaultEvent| {
        let id = device_of.get(f.component.as_str()).copied()?;
        let (ic, ib) = bjt_currents(&bound.circuit, id, &node_v)?;
        Some((id, ic, ib))
    };
    let overstressed = by_part
        .into_iter()
        .map(|(part, events)| {
            let mut worst: BTreeMap<u8, (&FaultEvent, f64, f64)> = BTreeMap::new();
            for f in events {
                let ratio = if f.limit.abs() > 0.0 {
                    (f.value / f.limit).abs()
                } else {
                    f64::INFINITY
                };
                let carried = bjt(f).map_or(0.0, |(_, ic, ib)| ic.max(ib));
                let slot = worst
                    .entry(kind_class(f.kind))
                    .or_insert((f, ratio, carried));
                if (ratio, carried) > (slot.1, slot.2) {
                    *slot = (f, ratio, carried);
                }
            }
            let mut nets = Vec::new();
            let clauses = worst
                .into_values()
                .map(|(f, ..)| {
                    let detail = bjt(f)
                        .filter(|_| kind_class(f.kind) == 0)
                        .map(|(id, ic, ib)| {
                            if let Some(hauksbee_ir::Device::Bjt { b, .. }) =
                                bound.circuit.devices.get(id.0 as usize)
                            {
                                nets.extend(net_of.get(b).map(|n| (*n).to_string()));
                            }
                            format!(" (base {}, collector {})", amps(ib), amps(ic))
                        })
                        .unwrap_or_default();
                    format!(
                        "{} {} {}{detail}, over its {} rating",
                        f.component,
                        verb(f.kind),
                        quantity(f.kind, f.value),
                        quantity(f.kind, f.limit),
                    )
                })
                .collect();
            (part, clauses, nets)
        })
        .collect();
    (overstressed, under)
}

/// The rating a fault is judged against, coarsely: continuous and surge
/// current are one clause (current first, as the cause), then power,
/// temperature and voltage.
const fn kind_class(kind: FaultKind) -> u8 {
    match kind {
        FaultKind::Overcurrent | FaultKind::SurgeCurrent | FaultKind::PinOvercurrent => 0,
        FaultKind::Overpower => 1,
        FaultKind::Overtemperature => 2,
        FaultKind::Overvoltage | FaultKind::ReverseBias => 3,
        FaultKind::Short => 4,
    }
}

const fn verb(kind: FaultKind) -> &'static str {
    match kind {
        FaultKind::Overcurrent | FaultKind::SurgeCurrent => "carries",
        FaultKind::PinOvercurrent => "passes",
        FaultKind::Overpower => "dissipates",
        FaultKind::Overvoltage | FaultKind::ReverseBias => "sees",
        FaultKind::Overtemperature => "reaches a junction temperature of",
        FaultKind::Short => "is shorted at",
    }
}

fn amps(a: f64) -> String {
    if a.abs() >= 1.0 {
        format!("{a:.2} A")
    } else {
        format!("{:.1} mA", a * 1e3)
    }
}

fn quantity(kind: FaultKind, v: f64) -> String {
    match kind {
        FaultKind::Overcurrent | FaultKind::SurgeCurrent | FaultKind::PinOvercurrent => amps(v),
        FaultKind::Overpower => format!("{:.0} mW", v * 1e3),
        FaultKind::Overvoltage | FaultKind::ReverseBias | FaultKind::Short => format!("{v:.2} V"),
        FaultKind::Overtemperature => format!("{v:.0} C"),
    }
}
