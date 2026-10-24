//! Bulk hold-up: the ripple valley on a rectifier's bulk capacitor against the
//! minimum input of the regulator it feeds.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! A mains transformer and bridge (or a single diode) feed a bulk capacitor
//! that is topped up twice per mains cycle (once for a half-wave rectifier) and
//! discharged by the load in between. The bulk voltage therefore ripples, and a
//! regulator fed from it drops out in every valley when the valley falls below
//! the regulator's minimum input: hum on the output, or a rail that resets the
//! board at the mains frequency. Ripple is `I / (f * C)` to first order; the
//! solver below uses the actual rectified waveform, so a small capacitor that
//! conducts for much of the cycle is not over-penalised.
//!
//! ## When it speaks
//!
//! Only with positive evidence of an AC source: a bridge (four diodes, or a
//! bridge package) or a lone rectifier diode whose AC side carries a stated RMS
//! voltage ("12VAC", "24 V AC", `AC_9V`") on a net name, a connector or
//! transformer value, or a net property.
//!
//! A bridge whose two AC nets each reach a connector or a transformer (through
//! at most two series fuses, resistors or ferrites) also counts as AC evidence
//! when no voltage is stated: the transformer is off-board and the board never
//! says what it delivers. The valley cannot be computed then, so the finding is
//! a Medium "hold-up unverified: state the AC input voltage" that names the
//! bridge, the bulk total, the regulators' minimum input and the RMS voltage the
//! connector must deliver for the rail to hold up. The evidence is withdrawn
//! when a net on the path or the connector itself reads as DC (a rail or battery
//! name, a barrel jack, USB): a bridge on a DC input is polarity protection and
//! has no AC voltage. A bridge behind an unmarked connector cannot be told from
//! polarity protection by topology, so the message says to ignore it for a DC
//! input. A lone diode needs a stated voltage: a series diode from a connector
//! is reverse protection far more often than a half-wave rectifier. The bulk
//! capacitor is what the
//! `supply_cap` check counts on the DC node: fitted capacitors from the
//! rectifier's positive node to its negative. A regulator is judged when its
//! `in` pin sits on that node and its card has a `supply_range` row for `in`
//! (the datasheet's recommended minimum input).
//!
//! ## Why a finding is sound
//!
//! Every approximation errs toward a higher valley, so a flag stands:
//!
//!   - the line is taken 10 % high, the diodes drop only 0.6 V each, and the
//!     source has no impedance;
//!   - the load is a lower bound: the sum of the currents the board and the
//!     cards establish downstream of the regulator (an attributed operating
//!     current, a resistor to ground, a card's `icc_a`/`supply_static_ua` or
//!     downstream regulator's `iq_a`). Unknown loads only add to it, so the true
//!     droop is at least what is computed. With no known load the check is
//!     silent;
//!   - the mains frequency is assumed 50 Hz, the lower of the two in use and so
//!     the longer discharge. A 60 Hz mains has a shorter one: the message says
//!     the assumption and the figure at 60 Hz would be better.
//!
//! The regulator's `dropout_v` is not used: it is specified at maximum load, and
//! at the lower load assumed here it would overstate the need.

use std::collections::{BTreeMap, BTreeSet};

use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Pin, Severity, REMEDY_MARKER,
};
use hauksbee_ir::units::{format_amps, format_engineering, format_volts};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary, OperatingEnvelope};

use super::envelope::role_net_id;
use super::regulator_cout::{capacitance, caps_across_output, farads};
use super::ripple::named_rail_voltage;
use super::straps::is_assembled_resistor;

/// Mains frequency assumed: the lower of 50 and 60 Hz, which discharges longest.
const MAINS_HZ: f64 = 50.0;
/// Line tolerance taken in the source's favour (a +10 % mains swell).
const LINE_HIGH: f64 = 1.10;
/// A diode drop taken in the source's favour (a Schottky-class forward drop).
const DIODE_DROP_V: f64 = 0.6;

/// One rectifier found on the board.
struct Rectifier {
    /// Positive and negative DC nodes.
    pos: i64,
    neg: i64,
    /// Diodes in the conduction path (2 for a bridge, 1 for a half-wave).
    drops: u32,
    /// Ripples per mains cycle (2 full-wave, 1 half-wave).
    pulses: u32,
    /// The stated RMS voltage at the AC side (`None` when only a connector or
    /// transformer says the input is AC), and the text it was read from.
    vrms: Option<f64>,
    source: String,
    /// The parts of the rectifier, for the message.
    refs: Vec<String>,
}

/// An RMS voltage stated in `text`: "12VAC", "24 V AC", `9V_RMS`, `AC_12V`.
fn stated_vrms(text: &str) -> Option<f64> {
    let t = text.to_ascii_uppercase();
    let b = t.as_bytes();
    // A number ending just before the marker, allowing a space or underscore.
    let number_before = |end: usize| -> Option<f64> {
        let mut e = end;
        while e > 0 && matches!(b[e - 1], b' ' | b'_' | b'-') {
            e -= 1;
        }
        let mut s = e;
        while s > 0 && (b[s - 1].is_ascii_digit() || b[s - 1] == b'.') {
            s -= 1;
        }
        t[s..e].parse::<f64>().ok().filter(|v| *v > 0.0)
    };
    // A number starting just after the marker, then a "V".
    let number_after = |start: usize| -> Option<f64> {
        let mut s = start;
        while s < b.len() && matches!(b[s], b' ' | b'_' | b'-') {
            s += 1;
        }
        let mut e = s;
        while e < b.len() && (b[e].is_ascii_digit() || b[e] == b'.') {
            e += 1;
        }
        let after_v = t[e..].starts_with('V');
        after_v
            .then(|| t[s..e].parse::<f64>().ok())
            .flatten()
            .filter(|v| *v > 0.0)
    };
    for marker in ["VAC", "V_AC", "V AC", "VRMS", "V_RMS", "V RMS"] {
        let mut from = 0;
        while let Some(i) = t[from..].find(marker) {
            let at = from + i;
            if let Some(v) = number_before(at) {
                return Some(v);
            }
            from = at + marker.len();
        }
    }
    for marker in ["AC_", "AC ", "AC-", "AC"] {
        let mut from = 0;
        while let Some(i) = t[from..].find(marker) {
            let at = from + i;
            let starts_word = at == 0 || !b[at - 1].is_ascii_alphabetic();
            if starts_word {
                if let Some(v) = number_after(at + marker.len()) {
                    return Some(v);
                }
            }
            from = at + marker.len();
        }
    }
    None
}

/// A diode's (anode net, cathode net) from its pin names.
fn diode_ends(c: &Component) -> Option<(i64, i64)> {
    if RefClass::of(&c.reference) != RefClass::Diode || c.dnp || c.pins.len() != 2 {
        return None;
    }
    let by = |names: &[&str]| {
        c.pins
            .iter()
            .find(|p| names.contains(&p.function.trim().to_ascii_uppercase().as_str()))
            .and_then(|p| p.net)
    };
    let anode = by(&["A", "ANODE", "A1"])?;
    let cathode = by(&["K", "C", "CATHODE", "K1"])?;
    (anode != cathode).then_some((anode, cathode))
}

/// A stated AC voltage near the AC nets `nets`: their names, and the value and
/// properties of the parts that touch them.
fn ac_source(board: &ExtractedBoard, nets: &[i64]) -> Option<(f64, String)> {
    for &id in nets {
        if let Some(n) = board.net(id) {
            if let Some(v) = stated_vrms(&n.name) {
                return Some((v, format!("net '{}'", n.name)));
            }
        }
        for (c, _) in board.net_members(id) {
            if RefClass::of(&c.reference) == RefClass::Diode {
                continue;
            }
            let texts = std::iter::once(c.value.as_str())
                .chain(c.properties.iter().map(|(_, v)| v.as_str()));
            for text in texts {
                if let Some(v) = stated_vrms(text) {
                    return Some((v, format!("{} '{}'", c.reference, text.trim())));
                }
            }
        }
    }
    None
}

/// Words in a net name that say the net is a DC supply, not an AC input.
const DC_NET_TOKENS: [&str; 15] = [
    "VIN", "VBUS", "VBAT", "VBATT", "BAT", "BATT", "DC", "VDC", "DCIN", "VCC", "VDD", "VSYS",
    "USB", "SOLAR", "PV",
];

/// Whether a net's name reads as a DC rail, a battery or ground.
fn net_reads_dc(name: &str) -> bool {
    let n = super::norm(name);
    n.starts_with('+')
        || n.starts_with('-')
        || super::is_ground_name(name)
        || named_rail_voltage(name).is_some()
        || n.split(|c: char| !c.is_ascii_alphanumeric())
            .any(|t| DC_NET_TOKENS.contains(&t))
}

/// Whether a connector's own text (value, symbol, footprint, properties) says
/// it brings in DC: a barrel jack, a battery, USB, a solar panel.
fn connector_reads_dc(c: &Component) -> bool {
    let texts = [c.value.as_str(), c.lib_id.as_str(), c.footprint.as_str()]
        .into_iter()
        .chain(c.properties.iter().map(|(_, v)| v.as_str()));
    texts.into_iter().any(|t| {
        let t = t.to_ascii_uppercase();
        ["JACK", "BARREL", "BATT", "USB", "SOLAR", "POLARITY"]
            .iter()
            .any(|w| t.contains(w))
            || t.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| matches!(w, "DC" | "DCIN" | "VDC" | "PV"))
    })
}

/// A part the AC input enters the board through: a connector proper (not a
/// test point, mounting hole, switch or jumper) or a transformer, that does not
/// read as a DC source.
fn is_ac_entry(c: &Component) -> bool {
    if c.dnp {
        return false;
    }
    let r = c.reference.to_ascii_uppercase();
    let numbered = |p: &str| {
        r.strip_prefix(p)
            .is_some_and(|rest| rest.starts_with(|ch: char| ch.is_ascii_digit()))
    };
    let transformer = numbered("T")
        || numbered("TR")
        || numbered("TX")
        || c.value.to_ascii_uppercase().contains("TRANSFORMER");
    let connector = hauksbee_extract::is_connector_like(c)
        && !["TP", "MH", "MK", "SW", "JP"]
            .iter()
            .any(|p| r.starts_with(p));
    (connector || transformer) && !connector_reads_dc(c)
}

/// A two-pad part an AC input passes through on its way to the bridge: a
/// fuse, a series resistor, a ferrite bead or an inductor.
fn is_series_part(c: &Component) -> bool {
    let r = c.reference.to_ascii_uppercase();
    let fuse = r.starts_with('F') && !r.starts_with("FB");
    !c.dnp
        && c.pins.len() == 2
        && (fuse
            || matches!(
                RefClass::of(&c.reference),
                RefClass::Resistor | RefClass::Inductor | RefClass::FerriteBead
            ))
}

/// The connector or transformer `net` is fed from, through at most two series
/// parts, where no net on the way and no entry part reads as DC.
fn entry_of(board: &ExtractedBoard, net: i64) -> Option<&Component> {
    let mut seen = BTreeSet::from([net]);
    let mut frontier = vec![net];
    for _ in 0..3 {
        let mut onward = Vec::new();
        for &id in &frontier {
            if board.net(id).is_none_or(|n| net_reads_dc(&n.name)) {
                return None; // a DC-named net on the way: not an AC input
            }
            for (c, _) in board.net_members(id) {
                if is_ac_entry(c) {
                    return Some(c);
                }
                if is_series_part(c) {
                    onward.extend(
                        c.pins
                            .iter()
                            .filter_map(|p| p.net)
                            .filter(|far| seen.insert(*far)),
                    );
                }
            }
        }
        frontier = onward;
    }
    None
}

/// The text naming the connector(s) every net in `nets` is fed from, where each
/// one reaches an AC entry part.
fn connector_feed(board: &ExtractedBoard, nets: &[i64]) -> Option<String> {
    let mut entries: Vec<&str> = Vec::new();
    for &id in nets {
        let c = entry_of(board, id)?;
        if !entries.contains(&c.reference.as_str()) {
            entries.push(c.reference.as_str());
        }
    }
    Some(format!(
        "{} (the AC voltage is not stated on the board)",
        entries.join(", ")
    ))
}

/// The AC source of a bridge: a stated RMS voltage, or (voltage `None`) only the
/// connector or transformer the AC nets come from.
fn bridge_feed(board: &ExtractedBoard, ac: &[i64]) -> Option<(Option<f64>, String)> {
    ac_source(board, ac)
        .map(|(v, s)| (Some(v), s))
        .or_else(|| connector_feed(board, ac).map(|s| (None, s)))
}

/// The pin of a bridge package by its printed name.
fn bridge_pin(c: &Component, names: &[&str]) -> Option<i64> {
    c.pins
        .iter()
        .find(|p| names.contains(&p.function.trim().to_ascii_uppercase().as_str()))
        .and_then(|p: &Pin| p.net)
}

type DiodeEnds<'a> = Vec<(&'a Component, i64, i64)>;

/// Discrete full-wave bridges: two diodes into P from distinct nets a1, a2,
/// and two out of N from the same a1 and a2.
fn discrete_bridges(board: &ExtractedBoard, diodes: &DiodeEnds<'_>) -> Vec<Rectifier> {
    let mut found = Vec::new();
    let mut seen_bridges = BTreeSet::new();
    for &(_, a1, p) in diodes {
        for &(_, a2, p2) in diodes {
            if p2 != p || a2 <= a1 {
                continue;
            }
            for &(_, n, k1) in diodes {
                if k1 != a1 {
                    continue;
                }
                let closes = diodes.iter().any(|&(_, n2, k2)| n2 == n && k2 == a2);
                if !closes || n == p || !seen_bridges.insert((p, n)) {
                    continue;
                }
                let refs: Vec<String> = diodes
                    .iter()
                    .filter(|&&(_, a, k)| {
                        (k == p && (a == a1 || a == a2)) || (a == n && (k == a1 || k == a2))
                    })
                    .map(|&(c, _, _)| c.reference.clone())
                    .collect();
                if let Some((vrms, source)) = bridge_feed(board, &[a1, a2]) {
                    found.push(Rectifier {
                        pos: p,
                        neg: n,
                        drops: 2,
                        pulses: 2,
                        vrms,
                        source,
                        refs,
                    });
                }
            }
        }
    }
    found
}

/// Bridge packages: two "~"/AC pins and a "+" and a "-" pin.
fn package_bridges(board: &ExtractedBoard) -> Vec<Rectifier> {
    let mut found = Vec::new();
    for c in &board.components {
        if c.dnp || c.pins.len() != 4 {
            continue;
        }
        let ac: Vec<i64> = c
            .pins
            .iter()
            .filter(|p| {
                let f = p.function.trim().to_ascii_uppercase();
                f.starts_with('~') || f == "AC" || f == "AC1" || f == "AC2"
            })
            .filter_map(|p| p.net)
            .collect();
        let (Some(pos), Some(neg)) = (
            bridge_pin(c, &["+", "DC+", "V+", "POS"]),
            bridge_pin(c, &["-", "DC-", "V-", "NEG"]),
        ) else {
            continue;
        };
        if ac.len() != 2 || pos == neg {
            continue;
        }
        if let Some((vrms, source)) = bridge_feed(board, &ac) {
            found.push(Rectifier {
                pos,
                neg,
                drops: 2,
                pulses: 2,
                vrms,
                source,
                refs: vec![c.reference.clone()],
            });
        }
    }
    found
}

/// A lone diode from an AC source into a bulk capacitor: half-wave, return on
/// the negative side of the capacitor (a ground net). Diodes already part of
/// `found` are skipped.
fn half_wave(
    board: &ExtractedBoard,
    diodes: &DiodeEnds<'_>,
    found: &[Rectifier],
) -> Vec<Rectifier> {
    let mut out = Vec::new();
    for &(c, a, k) in diodes {
        if found.iter().any(|r| r.refs.contains(&c.reference)) {
            continue;
        }
        if board
            .net(a)
            .is_some_and(|n| hauksbee_bind::binder::is_ground(&n.name))
        {
            continue;
        }
        let Some((vrms, source)) = ac_source(board, &[a]) else {
            continue;
        };
        let vrms = Some(vrms);
        let ground = board
            .nets
            .iter()
            .find(|n| hauksbee_bind::binder::is_ground(&n.name))
            .map(|n| n.id);
        if let Some(neg) = ground.filter(|g| *g != k) {
            out.push(Rectifier {
                pos: k,
                neg,
                drops: 1,
                pulses: 1,
                vrms,
                source,
                refs: vec![c.reference.clone()],
            });
        }
    }
    out
}

/// Every rectifier with a stated AC voltage.
fn rectifiers(board: &ExtractedBoard) -> Vec<Rectifier> {
    let diodes: DiodeEnds<'_> = board
        .components
        .iter()
        .filter_map(|c| diode_ends(c).map(|(a, k)| (c, a, k)))
        .collect();
    let mut found = discrete_bridges(board, &diodes);
    found.extend(package_bridges(board));
    let lone = half_wave(board, &diodes, &found);
    found.extend(lone);
    found
}

/// The lowest voltage the bulk capacitor reaches (V) with `c` farads feeding a
/// constant `i` amps, from a rectified sine of peak `vp` with `f_ripple` pulses
/// a second. The rectified waveform is `vp * |cos(pi f t)|`: the capacitor
/// follows it to the peak, lets go where the waveform falls faster than the
/// load discharges it, and discharges linearly until the next rising branch
/// climbs back to it.
fn valley_v(vp: f64, i: f64, c: f64, f_ripple: f64) -> f64 {
    let pi_f = std::f64::consts::PI * f_ripple;
    let period = 1.0 / f_ripple;
    let k = i / (c * vp * pi_f);
    if k >= 1.0 {
        return 0.0; // the load outruns even the waveform's steepest fall
    }
    let t0 = k.asin() / pi_f;
    let v0 = vp * (pi_f * t0).cos();
    let slope = i / c;
    let cap = |t: f64| slope.mul_add(-(t - t0), v0);
    let rising = |t: f64| vp * (pi_f * (t - period)).cos();
    if cap(period / 2.0) <= 0.0 {
        return 0.0;
    }
    let (mut lo, mut hi) = (period / 2.0, period);
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if cap(mid) > rising(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    cap(hi).max(0.0)
}

/// The smallest capacitance for which the valley reaches `need` volts.
fn capacitance_for(vp: f64, i: f64, need: f64, f_ripple: f64) -> Option<f64> {
    if need >= vp {
        return None;
    }
    let (mut lo, mut hi) = (1e-9_f64, 10.0_f64);
    if valley_v(vp, i, hi, f_ripple) < need {
        return None;
    }
    for _ in 0..80 {
        let mid = (lo * hi).sqrt();
        if valley_v(vp, i, mid, f_ripple) < need {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(hi)
}

/// The datasheet's minimum input voltage on a regulator card, with its basis.
fn min_input(model: &ModelEntry) -> Option<(f64, String)> {
    model.envelope.iter().find_map(|e| match e {
        OperatingEnvelope::SupplyRange {
            pin, min_v, basis, ..
        } if pin.eq_ignore_ascii_case("in") && *min_v > 0.0 => Some((*min_v, basis.clone())),
        _ => None,
    })
}

/// How much of the board a downstream draw was read from.
struct Draw {
    amps: f64,
    from: Vec<String>,
}

/// A lower bound on the current drawn from `net` (name `name`), by parts the
/// board and cards establish, to a depth of three regulators.
fn known_draw(
    board: &ExtractedBoard,
    models: &BTreeMap<&str, ModelEntry>,
    attributed: &std::collections::HashMap<String, (f64, String)>,
    (net, source): (i64, &str),
    depth: u32,
) -> Draw {
    let mut draw = Draw {
        amps: 0.0,
        from: Vec::new(),
    };
    let Some(n) = board.net(net) else {
        return draw;
    };
    if let Some((a, why)) = attributed.get(&n.name) {
        draw.amps += a;
        draw.from.push(format!("{why} ({})", format_amps(*a)));
    }
    let volts = named_rail_voltage(&n.name);
    for (c, _) in board.net_members(net) {
        if c.reference == source {
            continue;
        }
        if is_assembled_resistor(c) {
            let far = c.pins.iter().filter_map(|p| p.net).find(|id| *id != net);
            let to_ground = far
                .and_then(|id| board.net(id))
                .is_some_and(|f| hauksbee_bind::binder::is_ground(&f.name));
            let ohms = hauksbee_models::value::parse_value(&c.value)
                .filter(|v| matches!(v.unit.as_deref(), None | Some("Ω")))
                .map(|v| v.si)
                .filter(|r| *r > 0.0);
            if let (true, Some(r), Some(v)) = (to_ground, ohms, volts) {
                draw.amps += v / r;
                draw.from
                    .push(format!("{} {}", c.reference, c.value.trim()));
            }
            continue;
        }
        let Some(model) = models.get(c.reference.as_str()) else {
            continue;
        };
        let on_supply = model.pins.iter().any(|(pad, role)| {
            c.pins
                .iter()
                .any(|p| p.number == *pad && p.net == Some(net))
                && matches!(
                    role.to_ascii_lowercase().as_str(),
                    "vcc" | "vdd" | "vs" | "v+" | "vin" | "in" | "vio"
                )
        });
        if !on_supply {
            continue;
        }
        if model.kind == ComponentKind::Vreg {
            // A downstream regulator: its own quiescent current, and (to a
            // depth of three) what it feeds, scaled to its input.
            let iq = model.params.get_f64("iq_a").unwrap_or(0.0);
            let mut sub = iq;
            if depth < 3 {
                if let Some(out) = role_net_id(c, model, "out") {
                    let inner =
                        known_draw(board, models, attributed, (out, &c.reference), depth + 1);
                    let vout = model
                        .params
                        .get_f64("vout")
                        .or_else(|| board.net(out).and_then(|o| named_rail_voltage(&o.name)));
                    let scale = if model.behavioral.converter.is_some() {
                        // Power conservation at 100 % efficiency, at the higher input.
                        vout.zip(volts).map_or(0.0, |(vo, vi)| (vo / vi).min(1.0))
                    } else {
                        1.0
                    };
                    sub += inner.amps * scale;
                    draw.from.extend(inner.from);
                }
            }
            if sub > 0.0 {
                draw.amps += sub;
                draw.from.push(format!("{} input", c.reference));
            }
            continue;
        }
        let a = model
            .params
            .get_f64("icc_a")
            .or_else(|| model.params.get_f64("supply_static_ua").map(|ua| ua * 1e-6));
        if let Some(a) = a.filter(|a| *a > 0.0) {
            draw.amps += a;
            draw.from
                .push(format!("{} ({})", c.reference, format_amps(a)));
        }
    }
    draw
}

/// One finding per regulator on a rectifier's bulk node whose input valley is
/// below its card's minimum input.
#[must_use]
pub fn holdup_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let mut report = NetLintReport::default();
    let rects = rectifiers(board);
    if rects.is_empty() {
        return report;
    }
    let models: BTreeMap<&str, ModelEntry> = super::resolved_components(board, lib)
        .map(|(c, m)| (c.reference.as_str(), m))
        .collect();
    let attributed = super::ampacity::attributed_operating_currents(board, lib);
    for rect in &rects {
        report
            .findings
            .extend(rectifier_findings(board, &models, &attributed, rect));
    }
    report
}

/// The regulators on `rect`'s bulk node that its valley starves.
fn rectifier_findings(
    board: &ExtractedBoard,
    models: &BTreeMap<&str, ModelEntry>,
    attributed: &std::collections::HashMap<String, (f64, String)>,
    rect: &Rectifier,
) -> Vec<LintFinding> {
    let mut findings = Vec::new();
    let caps = caps_across_output(board, rect.pos, Some(rect.neg));
    if caps.iter().any(|c| capacitance(c).is_none()) {
        return findings; // an unreadable bulk value: no claim either way
    }
    let bulk: f64 = caps.iter().filter_map(|c| capacitance(c)).sum();
    if bulk <= 0.0 {
        return findings;
    }
    let regs: Vec<(&Component, &ModelEntry)> = board
        .components
        .iter()
        .filter_map(|c| models.get(c.reference.as_str()).map(|m| (c, m)))
        .filter(|(c, m)| m.kind == ComponentKind::Vreg && role_net_id(c, m, "in") == Some(rect.pos))
        .collect();
    let f_ripple = MAINS_HZ * f64::from(rect.pulses);
    // The bulk node's input current: every regulator on it.
    let mut loads: Vec<RegLoad> = Vec::new();
    let mut from: Vec<String> = Vec::new();
    for (c, m) in &regs {
        let Some(out) = role_net_id(c, m, "out") else {
            continue;
        };
        let d = known_draw(board, models, attributed, (out, &c.reference), 1);
        loads.push(RegLoad {
            amps: d.amps,
            iq: m.params.get_f64("iq_a").unwrap_or(0.0),
            vout: m
                .params
                .get_f64("vout")
                .or_else(|| board.net(out).and_then(|o| named_rail_voltage(&o.name))),
            converter: m.behavioral.converter.is_some(),
        });
        from.extend(d.from);
    }
    let Some(vrms) = rect.vrms else {
        return unverified_finding(board, rect, &caps, (&regs, &loads, &from), (bulk, f_ripple))
            .into_iter()
            .collect();
    };
    let vp = peak_v(vrms, rect.drops);
    let total: f64 = loads.iter().map(|l| l.input_current(vp)).sum();
    if total <= 0.0 {
        return findings;
    }
    let valley = valley_v(vp, total, bulk, f_ripple);
    let hold = Hold {
        vrms,
        vp,
        total,
        bulk,
        valley,
        f_ripple,
    };
    for &(c, m) in &regs {
        let Some((need, basis)) = min_input(m) else {
            continue;
        };
        if valley < need {
            findings.push(droop_finding(
                board,
                rect,
                &caps,
                (c, m),
                (need, basis.as_str()),
                &hold,
                &from,
            ));
        }
    }
    findings
}

/// The peak bulk voltage from a line of `vrms` (10 % high) through `drops` diodes.
fn peak_v(vrms: f64, drops: u32) -> f64 {
    (vrms * std::f64::consts::SQRT_2).mul_add(LINE_HIGH, -(f64::from(drops) * DIODE_DROP_V))
}

/// What one regulator on the bulk node draws from it.
struct RegLoad {
    /// The known current on its output rail, and its own quiescent current (A).
    amps: f64,
    iq: f64,
    vout: Option<f64>,
    converter: bool,
}

impl RegLoad {
    /// The current drawn from a bulk node at peak `vp`. A converter draws the
    /// output power over the input voltage (100 % efficiency, the peak input).
    fn input_current(&self, vp: f64) -> f64 {
        if self.converter {
            self.vout.map_or(0.0, |vo| self.amps * (vo / vp).min(1.0)) + self.iq
        } else {
            self.amps + self.iq
        }
    }
}

/// The one finding for a bridge whose AC voltage the board never states: the
/// hold-up cannot be judged, and the message says what the input must deliver.
fn unverified_finding(
    board: &ExtractedBoard,
    rect: &Rectifier,
    caps: &[&Component],
    (regs, loads, from): (&[(&Component, &ModelEntry)], &[RegLoad], &[String]),
    (bulk, f_ripple): (f64, f64),
) -> Option<LintFinding> {
    let judged: Vec<(&Component, &ModelEntry, f64, String)> = regs
        .iter()
        .filter_map(|&(c, m)| min_input(m).map(|(need, basis)| (c, m, need, basis)))
        .collect();
    let (_, strictest, need, basis) = judged.iter().max_by(|a, b| a.2.total_cmp(&b.2))?;
    let need = *need;
    let total_at = |vp: f64| loads.iter().map(|l| l.input_current(vp)).sum::<f64>();
    // As with a stated voltage, no known load downstream means no claim.
    if from.is_empty() || total_at(need) <= 0.0 {
        return None;
    }
    // The least peak at which the valley still reaches the minimum input; the
    // valley rises with the peak, so bisect.
    let holds = |vp: f64| valley_v(vp, total_at(vp), bulk, f_ripple) >= need;
    let (mut lo, mut hi) = (need, 5_000.0_f64);
    let peak = holds(hi).then(|| {
        for _ in 0..80 {
            let mid = 0.5 * (lo + hi);
            if holds(mid) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        hi
    });
    let bulk_net = board.net(rect.pos).map_or("?", |n| n.name.as_str());
    let reg_refs: Vec<String> = judged.iter().map(|(c, ..)| c.reference.clone()).collect();
    let required = peak.map_or_else(
        || {
            "No AC input voltage holds the rail up at this load with this bulk capacitance"
                .to_string()
        },
        |vp| {
            let vrms = f64::from(rect.drops).mul_add(DIODE_DROP_V, vp)
                / (std::f64::consts::SQRT_2 * LINE_HIGH);
            format!(
                "At about {} of load ({}) the rail only stays above {} in every valley if \
                 the AC input is at least {} RMS under load",
                format_amps(total_at(vp)),
                from.join(", "),
                format_volts(need),
                format_engineering(vrms, "V"),
            )
        },
    );
    let message = format!(
        "Rectified supply '{bulk_net}', hold-up unverified: state the AC input \
         voltage. {} rectify an AC input fed from {}; with {} of bulk capacitance, {} \
         ({}, card '{}') needs at least {} at its input (datasheet: {}). {required} \
         ({} Hz mains, a line 10 % high, {} per diode, no source impedance; unlisted loads \
         raise the figure). If this input is DC and the bridge is polarity protection, \
         ignore this.",
        rect.refs.join(", "),
        rect.source,
        farads(bulk),
        reg_refs.join(", "),
        strictest.description.trim(),
        strictest.id,
        format_volts(need),
        basis.trim(),
        MAINS_HZ,
        format_volts(DIODE_DROP_V),
    );
    let mut involved = reg_refs;
    involved.extend(rect.refs.iter().cloned());
    involved.extend(caps.iter().map(|x| x.reference.clone()));
    Some(LintFinding {
        check: LintCheck::HoldUpDroop,
        severity: Severity::Medium,
        message: format!(
            "{message} {REMEDY_MARKER}State the AC input voltage (a net name such as '12VAC', \
             or the transformer or connector value) so the valley can be computed, and size \
             the bulk capacitance on '{bulk_net}' for the lowest voltage it delivers under \
             load."
        ),
        refs: involved,
        nets: vec![bulk_net.to_string()],
    })
}

/// A rectifier's operating point: peak, input current, bulk C, valley, ripple.
struct Hold {
    vrms: f64,
    vp: f64,
    total: f64,
    bulk: f64,
    valley: f64,
    f_ripple: f64,
}

/// The finding for one starved regulator.
fn droop_finding(
    board: &ExtractedBoard,
    rect: &Rectifier,
    caps: &[&Component],
    (c, m): (&Component, &ModelEntry),
    (need, basis): (f64, &str),
    h: &Hold,
    from: &[String],
) -> LintFinding {
    let bulk_net = board.net(rect.pos).map_or("?", |n| n.name.as_str());
    let fix = capacitance_for(h.vp, h.total, need, h.f_ripple).map_or_else(
        || {
            format!(
                "{REMEDY_MARKER}No bulk capacitance holds the rail above {} at this load: \
                 raise the transformer or AC input voltage above the {} peak, or lower the load.",
                format_volts(need),
                format_volts(h.vp),
            )
        },
        |cmin| {
            format!(
                "{REMEDY_MARKER}Raise the bulk capacitance on '{bulk_net}' from {} to at \
                 least {} (or raise the AC input voltage, or use a regulator with a lower \
                 minimum input).",
                farads(h.bulk),
                farads(cmin),
            )
        },
    );
    let mut refs = vec![c.reference.clone()];
    refs.extend(rect.refs.iter().cloned());
    refs.extend(caps.iter().map(|x| x.reference.clone()));
    let loads = from.join(", ");
    LintFinding {
        check: LintCheck::HoldUpDroop,
        severity: Severity::High,
        message: format!(
            "{} ({}) is fed from '{bulk_net}', the rectified {} supply ({}, {} at the AC \
             side, {}): with {} of bulk capacitance the rail ripples down to about {} at \
             {} of load ({loads}), below the {} minimum input the datasheet gives (card \
             '{}': {}). The regulator drops out in every valley: hum on its output, or a \
             rail that resets the board at the mains frequency. The figure assumes \
             {} Hz mains (the worse case; 60 Hz is shorter), a line 10 % high, {} per \
             diode, no source impedance, and only the load listed; unlisted loads make it \
             worse. {fix}",
            c.reference,
            c.value.trim(),
            if rect.pulses == 2 {
                "full-wave"
            } else {
                "half-wave"
            },
            rect.refs.join(", "),
            format_engineering(h.vrms, "V"),
            rect.source,
            farads(h.bulk),
            format_volts(h.valley),
            format_amps(h.total),
            format_volts(need),
            m.id,
            basis.trim(),
            MAINS_HZ,
            format_volts(DIODE_DROP_V),
        ),
        refs,
        nets: vec![bulk_net.to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::*;
    use crate::test_support::pin_fn;

    #[test]
    fn an_ac_voltage_is_read_from_the_forms_boards_use() {
        for (text, want) in [
            ("12VAC", 12.0),
            ("24 V AC", 24.0),
            ("9V_AC", 9.0),
            ("AC_12V", 12.0),
            ("Net-(J1-12VAC)", 12.0),
            ("15V RMS", 15.0),
        ] {
            assert_eq!(stated_vrms(text), Some(want), "{text}");
        }
        for text in ["+12V", "VBUS", "ACIN", "BRIDGE", "10k", "PACK12V"] {
            assert_eq!(stated_vrms(text), None, "{text}");
        }
    }

    #[test]
    fn the_valley_falls_with_load_and_rises_with_capacitance() {
        let (vp, f) = (16.0, 100.0);
        let v = |i: f64, c: f64| valley_v(vp, i, c, f);
        assert!(v(0.5, 1e-3) > v(1.0, 1e-3));
        assert!(v(0.5, 4.7e-3) > v(0.5, 1e-3));
        // First-order check: dV = I/(f C) = 0.5 / (100 * 1e-3) = 5 V, an upper
        // bound on the droop; the real waveform climbs back sooner.
        assert!(v(0.5, 1e-3) >= vp - 5.0 - 0.5);
        assert!(v(0.5, 1e-3) < vp);
    }

    #[test]
    fn the_capacitance_found_reaches_the_needed_valley() {
        let (vp, i, f) = (16.0, 0.5, 100.0);
        let c = capacitance_for(vp, i, 10.0, f).unwrap();
        assert!(valley_v(vp, i, c, f) >= 10.0 - 1e-3);
        assert!(valley_v(vp, i, c * 0.9, f) < 10.0);
    }

    /// A user card for the regulator (min input 9 V, `iq_a`) and a load (an
    /// `icc_a` consumer on the output rail).
    const CARDS: &str = r#"
[[models]]
id = "holdup_test_ldo"
kind = "vreg"
description = "hold-up fixture"
[models.match]
value_re = "^HOLDLDO$"
[models.pins]
"1" = "gnd"
"2" = "out"
"3" = "in"
[models.params]
vout = 5.0
iq_a = 0.005
dropout_v = 2.0
[[models.envelope]]
kind = "supply_range"
pin = "in"
min_v = 9.0
max_v = 20.0
basis = "fixture datasheet Recommended Operating Conditions, VIN 9 V to 20 V, p.4"

[[models]]
id = "holdup_test_load"
kind = "digital"
description = "hold-up load"
[models.match]
value_re = "^HOLDLOAD$"
[models.pins]
"1" = "gnd"
"2" = "vcc"
[models.params]
identity_only = true
warning = "identity only"
unlocked_by = "validated behavior"
icc_a = 0.4
"#;

    fn library() -> ModelLibrary {
        let dir = hauksbee_testkit::fresh_scratch_dir("holdup", "cards");
        std::fs::write(dir.join("models.toml"), CARDS).unwrap();
        let mut lib = ModelLibrary::builtin();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        lib
    }

    /// Nets: 1 GND, 2 +5V, 3 RAW (bulk node), 4 AC1 (`ac1`), 5 AC2.
    /// A four-diode bridge from the AC nets, `bulk` on RAW, an LDO to +5V and a
    /// 0.4 A load.
    fn supply(ac1: &str, bulk: &[&str], load: bool) -> ExtractedBoard {
        let d = |r: &str, a: i64, k: i64| {
            let mut c = part(r, "1N4007", &[("1", k), ("2", a)]);
            c.pins = vec![pin_fn("1", k, "K"), pin_fn("2", a, "A")];
            c
        };
        let mut parts = vec![
            d("D1", 4, 3),
            d("D2", 5, 3),
            d("D3", 1, 4),
            d("D4", 1, 5),
            part("U1", "HOLDLDO", &[("1", 1), ("2", 2), ("3", 3)]),
        ];
        for (i, v) in bulk.iter().enumerate() {
            parts.push(part(&format!("C{}", i + 1), v, &[("1", 3), ("2", 1)]));
        }
        if load {
            parts.push(part("U2", "HOLDLOAD", &[("1", 1), ("2", 2)]));
        }
        board(
            &[(1, "GND"), (2, "+5V"), (3, "RAW"), (4, ac1), (5, "AC2")],
            parts,
        )
    }

    fn lint(b: &ExtractedBoard) -> Vec<LintFinding> {
        holdup_lint(b, &library()).findings
    }

    #[test]
    fn a_small_bulk_capacitor_on_a_12vac_bridge_drops_the_ldo_out() {
        // 12 VAC: peak 12*1.414*1.1 - 1.2 = 17.5 V. 0.4 A + 5 mA at 100 Hz
        // across 220 uF falls far below the 9 V minimum.
        let f = lint(&supply("12VAC", &["220u"], true));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].check, LintCheck::HoldUpDroop);
        assert_eq!(f[0].severity, Severity::High);
        for s in [
            "U1",
            "full-wave",
            "220 µF",
            "minimum input",
            "9 V",
            "50 Hz",
            "fixture datasheet Recommended Operating Conditions",
            "U2",
        ] {
            assert!(f[0].message.contains(s), "{s:?} in {}", f[0].message);
        }
        assert!(f[0].own_remedy().is_some());
    }

    #[test]
    fn enough_bulk_capacitance_keeps_the_valley_above_the_minimum() {
        let f = lint(&supply("12VAC", &["4700u"], true));
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn a_dc_input_bridge_with_no_stated_ac_voltage_is_silent() {
        let f = lint(&supply("POLARITY_IN", &["220u"], true));
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn with_no_known_load_the_check_makes_no_claim() {
        // Only the LDO's own 5 mA: 17.5 V peak across 220 uF holds well above 9 V.
        let f = lint(&supply("12VAC", &["220u"], false));
        assert!(f.is_empty(), "{f:?}");
    }

    /// The `supply` bridge with its AC side on a 2-pin connector `J1` of
    /// `connector` value, and no AC-named net: the board never says the voltage.
    fn connector_supply(connector: &str, bulk: &[&str], load: bool) -> ExtractedBoard {
        let mut b = supply("Net-(J1-Pin_1)", bulk, load);
        b.nets[4].name = "Net-(J1-Pin_2)".into();
        b.components
            .push(part("J1", connector, &[("1", 4), ("2", 5)]));
        b
    }

    #[test]
    fn a_connector_fed_bridge_with_no_stated_voltage_is_unverified() {
        let f = lint(&connector_supply("Conn_01x02", &["330u"], true));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].check, LintCheck::HoldUpDroop);
        assert_eq!(f[0].severity, Severity::Medium);
        for s in [
            "hold-up unverified: state the AC input voltage",
            "D1",
            "J1",
            "330 µF",
            "U1",
            "9 V",
            "RMS",
            "ignore this",
        ] {
            assert!(f[0].message.contains(s), "{s:?} in {}", f[0].message);
        }
        assert!(f[0].own_remedy().is_some());
        // The same bridge with the voltage named is judged, not "unverified".
        let mut named = connector_supply("Conn_01x02", &["330u"], true);
        named.nets[3].name = "9VAC".into();
        let g = lint(&named);
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!(g[0].severity, Severity::High);
        assert!(!g[0].message.contains("unverified"), "{}", g[0].message);
        // And with a stated voltage a big bulk capacitor is silent.
        named.components.retain(|c| c.reference != "C1");
        named
            .components
            .push(part("C1", "4700u", &[("1", 3), ("2", 1)]));
        assert!(lint(&named).is_empty());
    }

    #[test]
    fn the_connector_must_not_read_as_a_dc_source() {
        for jack in ["DC Barrel Jack 2.1mm", "Battery", "USB_B", "Solar panel"] {
            let f = lint(&connector_supply(jack, &["330u"], true));
            assert!(f.is_empty(), "{jack}: {f:?}");
        }
        // A DC-named net on the way withdraws the evidence too.
        let mut dc = connector_supply("Conn_01x02", &["330u"], true);
        dc.nets[3].name = "VIN_RAW".into();
        assert!(lint(&dc).is_empty());
    }

    #[test]
    fn a_fuse_between_the_connector_and_the_bridge_still_counts() {
        let mut b = connector_supply("Conn_01x02", &["330u"], true);
        b.nets.push(hauksbee_extract::Net::new(6, "Net-(F1-Pad2)"));
        // J1 pin 1 now reaches the bridge through F1.
        let j1 = b.components.iter_mut().find(|c| c.reference == "J1");
        j1.unwrap().pins[0].net = Some(6);
        b.components.push(part("F1", "1A", &[("1", 6), ("2", 4)]));
        let f = lint(&b);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].severity, Severity::Medium);
    }

    #[test]
    fn an_unverified_bridge_needs_a_connector_and_a_known_load() {
        // No connector, no named voltage: a DC-side bridge, silent.
        assert!(lint(&supply("Net-(J1-Pin_1)", &["330u"], true)).is_empty());
        // A connector but no known load downstream: no claim.
        assert!(lint(&connector_supply("Conn_01x02", &["330u"], false)).is_empty());
    }

    #[test]
    fn a_bridge_package_is_found_by_its_pin_names() {
        let mut bridge = part("BR1", "DF04M", &[("1", 4), ("2", 5), ("3", 3), ("4", 1)]);
        bridge.pins = vec![
            pin_fn("1", 4, "~"),
            pin_fn("2", 5, "~"),
            pin_fn("3", 3, "+"),
            pin_fn("4", 1, "-"),
        ];
        let b = board(
            &[(1, "GND"), (2, "+5V"), (3, "RAW"), (4, "12VAC"), (5, "AC2")],
            vec![
                bridge,
                part("U1", "HOLDLDO", &[("1", 1), ("2", 2), ("3", 3)]),
                part("C1", "220u", &[("1", 3), ("2", 1)]),
                part("U2", "HOLDLOAD", &[("1", 1), ("2", 2)]),
            ],
        );
        let f = lint(&b);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(f[0].message.contains("BR1"), "{}", f[0].message);
    }
}
