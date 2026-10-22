//! Sourced operating-envelope checks.
//!
//! The checker derives conservative DC rail intervals from declared supplies,
//! modelled converter outputs, and assembly state, then compares connected pin
//! roles with model-authored operating limits. Unknown rail authority remains
//! unknown; it is never replaced by a nominal voltage merely to produce a
//! verdict. Findings retain the limit, inferred range, model source, and any
//! uncertainty that raised their severity.
//!
//! On solved levels, a rail that never reached its parts' minimum supply
//! because their own card-stated static draw drops more than the headroom
//! across a series feed resistor is reported once, as a starved rail naming
//! the resistor, the feed and the drop, in place of one finding per part.
//!
//! Design rationale: `docs/how-and-why/hauksbee-engine/checks.md`.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_plain_inductor, is_resistor, Component, ExtractedBoard, LintCheck,
    LintFinding, NetLintReport, Severity, REMEDY_MARKER,
};
use hauksbee_ir::evidence::{Assumption, ModelUncertainty};
use hauksbee_models::derating::{temp_span, TempRef};
use hauksbee_models::rating_basis::{self, Limit};
use hauksbee_models::value::parse_value;
use hauksbee_models::{
    abs_max_citation, ComponentKind, EnvelopeSeverity, ModelEntry, ModelLibrary, OperatingEnvelope,
};

use super::BoardStatics;
use hauksbee_bind::binder::{is_ground, power_rail_voltage, resolve};
use hauksbee_ir::units::{format_amps, format_percent, format_series, format_volts};
use hauksbee_models::netclass::unreadable_rail_name;

/// The complete DC interval a net can present during normal operation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RailRange {
    pub min_v: f64,
    pub max_v: f64,
}

impl RailRange {
    const fn point(volts: f64) -> Self {
        Self {
            min_v: volts,
            max_v: volts,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum RailKnowledge {
    Known(RailRange),
    BatteryWithoutChemistry,
    Unknown,
}

pub(crate) const fn finding_severity(severity: EnvelopeSeverity) -> Severity {
    match severity {
        EnvelopeSeverity::Serious => Severity::High,
        EnvelopeSeverity::Medium => Severity::Medium,
        EnvelopeSeverity::Note => Severity::Low,
    }
}

/// `f`, a finding that `m`'s part passed its card's `limit`, worded as the
/// stated condition at low severity when that limit is a placeholder or
/// guessed card's, not the part's own ([`rating_basis`]): a tool or model
/// limit is never a board fault. `subject` says what was seen and `rating`
/// what it was held to. A note already at low severity is left as it is.
pub(crate) fn stated_if_stand_in(
    mut f: LintFinding,
    m: &ModelEntry,
    limit: Limit,
    subject: &str,
    rating: &str,
) -> LintFinding {
    if let Some(s) = m.ratings.basis.stand_in_for(limit) {
        if f.severity != Severity::Low {
            let part = f.refs.first().cloned().unwrap_or_default();
            f.severity = Severity::Low;
            f.message = rating_basis::condition(&part, s, subject, rating);
        }
    }
    f
}

/// `severity`, lowered to `cap` when it is more severe.
const fn at_most(severity: Severity, cap: Severity) -> Severity {
    const fn rank(s: Severity) -> u8 {
        match s {
            Severity::Low => 0,
            Severity::Medium => 1,
            Severity::High => 2,
        }
    }
    if rank(severity) > rank(cap) {
        cap
    } else {
        severity
    }
}

const fn raised_one_band(severity: Severity) -> Severity {
    match severity {
        Severity::Low => Severity::Medium,
        Severity::Medium | Severity::High => Severity::High,
    }
}

pub(crate) fn role_net_id(comp: &Component, model: &ModelEntry, role: &str) -> Option<i64> {
    model
        .pins
        .iter()
        .filter(|(_, known)| known.eq_ignore_ascii_case(role))
        .find_map(|(pad, _)| {
            comp.pins
                .iter()
                .find(|pin| pin.number == *pad)
                .and_then(|pin| pin.net)
                .filter(|id| *id != 0)
        })
}

/// Whether a card's output range is a span the output can be SET across, not a
/// regulation band: a rail never regulates over a spread this wide.
fn is_span(range: RailRange) -> bool {
    range.min_v > 0.0 && range.max_v > range.min_v * 1.25
}

fn model_output_range(model: &ModelEntry) -> Option<RailRange> {
    match (
        model.params.get_f64("vout_setpoint_low"),
        model.params.get_f64("vout_setpoint_high"),
    ) {
        (Some(min_v), Some(max_v)) if min_v.is_finite() && max_v.is_finite() && min_v <= max_v => {
            return Some(RailRange { min_v, max_v });
        }
        _ => {}
    }
    model
        .behavioral
        .converter
        .as_ref()
        .map(|converter| RailRange::point(converter.vout_setpoint))
        .or_else(|| model.params.get_f64("vout").map(RailRange::point))
}

/// A fitted two-terminal resistor between nets `a` and `b` (either way
/// round): its ohms and the fractional tolerance it states, `0` when it states
/// none. `None` when the part is not one.
fn resistor_between(c: &Component, a: i64, b: impl Fn(i64) -> bool) -> Option<(f64, f64)> {
    if !AssemblyState::of(c).is_present() || !is_resistor(c) {
        return None;
    }
    let nets: Vec<i64> = c.pins.iter().filter_map(|pin| pin.net).collect();
    let [first, second] = nets[..] else {
        return None;
    };
    let joins = (first == a && b(second)) || (second == a && b(first));
    let ohms = parse_value(&c.value).map(|value| value.si)?;
    (joins && ohms.is_finite() && ohms > 0.0)
        .then(|| (ohms, super::stated_tolerance(c).unwrap_or(0.0)))
}

/// The parallel combination of resistors given as (ohms, tolerance): the
/// lowest and highest it can read with each resistor at its stated tolerance
/// (both its nominal when none states one). `None` when empty.
fn parallel(parts: impl Iterator<Item = (f64, f64)>) -> Option<Spread> {
    let (mut least_g, mut most_g) = (0.0_f64, 0.0_f64);
    for (ohms, tol) in parts {
        // A resistor at +tol ohms conducts least; at -tol, most.
        least_g += (ohms * (1.0 + tol)).recip();
        most_g += (ohms * (1.0 - tol).max(f64::EPSILON)).recip();
    }
    (least_g > 0.0).then(|| Spread {
        low: most_g.recip(),
        high: least_g.recip(),
    })
}

/// An ohmic value and the range its stated tolerances allow.
#[derive(Debug, Clone, Copy)]
struct Spread {
    low: f64,
    high: f64,
}

/// How a board divider sets an adjustable part's output from its reference.
#[derive(Debug, Clone, Copy)]
enum DividerLaw {
    /// The error amplifier holds FB at `Vref` above ground: `Vref * (1 +
    /// Rtop / Rbottom)`, Rtop from FB to the output.
    Grounded,
    /// The LM317 class (no ground pin): `Vref` sits between OUT and ADJ, and
    /// the ADJ pin's own current flows on through Rbottom: `Vref * (1 +
    /// Rbottom / Rtop) + Iadj * Rbottom`.
    OutReferenced { iadj_a: f64 },
}

/// The two nets of a two-terminal part, when it is one.
fn ends(c: &Component) -> Option<(i64, i64)> {
    let nets: Vec<i64> = c.pins.iter().filter_map(|pin| pin.net).collect();
    let [a, b] = nets[..] else { return None };
    Some((a, b))
}

/// The net a divider from `fb_net` senses: `out_net` itself (or a net tied to
/// it) when a feedback resistor lands there, else the one non-ground net the
/// feedback resistors reach that a fitted inductor joins to `out_net` (a
/// buck's card names its switch node as the output; the rail is past the
/// inductor). `None` when neither is on the board.
fn sensed_net(
    board: &ExtractedBoard,
    tied: &BTreeMap<i64, BTreeSet<i64>>,
    fb_net: i64,
    out_net: i64,
) -> Option<i64> {
    let on_output = |n: i64| n == out_net || tied.get(&out_net).is_some_and(|set| set.contains(&n));
    let grounded = |n: i64| board.net(n).is_some_and(|net| is_ground(&net.name));
    let fitted = |c: &&Component| AssemblyState::of(c).is_present();
    let reached: BTreeSet<i64> = (board.components.iter().filter(fitted))
        .filter(|c| is_resistor(c))
        .filter_map(|c| match ends(c)? {
            (a, b) if a == fb_net => Some(b),
            (a, b) if b == fb_net => Some(a),
            _ => None,
        })
        .filter(|&n| n != fb_net && !grounded(n))
        .collect();
    if reached.iter().any(|&n| on_output(n)) {
        return Some(out_net);
    }
    let past_inductor = |n: i64| {
        (board.components.iter().filter(fitted))
            .filter(|c| is_plain_inductor(c))
            .any(|c| ends(c).is_some_and(|e| e == (out_net, n) || e == (n, out_net)))
    };
    let mut sensed = reached.into_iter().filter(|&n| past_inductor(n));
    let first = sensed.next()?;
    sensed.next().is_none().then_some(first)
}

/// The feedback pin, reference and law of an adjustable part: a converter
/// with a feedback pin, or a regulator card with a `vref` and an `fb`/`adj`
/// pin (a ground pin on the card makes it grounded, see [`DividerLaw`]).
fn adjustable(model: &ModelEntry) -> Option<(&str, f64, DividerLaw)> {
    let converter = model.behavioral.converter.as_ref();
    if let Some(feedback) = converter.and_then(|c| c.feedback.as_ref()) {
        return Some((feedback.pin.as_str(), feedback.vref_v, DividerLaw::Grounded));
    }
    let regulator = model.kind == ComponentKind::Vreg
        && converter.is_none()
        && !model.params.get_bool("shunt").unwrap_or(false);
    let vref = (model.params.get_f64("vref")).filter(|v| regulator && v.is_finite() && *v > 0.0)?;
    let pin = ["fb", "adj"]
        .into_iter()
        .find(|role| model.pins.values().any(|known| known == role))?;
    let law = if model.pins.values().any(|known| known == "gnd") {
        DividerLaw::Grounded
    } else {
        let iadj_a = model.params.get_f64("iadj_a").filter(|a| a.is_finite());
        DividerLaw::OutReferenced {
            iadj_a: iadj_a.unwrap_or(0.0),
        }
    };
    Some((pin, vref, law))
}

/// A regulator's board divider and the rail it sets, with the parts that set
/// it: the evidence a finding needs to name what to change.
#[derive(Debug, Clone)]
pub(crate) struct Divider {
    /// The regulator's reference, `"U2"`, and its value, `"TPS62933"`.
    regulator: String,
    value: String,
    /// The net the divider senses: the rail it sets.
    sensed: i64,
    /// The rail's level across the reference interval and resistor tolerances.
    range: RailRange,
    /// The reference the regulator holds its feedback pin at, in volts.
    vref_v: f64,
    law: DividerLaw,
    /// The fitted resistors of the top leg (feedback pin to the sensed output)
    /// and the bottom leg (feedback pin to ground), each with its ohms.
    top: Vec<(String, f64)>,
    bottom: Vec<(String, f64)>,
    /// True when only the two legs (and capacitors) load the feedback node, so
    /// the divider alone sets the level. A trim resistor, a DAC or any other
    /// part injecting there makes the closed-form level a guess.
    sole: bool,
}

/// The parallel combination of a leg's nominal resistances.
fn leg_ohms(leg: &[(String, f64)]) -> f64 {
    leg.iter()
        .map(|(_, ohms)| ohms.recip())
        .sum::<f64>()
        .recip()
}

impl Divider {
    /// The level at the nominal reference and nominal resistor values.
    fn nominal_v(&self) -> f64 {
        let (top, bottom) = (leg_ohms(&self.top), leg_ohms(&self.bottom));
        match self.law {
            DividerLaw::Grounded => self.vref_v * (1.0 + top / bottom),
            DividerLaw::OutReferenced { iadj_a } => {
                self.vref_v.mul_add(1.0 + bottom / top, iadj_a * bottom)
            }
        }
    }

    /// The top-leg resistance that would set `target_v` with the bottom leg as
    /// fitted, when one exists.
    fn top_for(&self, target_v: f64) -> Option<f64> {
        let bottom = leg_ohms(&self.bottom);
        let ohms = match self.law {
            DividerLaw::Grounded => bottom * (target_v / self.vref_v - 1.0),
            DividerLaw::OutReferenced { iadj_a } => {
                self.vref_v * bottom / iadj_a.mul_add(-bottom, target_v - self.vref_v)
            }
        };
        (ohms.is_finite() && ohms > 0.0).then_some(ohms)
    }
}

/// The rail an adjustable part ([`adjustable`]) sets with the board's own
/// divider, and the net it is on: Rtop from the feedback pin to the sensed
/// output ([`sensed_net`]) and Rbottom from the feedback pin to ground, by the
/// card's law, across the card's stated reference interval when it gives one.
/// `None` when the card is not adjustable or the divider is not on the board,
/// in which case the card's own set point stands.
fn feedback_output_range(
    board: &ExtractedBoard,
    tied: &BTreeMap<i64, BTreeSet<i64>>,
    (comp, model): (&Component, &ModelEntry),
    uncertainty: &[ModelUncertainty],
    out_net: i64,
) -> Option<Divider> {
    let (pin, vref, law) = adjustable(model)?;
    let fb_net = role_net_id(comp, model, pin)?;
    let sensed = sensed_net(board, tied, fb_net, out_net)?;
    let on_output = |n: i64| n == sensed || tied.get(&sensed).is_some_and(|set| set.contains(&n));
    let grounded = |n: i64| board.net(n).is_some_and(|net| is_ground(&net.name));
    let legs = |far: &dyn Fn(i64) -> bool| -> Vec<(String, (f64, f64))> {
        (board.components.iter())
            .filter_map(|c| Some((c.reference.clone(), resistor_between(c, fb_net, far)?)))
            .collect()
    };
    let (top_legs, bottom_legs) = (legs(&on_output), legs(&grounded));
    let top = parallel(top_legs.iter().map(|(_, leg)| *leg))?;
    let bottom = parallel(bottom_legs.iter().map(|(_, leg)| *leg))?;
    let (lo, hi) = uncertainty
        .iter()
        .find_map(|u| match u {
            ModelUncertainty::Interval {
                parameter,
                low,
                high,
                ..
            } if (parameter == "feedback_reference" || parameter == "vref") && low <= high => {
                Some((*low, *high))
            }
            _ => None,
        })
        .unwrap_or((vref, vref));
    // The lowest and highest gain and offset the stated resistor tolerances
    // allow; with none stated they collapse to the nominal divider.
    let ((gain_lo, offset_lo), (gain_hi, offset_hi)) = match law {
        DividerLaw::Grounded => (
            (1.0 + top.low / bottom.high, 0.0),
            (1.0 + top.high / bottom.low, 0.0),
        ),
        DividerLaw::OutReferenced { iadj_a } => (
            (1.0 + bottom.low / top.high, iadj_a * bottom.low),
            (1.0 + bottom.high / top.low, iadj_a * bottom.high),
        ),
    };
    let range = RailRange {
        min_v: lo.mul_add(gain_lo, offset_lo),
        max_v: hi.mul_add(gain_hi, offset_hi),
    };
    let in_divider: BTreeSet<&str> = (top_legs.iter().chain(&bottom_legs))
        .map(|(r, _)| r.as_str())
        .collect();
    // Only the regulator, the two legs and capacitors load the feedback node.
    let sole = (board.net_members(fb_net).into_iter()).all(|(c, _)| {
        c.reference == comp.reference
            || in_divider.contains(c.reference.as_str())
            || is_capacitor(c)
            || !AssemblyState::of(c).is_present()
    });
    let ohms = |legs: Vec<(String, (f64, f64))>| -> Vec<(String, f64)> {
        legs.into_iter().map(|(r, (ohms, _))| (r, ohms)).collect()
    };
    Some(Divider {
        regulator: comp.reference.clone(),
        value: comp.value.trim().to_string(),
        sensed,
        range,
        vref_v: vref,
        law,
        top: ohms(top_legs),
        bottom: ohms(bottom_legs),
        sole,
    })
}

/// Nets joined by a fitted net tie or near-zero-ohm link, each net mapped to
/// every other net of its tied group.
fn tied_nets(board: &ExtractedBoard) -> BTreeMap<i64, BTreeSet<i64>> {
    let is_tie = |c: &Component| {
        let text = format!("{} {}", c.lib_id, c.footprint).to_ascii_lowercase();
        let named_tie = text.contains("nettie")
            || text.contains("net_tie")
            || c.reference
                .strip_prefix("NT")
                .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_digit()));
        let zero_ohm = is_resistor(c)
            && parse_value(&c.value).is_some_and(|v| v.si.is_finite() && v.si <= 0.1);
        AssemblyState::of(c).is_present() && (named_tie || zero_ohm)
    };
    let mut groups: Vec<BTreeSet<i64>> = Vec::new();
    for c in board.components.iter().filter(|c| is_tie(c)) {
        let nets: BTreeSet<i64> = c
            .pins
            .iter()
            .filter_map(|p| p.net)
            .filter(|&n| n != 0)
            .collect();
        if nets.len() < 2 {
            continue;
        }
        let (merged, rest): (Vec<_>, Vec<_>) =
            groups.into_iter().partition(|g| !g.is_disjoint(&nets));
        let mut group = nets;
        for g in merged {
            group.extend(g);
        }
        groups = rest;
        groups.push(group);
    }
    let mut map = BTreeMap::new();
    for g in groups {
        for &n in &g {
            map.insert(n, g.iter().copied().filter(|&o| o != n).collect());
        }
    }
    map
}

/// What the board's parts alone say each net's level is: the regulator and
/// battery rails. It reads no run level, so every ensemble member on this board
/// shares one (see [`BoardStatics`]).
pub(crate) struct Rails {
    regulators: BTreeMap<i64, RailRange>,
    /// The divider-set rails, each the only source its net has.
    dividers: Vec<Divider>,
    batteries: BTreeMap<i64, Option<RailRange>>,
    /// Nets a converter-shaped part drives whose output level no card states
    /// (an unmodelled buck, or a regulator card with no output range), each
    /// with the part, `"U21 (TPS62410)"`.
    unknown_drivers: BTreeMap<i64, String>,
}

impl Rails {
    pub(crate) fn of(board: &ExtractedBoard, st: &BoardStatics<'_>) -> Self {
        let (regulators, dividers) = regulator_ranges(board, st);
        let unknown_drivers = converter_driven_nets(board)
            .into_iter()
            .filter(|(net, _)| !regulators.contains_key(net))
            .collect();
        Self {
            regulators,
            dividers,
            batteries: battery_ranges(board, st),
            unknown_drivers,
        }
    }
}

/// A pin name with `KiCad`'s `_<pad>` suffix and any channel digits removed,
/// upper-cased: `SW1` and `FB_3` read as `SW` and `FB`.
fn pin_base(function: &str) -> String {
    let f = function.trim().to_ascii_uppercase();
    let f = match f.rsplit_once('_') {
        Some((head, tail)) if !head.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => {
            head.to_string()
        }
        _ => f,
    };
    f.trim_end_matches(|c: char| c.is_ascii_digit()).to_string()
}

/// Nets a converter-shaped part drives, judged from pin names alone so it
/// holds whether or not the part bound a card: a part with an input pin
/// (`VIN`, `IN`, `PVIN`) drives the nets of its output and feedback pins
/// (`VOUT`, `OUT`, `FB`), and through an inductor the far net of its switch
/// pin (`SW`, `LX`). Spread across net ties, as the regulated rails are.
fn converter_driven_nets(board: &ExtractedBoard) -> BTreeMap<i64, String> {
    const INPUTS: [&str; 7] = ["IN", "VIN", "VI", "INPUT", "PVIN", "AVIN", "VINA"];
    const OUTPUTS: [&str; 7] = ["OUT", "VOUT", "VO", "OUTPUT", "FB", "VFB", "VOS"];
    const SWITCHES: [&str; 3] = ["SW", "LX", "PH"];
    let has =
        |p: &hauksbee_extract::Pin, names: &[&str]| names.contains(&pin_base(&p.function).as_str());
    let inductor_far = |net: i64| {
        board
            .net_members(net)
            .into_iter()
            .filter_map(move |(c, _)| {
                let is_l = c
                    .reference
                    .strip_prefix('L')
                    .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_digit()));
                let nets: BTreeSet<i64> = c.pins.iter().filter_map(|p| p.net).collect();
                (is_l && AssemblyState::of(c).is_present() && nets.len() == 2)
                    .then(|| nets.into_iter().find(|&n| n != net))
                    .flatten()
            })
    };
    let mut out = BTreeMap::new();
    for comp in &board.components {
        if !AssemblyState::of(comp).is_present() || !comp.pins.iter().any(|p| has(p, &INPUTS)) {
            continue;
        }
        let who = format!("{} ({})", comp.reference, comp.value.trim());
        for pin in &comp.pins {
            let Some(net) = pin.net.filter(|n| *n != 0) else {
                continue;
            };
            let driven: Vec<i64> = if has(pin, &OUTPUTS) {
                vec![net]
            } else if has(pin, &SWITCHES) {
                inductor_far(net).collect()
            } else {
                Vec::new()
            };
            for n in driven {
                if board.net(n).is_some_and(|x| !is_ground(&x.name)) {
                    out.entry(n).or_insert_with(|| who.clone());
                }
            }
        }
    }
    let tied = tied_nets(board);
    let spread: Vec<(i64, String)> = out
        .iter()
        .flat_map(|(net, who)| {
            tied.get(net)
                .into_iter()
                .flatten()
                .map(move |&other| (other, who.clone()))
        })
        .collect();
    for (net, who) in spread {
        out.entry(net).or_insert(who);
    }
    out
}

/// Every regulator's output range by net, and the board dividers that set a
/// rail no other regulator also drives.
fn regulator_ranges(
    board: &ExtractedBoard,
    st: &BoardStatics<'_>,
) -> (BTreeMap<i64, RailRange>, Vec<Divider>) {
    let mut ranges = BTreeMap::new();
    let mut dividers: Vec<Divider> = Vec::new();
    let mut sources: BTreeMap<i64, usize> = BTreeMap::new();
    let tied = tied_nets(board);
    for (comp, model) in st.parts(board) {
        let range = model_output_range(model);
        let output_role = model
            .behavioral
            .converter
            .as_ref()
            .map(|converter| converter.out_pin.as_str())
            .or_else(|| {
                ["out", "vout"]
                    .into_iter()
                    .find(|role| model.pins.values().any(|known| known == role))
            });
        let Some(role) = output_role else { continue };
        if let Some(net_id) = role_net_id(comp, model, role) {
            // The library keeps a card's declared uncertainty on the
            // resolution's provenance, not on the compiled entry.
            let uncertainty = AssemblyState::of(comp)
                .fitted()
                .and_then(|part| resolve(st.lib, part).provenance)
                .map(|source| source.uncertainty().to_vec())
                .unwrap_or_default();
            // The divider's rail lands on the net it senses; a card's own set
            // point stays on its output pin's net (a buck's switch node).
            let feedback = feedback_output_range(board, &tied, (comp, model), &uncertainty, net_id);
            let sensed = feedback.as_ref().map(|d| d.sensed);
            if let Some(divider) = feedback {
                ranges.insert(divider.sensed, divider.range);
                *sources.entry(divider.sensed).or_default() += 1;
                dividers.push(divider);
            }
            // An adjustable part's card states the span its output CAN be set across
            // (a buck's 0.9 to 6 V); that is no level of any rail it drives, and with
            // no divider read it must not pose as one.
            let capability = range.is_some_and(|r| adjustable(model).is_some() && is_span(r));
            if let Some(range) = range
                .filter(|_| sensed.is_none_or(|n| n != net_id))
                .filter(|_| !capability)
            {
                ranges.insert(net_id, range);
                *sources.entry(net_id).or_default() += 1;
            }
        }
    }
    // A regulated net reaches the loads through net ties and zero-ohm links
    // as the same rail: the level is the regulator's, not one the far net's
    // name suggests.
    let spread: Vec<(i64, RailRange)> = ranges
        .iter()
        .flat_map(|(&net, &range)| {
            tied.get(&net)
                .into_iter()
                .flatten()
                .map(move |&other| (other, range))
        })
        .collect();
    for (net, range) in spread {
        ranges.entry(net).or_insert(range);
    }
    // A fitted module's own supply pins (a dev board's 5V/3V3 header), unless
    // a regulator on the board already defines the net.
    for (comp, model) in st.parts(board) {
        for out in &model.supply_outputs {
            if let Some(net_id) = role_net_id(comp, model, &out.role) {
                let range = RailRange {
                    min_v: out.min,
                    max_v: out.max,
                };
                ranges.entry(net_id).or_insert(range);
                *sources.entry(net_id).or_default() += 1;
            }
        }
    }
    // A divider speaks for its rail only when nothing else also sources it
    // (a second regulator, a module's supply pin, even through a net tie).
    let sole_source = |d: &Divider| {
        let group = tied.get(&d.sensed).into_iter().flatten();
        let count = |n: &i64| sources.get(n).copied().unwrap_or(0);
        count(&d.sensed) + group.map(count).sum::<usize>() == 1
    };
    dividers.retain(sole_source);
    (ranges, dividers)
}

fn looks_like_battery_connector(comp: &Component) -> bool {
    let text = format!("{} {} {}", comp.value, comp.lib_id, comp.footprint).to_ascii_lowercase();
    // Match only actual battery connector/holder parts, not chargers that mention "battery".
    // The discriminant is "holder" or specific connector keywords.
    text.contains("batt_holder")
        || text.contains("cell_holder")
        || (text.contains("battery") && (text.contains("holder") || text.contains("clip")))
}

fn battery_floor(chemistry: &str) -> Option<f64> {
    match chemistry.trim().to_ascii_lowercase().as_str() {
        "liion" | "li-ion" | "lipo" | "li-po" => Some(3.0),
        _ => None,
    }
}

/// Battery-class nets and the range their chemistry witness authorizes.
/// `None` means the topology establishes a battery but no chemistry card does.
fn battery_ranges(
    board: &ExtractedBoard,
    st: &BoardStatics<'_>,
) -> BTreeMap<i64, Option<RailRange>> {
    let mut ranges = BTreeMap::new();

    for comp in &board.components {
        if AssemblyState::of(comp).is_present() && looks_like_battery_connector(comp) {
            for pin in &comp.pins {
                let Some(net_id) = pin.net.filter(|id| *id != 0) else {
                    continue;
                };
                if board.net(net_id).is_some_and(|net| !is_ground(&net.name)) {
                    ranges.entry(net_id).or_insert(None);
                }
            }
        }
    }

    for (comp, model) in st.parts(board) {
        // Only a part that manages a cell (a charger or power-path card, or
        // any card that states the chemistry) makes its battery pin a battery
        // net. A chip that merely CONSUMES backup power on a VBAT pin (an
        // MCU's or RTC's backup domain) is routinely strapped to a regulated
        // rail; its pin says nothing about what feeds that net.
        if model.kind != ComponentKind::Vreg && model.params.get_str("battery_chemistry").is_none()
        {
            continue;
        }
        let Some(battery_role) = ["bat", "vbat", "battery"].into_iter().find(|role| {
            model
                .pins
                .values()
                .any(|known| known.eq_ignore_ascii_case(role))
        }) else {
            continue;
        };
        let Some(net_id) = role_net_id(comp, model, battery_role) else {
            continue;
        };
        let range = model
            .params
            .get_str("battery_chemistry")
            .and_then(battery_floor)
            .and_then(|min_v| {
                model_output_range(model).map(|setpoint| RailRange {
                    min_v,
                    max_v: setpoint.max_v,
                })
            });
        let slot = ranges.entry(net_id).or_insert(None);
        if range.is_some() {
            *slot = range;
        }
    }

    ranges
}

pub(crate) struct RailResolver<'a> {
    board: &'a ExtractedBoard,
    /// Levels a run solved (or a spec forced), each the span the net covered.
    solved: &'a BTreeMap<String, RailRange>,
    rails: &'a Rails,
}

impl<'a> RailResolver<'a> {
    pub(crate) fn new(
        board: &'a ExtractedBoard,
        st: &'a BoardStatics<'_>,
        solved: &'a BTreeMap<String, RailRange>,
    ) -> Self {
        Self {
            board,
            solved,
            rails: st.rails(board),
        }
    }

    fn solved(&self, net_id: i64) -> Option<RailRange> {
        self.solved.get(&self.board.net(net_id)?.name).copied()
    }

    /// Every level a net is judged at: the solved span when a run gave it,
    /// plus both corners of its declared range. Empty when neither is known.
    pub(crate) fn corners(&self, net_id: i64) -> Vec<f64> {
        let declared = match self.declared(net_id) {
            RailKnowledge::Known(r) => Some(r),
            _ => None,
        };
        let spans = self.solved(net_id).into_iter().chain(declared);
        let mut corners: Vec<f64> = spans.flat_map(|r| [r.min_v, r.max_v]).collect();
        corners.dedup();
        corners
    }

    fn range(&self, net_id: i64) -> RailKnowledge {
        self.solved(net_id)
            .map_or_else(|| self.declared(net_id), RailKnowledge::Known)
    }

    /// True when the net's level is only the ±5 % band its NAME suggests: no
    /// run solved it, and no regulator, battery or ground identity defines it.
    fn assumed_from_name(&self, net_id: i64) -> bool {
        self.solved(net_id).is_none()
            && !self.rails.regulators.contains_key(&net_id)
            && !self.rails.batteries.contains_key(&net_id)
            && self
                .board
                .net(net_id)
                .is_some_and(|n| !is_ground(&n.name) && power_rail_voltage(&n.name).is_some())
    }

    /// True when the board takes its power from a battery: a battery connector,
    /// holder or a charger's battery pin sits on it. A rail whose level is only
    /// its name's band is then a guess about a converter's output or a cell's
    /// state of charge, not about a fixed supply, so it cannot gate.
    fn battery_fed(&self) -> bool {
        !self.rails.batteries.is_empty()
    }

    /// The part that drives a net whose level is only its name's band, where
    /// that part is a converter with no stated output: the name is then
    /// contradicted by nothing yet confirmed by nothing either, since the
    /// converter sets the real level (a `VCC` buck output is often 3.3 V).
    /// Only a magnitude-free name (`VCC`, `VDD`) qualifies: a converter output
    /// named `+5V` states its level in the name.
    fn unknown_driver(&self, net_id: i64) -> Option<&str> {
        let magnitude_free = self.board.net(net_id).is_some_and(|n| {
            let leaf = n.name.rsplit('/').next().unwrap_or(&n.name);
            !leaf.chars().any(|c| c.is_ascii_digit())
        });
        (magnitude_free && self.assumed_from_name(net_id))
            .then(|| self.rails.unknown_drivers.get(&net_id).map(String::as_str))
            .flatten()
    }

    fn declared(&self, net_id: i64) -> RailKnowledge {
        let Some(net) = self.board.net(net_id) else {
            return RailKnowledge::Unknown;
        };
        if let Some(range) = self.rails.regulators.get(&net_id).copied() {
            return RailKnowledge::Known(range);
        }
        if let Some(range) = self.rails.batteries.get(&net_id) {
            return range
                .map(RailKnowledge::Known)
                .unwrap_or(RailKnowledge::BatteryWithoutChemistry);
        }
        if is_ground(&net.name) {
            return RailKnowledge::Known(RailRange::point(0.0));
        }
        power_rail_voltage(&net.name).map_or(RailKnowledge::Unknown, |nominal| {
            let delta = nominal.abs() * 0.05;
            RailKnowledge::Known(RailRange {
                min_v: nominal - delta,
                max_v: nominal + delta,
            })
        })
    }
}

fn compatible_supply_rail(
    resolver: &RailResolver<'_>,
    current_net: i64,
    min_v: f64,
    max_v: f64,
) -> Option<String> {
    let mut candidates: Vec<_> = resolver
        .board
        .nets
        .iter()
        .filter(|net| net.id != current_net)
        .filter_map(|net| match resolver.range(net.id) {
            RailKnowledge::Known(range) if range.min_v >= min_v && range.max_v <= max_v => {
                Some(net.name.clone())
            }
            _ => None,
        })
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// The low-severity abstention for a rail whose range is not known, cited on
/// the part (`reference`) whose envelope needed it.
fn unknown_abstention(net_name: &str, knowledge: &RailKnowledge, reference: &str) -> LintFinding {
    let message = if matches!(knowledge, RailKnowledge::BatteryWithoutChemistry) {
        format!(
            "operating envelope on net '{net_name}' abstained: the topology identifies a battery net but no charger card declares its chemistry; add battery_chemistry and the sourced charge-voltage range to the charger model"
        )
    } else if let Some(why) = unreadable_rail_name(net_name) {
        // The name looks like it states a voltage; say why no level is taken
        // from it, so a typo'd rail is not mistaken for an unlabelled one.
        format!(
            "operating envelope on net '{net_name}' abstained: its name {why}, so no level is assumed from it; declare the level with a net_drive, or bind the regulator that defines this net, or rename the net to a plain form such as +5V or +3V3"
        )
    } else {
        format!(
            "operating envelope on net '{net_name}' abstained: declare the level with a net_drive, or bind the regulator that defines this net"
        )
    };
    LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity: Severity::Low,
        message,
        refs: vec![reference.to_string()],
        nets: vec![net_name.to_string()],
    }
}

/// A point row (`TA = 25 °C`) binds within this of its temperature: the bench
/// tolerance a room-temperature figure is taken at. Further off, it is a guess.
const POINT_ROW_C: f64 = 5.0;

/// The temperature span row `e` is printed for. A one-sided bound (`TA ≤ 85
/// °C`, `25 °C ≤ TA`) reads as a point at its bound, so it is taken as no span:
/// judged at every temperature, like an unreadable row.
fn span(e: &OperatingEnvelope) -> Option<(TempRef, f64, f64)> {
    let text = e.conditions();
    temp_span(text).filter(|(_, lo, hi)| lo < hi || !text.contains(['≤', '<', '≥', '>']))
}

/// Whether row `e` is guaranteed at `ambient_c`; every row is on a static
/// surface, which knows no temperature. An untagged or unreadable row holds
/// everywhere, and a TC or TJ row covers any temperature from ambient up: the
/// part runs hotter.
fn covers(e: &OperatingEnvelope, ambient_c: Option<f64>) -> bool {
    let Some(ta) = ambient_c else { return true };
    span(e).is_none_or(|(at, lo, hi)| {
        let tol = if lo < hi { 0.0 } else { POINT_ROW_C };
        (at != TempRef::Ambient || lo - tol <= ta) && ta <= hi + tol
    })
}

/// The temperature rows of `e`'s channel. A row whose temperature cannot be
/// read is not one: holding everywhere, it must not stand in for the channel's
/// rows at an ambient none of them covers.
fn rows_of<'a>(
    envelope: &'a [OperatingEnvelope],
    e: &OperatingEnvelope,
) -> impl Iterator<Item = &'a OperatingEnvelope> {
    let (channel, rows) = (e.channel(), envelope.iter());
    rows.filter(move |o| span(o).is_some() && o.channel() == channel)
}

/// The rows judged at `ambient_c`: each one guaranteed there, and every row of
/// a channel none of whose rows is. Guaranteed windows are SELECTED, never
/// interpolated, and judging every binding row makes the verdict the strictest.
pub(crate) fn binding(
    envelope: &[OperatingEnvelope],
    ambient_c: Option<f64>,
) -> impl Iterator<Item = &OperatingEnvelope> {
    let held =
        move |e: &&_| covers(e, ambient_c) || !rows_of(envelope, e).any(|o| covers(o, ambient_c));
    envelope.iter().filter(held)
}

/// A caveat per channel none of whose rows covers `ambient_c`, as run evidence
/// that qualifies the answer.
pub(crate) fn temperature_caveats(
    board: &ExtractedBoard,
    st: &BoardStatics<'_>,
    ambient_c: f64,
) -> Vec<Assumption> {
    let fix = "run the part inside the temperature range the card's rows are printed for, or add the row for this temperature";
    let mut out = Vec::new();
    for (comp, model) in st.parts(board) {
        let (r, envelope) = (comp.reference.as_str(), &model.envelope);
        for e in envelope {
            let first = rows_of(envelope, e).next() == Some(e);
            if !first || rows_of(envelope, e).any(|o| covers(o, Some(ambient_c))) {
                continue;
            }
            let channel = e.channel().unwrap_or_default();
            let printed: Vec<&str> = rows_of(envelope, e)
                .map(OperatingEnvelope::conditions)
                .collect();
            let gap = format!("{r} at {ambient_c:.1} °C ambient is outside its characterised temperature range: its {channel} limit is characterised only at {}, so it is judged against every printed row, none of which the datasheet guarantees there.", printed.join("; "));
            let id = format!("{r}-envelope-temperature-{channel}").replace(' ', "-");
            let caveat = Assumption::partial_model_for_component(&id, r, "", &gap, fix);
            out.push(caveat);
        }
    }
    out
}

/// Run operating-envelope checks using only rail knowledge carried by the
/// board and its bound model cards.
#[must_use]
pub fn envelope_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    envelope_lint_with_drives(board, lib, &BTreeMap::new())
}

/// Run operating-envelope checks with exact spec or forced-run net levels.
#[must_use]
pub fn envelope_lint_with_drives(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    drives: &BTreeMap<String, f64>,
) -> NetLintReport {
    let points = drives
        .iter()
        .map(|(net, v)| (net.clone(), RailRange::point(*v)));
    let st = BoardStatics::of(board, lib);
    let mut report = envelope_lint_with_tolerance(board, &st, &points.collect(), (0.0, 0.0), None);
    report
        .findings
        .extend(divider_name_findings(board, &st, drives));
    report
}

/// A divider-set rail may differ from the level its name states by this
/// fraction before the name and the divider are called inconsistent: the
/// tolerance of a logic or USB rail (±10 %), wide enough that E24 and E96
/// resistor rounding never trips it.
const NAME_TOLERANCE: f64 = 0.10;

/// `1000` as `"1k"`, `100000` as `"100k"`, `30900` as `"30.9k"`.
fn ohms_text(ohms: f64) -> String {
    super::device_decode::format_ohms(ohms)
        .replace(".0k", "k")
        .replace(".0M", "M")
}

/// The one voltage a net name states outright (`+3V3`, `5V`, `VDD_1V8`), or
/// `None` when it states none, several, or is not a positive supply.
fn named_voltage(name: &str) -> Option<f64> {
    let spelled = super::ripple::named_rail_voltage(name)?;
    power_rail_voltage(name)
        .filter(|read| (read - spelled).abs() < 1e-9)
        .map(|_| spelled)
}

/// A rail whose board divider sets a level its own net name contradicts.
///
/// The divider is the only thing that fixes an adjustable regulator's output,
/// and the net name is what the designer meant the rail to be: a `+3V3` net
/// whose divider computes 4.9 V (a resistor mis-valued, swapped or mis-fitted)
/// feeds every part on it a rail none of them was chosen for, and no part on
/// it need carry a supply window for the mistake to be real. Judged only when
/// the divider alone loads the feedback node and no other source drives the
/// rail, and only on a name that states one unambiguous voltage; a bare `VCC`
/// or `VDD` states none, so a buck on it is never second-guessed. Levels the
/// caller forced (`drives`) are the user's statement, not the divider's.
fn divider_name_findings(
    board: &ExtractedBoard,
    st: &BoardStatics<'_>,
    drives: &BTreeMap<String, f64>,
) -> Vec<LintFinding> {
    let tied = tied_nets(board);
    let mut out = Vec::new();
    for divider in st.rails(board).dividers.iter().filter(|d| d.sole) {
        let group = std::iter::once(divider.sensed)
            .chain(tied.get(&divider.sensed).into_iter().flatten().copied());
        let names: Vec<&str> = group
            .filter_map(|n| board.net(n).map(|net| net.name.as_str()))
            .collect();
        if names.iter().any(|n| drives.contains_key(*n)) {
            continue;
        }
        let stated: Vec<(&str, f64)> = names
            .iter()
            .filter_map(|n| Some((*n, named_voltage(n)?)))
            .collect();
        let [(net, want_v)] = stated[..] else {
            continue;
        };
        let band = (
            want_v * (1.0 - NAME_TOLERANCE),
            want_v * (1.0 + NAME_TOLERANCE),
        );
        if divider.range.max_v >= band.0 && divider.range.min_v <= band.1 {
            continue;
        }
        let set = divider.nominal_v();
        let pct = (set / want_v - 1.0).abs() * 100.0;
        let side = if set > want_v { "above" } else { "below" };
        let leg = |leg: &[(String, f64)]| {
            let parts: Vec<String> = leg
                .iter()
                .map(|(r, ohms)| format!("{r} {}", ohms_text(*ohms)))
                .collect();
            parts.join(" || ")
        };
        let (top_refs, bottom_refs): (Vec<&str>, Vec<&str>) = (
            divider.top.iter().map(|(r, _)| r.as_str()).collect(),
            divider.bottom.iter().map(|(r, _)| r.as_str()).collect(),
        );
        let (vref, top, bottom) = (
            divider.vref_v,
            ohms_text(leg_ohms(&divider.top)),
            ohms_text(leg_ohms(&divider.bottom)),
        );
        let law = match divider.law {
            DividerLaw::Grounded => format!("{} x (1 + {top} / {bottom})", format_volts(vref)),
            DividerLaw::OutReferenced { .. } => format!(
                "{} x (1 + {bottom} / {top}) plus the ADJ pin current through {bottom}",
                format_volts(vref)
            ),
        };
        let fix = divider.top_for(want_v).map_or_else(
            || {
                format!(
                    "Re-value the divider ({}) so Vout is {}",
                    top_refs.join(", "),
                    format_volts(want_v)
                )
            },
            |ohms| {
                format!(
                    "Make {} about {} (with {} at {bottom}) so Vout is {}",
                    top_refs.join(" || "),
                    ohms_text(ohms),
                    bottom_refs.join(" || "),
                    format_volts(want_v)
                )
            },
        );
        let message = format!( "{} ({}) sets rail '{net}' to {}, {} {side} the {} its name states: its feedback divider ({} from the output to the feedback pin, {} from it to ground) holds the pin at the {} reference, so Vout = {law} = {}, and every part on '{net}' is supplied {} instead of {}. {REMEDY_MARKER}{fix}, or, if {} is what the board needs, rename the net to say so.",
            divider.regulator,
            divider.value, format_volts(set), format_percent(pct, 0), format_volts(want_v),
            leg(&divider.top),
            leg(&divider.bottom), format_volts(vref), format_volts(set), format_volts(set), format_volts(want_v), format_volts(set));
        let mut refs = vec![divider.regulator.clone()];
        refs.extend(
            divider
                .top
                .iter()
                .chain(&divider.bottom)
                .map(|(r, _)| r.clone()),
        );
        out.push(LintFinding {
            check: LintCheck::OperatingEnvelope,
            severity: Severity::High,
            message,
            refs,
            nets: vec![net.to_string()],
        });
    }
    out
}

// One match over envelope kinds sharing the resolver and per-net abstention bookkeeping.
#[allow(clippy::too_many_lines)]
fn envelope_lint_with_tolerance(
    board: &ExtractedBoard,
    st: &BoardStatics<'_>,
    solved: &BTreeMap<String, RailRange>,
    (reltol, vntol): (f64, f64),
    ambient_c: Option<f64>,
) -> NetLintReport {
    let resolver = RailResolver::new(board, st, solved);
    let ties = tied_nets(board);
    let mut report = NetLintReport::default();
    let mut abstained_nets = BTreeSet::new();
    let mut starved: BTreeMap<i64, Vec<Starved>> = BTreeMap::new();
    let mut unleveled: BTreeMap<i64, Vec<Starved>> = BTreeMap::new();

    for (comp, model) in st.parts(board) {
        let net_of = |role| role_net_id(comp, model, role).and_then(|id| board.net(id));
        // A stand-in card's window is no measure of the part's own supply.
        let own = model.ratings.basis.judges(Limit::Envelope);
        for envelope in binding(&model.envelope, ambient_c) {
            match envelope {
                OperatingEnvelope::SupplyRange {
                    pin,
                    min_v,
                    max_v,
                    abs_max_v,
                    basis,
                    abs_max_basis,
                    ..
                } => {
                    let Some(net) = net_of(pin) else { continue };
                    let net_id = net.id;
                    // An op-amp or comparator card prints its limits as the V+ to V-
                    // span. On the negative supply pin of a single-supply design
                    // (V- on ground) the span is the single-supply case: the
                    // split-supply figures for that pin are no limit on ground.
                    if matches!(model.kind, ComponentKind::Opamp | ComponentKind::Comparator)
                        && hauksbee_models::card_trust::is_negative_supply_role(pin)
                        && *max_v <= 0.0
                        && is_ground(&net.name)
                    {
                        continue;
                    }
                    let range = match resolver.range(net_id) {
                        RailKnowledge::Known(range) => range,
                        knowledge => {
                            if abstained_nets.insert(net_id) {
                                let finding =
                                    unknown_abstention(&net.name, &knowledge, &comp.reference);
                                report.findings.push(finding);
                            }
                            // No level known: a candidate for the static
                            // starved-rail estimate, from the card's draw.
                            if own && matches!(knowledge, RailKnowledge::Unknown) {
                                unleveled.entry(net_id).or_default().push(Starved {
                                    finding: usize::MAX,
                                    reference: comp.reference.clone(),
                                    min_v: *min_v,
                                    draw_a: static_draw_a(model),
                                });
                            }
                            continue;
                        }
                    };
                    let min_tol = reltol.mul_add(min_v.abs(), vntol);
                    let max_tol = reltol.mul_add(max_v.abs(), vntol);
                    if range.min_v >= *min_v - min_tol && range.max_v <= *max_v + max_tol {
                        continue;
                    }
                    let above_abs_max = abs_max_v.is_some_and(|limit| range.max_v > limit);
                    let mut severity = finding_severity(envelope.severity());
                    if above_abs_max {
                        severity = raised_one_band(severity);
                    }
                    // Only a tolerance corner leaves the window: the nominal
                    // level is inside it and nothing reaches abs max. A rail
                    // set at the recommended limit is a deliberate design
                    // choice, so the corner is a margin note, not a fault.
                    let nominal = f64::midpoint(range.min_v, range.max_v);
                    let nominal_inside = nominal >= *min_v - min_tol && nominal <= *max_v + max_tol;
                    // Declared ranges only: a solved span's midpoint is not
                    // a nominal, and a solved excursion is what happened.
                    let declared_only = resolver.solved(net_id).is_none();
                    if declared_only && nominal_inside && !above_abs_max {
                        severity = at_most(severity, Severity::Low);
                    }
                    // A band the net's name implies is an assumption about a
                    // level nothing on the board states: it can flag a margin
                    // to check, not a fault, and it gives no basis for
                    // rewiring the part to another rail.
                    let assumed = resolver.assumed_from_name(net_id);
                    let fix = if let Some(driver) = resolver.unknown_driver(net_id) {
                        severity = at_most(severity, Severity::Low);
                        unknown_driver_note(&net.name, nominal, driver)
                    } else if assumed {
                        // A name-derived level is an assumption. On a
                        // battery-fed board the rail's real level is whatever
                        // the converter or cell behind it makes it, so the
                        // name cannot gate there.
                        let battery = resolver.battery_fed();
                        let cap = if (nominal_inside && !above_abs_max) || battery {
                            Severity::Low
                        } else {
                            Severity::Medium
                        };
                        severity = at_most(severity, cap);
                        let why = if battery {
                            "this board is battery-fed and no regulator or solved run \
                             states this rail's level"
                        } else {
                            "no regulator, battery or solved run states this rail's level"
                        };
                        format!(
                            " The range is assumed: ±5 % around the {} the net name \
                             implies, since {why}; confirm the real supply tolerance.",
                            format_volts(nominal)
                        )
                    } else if declared_only && nominal_inside && !above_abs_max {
                        // The rail's own nominal suits the part: moving the
                        // pin to another rail would change the circuit, while
                        // the margin lives in the rail's set point.
                        format!(
                            " The {} nominal is inside the window; only the rail's \
                             tolerance corner leaves it, so tighten or lower the set point \
                             (feedback divider, regulator grade) or use a part rated for the \
                             full range.",
                            format_volts(nominal)
                        )
                    } else {
                        compatible_supply_rail(&resolver, net_id, *min_v, *max_v)
                            .map(|candidate| {
                                format!(" Tie {pin} to compatible rail '{candidate}'.")
                            })
                            .unwrap_or_default()
                    };
                    // A solved rail that never came up to this part's minimum:
                    // a candidate for one rail-level starvation finding.
                    if own && resolver.solved(net_id).is_some() && range.max_v < *min_v - min_tol {
                        starved.entry(net_id).or_default().push(Starved {
                            finding: report.findings.len(),
                            reference: comp.reference.clone(),
                            min_v: *min_v,
                            draw_a: static_draw_a(model),
                        });
                    }
                    let over_abs = abs_max_v
                        .filter(|_| above_abs_max)
                        .map(|limit| {
                            format!(
                                ", {} above its {} absolute maximum",
                                format_volts(range.max_v - limit),
                                format_volts(limit)
                            )
                        })
                        .unwrap_or_default();
                    let seen = format!( "{} role {pin} on '{}' presents {}, outside the {} operating envelope{over_abs}",
                        comp.reference, net.name, format_series(&[range.min_v, range.max_v], "..", "V"), format_series(&[*min_v, *max_v], "..", "V"));
                    // The window and the absolute maximum come from different
                    // tables: a finding that states the latter cites both.
                    let citation = abs_max_v.filter(|_| above_abs_max).map_or_else(
                        || format!("basis: \"{basis}\""),
                        |limit| abs_max_citation(basis, abs_max_basis, limit),
                    );
                    let finding = LintFinding {
                        check: LintCheck::OperatingEnvelope,
                        severity,
                        message: format!("{seen}; {citation}.{fix}"),
                        refs: vec![comp.reference.clone()],
                        nets: vec![net.name.clone()],
                    };
                    let rating = format!("{} supply", format_series(&[*min_v, *max_v], "..", "V"));
                    report.findings.push(stated_if_stand_in(
                        finding,
                        model,
                        Limit::Envelope,
                        &seen,
                        &rating,
                    ));
                }
                OperatingEnvelope::RailOrder {
                    lower,
                    upper,
                    basis,
                    ..
                } => {
                    let (Some(lower_net), Some(upper_net)) = (net_of(lower), net_of(upper)) else {
                        continue;
                    };
                    // Two pins on ONE net (an EN tied to its own IN, VCCA and
                    // VCCB on one rail) are at the same level by construction:
                    // the order holds with equality, and comparing the net's
                    // tolerance band with itself (max above min) is no order
                    // violation. A fitted net tie or zero-ohm link joins nets
                    // the same way.
                    if lower_net.id == upper_net.id
                        || ties
                            .get(&lower_net.id)
                            .is_some_and(|group| group.contains(&upper_net.id))
                    {
                        continue;
                    }
                    let mut known = Vec::new();
                    for net in [lower_net, upper_net] {
                        match resolver.range(net.id) {
                            RailKnowledge::Known(range) => known.push(range),
                            knowledge => {
                                if abstained_nets.insert(net.id) {
                                    let finding =
                                        unknown_abstention(&net.name, &knowledge, &comp.reference);
                                    report.findings.push(finding);
                                }
                            }
                        }
                    }
                    if known.len() != 2 || known[0].max_v <= known[1].min_v {
                        continue;
                    }
                    let seen = format!( "{} has '{}' at up to {} above '{}' at {}, against its {lower} <= {upper} rail order",
                        comp.reference, lower_net.name, format_volts(known[0].max_v), upper_net.name, format_volts(known[1].min_v));
                    let finding = LintFinding {
                        check: LintCheck::OperatingEnvelope,
                        severity: finding_severity(envelope.severity()),
                        message: format!( "{} requires {lower} <= {upper}, but '{}' reaches {} while '{}' falls to {}, so {} <= {} is false; basis: \"{basis}\". Tie {lower} to a rail no higher than {upper}, or raise {upper} so the full ranges do not cross.",
                            comp.reference,
                            lower_net.name, format_volts(known[0].max_v),
                            upper_net.name, format_volts(known[1].min_v),
                            format_volts(known[0].max_v),
                            format_volts(known[1].min_v)
                        ),
                        refs: vec![comp.reference.clone()],
                        nets: vec![lower_net.name.clone(), upper_net.name.clone()],
                    };
                    let rating = format!("{lower} <= {upper} rail-order");
                    report.findings.push(stated_if_stand_in(
                        finding,
                        model,
                        Limit::Envelope,
                        &seen,
                        &rating,
                    ));
                }
                // Supply-relative input windows need the level ON the input
                // pins, not the rail interval this resolver derives: they are
                // evaluated in `super::analog_inputs`. A supply-independent
                // input ceiling raises the clamp limit `super::back_power`
                // judges a pin against; it rates no rail.
                OperatingEnvelope::InputCommonMode { .. }
                | OperatingEnvelope::InputTolerance { .. } => {}
            }
        }
        report
            .findings
            .extend(supply_abs_max_findings(comp, model, &resolver));
    }
    report.findings.extend(unreadable_name_abstentions(
        board,
        &resolver,
        &abstained_nets,
    ));
    fold_starved_rails(&resolver, starved, &mut report);
    estimate_starved_rails(&resolver, st, &unleveled, &mut report);
    report
}

/// One abstention per supply net whose name seems to state a voltage it does
/// not state readably (`+1KV`, `+1E3V`, `+5V5V`, `+1V8_3V3`) and whose level
/// nothing else defines, on every board, whether or not any part on it has an
/// operating envelope. Without this the same typo is a visible note on a board
/// with an enveloped part and silent on one without. `already` holds the nets
/// a part's envelope has abstained on.
fn unreadable_name_abstentions(
    board: &ExtractedBoard,
    resolver: &RailResolver<'_>,
    already: &BTreeSet<i64>,
) -> Vec<LintFinding> {
    board
        .nets
        .iter()
        .filter(|net| !already.contains(&net.id) && unreadable_rail_name(&net.name).is_some())
        .filter(|net| matches!(resolver.range(net.id), RailKnowledge::Unknown))
        .filter_map(|net| {
            let reference = board
                .components
                .iter()
                .filter(|c| c.pins.iter().any(|p| p.net == Some(net.id)))
                .map(|c| c.reference.as_str())
                .min()?;
            Some(unknown_abstention(
                &net.name,
                &RailKnowledge::Unknown,
                reference,
            ))
        })
        .collect()
}

/// Card pin-map roles that feed an IC's own supply domain: the pins its
/// `ratings.max_voltage_v` (the schema's "IC supply abs max") rates. Regulator
/// and charger inputs (`vin`, `vbus`, `vbat`) are rated by their own rows.
pub(crate) const IC_SUPPLY_ROLES: [&str; 15] = [
    "vcc", "vdd", "vddio", "iovdd", "vio", "vccio", "vcca", "vccb", "dvdd", "avdd", "vdda", "avcc",
    "dvcc", "3v3", "v+",
];

/// Kinds whose `ratings.max_voltage_v` is the supply's absolute maximum
/// (for a regulator it is VIN, for discretes a blocking voltage).
const fn rates_supply(kind: ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::Mcu
            | ComponentKind::Digital
            | ComponentKind::ShiftRegister
            | ComponentKind::Adc
            | ComponentKind::Dac
            | ComponentKind::AnalogSwitch
            | ComponentKind::Opamp
            | ComponentKind::Comparator
            | ComponentKind::Timer
    )
}

/// A supply pin on a rail that can exceed the IC card's absolute-maximum
/// supply (`ratings.max_voltage_v`), for every supply role no `supply_range`
/// row already judges. Exceeding abs max is damage, not a margin: serious,
/// or a warning when the rail's level is only the band its name implies.
fn supply_abs_max_findings(
    comp: &Component,
    model: &ModelEntry,
    resolver: &RailResolver<'_>,
) -> Vec<LintFinding> {
    let Some(abs_max) = model.ratings.max_voltage_v.filter(|v| *v > 0.0) else {
        return Vec::new();
    };
    if !rates_supply(model.kind) {
        return Vec::new();
    }
    let judged: BTreeSet<String> = model
        .envelope
        .iter()
        .filter_map(|e| match e {
            OperatingEnvelope::SupplyRange { pin, .. } => Some(pin.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    let roles: BTreeSet<String> = model
        .pins
        .values()
        .map(|r| r.to_ascii_lowercase())
        .filter(|r| IC_SUPPLY_ROLES.contains(&r.as_str()) && !judged.contains(r))
        .collect();
    let mut out = Vec::new();
    let mut seen_nets = BTreeSet::new();
    for role in roles {
        let Some(net_id) = role_net_id(comp, model, &role) else {
            continue;
        };
        let Some(net) = resolver.board.net(net_id) else {
            continue;
        };
        let RailKnowledge::Known(range) = resolver.range(net_id) else {
            continue;
        };
        if range.max_v <= abs_max || !seen_nets.insert(net_id) {
            continue;
        }
        let assumed = resolver.assumed_from_name(net_id);
        let driver = resolver.unknown_driver(net_id);
        let (severity, level) = if driver.is_some() {
            (Severity::Low, " (the ±5 % band its name implies)")
        } else if assumed && resolver.battery_fed() {
            (
                Severity::Low,
                " (the ±5 % band its name implies; the board is battery-fed and no regulator or run states it)",
            )
        } else if assumed {
            (
                Severity::Medium,
                " (the ±5 % band its name implies; no regulator or run states it)",
            )
        } else {
            (Severity::High, "")
        };
        let over = range.max_v - abs_max;
        let seen = format!( "{} supply pin {role} on '{}' reaches {}{level}, {} above the {} absolute-maximum supply on card '{}'",
            comp.reference, net.name, format_volts(range.max_v), format_volts(over), format_volts(abs_max), model.id
        );
        let message = driver.map_or_else(
            || {
                format!( "{seen}: the part can be damaged. Feed {role} from a rail that stays at or below {} (inside the card's recommended supply), or fit a part rated for this rail.", format_volts(abs_max))
            },
            |driver| {
                let nominal = f64::midpoint(range.min_v, range.max_v);
                format!("{seen}, if the name's level holds.{}", unknown_driver_note(&net.name, nominal, driver))
            },
        );
        let finding = LintFinding {
            check: LintCheck::OperatingEnvelope,
            severity,
            message,
            refs: vec![comp.reference.clone()],
            nets: vec![net.name.clone()],
        };
        let rating = format!("{} absolute-maximum supply", format_volts(abs_max));
        out.push(stated_if_stand_in(
            finding,
            model,
            Limit::Voltage,
            &seen,
            &rating,
        ));
    }
    out
}

/// The note for a rail whose only level is its name's band while a converter
/// with no stated output drives it: the converter, not the name, sets the
/// level, so the comparison is a question to answer, never a fault.
fn unknown_driver_note(net: &str, nominal: f64, driver: &str) -> String {
    format!(
        " The level is not known: only the net name suggests {}, and '{net}' is driven by \
         {driver}, a converter whose output no model states. Declare the rail's level with a \
         net_drive (or give {driver} a card with its output), then re-check.",
        format_volts(nominal)
    )
}

/// How many of a starved rail's parts its finding names before "and N more".
const STARVED_PARTS_SHOWN: usize = 6;

/// A part whose solved supply rail never reached its minimum operating supply.
struct Starved {
    /// Index of the part's own envelope finding in the report.
    finding: usize,
    reference: String,
    min_v: f64,
    /// The part's static supply draw from its card, amps; `0` when it has none.
    draw_a: f64,
}

/// The static supply current a card states for the part, in amps: its
/// operating-mode `supply_static_ua`, else its quiescent `iq_a`. What the part
/// draws once its rail is up, which is what a series feed must carry.
fn static_draw_a(model: &ModelEntry) -> f64 {
    let ua = model.params.get_f64("supply_static_ua").map(|ua| ua * 1e-6);
    ua.or_else(|| model.params.get_f64("iq_a"))
        .filter(|a| a.is_finite() && *a > 0.0)
        .unwrap_or(0.0)
}

/// Resistors joining `net_id` to a higher, declared, non-ground rail, lowest
/// resistance first: each one's reference, ohms and the far net's id and range.
fn series_feeds(
    resolver: &RailResolver<'_>,
    net_id: i64,
    rail: RailRange,
) -> Vec<(String, f64, i64, RailRange)> {
    let board = resolver.board;
    let mut feeds = Vec::new();
    for (comp, _) in board.net_members(net_id) {
        let Some(ohms) = super::bus_loading::resistor_ohms(comp) else {
            continue;
        };
        let far = comp
            .pins
            .iter()
            .filter_map(|p| p.net)
            .find(|n| *n != net_id);
        let Some(far) = far.filter(|n| *n != 0) else {
            continue;
        };
        if board.net(far).is_none_or(|net| is_ground(&net.name)) {
            continue;
        }
        // A feed comes from a rail the board declares (a named supply or a
        // regulator's output), judged at its solved level: a bias resistor to
        // a signal net that happens to sit higher is a load, not a feed.
        if !matches!(resolver.declared(far), RailKnowledge::Known(_)) {
            continue;
        }
        if let RailKnowledge::Known(range) = resolver.range(far) {
            if range.max_v > rail.max_v {
                feeds.push((comp.reference.clone(), ohms, far, range));
            }
        }
    }
    feeds.sort_by(|a, b| a.1.total_cmp(&b.1));
    feeds
}

/// Replace the per-part findings of a rail that never came up with ONE
/// rail-level finding, where the parts' own static draw through the rail's
/// series feed resistance explains it: the feed alone sits at or above the
/// parts' minimum supply, but that draw would drop more than the headroom
/// across the resistance. That is a starved rail, not N independent parts out
/// of range, and the fix is the feed, not the parts. A rail a regulator
/// defines, one with no resistive feed from a higher net, or one whose parts
/// state no supply current keeps its per-part findings.
fn fold_starved_rails(
    resolver: &RailResolver<'_>,
    starved: BTreeMap<i64, Vec<Starved>>,
    report: &mut NetLintReport,
) {
    let mut folded = BTreeSet::new();
    let mut rail_findings = Vec::new();
    for (net_id, parts) in starved {
        let Some(rail) = resolver.solved(net_id) else {
            continue;
        };
        if resolver.rails.regulators.contains_key(&net_id) {
            continue;
        }
        let Some(finding) = explain_starved(resolver, net_id, &parts, Some(rail)) else {
            continue;
        };
        folded.extend(parts.iter().map(|p| p.finding));
        rail_findings.push(finding);
    }
    let mut index = 0usize;
    report.findings.retain(|_| {
        index += 1;
        !folded.contains(&(index - 1))
    });
    report.findings.extend(rail_findings);
}

/// The static counterpart of [`fold_starved_rails`], for a rail no level is
/// known for (no solve, no regulator, no declared voltage): the parts' own
/// card-stated static draw through the rail's series feed from a declared rail
/// is a floor on the drop, so when that alone exceeds the headroom the rail
/// cannot reach the parts' minimum supply. Worded as an estimate. It is raised
/// only when everything else on the rail is a passive or a part with a card:
/// a pin of an unmodelled part (a connector, a module) might source the rail
/// and hold it up, which would make the estimate wrong. A modelled source of
/// the rail (a regulator, a battery) already gives the rail a declared level.
fn estimate_starved_rails(
    resolver: &RailResolver<'_>,
    st: &BoardStatics<'_>,
    unleveled: &BTreeMap<i64, Vec<Starved>>,
    report: &mut NetLintReport,
) {
    let modelled: BTreeSet<&str> = st
        .parts(resolver.board)
        .map(|(comp, _)| comp.reference.as_str())
        .collect();
    for (&net_id, parts) in unleveled {
        if rail_may_have_other_source(resolver.board, &modelled, net_id) {
            continue;
        }
        report
            .findings
            .extend(explain_starved(resolver, net_id, parts, None));
    }
}

/// Whether some pin on the net belongs to an unmodelled part or a connector
/// (not a passive two-terminal one), so might source the net.
fn rail_may_have_other_source(
    board: &ExtractedBoard,
    modelled: &BTreeSet<&str>,
    net_id: i64,
) -> bool {
    board.net_members(net_id).into_iter().any(|(comp, _)| {
        let r = comp.reference.to_ascii_uppercase();
        // A connector is a way in for power even when its card is a generic one.
        let connector = ["J", "P", "X", "CN"].iter().any(|p| r.starts_with(p));
        if modelled.contains(comp.reference.as_str()) && !connector {
            return false;
        }
        let passive = comp.pins.len() == 2
            && ["R", "C", "L"].iter().any(|p| r.starts_with(p))
            && !["RV", "RT"].iter().any(|p| r.starts_with(p));
        // A DNP part is absent; one of unknown identity may well be there.
        !passive && !matches!(AssemblyState::of(comp), AssemblyState::DnpAbsent(_))
    })
}

/// The rail-level starved finding for `parts`' supply rail `net_id`, or `None`
/// when the rail has no resistive feed from a higher declared rail, the parts
/// state no supply current, or their draw through the feed fits the headroom.
/// `reached` is the level the rail solved to; `None` words it as an estimate
/// from the cards alone.
fn explain_starved(
    resolver: &RailResolver<'_>,
    net_id: i64,
    parts: &[Starved],
    reached: Option<RailRange>,
) -> Option<LintFinding> {
    let board = resolver.board;
    let net = board.net(net_id)?;
    let feeds = series_feeds(resolver, net_id, reached.unwrap_or(RailRange::point(0.0)));
    if feeds.is_empty() {
        return None;
    }
    // One entry per part: a card with several temperature rows records the
    // part once per row.
    let mut per_part: BTreeMap<&str, (f64, f64)> = BTreeMap::new();
    for p in parts {
        let slot = per_part.entry(&p.reference).or_insert((p.min_v, p.draw_a));
        slot.0 = slot.0.max(p.min_v);
    }
    let need_v = per_part.values().map(|(v, _)| *v).fold(f64::MIN, f64::max);
    let draw_a: f64 = per_part.values().map(|(_, a)| *a).sum();
    let r_ohms = 1.0 / feeds.iter().map(|(_, ohms, ..)| 1.0 / ohms).sum::<f64>();
    let (feed_net, feed) = feeds
        .iter()
        .map(|(_, _, far, range)| (*far, *range))
        .max_by(|a, b| a.1.max_v.total_cmp(&b.1.max_v))?;
    let headroom_v = feed.max_v - need_v;
    let drop_v = draw_a * r_ohms;
    if draw_a <= 0.0 || headroom_v < 0.0 || drop_v <= headroom_v {
        return None;
    }
    let feed_name = board.net(feed_net).map_or("?", |n| n.name.as_str());
    let resistors: Vec<String> = feeds
        .iter()
        .map(|(r, ohms, ..)| format!("{r} ({} ohm)", super::device_decode::format_ohms(*ohms)))
        .collect();
    let refs: Vec<&str> = per_part.keys().copied().collect();
    let listed = if refs.len() > STARVED_PARTS_SHOWN {
        let shown = refs[..STARVED_PARTS_SHOWN].join(", ");
        format!("{shown}, and {} more", refs.len() - STARVED_PARTS_SHOWN)
    } else {
        refs.join(", ")
    };
    let max_ohms = super::device_decode::format_ohms(headroom_v / draw_a);
    let (count, feed_v) = (refs.len(), feed.max_v);
    let draw = format_amps(draw_a);
    let fed_by = resistors.join(" and ");
    let message = reached.map_or_else(
        || {
            format!( "'{}' is likely starved (estimate from the cards' stated supply current; no operating point was solved): the {count} part(s) it powers ({listed}) need at least {}, but their {draw} static supply draw alone through {fed_by} from '{feed_name}' ({}) would drop {}, more than the {} of headroom, so the rail cannot come up and those parts would not run. Remedy: Feed the rail through no more than {max_ohms} ohm in total, or directly from '{feed_name}'.",
                net.name, format_volts(need_v), format_volts(feed_v), format_volts(drop_v), format_volts(headroom_v))
        },
        |rail| {
            format!( "'{}' is starved: it reaches only {}, below the {} minimum supply of the {count} part(s) it powers ({listed}). Their {draw} static supply draw through {fed_by} from '{feed_name}' ({}) drops {}, more than the {} of headroom, so the rail cannot come up and those parts do not run. Remedy: Feed the rail through no more than {max_ohms} ohm in total, or directly from '{feed_name}'.",
                net.name, format_volts(rail.max_v), format_volts(need_v), format_volts(feed_v), format_volts(drop_v), format_volts(headroom_v))
        },
    );
    let mut all_refs: Vec<String> = feeds.iter().map(|(r, ..)| r.clone()).collect();
    all_refs.extend(refs.iter().map(|r| (*r).to_string()));
    Some(LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity: Severity::High,
        message,
        refs: all_refs,
        nets: vec![net.name.clone(), feed_name.to_string()],
    })
}

/// The same limits as run evidence, judged on the levels the run SOLVED.
///
/// A serious limit undermines whatever the part drives; a lesser one qualifies
/// it. A rail the run did not solve is an abstention, never a violation. Each
/// level is the span the net covered over the run, so a rail that sags out of
/// its envelope and recovers is judged on the sag, not on where it ended.
/// `ambient` is the run's ambient (°C) and whether the scenario stated it.
#[must_use]
pub fn run_assumptions(
    board: &ExtractedBoard,
    st: &BoardStatics<'_>,
    solved: &BTreeMap<String, RailRange>,
    tolerance: (f64, f64),
    ambient: (f64, bool),
) -> Vec<Assumption> {
    let solved_only = |f: &LintFinding| f.nets.iter().all(|n| solved.contains_key(n));
    let judge = |at| {
        let found = envelope_lint_with_tolerance(board, st, solved, tolerance, at).findings;
        let serious = |f: LintFinding| (f.severity == Severity::High, f);
        let found = found.into_iter().filter(&solved_only).map(serious);
        found.map(|(bad, f)| (f, bad)).collect()
    };
    let fix = "bring the rail inside the envelope the card quotes, then re-run";
    let mut out = super::analog_inputs::at_ambient(ambient, judge, "supply-envelope", fix);
    out.extend(temperature_caveats(board, st, ambient.0));
    out
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, pin};
    use super::*;
    use hauksbee_extract::{Component, LintCheck, Pin, Severity};

    /// The shared builder, taking pins already built rather than pad/net pairs.
    fn part(reference: &str, value: &str, pins: Vec<Pin>) -> Component {
        Component {
            pins,
            ..super::super::test_support::part(reference, value, &[])
        }
    }

    fn library(tag: &str, source: &str) -> ModelLibrary {
        // Tests that share a tag run as threads of one process under `cargo test`; a
        // per-call directory keeps one from reading another's half-written card.
        let dir = hauksbee_testkit::fresh_scratch_dir("envelope", tag);
        std::fs::write(dir.join("models.toml"), source).unwrap();
        let mut lib = ModelLibrary::empty();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "test model must load: {errors:?}");
        lib
    }

    const SUPPLY_CARD: &str = r#"
[[models]]
id = "range_part"
kind = "digital"
description = "identity-only envelope fixture"
[models.match]
value_re = "^RANGE_PART$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vdd"
"2" = "gnd"
[[models.envelope]]
kind = "supply_range"
pin = "vdd"
min_v = 3.2
max_v = 3.6
basis = "Recommended Operating Conditions, Table 1, VDD row"
"#;

    /// A fitted Arduino Nano's 5V and 3V3 header pins source their rails per
    /// the card (USB/VIN through the module's own parts), so a carrier rail
    /// fed only from them has a range instead of abstaining; a DNP Nano does
    /// not.
    #[test]
    fn a_fitted_modules_supply_pins_define_the_rails_they_feed() {
        let nets = &[(1, "GND"), (2, "+5P"), (3, "MOD_3V3"), (4, "VREG")];
        let nano = |dnp: bool| Component {
            footprint: "Module:Arduino_Nano".into(),
            dnp,
            ..part(
                "A101",
                "Arduino Nano",
                vec![pin("4", 1), pin("27", 2), pin("17", 3)],
            )
        };
        let lib = ModelLibrary::builtin();
        let solved = BTreeMap::new();
        let known = |b: &ExtractedBoard, id| match RailResolver::new(
            b,
            &BoardStatics::of(b, &lib),
            &solved,
        )
        .range(id)
        {
            RailKnowledge::Known(r) => Some((r.min_v, r.max_v)),
            _ => None,
        };
        let fitted = board(nets, vec![nano(false)]);
        assert_eq!(known(&fitted, 2), Some((4.365, 5.25)), "5V from USB/VIN");
        assert_eq!(known(&fitted, 3), Some((3.0, 3.6)), "3V3 from the FT232RL");
        assert_eq!(
            known(&board(nets, vec![nano(true)]), 2),
            None,
            "a DNP module sources nothing"
        );
    }

    /// An RP2040 core net fed from its own regulator output is at the 1.1 V
    /// that regulator starts at, whatever level the net's name implies; the
    /// same net fed from nowhere the board shows keeps the name's assumed band,
    /// which leaves the DVDD window.
    #[test]
    fn an_on_chip_regulator_output_sets_its_rail_over_the_net_name() {
        let nets = &[(1, "GND"), (2, "+1V2"), (3, "VREG_OUT_NC")];
        let rp2040 = |vreg_vout_net: i64| Component {
            footprint: "Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm".into(),
            ..part(
                "U7",
                "RP2040",
                vec![
                    pin("23", 2),
                    pin("50", 2),
                    pin("45", vreg_vout_net),
                    pin("57", 1),
                ],
            )
        };
        let core = |vreg_vout_net: i64| {
            let report = envelope_lint(
                &board(nets, vec![rp2040(vreg_vout_net)]),
                &ModelLibrary::builtin(),
            );
            report
                .of_check(LintCheck::OperatingEnvelope)
                .filter(|f| f.message.contains("dvdd"))
                .count()
        };
        assert_eq!(core(2), 0, "VREG_VOUT drives the core net");
        assert!(core(3) >= 1, "nothing on the board sets the core net");
    }

    #[test]
    fn named_rail_uses_full_plus_minus_five_percent_range() {
        let report = envelope_lint(
            &board(
                &[(1, "GND"), (2, "+3V3")],
                vec![part("U1", "RANGE_PART", vec![pin("1", 2), pin("2", 1)])],
            ),
            &library("range", SUPPLY_CARD),
        );
        let findings: Vec<_> = report.of_check(LintCheck::OperatingEnvelope).collect();
        assert_eq!(findings.len(), 1);
        assert!(findings[0].message.contains("3.135"));
        assert!(findings[0].message.contains("3.465"));
        assert!(findings[0].message.contains("Table 1"));
    }

    /// An ESP32-S3 module (3.6 V absolute maximum, datasheet Table 6-1) on a
    /// 5 V rail warns with the excess and the limit; on 3.3 V it is clean. The
    /// card states a `supply_range` row with `abs_max_v` on its one supply pin,
    /// so that row judges the pin once: the ratings-only path
    /// (`supply_abs_max_findings`, covered with the W25Q card below) skips a
    /// role a `supply_range` row already judges and must not add a second
    /// finding for the same pin.
    #[test]
    fn an_ic_on_a_rail_above_its_card_abs_max_supply_is_flagged() {
        let lint = |rail: &str| {
            let module = Component {
                footprint: "RF_Module:ESP32-S3-WROOM-1".into(),
                ..part("U1", "ESP32-S3-WROOM-1", vec![pin("2", 2), pin("1", 1)])
            };
            let report = envelope_lint(
                &board(&[(1, "GND"), (2, rail)], vec![module]),
                &ModelLibrary::builtin(),
            );
            report
                .of_check(LintCheck::OperatingEnvelope)
                .cloned()
                .collect::<Vec<_>>()
        };
        let hot = lint("+5V");
        assert_eq!(hot.len(), 1, "one finding for the one pin: {hot:?}");
        assert_eq!(
            hot[0].severity,
            Severity::Medium,
            "name-assumed level warns"
        );
        for says in [
            "U1 role vdd on '+5V'",
            "4.75..5.25 V",
            "1.65 V above its 3.6 V absolute maximum",
            "Table 6-1",
        ] {
            assert!(
                hot[0].message.contains(says),
                "{says:?} in {}",
                hot[0].message
            );
        }
        assert!(lint("+3V3").is_empty(), "3.465 V is under 3.6 V");
    }

    /// A rail whose only level is its name's band (bare `VCC` reads 5 V) but
    /// which an unmodelled converter drives, here through its switch-node
    /// inductor and a net tie, is a note asking for the level: the converter
    /// sets it (a `VCC` buck output is often 3.3 V). The same name with
    /// nothing driving it keeps the gating warning (faulty and control).
    #[test]
    fn a_name_only_rail_an_unmodelled_converter_drives_is_a_note() {
        let w25q = |net| part("U20", "W25Q128JVSIQ", vec![pin("8", net), pin("4", 1)]);
        let named = |p: Pin, name: &str| Pin {
            function: name.into(),
            ..p
        };
        let buck = part(
            "U21",
            "TPS62410",
            vec![
                named(pin("3", 4), "VIN"),
                named(pin("6", 5), "SW1"),
                named(pin("4", 3), "FB1"),
                named(pin("8", 1), "GND"),
            ],
        );
        let inductor = part("L11", "4u7", vec![pin("1", 3), pin("2", 5)]);
        let tie = Component {
            lib_id: "Device:NetTie_2".into(),
            ..part("NT2", "VCC", vec![pin("1", 2), pin("2", 3)])
        };
        let nets = &[
            (1, "GND"),
            (2, "VCC"),
            (3, "REG_OUT1"),
            (4, "VIN"),
            (5, "SW_NODE"),
        ];
        let abs_max = |b: &ExtractedBoard| -> Vec<LintFinding> {
            envelope_lint(b, &ModelLibrary::builtin())
                .of_check(LintCheck::OperatingEnvelope)
                .filter(|f| f.message.contains("absolute-maximum"))
                .cloned()
                .collect()
        };
        let parts = vec![w25q(2), buck, inductor, tie];
        let driven = abs_max(&board(nets, parts.clone()));
        assert_eq!(driven.len(), 1, "{driven:?}");
        assert_eq!(driven[0].severity, Severity::Low, "{}", driven[0].message);
        for says in [
            "U21 (TPS62410)",
            "net_drive",
            "only the net name suggests 5 V",
        ] {
            assert!(
                driven[0].message.contains(says),
                "{says:?} in {}",
                driven[0].message
            );
        }
        let alone = abs_max(&board(nets, vec![w25q(2)]));
        assert_eq!(alone.len(), 1, "{alone:?}");
        assert_eq!(alone[0].severity, Severity::Medium, "{}", alone[0].message);
        // A converter output whose name states its level ("+5V") keeps the
        // warning: the name is a magnitude, not a convention.
        let mut stated = nets.to_vec();
        stated[1] = (2, "+5V");
        let named_5v = abs_max(&board(&stated, parts));
        assert_eq!(named_5v.len(), 1, "{named_5v:?}");
        assert_eq!(
            named_5v[0].severity,
            Severity::Medium,
            "{}",
            named_5v[0].message
        );
    }

    const MARGIN_CARD: &str = r#"
[[models]]
id = "margin_part"
kind = "digital"
description = "identity-only envelope fixture with an abs max"
[models.match]
value_re = "^MARGIN_PART$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vdd"
"2" = "gnd"
[[models.envelope]]
kind = "supply_range"
pin = "vdd"
min_v = 3.0
max_v = 3.4
abs_max_v = 4.0
severity = "serious"
basis = "Recommended Operating Conditions, VDD row"
"#;

    /// A band only the net NAME implies is an assumption: when just its
    /// tolerance corner leaves the window (under abs max) it is a note that
    /// says so, never a serious finding, and never a suggestion to rewire
    /// the part to some other rail. Past abs max it is at most a warning.
    #[test]
    fn name_assumed_band_is_a_stated_note_not_a_serious_rewire() {
        let lib = library("assumed", MARGIN_CARD);
        let one = |rail: &str| {
            let b = board(
                &[(1, "GND"), (2, rail), (3, "+3V0")],
                vec![part("U1", "MARGIN_PART", vec![pin("1", 2), pin("2", 1)])],
            );
            let report = envelope_lint(&b, &lib);
            let found: Vec<_> = report
                .of_check(LintCheck::OperatingEnvelope)
                .cloned()
                .collect();
            assert_eq!(found.len(), 1, "{rail}: {found:?}");
            found.into_iter().next().unwrap()
        };
        // +3V3 -> 3.135..3.465: nominal inside, corner above 3.4, under 4.0.
        let corner = one("+3V3");
        assert_eq!(corner.severity, Severity::Low, "{}", corner.message);
        assert!(corner.message.contains("assumed"), "{}", corner.message);
        assert!(!corner.message.contains("Tie "), "{}", corner.message);
        // +5V -> 4.75..5.25: past abs max, but still only an assumed level.
        let over = one("+5V");
        assert_eq!(over.severity, Severity::Medium, "{}", over.message);
        assert!(!over.message.contains("Tie "), "{}", over.message);
    }

    /// A rail whose level is only its name's band gates (Medium) when nothing
    /// else says what the board runs from, but never on a battery-fed board:
    /// there the converter or cell behind the rail sets its level, so the name
    /// is a guess and the finding is a non-gating note that says so.
    #[test]
    fn name_assumed_band_does_not_gate_on_a_battery_fed_board() {
        let lib = library("assumed_battery", MARGIN_CARD);
        let run = |battery: bool| {
            let mut parts = vec![part("U1", "MARGIN_PART", vec![pin("1", 2), pin("2", 1)])];
            if battery {
                parts.push(part(
                    "BT1",
                    "Battery holder",
                    vec![pin("1", 3), pin("2", 1)],
                ));
            }
            let b = board(&[(1, "GND"), (2, "+5V"), (3, "CELL")], parts);
            let report = envelope_lint(&b, &lib);
            report
                .of_check(LintCheck::OperatingEnvelope)
                .filter(|f| f.refs == ["U1"])
                .cloned()
                .collect::<Vec<_>>()
        };
        let fixed = run(false);
        assert_eq!(fixed.len(), 1, "{fixed:?}");
        assert_eq!(fixed[0].severity, Severity::Medium, "{}", fixed[0].message);
        let battery = run(true);
        assert_eq!(battery.len(), 1, "{battery:?}");
        assert_eq!(battery[0].severity, Severity::Low, "{}", battery[0].message);
        assert!(
            battery[0].message.contains("battery-fed"),
            "{}",
            battery[0].message
        );
    }

    /// The window and the absolute maximum are different tables of the sheet. A
    /// finding that states "N V above its X V absolute maximum" cites both
    /// bases, says when the card records only one, and states a plain
    /// out-of-window finding (under abs max) with the window's basis alone.
    #[test]
    fn an_absolute_maximum_finding_cites_both_tables() {
        let amr = "Absolute Maximum Ratings, VDD supply voltage -0.5 V to 4.0 V";
        let cards = [
            // Window-only basis, no abs-max source recorded (the faulty shape).
            ("only_roc", String::new()),
            // The abs-max table recorded beside the window's.
            ("both", format!("abs_max_basis = \"{amr}\"\n")),
            // The window basis already names the second table.
            (
                "named",
                "basis = \"Recommended Operating Conditions, VDD row; Section 5.1 VDD 4 V\"\n"
                    .to_string(),
            ),
        ];
        let mut text = String::new();
        for (id, extra) in &cards {
            let card = MARGIN_CARD
                .replace("margin_part", id)
                .replace("^MARGIN_PART$", &format!("^{}$", id.to_ascii_uppercase()))
                .replace(
                    "severity = \"serious\"\n",
                    &format!("severity = \"serious\"\n{extra}"),
                );
            // A repeated `basis` key would not parse: `named` replaces it.
            let card = if id == &"named" {
                card.replace(
                    "basis = \"Recommended Operating Conditions, VDD row\"\n",
                    "",
                )
            } else {
                card
            };
            text.push_str(&card);
        }
        let lib = library("abscite", &text);
        let message = |value: &str, rail: &str| {
            let b = board(
                &[(1, "GND"), (2, rail)],
                vec![part("U1", value, vec![pin("1", 2), pin("2", 1)])],
            );
            let report = envelope_lint(&b, &lib);
            let found: Vec<_> = report
                .of_check(LintCheck::OperatingEnvelope)
                .cloned()
                .collect();
            assert_eq!(found.len(), 1, "{value} {rail}: {found:?}");
            found[0].message.clone()
        };
        // +5V reads 4.75..5.25 V: past the 4.0 V absolute maximum.
        let only_roc = message("ONLY_ROC", "+5V");
        assert!(
            only_roc.contains("above its 4 V absolute maximum"),
            "{only_roc}"
        );
        assert!(
            only_roc.contains("records no source for its 4 V absolute maximum"),
            "{only_roc}"
        );
        let both = message("BOTH", "+5V");
        assert!(
            both.contains("basis: \"Recommended Operating Conditions, VDD row\""),
            "{both}"
        );
        assert!(
            both.contains(&format!("absolute maximum basis: \"{amr}\"")),
            "{both}"
        );
        let named = message("NAMED", "+5V");
        assert!(named.contains("Section 5.1 VDD 4 V"), "{named}");
        assert!(!named.contains("records no source"), "{named}");
        // Control: +3V3 reads 3.135..3.465 V, outside the 3.0..3.4 V window but
        // under the absolute maximum, so no abs-max sentence and no second basis.
        for value in ["ONLY_ROC", "BOTH"] {
            let near = message(value, "+3V3");
            assert!(!near.contains("absolute maximum"), "{near}");
            assert!(
                near.contains("basis: \"Recommended Operating Conditions, VDD row\""),
                "{near}"
            );
        }
    }

    const BOOST_AND_LOAD: &str = r#"
[[models]]
id = "fb_boost"
kind = "vreg"
description = "feedback-set boost fixture"
[models.match]
value_re = "^FB_BOOST$"
[models.pins]
"1" = "in"
"2" = "gnd"
"3" = "fb"
"4" = "out"
[models.behavioral.converter]
topology = "boost"
out_pin = "out"
in_pin = "in"
vout_setpoint = 9.0
[models.behavioral.converter.feedback]
pin = "fb"
vref_v = 0.5

[[models]]
id = "load_part"
kind = "digital"
description = "identity-only load"
[models.match]
value_re = "^LOAD_PART$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vdd"
"2" = "gnd"
[[models.envelope]]
kind = "supply_range"
pin = "vdd"
min_v = 2.5
max_v = 4.5
abs_max_v = 6.0
basis = "Recommended Operating Conditions, V+ row"
"#;

    /// The regulator's own feedback divider sets the rail, and the level
    /// carries through a net tie to the load's net: the finding is judged at
    /// Vref * (1 + Rtop/Rbot) = 5.0 V, not at the card's 9 V default nor at
    /// the ±5 % band the far net's name would imply.
    #[test]
    fn divider_set_point_reaches_the_load_through_a_net_tie() {
        let lib = library("fbboost", BOOST_AND_LOAD);
        let res = |r: &str, v: &str, a: i64, b: i64| {
            let mut c = part(r, v, vec![pin("1", a), pin("2", b)]);
            c.lib_id = "Device:R".into();
            c
        };
        let mut tie = part("NT1", "NetTie", vec![pin("1", 4), pin("2", 5)]);
        tie.lib_id = "Device:NetTie_2".into();
        let b = board(
            &[
                (1, "GND"),
                (2, "VIN"),
                (3, "FB"),
                (4, "BOOST_OUT"),
                (5, "+5V0"),
            ],
            vec![
                part(
                    "U1",
                    "FB_BOOST",
                    vec![pin("1", 2), pin("2", 1), pin("3", 3), pin("4", 4)],
                ),
                res("R1", "450k", 3, 4),
                res("R2", "50k", 3, 1),
                tie,
                part("U2", "LOAD_PART", vec![pin("1", 5), pin("2", 1)]),
            ],
        );
        let report = envelope_lint(&b, &lib);
        let found: Vec<_> = report
            .of_check(LintCheck::OperatingEnvelope)
            .filter(|f| f.refs.contains(&"U2".to_string()))
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        let msg = &found[0].message;
        assert!(msg.contains("5..5 V"), "{msg}");
        assert!(!msg.contains("assumed"), "{msg}");
        assert_eq!(found[0].severity, Severity::Medium, "{msg}");
    }

    /// A set point exactly at the load's recommended maximum is only in range
    /// at the reference's typical value: the card's sourced reference interval
    /// carries through the divider, so the tolerance corner is reported as a
    /// margin note (under abs max) rather than hidden behind the nominal.
    #[test]
    fn divider_set_point_carries_the_reference_tolerance() {
        let card = BOOST_AND_LOAD.replace(
            "vref_v = 0.5\n",
            "vref_v = 0.5\n[models.source]\ntier = \"curated-library\"\n\
             validation = \"physical-bounds-only\"\n[[models.source.uncertainty]]\n\
             status = \"interval\"\nparameter = \"feedback_reference\"\nlow = 0.485\n\
             high = 0.515\nunit = \"V\"\nkind = \"specification-limits\"\n\
             basis = \"reference min/max\"\n",
        );
        let lib = library("fbtol", &card);
        let res = |r: &str, v: &str, a: i64, b: i64| {
            let mut c = part(r, v, vec![pin("1", a), pin("2", b)]);
            c.lib_id = "Device:R".into();
            c
        };
        let b = board(
            &[(1, "GND"), (2, "VIN"), (3, "FB"), (4, "OUT")],
            vec![
                part(
                    "U1",
                    "FB_BOOST",
                    vec![pin("1", 2), pin("2", 1), pin("3", 3), pin("4", 4)],
                ),
                res("R1", "400k", 3, 4),
                res("R2", "50k", 3, 1),
                part("U2", "LOAD_PART", vec![pin("1", 4), pin("2", 1)]),
            ],
        );
        let report = envelope_lint(&b, &lib);
        let found: Vec<_> = report
            .of_check(LintCheck::OperatingEnvelope)
            .filter(|f| f.refs.contains(&"U2".to_string()))
            .collect();
        assert_eq!(found.len(), 1, "{found:?}");
        let msg = &found[0].message;
        assert!(msg.contains("4.365..4.635"), "{msg}");
        assert!(!msg.contains("Tie "), "a margin is not a rewire: {msg}");
        assert_eq!(found[0].severity, Severity::Low, "{msg}");
    }

    /// The divider's own resistor tolerances widen the set point: 400k/50k
    /// sets exactly the load's 4.5 V maximum, so 1 % parts reach 4.58 V
    /// (a margin note, under abs max) where the same values with no stated
    /// tolerance are judged at nominal and say nothing.
    #[test]
    fn divider_set_point_carries_the_resistor_tolerances() {
        let lib = library("fbrtol", BOOST_AND_LOAD);
        let load_report = |top: &str, bottom: &str| {
            let b = board(
                &[(1, "GND"), (2, "VIN"), (3, "FB"), (4, "OUT")],
                vec![
                    part(
                        "U1",
                        "FB_BOOST",
                        vec![pin("1", 2), pin("2", 1), pin("3", 3), pin("4", 4)],
                    ),
                    resistor("R1", top, 3, 4),
                    resistor("R2", bottom, 3, 1),
                    part("U2", "LOAD_PART", vec![pin("1", 4), pin("2", 1)]),
                ],
            );
            envelope_lint(&b, &lib)
                .of_check(LintCheck::OperatingEnvelope)
                .filter(|f| f.refs.contains(&"U2".to_string()))
                .cloned()
                .collect::<Vec<_>>()
        };
        let found = load_report("400k 1%", "50k 1%");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].message.contains("4.421..4.581"), "{:?}", found[0]);
        assert!(
            load_report("400k", "50k").is_empty(),
            "no stated tolerance is judged at the nominal divider"
        );
        assert!(
            load_report("390k 1%", "50k 1%").is_empty(),
            "a divider whose tolerance corner stays inside the window says nothing"
        );
    }

    /// A buck whose card names its switch node as the output, an LDO set by
    /// the LM317 law, and the load from [`BOOST_AND_LOAD`].
    const BUCK_AND_ADJ: &str = r#"
[[models]]
id = "fb_buck"
kind = "vreg"
description = "feedback-set buck fixture whose output pin is the switch node"
[models.match]
value_re = "^FB_BUCK$"
[models.pins]
"1" = "in"
"2" = "gnd"
"3" = "fb"
"4" = "sw"
[models.behavioral.converter]
topology = "buck"
out_pin = "sw"
in_pin = "in"
vout_setpoint = 9.0
[models.behavioral.converter.feedback]
pin = "fb"
vref_v = 0.5

[[models]]
id = "adj_ldo"
kind = "vreg"
description = "ADJ-referenced (no ground pin) adjustable LDO fixture"
[models.match]
value_re = "^ADJ_LDO$"
[models.params]
vref = 1.25
iadj_a = 50.0e-6
dropout_v = 3.0
[models.pins]
"1" = "fb"
"2" = "out"
"3" = "in"
"#;

    /// The load U2's findings on `parts` (with the load on net 5).
    fn load_findings(parts: Vec<Component>, nets: &[(i64, &str)]) -> Vec<LintFinding> {
        let card = format!("{BOOST_AND_LOAD}\n{BUCK_AND_ADJ}");
        let lib = library("adjrails", &card);
        let mut parts = parts;
        parts.push(part("U2", "LOAD_PART", vec![pin("1", 5), pin("2", 1)]));
        let report = envelope_lint(&board(nets, parts), &lib);
        (report.of_check(LintCheck::OperatingEnvelope))
            .filter(|f| f.refs.contains(&"U2".to_string()))
            .cloned()
            .collect()
    }

    fn resistor(reference: &str, value: &str, net_a: i64, net_b: i64) -> Component {
        let mut built = part(reference, value, vec![pin("1", net_a), pin("2", net_b)]);
        built.lib_id = "Device:R".into();
        built
    }

    /// The buck's divider senses the rail past the inductor, not the switch
    /// node its card names: the load is judged at Vref * (1 + Rtop/Rbot).
    /// Faulty: 450k/50k sets 5.0 V, past the load's 4.5 V. Legal control:
    /// 270k/50k sets 3.2 V, inside it, so nothing is said.
    #[test]
    fn a_bucks_divider_sets_the_rail_past_its_inductor() {
        let buck = |top: &str| {
            let mut l1 = part("L1", "10uH", vec![pin("1", 4), pin("2", 5)]);
            l1.lib_id = "Device:L".into();
            let parts = vec![
                part(
                    "U1",
                    "FB_BUCK",
                    vec![pin("1", 2), pin("2", 1), pin("3", 3), pin("4", 4)],
                ),
                l1,
                resistor("R1", top, 3, 5),
                resistor("R2", "50k", 3, 1),
            ];
            let nets = [(1, "GND"), (2, "VIN"), (3, "FB"), (4, "SW"), (5, "VOUT")];
            load_findings(parts, &nets)
        };
        let high = buck("450k");
        assert_eq!(high.len(), 1, "{high:#?}");
        assert!(high[0].message.contains("5..5 V"), "{}", high[0].message);
        assert_ne!(high[0].severity, Severity::Low, "{}", high[0].message);
        assert!(buck("270k").is_empty(), "{:#?}", buck("270k"));
    }

    /// An adjustable LDO with no ground pin holds Vref between OUT and ADJ
    /// (the LM317 law): 240R OUT-ADJ over 390R ADJ-GND is 3.30 V, inside the
    /// load's window (the grounded law would read 2.02 V and fire). Faulty:
    /// 715R ADJ-GND sets 5.01 V, past it.
    #[test]
    fn an_adj_referenced_ldo_sets_its_rail_by_the_lm317_law() {
        let ldo = |bottom: &str| {
            let parts = vec![
                part("U1", "ADJ_LDO", vec![pin("1", 3), pin("2", 5), pin("3", 2)]),
                resistor("R1", "240", 5, 3),
                resistor("R2", bottom, 3, 1),
            ];
            load_findings(parts, &[(1, "GND"), (2, "VIN"), (3, "ADJ"), (5, "VOUT")])
        };
        assert!(ldo("390").is_empty(), "{:#?}", ldo("390"));
        let high = ldo("715");
        assert_eq!(high.len(), 1, "{high:#?}");
        assert!(high[0].message.contains("5.01"), "{}", high[0].message);
    }

    /// The divider findings on `parts` for `nets`, with no load on the rail:
    /// the rail's name and the divider are all there is to judge.
    fn divider_findings(parts: Vec<Component>, nets: &[(i64, &str)]) -> Vec<LintFinding> {
        let card = format!("{BOOST_AND_LOAD}\n{BUCK_AND_ADJ}");
        let lib = library("divname", &card);
        let report = envelope_lint(&board(nets, parts), &lib);
        (report.of_check(LintCheck::OperatingEnvelope))
            .filter(|f| f.message.contains("its feedback divider"))
            .cloned()
            .collect()
    }

    fn boost(top: &str, rail: &str) -> Vec<LintFinding> {
        let parts = vec![
            part(
                "U1",
                "FB_BOOST",
                vec![pin("1", 2), pin("2", 1), pin("3", 3), pin("4", 4)],
            ),
            resistor("R1", top, 3, 4),
            resistor("R2", "50k", 3, 1),
        ];
        divider_findings(parts, &[(1, "GND"), (2, "VIN"), (3, "FB"), (4, rail)])
    }

    /// A divider that sets a level its own rail name contradicts is a fault
    /// with no load on the rail to notice it: 450k over 50k holds a 0.5 V
    /// reference at 5.0 V on a net named +3V3. The finding names the
    /// regulator, both resistors, the level, and the top value that would
    /// give the named level (280k). Legal controls: the divider that gives
    /// 3.3 V, one inside the name's 10 % band, a name that states no voltage,
    /// and a name that states another voltage the divider does match.
    #[test]
    fn a_divider_that_contradicts_its_rails_name_is_a_finding() {
        let high = boost("450k", "+3V3");
        assert_eq!(high.len(), 1, "{high:#?}");
        let (finding, msg) = (&high[0], &high[0].message);
        assert_eq!(finding.severity, Severity::High, "{msg}");
        assert_eq!(finding.refs, ["U1", "R1", "R2"], "{msg}");
        assert_eq!(finding.nets, ["+3V3"], "{msg}");
        for part in [
            "U1 (FB_BOOST)",
            "5 V",
            "3.3 V",
            "52% above",
            "R1 450k",
            "R2 50k",
        ] {
            assert!(msg.contains(part), "{part}: {msg}");
        }
        // The remedy is the finding's own fix, not the check kind's generic advice.
        let (_, fix) = msg.rsplit_once(REMEDY_MARKER).expect("a remedy");
        assert!(fix.starts_with("Make R1 about 280k"), "{fix}");
        let low = boost("200k", "+3V3");
        assert_eq!(low.len(), 1, "{low:#?}");
        assert!(low[0].message.contains("below"), "{}", low[0].message);
        for (top, rail) in [
            ("280k", "+3V3"),
            ("300k", "+3V3"),
            ("450k", "VCC"),
            ("450k", "VOUT"),
            ("450k", "+5V"),
            ("450k", "5V_RF"),
        ] {
            let quiet = boost(top, rail);
            assert!(quiet.is_empty(), "{top} on {rail}: {quiet:#?}");
        }
    }

    /// The name is what the designer meant only when it states one voltage:
    /// two voltage tokens (`5V_TO_3V3`) or an enable named after a rail
    /// (`3V3_EN`) say nothing about the level, so nothing is judged.
    #[test]
    fn an_ambiguous_or_signal_name_is_not_a_stated_level() {
        for rail in ["5V_TO_3V3", "3V3_EN", "VBUS", "VDD"] {
            let quiet = boost("450k", rail);
            assert!(quiet.is_empty(), "{rail}: {quiet:#?}");
        }
    }

    /// The divider alone must set the level. A trim resistor into the
    /// feedback node, or a second regulator on the rail, leaves the closed
    /// form a guess, so it says nothing. A user-forced level is the user's
    /// statement, not the divider's.
    #[test]
    fn a_divider_is_judged_only_when_it_alone_sets_the_rail() {
        let regulator = || {
            part(
                "U1",
                "FB_BOOST",
                vec![pin("1", 2), pin("2", 1), pin("3", 3), pin("4", 4)],
            )
        };
        let nets = [(1, "GND"), (2, "VIN"), (3, "FB"), (4, "+3V3"), (5, "TRIM")];
        let trimmed = vec![
            regulator(),
            resistor("R1", "450k", 3, 4),
            resistor("R2", "50k", 3, 1),
            resistor("R3", "100k", 3, 5),
        ];
        assert!(divider_findings(trimmed, &nets).is_empty());
        let plain = vec![
            regulator(),
            resistor("R1", "450k", 3, 4),
            resistor("R2", "50k", 3, 1),
        ];
        assert_eq!(divider_findings(plain.clone(), &nets).len(), 1);
        let mut rival = plain.clone();
        rival.push(part(
            "U9",
            "FB_BOOST",
            vec![pin("1", 2), pin("2", 1), pin("3", 6), pin("4", 4)],
        ));
        let nets_rival = [(1, "GND"), (2, "VIN"), (3, "FB"), (4, "+3V3"), (6, "FB2")];
        assert!(divider_findings(rival, &nets_rival).is_empty());
        let lib = library("divforced", &format!("{BOOST_AND_LOAD}\n{BUCK_AND_ADJ}"));
        let forced = BTreeMap::from([("+3V3".to_string(), 3.3)]);
        let report = envelope_lint_with_drives(&board(&nets, plain), &lib, &forced);
        let said = (report.findings.iter()).any(|f| f.message.contains("its feedback divider"));
        assert!(!said, "{:#?}", report.findings);
    }

    /// The buck's rail is the net past its inductor, and the finding is on
    /// that net, not on the switch node its card names. The LM317-law LDO is
    /// judged by its own law: 240R over 715R is 5.01 V on a +3V3 net, over
    /// 390R is the 3.3 V the name states.
    #[test]
    fn bucks_and_lm317_dividers_are_judged_on_their_own_rail() {
        let buck = |top: &str, rail: &str| {
            let mut l1 = part("L1", "10uH", vec![pin("1", 4), pin("2", 5)]);
            l1.lib_id = "Device:L".into();
            let parts = vec![
                part(
                    "U1",
                    "FB_BUCK",
                    vec![pin("1", 2), pin("2", 1), pin("3", 3), pin("4", 4)],
                ),
                l1,
                resistor("R1", top, 3, 5),
                resistor("R2", "50k", 3, 1),
            ];
            let nets = [(1, "GND"), (2, "VIN"), (3, "FB"), (4, "SW"), (5, rail)];
            divider_findings(parts, &nets)
        };
        let high = buck("450k", "+3V3");
        assert_eq!(high.len(), 1, "{high:#?}");
        assert_eq!(high[0].nets, ["+3V3"]);
        assert!(buck("280k", "+3V3").is_empty());
        let ldo = |bottom: &str| {
            let parts = vec![
                part("U1", "ADJ_LDO", vec![pin("1", 3), pin("2", 5), pin("3", 2)]),
                resistor("R1", "240", 5, 3),
                resistor("R2", bottom, 3, 1),
            ];
            divider_findings(parts, &[(1, "GND"), (2, "VIN"), (3, "ADJ"), (5, "+3V3")])
        };
        let high = ldo("715");
        assert_eq!(high.len(), 1, "{high:#?}");
        assert!(high[0].message.contains("5.01 V"), "{}", high[0].message);
        assert!(ldo("390").is_empty(), "{:#?}", ldo("390"));
    }

    /// Ensemble members on one board share its statics, yet each is judged at
    /// its own solved level, whatever order they are asked in.
    #[test]
    fn shared_statics_judge_each_members_own_supply() {
        let b = board(
            &[(1, "GND"), (2, "UNNAMED_SUPPLY")],
            vec![part("U1", "RANGE_PART", vec![pin("1", 2), pin("2", 1)])],
        );
        let lib = library("shared", SUPPLY_CARD);
        let shared = BoardStatics::of(&b, &lib);
        let fires = |st: &BoardStatics<'_>, v: f64| {
            let level = RailRange { min_v: v, max_v: v };
            let solved = BTreeMap::from([("UNNAMED_SUPPLY".to_string(), level)]);
            let found = run_assumptions(&b, st, &solved, (1.0e-3, 1.0e-6), (25.0, true));
            !found.is_empty()
        };
        let volts = [3.3, 3.8, 3.3, 2.8, 3.4];
        let members: Vec<_> = volts.iter().map(|&v| fires(&shared, v)).collect();
        assert_eq!(members, [false, true, false, true, false]);
        for (v, got) in volts.iter().zip(members) {
            assert_eq!(fires(&BoardStatics::of(&b, &lib), *v), got, "{v} V");
        }
    }

    /// An op-amp card prints its limits as the V+ to V- span. Its negative supply
    /// pin on ground is the single-supply case, so the split-supply figures
    /// for that pin are no limit on ground; the same pin on a rail is still judged.
    #[test]
    fn an_opamps_negative_supply_pin_on_ground_is_the_single_supply_case() {
        let card = r#"
[[models]]
id = "oa_split"
kind = "opamp"
description = "split-supply op amp"
[models.match]
value_re = "^OA_SPLIT$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vcc"
"2" = "vee"
[[models.envelope]]
kind = "supply_range"
pin = "vcc"
min_v = 4.5
max_v = 36.0
basis = "Recommended Operating Conditions, V+ row"
[[models.envelope]]
kind = "supply_range"
pin = "vee"
min_v = -18.0
max_v = -2.25
basis = "Recommended Operating Conditions, V- row"
"#;
        let lib = library("opamp_split", card);
        let findings = |low_rail: &str| {
            let b = board(
                &[(1, "+12V"), (2, low_rail)],
                vec![part("U1", "OA_SPLIT", vec![pin("1", 1), pin("2", 2)])],
            );
            envelope_lint(&b, &lib)
                .findings
                .into_iter()
                .filter(|f| f.check == LintCheck::OperatingEnvelope)
                .count()
        };
        assert_eq!(findings("GND"), 0, "V- on ground is a single supply");
        assert_eq!(findings("+5V"), 1, "V- on a positive rail is still wrong");
    }

    /// An adjustable regulator's card states the span its output can be set across
    /// (a buck's 0.9 to 6 V). With no feedback divider on the board that span is no
    /// level of the rail it drives, so no rail range is taken from it; a fixed
    /// regulator's tight band still is.
    #[test]
    fn an_adjustable_parts_setting_span_is_not_a_rail_level() {
        let card = |low: f64, high: f64, fb: bool| {
            let feedback = if fb {
                "[models.behavioral.converter.feedback]\npin = \"fb\"\nvref_v = 0.5\n"
            } else {
                ""
            };
            format!(
                r#"
[[models]]
id = "span_buck"
kind = "vreg"
description = "span fixture"
[models.match]
value_re = "^SPAN_BUCK$"
[models.params]
vout_setpoint_low = {low}
vout_setpoint_high = {high}
[models.pins]
"1" = "in"
"2" = "gnd"
"3" = "fb"
"4" = "out"
[models.behavioral.converter]
topology = "buck"
out_pin = "out"
in_pin = "in"
vout_setpoint = 3.3
{feedback}"#
            )
        };
        let b = board(
            &[(1, "GND"), (2, "VIN"), (3, "FB"), (4, "OUT")],
            vec![part(
                "U1",
                "SPAN_BUCK",
                vec![pin("1", 2), pin("2", 1), pin("3", 3), pin("4", 4)],
            )],
        );
        let regulated = |low, high| {
            let lib = library("span", &card(low, high, true));
            BoardStatics::of(&b, &lib)
                .rails(&b)
                .regulators
                .get(&4)
                .copied()
        };
        assert_eq!(regulated(0.9, 6.0), None, "a setting span is no rail level");
        let tight = regulated(3.2, 3.4).expect("a regulation band is");
        assert!((tight.min_v - 3.2).abs() < 1e-9 && (tight.max_v - 3.4).abs() < 1e-9);
    }

    /// The statics belong to their exact board: a feedback resistor changed
    /// by a sampled member sets a different rail, so that member must not be
    /// handed the statics taken from the nominal board.
    #[test]
    fn statics_follow_the_boards_feedback_divider() {
        let lib = library("fbstatics", BOOST_AND_LOAD);
        let with_top = |top: &str| {
            let mut r1 = part("R1", top, vec![pin("1", 3), pin("2", 4)]);
            r1.lib_id = "Device:R".into();
            let mut r2 = part("R2", "50k", vec![pin("1", 3), pin("2", 1)]);
            r2.lib_id = "Device:R".into();
            board(
                &[(1, "GND"), (2, "VIN"), (3, "FB"), (4, "OUT")],
                vec![
                    part(
                        "U1",
                        "FB_BOOST",
                        vec![pin("1", 2), pin("2", 1), pin("3", 3), pin("4", 4)],
                    ),
                    r1,
                    r2,
                    part("U2", "LOAD_PART", vec![pin("1", 4), pin("2", 1)]),
                ],
            )
        };
        let (nominal, sampled) = (with_top("450k"), with_top("250k"));
        let (st_nominal, st_sampled) = (
            BoardStatics::of(&nominal, &lib),
            BoardStatics::of(&sampled, &lib),
        );
        assert!(st_nominal.describes(&nominal) && st_sampled.describes(&sampled));
        assert!(
            !st_nominal.describes(&sampled),
            "a changed value is a new board"
        );
        let out = |st: &BoardStatics<'_>, b: &ExtractedBoard| st.rails(b).regulators[&4];
        let (hi, lo) = (out(&st_nominal, &nominal), out(&st_sampled, &sampled));
        assert!((hi.max_v - 5.0).abs() < 1e-9, "{hi:?}");
        assert!((lo.max_v - 3.0).abs() < 1e-9, "{lo:?}");
        let judged = |b: &ExtractedBoard| {
            let found = envelope_lint(b, &lib);
            let mut found = found.of_check(LintCheck::OperatingEnvelope);
            found.any(|f| f.refs.contains(&"U2".to_string()))
        };
        assert!(judged(&nominal), "5.0 V is over the load's 4.5 V maximum");
        assert!(!judged(&sampled), "3.0 V is inside the load's range");
    }

    #[test]
    fn solved_level_uses_solver_voltage_tolerance_at_envelope_edge() {
        let b = board(
            &[(1, "GND"), (2, "UNNAMED_SUPPLY")],
            vec![part("U1", "RANGE_PART", vec![pin("1", 2), pin("2", 1)])],
        );
        let lib = library("drive", SUPPLY_CARD);
        let drives = BTreeMap::from([("UNNAMED_SUPPLY".to_string(), 3.8)]);
        let report = envelope_lint_with_drives(&b, &lib, &drives);
        let mut found = report.of_check(LintCheck::OperatingEnvelope);
        let finding = found
            .next()
            .expect("an exact out-of-band forced level must fire");
        assert!(finding.message.contains("3.8..3.8 V"));
        // (run min, run max): a steady rail, one grazing the edge within solver
        // tolerance, one that sags out and recovers to 3.3 V (its final level
        // alone would pass), and one that overshoots.
        for (min_v, max_v, fires) in [
            (3.3, 3.3, false),
            (3.2 - 1.0e-9, 3.3, false),
            (3.19, 3.3, true),
            (3.3, 3.7, true),
        ] {
            let solved =
                BTreeMap::from([("UNNAMED_SUPPLY".to_string(), RailRange { min_v, max_v })]);
            let findings = run_assumptions(
                &b,
                &BoardStatics::of(&b, &lib),
                &solved,
                (1.0e-3, 1.0e-6),
                (25.0, true),
            );
            assert_eq!(
                !findings.is_empty(),
                fires,
                "{min_v}..{max_v}: {findings:?}"
            );
        }
    }

    #[test]
    fn builtin_supply_between_recommended_and_abs_max_is_not_clean() {
        use Severity::{High, Medium};
        for (value, vin, volts, want) in [
            ("MIC5205-3.3", "1", 18.0, Medium),
            ("MIC5205-3.3", "1", 21.0, High),
            ("MCP73831", "4", 6.5, Medium),
            ("BL4054B-42TPRN", "4", 8.0, Medium),
            ("LTC4020EUHFPBF", "36", 58.0, Medium),
            ("NE555", "8", 17.0, Medium),
            ("ATmega1284P", "5", 5.8, Medium),
            ("LM5109", "3", 16.0, Medium),
            ("INA181A3IDBVR", "6", 5.8, Medium),
            ("INA181A3IDBVR", "6", 7.0, High),
            ("FSUSB42", "1", 5.0, Medium),
        ] {
            let b = board(&[(2, "VIN")], vec![part("U1", value, vec![pin(vin, 2)])]);
            let drives = BTreeMap::from([("VIN".to_string(), volts)]);
            let report = envelope_lint_with_drives(&b, &ModelLibrary::builtin(), &drives);
            let got = report.of_check(LintCheck::OperatingEnvelope).next();
            assert_eq!(got.map(|f| f.severity), Some(want), "{value} {volts} V");
        }
    }

    #[test]
    fn unknown_rail_abstains_once_per_net() {
        let b = board(
            &[(1, "GND"), (2, "UNNAMED_SUPPLY")],
            vec![
                part("U1", "RANGE_PART", vec![pin("1", 2), pin("2", 1)]),
                part("U2", "RANGE_PART", vec![pin("1", 2), pin("2", 1)]),
            ],
        );
        let report = envelope_lint(&b, &library("unknown", SUPPLY_CARD));
        let findings: Vec<_> = report.of_check(LintCheck::OperatingEnvelope).collect();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Low);
        assert!(findings[0].message.contains(
            "declare the level with a net_drive, or bind the regulator that defines this net"
        ));
    }

    #[test]
    fn rail_order_compares_lower_maximum_with_upper_minimum() {
        let card = r#"
[[models]]
id = "ordered_part"
kind = "digital"
description = "identity-only envelope fixture"
[models.match]
value_re = "^ORDERED_PART$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vcca"
"2" = "vccb"
"3" = "gnd"
[[models.envelope]]
kind = "rail_order"
lower = "vcca"
upper = "vccb"
basis = "Recommended Operating Conditions, VCCA <= VCCB note"
"#;
        let report = envelope_lint(
            &board(
                &[(1, "GND"), (2, "+3V3"), (3, "+3V0")],
                vec![part(
                    "U1",
                    "ORDERED_PART",
                    vec![pin("1", 2), pin("2", 3), pin("3", 1)],
                )],
            ),
            &library("order", card),
        );
        let finding = report
            .of_check(LintCheck::OperatingEnvelope)
            .next()
            .expect("3.465 V is not <= 2.85 V");
        assert!(finding.message.contains("3.465"));
        assert!(finding.message.contains("2.85 V"));
    }

    /// An enable pin tied to its own regulator input (the normal always-on
    /// wiring) sits on one net: its `en <= in` order holds with equality and
    /// the net's tolerance band is not a violation of itself. The same order
    /// still fires when EN is on a different, higher rail, and it is silent
    /// again when a zero-ohm link ties the two nets into one level.
    #[test]
    fn a_pin_on_the_same_net_as_its_rail_is_not_a_rail_order_finding() {
        let card = r#"
[[models]]
id = "en_part"
kind = "digital"
description = "identity-only enable fixture"
[models.match]
value_re = "^EN_PART$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "in"
"2" = "en"
"3" = "gnd"
[[models.envelope]]
kind = "rail_order"
lower = "en"
upper = "in"
basis = "Absolute Maximum Ratings, EN must not exceed IN"
"#;
        let lib = library("en_order", card);
        let findings = |nets: &[(i64, &str)], parts: Vec<Component>| {
            envelope_lint(&board(nets, parts), &lib)
                .of_check(LintCheck::OperatingEnvelope)
                .count()
        };
        let nets = [(1, "GND"), (2, "+5V"), (3, "+3V3")];
        let regulator = |en_net: i64| {
            part(
                "U1",
                "EN_PART",
                vec![pin("1", 2), pin("2", en_net), pin("3", 1)],
            )
        };
        assert_eq!(
            findings(&nets, vec![regulator(2)]),
            0,
            "EN on the IN net is normal"
        );
        assert_eq!(
            findings(&nets, vec![regulator(3)]),
            0,
            "EN on a lower rail satisfies en <= in"
        );
        let high = [(1, "GND"), (2, "+3V3"), (3, "+5V")];
        assert_eq!(
            findings(&high, vec![regulator(3)]),
            1,
            "EN on a rail above IN is still a rail-order finding"
        );
        let mut tie = part("R9", "0R", vec![pin("1", 2), pin("2", 4)]);
        tie.lib_id = "Device:R".into();
        let tied = [(1, "GND"), (2, "+5V"), (3, "+3V3"), (4, "EN_PRE")];
        assert_eq!(
            findings(&tied, vec![regulator(4), tie]),
            0,
            "a zero-ohm link makes EN and IN one level"
        );
    }

    const ROWS_CARD: &str = r#"
[[models]]
id = "rows_part"
kind = "digital"
match = { value_re = "^ROWS_PART$" }
params = { identity_only = true, warning = "identity only", unlocked_by = "validated behavior" }
pins = { "1" = "vdd", "2" = "inp", "3" = "inm" }
envelope = [
  { kind = "supply_range", pin = "vdd", min_v = 3.0, max_v = 3.6, conditions = "TA = −40 °C to 25 °C", basis = "ROC, cold" },
  { kind = "supply_range", pin = "vdd", min_v = 3.2, max_v = 3.6, conditions = "25 °C ≤ TA ≤ 85 °C", basis = "ROC, hot" },
  { kind = "input_common_mode", supply = "vdd", inputs = ["inp", "inm"], headroom_v = 1.5, conditions = "TA = 25 °C", basis = "VICR, 25 °C" },
  { kind = "input_common_mode", supply = "vdd", inputs = ["inp", "inm"], headroom_v = 2.0, conditions = "TA = 70 °C to 85 °C", basis = "VICR, hot" },
]
"#;

    // VDD 3.1 V breaks only the hot supply row, inputs at 1.4 V only the hot
    // 2.0 V headroom. An uncovered ambient, like the static lint, judges every
    // row, and says so. A defaulted 25 °C judges its own rows, but discloses
    // the default the hot common-mode row would have flagged.
    #[test]
    fn each_limit_is_the_row_printed_for_the_ambient() {
        use super::super::analog_inputs;
        let (nets, pins) = (
            [(1, "VDD"), (2, "INP"), (3, "INM")],
            vec![pin("1", 1), pin("2", 2), pin("3", 3)],
        );
        let b = board(&nets, vec![part("U1", "ROWS_PART", pins)]);
        let lib = library("rows", ROWS_CARD);
        let volts = [("VDD", 3.1), ("INP", 1.4), ("INM", 1.4)];
        let solved = BTreeMap::from(volts.map(|(n, x)| (n.to_string(), RailRange::point(x))));
        let v = |net: &str| solved.get(net).map(|r| r.max_v);
        let lint = envelope_lint_with_drives(&b, &lib, &BTreeMap::new()).findings;
        let analog = analog_inputs::analog_input_lint(&b, &lib, &v).findings;
        assert_eq!((lint.len(), analog.len()), (1, 1), "{lint:?} {analog:?}");
        // (ambient °C, stated, [supply, common-mode, caveats, default disclosures])
        for (ta, given, want) in [
            (0.0, true, [0, 1, 1, 0]),
            (25.0, true, [1, 0, 0, 0]),
            (29.0, true, [1, 0, 0, 0]),
            (85.0, true, [1, 1, 0, 0]),
            (125.0, true, [1, 1, 2, 0]),
            (25.0, false, [1, 0, 0, 1]),
        ] {
            let mut ids = run_assumptions(
                &b,
                &BoardStatics::of(&b, &lib),
                &solved,
                (0.0, 0.0),
                (ta, given),
            );
            ids.extend(analog_inputs::run_assumptions(
                &b,
                &BoardStatics::of(&b, &lib),
                &v,
                (ta, given),
            ));
            let ids: Vec<String> = ids.iter().map(|a| a.id().to_string()).collect();
            let count = |s: &str| ids.iter().filter(|id| id.contains(s)).count();
            let got = ["supply-env", "analog-input", "temperature", "ambient"].map(count);
            assert_eq!(got, want, "{ta} °C: {ids:?}");
        }
    }

    // An unreadable row (never valid on a user card, but builtins skip
    // validation) holds everywhere, yet must not stand in for the hot row no
    // other row covers at 25 °C; a one-sided `TA ≤ 85 °C` is no point at 85.
    #[test]
    fn an_unreadable_row_does_not_cover_for_its_channel() {
        let row = |at: &str| {
            let row = serde_json::json!({"kind": "supply_range", "pin": "vdd", "min_v": 3.2,
                "max_v": 3.6, "conditions": at});
            serde_json::from_value::<OperatingEnvelope>(row).unwrap()
        };
        let rows = [row("TA = 70 °C to 85 °C"), row("over temperature")];
        assert_eq!(binding(&rows, Some(25.0)).count(), 2);
        let rows = [row("TA = 25 °C"), row("TA ≤ 85 °C")];
        assert_eq!(binding(&rows, Some(25.0)).count(), 2);
    }

    #[test]
    fn builtin_txb0101_card_emits_the_sourced_rail_order_finding() {
        let b = board(
            &[(1, "GND"), (2, "V_SYS"), (3, "VSW")],
            vec![part(
                "U9",
                "TXB0101",
                vec![
                    pin("1", 2),
                    pin("2", 1),
                    pin("3", 0),
                    pin("4", 0),
                    pin("5", 0),
                    pin("6", 3),
                ],
            )],
        );
        let drives = BTreeMap::from([("V_SYS".to_string(), 3.3), ("VSW".to_string(), 3.0)]);
        let report = envelope_lint_with_drives(&b, &ModelLibrary::builtin(), &drives);
        let findings: Vec<_> = report
            .of_check(LintCheck::OperatingEnvelope)
            .filter(|finding| finding.severity == Severity::Medium)
            .collect();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(
            findings[0].message,
            "U9 requires vcca <= vccb, but 'V_SYS' reaches 3.3 V while 'VSW' falls to 3 V, so 3.3 V <= 3 V is false; basis: \"TI TXB0101 SCES639F, Section 5.3 Recommended Operating Conditions, note (2): VCCA must be less than or equal to VCCB\". Tie vcca to a rail no higher than vccb, or raise vccb so the full ranges do not cross."
        );
    }

    #[test]
    fn charger_chemistry_witness_turns_battery_setpoint_into_discharge_range() {
        let cards = r#"
[[models]]
id = "battery_part"
kind = "digital"
description = "identity-only battery load"
[models.match]
value_re = "^BATTERY_PART$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vdd"
"2" = "gnd"
[[models.envelope]]
kind = "supply_range"
pin = "vdd"
min_v = 3.5
max_v = 5.5
basis = "Recommended Operating Conditions, supply row"

[[models]]
id = "liion_charger"
kind = "vreg"
description = "identity-only Li-ion charger"
[models.match]
value_re = "^LIION_CHARGER$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated charger behavior"
battery_chemistry = "liion"
vout_setpoint_low = 4.16
vout_setpoint_high = 4.23
[models.pins]
"1" = "bat"
"2" = "gnd"
"#;
        let report = envelope_lint(
            &board(
                &[(1, "GND"), (2, "CELL")],
                vec![
                    part("D1", "BATTERY_PART", vec![pin("1", 2), pin("2", 1)]),
                    part("U1", "LIION_CHARGER", vec![pin("1", 2), pin("2", 1)]),
                ],
            ),
            &library("battery", cards),
        );
        let finding = report
            .of_check(LintCheck::OperatingEnvelope)
            .find(|finding| finding.refs == ["D1"])
            .expect("the 3.0 V Li-ion floor must violate the 3.5 V load floor");
        assert!(finding.message.contains("3..4.23 V"));
    }

    #[test]
    fn battery_without_chemistry_abstains_and_names_the_unlock() {
        let b = board(
            &[(1, "GND"), (2, "CELL")],
            vec![
                part("D1", "RANGE_PART", vec![pin("1", 2), pin("2", 1)]),
                part("J1", "Battery holder", vec![pin("1", 2), pin("2", 1)]),
            ],
        );
        let report = envelope_lint(&b, &library("battery_unknown", SUPPLY_CARD));
        let finding = report
            .of_check(LintCheck::OperatingEnvelope)
            .next()
            .expect("battery topology without chemistry must abstain");
        assert_eq!(finding.severity, Severity::Low);
        assert!(finding
            .message
            .contains("no charger card declares its chemistry"));
        assert!(finding.message.contains("battery_chemistry"));
    }

    /// A chip that only consumes backup power on its VBAT pin (an MCU's or
    /// RTC's backup domain) strapped to a regulated rail does not make that
    /// rail a battery net: nothing about the cell's chemistry is asked for.
    #[test]
    fn a_backup_domain_vbat_pin_does_not_make_a_battery_net() {
        let card = format!(
            "{SUPPLY_CARD}{}",
            r#"
[[models]]
id = "backup_mcu"
kind = "digital"
description = "MCU with a backup-domain pin"
[models.match]
value_re = "^BACKUP_MCU$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vdd"
"2" = "gnd"
"3" = "vbat"
"#
        );
        let b = board(
            &[(1, "GND"), (2, "+3V3")],
            vec![
                part(
                    "U1",
                    "BACKUP_MCU",
                    vec![pin("1", 2), pin("2", 1), pin("3", 2)],
                ),
                part("U2", "RANGE_PART", vec![pin("1", 2), pin("2", 1)]),
            ],
        );
        let report = envelope_lint(&b, &library("backup_vbat", &card));
        assert!(
            report
                .of_check(LintCheck::OperatingEnvelope)
                .all(|f| !f.message.contains("identifies a battery net")),
            "{:?}",
            report.findings
        );
    }

    /// Three LMV7219s (1.1 mA each, 2.7 V minimum supply) on rail 'VA', fed
    /// from '+5V' through R1 of `feed_ohms`, or straight from '+5V' when
    /// `None`. Bound and DC-solved for real; returns the board and every net's
    /// solved level.
    fn comparator_rail(feed_ohms: Option<&str>) -> (ExtractedBoard, BTreeMap<String, RailRange>) {
        use hauksbee_solve::{dc_operating_point, SolverOptions, Workspace};
        let (gnd, five, va) = (1, 2, 3);
        let vcc = if feed_ohms.is_some() { va } else { five };
        let mut parts: Vec<Component> = (1..=3)
            .map(|i| {
                let out = 3 + i;
                let pads = [("1", out), ("2", gnd), ("3", gnd), ("4", gnd), ("5", vcc)];
                super::super::test_support::part(&format!("U{i}"), "LMV7219", &pads)
            })
            .collect();
        if let Some(ohms) = feed_ohms {
            let pads = [("1", five), ("2", va)];
            parts.push(super::super::test_support::part("R1", ohms, &pads));
        }
        let nets = [
            (gnd, "GND"),
            (five, "+5V"),
            (va, "VA"),
            (4, "O1"),
            (5, "O2"),
            (6, "O3"),
        ];
        let b = board(&nets, parts);
        let bound = hauksbee_bind::binder::bind_board(&b, &ModelLibrary::builtin());
        let mut ws = Workspace::new(&bound.circuit);
        dc_operating_point(&mut ws, &bound.circuit, &SolverOptions::default())
            .expect("the comparator rail solves");
        let solved = bound
            .net_nodes
            .iter()
            .filter_map(|(name, node)| {
                let v = ws.x.get(ws.layout.node(*node)?).copied()?;
                Some((name.clone(), RailRange::point(v)))
            })
            .collect();
        (b, solved)
    }

    #[test]
    fn a_rail_its_parts_own_draw_starves_through_a_series_resistor_is_one_finding() {
        let lib = ModelLibrary::builtin();
        let tol = {
            let o = hauksbee_solve::SolverOptions::default();
            (o.reltol, o.vntol)
        };
        // Legal: a 10 ohm feed drops 33 mV; a direct feed drops nothing.
        for feed in [Some("10"), None] {
            let (b, solved) = comparator_rail(feed);
            let report = envelope_lint_with_tolerance(
                &b,
                &BoardStatics::of(&b, &lib),
                &solved,
                tol,
                Some(25.0),
            );
            let starved = |f: &&LintFinding| f.message.contains("is starved");
            assert_eq!(
                report.findings.iter().filter(starved).count(),
                0,
                "{feed:?}: {report:?}"
            );
            let rail = solved
                .get(if feed.is_some() { "VA" } else { "+5V" })
                .unwrap();
            assert!(rail.min_v > 4.9, "{feed:?}: the rail is up, got {rail:?}");
            let evidence =
                run_assumptions(&b, &BoardStatics::of(&b, &lib), &solved, tol, (25.0, true));
            assert!(evidence.is_empty(), "{feed:?}: {evidence:?}");
        }

        // Faulty: 1 kohm. The three parts' 3.3 mA would drop 3.3 V, more than
        // the 2.3 V between +5V and their 2.7 V minimum, so VA cannot come up.
        let (b, solved) = comparator_rail(Some("1k"));
        let va = solved["VA"].max_v;
        assert!(va < 2.7, "VA must solve below the parts' minimum, got {va}");
        let report =
            envelope_lint_with_tolerance(&b, &BoardStatics::of(&b, &lib), &solved, tol, Some(25.0));
        assert_eq!(
            report.findings.len(),
            1,
            "one rail finding, not one per part: {report:?}"
        );
        let f = &report.findings[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.refs, ["R1", "U1", "U2", "U3"]);
        assert_eq!(f.nets, ["VA", "+5V"]);
        for says in [
            "'VA' is starved",
            &format!("reaches only {va:.3} V"),
            "2.7 V minimum supply of the 3 part(s)",
            "3.3 mA static supply draw through R1 (1.0k ohm) from '+5V'",
            "drops 3.3 V",
            "no more than 697 ohm",
        ] {
            assert!(f.message.contains(says), "{says:?} in {}", f.message);
        }
        // As run evidence it is one measured violation, keyed on the feed.
        let evidence = run_assumptions(&b, &BoardStatics::of(&b, &lib), &solved, tol, (25.0, true));
        let kinds: Vec<_> = evidence.iter().map(Assumption::kind).collect();
        assert_eq!(
            kinds,
            [hauksbee_ir::evidence::AssumptionKind::OperatingLimitViolated],
            "{evidence:?}"
        );
    }

    #[test]
    fn a_rail_with_no_level_is_estimated_starved_from_the_cards_draw() {
        let lib = ModelLibrary::builtin();
        let starved = |report: &NetLintReport| {
            let f = |f: &&LintFinding| f.message.contains("is likely starved");
            report
                .findings
                .iter()
                .filter(f)
                .cloned()
                .collect::<Vec<_>>()
        };
        // 1 kohm feed: the three parts' 3.3 mA alone would drop 3.3 V of 2.3 V.
        let (b, _) = comparator_rail(Some("1k"));
        let found = starved(&envelope_lint(&b, &lib));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::High);
        assert_eq!(found[0].nets, ["VA", "+5V"]);
        // +5V is judged at its 5.25 V top (2.55 V of headroom) on a static surface.
        for says in [
            "estimate",
            "3.3 mA",
            "R1 (1.0k ohm)",
            "no more than 773 ohm",
        ] {
            assert!(found[0].message.contains(says), "{says:?}: {found:?}");
        }
        // 10 ohm feed: fine. No feed at all: nothing to blame.
        for feed in [Some("10"), None] {
            let (b, _) = comparator_rail(feed);
            assert!(starved(&envelope_lint(&b, &lib)).is_empty(), "{feed:?}");
        }
        // An unmodelled part on the rail could source it: no estimate.
        let (mut b, _) = comparator_rail(Some("1k"));
        let va = b.nets.iter().find(|n| n.name == "VA").unwrap().id;
        b.components.push(super::super::test_support::part(
            "J1",
            "WIDGET-X9",
            &[("1", va), ("2", 1), ("3", 1)],
        ));
        assert!(starved(&envelope_lint(&b, &lib)).is_empty());
        // The overlay's rework of the feed, applied statically, clears it.
        let (mut b, _) = comparator_rail(Some("1k"));
        hauksbee_bind::asbuilt::AsBuiltOverlay::parse(
            "[[replace]]\nref = \"R1\"\nset = { ohms = 0.001 }\n",
            "test",
        )
        .and_then(|o| o.apply_to_board(&mut b, &lib))
        .unwrap();
        assert!(starved(&envelope_lint(&b, &lib)).is_empty());
    }
    /// A supply net whose name seems to state a voltage it does not state
    /// readably is named on every board, whether or not a part on it has an
    /// operating envelope: the same typo was a visible note beside an
    /// enveloped part and silent on a board of plain resistors.
    #[test]
    fn an_unreadable_rail_name_is_an_abstention_on_a_board_with_no_enveloped_part() {
        let lint = |rail: &str| {
            let loads = vec![
                part("R1", "10k", vec![pin("1", 2), pin("2", 1)]),
                part("R2", "10k", vec![pin("1", 2), pin("2", 1)]),
            ];
            envelope_lint(
                &board(&[(1, "GND"), (2, rail)], loads),
                &ModelLibrary::builtin(),
            )
            .of_check(LintCheck::OperatingEnvelope)
            .cloned()
            .collect::<Vec<_>>()
        };
        for (rail, why) in [
            ("+1KV", "scale prefix"),
            ("+5mV", "scale prefix"),
            ("+1e3V", "exponent"),
            ("VCC_1E3V", "exponent"),
            ("+5V5V", "second V"),
            ("+1V8_3V3", "two voltages"),
        ] {
            let found = lint(rail);
            assert_eq!(found.len(), 1, "{rail}: {found:?}");
            assert_eq!(found[0].severity, Severity::Low);
            assert!(found[0].is_abstention(), "{rail}: {}", found[0].message);
            assert!(
                found[0].message.contains(why),
                "{rail}: {}",
                found[0].message
            );
            assert_eq!(found[0].nets, vec![rail.to_string()]);
            assert_eq!(found[0].refs, vec!["R1".to_string()]);
        }
        // Legal controls: a readable name, a name that states nothing, and a
        // tap named after a rail say nothing here.
        for rail in ["+5V", "+3V3_3V3", "VDD", "+12V_FB", "+1KV_EN"] {
            assert!(lint(rail).is_empty(), "{rail}: {:?}", lint(rail));
        }
    }
}
