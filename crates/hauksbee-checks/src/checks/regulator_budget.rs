//! A linear regulator's current and headroom budget, and a supply pin's feed,
//! read from what the cards state.
//!
//! The DC solve only draws what the bound models stamp, and a regulator's
//! rating is only judged where it carries a stamped load. This check adds the
//! rail arithmetic the cards alone support, with no solve, and states each
//! shortfall with the numbers and the card behind it:
//!
//! - **Load over rating.** The supply currents the cards state for the parts
//!   fed from a linear regulator's output (`supply_current_a` on the rail its
//!   draw law names, `supply_static_ua` on its `supply_pin`, a downstream
//!   linear regulator's own load and `iq_a`) add up to more than the
//!   regulator's `max_current_a`. The sum is a lower bound, so a rail over the
//!   rating on the stated parts alone is over it; a part with no stated current
//!   is named as making the real draw higher, never counted as zero amps in a
//!   claim of margin. A rail under the rating is silent: not knowing a load is
//!   no finding.
//! - **Headroom.** The regulator's input rail, at the lowest level the board
//!   declares for it, less the card's `dropout_v`, is under the output it sets.
//!   Below the output itself the regulator cannot regulate at any load (High);
//!   between the output and the output plus the dropout the regulator drops out
//!   at the datasheet's load (a note below half the rated load, Medium above).
//!   An input level that is only the band its net name implies caps the finding
//!   at Medium, as the operating-envelope check does.
//! - **Series feed.** An IC whose card states a supply current and a minimum
//!   supply, powered through a resistor from a declared higher rail, sees the
//!   rail's lowest level less the drop of every stated current on that net
//!   across the resistor. Under the part's minimum, the part is not running
//!   inside its envelope. A rail the operating-envelope check already reports
//!   as starved is that finding's.
//! - **Motor driver full scale.** A driver whose card programs a current limit
//!   from a resistor (`[models.current_program]`) and names a motor supply pin
//!   (`vm`, `vbb`, `vmot`) can draw that programmed current from the regulator
//!   that feeds the motor rail. The regulator's rating is held to it; the
//!   draw is conditional on the motor loading the driver, so it is a Medium
//!   finding that says so.
//! - **Reference-programmed driver.** A driver whose card gives its winding
//!   current as a reference voltage over a sense resistor
//!   (`equation = "reference_over_sense"`: A4988, DRV8825) has that full scale
//!   read from the board: the level on each reference pin, from a rail, a
//!   regulator or a resistor divider off known rails, over the one sense
//!   resistor to ground. A full scale over the part's output current rating, a
//!   reference pin at 0 V (no winding current at all) and a reference pin with
//!   nothing driving it (the trip level is then undefined) are findings; the
//!   same full scale is held to the supply as above. A reference that comes
//!   from a trimmer, a part, a pin of the driver itself or a filtered logic
//!   pin, and a sense path that is not exactly one resistor, are not judged.
//!
//! ## What it never claims
//!
//! A rail with another source on it (a second regulator, a diode or pass
//! transistor from another rail, a pin typed as an output) is not judged: the
//! load may be shared. Currents come only from cards, and a card value outside
//! its physical range is read as unstated. A regulator whose card is a stand-in
//! for the part is not judged on its rating.
//!

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_bind::binder::{is_ground, power_rail_voltage};
use hauksbee_bind::program_resistor::ProgramReading;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    is_capacitor, is_test_point_or_hole, Component, ExtractedBoard, LintCheck, LintFinding,
    NetLintReport, Severity, REMEDY_MARKER,
};
use hauksbee_ir::units::{format_amps, format_volts};
use hauksbee_models::behavioral::LawKind;
use hauksbee_models::rating_basis::Limit;
use hauksbee_models::schema::{CurrentProgramEquation, CurrentProgramSemantics};
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary, OperatingEnvelope};

use super::ampacity::{adjacent_sense_resistor, ExpectedSenseFar};
use super::envelope::{
    binding, role_net_id, series_feeds, tied_nets, RailRange, RailResolver, IC_SUPPLY_ROLES,
};
use super::BoardStatics;

/// Pad roles that put a motor supply on a driver (the rail its load current
/// is drawn from).
const MOTOR_SUPPLY_ROLES: [&str; 7] = ["vm", "vbb", "vmot", "vbb1", "vbb2", "vma", "vmb"];

/// Pad roles that can drive a rail, so a part with one on a rail is a source
/// of it.
const SOURCE_ROLES: [&str; 4] = ["out", "vout", "bat", "vbat"];

/// Slack on every comparison against a card's figure: one part in a billion,
/// so a load exactly at the rating meets it.
const SLACK: f64 = 1e-9;

/// The load the cards state on one rail.
#[derive(Default)]
struct Loads {
    /// Each stated draw, `(label, amps)`.
    items: Vec<(String, f64)>,
    /// Parts on the rail whose current no card states.
    silent: Vec<String>,
}

impl Loads {
    fn total_a(&self) -> f64 {
        self.items.iter().map(|(_, a)| a).sum()
    }

    fn listed(&self) -> String {
        const SHOWN: usize = 6;
        let mut shown: Vec<String> = self
            .items
            .iter()
            .take(SHOWN)
            .map(|(who, a)| format!("{who} {}", format_amps(*a)))
            .collect();
        if self.items.len() > SHOWN {
            shown.push(format!("and {} more", self.items.len() - SHOWN));
        }
        shown.join(", ")
    }

    /// The sentence on parts of unknown draw, empty when there are none.
    fn silent_note(&self) -> String {
        if self.silent.is_empty() {
            return String::new();
        }
        let mut names = self.silent.clone();
        names.sort();
        names.dedup();
        format!(
            " {} state{} no supply current, so the real draw is higher still.",
            names.join(", "),
            if names.len() == 1 { "s" } else { "" }
        )
    }
}

/// A fitted linear regulator the budget is read against.
struct Reg<'a> {
    comp: &'a Component,
    model: &'a ModelEntry,
    input: Option<i64>,
    output: i64,
}

/// The supply currents a card states for a part, as `(net id, amps)`: the
/// current its draw law names a pin for, and its static current on its
/// `supply_pin` (or its first supply role). One net is counted once, at the
/// larger of the two, so a card stating both for the same pin is not doubled.
fn supply_draws(comp: &Component, model: &ModelEntry) -> Vec<(i64, f64)> {
    let mut out: Vec<(i64, f64)> = Vec::new();
    let mut add = |net: i64, amps: f64| {
        if !amps.is_finite() || amps <= 0.0 {
            return;
        }
        match out.iter_mut().find(|(n, _)| *n == net) {
            Some(slot) => slot.1 = slot.1.max(amps),
            None => out.push((net, amps)),
        }
    };
    if let Some(amps) = super::plausible_param(model, "supply_current_a") {
        for law in &model.behavioral.laws {
            if law.kind == LawKind::Current && law.expr.contains("supply_current_a") {
                if let Some(net) = role_net_id(comp, model, &law.a) {
                    add(net, amps);
                }
            }
        }
    }
    if let Some(ua) = super::plausible_param(model, "supply_static_ua") {
        let by_pad = |pad: &str| {
            comp.pins
                .iter()
                .find(|p| p.number == pad)
                .and_then(|p| p.net)
                .filter(|n| *n != 0)
        };
        let net = model
            .params
            .get_str("supply_pin")
            .and_then(|pin| by_pad(pin).or_else(|| role_net_id(comp, model, pin)))
            .or_else(|| {
                IC_SUPPLY_ROLES
                    .iter()
                    .find_map(|r| role_net_id(comp, model, r))
            });
        if let Some(net) = net {
            add(net, ua * 1e-6);
        }
    }
    out
}

/// Whether `model` is a linear regulator this check reads: not a switching
/// converter, not a charger.
fn is_linear(model: &ModelEntry) -> bool {
    // A power-management IC (a battery, system or bus pin beside the
    // regulator's own) is several converters and a charger behind one `out`
    // and `in`: not a regulator with an LDO's dropout.
    const PMIC_ROLES: [&str; 3] = ["vbat", "vsys", "vbus"];
    model.kind == ComponentKind::Vreg
        && model.behavioral.converter.is_none()
        && !model
            .current_program
            .as_ref()
            .is_some_and(|p| p.semantics == CurrentProgramSemantics::RegulatedCurrent)
        && !model
            .pins
            .values()
            .any(|role| PMIC_ROLES.iter().any(|r| role.eq_ignore_ascii_case(r)))
}

struct Budget<'a> {
    board: &'a ExtractedBoard,
    parts: Vec<(&'a Component, &'a ModelEntry)>,
    tied: BTreeMap<i64, BTreeSet<i64>>,
    resolver: RailResolver<'a>,
}

impl<'a> Budget<'a> {
    fn group(&self, net: i64) -> BTreeSet<i64> {
        let mut g = self.tied.get(&net).cloned().unwrap_or_default();
        g.insert(net);
        g
    }

    fn net_name(&self, net: i64) -> String {
        self.board
            .net(net)
            .map_or_else(|| "?".to_string(), |n| n.name.clone())
    }

    fn model_of(&self, comp: &Component) -> Option<&'a ModelEntry> {
        self.parts
            .iter()
            .find(|(c, _)| std::ptr::eq(*c, comp))
            .map(|(_, m)| *m)
    }

    /// The fitted linear regulators with an output on a real, non-ground net.
    fn regulators(&self) -> Vec<Reg<'a>> {
        self.parts
            .iter()
            .filter(|(_, model)| is_linear(model))
            .filter_map(|(comp, model)| {
                let output = role_net_id(comp, model, "out")?;
                let name = self.net_name(output);
                if is_ground(&name) || super::is_unconnected_net(&name) {
                    return None;
                }
                let input = role_net_id(comp, model, "in");
                // An input and output on one rail is a part wired as something
                // else (a bypass, a PMIC rail fed back), not a regulation.
                if input.is_some_and(|i| self.group(i).contains(&output)) {
                    return None;
                }
                // An output net named for a voltage the card does not set
                // says the pad roles do not match this footprint (a pinout
                // the card does not know): no budget is read from it.
                if self.name_contradicts(output) {
                    return None;
                }
                Some(Reg {
                    comp,
                    model,
                    input,
                    output,
                })
            })
            .collect()
    }

    /// Whether the net's own name states a voltage the regulator's card does
    /// not set: `+12V` on the output of a 5 V part.
    fn name_contradicts(&self, net: i64) -> bool {
        let Some(named) = power_rail_voltage(&self.net_name(net)) else {
            return false;
        };
        let Some((range, _)) = self.resolver.declared_levels(net) else {
            return false;
        };
        let set = f64::midpoint(range.min_v, range.max_v);
        set.is_finite() && set != 0.0 && (named.abs() - set.abs()).abs() > 0.15 * set.abs()
    }

    /// What the cards state is drawn from the rail `net` belongs to, leaving
    /// out the part `skip`. A linear regulator fed from the rail adds its own
    /// quiescent current and everything on its output, which passes through it.
    fn loads_on(&self, net: i64, skip: &str, visiting: &mut BTreeSet<i64>) -> Loads {
        let group = self.group(net);
        let mut loads = Loads::default();
        if !visiting.insert(net) {
            return loads;
        }
        for comp in &self.board.components {
            if comp.reference == skip || !AssemblyState::of(comp).is_present() {
                continue;
            }
            if !comp
                .pins
                .iter()
                .any(|p| p.net.is_some_and(|n| group.contains(&n)))
            {
                continue;
            }
            let Some(model) = self.model_of(comp) else {
                if is_unmodelled_load(comp) {
                    loads.silent.push(comp.reference.clone());
                }
                continue;
            };
            if model.kind == ComponentKind::Vreg {
                self.regulator_load(comp, model, &group, visiting, &mut loads);
                continue;
            }
            if !model.ratings.basis.judges(Limit::Envelope) {
                continue;
            }
            let draws = supply_draws(comp, model);
            let mut counted = false;
            for (n, amps) in draws {
                if group.contains(&n) {
                    loads.items.push((comp.reference.clone(), amps));
                    counted = true;
                }
            }
            if !counted && is_supply_pad_on(comp, model, &group) {
                loads.silent.push(comp.reference.clone());
            }
        }
        visiting.remove(&net);
        loads
    }

    /// A regulator whose input is on the rail: its quiescent current, and, for
    /// a linear one, what its output feeds.
    fn regulator_load(
        &self,
        comp: &Component,
        model: &ModelEntry,
        group: &BTreeSet<i64>,
        visiting: &mut BTreeSet<i64>,
        loads: &mut Loads,
    ) {
        if role_net_id(comp, model, "in").is_none_or(|n| !group.contains(&n)) {
            return;
        }
        if !is_linear(model) {
            loads.silent.push(comp.reference.clone());
            return;
        }
        if let Some(iq) = super::plausible_param(model, "iq_a").filter(|a| *a > 0.0) {
            loads
                .items
                .push((format!("{} (own quiescent)", comp.reference), iq));
        }
        if let Some(out) = role_net_id(comp, model, "out") {
            let inner = self.loads_on(out, &comp.reference, visiting);
            for (who, amps) in inner.items {
                loads
                    .items
                    .push((format!("{who} via {}", comp.reference), amps));
            }
            loads.silent.extend(inner.silent);
        }
    }

    /// Another source of the rail `group` that shares its load: a regulator or
    /// battery output, a diode or transistor from another power rail, a pin the
    /// schematic types as an output. `reg` is the regulator under judgement.
    fn rival_source(&self, group: &BTreeSet<i64>, reg: &str) -> Option<String> {
        let other_rail = |c: &Component| {
            c.pins.iter().filter_map(|p| p.net).any(|n| {
                !group.contains(&n)
                    && self.board.net(n).is_some_and(|net| {
                        !is_ground(&net.name)
                            && (power_rail_voltage(&net.name).is_some()
                                || self.resolver.defined_by_a_regulator(n))
                    })
            })
        };
        for comp in &self.board.components {
            if comp.reference == reg || !AssemblyState::of(comp).is_present() {
                continue;
            }
            let on_rail = |p: &&hauksbee_extract::Pin| p.net.is_some_and(|n| group.contains(&n));
            if !comp.pins.iter().any(|p| on_rail(&p)) {
                continue;
            }
            let prefix: String = comp
                .reference
                .chars()
                .take_while(char::is_ascii_alphabetic)
                .collect::<String>()
                .to_ascii_uppercase();
            let model = self.model_of(comp);
            let sources = comp.pins.iter().filter(on_rail).any(|p| {
                model.map_or_else(
                    || matches!(p.kind.as_str(), "power_out" | "output"),
                    |m| {
                        m.pins.get(&p.number).is_some_and(|role| {
                            SOURCE_ROLES.contains(&role.to_ascii_lowercase().as_str())
                        })
                    },
                )
            });
            let rival = sources || (matches!(prefix.as_str(), "D" | "Q") && other_rail(comp));
            if rival {
                return Some(comp.reference.clone());
            }
        }
        None
    }

    /// The regulator that alone sets the rail `net` belongs to, with its
    /// rating, when exactly one linear regulator's output is on it and nothing
    /// else sources it.
    fn sole_regulator(&self, net: i64) -> Option<(Reg<'a>, f64)> {
        let group = self.group(net);
        let mut feeding = self
            .regulators()
            .into_iter()
            .filter(|r| group.contains(&r.output));
        let reg = feeding.next()?;
        if feeding.next().is_some() || self.rival_source(&group, &reg.comp.reference).is_some() {
            return None;
        }
        let rating = rated_current(reg.model)?;
        Some((reg, rating))
    }
}

/// The current the card rates the regulator's output for, when it is the part's
/// own rating.
fn rated_current(model: &ModelEntry) -> Option<f64> {
    if !model.ratings.basis.judges(Limit::Current) {
        return None;
    }
    model
        .ratings
        .max_current_a
        .filter(|a| a.is_finite() && *a > 0.0)
}

/// A part with no card and three or more pads that is not a connector, test
/// point or switch: a load whose current is unknown.
fn is_unmodelled_load(comp: &Component) -> bool {
    let r = comp.reference.to_ascii_uppercase();
    comp.pins.len() >= 3
        && !["J", "P", "X", "CN", "TP", "H", "SW", "MH", "FID"]
            .iter()
            .any(|p| r.starts_with(p))
}

/// Whether one of the part's pads on the rail is a supply pad by its card's
/// role (so the part draws from it, whether or not a current is stated).
fn is_supply_pad_on(comp: &Component, model: &ModelEntry, group: &BTreeSet<i64>) -> bool {
    comp.pins.iter().any(|p| {
        p.net.is_some_and(|n| group.contains(&n))
            && model.pins.get(&p.number).is_some_and(|role| {
                let role = role.to_ascii_lowercase();
                IC_SUPPLY_ROLES.contains(&role.as_str())
                    || MOTOR_SUPPLY_ROLES.contains(&role.as_str())
            })
    })
}

/// A message's verdict bands, with the cap an assumed input level imposes.
const fn capped(severity: Severity, assumed: bool) -> Severity {
    match (severity, assumed) {
        (Severity::High, true) => Severity::Medium,
        (Severity::Medium, true) => Severity::Low,
        (s, _) => s,
    }
}

fn describe(comp: &Component) -> String {
    format!("{} ({})", comp.reference, comp.value.trim())
}

/// The load-over-rating finding for `reg`, or `None`.
fn overload(budget: &Budget<'_>, reg: &Reg<'_>, rating_a: f64) -> Option<LintFinding> {
    let loads = budget.loads_on(reg.output, &reg.comp.reference, &mut BTreeSet::new());
    let total = loads.total_a();
    if total <= rating_a * (1.0 + SLACK) {
        return None;
    }
    let out = budget.net_name(reg.output);
    let mut refs = vec![reg.comp.reference.clone()];
    for (who, _) in &loads.items {
        let reference = who.split_whitespace().next().unwrap_or(who).to_string();
        if !refs.contains(&reference) {
            refs.push(reference);
        }
    }
    Some(LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity: Severity::High,
        message: format!(
            "{} is rated for {} of output (card '{}'), but the supply currents the cards state \
             for the parts on its output '{out}' add up to {} ({}).{} {REMEDY_MARKER}Fit a \
             regulator rated above the load, move part of the load to another regulator, or \
             feed the heavy part from the input rail.",
            describe(reg.comp),
            format_amps(rating_a),
            reg.model.id,
            format_amps(total),
            loads.listed(),
            loads.silent_note(),
        ),
        refs,
        nets: vec![out],
    })
}

/// The dropout-headroom finding for `reg`, or `None`.
fn headroom(budget: &Budget<'_>, reg: &Reg<'_>) -> Option<LintFinding> {
    let dropout = super::plausible_param(reg.model, "dropout_v").filter(|d| *d > 0.0)?;
    let input = reg.input?;
    let (input_range, assumed) = budget.resolver.declared_levels(input)?;
    // The regulator has to be what sets its own output: an output net another
    // source or a name defines says nothing about what it regulates to.
    let (out_range, _) = budget.resolver.declared_levels(reg.output)?;
    if !budget.resolver.defined_by_a_regulator(reg.output) {
        return None;
    }
    let target = f64::midpoint(out_range.min_v, out_range.max_v);
    // A negative or zero target is no positive linear regulator's output.
    if !target.is_finite() || target <= 0.0 || input_range.min_v <= 0.0 {
        return None;
    }
    let slack_v = 1e-3;
    let vin_min = input_range.min_v;
    let in_name = budget.net_name(input);
    let out_name = budget.net_name(reg.output);
    let how_low = if assumed {
        format!(
            "{} (assumed: only the band its name implies)",
            format_volts(vin_min)
        )
    } else {
        format_volts(vin_min)
    };
    let (severity, body) = if vin_min < target - slack_v {
        (
            capped(Severity::High, assumed),
            format!(
                "{} sets '{out_name}' to {} but its input '{in_name}' can be as low as {how_low}, \
                 {} below that output: the regulator cannot reach its set point at any load and \
                 the output follows the input down.",
                describe(reg.comp),
                format_volts(target),
                format_volts(target - vin_min),
            ),
        )
    } else if vin_min < target + dropout - slack_v {
        let rated = rated_current(reg.model);
        let load = budget
            .loads_on(reg.output, &reg.comp.reference, &mut BTreeSet::new())
            .total_a();
        // Dropout is a figure at the datasheet's rated load and falls with it:
        // a rail drawing under half the rating is a note, not a fault.
        let heavy = rated.is_some_and(|r| load >= 0.5 * r);
        // Light or unknown load on a name-implied input band: no claim.
        if !heavy && assumed {
            return None;
        }
        (
            capped(
                if heavy {
                    Severity::Medium
                } else {
                    Severity::Low
                },
                assumed,
            ),
            format!(
                "{} sets '{out_name}' to {} but its input '{in_name}' can be as low as {how_low}, \
                 leaving {} of headroom against the {} dropout card '{}' states at its rated \
                 load: at that load the output falls out of regulation{}.",
                describe(reg.comp),
                format_volts(target),
                format_volts(vin_min - target),
                format_volts(dropout),
                reg.model.id,
                if load > 0.0 {
                    format!(" (the cards state {} drawn from it)", format_amps(load))
                } else {
                    String::new()
                },
            ),
        )
    } else {
        return None;
    };
    Some(LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity,
        message: format!(
            "{body} {REMEDY_MARKER}Raise the input to at least {}, lower the output, or use \
             a lower-dropout regulator.",
            format_volts(target + dropout),
        ),
        refs: vec![reg.comp.reference.clone()],
        nets: vec![out_name, in_name],
    })
}

/// The findings for IC supply pins fed through a series resistor.
fn series_feed_findings(budget: &Budget<'_>) -> Vec<LintFinding> {
    let mut found: Vec<LintFinding> = Vec::new();
    let mut seen: BTreeSet<i64> = BTreeSet::new();
    for (comp, model) in &budget.parts {
        if model.kind == ComponentKind::Vreg || !model.ratings.basis.judges(Limit::Envelope) {
            continue;
        }
        let draws = supply_draws(comp, model);
        for envelope in binding(&model.envelope, None) {
            let OperatingEnvelope::SupplyRange { pin, min_v, .. } = envelope else {
                continue;
            };
            let Some(net) = role_net_id(comp, model, pin) else {
                continue;
            };
            if !draws.iter().any(|(n, _)| budget.group(net).contains(n)) || !seen.insert(net) {
                continue;
            }
            if let Some(f) = series_feed_finding(budget, net, (comp, *min_v)) {
                found.push(f);
            }
        }
    }
    found
}

/// The finding for the rail `net`, whose part `at.0` needs `at.1` volts.
fn series_feed_finding(
    budget: &Budget<'_>,
    net: i64,
    at: (&Component, f64),
) -> Option<LintFinding> {
    let group = budget.group(net);
    // A rail a regulator sets, or that anything else also sources, is not a
    // rail fed through its resistor alone.
    if group
        .iter()
        .any(|n| budget.resolver.defined_by_a_regulator(*n))
        || budget.rival_source(&group, "").is_some()
    {
        return None;
    }
    let feeds = series_feeds(&budget.resolver, net, RailRange::point(0.0));
    let (feed_net, feed_range) = feeds
        .iter()
        .map(|(_, _, far, range)| (*far, *range))
        .max_by(|a, b| a.1.max_v.total_cmp(&b.1.max_v))?;
    let loads = budget.loads_on(net, "", &mut BTreeSet::new());
    let amps = loads.total_a();
    if amps <= 0.0 {
        return None;
    }
    let ohms = 1.0 / feeds.iter().map(|(_, o, ..)| 1.0 / o).sum::<f64>();
    let drop_v = amps * ohms;
    let at_pin = feed_range.min_v - drop_v;
    let need = at.1;
    if at_pin >= need - 1e-3 {
        return None;
    }
    let (_, assumed) = budget.resolver.declared_levels(feed_net)?;
    let resistors: Vec<String> = feeds
        .iter()
        .map(|(r, o, ..)| format!("{r} ({} ohm)", super::device_decode::format_ohms(*o)))
        .collect();
    let net_name = budget.net_name(net);
    let feed_name = budget.net_name(feed_net);
    let max_ohms = (feed_range.min_v - need) / amps;
    let reach = if max_ohms > 0.0 {
        format!(
            "Feed the rail through no more than {} ohm in total, or directly from '{feed_name}'.",
            super::device_decode::format_ohms(max_ohms)
        )
    } else {
        format!("Feed the rail directly from '{feed_name}'; the source alone is under the minimum.")
    };
    let mut refs: Vec<String> = feeds.iter().map(|(r, ..)| r.clone()).collect();
    refs.push(at.0.reference.clone());
    let who = describe(at.0);
    Some(LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity: capped(Severity::High, assumed),
        message: format!(
            "{who} needs at least {} on '{net_name}', but the supply currents the cards state \
             on that rail ({}, {}) drop {} across {} from '{feed_name}' (at least {}): the pin \
             sits at {} or less.{} {REMEDY_MARKER}{reach}",
            format_volts(need),
            format_amps(amps),
            loads.listed(),
            format_volts(drop_v),
            resistors.join(" and "),
            format_volts(feed_range.min_v),
            format_volts(at_pin.max(0.0)),
            loads.silent_note(),
        ),
        refs,
        nets: vec![net_name, feed_name],
    })
}

/// A motor driver's programmed full-scale current, the rail it draws it from
/// and how the board programs it, for the message.
struct Programmed {
    amps: f64,
    supply: i64,
    /// What sets the figure: "its ILIM resistor, 30 kohm".
    how: String,
    /// What a fix turns: "raise the ILIM resistor".
    lever: String,
}

/// The programmed full-scale current of a motor driver and the rail it draws
/// it from, or `None` for a part that is not one, or whose resistor cannot be
/// read.
fn driver_full_scale(
    budget: &Budget<'_>,
    comp: &Component,
    model: &ModelEntry,
) -> Option<Programmed> {
    let board = budget.board;
    let program = model
        .current_program
        .as_ref()
        .filter(|p| p.semantics == CurrentProgramSemantics::ProtectionLimit)
        .filter(|p| {
            !matches!(
                p.equation,
                CurrentProgramEquation::SenseScaledResistance { .. }
            )
        })?;
    let supply = MOTOR_SUPPLY_ROLES
        .iter()
        .find_map(|role| role_net_id(comp, model, role))?;
    if matches!(
        program.equation,
        CurrentProgramEquation::ReferenceOverSense { .. }
    ) {
        // `max_by` keeps the last of equal windings; reverse so the first is named.
        let top = windings(budget, comp, model)
            .into_iter()
            .rev()
            .filter_map(|w| Some((w.trip()?, w)))
            .max_by(|a, b| a.0.hi_a.total_cmp(&b.0.hi_a))?;
        let (trip, w) = top;
        return (trip.hi_a > 0.0).then(|| Programmed {
            amps: trip.hi_a,
            supply,
            how: format!(
                "its {} reference at up to {} over the {} ohm sense resistor {}",
                w.reference_role.to_ascii_uppercase(),
                format_volts(trip.hi_v),
                super::device_decode::format_ohms(trip.sense_ohms),
                trip.sense_ref,
            ),
            lever: format!(
                "lower the {} reference voltage or raise the sense resistor {}",
                w.reference_role.to_ascii_uppercase(),
                trip.sense_ref,
            ),
        });
    }
    let index = board
        .components
        .iter()
        .position(|c| std::ptr::eq(c, comp))?;
    let net = role_net_id(comp, model, &program.pin)?;
    let ProgramReading::Ohms(ohms, _) =
        super::ampacity::program_reading(board, net, 4, Some(index))
    else {
        return None;
    };
    // A resistor under the datasheet's floor is the programming check's.
    if program.min_program_ohms.is_some_and(|min| ohms < min) {
        return None;
    }
    let amps = program.equation_current_a(ohms)?;
    (amps.is_finite() && amps > 0.0).then(|| Programmed {
        amps,
        supply,
        how: format!(
            "its {} resistor, {} ohm",
            program.pin,
            super::device_decode::format_ohms(ohms)
        ),
        lever: format!("raise the {} resistor", program.pin),
    })
}

/// The motor-driver finding for `comp`, or `None`.
fn motor_findings(budget: &Budget<'_>) -> Vec<LintFinding> {
    let mut found = Vec::new();
    for (comp, model) in &budget.parts {
        let Some(Programmed {
            amps: full_scale,
            supply,
            how,
            lever,
        }) = driver_full_scale(budget, comp, model)
        else {
            continue;
        };
        let Some((reg, rating)) = budget.sole_regulator(supply) else {
            continue;
        };
        let others = budget.loads_on(supply, &comp.reference, &mut BTreeSet::new());
        let total = full_scale + others.total_a();
        if total <= rating * (1.0 + SLACK) {
            continue;
        }
        let rail = budget.net_name(supply);
        let alone = full_scale > rating * (1.0 + SLACK);
        let extra = if alone || others.items.is_empty() {
            String::new()
        } else {
            format!(
                " With the {} the cards state for the other parts on '{rail}' ({}) it comes to \
                 {}.",
                format_amps(others.total_a()),
                others.listed(),
                format_amps(total),
            )
        };
        found.push(LintFinding {
            check: LintCheck::OperatingEnvelope,
            severity: if alone {
                Severity::Medium
            } else {
                Severity::Low
            },
            message: format!(
                "{} programs its current limit to {} ({how}), and \
                 draws it from '{rail}' whenever the motor loads or stalls the driver, but \
                 {} feeding '{rail}' is rated for {} (card '{}').{extra} The draw is \
                 conditional on the motor, so this is the driver's full-scale current against \
                 the supply, not a measured load. {REMEDY_MARKER}Feed the motor rail from a \
                 supply rated above the programmed current, or {lever} so the limit falls to \
                 what the supply gives.",
                describe(comp),
                format_amps(full_scale),
                describe(reg.comp),
                format_amps(rating),
                reg.model.id,
            ),
            refs: vec![reg.comp.reference.clone(), comp.reference.clone()],
            nets: vec![rail],
        });
    }
    found
}

/// What drives the reference pin of a reference-programmed driver.
enum Reference {
    /// Nothing: no net, or a net that holds only the pin and capacitors.
    Open,
    /// A level the board gives it.
    Level(Level),
    /// Something the check does not read (a trimmer, a part, a pin of the
    /// driver itself, a resistor to a net of unknown level).
    Unread,
}

/// The level a reference pin sits at.
struct Level {
    range: RailRange,
    /// True when the range is only the band a rail's NAME implies.
    assumed: bool,
    /// What sets it, for the message.
    how: String,
}

/// One winding of a reference-programmed driver, as the board wires it.
struct Winding {
    reference_role: String,
    reference: Reference,
    /// The sense resistor to ground, as `(ohms, designator)`; `None` when the
    /// path is not exactly one resistor.
    sense: Option<(f64, String)>,
    divisor: f64,
}

/// A winding's chopping current across the reference level it can see.
struct Trip {
    lo_a: f64,
    hi_a: f64,
    lo_v: f64,
    hi_v: f64,
    sense_ohms: f64,
    sense_ref: String,
    assumed: bool,
    how: String,
}

impl Winding {
    /// The current at the reference's lowest and highest level, when both the
    /// level and the sense resistor are read.
    fn trip(&self) -> Option<Trip> {
        let Reference::Level(level) = &self.reference else {
            return None;
        };
        let (sense_ohms, sense_ref) = self.sense.clone()?;
        let at = |v: f64| v.max(0.0) / (self.divisor * sense_ohms);
        Some(Trip {
            lo_a: at(level.range.min_v),
            hi_a: at(level.range.max_v),
            lo_v: level.range.min_v,
            hi_v: level.range.max_v,
            sense_ohms,
            sense_ref,
            assumed: level.assumed,
            how: level.how.clone(),
        })
    }
}

/// The reference-over-sense windings of `comp`, empty for a part whose card
/// does not program its current that way.
fn windings(budget: &Budget<'_>, comp: &Component, model: &ModelEntry) -> Vec<Winding> {
    let Some(CurrentProgramEquation::ReferenceOverSense {
        reference_roles,
        sense_roles,
        divisor,
    }) = model.current_program.as_ref().map(|p| &p.equation)
    else {
        return Vec::new();
    };
    let board = budget.board;
    let Some(index) = board.components.iter().position(|c| std::ptr::eq(c, comp)) else {
        return Vec::new();
    };
    reference_roles
        .iter()
        .zip(sense_roles)
        .map(|(reference_role, sense_role)| Winding {
            reference_role: reference_role.clone(),
            reference: reference_of(budget, comp, model, reference_role, reference_roles),
            sense: role_net_id(comp, model, sense_role).and_then(|net| {
                adjacent_sense_resistor(board, net, &ExpectedSenseFar::Ground, index)
            }),
            divisor: *divisor,
        })
        .collect()
}

/// What drives the `role` pin of `comp`: a rail or ground by name or by its
/// regulator, a divider of resistors off such nets, or nothing.
fn reference_of(
    budget: &Budget<'_>,
    comp: &Component,
    model: &ModelEntry,
    role: &str,
    reference_roles: &[String],
) -> Reference {
    let board = budget.board;
    let Some(net) = role_net_id(comp, model, role) else {
        return Reference::Open;
    };
    let group = budget.group(net);
    if let Some((range, assumed)) = group
        .iter()
        .find_map(|n| budget.resolver.declared_levels(*n))
    {
        return Reference::Level(Level {
            range,
            assumed,
            how: format!("'{}'", budget.net_name(net)),
        });
    }
    let is_reference_pad = |pad: &str| {
        model.pins.get(pad).is_some_and(|r| {
            reference_roles
                .iter()
                .any(|known| known.eq_ignore_ascii_case(r))
        })
    };
    // Another pad of the driver on the net (a regulator output that feeds the
    // reference) is a source this check does not know the level of.
    if comp
        .pins
        .iter()
        .any(|p| p.net.is_some_and(|n| group.contains(&n)) && !is_reference_pad(&p.number))
    {
        return Reference::Unread;
    }
    let others: Vec<_> = group
        .iter()
        .flat_map(|n| board.net_members(*n))
        .filter(|(c, _)| {
            c.reference != comp.reference
                && !matches!(AssemblyState::of(c), AssemblyState::DnpAbsent(_))
                && !is_capacitor(c)
                && !is_test_point_or_hole(c)
        })
        .collect();
    if others.is_empty() {
        return Reference::Open;
    }
    // A resistor divider: every other member is a resistor to a net of known
    // level, so the pin sits at the conductance-weighted mean of them.
    let (mut weight, mut lo, mut hi, mut assumed) = (0.0, 0.0, 0.0, false);
    let (mut refs, mut rails) = (Vec::new(), Vec::new());
    for (c, p) in &others {
        let Some(ohms) = super::bus_loading::resistor_ohms(c) else {
            return Reference::Unread;
        };
        let far = c
            .pins
            .iter()
            .find(|q| q.number != p.number)
            .and_then(|q| q.net)
            .filter(|n| !group.contains(n));
        let Some((range, was_assumed)) = far.and_then(|n| budget.resolver.declared_levels(n))
        else {
            return Reference::Unread;
        };
        let g = ohms.recip();
        weight += g;
        lo += g * range.min_v;
        hi += g * range.max_v;
        assumed |= was_assumed;
        refs.push(c.reference.clone());
        rails.push(format!("'{}'", budget.net_name(far.unwrap_or(net))));
    }
    Reference::Level(Level {
        range: RailRange {
            min_v: lo / weight,
            max_v: hi / weight,
        },
        assumed,
        how: format!("{} from {}", refs.join(" and "), rails.join(" and ")),
    })
}

/// The pad number a card gives `role`, for a message.
fn pad_of(model: &ModelEntry, role: &str) -> String {
    model
        .pins
        .iter()
        .find(|(_, r)| r.eq_ignore_ascii_case(role))
        .map_or_else(String::new, |(pad, _)| format!(" (pad {pad})"))
}

/// The findings on the reference pins and the full scale of the reference-
/// programmed drivers (A4988, DRV8825): a reference nothing drives, a
/// reference at 0 V, and a full scale over the part's output current rating.
fn reference_findings(budget: &Budget<'_>) -> Vec<LintFinding> {
    let mut found = Vec::new();
    for (comp, model) in &budget.parts {
        let windings = windings(budget, comp, model);
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for w in &windings {
            let role = w.reference_role.to_ascii_uppercase();
            if !seen.insert(role.clone()) {
                continue;
            }
            let pad = pad_of(model, &w.reference_role);
            match &w.reference {
                Reference::Open => found.push(LintFinding {
                    check: LintCheck::OperatingEnvelope,
                    severity: Severity::Medium,
                    message: format!(
                        "{} programs its winding current from the voltage on its {role} pin{pad}, \
                         but nothing drives that pin: it is a high-impedance analog input with no \
                         bias of its own, so the current limit is undefined (it can sit at zero \
                         and never move the motor, or float high and over-drive the windings). \
                         {REMEDY_MARKER}Set {role} with a resistor divider from the logic supply \
                         or a trimmer, to the voltage that gives the motor's rated current across \
                         the sense resistor.",
                        describe(comp),
                    ),
                    refs: vec![comp.reference.clone()],
                    nets: Vec::new(),
                }),
                Reference::Level(level) if level.range.max_v.abs() < 1e-3 => {
                    found.push(LintFinding {
                        check: LintCheck::OperatingEnvelope,
                        severity: capped(Severity::High, level.assumed),
                        message: format!(
                            "{} has its {role} pin{pad} at 0 V ({}): the chopping threshold is \
                             the reference voltage over the sense resistor, so with the \
                             reference at ground the driver regulates no winding current and \
                             the motor does not turn. {REMEDY_MARKER}Drive {role} from a \
                             divider or trimmer set to the voltage that gives the motor's rated \
                             current across the sense resistor.",
                            describe(comp),
                            level.how,
                        ),
                        refs: vec![comp.reference.clone()],
                        nets: Vec::new(),
                    });
                }
                _ => {}
            }
        }
        if let Some(f) = over_rating(budget, comp, model, &windings) {
            found.push(f);
        }
    }
    found
}

/// The finding that a reference-programmed driver's full scale is over the
/// output current its card rates it for, naming every winding that is.
fn over_rating(
    budget: &Budget<'_>,
    comp: &Component,
    model: &ModelEntry,
    windings: &[Winding],
) -> Option<LintFinding> {
    let rating = rated_current(model)?;
    let over: Vec<(&Winding, Trip)> = windings
        .iter()
        .filter_map(|w| Some((w, w.trip()?)))
        .filter(|(_, t)| t.hi_a > rating * (1.0 + SLACK))
        .collect();
    let (first, first_trip) = over.first()?;
    let assumed = over.iter().all(|(_, t)| t.assumed);
    let certain = over.iter().all(|(_, t)| t.lo_a > rating * (1.0 + SLACK));
    let divisor = first.divisor;
    let mut clauses = Vec::new();
    for (w, t) in &over {
        clauses.push(format!(
            "{} at {} ({}) over {} ({} ohm) is {}",
            w.reference_role.to_ascii_uppercase(),
            if (t.hi_v - t.lo_v).abs() < 1e-9 {
                format_volts(t.hi_v)
            } else {
                format!("{} to {}", format_volts(t.lo_v), format_volts(t.hi_v))
            },
            t.how,
            t.sense_ref,
            super::device_decode::format_ohms(t.sense_ohms),
            format_amps(t.hi_a),
        ));
    }
    let max_v = rating * divisor * first_trip.sense_ohms;
    let min_ohms = first_trip.hi_v / (divisor * rating);
    let nets = role_net_id(comp, model, first.reference_role.as_str())
        .map(|n| budget.net_name(n))
        .into_iter()
        .collect();
    let mut refs = vec![comp.reference.clone()];
    refs.extend(over.iter().map(|(_, t)| t.sense_ref.clone()));
    Some(LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity: capped(
            if certain {
                Severity::High
            } else {
                Severity::Medium
            },
            assumed,
        ),
        message: format!(
            "{} sets its full-scale winding current by I = VREF / ({} x RS), and here {}: over \
             the {} output current its card rates the part for (card '{}'). The driver chops \
             at that current whenever the microstep table reaches 100 %, so the motor winding \
             and the driver are driven past the rating. {REMEDY_MARKER}Hold {} to at most {} \
             for a {} ohm sense resistor, or raise {} to at least {} ohm at this reference.",
            describe(comp),
            divisor,
            clauses.join("; "),
            format_amps(rating),
            model.id,
            first.reference_role.to_ascii_uppercase(),
            format_volts(max_v),
            super::device_decode::format_ohms(first_trip.sense_ohms),
            first_trip.sense_ref,
            super::device_decode::format_ohms(min_ohms),
        ),
        refs,
        nets,
    })
}

/// Whether an `existing` finding already says what `f` is about, so the board
/// carries one finding for it: a stress finding on the regulator for a load
/// over the rating, the operating-envelope check's starved rail for a feed.
fn already_reported(existing: &[&LintFinding], f: &LintFinding, kind: Kind) -> bool {
    existing.iter().any(|e| match kind {
        Kind::Rating => {
            e.check == LintCheck::DcOverstress && f.refs.first().is_some_and(|r| e.refs.contains(r))
        }
        Kind::Feed => {
            e.check == LintCheck::OperatingEnvelope
                && e.message.contains("starved")
                && e.nets.iter().any(|n| f.nets.contains(n))
        }
        Kind::Headroom => false,
    })
}

#[derive(Clone, Copy)]
enum Kind {
    Rating,
    Headroom,
    Feed,
}

/// The budget, headroom, feed and motor-driver findings of `board`, less what
/// `existing` findings already report.
pub(crate) fn regulator_budget_findings(
    board: &ExtractedBoard,
    st: &BoardStatics<'_>,
    existing: &[&LintFinding],
) -> NetLintReport {
    let solved = BTreeMap::new();
    let budget = Budget {
        board,
        parts: st.parts(board).collect(),
        tied: tied_nets(board),
        resolver: RailResolver::new(board, st, &solved),
    };
    let mut report = NetLintReport::default();
    let mut push = |f: LintFinding, kind: Kind| {
        if !already_reported(existing, &f, kind) {
            report.findings.push(f);
        }
    };
    for reg in budget.regulators() {
        let group = budget.group(reg.output);
        if budget.rival_source(&group, &reg.comp.reference).is_none() {
            if let Some(rating) = rated_current(reg.model) {
                if let Some(f) = overload(&budget, &reg, rating) {
                    push(f, Kind::Rating);
                }
            }
        }
        if let Some(f) = headroom(&budget, &reg) {
            push(f, Kind::Headroom);
        }
    }
    for f in series_feed_findings(&budget) {
        push(f, Kind::Feed);
    }
    for f in motor_findings(&budget) {
        push(f, Kind::Rating);
    }
    for f in reference_findings(&budget) {
        push(f, Kind::Headroom);
    }
    report
}

/// The regulator-budget lint, with no other finding to defer to.
#[must_use]
pub fn regulator_budget_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    regulator_budget_findings(board, &BoardStatics::of(board, lib), &[])
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;

    /// Linear regulators and loads for the fixtures. The numbers are the
    /// fixture's own; the real cards' figures are sourced in `db/`.
    const CARDS: &str = r#"
[[models]]
id = "bud_ldo"
kind = "vreg"
description = "budget fixture: 3.3 V LDO rated 100 mA, 1.0 V dropout"
[models.match]
value_re = "^BUD_LDO$"
[models.params]
vout = 3.3
dropout_v = 1.0
iq_a = 0.0001
[models.pins]
"1" = "gnd"
"2" = "out"
"3" = "in"
[models.ratings]
max_current_a = 0.1

[[models]]
id = "bud_ldo_4v"
kind = "vreg"
description = "budget fixture: 4.0 V LDO rated 1 A"
[models.match]
value_re = "^BUD_LDO_4V$"
[models.params]
vout = 4.0
dropout_v = 0.5
iq_a = 0.000001
[models.pins]
"1" = "gnd"
"2" = "out"
"3" = "in"
[models.ratings]
max_current_a = 1.0

[[models]]
id = "bud_ldo_3v"
kind = "vreg"
description = "budget fixture: 3.0 V LDO rated 1 A"
[models.match]
value_re = "^BUD_LDO_3V$"
[models.params]
vout = 3.0
dropout_v = 0.5
iq_a = 0.000001
[models.pins]
"1" = "gnd"
"2" = "out"
"3" = "in"
[models.ratings]
max_current_a = 1.0

[[models]]
id = "bud_ldo_9v"
kind = "vreg"
description = "budget fixture: 9 V LDO rated 1 A"
[models.match]
value_re = "^BUD_LDO_9V$"
[models.params]
vout = 9.0
dropout_v = 0.5
iq_a = 0.000001
[models.pins]
"1" = "gnd"
"2" = "out"
"3" = "in"
[models.ratings]
max_current_a = 1.0

[[models]]
id = "bud_load"
kind = "digital"
description = "budget fixture: 60 mA supply, 2.7 V minimum"
[models.match]
value_re = "^BUD_LOAD$"
[models.params]
supply_static_ua = 60000.0
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vdd"
"2" = "gnd"
[[models.envelope]]
kind = "supply_range"
pin = "vdd"
min_v = 2.7
max_v = 5.5
basis = "fixture"

[[models]]
id = "bud_silent"
kind = "digital"
description = "budget fixture: a part whose card states no supply current"
[models.match]
value_re = "^BUD_SILENT$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vdd"
"2" = "gnd"
"#;

    fn library(tag: &str) -> ModelLibrary {
        let dir = hauksbee_testkit::fresh_scratch_dir("regulator_budget", tag);
        std::fs::write(dir.join("models.toml"), CARDS).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "fixture cards must load: {errors:?}");
        lib
    }

    fn lint(tag: &str, b: &ExtractedBoard) -> Vec<LintFinding> {
        regulator_budget_lint(b, &library(tag)).findings
    }

    fn says(f: &LintFinding, all: &[&str]) {
        for s in all {
            assert!(f.message.contains(s), "{s:?} in {}", f.message);
        }
    }

    /// `U1` (`BUD_LDO`, rated 100 mA) from +5V to +3V3, with `loads` (card
    /// values) powered from +3V3.
    fn ldo_with(loads: &[&str]) -> ExtractedBoard {
        let mut parts = vec![part("U1", "BUD_LDO", &[("1", 1), ("2", 3), ("3", 2)])];
        for (i, value) in loads.iter().enumerate() {
            parts.push(part(&format!("Q{}", i + 1), value, &[("1", 3), ("2", 1)]));
        }
        board(&[(1, "GND"), (2, "+5V"), (3, "+3V3")], parts)
    }

    /// Three 60 mA parts on a 100 mA regulator: the stated currents alone are
    /// over the rating, so it is a finding naming each load and the rating.
    #[test]
    fn stated_loads_over_the_ldos_rating_are_flagged() {
        let found = lint("over", &ldo_with(&["BUD_LOAD", "BUD_LOAD", "BUD_LOAD"]));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::High);
        assert_eq!(found[0].refs, ["U1", "Q1", "Q2", "Q3"]);
        assert_eq!(found[0].nets, ["+3V3"]);
        says(
            &found[0],
            &[
                "U1 (BUD_LDO) is rated for 100 mA of output (card 'bud_ldo')",
                "add up to 180 mA",
                "Q1 60 mA, Q2 60 mA, Q3 60 mA",
            ],
        );
        assert!(found[0].own_remedy().is_some());
    }

    /// Silent controls: a load within the rating, a rail whose only extra part
    /// states no current (not knowing a load is no finding), and a rail with a
    /// second source on it.
    #[test]
    fn loads_within_the_rating_or_unstated_are_silent() {
        assert!(lint("under", &ldo_with(&["BUD_LOAD"])).is_empty());
        assert!(lint("none", &ldo_with(&[])).is_empty());
        assert!(lint("unstated", &ldo_with(&["BUD_LOAD", "BUD_SILENT"])).is_empty());
        // A second regulator on the same output shares the load: abstain.
        let mut shared = ldo_with(&["BUD_LOAD", "BUD_LOAD", "BUD_LOAD"]);
        shared
            .components
            .push(part("U2", "BUD_LDO", &[("1", 1), ("2", 3), ("3", 2)]));
        assert!(lint("shared", &shared).is_empty());
    }

    /// The full lint reports the over-rated regulator once, whichever of the
    /// stress and budget checks sees it, and reports the series-fed rail.
    #[test]
    fn the_engine_lint_carries_the_budget_findings_once() {
        let lib = library("engine");
        let over = ldo_with(&["BUD_LOAD", "BUD_LOAD", "BUD_LOAD"]);
        let found = super::super::engine_lint(&over, &lib).findings;
        let rated = found
            .iter()
            .filter(|f| f.refs.first().is_some_and(|r| r == "U1"))
            .filter(|f| f.message.contains("rated for 100 mA"))
            .count();
        assert_eq!(rated, 1, "{found:?}");
        let fed = super::super::engine_lint(&series_fed("47"), &lib).findings;
        let drop: Vec<_> = fed
            .iter()
            .filter(|f| f.nets.contains(&"VX".to_string()))
            .collect();
        assert!(
            drop.iter()
                .any(|f| f.message.contains("needs at least 2.7 V")),
            "{fed:?}"
        );
    }

    /// A part with no stated current on an over-rated rail is named as making
    /// the real draw higher, never counted as zero.
    #[test]
    fn a_part_with_no_stated_current_is_named_not_counted() {
        let found = lint("named", &ldo_with(&["BUD_LOAD", "BUD_LOAD", "BUD_SILENT"]));
        assert_eq!(found.len(), 1, "{found:?}");
        says(
            &found[0],
            &["add up to 120 mA", "Q3 states no supply current"],
        );
    }

    /// A linear regulator downstream adds its own quiescent current and what
    /// it feeds to the upstream one.
    #[test]
    fn a_downstream_regulators_load_passes_through_to_the_upstream_one() {
        let b = board(
            &[(1, "GND"), (2, "+5V"), (3, "+3V3"), (4, "+3V0")],
            vec![
                part("U1", "BUD_LDO", &[("1", 1), ("2", 3), ("3", 2)]),
                part("U2", "BUD_LDO_3V", &[("1", 1), ("2", 4), ("3", 3)]),
                part("Q1", "BUD_LOAD", &[("1", 4), ("2", 1)]),
                part("Q2", "BUD_LOAD", &[("1", 4), ("2", 1)]),
            ],
        );
        let found = lint("chain", &b);
        let u1: Vec<_> = found.iter().filter(|f| f.refs[0] == "U1").collect();
        assert_eq!(u1.len(), 1, "{found:?}");
        says(
            u1[0],
            &["Q1 via U2 60 mA", "Q2 via U2 60 mA", "add up to 120"],
        );
    }

    /// A regulator whose input, at the lowest the board declares, is under its
    /// output plus dropout drops out; one that is under its output cannot
    /// regulate at all; one with the headroom is silent.
    #[test]
    fn dropout_headroom_is_judged_on_the_declared_input_minimum() {
        // `upstream` feeds the 3.3 V LDO (1.0 V dropout, 100 mA).
        let chain = |upstream: &str, loads: &[&str]| {
            let mut parts = vec![
                part("U0", upstream, &[("1", 1), ("2", 3), ("3", 2)]),
                part("U1", "BUD_LDO", &[("1", 1), ("2", 4), ("3", 3)]),
            ];
            for (i, value) in loads.iter().enumerate() {
                parts.push(part(&format!("Q{}", i + 1), value, &[("1", 4), ("2", 1)]));
            }
            board(
                &[(1, "GND"), (2, "VBUS_IN"), (3, "RAIL_A"), (4, "RAIL_B")],
                parts,
            )
        };
        let of_u1 = |f: Vec<LintFinding>| -> Vec<LintFinding> {
            f.into_iter().filter(|f| f.refs == ["U1"]).collect()
        };
        // 0.7 V of headroom against a 1.0 V dropout, 60 of 100 mA drawn.
        let near = of_u1(lint("near", &chain("BUD_LDO_4V", &["BUD_LOAD"])));
        assert_eq!(near.len(), 1, "{near:?}");
        assert_eq!(near[0].severity, Severity::Medium);
        says(
            &near[0],
            &[
                "U1 (BUD_LDO) sets 'RAIL_B' to 3.3 V",
                "'RAIL_A' can be as low as 4 V",
                "700 mV of headroom against the 1 V dropout card 'bud_ldo'",
            ],
        );
        // The same headroom with the output idle is a note.
        let idle = of_u1(lint("idle", &chain("BUD_LDO_4V", &[])));
        assert_eq!(idle.len(), 1, "{idle:?}");
        assert_eq!(idle[0].severity, Severity::Low);
        // An input under the output itself cannot regulate at any load.
        let below = of_u1(lint("below", &chain("BUD_LDO_3V", &["BUD_LOAD"])));
        assert_eq!(below.len(), 1, "{below:?}");
        assert_eq!(below[0].severity, Severity::High);
        says(&below[0], &["cannot reach its set point at any load"]);
        // A 9 V input has the headroom.
        let fine = of_u1(lint("fine", &chain("BUD_LDO_9V", &["BUD_LOAD"])));
        assert!(fine.is_empty(), "{fine:?}");
    }

    /// An input rail known only by its name is a band, not a level: the
    /// finding is a band lower than the same fault on a regulated input.
    #[test]
    fn an_input_known_only_by_name_caps_the_finding() {
        let b = board(
            &[(1, "GND"), (2, "+3V0"), (3, "+3V3")],
            vec![part("U1", "BUD_LDO", &[("1", 1), ("2", 3), ("3", 2)])],
        );
        let found = lint("named_in", &b);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            matches!(found[0].severity, Severity::Low | Severity::Medium),
            "{found:?}"
        );
        says(&found[0], &["assumed"]);
    }

    /// Silent controls for the headroom rule: a dropout band on a name-implied
    /// input with the output idle is no claim (dropout falls with load); an
    /// output net named for a voltage the card does not set (pad roles from
    /// another package's pinout) is not read; a PMIC is not an LDO.
    #[test]
    fn headroom_abstains_where_it_cannot_tell() {
        let named = |input: &str, loads: &[&str]| {
            let mut parts = vec![part("U1", "BUD_LDO", &[("1", 1), ("2", 3), ("3", 2)])];
            for (i, value) in loads.iter().enumerate() {
                parts.push(part(&format!("Q{}", i + 1), value, &[("1", 3), ("2", 1)]));
            }
            board(&[(1, "GND"), (2, input), (3, "+3V3")], parts)
        };
        // 3.8 V is inside the 1 V dropout band of a 3.3 V output.
        assert!(lint("band_idle", &named("+4V0", &[])).is_empty());
        let loaded = lint("band_loaded", &named("+4V0", &["BUD_LOAD"]));
        assert_eq!(loaded.len(), 1, "{loaded:?}");
        assert_eq!(loaded[0].severity, Severity::Low);
        assert!(lint("band_ok", &named("+5V", &["BUD_LOAD"])).is_empty());
        // The same regulator with its pads the other way round on a board
        // that names its rails: the output net says 5 V, the card says 3.3 V.
        let swapped = board(
            &[(1, "GND"), (2, "+3V3"), (3, "+5V")],
            vec![part("U1", "BUD_LDO", &[("1", 1), ("2", 3), ("3", 2)])],
        );
        assert!(lint("swapped", &swapped).is_empty());
    }

    /// `BUD_LOAD` (60 mA, 2.7 V minimum) on `VX`, fed from +5V through `ohms`.
    fn series_fed(ohms: &str) -> ExtractedBoard {
        board(
            &[(1, "GND"), (2, "+5V"), (3, "VX")],
            vec![
                part("R1", ohms, &[("1", 2), ("2", 3)]),
                part("Q1", "BUD_LOAD", &[("1", 3), ("2", 1)]),
            ],
        )
    }

    /// 60 mA through 47 ohm drops 2.82 V of the 4.75 V rail, leaving 1.93 V
    /// at a pin that needs 2.7 V; 10 ohm drops 0.6 V and is silent.
    #[test]
    fn a_supply_pin_fed_through_too_large_a_resistor_is_flagged() {
        let found = lint("series", &series_fed("47"));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].refs, ["R1", "Q1"]);
        assert_eq!(found[0].nets, ["VX", "+5V"]);
        says(
            &found[0],
            &[
                "Q1 (BUD_LOAD) needs at least 2.7 V on 'VX'",
                "60 mA",
                "across R1 (47 ohm) from '+5V'",
                "no more than 34",
            ],
        );
        assert!(lint("series_ok", &series_fed("10")).is_empty());
        // A part with no stated draw drops nothing: no claim.
        let mut silent = series_fed("47");
        silent.components[1].value = "BUD_SILENT".into();
        assert!(lint("series_silent", &silent).is_empty());
    }

    /// DRV8871: `ITRIP = 64000 / RILIM`. 30 kohm programs 2.13 A from a motor
    /// rail whose regulator is rated 1 A; 100 kohm programs 0.64 A and is
    /// silent. Sums with other stated loads are a note.
    #[test]
    fn a_motor_drivers_programmed_current_is_held_to_its_supply_rating() {
        let motor = |ilim: &str, loads: usize| {
            let mut parts = vec![
                part("U0", "BUD_LDO_9V", &[("1", 1), ("2", 3), ("3", 2)]),
                part("U1", "DRV8871", &[("1", 1), ("4", 4), ("5", 3), ("9", 1)]),
                part("R9", ilim, &[("1", 4), ("2", 1)]),
            ];
            for i in 0..loads {
                parts.push(part(
                    &format!("Q{}", i + 1),
                    "BUD_LOAD",
                    &[("1", 3), ("2", 1)],
                ));
            }
            board(&[(1, "GND"), (2, "+12V"), (3, "VMOT"), (4, "ILIM")], parts)
        };
        let of_motor = |f: Vec<LintFinding>| -> Vec<LintFinding> {
            f.into_iter().filter(|f| f.refs == ["U0", "U1"]).collect()
        };
        let found = of_motor(lint("motor", &motor("30k", 0)));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::Medium);
        says(
            &found[0],
            &[
                "programs its current limit to 2.133 A",
                "from 'VMOT'",
                "rated for 1 A (card 'bud_ldo_9v')",
                "conditional on the motor",
            ],
        );
        assert!(of_motor(lint("motor_ok", &motor("100k", 0))).is_empty());
        // 68 kohm is 0.94 A alone, 1.06 A with two 60 mA parts on the rail.
        let sum = of_motor(lint("motor_sum", &motor("68k", 2)));
        assert_eq!(sum.len(), 1, "{sum:?}");
        assert_eq!(sum[0].severity, Severity::Low);
        assert!(of_motor(lint("motor_alone", &motor("68k", 0))).is_empty());
    }

    // ── Reference-programmed drivers (A4988: I = VREF / (8 x RS); DRV8825:
    // I = V(xREF) / (5 x RISENSE)). The cards are the shipped ones.

    /// The findings that are about the reference or the full scale (not the
    /// supply budget) of the driver `U1`.
    fn of_driver(found: Vec<LintFinding>) -> Vec<LintFinding> {
        found
            .into_iter()
            .filter(|f| f.refs.first().is_some_and(|r| r == "U1"))
            .collect()
    }

    /// An A4988 `U1` with `sense` ohms on both SENSE pins, and whatever
    /// `reference` parts the caller puts on REF (net 4). Nets: 1 GND, 2 +5V,
    /// 3 VMOT, 4 REF, 5 SENSE1, 6 SENSE2.
    fn a4988(sense: &str, reference: Vec<Component>) -> ExtractedBoard {
        let mut parts = vec![
            part(
                "U1",
                "A4988",
                &[
                    ("3", 1),
                    ("18", 1),
                    ("17", 4),
                    ("22", 3),
                    ("28", 3),
                    ("23", 5),
                    ("27", 6),
                ],
            ),
            part("R5", sense, &[("1", 5), ("2", 1)]),
            part("R6", sense, &[("1", 6), ("2", 1)]),
        ];
        parts.extend(reference);
        board(
            &[
                (1, "GND"),
                (2, "+5V"),
                (3, "VMOT"),
                (4, "REF"),
                (5, "SENSE1"),
                (6, "SENSE2"),
            ],
            parts,
        )
    }

    /// REF divided from +5V by `top` over `bottom`.
    fn divider(top: &str, bottom: &str) -> Vec<Component> {
        vec![
            part("R1", top, &[("1", 2), ("2", 4)]),
            part("R2", bottom, &[("1", 4), ("2", 1)]),
        ]
    }

    /// 5 V over 10 k / 10 k is 2.5 V (2.375 to 2.625 V on the rail's band);
    /// across 0.1 ohm that is 2.5 / (8 x 0.1) = 3.1 A, over the A4988's 2 A.
    /// The same divider with a 0.5 ohm resistor is 0.63 A and silent.
    #[test]
    fn a_full_scale_over_the_drivers_output_rating_is_flagged() {
        let found = of_driver(lint("a4988_over", &a4988("0.1", divider("10k", "10k"))));
        assert_eq!(found.len(), 1, "{found:?}");
        // The level is only the +5V name's band, so the verdict is capped.
        assert_eq!(found[0].severity, Severity::Medium);
        assert_eq!(found[0].refs, ["U1", "R5", "R6"]);
        says(
            &found[0],
            &[
                "U1 (A4988) sets its full-scale winding current by I = VREF / (8 x RS)",
                "REF at 2.375 V to 2.625 V (R1 and R2 from '+5V' and 'GND')",
                "over R5 (0.100 ohm) is 3.281 A",
                "over the 2 A output current its card rates the part for",
                "at most 1.6 V for a 0.100 ohm sense resistor",
            ],
        );
        assert!(
            of_driver(lint("a4988_fine", &a4988("0.5", divider("10k", "10k")))).is_empty(),
            "0.63 A is inside the rating"
        );
        // A real design: 0.1 ohm with about 0.9 V is 1.1 A.
        assert!(of_driver(lint("a4988_real", &a4988("0.1", divider("47k", "10k")))).is_empty());
    }

    /// A reference tied straight to a named rail is the same arithmetic: +3V3
    /// across 0.1 ohm is 4.1 A to 4.3 A, over the rating at every corner of
    /// the band, so it is High once the rail is known and Medium when only
    /// the name says what it is.
    #[test]
    fn a_reference_on_a_rail_is_read_from_the_rail() {
        let mut b = a4988("0.1", vec![]);
        b.nets[3].name = "+3V3".into();
        let found = of_driver(lint("a4988_rail", &b));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::Medium);
        says(&found[0], &["REF at 3.135 V to 3.465 V ('+3V3')"]);
    }

    /// A reference pin with nothing on it, or only a capacitor, leaves the trip
    /// level undefined. The same board with a divider on REF is silent.
    #[test]
    fn a_reference_nothing_drives_is_flagged() {
        let none = of_driver(lint("a4988_open", &a4988("0.1", vec![])));
        assert_eq!(none.len(), 1, "{none:?}");
        assert_eq!(none[0].severity, Severity::Medium);
        says(
            &none[0],
            &[
                "from the voltage on its REF pin (pad 17)",
                "nothing drives that pin",
                "current limit is undefined",
            ],
        );
        let cap = vec![part("C9", "100n", &[("1", 4), ("2", 1)])];
        let capped = of_driver(lint("a4988_cap", &a4988("0.1", cap)));
        assert_eq!(capped.len(), 1, "{capped:?}");
        says(&capped[0], &["nothing drives that pin"]);
        // No net on the pad at all is the same fault.
        let mut unwired = a4988("0.1", vec![]);
        unwired.components[0].pins.retain(|p| p.number != "17");
        assert_eq!(of_driver(lint("a4988_unwired", &unwired)).len(), 1);
        assert!(of_driver(lint("a4988_driven", &a4988("0.1", divider("47k", "10k")))).is_empty());
    }

    /// A reference at ground gives a zero trip level: no winding current.
    #[test]
    fn a_reference_at_ground_is_flagged() {
        let low = vec![part("R2", "10k", &[("1", 4), ("2", 1)])];
        let found = of_driver(lint("a4988_gnd", &a4988("0.1", low)));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::High);
        says(
            &found[0],
            &["REF pin (pad 17) at 0 V", "regulates no winding current"],
        );
    }

    /// What the check cannot read it does not judge: a trimmer on REF, a
    /// sense path of two parallel resistors.
    #[test]
    fn an_unreadable_reference_or_sense_path_is_not_judged() {
        let trimmer = vec![
            part("RV1", "10k", &[("1", 2), ("2", 4), ("3", 1)]),
            part("R1", "1k", &[("1", 4), ("2", 1)]),
        ];
        assert!(of_driver(lint("a4988_trim", &a4988("0.1", trimmer))).is_empty());
        let mut parallel = a4988("0.1", divider("10k", "10k"));
        parallel
            .components
            .push(part("R7", "0.1", &[("1", 5), ("2", 1)]));
        parallel
            .components
            .push(part("R8", "0.1", &[("1", 6), ("2", 1)]));
        assert!(of_driver(lint("a4988_parallel", &parallel)).is_empty());
    }

    /// DRV8825 bridges are separate pads: AVREF/ISENA and BVREF/ISENB. Nets:
    /// 1 GND, 2 +3V3, 3 VM, 4 AVREF, 5 BVREF, 6 ISENA, 7 ISENB.
    fn drv8825(sense_a: &str, sense_b: &str, vref_a: &str, vref_b: &str) -> ExtractedBoard {
        let mut b = board(
            &[
                (1, "GND"),
                (2, "+3V3"),
                (3, "VM"),
                (4, "AVREF"),
                (5, "BVREF"),
                (6, "ISENA"),
                (7, "ISENB"),
            ],
            vec![
                part(
                    "U1",
                    "DRV8825",
                    &[
                        ("14", 1),
                        ("28", 1),
                        ("12", 4),
                        ("13", 5),
                        ("4", 3),
                        ("11", 3),
                        ("6", 6),
                        ("9", 7),
                    ],
                ),
                part("R5", sense_a, &[("1", 6), ("2", 1)]),
                part("R6", sense_b, &[("1", 7), ("2", 1)]),
            ],
        );
        for (i, (net, top, bottom)) in [(4, vref_a, "10k"), (5, vref_b, "10k")]
            .into_iter()
            .enumerate()
        {
            b.components
                .push(part(&format!("R{}", 10 + i), top, &[("1", 2), ("2", net)]));
            b.components.push(part(
                &format!("R{}", 20 + i),
                bottom,
                &[("1", net), ("2", 1)],
            ));
        }
        b
    }

    /// 3.3 V halved is 1.65 V: across 0.1 ohm the DRV8825 gives 1.65 / 0.5 =
    /// 3.3 A on bridge A, over its 2.5 A; bridge B with 0.33 ohm is 1 A. Both
    /// windings use their own pads, and only the one over is named.
    #[test]
    fn each_winding_of_a_drv8825_is_read_from_its_own_pads() {
        let found = of_driver(lint("drv_over", &drv8825("0.1", "0.33", "10k", "10k")));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].refs, ["U1", "R5"]);
        says(
            &found[0],
            &[
                "U1 (DRV8825) sets its full-scale winding current by I = VREF / (5 x RS)",
                "AVREF at 1.567 V to 1.732 V",
                "over R5 (0.100 ohm)",
                "over the 2.5 A output current",
            ],
        );
        assert!(
            !found[0].message.contains("BVREF at"),
            "{}",
            found[0].message
        );
        // 0.33 ohm on both is 1.0 A on both: silent.
        assert!(of_driver(lint("drv_ok", &drv8825("0.33", "0.33", "10k", "10k"))).is_empty());
    }

    /// The full scale is also held to the regulator that feeds the motor rail.
    /// 1.2 V (a 33 k / 10 k divider off +5V) across 0.1 ohm is 1.5 A from a
    /// 1 A regulator; across 0.5 ohm it is 0.3 A and silent.
    #[test]
    fn a_reference_programmed_full_scale_is_held_to_the_motor_supply() {
        let fed = |sense: &str| {
            let mut b = a4988(sense, divider("33k", "10k"));
            b.components
                .push(part("U0", "BUD_LDO_9V", &[("1", 1), ("2", 3), ("3", 2)]));
            b
        };
        let both = lint("a4988_supply", &fed("0.1"));
        let found: Vec<_> = both.iter().filter(|f| f.refs == ["U0", "U1"]).collect();
        assert_eq!(found.len(), 1, "{both:?}");
        says(
            found[0],
            &[
                "programs its current limit to 1.5",
                "its REF reference at up to 1.2",
                "over the 0.100 ohm sense resistor R5",
                "rated for 1 A (card 'bud_ldo_9v')",
                "lower the REF reference voltage or raise the sense resistor R5",
            ],
        );
        assert!(lint("a4988_supply_ok", &fed("0.5"))
            .iter()
            .all(|f| f.refs != ["U0", "U1"]));
    }
}
