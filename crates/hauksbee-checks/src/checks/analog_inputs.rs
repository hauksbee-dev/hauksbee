//! Analog input-range checks.
//!
//! They cover a comparator's input common-mode ceiling, which
//! TRACKS THE SUPPLY (`[[models.envelope]] kind = "input_common_mode"`, since
//! the `supply_range` kind is absolute volts and cannot express `V+ - 1.5 V`),
//! and the source impedance an MCU ADC pin is fed from
//! (`max_adc_source_impedance_ohms`, against the parallel resistance of the
//! resistors on the pin's net).
//!
//! A third rule reads the MCU card's structured pin capability
//! (`[models.pin_capability.adc]`): an analogue source wired to a pad the
//! card does not list as an ADC input is `adc_capability_flags`'s finding.
//!
//! The first two are deliberately under-reporting. The LM339/LM393 datasheets guarantee
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
    Component, ExtractedBoard, LintCheck, LintFinding, Net, NetLintReport, Severity, REMEDY_MARKER,
};
use hauksbee_ir::evidence::Assumption;
use hauksbee_ir::units::format_volts;
use hauksbee_models::rating_basis::Limit;
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
    let show = |(n, x): &(String, f64)| format!("'{n}' {}", format_volts(x));
    let shown: Vec<String> = seen.iter().map(show).collect();
    let nets = seen.iter().map(|(n, _)| n.clone()).collect();
    let message = format!( "{r} has every input of one channel above its common-mode range: {}, all above V('{rail}') {} - {} = {}, so the output state it reports is not guaranteed; basis: \"{basis}\". Keep at least one input below that ceiling (divide the signal down, or move the pair to a lower rail).", shown.join(" and "), format_volts(sup), format_volts(headroom_v), format_volts(limit));
    let severity = super::envelope::finding_severity(e.severity());
    let seen = format!(
        "{r} has every input of one channel above V('{rail}') - {} = {}",
        format_volts(headroom_v),
        format_volts(limit)
    );
    let rating = format!("{} common-mode headroom", format_volts(headroom_v));
    let flag = finding(c, severity, nets, message);
    let flag = super::envelope::stated_if_stand_in(flag, m, Limit::Envelope, &seen, &rating);
    Some((flag, true))
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
    let message = format!("{r} pin role {role} (net '{}') is fed from {:.1} kOhm ({}), above the {:.1} kOhm maximum ADC source impedance its card declares, so the sample-and-hold cannot settle and the conversion is unreliable once channels are multiplexed or sampled quickly. The pin is taken as an ADC input from the card's pin role and the purely resistive net feeding it: the board does not record which pins firmware converts. {REMEDY_MARKER}Lower the divider, buffer it, or fit a >= {:.0} nF capacitor at the pin.", net.name, 1e-3 / inv_r, feeders.join(", "), limit / 1e3, ADC_HOLD_CAP_F * 1e9);
    let nets = vec![net.name.clone()];
    let seen = format!("{r} pin role {role} is fed from {:.1} kOhm", 1e-3 / inv_r);
    let rating = format!("{:.1} kOhm ADC source-impedance", limit / 1e3);
    let flag = finding(c, Severity::Medium, nets, message);
    let flag = super::envelope::stated_if_stand_in(flag, m, Limit::Envelope, &seen, &rating);
    Some((flag, false))
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
    st: &super::BoardStatics<'_>,
    v: &dyn Fn(&str) -> Option<f64>,
    ambient_c: Option<f64>,
) -> Vec<Flag> {
    let (cx, mut out) = (Ctx { board, v }, Vec::new());
    for (c, m) in st.parts(board) {
        let channel = |e| channel_flag(&cx, c, m, e);
        out.extend(super::envelope::binding(&m.envelope, ambient_c).filter_map(channel));
        out.extend(adc_flags(&cx, c, m));
    }
    out
}

/// The lint surface of `flags`, for every static caller.
pub fn analog_input_lint(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    volts: &dyn Fn(&str) -> Option<f64>,
) -> NetLintReport {
    let st = super::BoardStatics::of(board, lib);
    analog_input_lint_in(board, &st, volts)
}

/// [`analog_input_lint`] over statics the caller already holds.
pub fn analog_input_lint_in(
    board: &ExtractedBoard,
    st: &super::BoardStatics<'_>,
    volts: &dyn Fn(&str) -> Option<f64>,
) -> NetLintReport {
    let findings = flags(board, st, volts, None).into_iter().map(|(f, _)| f);
    let mut findings: Vec<LintFinding> = findings.collect();
    findings.extend(adc_capability_flags(board, st, volts));
    NetLintReport { findings }
}

// ── An analogue source on a pad that is not an ADC input ─────────────────────

/// Pad roles that can be a general-purpose pin whose analogue function is only
/// one of several: `pa3`, `gpio4`, `io13`, `p0_12`, `d5`, `a0`, with the
/// alternate functions the card appends after the port name.
fn is_gpio_role(role: &str) -> bool {
    let r = role.trim().to_ascii_lowercase();
    let head = r.split('_').next().unwrap_or("");
    let numbered = |prefix: &str| {
        head.strip_prefix(prefix)
            .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
    };
    let port_pin = head.len() >= 3
        && head.starts_with('p')
        && head.as_bytes()[1].is_ascii_lowercase()
        && head[2..].chars().all(|c| c.is_ascii_digit());
    port_pin
        || ["gpio", "io", "gp", "d", "a", "p"]
            .iter()
            .any(|p| numbered(p))
}

/// Alternate functions that are themselves analogue or analogue-adjacent: a
/// pad that carries one is not held to the ADC list (a comparator input, a
/// reference, a DAC or op-amp pin, a touch or capacitive-sense channel).
fn has_analog_function(role: &str) -> bool {
    role.to_ascii_lowercase().split('_').any(|t| {
        [
            "ain", "adc", "comp", "cmp", "dac", "aref", "vref", "opamp", "opa", "touch", "tsc",
            "cap", "lcd", "sense",
        ]
        .iter()
        .any(|a| t.starts_with(a))
    })
}

/// How the net feeding a pad is an analogue source.
struct Source {
    /// The part(s) that make it one, for the message.
    what: String,
    refs: Vec<String>,
}

/// A potentiometer or trimmer, by designator or footprint.
fn is_pot(comp: &Component) -> bool {
    let prefix: String = comp
        .reference
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .collect::<String>()
        .to_ascii_uppercase();
    let fp = format!("{} {}", comp.footprint, comp.lib_id).to_ascii_lowercase();
    matches!(prefix.as_str(), "RV" | "VR" | "POT" | "TRIM")
        || ["potentiometer", "trimmer", "trimpot"]
            .iter()
            .any(|w| fp.contains(w))
}

/// Whether `pin` is the wiper of the three-pad potentiometer `comp`.
fn is_wiper(comp: &Component, pin: &hauksbee_extract::Pin) -> bool {
    let f = pin.function.to_ascii_lowercase();
    f == "w" || f == "wiper" || (f.is_empty() && comp.pins.len() == 3 && pin.number == "2")
}

/// The pin roles of an analogue sensor's output, by card: a part whose card
/// says its pad is an analogue output (a temperature sensor's `aout`).
fn is_analog_out_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "aout" | "analog_out" | "analogout" | "ao" | "vtemp" | "temp_out" | "sensor_out" | "viout"
    )
}

/// The analogue output a single pin of `c` is: one its symbol names (`AOUT`,
/// `ANALOG_OUT`, `VTEMP`; a pin typed an input is the other end of such a line)
/// or its card names for the pad.
fn pin_source(
    st: &super::BoardStatics<'_>,
    board: &ExtractedBoard,
    c: &Component,
    p: &hauksbee_extract::Pin,
) -> Option<Source> {
    if hauksbee_ir::pin_functions::names_an_analog_output(&p.function)
        && !matches!(
            p.kind.trim().to_ascii_lowercase().as_str(),
            "input" | "power_in"
        )
    {
        return Some(Source {
            what: format!(
                "the analogue output '{}' of {} pin {}",
                p.function.trim(),
                c.reference,
                p.number
            ),
            refs: vec![c.reference.clone()],
        });
    }
    let (kind, role) = st
        .parts(board)
        .find(|(pc, _)| pc.reference == c.reference)
        .and_then(|(_, m)| m.pins.get(&p.number).map(|r| (m.kind, r.clone())))?;
    let source_kind = matches!(
        kind,
        hauksbee_models::ComponentKind::Digital | hauksbee_models::ComponentKind::Adc
    );
    (source_kind && is_analog_out_role(&role)).then(|| Source {
        what: format!("the analogue output '{role}' of {}", c.reference),
        refs: vec![c.reference.clone()],
    })
}

/// The analogue source driving `net_id`, other than the pads of `skip`.
///
/// Three shapes: a potentiometer wiper; a part whose card names the pad an
/// analogue output; and a resistor divider between a supply and ground (an
/// NTC thermistor divider, a battery sense), where the net holds nothing but
/// the resistors, capacitors and the reading pad. A series resistor (the
/// anti-alias filter in front of an ADC) is followed once.
fn source_of(
    cx: &Ctx,
    st: &super::BoardStatics<'_>,
    net_id: i64,
    skip: &str,
    hop: bool,
    mcu_vdd: Option<f64>,
) -> Option<Source> {
    let board = cx.board;
    let members = board.net_members(net_id);
    let others: Vec<_> = members
        .iter()
        .filter(|(c, _)| c.reference != skip)
        .collect();
    for (c, p) in &others {
        if is_pot(c) && is_wiper(c, p) {
            return Some(Source {
                what: format!("the wiper of potentiometer {}", c.reference),
                refs: vec![c.reference.clone()],
            });
        }
        if let Some(source) = pin_source(st, board, c, p) {
            return Some(source);
        }
    }
    // A divider: resistors to a positive supply and to ground, nothing active.
    let (mut tops, mut bots, mut series) = (Vec::new(), Vec::new(), Vec::new());
    for (c, p) in &others {
        if hauksbee_extract::is_capacitor(c) || hauksbee_extract::is_test_point_or_hole(c) {
            continue;
        }
        let ohms = super::bus_loading::resistor_ohms(c)?;
        let far = c.pins.iter().find(|q| q.number != p.number)?.net?;
        let far_name = &board.net(far)?.name;
        if super::is_ground_name(far_name) {
            bots.push((c.reference.clone(), ohms));
        } else if hauksbee_models::netclass::is_positive_supply(far_name) {
            tops.push((c.reference.clone(), ohms, far_name.clone()));
        } else {
            series.push((c, far));
        }
    }
    if tops.is_empty() || bots.is_empty() {
        // One series resistor in front of the pad: look at its far side.
        let [(rs, far)] = series.as_slice() else {
            return None;
        };
        let alone = tops.is_empty() && bots.is_empty();
        if !hop || !alone {
            return None;
        }
        return source_of(cx, st, *far, &rs.reference, false, mcu_vdd);
    }
    if !series.is_empty() {
        return None;
    }
    let par = |v: Vec<f64>| 1.0 / v.iter().map(|o| 1.0 / o).sum::<f64>();
    let (r_top, r_bot) = (
        par(tops.iter().map(|t| t.1).collect()),
        par(bots.iter().map(|b| b.1).collect()),
    );
    let ratio = r_bot / (r_top + r_bot);
    let rail = tops[0].2.clone();
    let rail_v = (cx.v)(&rail).or_else(|| hauksbee_bind::binder::power_rail_voltage(&rail));
    // A tap that sits at a clean logic level is a digital read of a divided
    // rail (a USB-VBUS-present sense), not an analogue measurement. A USB
    // VBUS rail is a presence sense whatever the divider; a rail or MCU
    // supply of unknown level falls back to the ratio alone.
    let logic_level = |tap: f64, vdd: f64| tap >= 0.7 * vdd || tap <= 0.3 * vdd;
    let digital = rail.to_ascii_uppercase().contains("VBUS")
        || match (rail_v, mcu_vdd) {
            (Some(rv), Some(vdd)) => logic_level(rv * ratio, vdd),
            _ => !(0.15..=0.85).contains(&ratio),
        };
    if digital {
        return None;
    }
    let mut refs: Vec<String> = tops.iter().map(|t| t.0.clone()).collect();
    refs.extend(bots.iter().map(|b| b.0.clone()));
    Some(Source {
        what: format!(
            "the tap of a resistor divider ({} from '{rail}', {} to ground)",
            refs[..tops.len()].join("||"),
            refs[tops.len()..].join("||"),
        ),
        refs,
    })
}

/// The supply voltage of `c`, from the net on its first supply pad.
fn supply_of(cx: &Ctx, c: &Component, m: &ModelEntry) -> Option<f64> {
    m.pins.iter().find_map(|(pad, role)| {
        let power = hauksbee_models::pin_roles::power_pin(role, m.kind)?;
        (power.kind == hauksbee_models::pin_roles::PowerKind::Supply && power.base).then_some(())?;
        let pin = c.pins.iter().find(|p| &p.number == pad)?;
        let name = &cx.board.net(pin.net?)?.name;
        (cx.v)(name).or_else(|| hauksbee_bind::binder::power_rail_voltage(name))
    })
}

/// The ADC-capable pads of `m` that nothing on `c` uses, as a message lists them.
fn adc_alternatives(c: &Component, m: &ModelEntry, board: &ExtractedBoard) -> String {
    let Some(adc) = &m.pin_capability.adc else {
        return String::new();
    };
    let used = |pad: &str| {
        c.pins
            .iter()
            .filter(|p| p.number == pad)
            .filter_map(|p| p.net)
            .filter(|&id| id != 0)
            .filter_map(|id| board.net(id))
            .any(|n| !super::is_unconnected_net(&n.name))
    };
    let list = |free_only: bool| -> Vec<String> {
        adc.pads
            .iter()
            .filter(|(pad, _)| !free_only || !used(pad))
            .map(|(pad, ch)| {
                let role = m.pins.get(pad).map_or("", String::as_str);
                format!("{} (pad {pad}, {ch})", role.to_ascii_uppercase())
            })
            .collect()
    };
    let (free, all) = (list(true), list(false));
    if all.is_empty() {
        return String::new();
    }
    let (shown, which) = if free.is_empty() {
        (all, "")
    } else {
        (free, "unused ")
    };
    let more = shown.len().saturating_sub(8);
    let text = shown.into_iter().take(8).collect::<Vec<_>>().join(", ");
    let tail = if more > 0 {
        format!(" and {more} more")
    } else {
        String::new()
    };
    format!("The card's {which}ADC-capable pads are {text}{tail}.")
}

/// An analogue source wired to an MCU pad that its card does not list as an ADC
/// input (`[models.pin_capability.adc]`): the reading is a digital level and
/// firmware can never convert it.
fn adc_capability_flags(
    board: &ExtractedBoard,
    st: &super::BoardStatics<'_>,
    v: &dyn Fn(&str) -> Option<f64>,
) -> Vec<LintFinding> {
    let cx = Ctx { board, v };
    let mut out = Vec::new();
    for (c, m) in st.parts(board) {
        if m.kind != hauksbee_models::ComponentKind::Mcu || !m.adc_declared() {
            continue;
        }
        let vdd = supply_of(&cx, c, m);
        for pin in &c.pins {
            let (Some(net_id), Some(role)) = (pin.net.filter(|&n| n != 0), m.pins.get(&pin.number))
            else {
                continue;
            };
            if m.adc_channel(&pin.number).is_some()
                || !is_gpio_role(role)
                || has_analog_function(role)
            {
                continue;
            }
            let Some(net) = board.net(net_id) else {
                continue;
            };
            // A net that also reaches an ADC pad (of this or another MCU) is read.
            let reads = board.net_members(net_id).iter().any(|(oc, op)| {
                st.parts(board).any(|(pc, pm)| {
                    pc.reference == oc.reference && pm.adc_channel(&op.number).is_some()
                })
            });
            if reads || super::is_unconnected_net(&net.name) {
                continue;
            }
            let Some(src) = source_of(&cx, st, net_id, &c.reference, true, vdd) else {
                continue;
            };
            let mut refs = vec![c.reference.clone()];
            refs.extend(src.refs);
            out.push(LintFinding {
                check: LintCheck::OperatingEnvelope,
                severity: Severity::Medium,
                message: format!(
                    "{} pad {} ({}, net '{}') reads {}, but its card lists no ADC channel on that \
                     pad, so firmware can only read it as a digital level and never convert it. {} \
                     {REMEDY_MARKER}Move the signal to an ADC-capable pad of {}.",
                    c.reference,
                    pin.number,
                    role.to_ascii_uppercase(),
                    net.name,
                    src.what,
                    adc_alternatives(c, m, board),
                    c.reference,
                ),
                refs,
                nets: vec![net.name.clone()],
            });
        }
    }
    out
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
    st: &super::BoardStatics<'_>,
    volts: &dyn Fn(&str) -> Option<f64>,
    ambient: (f64, bool),
) -> Vec<Assumption> {
    let fix = "bring the input inside the range the card quotes, then re-run";
    let judge = |at| flags(board, st, volts, at);
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
    use super::super::BoardStatics;
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
    const CHANNEL_SAYS: &str = "U1|'INP' 4.306 V|'INM' 4 V|3.5 V|SLCS006Z";

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
        let assumptions = run_assumptions(b, &BoardStatics::of(b, &lib), v, (25.0, true));
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
            // The finding names its own fix, not the envelope kind's generic one.
            for f in lint.findings.iter().filter(adc) {
                let remedy = f.own_remedy().unwrap_or_default();
                assert!(
                    remedy.starts_with("Lower the divider"),
                    "{label}: {}",
                    f.message
                );
            }
        }
    }
    /// (label, pad the divider tap or wiper feeds, parts on the net besides
    /// the MCU, findings expected). `ATmega328P` DIP: pad 4 is PD2 (no ADC),
    /// pad 23 is PC0 (ADC0).
    const CAPABILITY_ROWS: &[(&str, &str, Source3, usize)] = &[
        (
            "divider tap on a non-ADC pad",
            "4",
            Source3::Divider(10_000),
            1,
        ),
        (
            "divider tap on an ADC pad",
            "23",
            Source3::Divider(10_000),
            0,
        ),
        ("pot wiper on a non-ADC pad", "4", Source3::Wiper, 1),
        ("pot wiper on an ADC pad", "23", Source3::Wiper, 0),
        (
            "one series resistor then a divider",
            "4",
            Source3::SeriesDivider,
            1,
        ),
        (
            "equal 100k/1k tap is a logic level",
            "4",
            Source3::Divider(1_000),
            0,
        ),
    ];

    // A divider from a VBUS rail (a USB-present sense, here 5 V) is not an
    // analogue measurement whatever its ratio: abstain.
    #[test]
    fn a_vbus_sense_or_unknown_rail_divider_is_not_an_analogue_read() {
        let lib = ModelLibrary::builtin();
        for rail in ["VBUSR", "USB_VBUS", "VBUS"] {
            let mut b = capability_board("4", Source3::Divider(10_000), false);
            b.nets[1].name = rail.into();
            let r = analog_input_lint(&b, &lib, &|_| None);
            assert!(
                r.findings
                    .iter()
                    .all(|f| !f.message.contains("lists no ADC")),
                "{rail}: {r:?}"
            );
        }
    }

    #[derive(Clone, Copy)]
    enum Source3 {
        /// Top resistor 10 k from +3V3, bottom resistor of this many ohms to ground.
        Divider(u32),
        Wiper,
        SeriesDivider,
    }

    fn capability_board(pad: &str, src: Source3, alt_adc: bool) -> ExtractedBoard {
        let mut parts = vec![part(
            "U1",
            "ATmega328P",
            &[("7", 2), ("8", 1), ("20", 2), ("22", 1), (pad, 3)],
        )];
        match src {
            Source3::Divider(bottom) => {
                parts.push(part("R1", "10k", &[("1", 2), ("2", 3)]));
                parts.push(part("R2", &format!("{bottom}"), &[("1", 3), ("2", 1)]));
            }
            Source3::Wiper => parts.push(part("RV1", "10k", &[("1", 2), ("2", 3), ("3", 1)])),
            Source3::SeriesDivider => {
                parts[0].pins.retain(|p| p.number != pad);
                parts[0].pins.push(hauksbee_extract::Pin {
                    number: pad.into(),
                    net: Some(4),
                    ..Default::default()
                });
                parts.push(part("R3", "1k", &[("1", 4), ("2", 3)]));
                parts.push(part("R1", "10k", &[("1", 2), ("2", 3)]));
                parts.push(part("R2", "10k", &[("1", 3), ("2", 1)]));
            }
        }
        if alt_adc {
            parts[0].pins.push(hauksbee_extract::Pin {
                number: "24".into(),
                net: Some(3),
                ..Default::default()
            });
        }
        board(&[(1, "GND"), (2, "+3V3"), (3, "SENSE"), (4, "PIN")], parts)
    }

    // A card that lists its ADC pads says every other pad of its map is not
    // one: an analogue source on such a pad can only be read as a digital level.
    #[test]
    fn analogue_source_on_a_pad_with_no_adc_channel_is_flagged() {
        let lib = ModelLibrary::builtin();
        for (label, pad, src, n) in CAPABILITY_ROWS {
            let b = capability_board(pad, *src, false);
            let got: Vec<_> = analog_input_lint(&b, &lib, &|_| None)
                .findings
                .into_iter()
                .filter(|f| f.message.contains("lists no ADC channel"))
                .collect();
            assert_eq!(got.len(), *n, "{label}: {got:?}");
            for f in &got {
                for want in ["U1 pad 4", "PD2_INT0", "ADC-capable pads", "PC0_ADC0"] {
                    assert!(f.message.contains(want), "{label}: {}", f.message);
                }
            }
        }
    }

    // The same net also reaching an ADC pad is read, not misrouted.
    #[test]
    fn a_net_that_also_reaches_an_adc_pad_is_silent() {
        let b = capability_board("4", Source3::Divider(10_000), true);
        let r = analog_input_lint(&b, &ModelLibrary::builtin(), &|_| None);
        assert!(
            r.findings
                .iter()
                .all(|f| !f.message.contains("lists no ADC")),
            "{r:?}"
        );
    }

    /// A sensor module with no card: its symbol names pin 1 `AOUT` (typed an
    /// output) and its supply pins. `mcu` is wired to the AOUT net on `pad`.
    fn sensor_board(mcu: &str, pad: &str, kind: &str) -> ExtractedBoard {
        let mut sensor = part("U2", "TMP-SENSOR", &[("1", 3), ("2", 2), ("3", 1)]);
        sensor.pins[0].function = "AOUT".into();
        sensor.pins[0].kind = kind.into();
        let u1 = part(
            "U1",
            mcu,
            &[("7", 2), ("8", 1), ("20", 2), ("22", 1), (pad, 3)],
        );
        board(&[(1, "GND"), (2, "+3V3"), (3, "TEMP")], vec![u1, sensor])
    }

    fn no_adc(b: &ExtractedBoard) -> Vec<LintFinding> {
        analog_input_lint(b, &ModelLibrary::builtin(), &|_| None)
            .findings
            .into_iter()
            .filter(|f| f.message.contains("lists no ADC"))
            .collect()
    }

    // A sensor's analogue output, named in its symbol, on an MCU pad with no ADC
    // channel: flagged, with the ADC pads named. On pad 23 (PC0_ADC0), silent.
    #[test]
    fn a_symbol_named_analogue_output_on_a_pad_with_no_adc_channel_is_flagged() {
        let got = no_adc(&sensor_board("ATmega328P", "4", "output"));
        assert_eq!(got.len(), 1, "{got:?}");
        for want in [
            "the analogue output 'AOUT' of U2 pin 1",
            "U1 pad 4",
            "ADC-capable pads",
            "PC0_ADC0",
        ] {
            assert!(got[0].message.contains(want), "{want}: {}", got[0].message);
        }
        assert!(no_adc(&sensor_board("ATmega328P", "23", "output")).is_empty());
        // The input end of an AOUT-named line (a buffer fed by it) is not a source.
        assert!(no_adc(&sensor_board("ATmega328P", "4", "input")).is_empty());
    }

    // An MCU card without an ADC table says nothing about its pads.
    #[test]
    fn an_mcu_card_with_no_adc_table_is_not_held_to_one() {
        assert!(no_adc(&sensor_board("STM32F4XX", "4", "output")).is_empty());
    }

    // A pad whose role itself is analogue (the AVR comparator input) is never held to the list.
    #[test]
    fn an_analogue_function_pad_is_not_held_to_the_adc_list() {
        let b = capability_board("12", Source3::Divider(10_000), false);
        let r = analog_input_lint(&b, &ModelLibrary::builtin(), &|_| None);
        assert!(
            r.findings
                .iter()
                .all(|f| !f.message.contains("lists no ADC")),
            "{r:?}"
        );
    }
}
