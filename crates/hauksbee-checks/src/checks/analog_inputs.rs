//! Analog input-range checks.
//!
//! They cover a comparator's input common-mode ceiling, which
//! TRACKS THE SUPPLY (`[[models.envelope]] kind = "input_common_mode"`, since
//! the `supply_range` kind is absolute volts and cannot express `V+ - 1.5 V`),
//! and the source impedance an MCU ADC pin is fed from
//! (`max_adc_source_impedance_ohms`, against the parallel resistance of the
//! resistors on the pin's net).
//!
//! Both are deliberately under-reporting. The LM339/LM393 datasheets guarantee
//! the output state while at least ONE input stays inside the range, so only a
//! whole channel above the ceiling is a violation; and the parallel resistance
//! is a LOWER bound on the net's true Thevenin resistance (it takes every far
//! end as stiff), with any part on the net that is not a resistor or a small
//! capacitor abstaining outright.
//!
//! They are not the same GRADE of evidence, which is `Flag`'s `bool`. With
//! every input of a channel above the ceiling the datasheet guarantees no
//! output state at all, so that is a MEASURED operating-limit violation and an
//! assertion on that output cannot be a pass (`Undermined`, exit 3). The ADC
//! flag says only that the conversion is unreliable; the net's DC voltage is
//! still genuinely what the tool reports, so it merely qualifies.
use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, Net, NetLintReport, Severity,
};
use hauksbee_ir::evidence::Assumption;
use hauksbee_models::OperatingEnvelope::InputCommonMode;
use hauksbee_models::{ModelEntry, ModelLibrary, OperatingEnvelope};
use std::collections::{BTreeSet, HashSet};

/// A capacitor at the pin this size or larger supplies the sample-and-hold
/// charge locally, so the series resistance no longer sets settling: ~700x the
/// `ATmega328P`'s 14 pF S/H capacitor, and the remedy the datasheets name.
const ADC_HOLD_CAP_F: f64 = 10.0e-9;

/// One finding, and whether the run MEASURED a datasheet limit violated.
pub(super) type Flag = (LintFinding, bool);

/// The board a part sits on, and the caller's net-voltage oracle.
///
/// `v` is the run's solved levels on the CI path, the static rail table from
/// [`super::engine_lint`]. A net it cannot answer for abstains, so an undriven
/// divider tap is never guessed at.
struct Ctx<'a> {
    board: &'a ExtractedBoard,
    v: &'a dyn Fn(&str) -> Option<f64>,
}

impl Ctx<'_> {
    /// The net on `c`'s pad carrying `role`.
    fn net(&self, c: &Component, m: &ModelEntry, role: &str) -> Option<&Net> {
        self.board.net(super::envelope::role_net_id(c, m, role)?)
    }

    /// That net's name and solved voltage, where the oracle answers for it.
    fn level(&self, c: &Component, m: &ModelEntry, role: &str) -> Option<(String, f64)> {
        let net = self.net(c, m, role)?;
        Some((net.name.clone(), (self.v)(&net.name)?))
    }
}

/// An assembled capacitor's value in farads.
fn capacitor_farads(comp: &Component) -> Option<f64> {
    (hauksbee_extract::assembly::AssemblyState::of(comp).is_present()
        && comp.reference.to_ascii_uppercase().starts_with('C'))
    .then(|| hauksbee_models::value::parse_value(&comp.value).map(|p| p.si))
    .flatten()
    .filter(|f| f.is_finite() && *f > 0.0)
}

/// One envelope finding against `c`.
fn finding(c: &Component, severity: Severity, nets: Vec<String>, message: String) -> LintFinding {
    LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity,
        message,
        refs: vec![c.reference.clone()],
        nets,
    }
}

/// The supply role, input roles, headroom and basis of `e`, where it is an
/// input common-mode envelope.
fn common_mode(e: &OperatingEnvelope) -> Option<(&str, &[String], f64, &str)> {
    match e {
        InputCommonMode {
            supply,
            inputs,
            headroom_v,
            basis,
            ..
        } => Some((supply, inputs, *headroom_v, basis)),
        _ => None,
    }
}

/// The channel's flag, where every one of its inputs sits above the card's
/// supply-relative ceiling.
fn channel_flag(cx: &Ctx, c: &Component, m: &ModelEntry, e: &OperatingEnvelope) -> Option<Flag> {
    let (r, (supply, inputs, headroom_v, basis)) = (c.reference.as_str(), common_mode(e)?);
    let (rail, sup) = cx.level(c, m, supply)?;
    let limit = sup - headroom_v;
    // `Option<Vec<_>>`, so a channel with one input the oracle cannot answer
    // for abstains whole rather than being judged on the rest.
    let levels = inputs.iter().map(|role| cx.level(c, m, role));
    let seen: Vec<(String, f64)> = levels.collect::<Option<_>>()?;
    seen.iter().all(|(_, x)| *x > limit).then_some(())?;
    let show = |(n, x): &(String, f64)| format!("'{n}' {x:.3} V");
    let shown: Vec<String> = seen.iter().map(show).collect();
    let nets = seen.iter().map(|(n, _)| n.clone()).collect();
    let message = format!("{r} has every input of one channel above its common-mode range: {}, all above V('{rail}') {sup:.3} V - {headroom_v:.3} V = {limit:.3} V, so the output state it reports is not guaranteed; basis: \"{basis}\". Keep at least one input below that ceiling (divide the signal down, or move the pair to a lower rail).", shown.join(" and "));
    let severity = super::envelope::finding_severity(e.severity());
    Some((finding(c, severity, nets, message), true))
}

/// The ADC pin's flag, where the parallel resistance of the resistors feeding
/// its net is above the `limit` ohms the card declares.
fn adc_flag(cx: &Ctx, c: &Component, m: &ModelEntry, role: &str, limit: f64) -> Option<Flag> {
    let (r, net) = (c.reference.as_str(), cx.net(c, m, role)?);
    let (mut inv_r, mut feeders, mut abstain) = (0.0f64, Vec::new(), false);
    let members = cx.board.net_members(net.id);
    for (member, _) in members.iter().filter(|(m, _)| m.reference != c.reference) {
        if let Some(ohms) = super::bus_loading::resistor_ohms(member) {
            inv_r += 1.0 / ohms;
            feeders.push(member.reference.clone());
        } else {
            abstain |= capacitor_farads(member).is_none_or(|f| f >= ADC_HOLD_CAP_F);
        }
    }
    (!abstain && inv_r > 0.0 && 1.0 / inv_r > limit).then_some(())?;
    let message = format!("{r} pin role {role} (net '{}') is fed from {:.1} kOhm ({}), above the {:.1} kOhm maximum ADC source impedance its card declares, so the sample-and-hold cannot settle and the conversion is unreliable once channels are multiplexed or sampled quickly. Lower the divider, buffer it, or fit a >= {:.0} nF capacitor at the pin. The pin is taken as an ADC input from the card's pin role and the purely resistive net feeding it: the board does not record which pins firmware converts.", net.name, 1e-3 / inv_r, feeders.join(", "), limit / 1e3, ADC_HOLD_CAP_F * 1e9);
    let nets = vec![net.name.clone()];
    Some((finding(c, Severity::Medium, nets, message), false))
}

/// Every ADC pin role the card names, judged against its impedance ceiling.
fn adc_flags(cx: &Ctx, c: &Component, m: &ModelEntry) -> Vec<Flag> {
    let limit = m.params.get_f64("max_adc_source_impedance_ohms");
    let Some(limit) = limit else { return vec![] };
    let is_adc = |p: &&String| p.split('_').any(|t| t.starts_with("adc"));
    let flag = |role: &String| adc_flag(cx, c, m, role, limit);
    m.pins.values().filter(is_adc).filter_map(flag).collect()
}

/// Comparator channels with every input above the card's supply-relative
/// ceiling, plus ADC pins fed from too high a source impedance.
fn flags(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    v: &dyn Fn(&str) -> Option<f64>,
    ambient_c: Option<f64>,
) -> Vec<Flag> {
    let (cx, mut out) = (Ctx { board, v }, Vec::new());
    for (c, m) in super::resolved_components(board, lib) {
        let channel = |e| channel_flag(&cx, c, &m, e);
        out.extend(super::envelope::binding(&m.envelope, ambient_c).filter_map(channel));
        out.extend(adc_flags(&cx, c, &m));
    }
    out
}

/// The lint surface of `flags`, for every static caller.
pub fn analog_input_lint(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    volts: &dyn Fn(&str) -> Option<f64>,
) -> NetLintReport {
    let findings = flags(board, lib, volts, None).into_iter().map(|(f, _)| f);
    NetLintReport {
        findings: findings.collect(),
    }
}

/// The same flags as run evidence, for the CI report: the path that keeps a
/// numeric pass from reading as clean (the `min_current_a` floor's discipline).
///
/// The subject carries the nets, so two channels of one quad are two gaps and
/// not one duplicate id the registry rejects; an identical (part, nets) pair IS
/// one gap, and collapses.
///
/// `ambient` is the run's ambient (°C) and whether the scenario stated it.
pub fn run_assumptions(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    volts: &dyn Fn(&str) -> Option<f64>,
    ambient: (f64, bool),
) -> Vec<Assumption> {
    let fix = "bring the input inside the range the card quotes, then re-run";
    let judge = |at| flags(board, lib, volts, at);
    at_ambient(ambient, judge, "analog-input-range", fix)
}

/// `judge`'s flags at the run's ambient, as evidence. An ambient the scenario
/// never stated is disclosed, as the stress monitor discloses it, on each part
/// a row the card prints for any other temperature would flag where the rows
/// judged at the default do not: every printed row is a superset of any single
/// probe, so a tighter hot or cold row is never silently passed over.
pub(super) fn at_ambient(
    (ambient_c, given): (f64, bool),
    judge: impl Fn(Option<f64>) -> Vec<Flag>,
    limit: &str,
    fix: &str,
) -> Vec<Assumption> {
    let now = judge(Some(ambient_c));
    let key = |(f, bad): &Flag| (f.refs.clone(), f.nets.clone(), *bad);
    let seen: HashSet<_> = now.iter().map(key).collect();
    let everywhere = if given { Vec::new() } else { judge(None) };
    let flipped = everywhere.iter().filter(|f| !seen.contains(&key(f)));
    let refs: BTreeSet<&String> = flipped.filter_map(|(f, _)| f.refs.first()).collect();
    let at = format!("{ambient_c} C");
    let default = |r: &String| Assumption::default_parameter_for_component(r, r, "ambient_c", &at);
    let mut out = as_evidence(now, limit, fix);
    out.extend(refs.into_iter().map(default));
    out
}

/// Findings as run evidence: `true` marks a limit the run MEASURED as violated
/// (it undermines), `false` one that only qualifies the answer. Of findings
/// sharing an id the violation is kept, so a lesser row never masks it.
pub(super) fn as_evidence(mut found: Vec<Flag>, limit: &str, fix: &str) -> Vec<Assumption> {
    found.sort_by_key(|(_, bad)| !bad);
    let mut seen = HashSet::new();
    found
        .into_iter()
        .filter_map(|(f, bad)| {
            let r = f.refs.first().map(String::as_str).unwrap_or_default();
            let id = format!("{r}-{limit}-{}", f.nets.join("-"));
            let new = seen.insert(id.clone());
            new.then(|| {
                if bad {
                    Assumption::operating_limit_violated(&id, r, &f.message, fix)
                } else {
                    Assumption::partial_model_for_component(&id, r, "", &f.message, fix)
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;
    use hauksbee_ir::evidence::AssumptionKind as Kind;
    use hauksbee_ir::evidence::AssumptionKind::{OperatingLimitViolated, ReducedFidelity};
    use std::collections::HashMap;

    /// (label, findings expected, `|`-separated substrings the message names).
    type Row<'a> = (&'a str, usize, &'a str);

    /// (label, V(INP), V(INM), findings expected) on a 5 V rail.
    const CHANNELS: &[(&str, f64, f64, usize)] = &[
        ("both taps high", 4.306, 4.000, 1),
        ("one tap inside", 4.306, 2.500, 0),
        ("both taps inside", 0.694, 2.500, 0),
    ];

    /// What the one expected common-mode message must name.
    const CHANNEL_SAYS: &str = "U1|'INP' 4.306 V|'INM' 4.000 V|3.500 V|SLCS006Z";

    /// (label, divider resistor value, capacitor at the pin, findings expected).
    const ADC_FEEDS: &[(&str, &str, Option<&str>, usize)] = &[
        ("1M/1M", "1M", None, 1),
        ("4k7/4k7", "4.7k", None, 0),
        ("1M/1M and a 100 nF hold cap", "1M", Some("100nF"), 0),
    ];

    /// What the one expected ADC source-impedance message must name.
    const ADC_SAYS: &str = "U1|pc0_adc0|'ADC0'|500.0 kOhm|10.0 kOhm|R1, R2";

    /// One table row, on both surfaces: the lint report and the run evidence
    /// must agree on the count, and the message must name what the row says.
    fn check(b: &ExtractedBoard, v: &dyn Fn(&str) -> Option<f64>, row: Row, kind: Kind) {
        let (label, n, says) = row;
        let lib = ModelLibrary::builtin();
        let report = analog_input_lint(b, &lib, v);
        assert_eq!(report.findings.len(), n, "{label}: {report:?}");
        let assumptions = run_assumptions(b, &lib, v, (25.0, true));
        let kinds: Vec<_> = assumptions
            .iter()
            .map(hauksbee_ir::evidence::Assumption::kind)
            .collect();
        assert_eq!(kinds, vec![kind; n], "{label}");
        let m = report.findings.first().map(|f| f.message.clone());
        let m = m.unwrap_or_default();
        let named = says.split('|').all(|s| m.contains(s));
        assert!(n == 0 || named, "{label}: {m}");
    }

    // LM339B on 5 V: VICR ceiling is 5.000 - 1.5 = 3.500 V. One input inside
    // the range is datasheet-legal, so only the pair fires. The datasheet then
    // guarantees NO output state, so the assumption must undermine (exit 3).
    #[test]
    fn lm339_fires_only_when_both_inputs_of_a_channel_are_above_the_ceiling() {
        let nets = &[(1, "GND"), (2, "V5"), (3, "INP"), (4, "INM"), (5, "OUT")];
        let pads = &[("2", 5), ("3", 2), ("4", 4), ("5", 3), ("12", 1)];
        for (label, inp, inm, n) in CHANNELS {
            let b = board(nets, vec![part("U1", "LM339B", pads)]);
            let mut volts = HashMap::from([("V5", 5.0), ("OUT", 5.0), ("GND", 0.0)]);
            volts.extend([("INP", *inp), ("INM", *inm)]);
            let oracle = |net: &str| volts.get(net).copied();
            check(
                &b,
                &oracle,
                (label, *n, CHANNEL_SAYS),
                OperatingLimitViolated,
            );
        }
    }

    // Two rows of one part on one net share an id: a lesser row printed first
    // must not mask the other's violation as a mere qualification.
    #[test]
    fn a_violation_outlives_a_lesser_finding_with_its_id() {
        let c = part("U1", "X", &[]);
        let v = finding(&c, Severity::Low, vec!["VDD".into()], String::new());
        let got = as_evidence(vec![(v.clone(), false), (v, true)], "supply-envelope", "");
        let kinds: Vec<_> = got.iter().map(Assumption::kind).collect();
        assert_eq!(kinds, [OperatingLimitViolated]);
    }

    // The net's DC voltage is genuinely what the tool says here, so this one
    // QUALIFIES a result rather than undermining it.
    #[test]
    fn atmega328p_adc_pin_fires_above_ten_kilohms_unless_a_hold_cap_is_fitted() {
        for (label, ohms, hold_cap, n) in ADC_FEEDS {
            let mut parts = vec![part("U1", "ATmega328P", &[("7", 4), ("8", 1), ("23", 3)])];
            parts.push(part("R1", ohms, &[("1", 2), ("2", 3)]));
            parts.push(part("R2", ohms, &[("1", 3), ("2", 1)]));
            parts.extend(hold_cap.map(|value| part("C1", value, &[("1", 3), ("2", 1)])));
            let b = board(&[(1, "GND"), (2, "VBATT"), (3, "ADC0"), (4, "V5")], parts);
            check(&b, &|_| None, (label, *n, ADC_SAYS), ReducedFidelity);
            // The chokepoint every surface shares must carry it too.
            let lint = super::super::engine_lint(&b, &ModelLibrary::builtin());
            let adc = |f: &&LintFinding| f.message.contains("ADC source impedance");
            assert_eq!(lint.findings.iter().filter(adc).count(), *n, "{label}");
        }
    }
}
