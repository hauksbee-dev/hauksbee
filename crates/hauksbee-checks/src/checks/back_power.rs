//! Back-powering / cross-domain pull lint: a modelled part's signal pin tied
//! or pulled up to a rail ABOVE the part's own supply domain.
//!
//! The physics: virtually every IC input carries a protection clamp diode from
//! the pin to its supply rail. Pull a pin to a rail higher than the part's
//! VDD + one diode drop and the clamp conducts: current flows from the higher
//! rail *through the part* into the lower rail. Two field failures follow:
//!
//! 1. **Back-powering.** With the part's own rail unpowered (main supply off,
//!    a USB-UART adapter still plugged in), the higher rail feeds the "off"
//!    rail through the clamp, holding the whole domain half-alive: parts brown
//!    out instead of resetting, flash writes corrupt, and the current path was
//!    never designed to carry the load.
//! 2. **Abs-max violation.** Powered up, the pin sits past the datasheet's
//!    VCC + 0.3..0.5 V input abs-max. Some pins are explicitly tolerant (a
//!    5 V-tolerant STM32 FT pin); everything else is slow damage or latch-up.
//!
//! Cross-layer, per the C6.1 audit: the fault spans the POWER layer (which
//! rail feeds which part) and the SIGNAL layer (which net pulls where); each
//! layer alone looks clean. A 5 V-pulled I2C bus wired to a 3.3 V-supplied
//! sensor routes fine, ERC passes, and every part is individually healthy.
//!
//! ## Evidence the check rests on
//!
//! Only decision-grade inputs fire a finding:
//! - The part's supply domain comes from its OWN model pin map: a supply-role
//!   pad (`vcc`/`vdd`/`avcc`/`vdda`/`vddio`/`vio`/`5v`) whose net resolves
//!   through [`power_rail_voltage`], the binder's one rail-name table, so this
//!   check and the co-sim's supply stamping cannot disagree about a rail. A
//!   part with several resolvable supply pads uses the HIGHEST (a dual-domain
//!   part is judged against its most permissive rail, failing toward silence).
//! - The offending pull is a plain assembled resistor from the pin's net to a
//!   resolvable rail, or the pin's net IS a resolvable rail (a direct tie).
//!
//! A part whose supply nets the rail table cannot resolve, a pull to an
//! unresolvable net, or a missing model all stay silent: no guessed voltage
//! may support a finding.
//!
//! ## Severity calibration
//!
//! - Direct tie of a signal pin to a higher rail: **High**. There is no series
//!   element; the clamp carries whatever the rail sources.
//! - Pull-up resistor to a higher rail, pin rating unknown: **Medium**. The
//!   resistor bounds the clamp current, and higher-voltage-tolerant pins make
//!   this idiom legitimate on SOME parts. The finding says what the clamp path
//!   is and names the exception the reader must verify.
//! - Pull-up resistor to a rail above a rating the card DECLARES for that pin:
//!   **High**. Where the card rates a signal pin with its own `supply_range`
//!   envelope, or lists it in an `input_tolerance` envelope (a 74LVC input's
//!   `VI` abs max of 6.5 V at any VCC), that supply-independent rating
//!   replaces the clamp assumption
//!   ([`hauksbee_models::declared_pin_ceiling_v`]): a rail at or under it is
//!   silent (a 5 V-tolerant input takes its 5 V pull-up), and a rail above it
//!   is a stated absolute maximum exceeded, whatever the clamp does, so there
//!   is no exception left for the reader to verify. A declared rating below
//!   the clamp's own reach counts the same way: a sense pin rated 1.4 V on a
//!   3.3 V part, fed from 5 V (or from a divider that still tops 1.4 V) or
//!   driven by another part's push-pull output that swings past it (an
//!   oscillator's 3.3 V output into a 2.45 V clock input), is past its stated
//!   maximum with no back-power path involved, and the finding says so.
//! - A VBUS-sense pin (`usb_vbus`, `vbus_sense`) the card does not rate:
//!   **Low**. The pin exists to see a bus above the part's supply, so the pull
//!   is the design, not a fault; the note asks to confirm the pin's maximum in
//!   the datasheet's pin table or to add the `input_tolerance` row, and the
//!   card's row then decides (silent under it, **High** over it).
//!
//! Pins the check deliberately skips: supply and ground roles (they belong on
//! rails), and roles that are higher-voltage inputs by design (`vin`, `vbus`,
//! `vbat`, `vsys`, `raw`, `vmot` feed regulators/monitors rated for it).

use std::collections::BTreeSet;

use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::validation::is_vbus_sense_role;
use hauksbee_models::{declared_pin_ceiling_v, ModelEntry, ModelLibrary};

use hauksbee_bind::binder::power_rail_voltage;
use hauksbee_extract::assembly::AssemblyState;
use hauksbee_ir::units::format_volts;

/// Margin (V) a rail must exceed the part's supply by before the finding
/// fires: one clamp-diode drop, the threshold at which the protection clamp
/// actually CONDUCTS and the back-powering current path exists. This is
/// deliberately keyed off conduction, not the tighter VCC + 0.3..0.5 V
/// abs-max window: rails 0.3..0.6 V above the supply may violate a
/// datasheet's abs-max without conducting meaningfully, and firing there on
/// name-derived nominal rail voltages would trade the check's silence
/// calibration for a band no field failure in the corpus occupies. The
/// finding therefore claims conduction, and the under-reported band is
/// documented here rather than hidden.
const CLAMP_MARGIN_V: f64 = 0.6;

/// Roles that define the part's supply domain (mirrors the scheduler's
/// direct-supply role list for its rail watches).
fn is_supply_role(role: &str) -> bool {
    matches!(
        role,
        "vcc" | "avcc" | "vdd" | "vdda" | "vddio" | "vio" | "5v"
    )
}

/// Roles never judged as signal pins: supplies, grounds, and inputs that are
/// higher-voltage by design (regulator/monitor feeds).
fn is_skipped_role(role: &str) -> bool {
    is_supply_role(role)
        || role.starts_with("gnd")
        || matches!(role, "vss" | "agnd" | "dgnd" | "pgnd" | "gndio" | "nc")
        || matches!(role, "vin" | "vbus" | "vbat" | "vsys" | "raw" | "vmot")
}

/// A plain two-terminal, assembled resistor, on the shared part-class ladder.
/// Value parse is not needed here: the pull's EXISTENCE and its far rail are
/// the evidence; its ohms only scale the clamp current.
fn is_plain_resistor(c: &Component) -> bool {
    // Three-state contract: a DNP pull-up is absent and an identity-refused
    // record is unprovable; neither may count as a rail tie.
    AssemblyState::of(c).is_present() && hauksbee_extract::is_plain_resistor(c)
}

/// The part's supply-domain voltage: the highest resolvable rail among its
/// supply-role pads, with the rail's net name. `None` when no supply pad
/// resolves (the check then has no domain to judge against and stays silent).
fn supply_domain(
    board: &ExtractedBoard,
    comp: &Component,
    model_pins: &std::collections::BTreeMap<String, String>,
) -> Option<(f64, String)> {
    let mut best: Option<(f64, String)> = None;
    for (pad, role) in model_pins {
        if !is_supply_role(role) {
            continue;
        }
        let Some(pin) = comp.pins.iter().find(|p| &p.number == pad) else {
            continue;
        };
        let Some(id) = pin.net.filter(|&id| id != 0) else {
            continue;
        };
        let Some(net) = board.net(id) else { continue };
        let Some(v) = power_rail_voltage(&net.name) else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| v > *b) {
            best = Some((v, net.name.clone()));
        }
    }
    best
}

/// Pull-up resistors from `net_id` to a resolvable rail: `(resistor ref,
/// rail net name, rail volts)`, one entry per resistor.
fn pullups_to_rails(board: &ExtractedBoard, net_id: i64) -> Vec<(String, String, f64)> {
    let mut out = Vec::new();
    for (c, _p) in board.net_members(net_id) {
        if !is_plain_resistor(c) {
            continue;
        }
        for op in &c.pins {
            let Some(oid) = op.net.filter(|&id| id != 0 && id != net_id) else {
                continue;
            };
            let Some(on) = board.net(oid) else { continue };
            if let Some(v) = power_rail_voltage(&on.name) {
                out.push((c.reference.clone(), on.name.clone(), v));
            }
        }
    }
    out
}

/// Resistor tolerance assumed for a divider's worst corner when the part
/// states none: the common 5 % thick-film grade.
const DIVIDER_TOLERANCE: f64 = 0.05;

/// The highest level a resistor divider holds `net_id` at, when every plain
/// resistor on it leads to a resolvable rail or to ground, at least one leads
/// to ground, and every value reads as ohms: pull-ups at their low corner,
/// pull-downs at their high corner. `None` when the net is not such a divider.
fn divider_max_v(board: &ExtractedBoard, net_id: i64) -> Option<f64> {
    let (mut sourced, mut conductance, mut grounded) = (0.0, 0.0, false);
    for (c, _p) in board.net_members(net_id) {
        if !is_plain_resistor(c) {
            continue;
        }
        let far = c
            .pins
            .iter()
            .filter_map(|p| p.net)
            .find(|&id| id != net_id)?;
        let name = &board.net(far)?.name;
        let ohms = hauksbee_models::value::parse_value(&c.value)
            .filter(|v| matches!(v.unit.as_deref(), None | Some("Ω")) && v.si > 0.0)?
            .si;
        if crate::checks::converter::is_ground_net(name) {
            grounded = true;
            conductance += 1.0 / (ohms * (1.0 + DIVIDER_TOLERANCE));
        } else {
            let g = 1.0 / (ohms * (1.0 - DIVIDER_TOLERANCE));
            sourced += power_rail_voltage(name)? * g;
            conductance += g;
        }
    }
    (grounded && conductance > 0.0).then(|| sourced / conductance)
}

/// The sentence that names the model card file when the user supplied it (a
/// `--models-dir` or user-directory card), so the fix can say which file takes
/// the `input_tolerance` row. Empty for a built-in or pack model, which the user
/// does not edit in place.
fn user_card_suffix(lib: &ModelLibrary, comp: &Component) -> String {
    use hauksbee_models::SourceLayer;
    let Some(part) = AssemblyState::of(comp).fitted() else {
        return String::new();
    };
    let resolution = hauksbee_bind::binder::resolve(lib, part);
    match (resolution.layer, resolution.origin) {
        (
            Some(SourceLayer::ModelsDirFlag | SourceLayer::UserDir | SourceLayer::UserConfigDir),
            Some(origin),
        ) => format!("{}{origin}", hauksbee_extract::MODEL_CARD_MESSAGE_MARKER),
        _ => String::new(),
    }
}

/// A level within this of a pin's declared ceiling is on it, not over it.
const CEILING_EPS_V: f64 = 1e-6;

/// Whether `level_v` is past the rating the card declares for the pin.
fn over_rating(ceiling: Option<f64>, level_v: f64) -> bool {
    ceiling.is_some_and(|c| level_v > c + CEILING_EPS_V)
}

/// How bad a pull-up above the part's supply is. A family-router model derives
/// its pad roles from CAD pin names and carries no per-pin tolerance, so the
/// "unless tolerant" exception is unverifiable and the finding is only a note
/// (STM32 FT pins such as PA9 are 5 V tolerant). A pull-up to a rail above a
/// rating the card states for the pin leaves no exception to verify: High.
/// A VBUS-sense pin the card does not rate sees a bus above its supply by
/// design, so it is only a note to confirm the pin's maximum. Otherwise the
/// exception is for the reader to check: Medium.
fn pull_up_severity(
    model: &ModelEntry,
    ceiling: Option<f64>,
    level_v: f64,
    unrated_vbus: bool,
) -> Severity {
    if model.params.get_str("auto_bind") == Some("family_router") {
        Severity::Low
    } else if over_rating(ceiling, level_v) {
        Severity::High
    } else if unrated_vbus {
        Severity::Low
    } else {
        Severity::Medium
    }
}

/// The closing clause of a pull-up finding: the tolerance exception to verify,
/// or the card's own rating the level is above.
fn tolerance_clause(ceiling: Option<f64>, level_v: f64) -> String {
    ceiling.filter(|c| level_v > *c).map_or_else(
        || {
            format!(
                "unless this pin is explicitly {}-tolerant.",
                format_volts(level_v)
            )
        },
        |c| {
            format!(
                "and the card rates this pin for only {}, below the {} rail: past its stated \
                 absolute maximum whatever the supply does.",
                format_volts(c),
                format_volts(level_v)
            )
        },
    )
}

/// What a VBUS-sense pin the card does not rate is told: it sees a bus above
/// its part's supply by design, and the pin's own maximum is the card's to say.
fn unrated_vbus_clause(role: &str) -> String {
    format!(
        " {role} is a VBUS-sense pin, which sees a bus above its part's supply by design, and the \
         card states no input_tolerance row for it: confirm the pin's absolute maximum in the \
         datasheet's pin table, or add the row so this check holds the pin to it."
    )
}

/// One pin held past the ground-referenced rating its card declares, by a
/// level that is not above its supply's clamp: nothing here is a back-power
/// path, so the finding names the rating and not the clamp.
struct PastRating<'a> {
    part: &'a str,
    pad: &'a str,
    role: &'a str,
    net: &'a str,
    how: String,
    level_v: f64,
    ceiling_v: f64,
    supply: String,
}

impl PastRating<'_> {
    fn message(&self) -> String {
        let Self {
            part,
            pad,
            role,
            net,
            how,
            level_v,
            ceiling_v,
            supply,
        } = self;
        format!(
            "{part} pad {pad} ({role}) on \"{net}\" {how}, which holds it at {}, but the card \
             rates this pin for only {}: past its stated absolute maximum whatever {part}'s \
             supply {supply} does.",
            format_volts(*level_v),
            format_volts(*ceiling_v)
        )
    }
}

/// The first push-pull output of another part on net `id`, with the level it
/// swings to (its own supply domain): `(reference, role, volts, supply net)`.
///
/// An output the card marks open-drain only pulls low and raises nothing.
fn driver_on_net(
    board: &ExtractedBoard,
    parts: &[(&Component, ModelEntry)],
    skip: &str,
    id: i64,
) -> Option<(String, String, f64, String)> {
    parts
        .iter()
        .filter(|(c, _)| c.reference != skip)
        .find_map(|(c, m)| {
            let pad = c.pins.iter().find(|p| {
                p.net == Some(id)
                    && m.pins.get(&p.number).is_some_and(|role| {
                        m.logic.outputs.iter().any(|o| o == role) && !m.is_open_drain_pad(&p.number)
                    })
            })?;
            let (volts, rail) = supply_domain(board, c, &m.pins)?;
            Some((
                c.reference.clone(),
                m.pins[&pad.number].clone(),
                volts,
                rail,
            ))
        })
}

/// One signal pin under judgement: its part, card, net, supply domain and the
/// rating its card declares for it.
struct PinUnderJudgement<'a> {
    board: &'a ExtractedBoard,
    lib: &'a ModelLibrary,
    comp: &'a Component,
    model: &'a ModelEntry,
    pad: &'a str,
    role: &'a str,
    id: i64,
    net: &'a str,
    supply_v: f64,
    supply_net: &'a str,
    /// The pin's own ground-referenced ceiling when the card declares one.
    ceiling: Option<f64>,
    /// A VBUS-sense pin the card does not rate.
    unrated_vbus: bool,
}

/// A finding already reported: (part ref, net id, rail or driver), so a bus
/// with several pull-ups to one rail, or a part with two pins on the net,
/// reads once.
type Seen = (String, i64, String);

impl PinUnderJudgement<'_> {
    /// Whether `level_v` is a fault for this pin: past the card's rating when it
    /// states one, whatever the supply, else one clamp drop above the supply.
    fn judged(&self, level_v: f64) -> bool {
        self.ceiling
            .map_or(level_v > self.supply_v + CLAMP_MARGIN_V, |c| {
                level_v > c + CEILING_EPS_V
            })
    }

    /// The rating when `level_v` is past it without the clamp conducting, so the
    /// finding speaks of the rating and not of back-powering.
    fn rating_only(&self, level_v: f64) -> Option<f64> {
        self.ceiling.filter(|_| {
            over_rating(self.ceiling, level_v) && level_v <= self.supply_v + CLAMP_MARGIN_V
        })
    }

    /// The closing words every finding on this pin carries.
    fn tail(&self) -> String {
        let vbus = if self.unrated_vbus {
            unrated_vbus_clause(self.role)
        } else {
            String::new()
        };
        vbus + &user_card_suffix(self.lib, self.comp)
    }

    /// The message of a pin held past its card's rating by `how`.
    fn past_rating(&self, how: String, level_v: f64, ceiling_v: f64) -> String {
        PastRating {
            part: &self.comp.reference,
            pad: self.pad,
            role: self.role,
            net: self.net,
            how,
            level_v,
            ceiling_v,
            supply: format!("{} ({})", self.supply_net, format_volts(self.supply_v)),
        }
        .message()
    }

    /// The pin's own net IS a higher rail.
    fn direct_tie(&self, rail_v: f64, seen: &mut BTreeSet<Seen>, out: &mut Vec<LintFinding>) {
        let reference = &self.comp.reference;
        if !self.judged(rail_v) || !seen.insert((reference.clone(), self.id, self.net.to_string()))
        {
            return;
        }
        let message = self.rating_only(rail_v).map_or_else(
            || {
                format!(
                    "{reference} pad {} ({}) is wired directly to {} ({}) while {reference}'s \
                     supply {} is {}: the pin's protection clamp conducts from the higher rail \
                     into the {} domain with nothing to limit the current (back-powering; past \
                     the VCC+0.5 V input abs-max).",
                    self.pad,
                    self.role,
                    self.net,
                    format_volts(rail_v),
                    self.supply_net,
                    format_volts(self.supply_v),
                    format_volts(self.supply_v)
                )
            },
            |c| self.past_rating("is wired directly to a rail".into(), rail_v, c),
        );
        out.push(LintFinding {
            check: LintCheck::BackPower,
            severity: if self.unrated_vbus {
                Severity::Low
            } else {
                Severity::High
            },
            message: message + &self.tail(),
            refs: vec![reference.clone()],
            nets: vec![self.net.to_string()],
        });
    }

    /// Pull-ups to a higher rail (severity: `pull_up_severity`). A divider that
    /// holds the pin within its supply at the worst resistor corner is how a
    /// higher rail is brought to an input: the clamp never conducts while the
    /// part is powered. A pin the card rates is judged at the divider's level,
    /// not the rail's. Returns whether such a divider settles the pin.
    fn pull_ups(&self, seen: &mut BTreeSet<Seen>, out: &mut Vec<LintFinding>) -> bool {
        let divider = divider_max_v(self.board, self.id);
        if divider.is_some_and(|v| !self.judged(v)) {
            return true;
        }
        let reference = &self.comp.reference;
        for (r_ref, rail_net, rail_v) in pullups_to_rails(self.board, self.id) {
            let level_v = self.ceiling.and(divider).unwrap_or(rail_v);
            if !self.judged(level_v) || !seen.insert((reference.clone(), self.id, rail_net.clone()))
            {
                continue;
            }
            let message = self.rating_only(level_v).map_or_else(
                || {
                    format!(
                        "{reference} pad {} ({}) on \"{}\" is pulled up through {r_ref} to \
                         {rail_net} ({}) while {reference}'s supply {} is {}: current flows \
                         through the pin clamp into the {} domain, back-powering it when it is \
                         off and sitting past the VCC+0.5 V input abs-max when it is on, {}",
                        self.pad,
                        self.role,
                        self.net,
                        format_volts(level_v),
                        self.supply_net,
                        format_volts(self.supply_v),
                        format_volts(self.supply_v),
                        tolerance_clause(self.ceiling, level_v)
                    )
                },
                |c| {
                    self.past_rating(
                        format!("is pulled up through {r_ref} to {rail_net}"),
                        level_v,
                        c,
                    )
                },
            );
            out.push(LintFinding {
                check: LintCheck::BackPower,
                severity: pull_up_severity(self.model, self.ceiling, level_v, self.unrated_vbus),
                message: message + &self.tail(),
                refs: vec![reference.clone(), r_ref],
                nets: vec![self.net.to_string(), rail_net],
            });
        }
        false
    }

    /// A pin the card rates, driven by another part's push-pull output that
    /// swings past the rating (an oscillator's 3.3 V output into a clock input
    /// rated 2.45 V): no rail is tied to the net, so the driver's own supply is
    /// the level it reaches.
    fn driven(
        &self,
        parts: &[(&Component, ModelEntry)],
        seen: &mut BTreeSet<Seen>,
        out: &mut Vec<LintFinding>,
    ) {
        let Some(c) = self.ceiling else { return };
        let reference = &self.comp.reference;
        let Some((driver, out_role, level_v, driver_rail)) =
            driver_on_net(self.board, parts, reference, self.id)
                .filter(|(_, _, v, _)| over_rating(Some(c), *v))
        else {
            return;
        };
        if !seen.insert((reference.clone(), self.id, driver.clone())) {
            return;
        }
        let how =
            format!("is driven by {driver} {out_role}, which swings to its supply {driver_rail}");
        out.push(LintFinding {
            check: LintCheck::BackPower,
            severity: Severity::High,
            message: self.past_rating(how, level_v, c) + &user_card_suffix(self.lib, self.comp),
            refs: vec![reference.clone(), driver],
            nets: vec![self.net.to_string(), driver_rail],
        });
    }
}

/// Run the back-powering lint over every modelled part on the board.
#[must_use]
pub fn back_power_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let mut seen: BTreeSet<Seen> = BTreeSet::new();
    let parts: Vec<_> = super::resolved_components(board, lib).collect();

    for (comp, model) in &parts {
        let Some((supply_v, supply_net)) = supply_domain(board, comp, &model.pins) else {
            continue;
        };

        // Deterministic pad order for stable finding order (`pins` is a BTreeMap).
        for (pad, role) in &model.pins {
            if is_skipped_role(role) {
                continue;
            }
            let Some(pin) = comp.pins.iter().find(|p| &p.number == pad) else {
                continue;
            };
            let Some(id) = pin.net.filter(|&id| id != 0) else {
                continue;
            };
            let Some(net) = board.net(id) else { continue };
            // A signal pin STRAPPED TO GROUND has no clamp path to report, and
            // the pull-up walk would misread the board if it looked: a resistor
            // from ground to a higher rail is a load on that rail (a
            // VBUS-present divider's lower leg, a bleeder), not a pull-up on
            // this pin. The pin's ROLE being a ground role is already skipped
            // above; this is the other half, a signal role whose NET is ground
            // (an I2C address select tied low, a mode pin strapped to 0).
            if crate::checks::converter::is_ground_net(&net.name) {
                continue;
            }
            // A pin the card rates is judged against that rating, whatever the
            // supply: a level at or under it is not a fault (a 5 V-tolerant
            // input takes its 5 V pull-up), a level above it is, even when the
            // supply's own clamp would not conduct yet (a sense pin rated 1.4 V
            // on a 3.3 V part). A pin it does not rate is judged against one
            // clamp drop above its supply.
            let ceiling = declared_pin_ceiling_v(&model.envelope, role);
            let pin = PinUnderJudgement {
                board,
                lib,
                comp,
                model,
                pad,
                role,
                id,
                net: &net.name,
                supply_v,
                supply_net: &supply_net,
                ceiling,
                unrated_vbus: ceiling.is_none() && is_vbus_sense_role(role),
            };
            if let Some(rail_v) = power_rail_voltage(&net.name) {
                pin.direct_tie(rail_v, &mut seen, &mut report.findings);
                continue; // a rail net has no pull-ups to judge
            }
            if !pin.pull_ups(&mut seen, &mut report.findings) {
                pin.driven(&parts, &mut seen, &mut report.findings);
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::two_pin_resistor;
    use hauksbee_extract::LintCheck;

    fn bare_resistor() -> Component {
        two_pin_resistor("R7", "10k")
    }

    /// Two-sided three-state contract: a fitted plain resistor counts as a
    /// rail tie; a DNP or identity-refused record of the same part must not.
    #[test]
    fn dnp_or_refused_resistor_is_not_a_rail_tie() {
        assert!(is_plain_resistor(&bare_resistor()));

        let mut dnp = bare_resistor();
        dnp.dnp = true;
        assert!(!is_plain_resistor(&dnp));

        let mut refused = bare_resistor();
        refused.properties.push((
            hauksbee_extract::DUPLICATE_REFERENCE_CONFLICT_KEY.into(),
            "two contradictory R7 records".into(),
        ));
        assert!(!is_plain_resistor(&refused));
    }

    /// Nets 1 GND, 2 +5V, 3 `/SENSOR_INT` and 5 +3V3 holding an ESP-WROOM-32
    /// (3.3 V part: pad 2 = vdd on +3V3) whose GPIO pad 25 sits on net
    /// `gpio`, plus a 0603 resistor `(reference, value, pad 1 net, pad 2 net)`
    /// when one is given.
    fn esp32_board(gpio: i64, resistor: Option<(&str, &str, i64, i64)>) -> String {
        let name = |net: i64| match net {
            1 => "GND",
            2 => "+5V",
            3 => "/SENSOR_INT",
            _ => "+3V3",
        };
        let resistor = resistor.map_or_else(String::new, |(reference, value, a, b)| {
            format!(
                r#"
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 120 100)
    (fp_text reference {reference} (at 0 0) (layer F.SilkS))
    (fp_text value {value} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net {a} "{}"))
    (pad 2 smd rect (at 2 0) (net {b} "{}"))
  )"#,
                name(a),
                name(b)
            )
        });
        format!(
            r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+5V")
  (net 3 "/SENSOR_INT")
  (net 5 "+3V3")
  (module RF_Module:ESP32-WROOM-32 (layer F.Cu)
    (at 100 100)
    (fp_text reference U3 (at 0 0) (layer F.SilkS))
    (fp_text value ESP-WROOM-32 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 1 "GND"))
    (pad 2 smd rect (at 0 1) (net 5 "+3V3"))
    (pad 25 smd rect (at 0 5) (net {gpio} "{}"))
  ){resistor}
)"#,
            name(gpio)
        )
    }

    /// The ESP32 whose GPIO0 net carries a 10k pull-up R7. `rail` picks the
    /// pull-up's far side: "+5V" back-powers, "+3V3" is the correct
    /// same-domain bias.
    fn esp32_pull_board(rail: &str) -> String {
        let rail_net = if rail == "+5V" { 2 } else { 5 };
        esp32_board(3, Some(("R7", "10k", 3, rail_net)))
    }

    fn run(text: &str) -> NetLintReport {
        let board = ExtractedBoard::from_kicad_pcb(text).expect("parse synthetic board");
        let lib = ModelLibrary::builtin();
        back_power_lint(&board, &lib)
    }

    /// Faulty side: a 3.3 V-supplied ESP32 GPIO pulled to +5V must fire one
    /// Medium finding naming the part, the resistor, and both rails.
    #[test]
    fn pullup_above_supply_domain_fires() {
        let r = run(&esp32_pull_board("+5V"));
        let f: Vec<_> = r.of_check(LintCheck::BackPower).collect();
        assert_eq!(f.len(), 1, "exactly one back-power finding, got {f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].refs.contains(&"U3".to_string()));
        assert!(f[0].refs.contains(&"R7".to_string()));
        assert!(
            f[0].message.contains("5 V") && f[0].message.contains("3.3 V"),
            "finding must carry both domain voltages: {}",
            f[0].message
        );
    }

    /// An MCU with no model-DB entry binds through the family router, whose
    /// pad roles come from CAD pin names and carry no per-pin tolerance. The
    /// same 5 V pull-up that is a Medium fault on a DB-modelled part is then
    /// only a Low note (an STM32 FT pin such as PA9 tolerates 5 V).
    #[test]
    fn family_router_binding_downgrades_the_pullup_finding_to_low() {
        let board = r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (net 0 "") (net 1 "GND") (net 2 "+5V") (net 3 "/SIG") (net 4 "+3V3")
  (footprint "Package_QFP:LQFP-100" (layer "F.Cu") (at 100 100)
    (property "Reference" "U9" (at 0 0)) (property "Value" "STM32F446VET6" (at 0 2))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 4 "+3V3") (pinfunction "VDD"))
    (pad "2" smd rect (at 0 1) (size 1 1) (layers "F.Cu") (net 1 "GND") (pinfunction "VSS"))
    (pad "3" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 3 "/SIG") (pinfunction "PA9")))
  (footprint "Resistor_SMD:R_0603_1608Metric" (layer "F.Cu") (at 120 100)
    (property "Reference" "R7" (at 0 0)) (property "Value" "10k" (at 0 2))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 3 "/SIG"))
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 2 "+5V"))))"#;
        let r = run(board);
        let f: Vec<_> = r.of_check(LintCheck::BackPower).collect();
        assert_eq!(
            f.len(),
            1,
            "the pull-up is still reported: {:?}",
            r.findings
        );
        assert_eq!(f[0].severity, Severity::Low);
    }

    /// Faulty/legal pair: a divider from +5V whose worst resistor corner
    /// still holds the 3.3 V part's pin under its supply is how a higher rail
    /// reaches an input, so it is silent; a lower leg too weak to pull the
    /// pin under the clamp leaves the pull-up a back-powering fault.
    #[test]
    fn a_divider_holding_the_pin_within_its_supply_is_silent() {
        let divider = |up: &str, down: &str| {
            let text = esp32_board(3, Some(("R7", up, 3, 2)));
            let lower = format!(
                r#"
  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 130 100)
    (fp_text reference R8 (at 0 0) (layer F.SilkS))
    (fp_text value {down} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "/SENSOR_INT"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
)"#
            );
            let body = text.trim_end().strip_suffix(')').expect("board closes");
            run(&format!("{body}{lower}"))
                .of_check(LintCheck::BackPower)
                .count()
        };
        assert_eq!(divider("470k", "470k"), 0, "2.6 V at the worst corner");
        assert_eq!(divider("10k", "100k"), 1, "4.6 V at the worst corner");
    }

    /// Clean side: the same pull-up to the part's OWN +3V3 rail is the
    /// correct bias and must stay silent.
    #[test]
    fn pullup_to_own_rail_is_silent() {
        let r = run(&esp32_pull_board("+3V3"));
        assert_eq!(
            r.of_check(LintCheck::BackPower).count(),
            0,
            "same-domain pull-up is the normal idiom, got {:?}",
            r.findings
        );
    }

    /// The clamp margin is pinned from the silent side too: a rail 0.3 V
    /// above the 3.3 V supply (+3V6) sits below one diode drop, the clamp
    /// does not conduct, and the check must stay silent. Together with the
    /// +5V case (1.7 V over) this bounds the margin inside (0.3, 1.7) V, so
    /// a margin of zero (fires on every mixed-nominal board) or of several
    /// volts (never fires) both fail.
    #[test]
    fn sub_clamp_margin_rail_is_silent() {
        // Rename net 2 from +5V to +3V6 everywhere (declaration + pads), so
        // the pull-up lands on a rail only 0.3 V above the supply.
        let board = esp32_pull_board("+5V").replace("(net 2 \"+5V\")", "(net 2 \"+3V6\")");
        let r = run(&board);
        assert_eq!(
            r.of_check(LintCheck::BackPower).count(),
            0,
            "+3V6 is 0.3 V over the 3.3 V supply, under the clamp drop, got {:?}",
            r.findings
        );
    }

    /// Direct tie of a signal pin to a higher rail is High: no series element
    /// bounds the clamp current.
    #[test]
    fn direct_tie_to_higher_rail_is_high() {
        let text = esp32_board(2, None);
        let r = run(&text);
        let f: Vec<_> = r.of_check(LintCheck::BackPower).collect();
        assert_eq!(f.len(), 1, "direct tie fires, got {f:?}");
        assert_eq!(f[0].severity, Severity::High);
    }

    /// A signal pin STRAPPED TO GROUND stays silent even when the board hangs a
    /// resistor from ground to a higher rail. That resistor (a VBUS-present
    /// divider's lower leg, a bleeder) is a load on the rail, not a pull-up on
    /// the pin, and reading it as one fires on every strapped-low configuration
    /// pin of every modelled part on such a board. Watchy is the case: R2 sits
    /// between VBUS and GND, and the BMA423's SDO address-select pad is tied
    /// low, which is the part working as designed at address 0x18.
    #[test]
    fn grounded_signal_pin_is_silent_even_with_a_resistor_from_ground_to_a_rail() {
        let text = esp32_board(1, Some(("R2", "100k", 2, 1)));
        let r = run(&text);
        assert_eq!(
            r.of_check(LintCheck::BackPower).count(),
            0,
            "a pin strapped to ground is not pulled up by a bleeder to a rail, got {:?}",
            r.findings
        );
    }

    /// A card that rates a signal pin with its own `supply_range` envelope
    /// (a current-sense amplifier's IN+ takes 40 V from a 3.3 V supply) is
    /// judged against that rating: a rail inside it is silent, a rail above it
    /// still fires, and the same board on a card without the rating fires.
    #[test]
    fn a_declared_pin_rating_replaces_the_supply_clamp_assumption() {
        let card = |envelope: &str| {
            format!(
                r#"
[[models]]
id = "sense_amp"
kind = "digital"
description = "pin-rating fixture"
[models.match]
value_re = "^SENSE_AMP$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vcc"
"2" = "gnd"
"3" = "in_plus"
{envelope}"#
            )
        };
        let rated = card(
            r#"[[models.envelope]]
kind = "supply_range"
pin = "in_plus"
min_v = -0.2
max_v = 40.0
abs_max_v = 42.0
basis = "Absolute Maximum Ratings, IN+ row""#,
        );
        let board = |rail: &str| {
            format!(
                r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (net 0 "") (net 1 "GND") (net 2 "{rail}") (net 4 "+3V3")
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 100 100)
    (property "Reference" "U4" (at 0 0)) (property "Value" "SENSE_AMP" (at 0 2))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 4 "+3V3"))
    (pad "2" smd rect (at 0 1) (size 1 1) (layers "F.Cu") (net 1 "GND"))
    (pad "3" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 2 "{rail}"))))"#
            )
        };
        let count = |source: &str, rail: &str| {
            let dir = std::env::temp_dir().join(format!(
                "hauksbee_back_power_{}_{}",
                source.len(),
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("models.toml"), source).unwrap();
            let mut lib = ModelLibrary::empty();
            assert!(lib.load_user_dir(&dir).is_empty(), "fixture card loads");
            let parsed = ExtractedBoard::from_kicad_pcb(&board(rail)).expect("parse");
            back_power_lint(&parsed, &lib)
                .of_check(LintCheck::BackPower)
                .count()
        };
        assert_eq!(count(&rated, "+5V"), 0, "5 V is inside the pin's rating");
        assert_eq!(count(&rated, "+48V"), 1, "48 V is above the pin's rating");
        assert_eq!(
            count(&card(""), "+5V"),
            1,
            "no rating: the clamp rule holds"
        );
    }

    /// A sense input's printed range is a measurement window, never a supply
    /// range: a card that carries it as a `supply_range` is refused at load, so
    /// it cannot pose as the pin's (or the part's) rail. The pin's real
    /// ground-referenced rating is an `input_tolerance` row, which the check
    /// honours the same way as for any other input.
    #[test]
    fn a_sense_input_range_is_not_a_supply_range_but_its_tolerance_row_is_honoured() {
        let card = |envelope: &str| {
            format!(
                r#"
[[models]]
id = "shunt_mon"
kind = "digital"
description = "sense-input fixture"
[models.match]
value_re = "^SHUNT_MON$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vcc"
"2" = "gnd"
"3" = "isense"
{envelope}"#
            )
        };
        let board = |rail: &str| {
            format!(
                r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (net 0 "") (net 1 "GND") (net 2 "{rail}") (net 4 "+3V3")
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 100 100)
    (property "Reference" "U4" (at 0 0)) (property "Value" "SHUNT_MON" (at 0 2))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 4 "+3V3"))
    (pad "2" smd rect (at 0 1) (size 1 1) (layers "F.Cu") (net 1 "GND"))
    (pad "3" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 2 "{rail}"))))"#
            )
        };
        let load = |tag: &str, source: &str| {
            let dir = std::env::temp_dir().join(format!(
                "hauksbee_back_power_sense_{tag}_{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("models.toml"), source).unwrap();
            let mut lib = ModelLibrary::empty();
            let errors = lib.load_user_dir(&dir);
            (lib, errors)
        };
        let count = |lib: &ModelLibrary, rail: &str| {
            let parsed = ExtractedBoard::from_kicad_pcb(&board(rail)).expect("parse");
            back_power_lint(&parsed, lib)
                .of_check(LintCheck::BackPower)
                .count()
        };

        let as_supply = card(
            r#"[[models.envelope]]
kind = "supply_range"
pin = "isense"
min_v = -0.3
max_v = 0.3
abs_max_v = 0.5
basis = "Absolute Maximum Ratings, sense row""#,
        );
        let (_, errors) = load("supply", &as_supply);
        assert!(
            errors
                .iter()
                .any(|e| e.to_string().contains("sense or measurement input")),
            "a sense input rated as a supply is refused, got {errors:?}"
        );

        let tolerant = card(
            r#"[[models.envelope]]
kind = "input_tolerance"
pins = ["isense"]
max_v = 26.0
abs_max_v = 28.0
basis = "Recommended Operating Conditions, sense input to GND""#,
        );
        let (lib, errors) = load("tolerance", &tolerant);
        assert!(errors.is_empty(), "tolerance card loads: {errors:?}");
        assert_eq!(count(&lib, "+5V"), 0, "5 V is inside the sense rating");
        assert_eq!(count(&lib, "+48V"), 1, "48 V is above the sense rating");
        let (bare, errors) = load("bare", &card(""));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(count(&bare, "+5V"), 1, "no rating: the clamp rule holds");
    }

    /// A finding on a part whose card is a user-supplied file names that file,
    /// so the fix can say where the `input_tolerance` row goes; a built-in
    /// model names none (the user does not edit it in place).
    #[test]
    fn a_user_supplied_card_is_named_in_the_finding_and_a_builtin_is_not() {
        let card = r#"
[[models]]
id = "sense_amp"
kind = "digital"
description = "card-file fixture"
[models.match]
value_re = "^SENSE_AMP$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vcc"
"2" = "gnd"
"3" = "in_plus"
"#;
        let board = r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (net 0 "") (net 1 "GND") (net 2 "+5V") (net 4 "+3V3")
  (footprint "Package_TO_SOT_SMD:SOT-23" (layer "F.Cu") (at 100 100)
    (property "Reference" "U4" (at 0 0)) (property "Value" "SENSE_AMP" (at 0 2))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 4 "+3V3"))
    (pad "2" smd rect (at 0 1) (size 1 1) (layers "F.Cu") (net 1 "GND"))
    (pad "3" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 2 "+5V"))))"#;
        let dir = std::env::temp_dir().join(format!(
            "hauksbee_back_power_card_file_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sense_amp.toml");
        std::fs::write(&file, card).unwrap();
        let mut lib = ModelLibrary::empty();
        assert!(
            lib.load_dir_layer(&dir, hauksbee_models::SourceLayer::ModelsDirFlag)
                .is_empty(),
            "fixture card loads"
        );
        let parsed = ExtractedBoard::from_kicad_pcb(board).expect("parse");
        let report = back_power_lint(&parsed, &lib);
        let finding = report.of_check(LintCheck::BackPower).next().expect("fires");
        assert_eq!(
            hauksbee_extract::model_card_file_in(&finding.message),
            Some(file.display().to_string().as_str()),
            "{}",
            finding.message
        );

        let builtin = run(&esp32_pull_board("+5V"));
        let finding = builtin
            .of_check(LintCheck::BackPower)
            .next()
            .expect("fires");
        assert_eq!(
            hauksbee_extract::model_card_file_in(&finding.message),
            None,
            "{}",
            finding.message
        );
    }

    /// A quad buffer `value` (74x125 pinout: pad 1 OE1, 2 A1, 3 Y1, 7 GND,
    /// 14 VCC) on +3V3 whose pad `pad` carries a 10k pull-up to `rail`.
    fn quad_buffer_pull_board(value: &str, pad: u8, rail: &str) -> String {
        format!(
            r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (net 0 "") (net 1 "GND") (net 2 "{rail}") (net 3 "/SIG") (net 4 "+3V3")
  (footprint "Package_SO:SOIC-14_3.9x8.7mm_P1.27mm" (layer "F.Cu") (at 100 100)
    (property "Reference" "U5" (at 0 0)) (property "Value" "{value}" (at 0 2))
    (pad "{pad}" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 3 "/SIG"))
    (pad "7" smd rect (at 0 1) (size 1 1) (layers "F.Cu") (net 1 "GND"))
    (pad "14" smd rect (at 0 2) (size 1 1) (layers "F.Cu") (net 4 "+3V3")))
  (footprint "Resistor_SMD:R_0603_1608Metric" (layer "F.Cu") (at 120 100)
    (property "Reference" "R7" (at 0 0)) (property "Value" "10k" (at 0 2))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 3 "/SIG"))
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net 2 "{rail}"))))"#
        )
    }

    /// A 5 V-tolerant input (the curated 74LVC125A card's `input_tolerance`,
    /// VI up to 5.5 V recommended and 6.5 V absolute whatever VCC is) takes a
    /// 5 V pull-up at a 3.3 V supply: silent. The 74HC125, whose inputs are
    /// rated only to VCC + 0.5 V, still fires on the same board, and so does
    /// the LVC part's driven output and a pull-up above the declared ceiling.
    #[test]
    fn a_five_volt_tolerant_input_takes_a_five_volt_pull_up() {
        let count = |value: &str, pad: u8, rail: &str| {
            run(&quad_buffer_pull_board(value, pad, rail))
                .of_check(LintCheck::BackPower)
                .count()
        };
        assert_eq!(
            count("74LVC125AD", 2, "+5V"),
            0,
            "an LVC input is 5 V-tolerant at VCC 3.3 V"
        );
        assert_eq!(
            count("74LVC125AD", 1, "+5V"),
            0,
            "the LVC output-enable input is 5 V-tolerant too"
        );
        assert_eq!(
            count("74HC125D", 2, "+5V"),
            1,
            "a 74HC input is rated only to VCC + 0.5 V"
        );
        assert_eq!(
            count("74LVC125AD", 3, "+5V"),
            1,
            "the LVC output is not covered by the input rating"
        );
        assert_eq!(
            count("74LVC125AD", 2, "+12V"),
            1,
            "12 V is above the input's 6.5 V absolute maximum"
        );
        let over = run(&quad_buffer_pull_board("74LVC125AD", 2, "+12V"));
        let over = over.of_check(LintCheck::BackPower).next().expect("fires");
        assert_eq!(
            over.severity,
            Severity::High,
            "a rail above the card's stated 6.5 V maximum leaves no exception to verify"
        );
        assert!(
            over.message.contains("rates this pin for only"),
            "{}",
            over.message
        );
        let clamp = run(&quad_buffer_pull_board("74HC125D", 2, "+5V"));
        assert_eq!(
            clamp
                .of_check(LintCheck::BackPower)
                .next()
                .expect("fires")
                .severity,
            Severity::Medium,
            "a pin the card rates only through its supply keeps the tolerance caveat"
        );
    }

    /// A library holding only the cards of `source`, loaded from a scratch user
    /// directory named for `tag`.
    fn card_lib(tag: &str, source: &str) -> ModelLibrary {
        let dir = std::env::temp_dir().join(format!(
            "hauksbee_back_power_rating_{tag}_{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.toml"), source).unwrap();
        let mut lib = ModelLibrary::empty();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "fixture card loads: {errors:?}");
        lib
    }

    /// One footprint `reference` of `value` with `(pad, net id, net name)` pads.
    fn fp(reference: &str, value: &str, pads: &[(&str, i64, &str)]) -> String {
        use std::fmt::Write as _;
        let mut pad_text = String::new();
        for (pad, id, net) in pads {
            write!(
                pad_text,
                r#"(pad "{pad}" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net {id} "{net}"))"#
            )
            .unwrap();
        }
        let pads = pad_text;
        let package = if reference.starts_with('R') {
            "Resistor_SMD:R_0603_1608Metric"
        } else {
            "Package_SO:SOIC-8"
        };
        format!(
            r#"(footprint "{package}" (layer "F.Cu") (at 100 100)
  (property "Reference" "{reference}" (at 0 0)) (property "Value" "{value}" (at 0 2)) {pads})"#
        )
    }

    /// The board of `parts` (from [`fp`]) over the declared `(id, name)` nets.
    fn board_of(nets: &[(i64, &str)], parts: &[String]) -> ExtractedBoard {
        use std::fmt::Write as _;
        let mut declared = String::new();
        for (id, name) in nets {
            write!(declared, "(net {id} \"{name}\") ").unwrap();
        }
        let text = format!(
            "(kicad_pcb (version 20221018) (generator pcbnew) (net 0 \"\") {declared} {})",
            parts.join("\n")
        );
        ExtractedBoard::from_kicad_pcb(&text).expect("parse")
    }

    /// A hub-like card with a VBUS-sense pin, a clock input and a plain GPIO on
    /// a 3.3 V supply; `rows` are its `[[models.envelope]]` rows.
    fn hub_card(rows: &str) -> String {
        format!(
            r#"
[[models]]
id = "hub"
kind = "digital"
description = "pin-rating fixture"
[models.match]
value_re = "^HUB$"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
[models.pins]
"1" = "vdd"
"2" = "gnd"
"3" = "usb_vbus"
"4" = "xi"
"5" = "gpio"
{rows}"#
        )
    }

    fn tolerance(role: &str, max_v: f64, abs_max_v: f64) -> String {
        format!(
            r#"[[models.envelope]]
kind = "input_tolerance"
pins = ["{role}"]
max_v = {max_v}
abs_max_v = {abs_max_v}
basis = "Recommended Operating Conditions, {role}"
abs_max_basis = "Absolute Maximum Ratings, {role}"
"#
        )
    }

    /// The back-power findings of the hub on +3V3 whose pad `pad` sits on a net
    /// pulled to `rail` through `up` and, when given, down to ground through `down`.
    fn hub_findings(
        lib: &ModelLibrary,
        pad: &str,
        rail: &str,
        up: &str,
        down: Option<&str>,
    ) -> Vec<LintFinding> {
        let nets = [(1, "GND"), (2, rail), (3, "/SENSE"), (4, "+3V3")];
        let mut parts = vec![
            fp(
                "U1",
                "HUB",
                &[("1", 4, "+3V3"), ("2", 1, "GND"), (pad, 3, "/SENSE")],
            ),
            fp("R1", up, &[("1", 3, "/SENSE"), ("2", 2, rail)]),
        ];
        if let Some(down) = down {
            parts.push(fp("R2", down, &[("1", 3, "/SENSE"), ("2", 1, "GND")]));
        }
        let board = board_of(&nets, &parts);
        back_power_lint(&board, lib)
            .of_check(LintCheck::BackPower)
            .cloned()
            .collect()
    }

    /// A 5 V-tolerant VBUS-sense pin pulled up to the USB bus is the part working
    /// as designed: the card's `input_tolerance` row decides, not the supply
    /// clamp. The same pull on a pin the card does not rate as tolerant is still
    /// a back-power fault, and a sense pin the card does not rate at all is a
    /// note to confirm its maximum, never a Medium warning.
    #[test]
    fn a_tolerant_vbus_sense_pin_is_not_back_powering() {
        let tolerant = card_lib("tolerant", &hub_card(&tolerance("usb_vbus", 5.5, 6.0)));
        assert!(
            hub_findings(&tolerant, "3", "+5V", "100k", None).is_empty(),
            "5 V into a pin the card rates for 6 V"
        );
        // Control: a plain GPIO of the same card, pulled up to 5 V, is reported.
        let gpio = hub_findings(&tolerant, "5", "+5V", "100k", None);
        assert_eq!(gpio.len(), 1, "{gpio:?}");
        assert_eq!(gpio[0].severity, Severity::Medium);
        assert!(gpio[0].message.contains("(gpio)"), "{}", gpio[0].message);

        // The sense pin with no rating on the card: a Low note, naming the fix.
        let unrated = card_lib("unrated", &hub_card(""));
        let note = hub_findings(&unrated, "3", "+5V", "100k", None);
        assert_eq!(note.len(), 1, "{note:?}");
        assert_eq!(note[0].severity, Severity::Low);
        assert!(
            note[0].message.contains("VBUS-sense") && note[0].message.contains("input_tolerance"),
            "{}",
            note[0].message
        );
        // The same unrated pull on a pin that is no sense pin keeps its warning.
        let plain = hub_findings(&unrated, "5", "+5V", "100k", None);
        assert_eq!(plain[0].severity, Severity::Medium, "{plain:?}");
    }

    /// A pin the card rates below the clamp's reach is held to the rating: a
    /// 1.4 V sense pin on a 3.3 V part fed from the 5 V bus is past its maximum,
    /// and so is one a divider leaves at 2.5 V; the datasheet's own divider
    /// (90.9 k over 10 k, 0.5 V) is inside it.
    #[test]
    fn a_vbus_sense_pin_fed_past_its_own_maximum_is_reported() {
        let lib = card_lib("low", &hub_card(&tolerance("usb_vbus", 1.155, 1.4)));
        let direct = hub_findings(&lib, "3", "+5V", "100R", None);
        assert_eq!(direct.len(), 1, "{direct:?}");
        assert_eq!(direct[0].severity, Severity::High);
        assert!(
            direct[0].message.contains("rates this pin for only 1.4 V")
                && direct[0].message.contains("5 V"),
            "{}",
            direct[0].message
        );
        let halved = hub_findings(&lib, "3", "+5V", "100k", Some("100k"));
        assert_eq!(halved.len(), 1, "2.5 V is over 1.4 V: {halved:?}");
        assert!(halved[0].message.contains("2.6"), "{}", halved[0].message);
        let divided = hub_findings(&lib, "3", "+5V", "90.9k", Some("10k"));
        assert!(
            divided.is_empty(),
            "0.5 V is inside the rating: {divided:?}"
        );
        // A pin tied straight to a rail the supply clamp would let through: 3.3 V
        // on a 2.45 V clock input is past its rating though not above the supply.
        let clock = card_lib("clock", &hub_card(&tolerance("xi", 2.45, 2.45)));
        let tied = hub_findings(&clock, "4", "+3V3", "100R", None);
        assert_eq!(tied.len(), 1, "{tied:?}");
        assert!(tied[0].message.contains("2.45 V"), "{}", tied[0].message);
        // Control: the same board on a card that rates the pin at 3.6 V is silent.
        let roomy = card_lib("roomy", &hub_card(&tolerance("xi", 3.6, 3.9)));
        assert!(hub_findings(&roomy, "4", "+3V3", "100R", None).is_empty());
    }

    /// An oscillator's push-pull output swings to its own supply: into a clock
    /// input rated below that, with no rail on the net, the pin is past its
    /// maximum. The same oscillator on a lower supply, or an open-drain
    /// output, leaves the pin alone.
    #[test]
    fn a_clock_input_driven_past_its_own_maximum_is_reported() {
        let source = format!(
            "{}{}",
            hub_card(&tolerance("xi", 2.0, 2.45)),
            r#"
[[models]]
id = "osc"
kind = "digital"
description = "oscillator fixture"
[models.match]
value_re = "^OSC$"
[models.pins]
"1" = "en"
"2" = "gnd"
"3" = "out"
"4" = "vdd"
[models.logic]
inputs = ["en"]
outputs = ["out"]
[models.logic.comb]
out = "en"
"#
        );
        let lib = card_lib("osc", &source);
        let run = |osc_rail: &str| {
            let nets = [(1, "GND"), (2, osc_rail), (3, "/XIN"), (4, "+3V3")];
            let parts = [
                fp(
                    "U1",
                    "HUB",
                    &[("1", 4, "+3V3"), ("2", 1, "GND"), ("4", 3, "/XIN")],
                ),
                fp(
                    "U2",
                    "OSC",
                    &[
                        ("1", 2, osc_rail),
                        ("2", 1, "GND"),
                        ("3", 3, "/XIN"),
                        ("4", 2, osc_rail),
                    ],
                ),
            ];
            let board = board_of(&nets, &parts);
            back_power_lint(&board, &lib)
                .of_check(LintCheck::BackPower)
                .cloned()
                .collect::<Vec<_>>()
        };
        let over = run("+3V3");
        assert_eq!(over.len(), 1, "{over:?}");
        assert_eq!(over[0].severity, Severity::High);
        for says in ["U1 pad 4 (xi)", "U2 out", "3.3 V", "2.45 V"] {
            assert!(
                over[0].message.contains(says),
                "{says:?} in {}",
                over[0].message
            );
        }
        assert!(run("+1V8").is_empty(), "1.8 V is inside the 2.45 V rating");
    }

    /// A part whose supply net the rail table cannot resolve has no domain
    /// evidence, so the check must stay silent rather than guess.
    #[test]
    fn unresolvable_supply_stays_silent() {
        // The +5V pull-up board with its supply renamed to a bare VDD.
        let text = esp32_pull_board("+5V").replace("\"+3V3\"", "\"VDD\"");
        let r = run(&text);
        assert_eq!(
            r.of_check(LintCheck::BackPower).count(),
            0,
            "bare VDD carries no magnitude; no guessed voltage may support a finding"
        );
    }
}
