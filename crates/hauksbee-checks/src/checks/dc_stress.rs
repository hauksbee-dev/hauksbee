//! Part ratings judged at the board's DC operating point.
//!
//! The stress monitor ([`hauksbee_bind::stress`]) holds every bound part to its
//! card's ratings, but only while a co-simulation runs. A static check never
//! got that far, and on a large board the co-simulation may not either, so an
//! overstress the board sits in from the moment it powers up went unreported.
//! A switched-mirror cell on the large internal test board is the case that
//! found it: an analog switch drives a transistor's base from a supply rail
//! through nothing but its own channel, so a large current flows through the
//! base-emitter junction in either switch state. A solved cell flagged it;
//! `check` printed a clean verdict.
//!
//! This check binds the board, solves its DC operating point once, and runs
//! the same monitor over that state as a steady state (see
//! [`StressMonitor::evaluate_steady`]). Nothing here is a pattern: any part
//! whose card carries a rating (a junction's current, a driver pin's or switch
//! channel's current, a resistor's wattage, a package's junction temperature)
//! is judged by the physics the solver settled on. A board whose DC solve
//! fails, or whose named rails never come up in it, gets one low-severity note
//! saying what was not judged ([`rating_basis::not_judged`]): never silence,
//! and never a pass.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.

use std::collections::{BTreeMap, HashMap};

use hauksbee_bind::binder::{
    bind_board, held_off_rails, is_ground, power_rail_voltage, BoundBoard,
};
use hauksbee_bind::stress::{
    bjt_currents, far_past_reason, is_far_past_rating, strip_unit_suffix, FaultEvent, FaultKind,
    GateDrive, RegulatorHeat, StandInExceedance, StressMonitor,
};
use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity};
use hauksbee_ir::{DeviceId, NodeId};
use hauksbee_models::rating_basis;
use hauksbee_models::ModelLibrary;
use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};

use super::rest_supply;

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
    let qualified = super::open_paths::open_path_qualifiers(board, &stand_in);
    if corners < 2 {
        return with_qualifiers(dc_stress_findings(stand_in).findings, qualified);
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
    with_qualifiers(findings, qualified)
}

/// `findings` plus each open-path abstention for a part they do not already
/// report: a part over a rating is not news as "unverified".
fn with_qualifiers(mut findings: Vec<LintFinding>, qualified: Vec<LintFinding>) -> NetLintReport {
    let reported: Vec<String> = (findings.iter())
        .filter(|f| f.severity != Severity::Low)
        .flat_map(|f| f.refs.clone())
        .collect();
    findings.extend(
        qualified
            .into_iter()
            .filter(|q| !q.refs.iter().any(|r| reported.contains(r))),
    );
    NetLintReport { findings }
}

/// The two resting states the pass judges. Combinational logic is settled
/// in both; what differs is the clocked state firmware or a bit stream sets.
const STATES: [(&str, bool); 2] = [
    ("at power-up (registers cleared)", false),
    ("with every register output set high", true),
];

/// Each state of [`STATES`] again with the logic outputs at their strong-drive
/// corner ([`set_drive`]). A logic card's `ro` is its weak corner (minimum
/// VOH under load), right for whether a level is met and wrong for how much
/// current a typical part pushes into a low-impedance load: a 74HCT output
/// at 93 ohm weak is about 30 ohm typical, three times the current into an
/// LED with no series resistor.
const STRONG_STATES: [&str; 2] = [
    "at power-up (registers cleared) with the logic outputs at their strong-drive corner",
    "with every register output set high and the logic outputs at their strong-drive corner",
];

/// Logic sweeps allowed for combinational outputs to settle against the
/// analog solve; a chain of gates settles one stage per sweep.
const SETTLE_SWEEPS: usize = 8;

/// One part's faults across the judged states: the state labels it tripped
/// in, the clauses describing the trip, and the nets named.
type PartFaults = (Vec<&'static str>, Vec<String>, Vec<String>);

/// One part's stress faults: `(part, clauses, nets)`.
type PartFault = (String, Vec<String>, Vec<String>);

/// Record a part's fault in `label`'s state: the state, the first clauses
/// seen, and every net named.
fn record_fault(
    by_part: &mut BTreeMap<String, PartFaults>,
    label: &'static str,
    (part, clauses, nets): PartFault,
) {
    let entry = by_part.entry(part).or_default();
    entry.0.push(label);
    if entry.1.is_empty() {
        entry.1 = clauses;
    }
    entry.2.extend(nets);
}

/// The parts of a state's `found` and `strong_found` stresses that survive
/// the rails being held to what their supplies can give.
///
/// A part dropped there is over a rating only at a draw no supply gives; its
/// first clause goes to `only_ideal`. Returns whether the held re-judge did
/// not solve.
fn hold_overloaded_rails(
    state: &mut BoundBoard,
    over: &[rest_supply::Overload],
    found: &mut Vec<PartFault>,
    strong_found: &mut Vec<PartFault>,
    only_ideal: &mut BTreeMap<String, String>,
) -> bool {
    let real = held_to_capability(state, over);
    let keep = |part: &str| real.as_ref().is_some_and(|r| r.iter().any(|p| p == part));
    for (part, clauses, _) in found.iter().chain(strong_found.iter()) {
        if !keep(part.as_str()) {
            only_ideal
                .entry(part.clone())
                .or_insert_with(|| clauses.first().cloned().unwrap_or_default());
        }
    }
    found.retain(|(part, ..)| keep(part.as_str()));
    strong_found.retain(|(part, ..)| keep(part.as_str()));
    real.is_none()
}

/// [`dc_stress_lint`] over an already bound board.
#[must_use]
pub fn dc_stress_findings(mut bound: BoundBoard) -> NetLintReport {
    if bound.device_meta.is_empty() {
        return NetLintReport::default();
    }
    // part -> (state labels, clauses); one finding per physical part.
    let mut by_part: BTreeMap<String, PartFaults> = BTreeMap::new();
    let mut unsolved = Vec::new();
    let mut impossible: BTreeMap<String, String> = BTreeMap::new();
    let mut under: BTreeMap<String, (GateDrive, Vec<String>)> = BTreeMap::new();
    // part -> the stated condition for its first limit passed that was a
    // placeholder or guessed card's, not its own: never a failing finding.
    let mut stated: BTreeMap<String, String> = BTreeMap::new();
    // Rails drawn past what their supply gives, and the parts over a rating
    // only at that draw (part -> first clause): one finding, not overstresses.
    let mut overloaded: Vec<rest_supply::Overload> = Vec::new();
    let mut only_ideal: BTreeMap<String, String> = BTreeMap::new();
    let mut held_unsolved = false;
    // Named rails dead in every state solved: what the solve did not judge.
    let mut dead: Option<BTreeMap<String, (f64, f64)>> = None;
    for ((label, registers_high), strong_label) in STATES.into_iter().zip(STRONG_STATES) {
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
        let here = dead_rails(state, &x);
        dead.get_or_insert_with(|| here.clone())
            .retain(|net, _| here.contains_key(net));
        let (overstressed, under_driven, absurd, stand_in) = judge(state, &x);
        let over = rest_supply::overloads(state, &x.0, &x.1);
        note_absurd(&mut impossible, absurd);
        note_stand_in(&mut stated, stand_in);
        for (drive, nets) in under_driven {
            let worst = under
                .entry(drive.reference.clone())
                .or_insert_with(|| (drive.clone(), nets));
            if drive.vgs < worst.0.vgs {
                worst.0 = drive;
            }
        }
        let mut weak: Vec<String> = Vec::new();
        let mut found = overstressed;
        // The same state at the strong-drive corner, then back to the weak
        // one so the next state starts where it always did. A part already
        // over a rating at the weak corner is not news at the strong one, and
        // a gate under-driven at the weak corner is judged there alone.
        let mut strong_found = Vec::new();
        if set_drive(state, true) {
            if let Some(x) = settle(state) {
                let (strong_over, _, absurd, stand_in) = judge(state, &x);
                note_absurd(&mut impossible, absurd);
                note_stand_in(&mut stated, stand_in);
                strong_found = strong_over;
            }
            set_drive(state, false);
        }
        // A draw no supply gives: keep only the parts still over a rating
        // with the rails held to what their supplies can give.
        if !over.is_empty() {
            held_unsolved |=
                hold_overloaded_rails(state, &over, &mut found, &mut strong_found, &mut only_ideal);
            rest_supply::note_overloads(&mut overloaded, over);
        }
        for fault in found {
            weak.push(fault.0.clone());
            record_fault(&mut by_part, label, fault);
        }
        for fault in strong_found {
            if !weak.contains(&fault.0) {
                record_fault(&mut by_part, strong_label, fault);
            }
        }
    }
    // A part over a rating at a plausible draw in any state is reported there.
    only_ideal.retain(|part, _| !by_part.contains_key(part));
    let mut conditional = unknown_state_faults(&mut bound, &by_part);
    conditional.retain(|part, _| !only_ideal.contains_key(part));
    let reported: Vec<&String> = (by_part.keys())
        .chain(conditional.keys())
        .chain(only_ideal.keys())
        .collect();
    let stated: Vec<LintFinding> = (stated.into_iter())
        .filter(|(part, _)| !reported.contains(&part))
        .map(|(part, condition)| stand_in_condition(part, condition))
        .collect();
    let rows = (by_part.into_iter().map(|row| (row, Severity::High)))
        .chain(conditional.into_iter().map(|row| (row, Severity::Medium)));
    let mut findings: Vec<LintFinding> = rows.map(rest_finding).collect();
    findings.extend(
        (under.into_values()).filter_map(|(drive, nets)| gate_underdrive(&bound, &drive, nets)),
    );
    findings.extend(
        impossible
            .into_iter()
            .map(|(part, clause)| not_a_number_note(part, &clause)),
    );
    findings.extend(stated);
    findings.extend(rest_supply::implausible_rest_finding(
        &overloaded,
        &only_ideal,
        held_unsolved,
    ));
    findings.extend(abstained_note(&unsolved));
    let pulled: Vec<String> = overloaded.iter().map(|o| o.net.clone()).collect();
    findings.extend((dead.filter(|d| !d.is_empty())).map(|d| dead_rail_note(d, &pulled)));
    NetLintReport { findings }
}

/// The note that the check abstained in the `unsolved` states, whose DC
/// operating point did not solve; none when every state solved.
fn abstained_note(unsolved: &[&str]) -> Option<LintFinding> {
    (!unsolved.is_empty()).then(|| {
        not_judged_note(
            &format!(
                "the DC ratings check abstained {}: the board's DC operating point did not \
                 solve, so no part was checked against its ratings there",
                unsolved.join(" or ")
            ),
            Vec::new(),
        )
    })
}

/// A rail whose solved level is under this share of the level its name
/// states did not come up in the solve.
const DEAD_RAIL_SHARE: f64 = 0.5;

/// The named rails (`+5V`, `3V3`) the solve `x` left below
/// [`DEAD_RAIL_SHARE`] of their named level, each with (solved, named)
/// volts. A rail the board itself holds off ([`held_off_rails`]) is off by
/// design, not dead.
fn dead_rails(bound: &BoundBoard, solved: &(Workspace, Vec<f64>)) -> BTreeMap<String, (f64, f64)> {
    let held: Vec<String> = held_off_rails(bound).into_iter().map(|(n, _)| n).collect();
    let (ws, x) = solved;
    (bound.net_nodes.iter())
        .filter(|(name, _)| !is_ground(name) && !held.contains(*name))
        .filter_map(|(name, &node)| {
            let named = power_rail_voltage(name).filter(|v| v.abs() >= 1.0)?;
            let v = ws.layout.node(node).and_then(|i| x.get(i).copied())?;
            // NaN compares false: a non-finite level is no rail either.
            let up = v * named.signum() >= DEAD_RAIL_SHARE * named.abs();
            (!up).then(|| (name.clone(), (v, named)))
        })
        .collect()
}

/// The note naming the rails no state brought up: the parts on them were
/// held to their ratings at a dead rail, so none of them was judged. A rail in
/// `pulled` (one the board draws more from than its supply gives) was not left unpowered by anything missing: the draw
/// pulled it down, and the note says so instead of blaming an absent source.
fn dead_rail_note(dead: BTreeMap<String, (f64, f64)>, pulled: &[String]) -> LintFinding {
    let is_pulled = |net: &String| pulled.contains(net);
    let rails: Vec<String> = (dead.iter())
        .map(|(net, (v, named))| {
            let v = if v.abs() < 0.005 { 0.0 } else { *v };
            if is_pulled(net) {
                format!("'{net}' (pulled down to {v:.2} V by the draw on it; its name says {named:.2} V)")
            } else {
                format!("'{net}' (solved at {v:.2} V; its name says {named:.2} V)")
            }
        })
        .collect();
    let cause = if dead.keys().all(is_pulled) {
        "the rail was pulled down by a draw no supply gives (a near-short on it), so it \
         never sat at its level"
    } else {
        "the rail did not come up in the solved operating point (an input with no declared \
         source, or a supply the model could not start)"
    };
    not_judged_note(
        &format!(
            "the DC ratings check abstained on the parts powered from {}: {cause}, so no part \
             on it was checked against its ratings at that level",
            rails.join(", ")
        ),
        dead.into_keys().collect(),
    )
}

/// The low-severity note that the pass did not run to a result for `what`.
fn not_judged_note(what: &str, nets: Vec<String>) -> LintFinding {
    LintFinding {
        check: LintCheck::DcOverstress,
        severity: Severity::Low,
        message: rating_basis::not_judged(what),
        refs: Vec::new(),
        nets,
    }
}

/// The finding for one part overstressed at rest (`High`) or only in the state
/// the board does not show (`Medium`, conditional).
fn rest_finding(
    ((part, (states, clauses, mut nets)), severity): ((String, PartFaults), Severity),
) -> LintFinding {
    nets.sort();
    nets.dedup();
    let message = if severity == Severity::High {
        format!(
            "{part} is overstressed with the board at rest, {}: {}",
            states.join(" and "),
            clauses.join("; ")
        )
    } else {
        format!(
            "{part} is overstressed only {UNKNOWN_STATE}: {} (CONDITIONAL: the board \
             does not show that state, so the firmware decides whether it is reached; \
             with the loads at their declared draw it is within its ratings)",
            clauses.join("; ")
        )
    };
    LintFinding {
        check: LintCheck::DcOverstress,
        severity,
        message,
        refs: vec![part],
        nets,
    }
}

/// The note for a part whose stress came out as no finite number: the
/// rating was not checked there, which is not a pass. (A finite figure however
/// absurd is a fault, worded without the figure: [`far_past_clause`].)
fn not_a_number_note(part: String, clause: &str) -> LintFinding {
    let mut note = not_judged_note(
        &format!(
            "{part} {clause}, so its rating was not checked: the computation did not give a \
             number, which usually means a part value or model that makes the circuit \
             singular"
        ),
        Vec::new(),
    );
    note.refs = vec![part];
    note
}

/// The note for a part held past a limit that was a placeholder or guessed
/// card's, not its own: the stated condition, at low severity, so it never
/// fails the board ([`hauksbee_models::rating_basis`]).
fn stand_in_condition(part: String, condition: String) -> LintFinding {
    LintFinding {
        check: LintCheck::DcOverstress,
        severity: Severity::Low,
        message: condition,
        refs: vec![part],
        nets: Vec::new(),
    }
}

/// Keep the first stand-in exceedance seen for each part, as its condition.
fn note_stand_in(into: &mut BTreeMap<String, String>, events: Vec<StandInExceedance>) {
    for e in events {
        let c = &e.fault.component;
        let part = strip_unit_suffix(c.split(':').next().unwrap_or(c));
        into.entry(part.to_string())
            .or_insert_with(|| e.condition());
    }
}

/// Keep the first non-finite value computed for each part, worded as a clause.
fn note_absurd(into: &mut BTreeMap<String, String>, events: Vec<FaultEvent>) {
    for f in events {
        let part = strip_unit_suffix(f.component.split(':').next().unwrap_or(&f.component));
        into.entry(part.to_string()).or_insert_with(|| {
            if f.value.is_finite() {
                format!("{} {}", verb(f.kind), quantity(f.kind, f.value))
            } else {
                format!("{} a non-finite value", verb(f.kind))
            }
        });
    }
}

/// Move every logic output stamped at its card's weak-corner `ro` to its
/// strong-corner `ro_strong` (`strong`), or back. False when no driver moved
/// (no logic part, or none whose corners differ).
fn set_drive(bound: &mut BoundBoard, strong: bool) -> bool {
    let BoundBoard {
        circuit, digital, ..
    } = bound;
    let mut any = false;
    for d in digital.iter_mut() {
        let (weak, hard) = (d.levels.ro, d.levels.ro_strong);
        let (from, to) = if strong { (weak, hard) } else { (hard, weak) };
        if (from - to).abs() <= f64::EPSILON * from.abs().max(1.0) {
            continue;
        }
        // Only the legs stamped at the corner being left: a sink-only or
        // custom-resistance leg is not this card's push-pull output.
        for drv in d.drivers.values_mut() {
            if (drv.ron - from).abs() <= f64::EPSILON * from.abs().max(1.0) {
                drv.set_ron(circuit, to);
                any = true;
            }
        }
    }
    any
}

/// The state [`set_unknown_state_loads`] judges, as findings name it.
const UNKNOWN_STATE: &str = "with every data-set load at its card's unknown-state maximum (an \
     addressable LED latched full white)";

/// Hold every model-owned load that declares `unknown_state_max_a` at it
/// (`worst`), or release it. False when the board has no such load.
fn set_unknown_state_loads(bound: &mut BoundBoard, worst: bool) -> bool {
    let BoundBoard {
        circuit,
        behavioral,
        ..
    } = bound;
    (behavioral.iter_mut()).fold(false, |any, d| {
        d.set_unknown_state_loads(circuit, worst) | any
    })
}

/// The state data or firmware sets and the board cannot show: every load
/// declaring `unknown_state_max_a` held there, judged, then released. The
/// parts over a rating there but not `at_rest` are conditional on that state.
fn unknown_state_faults(
    bound: &mut BoundBoard,
    at_rest: &BTreeMap<String, PartFaults>,
) -> BTreeMap<String, PartFaults> {
    let mut conditional = BTreeMap::new();
    if set_unknown_state_loads(bound, true) {
        if let Some(x) = settle(bound) {
            for (part, clauses, nets) in judge(bound, &x).0 {
                // A stand-in limit passed only here is no more a fault than at
                // rest; the monitor already kept it out of the trips.
                if !at_rest.contains_key(&part) {
                    conditional.insert(part, (vec![UNKNOWN_STATE], clauses, nets));
                }
            }
        }
        set_unknown_state_loads(bound, false);
    }
    conditional
}

/// The parts over a rating in `bound`'s current state with each rail of
/// `over` held to what its supply can give ([`rest_supply`]), the rails then
/// put back. `None` when the held board does not solve.
fn held_to_capability(
    bound: &mut BoundBoard,
    over: &[rest_supply::Overload],
) -> Option<Vec<String>> {
    let saved = rest_supply::hold_to_capability(bound, over);
    let real = settle(bound).map(|x| {
        let (still, ..) = judge(bound, &x);
        still.into_iter().map(|(part, ..)| part).collect()
    });
    rest_supply::release(bound, saved);
    real
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

/// An arming change below this (the converter's limit source, in its own
/// units) counts as settled, as in the co-sim's own converter settle.
const ARMING_SETTLED: f64 = 1e-6;

/// Solve DC, let the logic parts and the behavioural converters react to it,
/// and repeat until no driven output moves and no converter's arming does.
/// A converter is stamped disarmed and only an update arms it (enable high,
/// feedback sensed), so without this every rail a buck or boost makes stays
/// at 0 V here and nothing it powers is judged. Returns the solution vector
/// of the last solve.
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
            circuit,
            digital,
            behavioral,
            ..
        } = &mut *bound;
        for d in digital.iter_mut() {
            d.tick(circuit, &node_v);
        }
        let arming = (behavioral.iter_mut())
            .map(|d| d.update_converter(circuit, &node_v))
            .fold(0.0, f64::max);
        if drive_state(bound) == before && arming < ARMING_SETTLED {
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

/// What one settled state shows: the parts held past a rating there, the
/// gates conducting under-driven, the values too large to be physical, and
/// the limits passed that were a stand-in card's, not the part's own.
type Judged = (
    Vec<(String, Vec<String>, Vec<String>)>,
    Vec<(GateDrive, Vec<String>)>,
    Vec<FaultEvent>,
    Vec<StandInExceedance>,
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
    let trips: std::collections::HashSet<&str> = (bound.device_meta.iter())
        .filter(|m| m.ratings.trips_over_current)
        .map(|m| m.reference.as_str())
        .collect();
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
                    let rated = rating_phrase(f, trips.contains(f.component.as_str()));
                    if is_far_past_rating(f.kind, f.value, f.limit) {
                        return far_past_clause(f, rated);
                    }
                    let detail = rating_detail(bound, &device_of, &net_of, f, bjt(f), &mut nets);
                    format!(
                        "{} {} {}{detail}, over its {} {rated}",
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
    (overstressed, under, monitor.implausible, monitor.stand_in)
}

/// The parenthesised detail after a fault's measured value: a BJT's base and
/// collector currents for a current fault, the gate and source nets for a
/// gate-source overvoltage, else nothing. Nets named are added to `nets`.
fn rating_detail(
    bound: &BoundBoard,
    device_of: &HashMap<&str, DeviceId>,
    net_of: &HashMap<NodeId, &str>,
    f: &FaultEvent,
    bjt: Option<(DeviceId, f64, f64)>,
    nets: &mut Vec<String>,
) -> String {
    if let Some((id, ic, ib)) = bjt.filter(|_| kind_class(f.kind) == 0) {
        if let Some(hauksbee_ir::Device::Bjt { b, .. }) = bound.circuit.devices.get(id.0 as usize) {
            nets.extend(net_of.get(b).map(|n| (*n).to_string()));
        }
        format!(" (base {}, collector {})", amps(ib), amps(ic))
    } else if f.kind == FaultKind::GateOvervoltage {
        gate_source_nets(bound, device_of, net_of, f, nets)
    } else {
        String::new()
    }
}

/// What the fault's limit is called: a fuse's current rating is its hold
/// current (past it the part trips, so say what that does to the rail), a gate
/// limit is the absolute-maximum VGS with its remedy, else just "rating".
fn rating_phrase(f: &FaultEvent, trips: bool) -> &'static str {
    if kind_class(f.kind) == 0 && trips {
        "hold current: the fuse trips and the rail it feeds collapses"
    } else if f.kind == FaultKind::GateOvervoltage {
        "absolute-maximum VGS rating (the card's vgs_max, from the datasheet's \
         Absolute Maximum Ratings): clamp the gate, for example with a lower-voltage \
         Zener, or pick a part rated for this drive"
    } else {
        "rating"
    }
}

/// The " (gate 'G', source 'S')" detail for a gate-source overvoltage, naming
/// the two nets the voltage was read between and adding them to `nets`.
fn gate_source_nets(
    bound: &BoundBoard,
    device_of: &HashMap<&str, DeviceId>,
    net_of: &HashMap<NodeId, &str>,
    f: &FaultEvent,
    nets: &mut Vec<String>,
) -> String {
    let id = device_of.get(f.component.as_str());
    let Some(hauksbee_ir::Device::Mosfet { g, s, .. }) =
        id.and_then(|id| bound.circuit.devices.get(id.0 as usize))
    else {
        return String::new();
    };
    let name = |n: &NodeId| net_of.get(n).copied();
    match (name(g), name(s)) {
        (Some(gate), Some(source)) => {
            nets.extend([gate.to_string(), source.to_string()]);
            format!(" (gate net '{gate}', source net '{source}')")
        }
        _ => String::new(),
    }
}

/// The rating a fault is judged against, coarsely: continuous and surge
/// current are one clause (current first, as the cause), then power,
/// temperature, voltage and the gate's own rating, which a drain-source
/// overvoltage on the same part must not hide.
const fn kind_class(kind: FaultKind) -> u8 {
    match kind {
        FaultKind::Overcurrent | FaultKind::SurgeCurrent | FaultKind::PinOvercurrent => 0,
        FaultKind::Overpower => 1,
        FaultKind::Overtemperature => 2,
        FaultKind::Overvoltage | FaultKind::ReverseBias => 3,
        FaultKind::GateOvervoltage => 4,
        FaultKind::Short => 5,
    }
}

/// The clause for a part so far past a rating that the figure is not a result
/// ([`is_far_past_rating`]): the fact, the rating it is past and why no number
/// is printed. A circuit that gives the part no limit at all is the usual
/// cause (a diode straight across a rail); the finding's remedy says to add
/// the series limit.
fn far_past_clause(f: &FaultEvent, rated: &str) -> String {
    let past = match f.kind {
        FaultKind::Overtemperature => "reaches a junction temperature far past",
        FaultKind::Overpower => "dissipates far more than",
        _ => "carries far more than",
    };
    format!(
        "{} {past} its {} {rated} ({})",
        f.component,
        quantity(f.kind, f.limit),
        far_past_reason(f.kind),
    )
}

const fn verb(kind: FaultKind) -> &'static str {
    match kind {
        FaultKind::Overcurrent | FaultKind::SurgeCurrent => "carries",
        FaultKind::PinOvercurrent => "passes",
        FaultKind::Overpower => "dissipates",
        FaultKind::Overvoltage | FaultKind::ReverseBias => "sees",
        FaultKind::GateOvervoltage => "sees a gate-source voltage of",
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
        FaultKind::Overvoltage
        | FaultKind::ReverseBias
        | FaultKind::GateOvervoltage
        | FaultKind::Short => format!("{v:.2} V"),
        FaultKind::Overtemperature => format!("{v:.0} C"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};
    use hauksbee_extract::Component;

    /// An XC6206-3.3 (200 mA) fed from +5V and loading its 3.3 V rail with
    /// `pixels` chained WS2812Bs: 0.6 mA each as declared, 36.6 mA at full white.
    fn ldo_with_pixels(pixels: i64) -> NetLintReport {
        let mut parts = vec![part("U1", "XC6206P332MR", &[("1", 1), ("2", 3), ("3", 2)])];
        let mut nets = vec![
            (1, "GND".to_string()),
            (2, "+5V".into()),
            (3, "+3V3".into()),
        ];
        for i in 0..pixels {
            let din = 10 + i;
            let pads = [("1", 3), ("2", din + 1), ("3", 1), ("4", din)];
            let mut led = part(&format!("D{i}"), "WS2812B", &pads);
            led.footprint = "LED_SMD:LED_WS2812B_PLCC4_5.0x5.0mm_P3.2mm".into();
            parts.push(led);
            nets.push((din, format!("/D{i}")));
        }
        nets.push((10 + pixels, "/DOUT".into()));
        let nets: Vec<(i64, &str)> = nets.iter().map(|(i, n)| (*i, n.as_str())).collect();
        dc_stress_lint(&board(&nets, parts), &ModelLibrary::builtin())
    }

    fn on_regulator(r: &NetLintReport) -> Vec<&LintFinding> {
        r.findings
            .iter()
            .filter(|f| f.refs.iter().any(|x| x == "U1"))
            .collect()
    }

    /// Legal control: three pixels (110 mA at full white) stay inside the
    /// LDO's 200 mA even in the unknown state, so nothing fires.
    #[test]
    fn few_pixels_on_a_stiff_rail_pass() {
        let report = ldo_with_pixels(3);
        assert!(on_regulator(&report).is_empty(), "{:#?}", report.findings);
    }

    /// Faulty: ten pixels draw 6 mA as declared but 366 mA at full white,
    /// past the LDO's 200 mA: one conditional finding naming that state.
    #[test]
    fn many_pixels_on_a_weak_regulator_fire_conditionally() {
        let report = ldo_with_pixels(10);
        let found = on_regulator(&report);
        assert_eq!(found.len(), 1, "{:#?}", report.findings);
        assert_eq!(found[0].severity, Severity::Medium);
        assert!(
            found[0].message.contains(UNKNOWN_STATE),
            "{}",
            found[0].message
        );
        assert!(
            found[0].message.contains("CONDITIONAL"),
            "{}",
            found[0].message
        );
    }

    /// +5V through a series part (a fuse, or a resistor ahead of a diode) into
    /// a load, on a rail the board powers by name.
    fn series_path(series: Component, load: Component) -> NetLintReport {
        let nets = [(1, "GND"), (2, "+5V"), (3, "/MID")];
        dc_stress_lint(&board(&nets, vec![series, load]), &ModelLibrary::builtin())
    }

    fn findings_on<'a>(r: &'a NetLintReport, reference: &str) -> Vec<&'a LintFinding> {
        (r.findings.iter())
            .filter(|f| f.refs.iter().any(|x| x == reference))
            .collect()
    }

    /// A diode `value` fed from +5V through a resistor of `ohms`.
    fn diode_at(value: &str, ohms: &str) -> NetLintReport {
        let r = part("R1", ohms, &[("1", 2), ("2", 3)]);
        series_path(r, part("D1", value, &[("A", 3), ("K", 1)]))
    }

    /// `cells` copies of a 1N4148WT (125 mA) fed from +5V through `ohms`
    /// each, all on the one rail the board powers by name.
    fn diode_cells(cells: i64, ohms: &str) -> NetLintReport {
        let mut nets = vec![(1, "GND".to_string()), (2, "+5V".into())];
        let mut parts = Vec::new();
        for k in 0..cells {
            let mid = 10 + k;
            nets.push((mid, format!("/C{k}")));
            parts.push(part(&format!("R{k}"), ohms, &[("1", 2), ("2", mid)]));
            parts.push(part(&format!("D{k}"), "1N4148WT", &[("A", mid), ("K", 1)]));
        }
        let nets: Vec<(i64, &str)> = nets.iter().map(|(i, n)| (*i, n.as_str())).collect();
        dc_stress_lint(&board(&nets, parts), &ModelLibrary::builtin())
    }

    /// Faulty model, not faulty parts: forty cells at about 400 mA each pull
    /// some 16 A from +5V, which no supply the board names gives. Held to
    /// the 3 A assumed for it, each carries about 75 mA, inside its 125 mA:
    /// no part is reported overstressed, and one finding names the rail, the
    /// draw and the limit.
    #[test]
    fn a_rest_draw_no_supply_gives_is_one_supply_finding_not_forty_overstresses() {
        let report = diode_cells(40, "10R");
        let high: Vec<&LintFinding> = (report.findings.iter())
            .filter(|f| f.severity == Severity::High)
            .collect();
        assert!(high.is_empty(), "{high:#?}");
        let supply: Vec<&LintFinding> = (report.findings.iter())
            .filter(|f| f.message.starts_with(rest_supply::OPENING))
            .collect();
        assert_eq!(supply.len(), 1, "{:#?}", report.findings);
        let f = supply[0];
        assert_eq!(f.severity, Severity::Medium);
        assert!(f.message.contains("from +5V"), "{}", f.message);
        assert!(f.message.contains("3.00 A assumed"), "{}", f.message);
        assert!(f.refs.iter().any(|r| r == "D0"), "{:?}", f.refs);
        assert!(f.refs.iter().any(|r| r == "D39"), "{:?}", f.refs);
    }

    /// Legal control: the same cells at about 400 mA, but only five of them
    /// (some 2 A, a draw a supply gives). Each diode is a real overstress.
    #[test]
    fn a_plausible_rest_draw_keeps_each_overstress() {
        let report = diode_cells(5, "10R");
        for k in 0..5 {
            let on = findings_on(&report, &format!("D{k}"));
            assert!(
                on.iter().any(|f| f.severity == Severity::High),
                "D{k}: {:#?}",
                report.findings
            );
        }
        assert!(
            !(report.findings.iter()).any(|f| f.message.starts_with(rest_supply::OPENING)),
            "{:#?}",
            report.findings
        );
    }

    /// Faulty: one diode on a near-short (0.5 ohm) pulls more than the 3 A
    /// limit, and still carries 3 A, far past 125 mA, with the rail held
    /// there: a real overstress at any supply stays one.
    #[test]
    fn a_part_over_its_rating_even_at_the_supply_limit_stays_an_overstress() {
        let report = diode_cells(1, "0.5R");
        let on = findings_on(&report, "D0");
        assert!(
            on.iter().any(|f| f.severity == Severity::High),
            "{:#?}",
            report.findings
        );
        assert!(
            !(report.findings.iter()).any(|f| f.message.starts_with(rest_supply::OPENING)),
            "{:#?}",
            report.findings
        );
    }

    /// Legal control: about 100 mA is inside the SOD-523 part's 125 mA.
    #[test]
    fn small_signal_diode_in_sod523_passes_at_100ma() {
        let report = diode_at("1N4148WT", "43R");
        assert!(
            findings_on(&report, "D1").is_empty(),
            "{:#?}",
            report.findings
        );
    }

    /// Faulty: about 200 mA is past 125 mA, which the old DO-35 300 mA row let through.
    #[test]
    fn small_signal_diode_in_sod523_fails_at_200ma() {
        let report = diode_at("1N4148WT", "22R");
        assert_eq!(
            findings_on(&report, "D1").len(),
            1,
            "{:#?}",
            report.findings
        );
    }

    /// Faulty: a diode fed through a near-zero resistance computes to kiloamps.
    /// The exact figure is not physical, but the overstress is: the circuit
    /// gives the diode no limit at all, so it stays a serious finding, worded
    /// without the number. Legal control: the 22R case prints its real figure.
    #[test]
    fn an_impossible_current_is_a_serious_finding_without_the_number() {
        let report = diode_at("1N4148", "0.0001R");
        let found = findings_on(&report, "D1");
        assert_eq!(found.len(), 1, "{:#?}", report.findings);
        assert_eq!(found[0].severity, Severity::High, "{}", found[0].message);
        let m = &found[0].message;
        assert!(
            m.contains("D1 carries far more than its") && m.contains("not physical"),
            "{m}"
        );
        assert!(!m.contains("no real part can see"), "{m}");
        assert!(!m.contains("4000") && !m.contains(" kA"), "{m}");
        let real = diode_at("1N4148", "22R");
        let real = &findings_on(&real, "D1")[0].message;
        assert!(
            real.contains("mA") && !real.contains("not physical"),
            "{real}"
        );
        assert!(hauksbee_bind::is_implausible(FaultKind::Overpower, 3.0e3));
        assert!(hauksbee_bind::is_implausible(
            FaultKind::Overtemperature,
            4.0e3
        ));
        assert!(!hauksbee_bind::is_implausible(FaultKind::Overpower, 2.0));
    }

    /// A load straight across a rail through a 0 ohm link (no series limit),
    /// the rail named `rail`.
    fn straight_across(rail: &str, load: &str) -> NetLintReport {
        let nets = [(1, "GND"), (2, rail), (3, "/MID")];
        let link = part("R1", "0", &[("1", 2), ("2", 3)]);
        let d = part("D1", load, &[("A", 3), ("K", 1)]);
        dc_stress_lint(&board(&nets, vec![link, d]), &ModelLibrary::builtin())
    }

    /// Faulty: the same diode with no series limit on a 5, 12 or 24 V rail is
    /// overstressed on every one; raising the rail only makes it worse, so the
    /// finding must never vanish as the figure passes the plausibility bound.
    /// A generic LED is a finding at each too: its rating is a stand-in, but
    /// a stand-in is never 100x wrong, so "far past" is not excused by it.
    #[test]
    fn no_series_limit_stays_a_finding_as_the_rail_rises() {
        for rail in ["+5V", "+12V", "+24V"] {
            let own = straight_across(rail, "1N4148");
            let on = findings_on(&own, "D1");
            assert!(
                on.iter().any(|f| f.severity == Severity::High),
                "{rail}: {:#?}",
                own.findings
            );
            let led = straight_across(rail, "LED");
            let on = findings_on(&led, "D1");
            assert!(
                on.iter().any(|f| f.severity == Severity::High
                    && f.message.contains("far more than")
                    && !hauksbee_models::rating_basis::is_condition(&f.message)),
                "{rail}: {:#?}",
                led.findings
            );
        }
    }

    /// Faulty: a steady ~200 mA through a DO-35 1N4148 is past its 150 mA
    /// average forward current IF(AV) (Vishay 81857 p.1). The card used to rate
    /// DC current by the 300 mA continuous-IF row and called this board fine.
    #[test]
    fn do35_1n4148_fails_at_a_steady_200ma() {
        let report = diode_at("1N4148", "22R");
        assert_eq!(
            findings_on(&report, "D1").len(),
            1,
            "{:#?}",
            report.findings
        );
    }

    /// Legal control: about 100 mA is inside the DO-35 part's 150 mA.
    #[test]
    fn do35_1n4148_passes_at_100ma() {
        let report = diode_at("1N4148", "43R");
        assert!(
            findings_on(&report, "D1").is_empty(),
            "{:#?}",
            report.findings
        );
    }

    /// An MCP73831 whose input `input` is held by a 10k to ground, with its
    /// charge LED from the input through 1k to STAT, plus a yellow LED `D2`
    /// across +12V the wrong way round.
    fn charger_with_led(input: &str) -> NetLintReport {
        let nets = [
            (1, "GND"),
            (2, input),
            (3, "V_BATT"),
            (4, "/STAT"),
            (5, "/PROG"),
            (6, "/LED_A"),
            (7, "+12V"),
        ];
        let parts = vec![
            part(
                "U3",
                "MCP73831",
                &[("1", 4), ("2", 1), ("3", 3), ("4", 2), ("5", 5)],
            ),
            part("R6", "2k", &[("1", 5), ("2", 1)]),
            part("R15", "10k", &[("1", 2), ("2", 1)]),
            part("R2", "1k", &[("1", 2), ("2", 6)]),
            part("D1", "YELLOW LED", &[("A", 6), ("K", 4)]),
            part("D2", "YELLOW LED", &[("A", 1), ("K", 7)]),
        ];
        dc_stress_lint(&board(&nets, parts), &ModelLibrary::builtin())
    }

    /// Legal control: a charger's quiescent draw on an input nothing feeds
    /// (a USB rail with no cable) has no supply to draw from, so it does not
    /// pull that rail below ground and reverse-bias the charge LED. Faulty:
    /// the LED wired backwards across +12V is still over its 5 V reverse
    /// rating.
    #[test]
    fn an_unfed_charger_input_does_not_reverse_its_led() {
        for input in ["V_USB", "+5V"] {
            let report = charger_with_led(input);
            assert!(
                findings_on(&report, "D1").is_empty(),
                "{input}: {:#?}",
                report.findings
            );
            assert_eq!(
                findings_on(&report, "D2").len(),
                1,
                "{input}: {:#?}",
                report.findings
            );
        }
    }

    /// `value` on a fuse footprint feeding a load of `ohms` from +5V.
    fn fuse_at(value: &str, ohms: &str) -> NetLintReport {
        let mut f = part("F1", value, &[("1", 2), ("2", 3)]);
        f.footprint = "Fuse:Fuse_1206_3216Metric".into();
        series_path(f, part("R1", ohms, &[("1", 3), ("2", 1)]))
    }

    /// Legal control: 0.3 A through a polyfuse, however its rating is spelled.
    #[test]
    fn polyfuse_below_hold_current_passes() {
        for value in ["500mA", "6V 0.5A", "PTC 750mA / 33V"] {
            let report = fuse_at(value, "16R");
            assert!(
                findings_on(&report, "F1").is_empty(),
                "{value}: {:#?}",
                report.findings
            );
        }
    }

    /// Faulty: 1 A through it trips it and takes the rail down, and the finding says so.
    #[test]
    fn polyfuse_above_hold_current_reports_a_trip() {
        for value in ["500mA", "6V 0.5A", "PTC 750mA / 33V"] {
            let report = fuse_at(value, "4R7");
            let found = findings_on(&report, "F1");
            assert_eq!(found.len(), 1, "{value}: {:#?}", report.findings);
            let message = &found[0].message;
            assert!(message.contains("hold current"), "{message}");
            assert!(message.contains("trips"), "{message}");
        }
    }

    /// An LED the board names only "LED" binds the generic placeholder card,
    /// whose 25 mA is no datasheet's: ~60 mA through it is the stated
    /// condition, at low severity, never a failing finding.
    #[test]
    fn a_placeholder_rating_passed_is_a_stated_condition_not_a_fault() {
        let r = part("R1", "47R", &[("1", 2), ("2", 3)]);
        let report = series_path(r, part("D1", "LED", &[("A", 3), ("K", 1)]));
        let found = findings_on(&report, "D1");
        assert!(!found.is_empty(), "{:#?}", report.findings);
        assert!(
            found.iter().all(|f| f.severity == Severity::Low),
            "{:#?}",
            report.findings
        );
        let stated = found
            .iter()
            .find(|f| hauksbee_models::rating_basis::is_condition(&f.message))
            .unwrap_or_else(|| panic!("no stated condition: {:#?}", report.findings));
        assert!(
            stated.message.contains("judged against a generic")
                && stated.message.contains("give D1 its own model"),
            "{}",
            stated.message
        );
    }

    /// The part's own datasheet card: past its rating fails, inside it passes.
    #[test]
    fn an_own_card_rating_passed_fails_and_inside_it_passes() {
        let over = diode_at("1N4148", "22R");
        let found = findings_on(&over, "D1");
        assert_eq!(found.len(), 1, "{:#?}", over.findings);
        assert_eq!(found[0].severity, Severity::High, "{}", found[0].message);
        assert!(!hauksbee_models::rating_basis::is_condition(
            &found[0].message
        ));
        let inside = diode_at("1N4148", "43R");
        assert!(
            findings_on(&inside, "D1").is_empty(),
            "{:#?}",
            inside.findings
        );
    }

    /// A power FET the library knows only by its package binds the generic
    /// placeholder card (30 V, no datasheet's). Held off on a 48 V rail it sees
    /// 48 V: judged against that stand-in, this is the stated condition naming
    /// the part and the card, never a failing finding, because the fitted part
    /// may be rated far past it.
    #[test]
    fn a_power_fet_on_the_generic_placeholder_past_30_v_is_a_condition() {
        let nets = [(1, "GND"), (2, "+48V"), (3, "/DRAIN")];
        let pull = part("R1", "10k", &[("1", 2), ("2", 3)]);
        let mut fet = part("Q1", "NFET-UNCARDED", &[("G", 1), ("D", 3), ("S", 1)]);
        fet.footprint = "Package_TO_SOT_SMD:TO-252-2".into();
        let report = dc_stress_lint(&board(&nets, vec![pull, fet]), &ModelLibrary::builtin());
        let found = findings_on(&report, "Q1");
        assert!(!found.is_empty(), "{:#?}", report.findings);
        assert!(
            found.iter().all(|f| f.severity == Severity::Low),
            "{:#?}",
            report.findings
        );
        let stated = (found.iter())
            .find(|f| hauksbee_models::rating_basis::is_condition(&f.message))
            .unwrap_or_else(|| panic!("no stated condition: {:#?}", report.findings));
        assert!(
            stated
                .message
                .contains("judged against a generic 30 V voltage rating")
                && stated.message.contains("generic_nmos_power_pkg")
                && stated.message.contains("give Q1 its own model"),
            "{}",
            stated.message
        );
    }

    /// An LM1117-ADJ setting '+5V' from its divider, its input on `input`,
    /// loading the rail with a 1k.
    fn adj_ldo_from(input: &str) -> NetLintReport {
        let nets = [(1, "GND"), (2, input), (3, "+5V"), (4, "/ADJ")];
        let parts = vec![
            part("U1", "LM1117", &[("1", 4), ("2", 3), ("3", 2)]),
            part("R1", "240", &[("1", 3), ("2", 4)]),
            part("R2", "715", &[("1", 4), ("2", 1)]),
            part("R3", "1k", &[("1", 3), ("2", 1)]),
        ];
        dc_stress_lint(&board(&nets, parts), &ModelLibrary::builtin())
    }

    /// Faulty: nothing powers the regulator's input, so '+5V' never comes up
    /// in the solve and every rating on it was held at a dead rail: the pass
    /// says so as not judged (never silence, so never a pass). Legal control:
    /// fed from +12V the rail comes up and nothing is said.
    #[test]
    fn a_rail_that_never_comes_up_is_not_judged_and_a_live_one_is_silent() {
        use hauksbee_models::rating_basis::is_not_judged;
        let dead = adj_ldo_from("/VIN_JACK");
        let note = (dead.findings.iter())
            .find(|f| is_not_judged(&f.message))
            .unwrap_or_else(|| panic!("no not-judged note: {:#?}", dead.findings));
        assert_eq!(note.severity, Severity::Low);
        assert!(note.is_abstention(), "{}", note.message);
        assert!(
            note.nets == ["+5V"] || note.message.contains("did not solve"),
            "{note:#?}"
        );
        let live = adj_ldo_from("+12V");
        assert!(
            !live.findings.iter().any(|f| is_not_judged(&f.message)),
            "{:#?}",
            live.findings
        );
    }

    /// A 2512 resistor of `value` straight across a named +5V rail.
    fn resistor_across_5v(value: &str) -> NetLintReport {
        let nets = [(1, "GND"), (2, "+5V")];
        let mut r = part("R1", value, &[("1", 2), ("2", 1)]);
        r.footprint = "Resistor_SMD:R_2512_6332Metric".into();
        dc_stress_lint(&board(&nets, vec![r]), &ModelLibrary::builtin())
    }

    /// Faulty: a 1e-9 ohm resistor is bound at the solver's floor, so the rail
    /// draws thousands of amps and collapses. No figure of that is physical,
    /// so none is printed, and the rail's dead-rail note names the draw that
    /// pulled it down rather than a missing source. Legal control: 30 ohm is
    /// quiet.
    #[test]
    fn a_resistor_at_the_solver_floor_prints_no_draw_and_explains_the_dead_rail() {
        let report = resistor_across_5v("1e-9");
        let rest = (report.findings.iter())
            .find(|f| f.message.starts_with(rest_supply::OPENING))
            .unwrap_or_else(|| panic!("no rest-draw finding: {:#?}", report.findings));
        assert!(
            rest.message.contains("a near-short") && rest.message.contains("not physical"),
            "{}",
            rest.message
        );
        assert!(
            !rest.message.contains("4995") && !rest.message.contains(" mW"),
            "{}",
            rest.message
        );
        let dead = (report.findings.iter())
            .find(|f| rating_basis::is_not_judged(&f.message))
            .unwrap_or_else(|| panic!("no dead-rail note: {:#?}", report.findings));
        assert!(
            dead.message.contains("pulled down") && !dead.message.contains("no declared source"),
            "{}",
            dead.message
        );
        assert!(!dead.message.contains("-0.00"), "{}", dead.message);
        let fine = resistor_across_5v("30");
        assert!(fine.findings.is_empty(), "{:#?}", fine.findings);
    }

    /// Faulty: past twice its junction limit the temperature is stated as
    /// "far past", without the figure; legal control: the same part at a
    /// plausible overstress prints its figure. (Relative to the part's own
    /// rating, not only the 1500 C absolute bound.)
    #[test]
    fn far_past_a_rating_is_stated_without_the_figure() {
        use hauksbee_bind::is_far_past_rating as far;
        assert!(far(FaultKind::Overtemperature, 998.0, 150.0));
        assert!(!far(FaultKind::Overtemperature, 180.0, 150.0));
        assert!(far(FaultKind::Overcurrent, 20.0, 0.1));
        assert!(!far(FaultKind::Overcurrent, 0.851, 0.125));
        assert!(far(FaultKind::Overpower, 3.0e3, 1e3));
        assert!(!far(FaultKind::Overvoltage, 50.0, 3.3));
        assert!(!far(FaultKind::Overcurrent, f64::NAN, 0.1));
    }

    /// An LM2596-5.0 buck (averaged converter card, ON/OFF grounded = on)
    /// from +12V making '+5V' through its inductor, loaded by a 1N4148
    /// (DO-35) through `ohms`.
    fn buck_into_diode(ohms: &str) -> NetLintReport {
        let nets = [
            (1, "GND"),
            (2, "+12V"),
            (3, "/SW"),
            (4, "+5V"),
            (5, "/LOAD"),
        ];
        let parts = vec![
            part(
                "U1",
                "LM2596S-5.0",
                &[("1", 2), ("2", 3), ("3", 1), ("4", 4), ("5", 1)],
            ),
            part("L1", "33uH", &[("1", 3), ("2", 4)]),
            part("C1", "220uF", &[("1", 4), ("2", 1)]),
            part("R1", ohms, &[("1", 4), ("2", 5)]),
            part("D1", "1N4148", &[("A", 5), ("K", 1)]),
        ];
        dc_stress_lint(&board(&nets, parts), &ModelLibrary::builtin())
    }

    /// A behavioural converter is stamped disarmed; the DC ratings solve must
    /// arm it as the co-sim does, or every rail it makes sits at 0 V and
    /// nothing behind it is judged. Faulty: about 180 mA through the DO-35
    /// 1N4148 on the buck's '+5V' is over its rating and fires. Legal control:
    /// about 100 mA is inside it and nothing is said. Neither leaves '+5V'
    /// not judged.
    #[test]
    fn a_rail_a_buck_makes_is_judged_both_ways() {
        use hauksbee_models::rating_basis::is_not_judged;
        for (ohms, faults) in [("22R", 1), ("43R", 0)] {
            let report = buck_into_diode(ohms);
            assert!(
                !report.findings.iter().any(|f| is_not_judged(&f.message)),
                "{ohms}: {:#?}",
                report.findings
            );
            assert_eq!(
                findings_on(&report, "D1").len(),
                faults,
                "{ohms}: {:#?}",
                report.findings
            );
        }
    }
}
