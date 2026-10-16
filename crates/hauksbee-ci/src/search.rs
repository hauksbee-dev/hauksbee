//! Outcome-driven fault finding: state what the board is for, search the
//! operating conditions for where it is not.
//!
//! Two assertion kinds state an intended OUTCOME rather than a voltage window:
//!
//! - `logic_level`: a net reaches a valid logic level at every logic input it
//!   drives. The level to reach is not typed into the spec: it is the highest
//!   VIH (or, for `level = "low"`, the lowest VIL) any part on the net declares,
//!   read at that part's own solved supply. With `min_width_ms` the level must
//!   be held that long, in at least `min_toggles` separate pulses: the value
//!   judged is the highest level the net holds over any window that wide.
//! - `fires`: a comparator's output rises (`min_toggles` times, default once),
//!   and, with a `target`, trips at that input level within `tolerance`.
//!
//! A RELATION compares grid points instead of judging each alone:
//! `increases` / `decreases` hold that a quantity (a comparator `ref`'s firing
//! rate, or a `net`'s mean level) rises (falls) `along` one swept condition.
//! Members that differ only in that condition form a series; each neighbouring
//! pair is judged, and the violating pairs are reported with both values. A
//! relation is not culprit-ranked: a 10% nudge moves a value, not the sign of
//! a response, so an inverted one is a topology finding.
//!
//! All are read off every accepted solver step of the nets they judge (see
//! `Scheduler::watch_steps`), because a 100 ns spike or the instant a
//! comparator decides falls between chunk-end samples.
//!
//! A `[search]` block names the conditions to search, one `[[search.sweep]]`
//! per net (forced volts or injected amps); every combination runs, crossed
//! with any tolerance ensemble. A violated outcome reports its WORST member
//! with the condition that produced it, then ranks culprits: the worst member
//! reruns once per nearby part with that part nudged +10%, the parts that
//! move the outcome most are listed, and the strongest passive is solved for
//! the value that meets the intent (a secant on its log value, each step a
//! rerun). A switch whose threshold the outcome follows 1:1 is named as a
//! self-limiting loop. Nothing here knows any board: parts are found from the
//! netlist around the judged nets and their bound devices.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use hauksbee_engine::checks::logic_levels::LogicLoad;
use hauksbee_extract::ExtractedBoard;
use hauksbee_ir::{Device, NodeId, SourceKind};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::assertions::AssertResult;
use crate::error::SpecError;
use crate::runner::RunOutcome;
use crate::spec::{Assertion, Spec};
use crate::tolerance::{SampledValue, SeedPlan};
use hauksbee_ir::units::{format_engineering, format_volts};

/// The most members a search grid may expand to (before tolerance crossing).
pub const MAX_GRID: usize = 256;
/// Culprits listed per violated outcome unless `[search] culprits` says.
const DEFAULT_CULPRITS: usize = 3;
/// The nudge each culprit-ranking rerun applies to one part.
const NUDGE: f64 = 1.1;
/// A nudge that moves the measured outcome by less than this (V) is no lead.
const NO_EFFECT_V: f64 = 1e-3;
/// A culprit moving the outcome by less than this fraction of the top one's
/// effect is dropped.
const FLOOR_FRACTION: f64 = 0.05;
/// A switch whose threshold the outcome follows at this many volts per volt
/// or more is ending the pulse it is driven by.
const FOLLOWS: f64 = 0.4;
/// Secant reruns spent solving the strongest passive's fix value.
const SOLVE_STEPS: usize = 4;

/// The `[search]` block: the operating conditions every outcome must hold
/// across.
///
/// ```toml
/// [search]
/// culprits = 3
/// [[search.sweep]]
/// net   = "V_SET"
/// volts = [0.0, 2.0]
/// [[search.sweep]]
/// net  = "I_IN"
/// amps = [4e-6, 6e-6]
/// ```
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Search {
    /// The conditions to search, one entry per net. Every combination runs
    /// (a grid), crossed with any tolerance ensemble.
    #[serde(default)]
    pub sweep: Vec<Sweep>,
    /// How many culprit parts to rank under each violated outcome (default
    /// 3; 0 skips the ranking reruns).
    #[serde(default)]
    pub culprits: Option<usize>,
}

/// One searched net: forced to each of `volts`, or fed each of `amps`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Sweep {
    /// The net the condition is applied to.
    pub net: String,
    /// Hold the net at each of these voltages (an ideal source to ground).
    #[serde(default)]
    pub volts: Vec<f64>,
    /// Inject each of these currents into the net from ground (amps;
    /// negative draws current out).
    #[serde(default)]
    pub amps: Vec<f64>,
}

impl Search {
    /// Every problem with the block, as messages.
    #[must_use]
    pub fn problems(&self) -> Vec<String> {
        let mut out = Vec::new();
        for s in &self.sweep {
            let net = &s.net;
            if s.volts.is_empty() == s.amps.is_empty() {
                out.push(format!(
                    "[[search.sweep]] on '{net}' needs exactly one of `volts` or `amps`"
                ));
            }
            if s.volts.iter().chain(&s.amps).any(|v| !v.is_finite()) {
                out.push(format!(
                    "[[search.sweep]] on '{net}': every value must be finite"
                ));
            }
        }
        let grid = self.grid_size();
        if grid > MAX_GRID {
            out.push(format!(
                "[search] expands to {grid} combinations (cap {MAX_GRID}); sweep fewer values"
            ));
        }
        out
    }

    fn grid_size(&self) -> usize {
        let len = |s: &Sweep| s.volts.len().max(s.amps.len()).max(1);
        self.sweep.iter().map(len).product()
    }
}

/// One applied search condition: a net held at `value` volts, or fed `value`
/// amps.
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub net: String,
    pub value: f64,
    pub amps: bool,
}

impl Condition {
    /// `V_SET=2V` / `LOAD_I=6uA`: the net's last path segment.
    #[must_use]
    pub fn describe(&self) -> String {
        let unit = if self.amps { "A" } else { "V" };
        let leaf = self.net.rsplit('/').next().unwrap_or(&self.net);
        format!("{leaf}={}{unit}", format_engineering(self.value, ""))
    }
}

/// The kinds this module evaluates.
#[must_use]
pub fn is_outcome(a: &Assertion) -> bool {
    matches!(a.kind.as_str(), "logic_level" | "fires")
}

/// The kinds judged across grid points rather than per member.
#[must_use]
pub fn is_relation(a: &Assertion) -> bool {
    matches!(a.kind.as_str(), "increases" | "decreases")
}

/// One member's value of a relation's quantity: the comparator `ref`'s firing
/// rate (rising output edges per second of window), or the `net`'s
/// time-averaged level (V).
fn quantity(a: &Assertion, out: &RunOutcome) -> Option<f64> {
    let nets = a
        .reference
        .as_ref()
        .and_then(|r| out.probe.comparators.get(r));
    let s = window(a, out, nets.map(|c| &c.out).or(a.net.as_ref())?);
    let span = s.last()?.0 - s.first()?.0;
    let total = nets.map_or_else(
        || {
            s.windows(2)
                .map(|w| 0.5 * (w[0].1 + w[1].1) * (w[1].0 - w[0].0))
                .sum()
        },
        |c| {
            let rises = s
                .windows(2)
                .filter(|w| w[0].1 < c.mid && w[1].1 >= c.mid)
                .count();
            f64::from(u32::try_from(rises).unwrap_or(u32::MAX))
        },
    );
    (span > 0.0).then(|| total / span)
}

/// Judge a relation over the grid's members.
///
/// `skip` drops an untrustworthy member. Every neighbouring pair along the swept net, with all other
/// conditions held, must move in the intended sense. The worst violating
/// pair is reported first; both members of every violating pair fail.
#[must_use]
pub fn relation(
    a: &Assertion,
    outcomes: &[RunOutcome],
    skip: &dyn Fn(&RunOutcome) -> bool,
) -> AssertResult {
    let shell = AssertResult::shell(a.label(), a.kind.clone(), outcomes.len());
    let along = a.along.as_deref().unwrap_or_default();
    let unit = if a.reference.is_some() { "Hz" } else { "V" };
    let subject = a.reference.as_deref().or(a.net.as_deref()).map_or("", leaf);
    let sense = if a.kind == "increases" { 1.0 } else { -1.0 };
    // Members that differ only in `along` form one series.
    let mut series: HashMap<String, Vec<(&Condition, f64, u32)>> = HashMap::new();
    for o in outcomes.iter().filter(|o| !skip(o)) {
        let at = o.probe.conditions.iter().find(|c| c.net == along);
        let (Some(at), Some(v)) = (at, quantity(a, o)) else {
            continue;
        };
        let rest = o.probe.conditions.iter().filter(|c| c.net != along);
        let mut key: Vec<String> = rest.map(Condition::describe).collect();
        key.push(crate::tolerance::describe_values(&o.sampled_values));
        key.retain(|k| !k.is_empty());
        let key = key.join(", ");
        series.entry(key).or_default().push((at, v, o.seed));
    }
    let mut judged = 0;
    let mut bad = Vec::new();
    for (key, mut s) in series {
        s.sort_by(|x, y| x.0.value.total_cmp(&y.0.value));
        for w in s.windows(2) {
            judged += 1;
            let margin = sense * (w[1].1 - w[0].1);
            if margin <= 0.0 {
                bad.push((margin, w[0], w[1], key.clone()));
            }
        }
    }
    bad.sort_by(|x, y| x.0.total_cmp(&y.0));
    let Some((margin, (lo, v_lo, _), (hi, v_hi, _), key)) = bad.first() else {
        let detail = if judged == 0 {
            format!("no two members along {} measured {subject}", leaf(along))
        } else {
            format!(
                "{subject} {} along {} over {judged} pair(s)",
                a.kind,
                leaf(along)
            )
        };
        let passed = judged > 0;
        return AssertResult {
            passed,
            detail,
            ..shell
        };
    };
    let value = |v: f64| format!("{} {unit}", format_engineering(v, ""));
    let mut detail = format!(
        "{subject} is {} at {} but {} at {}; {} of {judged} neighbouring pair(s) violate",
        value(*v_lo),
        lo.describe(),
        value(*v_hi),
        hi.describe(),
        bad.len()
    );
    if !key.is_empty() {
        let _ = write!(detail, " [with {key}]");
    }
    let intended = if sense > 0.0 { "rise" } else { "fall" };
    let why = if *margin < 0.0 {
        format!(
            "{subject} moves against the intended sense: it should {intended} as {} rises, \
             so the response is inverted (a sign or topology fault, not a margin; a \
             relation is not culprit-ranked, since a nudge moves a value, not a sign)",
            leaf(along)
        )
    } else {
        format!(
            "{subject} does not change with {}: the condition never reaches it",
            leaf(along)
        )
    };
    let failing_seeds: Vec<u32> = bad.iter().flat_map(|b| [b.1 .2, b.2 .2]).collect();
    AssertResult {
        detail,
        why: Some(why),
        failing_seed: failing_seeds.first().copied(),
        failing_seeds,
        ..shell
    }
}

/// Cross every plan with every combination of the search grid, renumbering
/// members so each (plan, condition) pair has its own seed.
#[must_use]
pub fn cross(plans: Vec<SeedPlan>, search: Option<&Search>) -> Vec<SeedPlan> {
    let Some(search) = search.filter(|s| !s.sweep.is_empty()) else {
        return plans;
    };
    let mut grid: Vec<Vec<Condition>> = vec![Vec::new()];
    for s in &search.sweep {
        let (values, amps) = if s.amps.is_empty() {
            (&s.volts, false)
        } else {
            (&s.amps, true)
        };
        let extend = |g: &Vec<Condition>| {
            let one = |&value: &f64| {
                let mut g = g.clone();
                g.push(Condition {
                    net: s.net.clone(),
                    value,
                    amps,
                });
                g
            };
            values.iter().map(one).collect::<Vec<_>>()
        };
        grid = grid.iter().flat_map(extend).collect();
    }
    let member = |p: &SeedPlan| {
        let with = |g: &Vec<Condition>| SeedPlan {
            conditions: g.clone(),
            ..p.clone()
        };
        grid.iter().map(with).collect::<Vec<_>>()
    };
    let crossed = plans.iter().flat_map(member);
    (0u32..)
        .zip(crossed)
        .map(|(seed, p)| SeedPlan { seed, ..p })
        .collect()
}

/// A comparator's judged nets and its output mid-swing.
#[derive(Debug, Clone)]
pub struct ComparatorNets {
    pub out: String,
    pub inp: String,
    pub inn: String,
    pub mid: f64,
}

/// What one member recorded for the outcome kinds.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// The search condition this member ran at.
    pub conditions: Vec<Condition>,
    /// Every accepted step, `(t_s, volts)`, of each judged net.
    pub steps: HashMap<String, Vec<(f64, f64)>>,
    /// `fires` subjects, by reference.
    pub comparators: HashMap<String, ComparatorNets>,
    /// The logic inputs on each `logic_level` net.
    pub loads: HashMap<String, Vec<LogicLoad>>,
    /// Voltage-controlled switches by control net: `(reference, threshold)`.
    pub switches: HashMap<String, Vec<(String, f64)>>,
    /// Two-terminal R/C/L parts by reference, with their bound value: the
    /// parts culprit ranking may nudge.
    pub passives: HashMap<String, f64>,
    /// Culprit lines per violated outcome label (worst member only).
    pub culprits: HashMap<String, Vec<String>>,
}

/// The board reference that owns a device name: the longest reference the
/// name equals or extends with a non-alphanumeric separator.
fn owner<'a>(name: &str, refs: &'a [String]) -> Option<&'a String> {
    let owns = |r: &&String| {
        name.strip_prefix(r.as_str())
            .is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_alphanumeric()))
    };
    refs.iter().filter(owns).max_by_key(|r| r.len())
}

/// Post-bind, pre-engine: apply one member's search condition and nudges.
///
/// Injects the member's current conditions, nudges its switch thresholds, and
/// records which nets the outcome kinds judge. Returns the probe (without
/// steps) and the nets to watch every step.
pub fn prepare(
    spec: &Spec,
    board: &ExtractedBoard,
    bound: &mut hauksbee_engine::binder::BoundBoard,
    plan: &SeedPlan,
) -> (Probe, Vec<String>) {
    let refs: Vec<String> = board
        .components
        .iter()
        .map(|c| c.reference.clone())
        .collect();
    for c in plan.conditions.iter().filter(|c| c.amps) {
        if let Some(node) = bound.node(&c.net).filter(|n| !n.is_ground()) {
            let name = format!("Ici_search_{}", c.net);
            let src = Device::isource(name, NodeId::GROUND, node, SourceKind::Dc(c.value));
            bound.circuit.add(src);
        }
    }
    for dev in &mut bound.circuit.devices {
        if let Device::VSwitch {
            name, von, voff, ..
        } = dev
        {
            let mine = |(r, _): &&(String, f64)| owner(name, &refs) == Some(r);
            if let Some((_, k)) = plan.perturb.iter().find(mine) {
                *von *= k;
                *voff *= k;
            }
        }
    }
    let name_of = |n: NodeId| bound.circuit.node_name(n).to_string();
    let mut probe = Probe {
        conditions: plan.conditions.clone(),
        ..Probe::default()
    };
    let mut counts: HashMap<&String, usize> = HashMap::new();
    for dev in &bound.circuit.devices {
        if let Some(r) = owner(dev.name(), &refs) {
            *counts.entry(r).or_default() += 1;
        }
    }
    for dev in &bound.circuit.devices {
        let Some(r) = owner(dev.name(), &refs) else {
            continue;
        };
        match dev {
            Device::VSwitch {
                ctrl_p, von, voff, ..
            } => {
                let at = probe.switches.entry(name_of(*ctrl_p)).or_default();
                at.push((r.clone(), 0.5 * (von + voff)));
            }
            Device::Comparator {
                out,
                inp,
                inn,
                out_lo,
                out_hi,
                ..
            } => {
                let nets = ComparatorNets {
                    out: name_of(*out),
                    inp: name_of(*inp),
                    inn: name_of(*inn),
                    mid: 0.5 * (out_lo + out_hi),
                };
                probe.comparators.insert(r.clone(), nets);
            }
            Device::Resistor { ohms: v, .. }
            | Device::Capacitor { farads: v, .. }
            | Device::Inductor { henries: v, .. }
                if counts.get(r) == Some(&1) =>
            {
                probe.passives.insert(r.clone(), *v);
            }
            _ => {}
        }
    }
    let wanted: HashSet<&str> = spec
        .asserts
        .iter()
        .filter(|a| a.kind == "fires" || is_relation(a))
        .filter_map(|a| a.reference.as_deref())
        .collect();
    probe.comparators.retain(|r, _| wanted.contains(r.as_str()));
    let mut watch: Vec<String> = spec
        .asserts
        .iter()
        .filter(|a| a.kind == "logic_level" || is_relation(a))
        .filter_map(|a| a.net.clone())
        .collect();
    for c in probe.comparators.values() {
        watch.extend([c.out.clone(), c.inp.clone(), c.inn.clone()]);
    }
    watch.sort();
    watch.dedup();
    (probe, watch)
}

/// After the run: the judged nets' steps, and the logic loads on each
/// `logic_level` net read at the run's rail levels.
pub fn finish<S: std::hash::BuildHasher>(
    probe: &mut Probe,
    spec: &Spec,
    board: &ExtractedBoard,
    lib: &hauksbee_models::ModelLibrary,
    steps: &HashMap<String, Vec<(f64, f64)>, S>,
    rail_v: &dyn Fn(&str) -> Option<f64>,
) {
    probe.steps = steps.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let nets = spec
        .asserts
        .iter()
        .filter(|a| a.kind == "logic_level")
        .filter_map(|a| a.net.as_ref());
    for net in nets {
        let loads = hauksbee_engine::checks::logic_levels::loads_on_net(board, lib, net, rail_v);
        probe.loads.insert(net.clone(), loads);
    }
}

/// One member's reading of one outcome.
#[derive(Debug, Clone)]
pub struct Measure {
    pub passed: bool,
    /// Signed headroom (V): negative is a violation; the worst member has the
    /// least.
    pub margin: f64,
    /// The quantity a fix drives to zero (V): the margin, or the trip error
    /// against a `target`.
    pub error: f64,
    /// The measured level (V) culprit nudges are compared on.
    pub measured: f64,
    pub detail: String,
    pub why: Option<String>,
}

impl Measure {
    const fn refused(detail: String) -> Self {
        Self {
            passed: false,
            margin: f64::NEG_INFINITY,
            error: f64::NAN,
            measured: f64::NAN,
            detail,
            why: None,
        }
    }
}

/// The steps of `net` at or after the assertion's `after_ms`.
fn window<'a>(a: &Assertion, out: &'a RunOutcome, net: &str) -> &'a [(f64, f64)] {
    let from = a.after_ms.unwrap_or(0.0) / 1000.0;
    let s = out.probe.steps.get(net).map_or(&[][..], Vec::as_slice);
    &s[s.partition_point(|&(t, _)| t < from)..]
}

fn leaf(net: &str) -> &str {
    net.rsplit('/').next().unwrap_or(net)
}

/// Read one outcome assertion off one member.
#[must_use]
pub fn measure(a: &Assertion, out: &RunOutcome) -> Measure {
    match a.kind.as_str() {
        "logic_level" => logic_level(a, out),
        _ => fires(a, out),
    }
}

fn logic_level(a: &Assertion, out: &RunOutcome) -> Measure {
    let net = a.net.as_deref().unwrap_or_default();
    let high = a.level.as_deref() != Some("low");
    let s = window(a, out, net);
    if s.is_empty() {
        return Measure::refused(format!("'{net}' recorded no steps in the window"));
    }
    let loads = out.probe.loads.get(net).map_or(&[][..], Vec::as_slice);
    let pick = |l: &&LogicLoad| if high { l.vih } else { -l.vil };
    let load = loads.iter().max_by(|x, y| pick(x).total_cmp(&pick(y)));
    let edge = if high { "VIH" } else { "VIL" };
    let stated = if high { a.min } else { a.max };
    let (need, whose, part) = match (stated, load) {
        (Some(v), _) => (v, "the spec's level".to_string(), None),
        (None, Some(l)) => {
            let v = if high { l.vih } else { l.vil };
            let vcc = format_volts(l.vcc);
            let at = format!("{} {} {edge} at VCC {vcc}", l.reference, l.role);
            (v, at, Some(l.reference.as_str()))
        }
        (None, None) => {
            return Measure::refused(format!(
                "no powered part on '{net}' declares a logic input, so there is no VIH/VIL to \
                 hold it to; assert a `voltage` window instead"
            ))
        }
    };
    // Judged as a high either way: a low level is the high of the negated net.
    let sign = if high { 1.0 } else { -1.0 };
    let x: Vec<(f64, f64)> = s.iter().map(|&(t, v)| (t, sign * v)).collect();
    let width = a.min_width_ms.unwrap_or(0.0) / 1000.0;
    let pulses = usize::try_from(a.min_toggles.unwrap_or(1)).unwrap_or(usize::MAX);
    let (held, count) = sustained(&x, width, sign * need);
    let reached = sign * held;
    let margin = sign * (reached - need);
    let margin = if count < pulses {
        margin.min(-1e-6)
    } else {
        margin
    };
    let (word, verb) = if high {
        ("high", "peaks at")
    } else {
        ("low", "bottoms at")
    };
    let leaf = leaf(net);
    let (reached_s, need_s) = (format_volts(reached), format_volts(need));
    let detail = if a.min_width_ms.is_none() && a.min_toggles.is_none() {
        format!(
            "{leaf} {verb} {reached_s}; a valid logic {word} at every input it drives \
             needs {need_s} ({whose})"
        )
    } else {
        let peak = sign * sustained(&x, 0.0, f64::INFINITY).0;
        let longest = longest_past(&x, sign * need) * 1e3;
        let peak_s = format_volts(peak);
        format!(
            "{leaf} {verb} {peak_s} and holds {reached_s} for {ms} ms; past {need_s} \
             for at most {longest:.4} ms, in {count} pulse(s) {ms} ms long; a valid logic {word} \
             needs {need_s} held {ms} ms in {pulses} pulse(s) ({whose})",
            ms = a.min_width_ms.unwrap_or(0.0)
        )
    };
    let why = (margin < 0.0).then(|| {
        let who = part.unwrap_or("the next stage");
        let mut w = if count < pulses && sign * (reached - need) >= 0.0 {
            format!("only {count} of the {pulses} pulse(s) {who} needs hold {edge}")
        } else if a.min_width_ms.is_some() {
            format!(
                "{} short of {edge} over the {} ms {who} needs: no pulse is {word} enough \
                 for long enough",
                format_volts(-margin),
                a.min_width_ms.unwrap_or(0.0)
            )
        } else {
            format!(
                "{} short of {edge}: {who} cannot read it as {word}",
                format_volts(-margin)
            )
        };
        let near = |(_, vt): &&(String, f64)| (reached - vt).abs() <= 0.1 * vt.abs().max(1.0);
        let switches = out.probe.switches.get(net).map_or(&[][..], Vec::as_slice);
        if let Some((sw, vt)) = switches.iter().find(near) {
            let _ = write!(
                w,
                "; it stops at {sw}'s {} switching threshold, and {sw} is driven by \
                 this net",
                format_volts(vt)
            );
        }
        w
    });
    Measure {
        passed: margin >= 0.0,
        margin,
        error: margin,
        measured: reached,
        detail,
        why,
    }
}

/// `s` at time `t`, linear between steps and held flat past either end.
fn at(s: &[(f64, f64)], t: f64) -> f64 {
    match s.partition_point(|p| p.0 < t) {
        0 => s[0].1,
        k if k == s.len() => s[k - 1].1,
        k => {
            let ((t0, v0), (t1, v1)) = (s[k - 1], s[k]);
            (v1 - v0).mul_add((t - t0) / (t1 - t0), v0)
        }
    }
}

/// The highest level `s` holds continuously for `width` seconds (the peak
/// when `width` is 0), and how many separate stretches hold `need` that long.
///
/// The window minimum is piecewise linear in the window's start between the
/// starts where either end meets a step, so its maximum is at one of those or
/// where a rising start meets a falling end at equal level.
fn sustained(s: &[(f64, f64)], width: f64, need: f64) -> (f64, usize) {
    let (Some(first), Some(last)) = (s.first(), s.last()) else {
        return (f64::NEG_INFINITY, 0);
    };
    let fits = |t: &f64| (first.0..=last.0 - width).contains(t);
    let mut starts: Vec<f64> = s
        .iter()
        .flat_map(|p| [p.0, p.0 - width])
        .filter(fits)
        .collect();
    starts.sort_by(f64::total_cmp);
    starts.dedup();
    let gap = |t: f64| at(s, t) - at(s, t + width);
    let meets = starts.windows(2).filter_map(|w| {
        let (d0, d1) = (gap(w[0]), gap(w[1]));
        (d0 * d1 < 0.0).then(|| (w[1] - w[0]).mul_add(d0 / (d0 - d1), w[0]))
    });
    let mut starts: Vec<f64> = starts.iter().copied().chain(meets).collect();
    starts.sort_by(f64::total_cmp);
    let low = |t: f64| {
        let inner = s.partition_point(|p| p.0 <= t)..s.partition_point(|p| p.0 < t + width);
        let ends = at(s, t).min(at(s, t + width));
        s.get(inner)
            .unwrap_or_default()
            .iter()
            .fold(ends, |m, p| m.min(p.1))
    };
    let (mut best, mut count, mut inside) = (f64::NEG_INFINITY, 0, false);
    for l in starts.into_iter().map(low) {
        best = best.max(l);
        count += usize::from(l >= need && !inside);
        inside = l >= need;
    }
    (best, count)
}

/// The longest continuous stretch (s) `s` spends at or past `need`, edges
/// interpolated.
fn longest_past(s: &[(f64, f64)], need: f64) -> f64 {
    let (mut best, mut from) = (0.0_f64, None);
    for k in 0..s.len() {
        let above = s[k].1 >= need;
        let cross = || {
            let (t0, v0, t1, v1) = (s[k - 1].0, s[k - 1].1, s[k].0, s[k].1);
            (t1 - t0).mul_add((need - v0) / (v1 - v0), t0)
        };
        match (from, above) {
            (None, true) => from = Some(if k == 0 { s[0].0 } else { cross() }),
            (Some(t0), false) => {
                best = best.max(cross() - t0);
                from = None;
            }
            _ => {}
        }
    }
    let tail = from.map_or(0.0, |t0| s[s.len() - 1].0 - t0);
    best.max(tail)
}

/// The trip level of a comparator's first decision: where +IN first crossed
/// -IN, both inputs read at the interpolated crossing (-IN's opening level if
/// they never cross).
fn trip_level(plus: &[(f64, f64)], minus: &[(f64, f64)], len: usize) -> f64 {
    let diff = |k: usize| plus[k].1 - minus[k].1;
    let first = (1..len).find(|&k| diff(k - 1) <= 0.0 && diff(k) > 0.0);
    first.map_or(minus[0].1, |k| {
        let (before, after) = (diff(k - 1), diff(k));
        let frac = -before / (after - before);
        let at = |s: &[(f64, f64)]| frac.mul_add(s[k].1 - s[k - 1].1, s[k - 1].1);
        0.5 * (at(plus) + at(minus))
    })
}

fn fires(a: &Assertion, out: &RunOutcome) -> Measure {
    let reference = a.reference.as_deref().unwrap_or_default();
    let Some(nets) = out.probe.comparators.get(reference) else {
        return Measure::refused(format!(
            "'{reference}' bound to no comparator model, so there is no output to watch fire"
        ));
    };
    let output = window(a, out, &nets.out);
    let (plus, minus) = (window(a, out, &nets.inp), window(a, out, &nets.inn));
    let len = output.len().min(plus.len()).min(minus.len());
    if len < 2 {
        return Measure::refused(format!("{reference} recorded no steps in the window"));
    }
    let rises: Vec<usize> = (1..len)
        .filter(|&k| output[k - 1].1 < nets.mid && output[k].1 >= nets.mid)
        .collect();
    let edges = rises.len();
    let (peak_at, peak) = (0..len)
        .map(|k| (k, plus[k].1 - minus[k].1))
        .max_by(|x, y| x.1.total_cmp(&y.1))
        .unwrap_or((0, f64::NAN));
    let need = usize::try_from(a.min_toggles.unwrap_or(1)).unwrap_or(usize::MAX);
    if edges < need {
        let margin = peak.min(-1e-6);
        let detail = format!(
            "{reference} fires {edges} time(s) (needs {need}): +IN peaks at {} against \
             -IN {}",
            format_volts(plus[peak_at].1),
            format_volts(minus[peak_at].1)
        );
        let short = -peak.min(0.0);
        // A shortfall below the printed resolution is no shortfall: +IN only
        // reaches -IN (both inputs sit together, as on a dead rail).
        let why = if short < 0.0005 {
            format!(
                "{reference}'s +IN only reaches -IN ({} against {}), with no overdrive to \
                 trip it, so it never fires",
                format_volts(plus[peak_at].1),
                format_volts(minus[peak_at].1)
            )
        } else {
            format!(
                "{reference}'s +IN never rises above -IN: it stops {} short of the trip point",
                format_volts(short)
            )
        };
        return Measure {
            passed: false,
            margin,
            error: margin,
            measured: peak,
            detail,
            why: Some(why),
        };
    }
    let fired = a.target.map_or_else(
        || Measure {
            passed: true,
            margin: peak,
            error: peak,
            measured: peak,
            detail: format!("{reference} fires {edges} time(s)"),
            why: None,
        },
        |target| trips_at(a, reference, edges, target, &plus[..len], &minus[..len]),
    );
    let Some(below) = a.reset_below else {
        return fired;
    };
    let reset = resets(reference, &nets.inp, below, &rises, &plus[..len]);
    if reset.margin < fired.margin {
        Measure {
            detail: format!("{}; {}", fired.detail, reset.detail),
            ..reset
        }
    } else {
        Measure {
            detail: format!("{}; {}", fired.detail, reset.detail),
            ..fired
        }
    }
}

/// How fully the comparator's +IN resets after each firing: the highest
/// level it bottoms at between one rising output edge and the next (or the
/// window end after the last), held below `below`.
fn resets(reference: &str, inp: &str, below: f64, rises: &[usize], plus: &[(f64, f64)]) -> Measure {
    let ends = rises.iter().skip(1).copied().chain([plus.len()]);
    let worst = rises
        .iter()
        .zip(ends)
        .map(|(&from, to)| {
            plus[from..to]
                .iter()
                .map(|p| p.1)
                .fold(f64::INFINITY, f64::min)
        })
        .fold(f64::NEG_INFINITY, f64::max);
    let margin = below - worst;
    let detail = format!(
        "{} resets only to {} between firings; a full reset needs below {}",
        leaf(inp),
        format_volts(worst),
        format_volts(below)
    );
    let why = (margin < 0.0).then(|| {
        format!(
            "{reference}'s reset is partial: +IN ({}) stays {} above the reset level, so \
             each spike starts from a charged input and the firing count overstates the drive",
            leaf(inp),
            format_volts(-margin)
        )
    });
    Measure {
        passed: margin >= 0.0,
        margin,
        error: margin,
        measured: worst,
        detail,
        why,
    }
}

/// A `fires` outcome with a `target`: the first decision's trip level held to
/// `tolerance`.
fn trips_at(
    a: &Assertion,
    reference: &str,
    edges: usize,
    target: f64,
    plus: &[(f64, f64)],
    minus: &[(f64, f64)],
) -> Measure {
    let len = plus.len();
    let trip = trip_level(plus, minus, len);
    let tol = a.tolerance.unwrap_or(0.05) * target.abs();
    let margin = tol - (trip - target).abs();
    let detail = format!(
        "{reference} fires {edges} time(s), first tripping at {}; intended {} \
         +/- {}",
        format_volts(trip),
        format_volts(target),
        format_volts(tol)
    );
    let why = (margin < 0.0).then(|| {
        format!(
            "{reference} trips {} {} the intended level ({:.2}x)",
            format_volts((trip - target).abs()),
            if trip > target { "above" } else { "below" },
            trip / target
        )
    });
    Measure {
        passed: margin >= 0.0,
        margin,
        error: trip - target,
        measured: trip,
        detail,
        why,
    }
}

/// The `(passed, detail, why)` triple `check_seed` returns for an outcome
/// kind, with the member's condition and any culprits ranked on it.
#[must_use]
pub fn check(a: &Assertion, out: &RunOutcome) -> (bool, String, Option<String>) {
    let m = measure(a, out);
    let mut detail = m.detail;
    if !out.probe.conditions.is_empty() {
        let at: Vec<String> = out
            .probe
            .conditions
            .iter()
            .map(Condition::describe)
            .collect();
        let _ = write!(detail, " [at {}]", at.join(", "));
    }
    let mut why = m.why;
    if let (Some(w), Some(lines)) = (why.as_mut(), out.probe.culprits.get(&a.label())) {
        for line in lines {
            let _ = write!(w, "\n          {line}");
        }
    }
    (m.passed, detail, why)
}

/// Nets one hop past `nets` through any part, rails and ground excluded.
fn hop(board: &ExtractedBoard, nets: &HashSet<String>, rails: &HashSet<String>) -> HashSet<String> {
    let name = |id| board.net(id).map(|n| n.name.clone());
    let touches = |c: &&hauksbee_extract::Component| {
        c.pins
            .iter()
            .filter_map(|p| p.net.and_then(name))
            .any(|n| nets.contains(&n))
    };
    let mut out = nets.clone();
    for c in board.components.iter().filter(touches) {
        out.extend(c.pins.iter().filter_map(|p| p.net.and_then(name)));
    }
    out.retain(|n| !rails.contains(n) && !hauksbee_engine::binder::is_ground(n));
    out
}

/// The parts near what `a` judges: the switch on its net, then every bound
/// passive within two hops (not through a rail).
fn candidates(spec: &Spec, board: &ExtractedBoard, a: &Assertion, probe: &Probe) -> Vec<String> {
    let mut nets: HashSet<String> = a.net.iter().cloned().collect();
    if let Some(c) = a.reference.as_ref().and_then(|r| probe.comparators.get(r)) {
        nets.extend([c.out.clone(), c.inp.clone(), c.inn.clone()]);
    }
    let rails: HashSet<String> = (spec.supplies.iter().map(|s| s.net.clone()))
        .chain(spec.net_drives.iter().map(|d| d.net.clone()))
        .chain(spec.suppress_rail.iter().cloned())
        .collect();
    let near = hop(board, &hop(board, &nets, &rails), &rails);
    let name = |id| board.net(id).map(|n| n.name.clone());
    let on_near = |c: &&hauksbee_extract::Component| {
        c.pins
            .iter()
            .filter_map(|p| p.net.and_then(name))
            .any(|n| near.contains(&n))
    };
    let mut out: Vec<String> = nets
        .iter()
        .flat_map(|n| probe.switches.get(n).into_iter().flatten())
        .map(|(r, _)| r.clone())
        .collect();
    out.sort();
    out.dedup();
    let passives = board.components.iter().filter(on_near);
    out.extend(
        passives
            .map(|c| c.reference.clone())
            .filter(|r| probe.passives.contains_key(r)),
    );
    out
}

/// `plan` with part `r` scaled by `k`: a passive's sampled value, or a
/// switch's thresholds.
fn nudged(plan: &SeedPlan, probe: &Probe, r: &str, k: f64) -> SeedPlan {
    let mut p = plan.clone();
    if let Some(&bound) = probe.passives.get(r) {
        let base = plan
            .values
            .iter()
            .find(|v| v.reference == r)
            .map_or(bound, |v| v.si);
        p.values.retain(|v| v.reference != r);
        p.values.push(SampledValue {
            reference: r.to_string(),
            si: base * k,
            nominal_si: base,
            corner: None,
        });
    } else {
        p.perturb.push((r.to_string(), k));
    }
    p
}

/// Runs a batch of member plans and returns their outcomes in order.
pub type RunPlans<'a> = dyn Fn(&[SeedPlan]) -> Result<Vec<RunOutcome>, SpecError> + 'a;

/// Rank culprits for every violated outcome on its worst member, storing the
/// lines on that member's probe for [`check`] to print.
///
/// # Errors
///
/// Whatever a rerun returns.
pub fn rank_culprits(
    spec: &Spec,
    board: &ExtractedBoard,
    plans: &[SeedPlan],
    outcomes: &mut [RunOutcome],
    run: &RunPlans<'_>,
) -> Result<(), SpecError> {
    let limit = spec
        .search
        .as_ref()
        .and_then(|s| s.culprits)
        .unwrap_or(DEFAULT_CULPRITS);
    if limit == 0 {
        return Ok(());
    }
    for a in spec.asserts.iter().filter(|a| is_outcome(a)) {
        let worst = outcomes
            .iter()
            .enumerate()
            .map(|(i, o)| (i, measure(a, o)))
            .filter(|(_, m)| !m.passed && m.margin.is_finite())
            .min_by(|x, y| x.1.margin.total_cmp(&y.1.margin));
        let Some((i, m0)) = worst else { continue };
        let Some(plan) = plans.iter().find(|p| p.seed == outcomes[i].seed) else {
            continue;
        };
        let probe = &outcomes[i].probe;
        let parts = candidates(spec, board, a, probe);
        let batch: Vec<SeedPlan> = parts
            .iter()
            .map(|r| nudged(plan, probe, r, NUDGE))
            .collect();
        let reruns = run(&batch)?;
        let mut moved: Vec<(String, f64)> = parts
            .into_iter()
            .zip(&reruns)
            .map(|(r, o)| (r, measure(a, o).measured - m0.measured))
            .filter(|(_, d)| d.is_finite() && d.abs() >= NO_EFFECT_V)
            .collect();
        moved.sort_by(|x, y| y.1.abs().total_cmp(&x.1.abs()));
        // A part whose effect is a few percent of the strongest one's is
        // coupling noise, not a lead.
        let floor = moved.first().map_or(0.0, |m| FLOOR_FRACTION * m.1.abs());
        moved.retain(|m| m.1.abs() >= floor);
        moved.truncate(limit);
        let lines = explain(a, plan, &outcomes[i].probe, &m0, &moved, run)?;
        outcomes[i].probe.culprits.insert(a.label(), lines);
    }
    Ok(())
}

/// The culprit lines: each part's effect, a switch the outcome follows, and
/// the strongest passive's solved fix.
fn explain(
    a: &Assertion,
    plan: &SeedPlan,
    probe: &Probe,
    m0: &Measure,
    moved: &[(String, f64)],
    run: &RunPlans<'_>,
) -> Result<Vec<String>, SpecError> {
    if moved.is_empty() {
        return Ok(vec![
            "culprits: no nearby part moves this outcome by 1 mV when nudged 10%".to_string(),
        ]);
    }
    let mut lines = Vec::new();
    let mut limiting = None;
    for (rank, (r, d)) in (1..).zip(moved) {
        let what = probe.passives.get(r).map_or_else(
            || {
                let vt = probe.switches.values().flatten().find(|(s, _)| s == r);
                vt.map_or_else(
                    || "threshold".to_string(),
                    |(_, v)| format!("threshold {}", format_volts(v)),
                )
            },
            |v| format_engineering(*v, ""),
        );
        let mut line = format!("culprit {rank}: {r} ({what}): +10% moves it {d:+.3} V");
        if let Some((_, vt)) = probe.switches.values().flatten().find(|(s, _)| s == r) {
            let follows = d / (vt * (NUDGE - 1.0));
            if follows >= FOLLOWS {
                let _ = write!(
                    line,
                    "; the level follows {r}'s switching threshold ({} per V): \
                     the pulse ends once the switch it drives turns on, so it is \
                     self-limiting and set by that threshold, not by its driver. Drive \
                     {r} from a latched, full-swing signal (Schmitt buffer, one-shot)",
                    format_volts(follows)
                );
                limiting.get_or_insert(r.as_str());
            }
        }
        lines.push(line);
    }
    let top = moved.iter().find(|(r, _)| probe.passives.contains_key(r));
    if let Some((r, _)) = top {
        let mut line = solve(a, plan, probe, m0, r, run)?;
        if let Some(sw) = limiting {
            let _ = write!(
                line,
                " [a race, not a margin: a faster edge only outruns {sw}'s turn-on]"
            );
        }
        lines.push(line);
    }
    Ok(lines)
}

/// Secant on the log of `part`'s value to null the outcome's error: the
/// value that meets the intent, checked by a rerun.
fn solve(
    a: &Assertion,
    plan: &SeedPlan,
    probe: &Probe,
    worst: &Measure,
    part: &str,
    run: &RunPlans<'_>,
) -> Result<String, SpecError> {
    let base = probe.passives.get(part).copied().unwrap_or(1.0);
    // A margin is aimed 5% past its boundary so the solved value clears it;
    // a trip error is aimed at the target itself, inside its band.
    let aim = if (worst.error - worst.margin).abs() < 1e-12 {
        0.05 * worst.error.abs()
    } else {
        0.0
    };
    let mut pts = vec![(0.0_f64, worst.error - aim)];
    let mut log_scale = NUDGE.ln();
    let mut last = None;
    for _ in 0..SOLVE_STEPS {
        let ran = run(&[nudged(plan, probe, part, log_scale.exp())])?;
        let Some(reading) = ran.first().map(|o| measure(a, o)) else {
            break;
        };
        pts.push((log_scale, reading.error - aim));
        let passed = reading.passed;
        last = Some((log_scale, reading));
        if passed {
            break;
        }
        let [(x0, e0), (x1, e1)] = [pts[pts.len() - 2], pts[pts.len() - 1]];
        if !(e1 - e0).is_normal() {
            break;
        }
        log_scale = (x1 - e1 * (x1 - x0) / (e1 - e0)).clamp(-4.6, 4.6);
    }
    let Some((log_scale, reading)) = last else {
        return Ok(format!("fix: {part} could not be solved"));
    };
    let value = format_engineering(base * log_scale.exp(), "");
    Ok(if reading.passed {
        format!(
            "fix: {part} {} -> {value} meets the intent (rerun: {})",
            format_engineering(base, ""),
            reading.detail
        )
    } else {
        format!(
            "fix: moving {part} alone does not meet it; nearest tried {value} (rerun: {})",
            reading.detail
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sweep(net: &str, volts: &[f64], amps: &[f64]) -> Sweep {
        Sweep {
            net: net.into(),
            volts: volts.to_vec(),
            amps: amps.to_vec(),
        }
    }

    #[test]
    fn a_reset_is_judged_by_its_highest_bottom_between_firings() {
        // +IN fires at k=1 and k=4; the first gap bottoms at 0.02 V, the tail
        // after the second firing only falls to 0.5 V: a partial reset.
        let plus: Vec<(f64, f64)> = [0.0, 0.8, 0.02, 0.6, 0.8, 0.5, 0.6]
            .into_iter()
            .enumerate()
            .map(|(k, v)| (f64::from(u8::try_from(k).unwrap()), v))
            .collect();
        let partial = resets("U1", "/a/INTEG", 0.1, &[1, 4], &plus);
        assert!(!partial.passed);
        assert!((partial.measured - 0.5).abs() < 1e-12);
        assert!(partial.detail.contains("INTEG resets only to 500 mV"));
        assert!(partial.why.unwrap().contains("reset is partial"));
        let full = resets("U1", "/a/INTEG", 0.1, &[1], &plus[..3]);
        assert!(full.passed && full.why.is_none(), "{}", full.detail);
    }

    #[test]
    fn a_held_level_is_the_best_window_minimum_and_counts_separate_pulses() {
        // Two trapezoids: 0-4 V over 1 us, flat 2 us, down over 1 us; then a
        // 3 V spike with no flat top.
        let s = [
            (0.0, 0.0),
            (1.0, 4.0),
            (3.0, 4.0),
            (4.0, 0.0),
            (5.0, 0.0),
            (6.0, 3.0),
            (7.0, 0.0),
        ];
        for (width, need, held, count) in [
            (0.0, 2.5, 4.0, 2), // the peak; both pulses pass 2.5 V
            (2.0, 2.5, 4.0, 1), // only the flat top holds for 2
            (3.0, 2.5, 2.0, 0), // a 3-wide window reaches halfway down each ramp
            (1.0, 3.5, 4.0, 1), // the spike holds 1.5 V for 1, under 3.5 V
        ] {
            let (h, n) = sustained(&s, width, need);
            assert!((h - held).abs() < 1e-9, "width {width}: held {h}");
            assert_eq!(n, count, "width {width}, need {need}");
        }
        // Past 2 V: the first pulse from 0.5 to 3.5, the spike from 5.67 to 6.33.
        assert!((longest_past(&s, 2.0) - 3.0).abs() < 1e-9);
        assert!(longest_past(&s, 5.0).abs() < 1e-12);
    }

    #[test]
    fn the_grid_crosses_every_plan_with_every_condition_and_renumbers_seeds() {
        let search = Search {
            sweep: vec![
                sweep("/a/V_SET", &[0.0, 2.0], &[]),
                sweep("I_IN", &[], &[4e-6, 6e-6, 8e-6]),
            ],
            culprits: None,
        };
        let interior = SeedPlan {
            interior: true,
            ..SeedPlan::default()
        };
        let crossed = cross(vec![SeedPlan::default(), interior], Some(&search));
        assert_eq!(crossed.len(), 12);
        assert!(crossed.iter().map(|p| p.seed).eq(0..12));
        assert_eq!(crossed.iter().filter(|p| p.interior).count(), 6);
        let at: Vec<String> = crossed[5]
            .conditions
            .iter()
            .map(Condition::describe)
            .collect();
        assert_eq!(at, ["V_SET=2V", "I_IN=8uA"]);
        // An empty search leaves the plans alone.
        let none = cross(vec![SeedPlan::default()], Some(&Search::default()));
        assert_eq!(none.len(), 1);
    }

    #[test]
    fn a_sweep_needs_exactly_one_finite_axis_and_a_bounded_grid() {
        let bad = Search {
            sweep: vec![
                sweep("A", &[1.0], &[1e-6]),
                sweep("B", &[f64::NAN], &[]),
                sweep("C", &[], &[]),
            ],
            culprits: None,
        };
        let problems = bad.problems();
        assert_eq!(problems.len(), 3, "{problems:?}");
        let big = Search {
            sweep: vec![sweep("A", &[0.0; 17], &[]), sweep("B", &[0.0; 16], &[])],
            culprits: None,
        };
        assert!(big.problems().iter().any(|p| p.contains("cap 256")));
    }

    /// A value that rounds to zero prints as zero, never "-0.000 V" (a dead
    /// rail's comparator read "stops -0.000 V short"); real values keep sign.
    #[test]
    fn volts_never_print_negative_zero() {
        assert_eq!(super::format_volts(-0.0), "0 V");
        assert_eq!(super::format_volts(-1e-15), "0 V");
        assert_eq!(super::format_volts(0.0004), "400 uV");
        assert_eq!(super::format_volts(-0.267), "-267 mV");
        assert_eq!(super::format_volts(0.4191), "419.1 mV");
    }
}
