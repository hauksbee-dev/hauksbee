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
//! fails, or ends only on the solver's relaxed power-on stand-in (every diode
//! switched off: no root of the circuit, so no real junction current), or
//! whose named rails never come up in it, gets one low-severity note saying
//! what was not judged ([`rating_basis::not_judged`]): never silence, never a
//! pass, and never a figure the circuit does not give.
//!

use std::collections::{BTreeMap, HashMap};

use hauksbee_bind::binder::{
    bind_board, held_off_rails, is_ground, power_rail_voltage, BoundBoard,
};
use hauksbee_bind::control_inputs::{
    dc_determined_nodes, hold_off, undetermined_gate_note, undetermined_gates, UndeterminedGate,
};
use hauksbee_bind::junctions::dc_with_junction_homotopy;
use hauksbee_bind::stress::{
    bjt_currents, stress_clause, stress_quantity, strip_unit_suffix, unbounded_note, FaultEvent,
    FaultKind, GateDrive, RegulatorHeat, StandInExceedance, StressMonitor,
};
use hauksbee_extract::{
    ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity, INFORMATIONAL_MARKER,
};
use hauksbee_ir::{DeviceId, NodeId};
use hauksbee_models::rating_basis::{self, StandInKind, UNSTATED_RATING_MARKER};
use hauksbee_models::ModelLibrary;
use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};

use super::rest_supply;
use super::solved_levels::SolvedLevels;
use hauksbee_ir::units::format_volts;

/// Bind `board`, solve its DC operating point, and report every part held
/// past a rating there, at every assumed corner ([`dc_stress_at_corners`]).
#[must_use]
pub fn dc_stress_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let bound = bind_board(board, lib);
    dc_stress_at_corners(board, lib, bound).report
}

/// What the DC stress pass found, and the levels it solved to find it.
#[derive(Debug, Clone)]
pub struct DcStress {
    /// Every part held past a rating, and what was not judged.
    pub report: NetLintReport,
    /// The levels the stand-in pass's settled states gave, for the judges that
    /// read a net's level ([`SolvedLevels`]). Empty when no state solved.
    pub levels: SolvedLevels,
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
) -> DcStress {
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
        let (report, levels) = dc_stress_pass(stand_in);
        return DcStress {
            report: with_qualifiers(report.findings, qualified),
            levels,
        };
    }
    let pick = |k: usize| -> Vec<(&str, &str)> {
        (sets.iter())
            .map(|(r, ids)| (r.as_str(), ids[k.min(ids.len() - 1)].as_str()))
            .collect()
    };
    let (stand_in_report, levels) = dc_stress_pass(stand_in);
    let mut passes = vec![stand_in_report.findings];
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
    DcStress {
        report: with_qualifiers(findings, qualified),
        levels,
    }
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

/// One part's stress faults: `(part, clauses, nets, unbounded)`. `unbounded`
/// is true when every figure behind the clauses is physically unbounded
/// ([`hauksbee_bind::stress::is_unbounded`]): the idealised source, not the
/// part, set it, so it is a plain note and never a gating claim.
type PartFault = (String, Vec<String>, Vec<String>, bool);

/// The parts whose only stress is unbounded, with the nets named and the rail
/// the current flows from, when known: each becomes one warning
/// ([`unbounded_finding`]).
type Unbounded = BTreeMap<String, (Vec<String>, Option<String>)>;

/// Record a part's fault in `label`'s state: the state, the first clauses
/// seen, and every net named.
fn record_fault(
    by_part: &mut BTreeMap<String, PartFaults>,
    label: &'static str,
    (part, clauses, nets): (String, Vec<String>, Vec<String>),
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
/// A part still past a stand-in card's limit there (an LED with no series
/// resistor, held to a USB port's 0.5 A) is neither cleared nor a fault: its
/// stated condition at the held rail goes to `stated`, never the claim that
/// it is within its ratings. A part dropped otherwise is over a rating only
/// at a draw no supply gives; its first clause goes to `only_ideal`. Returns
/// whether the held re-judge did not solve.
fn hold_overloaded_rails(
    state: &mut BoundBoard,
    over: &[rest_supply::Overload],
    found: &mut Vec<PartFault>,
    strong_found: &mut Vec<PartFault>,
    only_ideal: &mut BTreeMap<String, String>,
    stated: &mut BTreeMap<String, String>,
) -> bool {
    let real = held_to_capability(state, over);
    let keep = |part: &str| {
        real.as_ref()
            .is_some_and(|(r, _)| r.iter().any(|p| p == part))
    };
    let held_condition = |part: &str| {
        real.as_ref()
            .and_then(|(_, conditions)| conditions.get(part).cloned())
    };
    for (part, clauses, ..) in found.iter().chain(strong_found.iter()) {
        if keep(part.as_str()) {
            continue;
        }
        if let Some(condition) = held_condition(part.as_str()) {
            stated.insert(part.clone(), condition);
        } else {
            only_ideal
                .entry(part.clone())
                .or_insert_with(|| clauses.first().cloned().unwrap_or_default());
        }
    }
    found.retain(|(part, ..)| keep(part.as_str()));
    strong_found.retain(|(part, ..)| keep(part.as_str()));
    real.is_none()
}

/// The parts over a rating in the current state at the strong-drive corner,
/// noting what the judge there found impossible or past a stand-in limit, then
/// back to the weak corner. Empty when the board has no drive to set or the
/// corner does not solve.
fn strong_corner_faults(
    state: &mut BoundBoard,
    impossible: &mut BTreeMap<String, String>,
    stated: &mut BTreeMap<String, String>,
    unstated: &mut BTreeMap<String, (f64, f64)>,
) -> Vec<PartFault> {
    let mut strong_found = Vec::new();
    if set_drive(state, true) {
        if let Ok(x) = settle(state) {
            let (strong_over, _, absurd, stand_in) = judge(state, &x);
            note_absurd(impossible, absurd);
            note_stand_in(stated, unstated, stand_in);
            strong_found = strong_over;
        }
        set_drive(state, false);
    }
    strong_found
}

/// Keep, per part, the gate drive that was lowest in any state.
fn note_under_driven(
    under: &mut BTreeMap<String, (GateDrive, Vec<String>)>,
    under_driven: Vec<(GateDrive, Vec<String>)>,
) {
    for (drive, nets) in under_driven {
        let worst = under
            .entry(drive.reference.clone())
            .or_insert_with(|| (drive.clone(), nets));
        if drive.vgs < worst.0.vgs {
            worst.0 = drive;
        }
    }
}

/// [`dc_stress_lint`] over an already bound board.
#[must_use]
pub fn dc_stress_findings(bound: BoundBoard) -> NetLintReport {
    dc_stress_pass(bound).0
}

/// [`dc_stress_findings`], and the levels the settled states solved to.
fn dc_stress_pass(mut bound: BoundBoard) -> (NetLintReport, SolvedLevels) {
    let mut levels = static_levels();
    // The nodes the netlist sets, before the hold below ties an undetermined
    // gate to its source: a level the solver gives a node nothing sets is its
    // gmin 0 V, never a measurement.
    let determined = dc_determined_nodes(&bound);
    // A FET whose gate only a pin with no DC behaviour drives is held off and
    // not judged: its gate level is the board's, not the solver's 0 V.
    let held = undetermined_gates(&bound);
    hold_off(&mut bound, &held);
    let held: Vec<LintFinding> = held.iter().map(undetermined_gate_finding).collect();
    if bound.device_meta.is_empty() {
        return (NetLintReport { findings: held }, levels);
    }
    // part -> (state labels, clauses); one finding per physical part.
    let mut by_part: BTreeMap<String, PartFaults> = BTreeMap::new();
    let mut unsolved = Vec::new();
    let mut impossible: BTreeMap<String, String> = BTreeMap::new();
    let mut under: BTreeMap<String, (GateDrive, Vec<String>)> = BTreeMap::new();
    // part -> the stated condition for its first limit passed that was a
    // placeholder or guessed card's, not its own: never a failing finding.
    let mut stated: BTreeMap<String, String> = BTreeMap::new();
    let mut unstated: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    // Rails drawn past what their supply gives, and the parts over a rating
    // only at that draw (part -> first clause): one finding, not overstresses.
    let mut overloaded: Vec<rest_supply::Overload> = Vec::new();
    let mut only_ideal: BTreeMap<String, String> = BTreeMap::new();
    let mut held_unsolved = false;
    let mut unbounded: Unbounded = BTreeMap::new();
    // Named rails dead in every state solved: what the solve did not judge.
    let mut dead: Option<BTreeMap<String, (f64, f64)>> = None;
    for ((label, registers_high), strong_label) in STATES.into_iter().zip(STRONG_STATES) {
        // The states run in order on the one bound board: the second starts
        // from the first's settled circuit and re-settles after its change.
        let state = &mut bound;
        if registers_high && !set_registers_high(state) {
            continue; // no clocked state: the second pass would repeat the first
        }
        let x = match settle(state) {
            Ok(x) => x,
            Err(why) => {
                unsolved.push((label, why));
                continue;
            }
        };
        let here = dead_rails(state, &x);
        record_levels(&mut levels, state, &x, &determined, &here);
        dead.get_or_insert_with(|| here.clone())
            .retain(|net, _| here.contains_key(net));
        let (overstressed, under_driven, absurd, stand_in) = judge(state, &x);
        let rail = source_rail(state, &x.0, &x.1);
        let over = rest_supply::overloads(state, &x.0, &x.1);
        note_absurd(&mut impossible, absurd);
        note_stand_in(&mut stated, &mut unstated, stand_in);
        note_under_driven(&mut under, under_driven);
        let mut found = overstressed;
        // The same state at the strong-drive corner, then back to the weak
        // one so the next state starts where it always did. A part already
        // over a rating at the weak corner is not news at the strong one, and
        // a gate under-driven at the weak corner is judged there alone.
        let mut strong_found =
            strong_corner_faults(state, &mut impossible, &mut stated, &mut unstated);
        // A draw no supply gives: keep only the parts still over a rating
        // with the rails held to what their supplies can give.
        if !over.is_empty() {
            held_unsolved |= hold_overloaded_rails(
                state,
                &over,
                &mut found,
                &mut strong_found,
                &mut only_ideal,
                &mut stated,
            );
            rest_supply::note_overloads(&mut overloaded, over);
        }
        let labels = (label, strong_label);
        record_state_faults(
            &mut by_part,
            &mut unbounded,
            labels,
            found,
            strong_found,
            rail.as_deref(),
        );
    }
    // A part over a rating at a plausible draw in any state is reported there.
    only_ideal.retain(|part, _| !by_part.contains_key(part));
    let mut conditional = unknown_state_faults(&mut bound, &by_part, &mut unbounded);
    conditional.retain(|part, _| !only_ideal.contains_key(part));
    // A part with a bounded overstress is reported as one; the unbounded
    // figure beside it adds nothing.
    unbounded.retain(|part, _| !by_part.contains_key(part) && !conditional.contains_key(part));
    let reported: Vec<&String> = (by_part.keys())
        .chain(conditional.keys())
        .chain(only_ideal.keys())
        .chain(unbounded.keys())
        .collect();
    let stated: Vec<LintFinding> = (stated.into_iter())
        .filter(|(part, _)| !reported.contains(&part))
        .map(|(part, condition)| stand_in_condition(part, condition))
        .chain(unstated_rating_note(&unstated, &reported))
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
    findings.extend(unbounded.into_iter().map(unbounded_finding));
    findings.extend(stated);
    findings.extend(rest_supply::implausible_rest_finding(
        &overloaded,
        &only_ideal,
        held_unsolved,
    ));
    findings.extend(abstained_note(&unsolved).into_iter().chain(held));
    findings.extend((dead.filter(|d| !d.is_empty())).map(|d| dead_rail_note(d, &overloaded)));
    (NetLintReport { findings }, levels.without_dark_nets())
}

/// No levels yet, to the tolerance the static solves run at.
fn static_levels() -> SolvedLevels {
    let opts = SolverOptions::default();
    SolvedLevels::empty((opts.reltol, opts.vntol))
}

/// Record the levels the settled state `solved` gave into `levels`: the nets
/// the netlist sets, less the named rails the state left dead (`dead`: down
/// below [`DEAD_RAIL_SHARE`] of their name, or held off by the board). A dead
/// rail's parts are unpowered, not out of range, and the dead-rail note
/// already says it; the judges fall back to what the board states of it.
fn record_levels(
    levels: &mut SolvedLevels,
    bound: &BoundBoard,
    solved: &(Workspace, Vec<f64>),
    determined: &std::collections::HashSet<NodeId>,
    dead: &BTreeMap<String, (f64, f64)>,
) {
    let held: Vec<String> = held_off_rails(bound).into_iter().map(|(n, _)| n).collect();
    let (ws, x) = solved;
    for (name, &node) in &bound.net_nodes {
        if is_ground(name) || !determined.contains(&node) {
            continue;
        }
        let Some(v) = ws.layout.node(node).and_then(|i| x.get(i).copied()) else {
            continue;
        };
        if dead.contains_key(name) || (held.contains(name) && power_rail_voltage(name).is_some()) {
            levels.forget(name);
            continue;
        }
        levels.observe(name, v);
    }
}

/// The note that the check abstained in the `unsolved` states, each with
/// why its DC gave no operating point to judge; none when every state solved.
fn abstained_note(unsolved: &[(&str, Unsettled)]) -> Option<LintFinding> {
    let states = |why: Unsettled| -> Vec<&str> {
        (unsolved.iter())
            .filter(|(_, w)| *w == why)
            .map(|(label, _)| *label)
            .collect()
    };
    let (failed, stand_in) = (states(Unsettled::NoSolve), states(Unsettled::StandIn));
    let mut why = Vec::new();
    if !failed.is_empty() {
        why.push(format!(
            "{}: the board's DC operating point did not solve",
            failed.join(" or ")
        ));
    }
    if !stand_in.is_empty() {
        why.push(format!(
            "{}: the DC solve found no operating point of the whole circuit, only its \
             power-on stand-in with every diode switched off, where a junction's own \
             equation gives a current it never carried",
            stand_in.join(" or ")
        ));
    }
    (!why.is_empty()).then(|| {
        not_judged_note(
            &format!(
                "the DC ratings check abstained {}, so no part was checked against its \
                 ratings there",
                why.join("; and ")
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
/// `pulled` (one the board draws more from than its supply gives) was not left
/// unpowered by anything missing: the draw pulled it down, and the note says
/// so instead of blaming an absent source, calling it a near-short only when
/// it is one ([`rest_supply::Overload::is_near_short`]).
fn dead_rail_note(
    dead: BTreeMap<String, (f64, f64)>,
    pulled: &[rest_supply::Overload],
) -> LintFinding {
    let pull = |net: &String| pulled.iter().find(|o| &o.net == net);
    let rails: Vec<String> = (dead.iter())
        .map(|(net, (v, named))| {
            let v = if v.abs() < 0.005 { 0.0 } else { *v };
            match pull(net) {
                Some(o) if o.is_near_short() => format!(
                    "'{net}' (pulled down to {} by a near-short on it; its name says {})",
                    format_volts(v),
                    format_volts(named)
                ),
                Some(_) => format!(
                    "'{net}' (pulled down to {} by the draw on it; its name says {})",
                    format_volts(v),
                    format_volts(named)
                ),
                None => format!(
                    "'{net}' (solved at {}; its name says {})",
                    format_volts(v),
                    format_volts(named)
                ),
            }
        })
        .collect();
    let cause = if dead.keys().all(|n| pull(n).is_some()) {
        "the rail was pulled down by a draw more than its supply gives, so it never sat at \
         its level"
    } else {
        "the rail did not come up in the solved operating point (an input with no declared \
         source, or a supply the model could not start)"
    };
    not_judged_note(
        &format!(
            "the DC ratings check abstained on the parts powered from {}: {cause}, so no part \
             on it was checked against its ratings at that level",
            hauksbee_ir::vocabulary::names_summary(&rails)
        ),
        dead.into_keys().collect(),
    )
}

/// The low-severity note that `gate`'s FET was held off and not judged
/// because nothing the DC model drives sets its gate level.
fn undetermined_gate_finding(gate: &UndeterminedGate) -> LintFinding {
    LintFinding {
        refs: vec![gate.reference.clone()],
        ..not_judged_note(&undetermined_gate_note(gate), vec![gate.net.clone()])
    }
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

/// File one part's stress: a bounded one as an overstress of that state, an
/// unbounded one as a warning for the part ([`unbounded_finding`]).
fn record_fault_or_note(
    by_part: &mut BTreeMap<String, PartFaults>,
    unbounded: &mut Unbounded,
    label: &'static str,
    (part, clauses, nets, is_unbounded): PartFault,
    rail: Option<&str>,
) {
    if is_unbounded {
        note_unbounded(unbounded, part, nets, rail);
    } else {
        record_fault(by_part, label, (part, clauses, nets));
    }
}

/// File one state's stresses: the weak corner's, then the strong corner's for
/// the parts the weak corner did not already name.
fn record_state_faults(
    by_part: &mut BTreeMap<String, PartFaults>,
    unbounded: &mut Unbounded,
    (label, strong_label): (&'static str, &'static str),
    found: Vec<PartFault>,
    strong_found: Vec<PartFault>,
    rail: Option<&str>,
) {
    let weak: Vec<String> = found.iter().map(|fault| fault.0.clone()).collect();
    for fault in found {
        record_fault_or_note(by_part, unbounded, label, fault, rail);
    }
    for fault in strong_found {
        if !weak.contains(&fault.0) {
            record_fault_or_note(by_part, unbounded, strong_label, fault, rail);
        }
    }
}

/// Remember an unbounded part, its nets, and the first rail named for it.
fn note_unbounded(unbounded: &mut Unbounded, part: String, nets: Vec<String>, rail: Option<&str>) {
    let entry = unbounded.entry(part).or_default();
    entry.0.extend(nets);
    if entry.1.is_none() {
        entry.1 = rail.map(str::to_owned);
    }
}

/// The source delivering the largest current at rest, past [`UNBOUNDED_RAIL_A`]:
/// the rail an unbounded part sits across. `None` when no source delivers that.
fn source_rail(bound: &BoundBoard, ws: &Workspace, x: &[f64]) -> Option<String> {
    (bound.supplies.iter())
        .filter_map(|leg| {
            let branch = ws
                .layout
                .branch(leg.vsource)
                .and_then(|i| x.get(i).copied())?;
            let draw = -branch * leg.supply.nominal_volts().signum();
            (draw.is_finite() && draw > UNBOUNDED_RAIL_A).then(|| (draw, leg.net_name.clone()))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, net)| net)
}

/// A source delivering more than this (A) at rest, while a part's current is
/// unbounded, is taken to be the one that part sits across: the largest draw
/// wins, so a quiet rail beside it is never named.
const UNBOUNDED_RAIL_A: f64 = 1.0;

/// The warning for a part whose only stress is physically unbounded: a diode
/// or a short straight across an ideal source gives a current no part or
/// supply sees, so the figure is not a measurement and is never printed
/// ([`hauksbee_bind::stress::unbounded_note`]). Warning severity, not serious:
/// the simulation cannot tell a real flaw (no series resistor, fitted the
/// wrong way) from a wrong model, but it must not let the board read clean.
fn unbounded_finding(
    (part, (mut nets, rail)): (String, (Vec<String>, Option<String>)),
) -> LintFinding {
    nets.sort();
    nets.dedup();
    LintFinding {
        check: LintCheck::DcOverstress,
        severity: Severity::Medium,
        message: unbounded_note(&part, rail.as_deref()),
        refs: vec![part],
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
/// absurd is a fault, worded without the figure: [`stress_clause`].)
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
/// A part past only an assumed floor (its rating unstated, its card its own)
/// goes to `unstated` instead, with the most it sees and the floor.
fn note_stand_in(
    into: &mut BTreeMap<String, String>,
    unstated: &mut BTreeMap<String, (f64, f64)>,
    events: Vec<StandInExceedance>,
) {
    for e in events {
        let c = &e.fault.component;
        let part = strip_unit_suffix(c.split(':').next().unwrap_or(c)).to_string();
        if e.stand_in.kind == StandInKind::Unstated {
            let seen = unstated
                .entry(part)
                .or_insert((e.fault.value, e.fault.limit));
            seen.0 = seen.0.max(e.fault.value);
        } else {
            into.entry(part).or_insert_with(|| e.condition());
        }
    }
}

/// One note for every part whose rated voltage nothing states and which sees
/// more than the floor assumed for it: a condition the pass rests on, listed
/// once for the board, never a fault and never a partial judgement (the part
/// is its own card; only its rating is unknown). `unstated` maps each part to
/// the most it sees and the assumed floor (V).
fn unstated_rating_note(
    unstated: &BTreeMap<String, (f64, f64)>,
    reported: &[&String],
) -> Option<LintFinding> {
    let parts: BTreeMap<&String, &(f64, f64)> = (unstated.iter())
        .filter(|(part, _)| !reported.contains(part))
        .collect();
    if parts.is_empty() {
        return None;
    }
    let refs: Vec<String> = parts.keys().map(|p| (*p).clone()).collect();
    let seen: Vec<String> = (parts.iter())
        .map(|(part, (v, floor))| {
            format!(
                "{part} sees {}, past the {} assumed",
                format_volts(v),
                format_volts(floor)
            )
        })
        .collect();
    Some(LintFinding {
        check: LintCheck::DcOverstress,
        severity: Severity::Low,
        message: format!(
            "{}: {UNSTATED_RATING_MARKER}, so the lowest rating a part of its class is sold \
             with is assumed and its real rating is not checked ({}). State each rating in \
             the value (for example 10uF 25V) or give the manufacturer part number\
             {INFORMATIONAL_MARKER}.",
            hauksbee_ir::vocabulary::names_summary(&refs),
            hauksbee_ir::vocabulary::list_summary(&seen, "; "),
        ),
        refs,
        nets: Vec::new(),
    })
}

/// Keep the first non-finite value computed for each part, worded as a clause.
fn note_absurd(into: &mut BTreeMap<String, String>, events: Vec<FaultEvent>) {
    for f in events {
        let part = strip_unit_suffix(f.component.split(':').next().unwrap_or(&f.component));
        into.entry(part.to_string())
            .or_insert_with(|| format!("{} came out as no finite number", f.kind.as_str()));
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
    unbounded: &mut Unbounded,
) -> BTreeMap<String, PartFaults> {
    let mut conditional = BTreeMap::new();
    if set_unknown_state_loads(bound, true) {
        if let Ok(x) = settle(bound) {
            for (part, clauses, nets, is_unbounded) in judge(bound, &x).0 {
                // A stand-in limit passed only here is no more a fault than at
                // rest; the monitor already kept it out of the trips.
                if at_rest.contains_key(&part) {
                    continue;
                }
                if is_unbounded {
                    note_unbounded(
                        unbounded,
                        part,
                        nets,
                        source_rail(bound, &x.0, &x.1).as_deref(),
                    );
                } else {
                    conditional.insert(part, (vec![UNKNOWN_STATE], clauses, nets));
                }
            }
        }
        set_unknown_state_loads(bound, false);
    }
    conditional
}

/// The parts over a rating in `bound`'s current state with each rail of
/// `over` held to what its supply can give ([`rest_supply`]), and the stated
/// condition of each part past a stand-in card's limit there, the rails then
/// put back. `None` when the held board does not solve.
fn held_to_capability(
    bound: &mut BoundBoard,
    over: &[rest_supply::Overload],
) -> Option<(Vec<String>, BTreeMap<String, String>)> {
    let saved = rest_supply::hold_to_capability(bound, over);
    let real = settle(bound).ok().map(|x| {
        let (still, _, _, stand_in) = judge(bound, &x);
        let mut conditions = BTreeMap::new();
        // A part past only an assumed floor is no part of `found`, so it has no
        // held condition to give here; its unstated-rating note is the unheld
        // judge's (which saw the same part at no less), kept by the caller.
        let mut unstated = BTreeMap::new();
        note_stand_in(&mut conditions, &mut unstated, stand_in);
        (
            still.into_iter().map(|(part, ..)| part).collect(),
            conditions,
        )
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
/// of the last solve, or why there is no operating point to judge.
fn settle(bound: &mut BoundBoard) -> Result<(Workspace, Vec<f64>), Unsettled> {
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
        solve_dc(&mut ws, &mut *bound)?;
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
            return Ok((ws, x));
        }
    }
    // Still moving (an oscillator, say): judge the last solved state anyway.
    solve_dc(&mut ws, &mut *bound)?;
    let x = ws.x.clone();
    Ok((ws, x))
}

/// Why a state's DC gave no operating point to judge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unsettled {
    /// No rung of the solver's DC ladder converged.
    NoSolve,
    /// The ladder ended on its relaxed power-on point (every diode a 1 GΩ
    /// stand-in) for want of a root of the whole circuit. A junction that
    /// point leaves forward-biased never carried the current its own
    /// equation gives there, so a rating judged at it would report a figure
    /// (hundreds of amps, on a working board) that is the solve's, not the
    /// circuit's.
    StandIn,
}

/// One DC operating-point solve of `bound` into `ws`, accepted only when it
/// is a root of the whole circuit ([`Unsettled`] otherwise). A failed solve
/// is retried through the junction homotopy
/// ([`hauksbee_bind::junctions::dc_with_junction_homotopy`]).
fn solve_dc(ws: &mut Workspace, bound: &mut BoundBoard) -> Result<(), Unsettled> {
    // The solve refuses every point that is not a root; the workspace still
    // says whether the first refusal was the relaxed stand-in.
    let opts = SolverOptions::default();
    let mut relaxed = false;
    let solved = dc_operating_point(ws, &bound.circuit, &opts).or_else(|_| {
        relaxed = ws.relaxed_seed_adopted();
        dc_with_junction_homotopy(ws, &mut bound.circuit, &opts, None)
    });
    solved.map_err(|_| {
        if relaxed {
            Unsettled::StandIn
        } else {
            Unsettled::NoSolve
        }
    })
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
             {at} it conducts with |VGS| = {}, below the {} maximum VGS(th) of \
             its class, so a part nearer {} is not guaranteed to turn on: put the MPN on \
             {reference} to remove this assumption, or drive its gate to at least {}",
            format_volts(vgs),
            format_volts(th),
            others.join(" or "),
            format_volts(spec)
        ),
        refs: vec![reference.to_string()],
        nets,
    })
}

/// What one settled state shows: the parts held past a rating there, the
/// gates conducting under-driven, the values too large to be physical, and
/// the limits passed that were a stand-in card's, not the part's own.
type Judged = (
    Vec<PartFault>,
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
            let unbounded = worst.values().all(|(f, ..)| f.is_unbounded());
            let clauses = worst
                .into_values()
                .map(|(f, ..)| {
                    let rated = rating_phrase(f, trips.contains(f.component.as_str()));
                    // Far past the rating the figure is no result, nor are the
                    // currents behind it: the one stress wording drops both.
                    let detail = if f.is_far_past_rating() {
                        String::new()
                    } else {
                        rating_detail(bound, &device_of, &net_of, f, bjt(f), &mut nets)
                    };
                    stress_clause(&f.component, f.kind, f.value, f.limit, rated, &detail)
                })
                .collect();
            (part, clauses, nets, unbounded)
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
        format!(
            " (base {}, collector {})",
            stress_quantity(FaultKind::Overcurrent, ib),
            stress_quantity(FaultKind::Overcurrent, ic)
        )
    } else if f.kind == FaultKind::GateOvervoltage {
        gate_source_nets(bound, device_of, net_of, f, nets)
    } else if f.kind == FaultKind::Overvoltage
        && f.value <= f.limit
        && device_of
            .get(f.component.as_str())
            .and_then(|id| bound.circuit.devices.get(id.0 as usize))
            .is_some_and(|d| matches!(d, hauksbee_ir::Device::Diode { .. }))
    {
        // Raised by the monitor's breakdown rule: the junction clamps at its
        // blocking rating, so the reading is the clamp, not what is applied.
        " (in reverse breakdown: it conducts at its rating instead of blocking, \
         so the circuit applies more than it can hold)"
            .to_string()
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
        assert!(f.message.contains("3 A assumed"), "{}", f.message);
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
    /// The figure is the ideal source's, not the part's, and the board may
    /// really lack a series resistor or have the part the wrong way round, so
    /// the finding is a warning (never serious, never clean) that names both
    /// possibilities and never the number. Legal control: the 22R case prints
    /// its real figure and stays serious.
    #[test]
    fn an_unbounded_current_is_a_warning_without_the_number() {
        let report = diode_at("1N4148", "0.0001R");
        let found = findings_on(&report, "D1");
        assert_eq!(found.len(), 1, "{:#?}", report.findings);
        assert_eq!(found[0].severity, Severity::Medium, "{}", found[0].message);
        let m = &found[0].message;
        assert_eq!(*m, hauksbee_bind::stress::unbounded_note("D1", Some("+5V")));
        assert_eq!(
            m,
            "D1 sits directly across +5V with nothing limiting its current: either it is fitted \
             the wrong way, it needs a series resistor, or its model is wrong. The simulated \
             current is unbounded, so no figure is shown."
        );
        assert!(
            !m.contains("not physical") && !m.contains("far more than"),
            "{m}"
        );
        // The only digits are those in the part's and the rail's names.
        let bare = m.replace("D1", "").replace("+5V", "");
        assert!(!bare.chars().any(|c| c.is_ascii_digit()), "{m}");
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

    /// A cell-balancing leg, as on a battery board: +5V through a 1N4148
    /// into /K, and /K through 1k to ground, so the diode carries 4.3 mA.
    /// With `astable`, the same circuit shares its DC with a loop that has no
    /// self-consistent DC state (a switch that discharges the very input
    /// that closes it), so the solver's ladder finds no root of the whole
    /// circuit and ends on its relaxed power-on point, every diode a 1 GΩ
    /// stand-in: there /K sits at 0 V and D1's own equation, read at 5 V
    /// forward, gives kiloamps it never carried. Returns the report and
    /// whether the solve adopted that relaxed point.
    fn balancing_leg(astable: bool) -> (NetLintReport, bool) {
        use hauksbee_ir::{Circuit, DiodeModel};
        use hauksbee_testkit::circuits::{cap, diode, res, sw, vdc, GND};
        let nets = [(1, "GND"), (2, "+5V"), (3, "/K")];
        let parts = vec![
            part("D1", "1N4148", &[("A", 2), ("K", 3)]),
            part("R1", "1k", &[("1", 3), ("2", 1)]),
        ];
        let mut bound = bind_board(&board(&nets, parts), &ModelLibrary::builtin());
        if astable {
            let c: &mut Circuit = &mut bound.circuit;
            let (vs, integ, cmp) = (c.node("~VS"), c.node("~INTEG"), c.node("~CMP"));
            let (a, out) = (c.node("~A"), c.node("~OUT"));
            vdc(c, "~V5", vs, 5.0);
            sw(c, "~Sk", vs, cmp, integ, (2.5075, 2.4925), 1.0);
            res(c, "~Rpd", cmp, GND, 10e3);
            res(c, "~Rcharge", cmp, a, 1e3);
            diode(c, "~Dstretch", a, out, DiodeModel::default());
            cap(c, "~Cs", out, GND, 10e-12);
            res(c, "~Rstretch", out, GND, 150e3);
            res(c, "~Rint", vs, integ, 100e3);
            cap(c, "~Cint", integ, GND, 1e-9);
            sw(c, "~Sreset", integ, GND, out, (1.75, 1.25), 6.0);
        }
        let mut ws = Workspace::new(&bound.circuit);
        let solved = dc_operating_point(&mut ws, &bound.circuit, &SolverOptions::default());
        let relaxed = solved.is_err() && ws.relaxed_seed_adopted();
        (dc_stress_findings(bound), relaxed)
    }

    /// Faulty solve, legal board: when the DC ends on the relaxed stand-in,
    /// the kiloamp figure D1 would read there is the solve's, not the
    /// circuit's, so the check abstains with a not-judged note saying so and
    /// raises nothing on D1, where it used to raise a serious "not physical"
    /// overstress (the MNT Reform motherboard's eight balancing diodes).
    /// Legal control: the same leg on a DC that solves is silent, and the
    /// near-zero-resistance diode still reads, as an unbounded-current note.
    #[test]
    fn a_figure_read_off_the_relaxed_stand_in_is_not_judged() {
        let (report, relaxed) = balancing_leg(true);
        assert!(relaxed, "fixture drift: the DC found a root");
        assert!(
            findings_on(&report, "D1").is_empty(),
            "{:#?}",
            report.findings
        );
        assert!(
            !(report.findings.iter())
                .any(|f| matches!(f.severity, Severity::High | Severity::Medium)),
            "{:#?}",
            report.findings
        );
        let note = (report.findings.iter())
            .find(|f| rating_basis::is_not_judged(&f.message))
            .unwrap_or_else(|| panic!("no not-judged note: {:#?}", report.findings));
        assert_eq!(note.severity, Severity::Low);
        assert!(note.is_abstention(), "{}", note.message);
        assert!(
            note.message.contains("every diode switched off") && !note.message.contains(" A "),
            "{}",
            note.message
        );
        let (solved, relaxed) = balancing_leg(false);
        assert!(!relaxed);
        assert!(solved.findings.is_empty(), "{:#?}", solved.findings);
        let unbounded = diode_at("1N4148", "0.0001R");
        let found = findings_on(&unbounded, "D1");
        assert_eq!(found.len(), 1, "{:#?}", unbounded.findings);
        assert_eq!(found[0].severity, Severity::Medium, "{}", found[0].message);
    }

    /// A diode clamping a net that only an unmodelled part (no model, left
    /// open) drives: nothing on the board sets the clamped net, so no current
    /// is the circuit's and nothing serious is said about the diode.
    #[test]
    fn a_diode_on_a_net_only_an_unmodelled_part_drives_is_not_serious() {
        let nets = [(1, "GND"), (2, "+5V"), (3, "/CLAMP")];
        let parts = vec![
            part("D1", "1N4148", &[("A", 2), ("K", 3)]),
            part("U1", "ZQX-NOSUCHPART-9", &[("1", 3), ("2", 1), ("3", 2)]),
        ];
        let report = dc_stress_lint(&board(&nets, parts), &ModelLibrary::builtin());
        assert!(
            !report.findings.iter().any(|f| f.severity == Severity::High),
            "{:#?}",
            report.findings
        );
    }

    /// A `SI7461DP` P-FET switching +30V onto a `load` resistor, its gate net `/G`
    /// carrying pin 4 of an XC6206 regulator (a pin its card does not list)
    /// and, when `pull` is set, a 10k pull-down that sets the gate's level.
    fn gate_on_unmodelled_pin(load: &str, pull: bool) -> NetLintReport {
        let nets = [
            (1, "GND"),
            (2, "+30V"),
            (3, "/OUT"),
            (4, "/G"),
            (5, "+5V"),
            (6, "+3V3"),
        ];
        let fet = [
            ("1", 2),
            ("2", 2),
            ("3", 2),
            ("4", 4),
            ("5", 3),
            ("6", 3),
            ("7", 3),
            ("8", 3),
        ];
        let mut parts = vec![
            part("Q1", "SI7461DP", &fet),
            part("R1", load, &[("1", 3), ("2", 1)]),
            part(
                "U1",
                "XC6206P332MR",
                &[("1", 1), ("2", 6), ("3", 5), ("4", 4)],
            ),
        ];
        if pull {
            parts.push(part("R2", "10k", &[("1", 4), ("2", 1)]));
        }
        dc_stress_lint(&board(&nets, parts), &ModelLibrary::builtin())
    }

    /// Faulty as solved, clean as built: a FET gate whose net carries only a
    /// pin the DC model gives no behaviour is not read as 0 V. Its gate-source
    /// rating and the conduction path it would close are not judged (no
    /// overstress, no unbounded-current or near-short warning), and one
    /// low-severity abstention names the pin, its card and the gate net.
    #[test]
    fn a_gate_driven_only_by_an_unmodelled_pin_abstains_with_a_note() {
        use hauksbee_bind::control_inputs::UNDETERMINED_GATE_MARKER;
        for load in ["1R", "1k"] {
            let report = gate_on_unmodelled_pin(load, false);
            assert!(
                !(report.findings.iter())
                    .any(|f| matches!(f.severity, Severity::High | Severity::Medium)),
                "{load}: {:#?}",
                report.findings
            );
        }
        let report = gate_on_unmodelled_pin("1R", false);
        assert!(
            !(report.findings.iter())
                .any(|f| f.message.contains(hauksbee_bind::stress::UNBOUNDED_MARKER)),
            "{:#?}",
            report.findings
        );
        let note = (report.findings.iter())
            .find(|f| f.message.contains(UNDETERMINED_GATE_MARKER))
            .unwrap_or_else(|| panic!("no undetermined-gate note: {:#?}", report.findings));
        assert_eq!(note.severity, Severity::Low);
        assert!(note.is_abstention(), "{}", note.message);
        assert_eq!(note.refs, vec!["Q1".to_string()]);
        assert_eq!(note.nets, vec!["/G".to_string()]);
        assert!(
            note.message.contains("U1 pin 4") && note.message.contains("xc6206"),
            "{}",
            note.message
        );
    }

    /// The contrast: the same gate with a pull-down on its net has a level
    /// the board sets, so it is judged as before: 30 V across a 20 V gate is
    /// a serious finding, and no undetermined-gate note is made.
    #[test]
    fn a_gate_with_a_pull_resistor_is_still_judged() {
        use hauksbee_bind::control_inputs::UNDETERMINED_GATE_MARKER;
        let report = gate_on_unmodelled_pin("1k", true);
        let on = findings_on(&report, "Q1");
        assert!(
            on.iter()
                .any(|f| f.severity == Severity::High && f.message.contains("gate-source")),
            "{:#?}",
            report.findings
        );
        assert!(
            !(report.findings.iter()).any(|f| f.message.contains(UNDETERMINED_GATE_MARKER)),
            "{:#?}",
            report.findings
        );
    }

    /// A gate a card drives is a known level. The LTC4020's BGATE law holds
    /// the P-FET gate 9.5 V below CSN (on the 30 V source here), so the FET is
    /// judged as driven: no undetermined-gate abstention, and VGS reads
    /// -9.5 V, inside the SI7461DP's +/-20 V, where a 0 V gate would put 30 V
    /// across it.
    #[test]
    fn a_gate_a_card_law_drives_is_judged_at_the_driven_level() {
        use hauksbee_bind::control_inputs::{undetermined_gates, UNDETERMINED_GATE_MARKER};
        let nets = [(1, "GND"), (2, "+30V"), (3, "/OUT"), (4, "/G")];
        let fet = [
            ("1", 2),
            ("2", 2),
            ("3", 2),
            ("4", 4),
            ("5", 3),
            ("6", 3),
            ("7", 3),
            ("8", 3),
        ];
        let parts = vec![
            part("Q1", "SI7461DP", &fet),
            part("R1", "1k", &[("1", 3), ("2", 1)]),
            part("U1", "LTC4020", &[("21", 4), ("22", 2), ("23", 2)]),
        ];
        let board = board(&nets, parts);
        let lib = ModelLibrary::builtin();
        let bound = bind_board(&board, &lib);
        assert!(
            undetermined_gates(&bound).is_empty(),
            "{:?}",
            undetermined_gates(&bound)
        );
        let report = dc_stress_lint(&board, &lib);
        assert!(
            !(report.findings.iter()).any(|f| f.message.contains(UNDETERMINED_GATE_MARKER)
                || (f.severity == Severity::High && f.message.contains("gate-source"))),
            "{:#?}",
            report.findings
        );
    }

    /// A load straight across a rail through a 0 ohm link (no series limit),
    /// the rail named `rail`.
    fn straight_across(rail: &str, load: &str) -> NetLintReport {
        let nets = [(1, "GND"), (2, rail), (3, "/MID")];
        let link = part("R1", "0", &[("1", 2), ("2", 3)]);
        let d = part("D1", load, &[("A", 3), ("K", 1)]);
        dc_stress_lint(&board(&nets, vec![link, d]), &ModelLibrary::builtin())
    }

    /// Faulty: the same diode with no series limit on a 5, 12 or 24 V rail
    /// sits directly across an ideal source: its current is the source's, not
    /// the part's, so it is never silent but is the unbounded-current warning
    /// (never a serious claim, never the number) on every rail, naming it. A
    /// generic LED is a stand-in card, so what it says is its stated condition
    /// or the same warning; it is never silent.
    #[test]
    fn no_series_limit_is_an_unbounded_warning_as_the_rail_rises() {
        for rail in ["+5V", "+12V", "+24V"] {
            let own = straight_across(rail, "1N4148");
            let on = findings_on(&own, "D1");
            assert_eq!(on.len(), 1, "{rail}: {:#?}", own.findings);
            assert_eq!(
                on[0].severity,
                Severity::Medium,
                "{rail}: {}",
                on[0].message
            );
            assert_eq!(
                on[0].message,
                hauksbee_bind::stress::unbounded_note("D1", Some(rail))
            );
            let led = straight_across(rail, "LED");
            let on = findings_on(&led, "D1");
            assert!(
                !on.is_empty(),
                "{rail}: LED went silent: {:#?}",
                led.findings
            );
        }
    }

    /// Genuine overstress stays serious and keeps its figure: a 1N4148 fed
    /// through 10R from 5 V carries some 0.4 A, a few times its rating and
    /// nowhere near unbounded, so it is High, names the milliamps and never
    /// says the current is unbounded.
    #[test]
    fn genuine_overstress_stays_serious_with_its_figure() {
        let report = diode_at("1N4148", "10R");
        let on = findings_on(&report, "D1");
        assert!(
            on.iter().any(|f| f.severity == Severity::High),
            "{:#?}",
            report.findings
        );
        assert!(
            on.iter()
                .all(|f| !f.message.contains(hauksbee_bind::stress::UNBOUNDED_MARKER)),
            "{:#?}",
            report.findings
        );
        assert!(on.iter().any(|f| f.message.contains("mA")), "{on:#?}");
    }

    /// The bound that separates a note from a finding: hundreds of amps,
    /// kilowatts on a small part and a junction past 500 C are unbounded;
    /// a few times the rating or a hot but real junction are not.
    #[test]
    fn unbounded_means_hundreds_of_amps_kilowatts_or_a_molten_junction() {
        use hauksbee_bind::stress::is_unbounded;
        assert!(is_unbounded(FaultKind::Overcurrent, 400.0, 0.2));
        assert!(is_unbounded(FaultKind::Overpower, 3.0e3, 0.25));
        assert!(is_unbounded(FaultKind::Overtemperature, 900.0, 150.0));
        assert!(!is_unbounded(FaultKind::Overcurrent, 0.43, 0.2));
        assert!(!is_unbounded(FaultKind::Overpower, 2.0, 0.25));
        assert!(!is_unbounded(FaultKind::Overtemperature, 180.0, 150.0));
        assert!(!is_unbounded(FaultKind::Overcurrent, f64::INFINITY, 0.2));
    }

    /// Faulty: a generic LED straight across a USB 2.0 port's +5V (declared
    /// 0.5 A) through a 0 ohm link. At the solve's rail it is far past its
    /// stand-in rating; held to the 0.5 A the port gives it is some 20x that
    /// stand-in, which a stand-in can be wrong by, so it is the part's stated
    /// condition (not a pass) with the held figure, never hidden and never
    /// called within its ratings. The draw is 12x the port's limit, which is
    /// no near-short, so nothing says one.
    #[test]
    fn a_stand_in_led_across_a_weak_usb_port_is_still_reported() {
        let nets = [(1, "GND"), (2, "+5V"), (3, "/MID")];
        let link = part("R1", "0", &[("1", 2), ("2", 3)]);
        let led = part("D1", "LED", &[("A", 3), ("K", 1)]);
        let mut bound = bind_board(&board(&nets, vec![link, led]), &ModelLibrary::builtin());
        let BoundBoard {
            circuit, supplies, ..
        } = &mut bound;
        let leg = (supplies.iter_mut())
            .find(|l| l.net_name == "+5V")
            .expect("+5V is powered by its name");
        leg.reconfigure(
            circuit,
            hauksbee_bind::PowerSupply::Usb {
                spec: hauksbee_bind::UsbSpec::V5_0_5A,
            },
        );
        let report = dc_stress_findings(bound);
        let on = findings_on(&report, "D1");
        assert!(
            on.iter().any(|f| f.severity != Severity::Low
                || (rating_basis::is_condition(&f.message)
                    && f.message.contains("D1 carries")
                    && f.message.contains("past its"))),
            "{:#?}",
            report.findings
        );
        assert!(
            !(report.findings.iter())
                .any(|f| f.refs.iter().any(|r| r == "D1")
                    && f.message.contains("within its ratings")),
            "{:#?}",
            report.findings
        );
        assert!(
            !(report.findings.iter()).any(|f| f.message.contains("near-short")),
            "{:#?}",
            report.findings
        );
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

    /// `count` 2512 resistors of `value` in parallel straight across a named
    /// +5V rail, whose stiff source is 1 milliohm.
    fn resistors_across_5v(value: &str, count: usize) -> NetLintReport {
        let nets = [(1, "GND"), (2, "+5V")];
        let parts = (1..=count)
            .map(|k| {
                let mut r = part(&format!("R{k}"), value, &[("1", 2), ("2", 1)]);
                r.footprint = "Resistor_SMD:R_2512_6332Metric".into();
                r
            })
            .collect();
        dc_stress_lint(&board(&nets, parts), &ModelLibrary::builtin())
    }

    /// The rest-draw finding of `report`, which must exist.
    fn rest_draw(report: &NetLintReport) -> &LintFinding {
        (report.findings.iter())
            .find(|f| f.message.starts_with(rest_supply::OPENING))
            .unwrap_or_else(|| panic!("no rest-draw finding: {:#?}", report.findings))
    }

    /// Faulty: 5 milliohm (stamped as written) across +5V draws some 830 A
    /// with the rail still at about 4.2 V, well above half its named level.
    /// The draw is a near-short, so no figure of it is printed, and the rail
    /// is up, so no dead-rail note. Legal control: 30 ohm is quiet.
    #[test]
    fn a_near_short_draw_prints_no_figure_and_the_rail_is_still_up() {
        let report = resistors_across_5v("0.005", 1);
        let rest = rest_draw(&report);
        assert!(
            rest.message.contains("a near-short") && rest.message.contains("not physical"),
            "{}",
            rest.message
        );
        assert!(
            !rest.message.contains("833") && !rest.message.contains(" mW"),
            "{}",
            rest.message
        );
        assert!(
            !(report.findings.iter()).any(|f| f.message.contains("pulled down")),
            "{:#?}",
            report.findings
        );
        let fine = resistors_across_5v("30", 1);
        assert!(fine.findings.is_empty(), "{:#?}", fine.findings);
    }

    /// Faulty: two 1 milliohm resistors in parallel (0.5 milliohm, stamped as
    /// written) across +5V hold the rail at about 1.7 V, clearly under half
    /// its named 5 V. The dead-rail note names the near-short that pulled it
    /// down, not a missing source, and the draw prints no figure.
    #[test]
    fn a_rail_a_near_short_pulls_down_says_so() {
        let report = resistors_across_5v("0.001", 2);
        let rest = rest_draw(&report);
        assert!(
            rest.message.contains("a near-short") && !rest.message.contains("3333"),
            "{}",
            rest.message
        );
        let dead = (report.findings.iter())
            .find(|f| rating_basis::is_not_judged(&f.message))
            .unwrap_or_else(|| panic!("no dead-rail note: {:#?}", report.findings));
        assert!(
            dead.message.contains("pulled down to 1.6")
                && dead.message.contains("by a near-short on it")
                && !dead.message.contains("no declared source"),
            "{}",
            dead.message
        );
        assert!(!dead.message.contains("-0.00"), "{}", dead.message);
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
        assert!(far(FaultKind::Overvoltage, 2.0e4, 3.3));
        assert!(far(FaultKind::Overcurrent, 600.0, 0.0));
        assert!(!far(FaultKind::Overcurrent, 6.0, 0.0));
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
