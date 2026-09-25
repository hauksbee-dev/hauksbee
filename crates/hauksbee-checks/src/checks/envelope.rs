//! Sourced operating-envelope checks.
//!
//! The checker derives conservative DC rail intervals from declared supplies,
//! modelled converter outputs, and assembly state, then compares connected pin
//! roles with model-authored operating limits. Unknown rail authority remains
//! unknown; it is never replaced by a nominal voltage merely to produce a
//! verdict. Findings retain the limit, inferred range, model source, and any
//! uncertainty that raised their severity.
//!
//! Design rationale: `docs/how-and-why/hauksbee-engine/checks.md`.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_models::{EnvelopeSeverity, ModelEntry, ModelLibrary, OperatingEnvelope};

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

fn regulator_ranges(board: &ExtractedBoard, lib: &ModelLibrary) -> BTreeMap<i64, RailRange> {
    let mut ranges = BTreeMap::new();
    for (comp, model) in super::resolved_components(board, lib) {
        let Some(range) = model_output_range(&model) else {
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
        if let Some(net_id) = role_net_id(comp, &model, role) {
            ranges.insert(net_id, range);
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
fn battery_ranges(board: &ExtractedBoard, lib: &ModelLibrary) -> BTreeMap<i64, Option<RailRange>> {
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

        let Some(part) = AssemblyState::of(comp).fitted() else {
            continue;
        };
        let Some(model) = resolve(lib, part).model else {
            continue;
        };
        let Some(battery_role) = ["bat", "vbat", "battery"].into_iter().find(|role| {
            model
                .pins
                .values()
                .any(|known| known.eq_ignore_ascii_case(role))
        }) else {
            continue;
        };
        let Some(net_id) = role_net_id(comp, &model, battery_role) else {
            continue;
        };
        let range = model
            .params
            .get_str("battery_chemistry")
            .and_then(battery_floor)
            .and_then(|min_v| {
                model_output_range(&model).map(|setpoint| RailRange {
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
    regulators: BTreeMap<i64, RailRange>,
    batteries: BTreeMap<i64, Option<RailRange>>,
}

impl<'a> RailResolver<'a> {
    pub(crate) fn new(
        board: &'a ExtractedBoard,
        lib: &ModelLibrary,
        solved: &'a BTreeMap<String, RailRange>,
    ) -> Self {
        let (regulators, batteries) = (regulator_ranges(board, lib), battery_ranges(board, lib));
        Self {
            board,
            solved,
            regulators,
            batteries,
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

    fn declared(&self, net_id: i64) -> RailKnowledge {
        let Some(net) = self.board.net(net_id) else {
            return RailKnowledge::Unknown;
        };
        if let Some(range) = self.regulators.get(&net_id).copied() {
            return RailKnowledge::Known(range);
        }
        if let Some(range) = self.batteries.get(&net_id) {
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

fn unknown_abstention(net_name: &str, battery_without_chemistry: bool) -> String {
    if battery_without_chemistry {
        format!(
            "operating envelope on net '{net_name}' abstained: the topology identifies a battery net but no charger card declares its chemistry; add battery_chemistry and the sourced charge-voltage range to the charger model"
        )
    } else {
        format!(
            "operating envelope on net '{net_name}' abstained: declare the level with a net_drive, or bind the regulator that defines this net"
        )
    }
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
    envelope_lint_with_tolerance(board, lib, &points.collect(), 0.0, 0.0)
}

fn envelope_lint_with_tolerance(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    solved: &BTreeMap<String, RailRange>,
    reltol: f64,
    vntol: f64,
) -> NetLintReport {
    let resolver = RailResolver::new(board, lib, solved);
    let mut report = NetLintReport::default();
    let mut abstained_nets = BTreeSet::new();

    for (comp, model) in super::resolved_components(board, lib) {
        for envelope in &model.envelope {
            match envelope {
                OperatingEnvelope::SupplyRange {
                    pin,
                    min_v,
                    max_v,
                    abs_max_v,
                    basis,
                    ..
                } => {
                    let Some(net_id) = role_net_id(comp, &model, pin) else {
                        continue;
                    };
                    let Some(net) = board.net(net_id) else {
                        continue;
                    };
                    let range = match resolver.range(net_id) {
                        RailKnowledge::Known(range) => range,
                        knowledge => {
                            if abstained_nets.insert(net_id) {
                                report.findings.push(LintFinding {
                                    check: LintCheck::OperatingEnvelope,
                                    severity: Severity::Low,
                                    message: unknown_abstention(
                                        &net.name,
                                        matches!(knowledge, RailKnowledge::BatteryWithoutChemistry),
                                    ),
                                    refs: vec![comp.reference.clone()],
                                    nets: vec![net.name.clone()],
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
                    let mut severity = finding_severity(envelope.severity());
                    if abs_max_v.is_some_and(|limit| range.max_v > limit) {
                        severity = raised_one_band(severity);
                    }
                    let fix = compatible_supply_rail(&resolver, net_id, *min_v, *max_v)
                        .map(|candidate| format!(" Tie {pin} to compatible rail '{candidate}'."))
                        .unwrap_or_default();
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
                    let (Some(lower_id), Some(upper_id)) = (
                        role_net_id(comp, &model, lower),
                        role_net_id(comp, &model, upper),
                    ) else {
                        continue;
                    };
                    let Some(lower_net) = board.net(lower_id) else {
                        continue;
                    };
                    let Some(upper_net) = board.net(upper_id) else {
                        continue;
                    };
                    let mut known = Vec::new();
                    for (net_id, net_name) in
                        [(lower_id, &lower_net.name), (upper_id, &upper_net.name)]
                    {
                        match resolver.range(net_id) {
                            RailKnowledge::Known(range) => known.push(range),
                            knowledge => {
                                if abstained_nets.insert(net_id) {
                                    report.findings.push(LintFinding {
                                        check: LintCheck::OperatingEnvelope,
                                        severity: Severity::Low,
                                        message: unknown_abstention(
                                            net_name,
                                            matches!(
                                                knowledge,
                                                RailKnowledge::BatteryWithoutChemistry
                                            ),
                                        ),
                                        refs: vec![comp.reference.clone()],
                                        nets: vec![net_name.clone()],
                                    });
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
    report
}

/// The same limits as run evidence, judged on the levels the run SOLVED.
///
/// A serious limit undermines whatever the part drives; a lesser one qualifies
/// it. A rail the run did not solve is an abstention, never a violation. Each
/// level is the span the net covered over the run, so a rail that sags out of
/// its envelope and recovers is judged on the sag, not on where it ended.
#[must_use]
pub fn run_assumptions(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    solved: &BTreeMap<String, RailRange>,
    reltol: f64,
    vntol: f64,
) -> Vec<hauksbee_ir::evidence::Assumption> {
    let found = envelope_lint_with_tolerance(board, lib, solved, reltol, vntol).findings;
    let judged = found
        .into_iter()
        .filter(|f| f.nets.iter().all(|net| solved.contains_key(net)))
        .map(|f| {
            let serious = f.severity == Severity::High;
            (f, serious)
        });
    let fix = "bring the rail inside the envelope the card quotes, then re-run";
    super::analog_inputs::as_evidence(judged.collect(), "supply-envelope", fix)
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
            let findings = run_assumptions(&b, &lib, &solved, 1.0e-3, 1.0e-6);
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
            ("INA181A3", "5", 5.8, Medium),
            ("INA181A3", "5", 7.0, High),
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
}
