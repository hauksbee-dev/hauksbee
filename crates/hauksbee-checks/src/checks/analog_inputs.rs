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
//! Two more rules read the front end in front of an MCU's ADC pad (see
//! `zener_clamp_flag` and `negative_rail_flag`): a Zener clamp to ground whose
//! knee leakage bends the top of the range, and a resistor network from a
//! negative rail whose tap, solved through the DC solver at the rail's nominal
//! level and ±10 %, leaves 0..full scale or sits within 5 % of an end. The
//! Zener's knee is ASSUMED, and the message says so: the compiled card library
//! keeps Vz but not the datasheet's IZK/("BZT52C3V3", "ASSUMED|1 mA") rows, so the knee voltage (90 % of
//! Vz), the decade width and the knee current are documented defaults. Reading
//! the rows needs the library to carry them (not done here). The card's `bv`/`ibv`
//! pair is a hard-breakdown model, so the solver itself shows none of this leakage. The
//! full scale is the card's `vref_v`, the level on a wired reference pad, or the
//! MCU's own supply (assumed: the card states no reference level).
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
use hauksbee_ir::units::{format_amps, format_engineering, format_volts};
use hauksbee_models::rating_basis::Limit;
use hauksbee_models::OperatingEnvelope::InputCommonMode;
use hauksbee_models::{ModelEntry, ModelLibrary, OperatingEnvelope};
use std::collections::{BTreeSet, HashMap, HashSet};

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
    let models: HashMap<&str, &ModelEntry> = st
        .parts(board)
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    for (c, m) in st.parts(board) {
        let channel = |e| channel_flag(&cx, c, m, e);
        out.extend(super::envelope::binding(&m.envelope, ambient_c).filter_map(channel));
        out.extend(adc_flags(&cx, c, m));
        out.extend(front_end_flags(&cx, &models, (c, m)));
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

// ── The front end in front of an ADC pad ─────────────────────────────────────
//
// Two faults leave the pad's net voltage inside its absolute ratings and the
// conversion still wrong. A Zener clamp whose breakdown sits at (or just above)
// full scale leaks well before its rated voltage, so a series resistor turns
// the leakage into a reading that bends over the top of the range. A divider
// from a negative rail lands the pad below ground, or pinned at an end of the
// range, so the reading never follows the rail.

/// A Zener this close to full scale (as a multiple of it) or lower leaks inside
/// the range; the brief for the rule is "within about 15 %".
const ZENER_WINDOW: f64 = 1.15;
/// A series resistance under this makes no leakage error worth reporting.
const MIN_CLAMP_SERIES_OHMS: f64 = 100.0;
/// The leakage drop that counts as visible, as a fraction of full scale: one
/// least-significant bit of a 10-bit converter.
const VISIBLE_ERROR: f64 = 1.0e-3;
/// ASSUMED: the knee voltage as a fraction of Vz. The datasheets of the small
/// Zeners quote the knee current (IZK) at a voltage a little under Vz, never
/// the voltage itself.
const KNEE_VOLTAGE_FRACTION: f64 = 0.9;
/// ASSUMED (the library carries no ZZK row): volts per decade of current below
/// the knee, as a fraction of Vz. A soft knee: leakage is significant from
/// about 70 % of Vz.
const KNEE_DECADE_FRACTION: f64 = 0.25;
/// ASSUMED (the library carries no IZK row): the knee current.
const KNEE_CURRENT_A: f64 = 1.0e-3;
/// A divider whose own peak is under this fraction of full scale never takes
/// the signal into the Zener's knee region, whatever the (assumed) knee.
const NEAR_FULL_SCALE: f64 = 0.8;
/// The negative rail's tolerance, either way, the tap is solved across.
const RAIL_TOLERANCE: f64 = 0.10;
/// A tap this close to either end of the range (as a fraction of it) has no
/// headroom for the converter's own error.
const END_MARGIN: f64 = 0.05;

/// The ADC full scale of `c`, and where the figure comes from: the card's own
/// `vref_v`, the level on a wired reference pad, or (the card states none) its
/// supply, which is the default reference of the MCUs this reads.
fn adc_full_scale(cx: &Ctx, c: &Component, m: &ModelEntry) -> Option<(f64, String)> {
    if let Some(v) = m.params.get_f64("vref_v").filter(|v| *v > 0.0) {
        return Some((v, "its card's vref_v".to_string()));
    }
    for (pad, role) in &m.pins {
        let r = role.to_ascii_lowercase();
        if !(r.starts_with("vref") || r.starts_with("aref")) || r.contains("int") {
            continue;
        }
        let Some(net) = (c.pins.iter().find(|p| &p.number == pad))
            .and_then(|p| p.net)
            .filter(|id| *id != 0)
            .and_then(|id| cx.board.net(id))
        else {
            continue;
        };
        let members = cx.board.net_members(net.id);
        let bypass_only = (members.iter())
            .filter(|(mc, _)| mc.reference != c.reference)
            .all(|(mc, _)| hauksbee_extract::is_capacitor(mc));
        if bypass_only || super::is_unconnected_net(&net.name) {
            continue; // a reference pin with only a bypass: the supply is the reference
        }
        let level =
            (cx.v)(&net.name).or_else(|| hauksbee_bind::binder::power_rail_voltage(&net.name));
        return level.map(|v| (v, format!("the level on its {role} pad ('{}')", net.name)));
    }
    let v = supply_of(cx, c, m)?;
    Some((
        v,
        "its own supply, assumed as the ADC reference: the card states no reference level"
            .to_string(),
    ))
}

/// The voltage a value text states ("3V3", "3.3V", "BZX84C3V3" read as 3.3 V).
fn voltage_token(text: &str) -> Option<f64> {
    let t = text.to_ascii_uppercase();
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
            i += 1;
        }
        if b.get(i) != Some(&b'V') {
            continue;
        }
        let head = &t[start..i];
        i += 1;
        let frac = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let text = if frac == i || head.contains('.') {
            i = frac;
            head.to_string()
        } else {
            format!("{head}.{}", &t[frac..i])
        };
        if let Some(v) = text.parse::<f64>().ok().filter(|v| *v > 0.0) {
            return Some(v);
        }
    }
    None
}

/// A Zener from an ADC net to ground, and what the card (or the lack of one)
/// says about how softly it breaks down.
struct Zener {
    reference: String,
    vz: f64,
    /// Where Vz comes from, for the message.
    vz_from: String,
    /// The knee: current at `KNEE_VOLTAGE_FRACTION * vz` and volts per decade.
    knee_a: f64,
    decade_v: f64,
    /// What the knee rests on, for the message.
    knee_from: String,
}

impl Zener {
    /// The leakage current (A) with `v` volts across the Zener, as an
    /// exponential below the knee: one decade per `decade_v`.
    fn current_at(&self, v: f64) -> f64 {
        let knee_v = KNEE_VOLTAGE_FRACTION * self.vz;
        self.knee_a * 10f64.powf((v - knee_v) / self.decade_v)
    }
}

/// The soft knee of a Zener `vz`: all of it ASSUMED. The compiled card library
/// keeps a Zener's Vz (`bv` at `ibv`) but not the datasheet's IZK and ZZK rows
/// (they are dropped when the card loads), so there is no printed knee data to
/// read; the defaults above stand in, and the sentence says so. Returns (knee
/// current, volts per decade, that sentence).
fn zener_knee(vz: f64) -> (f64, f64, String) {
    let text = format!(
        "No knee data is available to the check (the card library keeps Vz but not the \
         datasheet's IZK and ZZK rows), so a soft knee is ASSUMED: {} at {} % of Vz and one \
         decade of current per {} % of Vz",
        format_amps(KNEE_CURRENT_A),
        KNEE_VOLTAGE_FRACTION * 100.0,
        KNEE_DECADE_FRACTION * 100.0,
    );
    (KNEE_CURRENT_A, KNEE_DECADE_FRACTION * vz, text)
}

/// The net `comp`'s pin named one of `names` is on.
fn pin_named(comp: &Component, names: &[&str]) -> Option<i64> {
    comp.pins
        .iter()
        .find(|p| names.contains(&p.function.trim().to_ascii_uppercase().as_str()))
        .and_then(|p| p.net)
}

/// `comp` as a Zener from net `net` to ground, where it is one.
fn zener_clamp(
    cx: &Ctx,
    models: &HashMap<&str, &ModelEntry>,
    comp: &Component,
    net: i64,
) -> Option<Zener> {
    if !hauksbee_extract::assembly::AssemblyState::of(comp).is_present() {
        return None;
    }
    let model = models.get(comp.reference.as_str()).copied();
    let by_card = |role: &str| model.and_then(|m| super::envelope::role_net_id(comp, m, role));
    let cathode = by_card("cathode").or_else(|| pin_named(comp, &["K", "C", "CATHODE"]));
    let anode = by_card("anode").or_else(|| pin_named(comp, &["A", "ANODE"]));
    let to_ground = anode
        .and_then(|id| cx.board.net(id))
        .is_some_and(|n| super::is_ground_name(&n.name));
    if cathode != Some(net) || !to_ground {
        return None;
    }
    let says_zener = |text: &str| text.to_ascii_lowercase().contains("zener");
    let (vz, vz_from) = match model {
        Some(m) if says_zener(&m.description) => {
            let bv = m.params.get_f64("bv").filter(|v| *v > 0.0)?;
            (bv, format!("card '{}', bv", m.id))
        }
        _ if says_zener(&comp.lib_id) || says_zener(&comp.value) => {
            let v = voltage_token(&comp.value)?;
            (
                v,
                "ASSUMED from its value text, the board names no card".to_string(),
            )
        }
        _ => return None,
    };
    let (knee_a, decade_v, knee_from) = zener_knee(vz);
    Some(Zener {
        reference: comp.reference.clone(),
        vz,
        vz_from,
        knee_a,
        decade_v,
        knee_from,
    })
}

/// The ADC pads of `c` that are wired, with the net each is on.
fn wired_adc_pads<'a>(
    cx: &'a Ctx,
    c: &'a Component,
    m: &'a ModelEntry,
) -> impl Iterator<Item = (&'a hauksbee_extract::Pin, &'a Net)> + 'a {
    c.pins.iter().filter_map(move |p| {
        m.adc_channel(&p.number)?;
        let net = cx.board.net(p.net.filter(|id| *id != 0)?)?;
        (!super::is_unconnected_net(&net.name)).then_some((p, net))
    })
}

/// How a clamp at `vz` stands to the converter's full scale `fs`.
fn clamp_relation(vz: f64, fs: f64) -> String {
    if vz <= fs {
        "at or below full scale, so it also clips the top of the range".to_string()
    } else {
        format!("only {:.0} % above full scale", (vz / fs - 1.0) * 100.0)
    }
}

/// The level at the far end of resistor `member` (its pin `near` is on the
/// net under judgement): ground is 0 V, a rail its level, anything else unknown.
fn far_end_level(cx: &Ctx, member: &Component, near: &hauksbee_extract::Pin) -> Option<f64> {
    let far = (member.pins.iter().find(|q| q.number != near.number)).and_then(|q| q.net)?;
    let name = &cx.board.net(far)?.name;
    if super::is_ground_name(name) {
        return Some(0.0);
    }
    (cx.v)(name).or_else(|| hauksbee_bind::binder::power_rail_voltage(name))
}

/// The highest level the signal behind an ADC pad is meant to reach, and
/// whether it is a divider's own open-circuit level. `ends` are the resistors'
/// far ends as (conductance, level there if known). A resistor to ground makes
/// the net a divider: its level comes from the rails it hangs from (10 % high),
/// or is not known (`None`: it is designed to scale, so no claim). With no
/// resistor to ground the source is a series one, and the signal is taken to
/// span the converter's full scale.
fn signal_peak(ends: &[(f64, Option<f64>)], fs: f64) -> Option<(f64, bool)> {
    if !ends.iter().any(|(_, v)| *v == Some(0.0)) {
        return Some((fs, false));
    }
    let known = (ends.iter())
        .map(|(g, v)| v.map(|v| (*g, v)))
        .collect::<Option<Vec<_>>>()?;
    let conductance: f64 = known.iter().map(|(g, _)| g).sum();
    let open = known.iter().map(|(g, v)| g * v).sum::<f64>() / conductance;
    let peak = (open * (1.0 + RAIL_TOLERANCE)).min(fs);
    // A tap that tops out well under full scale never reaches the knee region.
    (peak >= NEAR_FULL_SCALE * fs).then_some((peak, true))
}

/// The level the pin settles at when the source `vin` behind `rs` feeds the
/// Zener's knee leakage as well: `(vin - v) / rs = I(v)`, solved by bisection.
fn loaded_pin_voltage(z: &Zener, vin: f64, rs: f64) -> f64 {
    let (mut lo, mut hi) = (0.0, vin);
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        if (vin - mid) / rs > z.current_at(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// A Zener clamp to ground on an ADC pad whose breakdown is at or just above
/// full scale, behind a series resistance that turns its knee leakage into a
/// visible reading error.
fn zener_clamp_flag(
    cx: &Ctx,
    models: &HashMap<&str, &ModelEntry>,
    (c, m): (&Component, &ModelEntry),
    (pin, net): (&hauksbee_extract::Pin, &Net),
    (fs, fs_from): (f64, &str),
) -> Option<Flag> {
    let (mut zeners, mut resistors, mut inv_r) = (Vec::new(), Vec::new(), 0.0f64);
    // The resistors' far ends: (conductance, the level there, if known).
    let mut ends: Vec<(f64, Option<f64>)> = Vec::new();
    for (member, mpin) in
        (cx.board.net_members(net.id).iter()).filter(|(mc, _)| mc.reference != c.reference)
    {
        if let Some(z) = zener_clamp(cx, models, member, net.id) {
            zeners.push(z);
        } else if let Some(ohms) = super::bus_loading::resistor_ohms(member) {
            inv_r += 1.0 / ohms;
            resistors.push(member.reference.clone());
            ends.push((1.0 / ohms, far_end_level(cx, member, mpin)));
        } else {
            // Anything else that is not a bypass, a diode or a test point could
            // be the source itself: its impedance is unknown, so no claim.
            let passive = hauksbee_extract::is_capacitor(member)
                || hauksbee_models::refdes::RefClass::of(&member.reference)
                    == hauksbee_models::refdes::RefClass::Diode
                || hauksbee_extract::is_test_point_or_hole(member);
            passive.then_some(())?;
        }
    }
    let z = zeners.iter().min_by(|a, b| a.vz.total_cmp(&b.vz))?;
    // The Thevenin resistance behind the pad, taken from the resistors on the
    // net with every far end stiff: a lower bound, so the error is a floor.
    let rs = 1.0 / inv_r;
    (!resistors.is_empty() && inv_r > 0.0 && rs >= MIN_CLAMP_SERIES_OHMS).then_some(())?;
    (z.vz <= ZENER_WINDOW * fs).then_some(())?;
    let (vin, shunted) = signal_peak(&ends, fs)?;
    let pin_v = loaded_pin_voltage(z, vin, rs);
    let leak = z.current_at(pin_v);
    let error = vin - pin_v;
    (error >= VISIBLE_ERROR * fs).then_some(())?;
    let onset = KNEE_VOLTAGE_FRACTION
        .mul_add(
            z.vz,
            z.decade_v * (VISIBLE_ERROR * fs / (rs * z.knee_a)).log10(),
        )
        .max(0.0);
    let role = m.pins.get(&pin.number).map_or("", String::as_str);
    let relation = clamp_relation(z.vz, fs);
    let source = if shunted {
        "the divider's own open-circuit level from its rails, 10 % high"
    } else {
        "assumed to span the converter's full scale; the board states no source level"
    };
    let message = format!(
        "{} clamps the ADC input of {} pad {} ({}, net '{}') to ground at {} ({}), but the ADC \
         full scale is {} ({fs_from}), and the clamp is {relation}. A Zener this small has a \
         soft knee and conducts well before its rated voltage. {}. Through the {} of source \
         resistance behind the pad ({}) a signal that reaches {} ({source}) is pulled down to \
         {} by {} of leakage: an error of {} ({:.1} % of full scale), and the reading departs \
         from linear from about {} at the \
         pin: it goes non-linear towards the top of the range during calibration. \
         {REMEDY_MARKER}Clamp with a diode pair to the ADC reference and ground (Schottky), or \
         a Zener well above full scale, and keep the series resistor no larger than the \
         converter's source-impedance limit needs.",
        z.reference,
        c.reference,
        pin.number,
        role.to_ascii_uppercase(),
        net.name,
        format_volts(z.vz),
        z.vz_from,
        format_volts(fs),
        z.knee_from,
        format_engineering(rs, "Ω"),
        resistors.join(", "),
        format_volts(vin),
        format_volts(pin_v),
        format_amps(leak),
        format_volts(error),
        error / fs * 100.0,
        format_volts(onset),
    );
    let mut refs = vec![c.reference.clone(), z.reference.clone()];
    refs.extend(resistors);
    let finding = LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity: Severity::Medium,
        message,
        refs,
        nets: vec![net.name.clone()],
    };
    Some((finding, false))
}

/// The most nets of a resistor network around an ADC pad that are followed.
const MAX_NETWORK_NETS: usize = 40;

/// The resistor network around `tap`, solved through the DC solver: every
/// resistor reachable from the tap through nets that are not rails, the rails
/// it ends on as voltage sources, the negative ones scaled by `neg_scale`.
/// Returns the tap voltage and the (name, volts) of each negative rail it reaches,
/// or `None` where the network cannot be judged: a net on the way carries
/// an active part, a rail has no known level, or no negative rail is reached.
fn solve_tap(
    cx: &Ctx,
    mcu: &Component,
    tap: i64,
    neg_scale: f64,
) -> Option<(f64, Vec<(String, f64)>)> {
    use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
    let board = cx.board;
    let rail_level = |name: &str, has_source: bool| -> Option<f64> {
        if super::is_ground_name(name) {
            return Some(0.0);
        }
        hauksbee_bind::binder::power_rail_voltage(name)
            .or_else(|| has_source.then(|| (cx.v)(name)).flatten())
    };
    let mut circuit = Circuit::new();
    let node_of = |c: &mut Circuit, id: i64, name: &str| -> NodeId {
        if super::is_ground_name(name) {
            NodeId::GROUND
        } else {
            c.node(&format!("n{id}"))
        }
    };
    let (mut seen, mut queue) = (BTreeSet::from([tap]), vec![tap]);
    let (mut negatives, mut resistors) = (Vec::new(), 0usize);
    let mut rails: Vec<(i64, f64)> = Vec::new();
    while let Some(id) = queue.pop() {
        let name = board.net(id)?.name.clone();
        let members = board.net_members(id);
        let others = || {
            members
                .iter()
                .filter(|(mc, _)| mc.reference != mcu.reference)
        };
        let is_rail = id != tap && {
            let sourced = others().any(|(mc, _)| {
                super::bus_loading::resistor_ohms(mc).is_none()
                    && !hauksbee_extract::is_capacitor(mc)
            });
            let level = rail_level(&name, sourced);
            if let Some(v) = level {
                rails.push((id, v));
                if v < 0.0 {
                    negatives.push((name.clone(), v));
                }
            } else if sourced {
                return None; // an active part on a net of unknown level
            }
            level.is_some()
        };
        if is_rail {
            continue;
        }
        for (member, pin) in others() {
            if let Some(ohms) = super::bus_loading::resistor_ohms(member) {
                let far = member.pins.iter().find(|q| q.number != pin.number)?.net;
                let Some(far) = far.filter(|f| *f != 0 && *f != id) else {
                    continue;
                };
                let (a, b) = (node_of(&mut circuit, id, &name), {
                    let far_name = board.net(far)?.name.clone();
                    node_of(&mut circuit, far, &far_name)
                });
                circuit.add(Device::resistor(member.reference.as_str(), a, b, ohms));
                resistors += 1;
                if seen.insert(far) {
                    queue.push(far);
                }
            } else if hauksbee_extract::is_capacitor(member)
                || hauksbee_models::refdes::RefClass::of(&member.reference)
                    == hauksbee_models::refdes::RefClass::Diode
                || hauksbee_extract::is_test_point_or_hole(member)
            {
                // DC-open (a diode only clamps the answer found below).
            } else if id == tap {
                return None; // something else drives the tap
            }
        }
        if seen.len() > MAX_NETWORK_NETS {
            return None;
        }
    }
    if negatives.is_empty() || resistors == 0 {
        return None;
    }
    for (id, v) in &rails {
        let name = &board.net(*id)?.name;
        let node = node_of(&mut circuit, *id, name);
        if node.is_ground() {
            continue;
        }
        let volts = if *v < 0.0 { v * neg_scale } else { *v };
        circuit.add(Device::vsource(
            format!("V{id}"),
            node,
            NodeId::GROUND,
            SourceKind::Dc(volts),
        ));
    }
    let tap_name = board.net(tap)?.name.clone();
    let tap_node = node_of(&mut circuit, tap, &tap_name);
    let v = node_voltage(&circuit, tap_node)?;
    Some((v, negatives))
}

/// The DC operating-point voltage of `node` in `circuit`, if it converges.
fn node_voltage(circuit: &hauksbee_ir::Circuit, node: hauksbee_ir::NodeId) -> Option<f64> {
    let mut ws = hauksbee_solve::Workspace::new(circuit);
    hauksbee_solve::dc_operating_point(&mut ws, circuit, &hauksbee_solve::SolverOptions::default())
        .ok()?;
    let v = ws.layout.node(node).and_then(|i| ws.x.get(i).copied())?;
    v.is_finite().then_some(v)
}

/// A divider from a negative rail whose tap, solved at the rail's nominal level
/// and ±10 %, lands outside the ADC's 0 to full scale range, or within 5 % of
/// either end of it.
fn negative_rail_flag(
    cx: &Ctx,
    (c, m): (&Component, &ModelEntry),
    (pin, net): (&hauksbee_extract::Pin, &Net),
    (fs, fs_from): (f64, &str),
) -> Option<Flag> {
    let (nominal, negatives) = solve_tap(cx, c, net.id, 1.0)?;
    let larger = solve_tap(cx, c, net.id, 1.0 + RAIL_TOLERANCE)?.0;
    let smaller = solve_tap(cx, c, net.id, 1.0 - RAIL_TOLERANCE)?.0;
    let margin = END_MARGIN * fs;
    let outside = |v: f64| v < 0.0 || v > fs;
    let tight = |v: f64| v < margin || v > fs - margin;
    let corners = [nominal, larger, smaller];
    let (nominal_out, any_out, any_tight) = (
        outside(nominal),
        corners.iter().any(|v| outside(*v)),
        corners.iter().any(|v| tight(*v)),
    );
    (any_out || any_tight).then_some(())?;
    let rails = negatives
        .iter()
        .map(|(n, v)| format!("'{n}' at {}", format_volts(*v)))
        .collect::<Vec<_>>()
        .join(", ");
    let role = m.pins.get(&pin.number).map_or("", String::as_str);
    let verdict = if nominal_out {
        let side = if nominal < 0.0 {
            "below ground"
        } else {
            "above full scale"
        };
        format!(
            "At the nominal rail the pin is driven {side}, outside the ADC range of 0 to {}: it sits at a clamp \
             (or overstresses the pin) and the reading never follows the rail",
            format_volts(fs)
        )
    } else if any_out {
        format!(
            "At the nominal rail the tap is inside the range, but with the rail {:.0} % \
             either way it leaves the range of 0 to {}",
            RAIL_TOLERANCE * 100.0,
            format_volts(fs)
        )
    } else {
        format!(
            "The tap is inside the range of 0 to {} but within {:.0} % of one end, with no \
             headroom for the converter's own error or the rail's tolerance",
            format_volts(fs),
            END_MARGIN * 100.0
        )
    };
    let message = format!(
        "{} pad {} ({}, net '{}') is the tap of a resistor network from a negative rail ({rails}). \
         Solved with the DC solver, the tap reads {} at the nominal rail, {} with the rail 10 % \
         higher in magnitude and {} with it 10 % lower. {verdict}; the ADC full scale is {} \
         ({fs_from}). {REMEDY_MARKER}Pull the tap up to the ADC reference (or a positive rail) \
         with the second resistor, and choose the ratio so the tap spans 5 % to 95 % of full \
         scale over the rail's tolerance.",
        c.reference,
        pin.number,
        role.to_ascii_uppercase(),
        net.name,
        format_volts(nominal),
        format_volts(larger),
        format_volts(smaller),
        format_volts(fs),
    );
    let severity = if nominal_out {
        Severity::High
    } else {
        Severity::Medium
    };
    let finding = LintFinding {
        check: LintCheck::OperatingEnvelope,
        severity,
        message,
        refs: vec![c.reference.clone()],
        nets: vec![net.name.clone()],
    };
    Some((finding, false))
}

/// The front-end flags of every wired ADC pad of an MCU `c` with an ADC table.
fn front_end_flags(
    cx: &Ctx,
    models: &HashMap<&str, &ModelEntry>,
    (c, m): (&Component, &ModelEntry),
) -> Vec<Flag> {
    if m.kind != hauksbee_models::ComponentKind::Mcu || !m.adc_declared() {
        return Vec::new();
    }
    let Some((fs, fs_from)) = adc_full_scale(cx, c, m) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for pad in wired_adc_pads(cx, c, m) {
        let basis = (fs, fs_from.as_str());
        out.extend(zener_clamp_flag(cx, models, (c, m), pad, basis));
        out.extend(negative_rail_flag(cx, (c, m), pad, basis));
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
    use crate::test_support::pin_fn;
    use hauksbee_ir::evidence::AssumptionKind as Kind;
    use hauksbee_ir::evidence::AssumptionKind::{OperatingLimitViolated, ReducedFidelity};

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

    // ── The ADC front end ────────────────────────────────────────────────────

    /// Nets: 1 GND, 2 +3V3, 3 ADC (the pad's net), 4 SIG, 5 -12V, 6 VNEG.
    /// U1 is an `ATmega328P` powered from +3V3 (full scale 3.3 V, its AREF pad
    /// unwired), with `extra` parts around the ADC0 pad.
    fn front_board(extra: Vec<Component>) -> ExtractedBoard {
        let mut parts = vec![part(
            "U1",
            "ATmega328P",
            &[("7", 2), ("8", 1), ("20", 2), ("22", 1), ("23", 3)],
        )];
        parts.extend(extra);
        let nets = [
            (1, "GND"),
            (2, "+3V3"),
            (3, "ADC"),
            (4, "SIG"),
            (5, "-12V"),
            (6, "VNEG"),
        ];
        board(&nets, parts)
    }

    fn rails(net: &str) -> Option<f64> {
        hauksbee_bind::binder::power_rail_voltage(net)
    }

    fn front_end(b: &ExtractedBoard, volts: &dyn Fn(&str) -> Option<f64>) -> Vec<LintFinding> {
        let all = analog_input_lint(b, &ModelLibrary::builtin(), volts).findings;
        let ours = |f: &LintFinding| {
            f.message.contains("clamps the ADC input")
                || f.message.contains("resistor network from a negative rail")
        };
        all.into_iter().filter(ours).collect()
    }

    /// A card-resolved Zener of `value` from the ADC net to ground, behind
    /// `ohms` from SIG.
    fn zener_front(value: &str, ohms: &str) -> ExtractedBoard {
        front_board(vec![
            part("R1", ohms, &[("1", 4), ("2", 3)]),
            part("D1", value, &[("K", 3), ("A", 1)]),
        ])
    }

    // A 3.3 V Zener behind 1 k on a 3.3 V ADC leaks long before full scale.
    #[test]
    fn a_zener_clamp_at_full_scale_behind_a_series_resistor_is_flagged() {
        for (value, says) in [
            ("1N4728A", "ASSUMED|IZK and ZZK"),
            ("BZT52C3V3", "ASSUMED|1 mA at 90 % of Vz"),
            ("1N4729A", "3.6 V|only 9 % above full scale"),
        ] {
            let b = zener_front(value, "1k");
            let got = front_end(&b, &rails);
            assert_eq!(got.len(), 1, "{value}: {got:?}");
            let f = &got[0];
            assert_eq!(f.severity, Severity::Medium, "{value}");
            let named = [
                "D1",
                "R1",
                "U1",
                "pad 23",
                "ADC0",
                "'ADC'",
                "3.3 V",
                "non-linear",
            ];
            for s in named.into_iter().chain(says.split('|')) {
                assert!(f.message.contains(s), "{value}: {s:?} in {}", f.message);
            }
            assert!(f.own_remedy().is_some(), "{value}");
            // The chokepoint every surface shares carries it too.
            let lint = super::super::engine_lint(&b, &ModelLibrary::builtin());
            let ours = |g: &&LintFinding| g.message.contains("clamps the ADC input");
            assert_eq!(lint.findings.iter().filter(ours).count(), 1, "{value}");
        }
    }

    // The silent controls: a clamp well above full scale (with a Schottky to the
    // rail), a series resistance too small to see, no series resistance, a
    // source that is not a resistor, and a Zener nowhere near full scale.
    #[test]
    fn zener_clamps_that_do_not_bend_the_range_are_silent() {
        let schottky = part("D2", "BAT54", &[("A", 3), ("K", 2)]);
        let mut five = zener_front("1N4733A", "1k");
        five.components.push(schottky);
        assert!(front_end(&five, &rails).is_empty(), "5.1 V + Schottky");
        let mid = zener_front("1N4732A", "1k");
        assert!(front_end(&mid, &rails).is_empty(), "4.7 V is 42 % above");
        assert!(front_end(&zener_front("1N4728A", "47"), &rails).is_empty());
        let direct = front_board(vec![part("D1", "1N4728A", &[("K", 3), ("A", 1)])]);
        assert!(front_end(&direct, &rails).is_empty(), "no series R");
        let mut driven = zener_front("1N4728A", "1k");
        driven.components.push(part("U2", "TL072", &[("1", 3)]));
        assert!(
            front_end(&driven, &rails).is_empty(),
            "an active part on the net"
        );
    }

    /// ADC net fed by `top` from net 7 (`rail`) with `bottom` to ground and a
    /// 3.3 V Zener across the lower leg.
    fn divider_front(rail: &str, top: &str, bottom: &str) -> ExtractedBoard {
        let mut b = front_board(vec![
            part("R1", top, &[("1", 7), ("2", 3)]),
            part("R2", bottom, &[("1", 3), ("2", 1)]),
            part("D1", "1N4728A", &[("K", 3), ("A", 1)]),
        ]);
        b.nets.push(hauksbee_extract::Net::new(7, rail));
        b
    }

    // A divider is designed to scale its source: a pack of unknown level, or a
    // known rail that leaves the tap well under full scale, never reaches the
    // Zener's knee; a known rail that does reach it is flagged.
    #[test]
    fn a_zener_across_a_divider_is_judged_by_the_divider_level() {
        let unknown = divider_front("BAT+", "100k", "10k");
        assert!(front_end(&unknown, &rails).is_empty(), "unknown top level");
        let low = divider_front("+12V", "100k", "10k");
        assert!(
            front_end(&low, &rails).is_empty(),
            "1.2 V tap on a 3.3 V ADC"
        );
        let high = divider_front("+12V", "27k", "10k");
        let got = front_end(&high, &rails);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(got[0].message.contains("divider's own open-circuit level"));
    }

    // A Zener the board names only by value text has no card: Vz is read from
    // the text and the message says the figure and the knee are assumed.
    #[test]
    fn a_zener_known_only_by_its_value_text_is_flagged_as_assumed() {
        let mut d = part("D1", "3.3V", &[("1", 3), ("2", 1)]);
        d.lib_id = "Device:D_Zener".into();
        d.pins = vec![pin_fn("1", 3, "K"), pin_fn("2", 1, "A")];
        let b = front_board(vec![part("R1", "1k", &[("1", 4), ("2", 3)]), d]);
        let got = front_end(&b, &rails);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].message.contains("ASSUMED from its value text"),
            "{}",
            got[0].message
        );
    }

    // The ADC reference is read from a wired reference pad when there is one.
    #[test]
    fn the_full_scale_follows_a_wired_reference_pad() {
        // AREF (pad 21) on a 2.5 V reference rail: a 3.3 V Zener sits 32 % above.
        let mut b = zener_front("1N4728A", "1k");
        b.nets.push(hauksbee_extract::Net::new(7, "VREF2V5"));
        b.components[0].pins.push(pin_fn("21", 7, ""));
        b.components
            .push(part("U2", "LM4040", &[("1", 7), ("2", 1)]));
        let volts = |n: &str| (n == "VREF2V5").then_some(2.5).or_else(|| rails(n));
        assert!(front_end(&b, &volts).is_empty(), "outside the 15 % window");
        // The same pad with only a bypass capacitor leaves the supply the reference.
        let mut bypass = zener_front("1N4728A", "1k");
        bypass.nets.push(hauksbee_extract::Net::new(7, "AREF"));
        bypass.components[0].pins.push(pin_fn("21", 7, ""));
        bypass
            .components
            .push(part("C1", "100n", &[("1", 7), ("2", 1)]));
        assert_eq!(front_end(&bypass, &rails).len(), 1);
    }

    /// The tap net (ADC) fed from -12 V through `neg` and from `pos_rail`
    /// through `pos` (net 2 is +3V3, net 1 is GND).
    fn neg_divider(neg: &str, pos: &str, pos_net: i64) -> ExtractedBoard {
        front_board(vec![
            part("R1", neg, &[("1", 5), ("2", 3)]),
            part("R2", pos, &[("1", pos_net), ("2", 3)]),
        ])
    }

    // -12 V through 10 k, up to +3.3 V through 100 k: the tap sits at
    // (-12*100k + 3.3*10k) / 110k = -10.6 V, nowhere near 0..3.3 V.
    #[test]
    fn a_negative_rail_divider_that_lands_below_ground_is_flagged() {
        let got = front_end(&neg_divider("10k", "100k", 2), &rails);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].severity, Severity::High);
        for s in [
            "U1 pad 23",
            "'-12V' at -12 V",
            "-10.61 V",
            "below ground",
            "0 to 3.3 V",
            "DC solver",
        ] {
            assert!(got[0].message.contains(s), "{s:?} in {}", got[0].message);
        }
        // Only to ground: the tap is -6 V.
        let to_ground = front_end(&neg_divider("10k", "10k", 1), &rails);
        assert_eq!(to_ground.len(), 1, "{to_ground:?}");
        assert!(
            to_ground[0].message.contains("-6 V"),
            "{}",
            to_ground[0].message
        );
    }

    // The control: -12 V through 100 k and +3.3 V through 10 k puts the tap at
    // 1.9 V, and 1.8 V to 2.0 V over the rail's 10 %: inside, with headroom.
    #[test]
    fn a_negative_rail_divider_that_lands_inside_the_range_is_silent() {
        assert!(front_end(&neg_divider("100k", "10k", 2), &rails).is_empty());
        // A plain positive divider is not this rule's business.
        let mut positive = neg_divider("100k", "10k", 2);
        positive.nets[4].name = "+12V".into();
        assert!(front_end(&positive, &rails).is_empty());
    }

    // A tap that lands inside the range with no headroom: 3.2 V of 3.3 V.
    #[test]
    fn a_negative_rail_divider_with_no_headroom_is_a_medium_finding() {
        let got = front_end(&neg_divider("100k", "680", 2), &rails);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].severity, Severity::Medium);
        assert!(got[0].message.contains("within 5 %"), "{}", got[0].message);
    }

    // A rail of unknown level abstains; the run's solved level judges it.
    #[test]
    fn a_negative_rail_the_name_does_not_state_is_judged_from_the_solved_level() {
        let mut b = neg_divider("10k", "100k", 2);
        b.components[1].pins[0].net = Some(6); // R1 now ends on VNEG
        b.components.push(part("U3", "TL072", &[("1", 6)])); // its source
        assert!(front_end(&b, &rails).is_empty(), "unknown level: abstain");
        let solved = |n: &str| (n == "VNEG").then_some(-12.0).or_else(|| rails(n));
        let got = front_end(&b, &solved);
        assert_eq!(got.len(), 1, "{got:?}");
        assert!(
            got[0].message.contains("'VNEG' at -12 V"),
            "{}",
            got[0].message
        );
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

    const HEADER_FP: &str = "Connector_PinHeader_2.54mm:PinHeader_2x05_P2.54mm_Vertical";

    /// A 2x5 header whose pin `n` is on net `nets[n - 1]` (0: unconnected).
    fn header(reference: &str, nets: &[i64]) -> Component {
        let mut h = part(reference, "Conn_02x05", &[]);
        h.footprint = HEADER_FP.into();
        h.lib_id = "Connector:Conn_02x05".into();
        h.pins = nets
            .iter()
            .enumerate()
            .map(|(i, &net)| hauksbee_extract::Pin {
                number: (i + 1).to_string(),
                net: (net != 0).then_some(net),
                ..Default::default()
            })
            .collect();
        h
    }

    /// Two boards mated by a 10-pin header pair: a carrier whose pot wiper is on
    /// header pin 3, and a breakout whose header pin 3 reaches pad `pad` of an
    /// `ATmega328P`.
    fn mated_boards(pad: &str) -> Vec<(String, ExtractedBoard)> {
        let carrier = board(
            &[(1, "GND"), (2, "+3V3"), (3, "WIPER")],
            vec![
                header("J1", &[1, 2, 3, 0, 0, 0, 0, 0, 0, 0]),
                part("RV1", "10k", &[("1", 2), ("2", 3), ("3", 1)]),
            ],
        );
        let breakout = board(
            &[(1, "GND"), (2, "VCC"), (3, "A_IN")],
            vec![
                header("J1", &[1, 2, 3, 0, 0, 0, 0, 0, 0, 0]),
                part(
                    "U1",
                    "ATmega328P",
                    &[("7", 2), ("8", 1), ("20", 2), ("22", 1), (pad, 3)],
                ),
            ],
        );
        vec![("carrier".into(), carrier), ("breakout".into(), breakout)]
    }

    // The pot is on one board and the MCU on the other: neither board alone has
    // an analogue source meeting the MCU's pad. Assembled, the wiper reaches pad
    // 4 (PD2, no ADC channel) and is flagged; on pad 23 (ADC0) it is silent.
    #[test]
    fn a_carrier_wiper_reaching_a_non_adc_pad_of_the_breakout_is_flagged_after_assembly() {
        let boards = mated_boards("4");
        for (label, b) in &boards {
            assert!(no_adc(b).is_empty(), "{label} alone: nothing to judge");
        }
        let assembled = hauksbee_extract::mate::assemble(&boards, &[]);
        assert_eq!(assembled.mates.len(), 1, "{:?}", assembled.notes);
        let got = no_adc(&assembled.board);
        assert_eq!(got.len(), 1, "{got:?}");
        for want in ["U1 pad 4", "PD2_INT0", "WIPER", "PC0_ADC0"] {
            assert!(got[0].message.contains(want), "{want}: {}", got[0].message);
        }
    }

    #[test]
    fn a_carrier_wiper_reaching_an_adc_pad_of_the_breakout_is_silent_after_assembly() {
        let assembled = hauksbee_extract::mate::assemble(&mated_boards("23"), &[]);
        assert_eq!(assembled.mates.len(), 1, "{:?}", assembled.notes);
        assert!(no_adc(&assembled.board).is_empty());
    }
}
