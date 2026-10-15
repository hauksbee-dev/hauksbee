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
    is_resistor, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_ir::evidence::{Assumption, ModelUncertainty};
use hauksbee_models::derating::{temp_span, TempRef};
use hauksbee_models::value::parse_value;
use hauksbee_models::{
    ComponentKind, EnvelopeSeverity, ModelEntry, ModelLibrary, OperatingEnvelope,
};

use super::BoardStatics;
use hauksbee_bind::binder::{is_ground, power_rail_voltage, resolve};

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

/// Ohms of a fitted two-terminal resistor between nets `a` and `b` (either
/// way round), or `None` when the part is not one.
fn resistor_between(c: &Component, a: i64, b: impl Fn(i64) -> bool) -> Option<f64> {
    if !AssemblyState::of(c).is_present() || !is_resistor(c) {
        return None;
    }
    let nets: Vec<i64> = c.pins.iter().filter_map(|pin| pin.net).collect();
    let [first, second] = nets[..] else {
        return None;
    };
    let joins = (first == a && b(second)) || (second == a && b(first));
    let ohms = parse_value(&c.value).map(|value| value.si)?;
    (joins && ohms.is_finite() && ohms > 0.0).then_some(ohms)
}

/// Parallel combination of `ohms`, `None` when empty.
fn parallel(ohms: impl Iterator<Item = f64>) -> Option<f64> {
    let g: f64 = ohms.map(f64::recip).sum();
    (g > 0.0).then(|| g.recip())
}

/// The output a feedback-regulated converter sets with the board's own
/// divider: `Vref * (1 + Rtop / Rbottom)`, Rtop from the feedback pin to the
/// output (or a net tied to it) and Rbottom from the feedback pin to ground,
/// across the card's stated reference interval when it gives one. `None`
/// when the card has no feedback pin or the divider is not on the board, in
/// which case the card's own set point stands.
fn feedback_output_range(
    board: &ExtractedBoard,
    tied: &BTreeMap<i64, BTreeSet<i64>>,
    (comp, model): (&Component, &ModelEntry),
    uncertainty: &[ModelUncertainty],
    out_net: i64,
) -> Option<RailRange> {
    let feedback = model.behavioral.converter.as_ref()?.feedback.as_ref()?;
    let fb_net = role_net_id(comp, model, &feedback.pin)?;
    let on_output = |n: i64| n == out_net || tied.get(&out_net).is_some_and(|set| set.contains(&n));
    let grounded = |n: i64| board.net(n).is_some_and(|net| is_ground(&net.name));
    let top = parallel(
        board
            .components
            .iter()
            .filter_map(|c| resistor_between(c, fb_net, on_output)),
    )?;
    let bottom = parallel(
        board
            .components
            .iter()
            .filter_map(|c| resistor_between(c, fb_net, grounded)),
    )?;
    let (lo, hi) = uncertainty
        .iter()
        .find_map(|u| match u {
            ModelUncertainty::Interval {
                parameter,
                low,
                high,
                ..
            } if parameter == "feedback_reference" && low <= high => Some((*low, *high)),
            _ => None,
        })
        .unwrap_or((feedback.vref_v, feedback.vref_v));
    let gain = 1.0 + top / bottom;
    Some(RailRange {
        min_v: lo * gain,
        max_v: hi * gain,
    })
}

/// Nets joined by a fitted net tie or near-zero-ohm link, each net mapped to
/// every other net of its tied group.
fn tied_nets(board: &ExtractedBoard) -> BTreeMap<i64, BTreeSet<i64>> {
    let is_tie = |c: &Component| {
        let text = format!("{} {}", c.lib_id, c.footprint).to_ascii_lowercase();
        let named_tie = text.contains("nettie") || text.contains("net_tie");
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
    batteries: BTreeMap<i64, Option<RailRange>>,
}

impl Rails {
    pub(crate) fn of(board: &ExtractedBoard, st: &BoardStatics<'_>) -> Self {
        Self {
            regulators: regulator_ranges(board, st),
            batteries: battery_ranges(board, st),
        }
    }
}

fn regulator_ranges(board: &ExtractedBoard, st: &BoardStatics<'_>) -> BTreeMap<i64, RailRange> {
    let mut ranges = BTreeMap::new();
    let tied = tied_nets(board);
    for (comp, model) in st.parts(board) {
        let Some(range) = model_output_range(model) else {
            continue;
        };
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
            let feedback = feedback_output_range(board, &tied, (comp, model), &uncertainty, net_id);
            ranges.insert(net_id, feedback.unwrap_or(range));
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
            }
        }
    }
    ranges
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
    envelope_lint_with_tolerance(board, &st, &points.collect(), (0.0, 0.0), None)
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
    let mut report = NetLintReport::default();
    let mut abstained_nets = BTreeSet::new();
    let mut starved: BTreeMap<i64, Vec<Starved>> = BTreeMap::new();
    let mut unleveled: BTreeMap<i64, Vec<Starved>> = BTreeMap::new();

    for (comp, model) in st.parts(board) {
        let net_of = |role| role_net_id(comp, model, role).and_then(|id| board.net(id));
        for envelope in binding(&model.envelope, ambient_c) {
            match envelope {
                OperatingEnvelope::SupplyRange {
                    pin,
                    min_v,
                    max_v,
                    abs_max_v,
                    basis,
                    ..
                } => {
                    let Some(net) = net_of(pin) else { continue };
                    let net_id = net.id;
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
                            if matches!(knowledge, RailKnowledge::Unknown) {
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
                    let fix = if assumed {
                        let cap = if nominal_inside && !above_abs_max {
                            Severity::Low
                        } else {
                            Severity::Medium
                        };
                        severity = at_most(severity, cap);
                        format!(
                            " The range is assumed: ±5 % around the {nominal:.3} V the net name \
                             implies, since no regulator, battery or solved run states this \
                             rail's level; confirm the real supply tolerance."
                        )
                    } else if declared_only && nominal_inside && !above_abs_max {
                        // The rail's own nominal suits the part: moving the
                        // pin to another rail would change the circuit, while
                        // the margin lives in the rail's set point.
                        format!(
                            " The {nominal:.3} V nominal is inside the window; only the rail's \
                             tolerance corner leaves it, so tighten or lower the set point \
                             (feedback divider, regulator grade) or use a part rated for the \
                             full range."
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
                    if resolver.solved(net_id).is_some() && range.max_v < *min_v - min_tol {
                        starved.entry(net_id).or_default().push(Starved {
                            finding: report.findings.len(),
                            reference: comp.reference.clone(),
                            min_v: *min_v,
                            draw_a: static_draw_a(model),
                        });
                    }
                    report.findings.push(LintFinding {
                        check: LintCheck::OperatingEnvelope,
                        severity,
                        message: format!(
                            "{} role {pin} on '{}' presents {:.3}..{:.3} V, outside the {:.3}..{:.3} V operating envelope; basis: \"{basis}\".{fix}",
                            comp.reference,
                            net.name,
                            range.min_v,
                            range.max_v,
                            min_v,
                            max_v
                        ),
                        refs: vec![comp.reference.clone()],
                        nets: vec![net.name.clone()],
                    });
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
                    report.findings.push(LintFinding {
                        check: LintCheck::OperatingEnvelope,
                        severity: finding_severity(envelope.severity()),
                        message: format!(
                            "{} requires {lower} <= {upper}, but '{}' reaches {:.3} V while '{}' falls to {:.3} V, so {:.3} <= {:.3} is false; basis: \"{basis}\". Tie {lower} to a rail no higher than {upper}, or raise {upper} so the full ranges do not cross.",
                            comp.reference,
                            lower_net.name,
                            known[0].max_v,
                            upper_net.name,
                            known[1].min_v,
                            known[0].max_v,
                            known[1].min_v
                        ),
                        refs: vec![comp.reference.clone()],
                        nets: vec![lower_net.name.clone(), upper_net.name.clone()],
                    });
                }
                // Supply-relative input windows need the level ON the input
                // pins, not the rail interval this resolver derives: they are
                // evaluated in `super::analog_inputs`.
                OperatingEnvelope::InputCommonMode { .. } => {}
            }
        }
    }
    fold_starved_rails(&resolver, starved, &mut report);
    estimate_starved_rails(&resolver, st, &unleveled, &mut report);
    report
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
    let (count, milliamps, feed_v) = (refs.len(), draw_a * 1e3, feed.max_v);
    let fed_by = resistors.join(" and ");
    let message = reached.map_or_else(
        || {
            format!(
                "'{}' is likely starved (estimate from the cards' stated supply current; no operating point was solved): the {count} part(s) it powers ({listed}) need at least {need_v:.3} V, but their {milliamps:.1} mA static supply draw alone through {fed_by} from '{feed_name}' ({feed_v:.3} V) would drop {drop_v:.1} V, more than the {headroom_v:.3} V of headroom, so the rail cannot come up and those parts would not run. Feed the rail through no more than {max_ohms} ohm in total, or directly from '{feed_name}'.",
                net.name,
            )
        },
        |rail| {
            format!(
                "'{}' is starved: it reaches only {:.3} V, below the {need_v:.3} V minimum supply of the {count} part(s) it powers ({listed}). Their {milliamps:.1} mA static supply draw through {fed_by} from '{feed_name}' ({feed_v:.3} V) drops {drop_v:.1} V, more than the {headroom_v:.3} V of headroom, so the rail cannot come up and those parts do not run. Feed the rail through no more than {max_ohms} ohm in total, or directly from '{feed_name}'.",
                net.name, rail.max_v,
            )
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
        let dir =
            std::env::temp_dir().join(format!("hauksbee_envelope_{tag}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
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
        assert!(msg.contains("5.000..5.000"), "{msg}");
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
        assert!(finding.message.contains("3.800..3.800"));
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
        assert!(finding.message.contains("2.850"));
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
            "U9 requires vcca <= vccb, but 'V_SYS' reaches 3.300 V while 'VSW' falls to 3.000 V, so 3.300 <= 3.000 is false; basis: \"TI TXB0101 SCES639F, Section 5.3 Recommended Operating Conditions, note (2): VCCA must be less than or equal to VCCB\". Tie vcca to a rail no higher than vccb, or raise vccb so the full ranges do not cross."
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
        assert!(finding.message.contains("3.000..4.230"));
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
            "2.700 V minimum supply of the 3 part(s)",
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
        .and_then(|o| o.apply_to_board(&mut b))
        .unwrap();
        assert!(starved(&envelope_lint(&b, &lib)).is_empty());
    }
}
