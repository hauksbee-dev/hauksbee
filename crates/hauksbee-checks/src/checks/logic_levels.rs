//! Undefined logic levels at a CMOS input.
//!
//! A logic input reads a voltage between its VIL and VIH as neither low nor
//! high: the datasheet guarantees no state there, and on a real part the input
//! may switch at any point in the band, oscillate, or draw shoot-through
//! current. Both edges SCALE WITH THE PART'S OWN SUPPLY on most CMOS parts (a
//! 74LVC control input has VIH = 0.7 x VCC: 2.31 V on 3.3 V, 3.5 V on 5 V), so
//! the band is read from the card's `[models.levels]` tables at the solved
//! level of the supply pin the input is referenced to, falling back to the
//! card's flat `vil`/`vih` corner.
//!
//! The inputs judged are the ones a card DECLARES: an analog switch's control
//! roles, the `inputs` of a `[models.logic]` block, and the bidirectional pads
//! its `sense` map samples (a translator port reads as well as drives). A part
//! with none abstains, as does an input whose net or supply the oracle cannot answer for
//! and a part whose supply is not up.
//!
//! On a run the oracle gives each net the span it covered, so three shapes
//! fire: a level that sits in the band throughout, one that rises off a valid
//! low but peaks inside the band without ever reaching VIH (a pulse too small
//! to register), and one that never falls from a valid high to VIL.
//! Crossing the band on an edge is not a finding. Each is MEASURED (the
//! datasheet guarantees no logic state), so it undermines an assertion as an
//! operating-limit violation.
use hauksbee_extract::{
    Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity,
};
use hauksbee_ir::evidence::Assumption;
use hauksbee_models::{ComponentKind, ModelEntry, ModelLibrary};

use super::analog_inputs::Flag;
use super::envelope::RailRange;
use hauksbee_ir::units::{format_series, format_volts};

/// Below this the part's supply is not up, and its inputs are not judged.
const POWERED_V: f64 = 1.0;

/// A level within this of a band edge counts as on the edge (solver tolerance).
const EDGE_V: f64 = 1e-3;

/// The roles of `m` that are logic inputs: an analog switch's control pins, or
/// the declared inputs of a `[models.logic]` block that are real pads, and the
/// pads its `sense` map samples.
pub(super) fn input_roles(m: &ModelEntry) -> Vec<&str> {
    let is_pad = |r: &str| m.pins.values().any(|p| p == r);
    let ctrl = |r: &&String| {
        let n = r.strip_prefix("ctrl_").map_or(r.as_str(), |_| "ctrl");
        matches!(n, "ctrl" | "sel")
    };
    let mut roles: Vec<&str> = match m.kind {
        ComponentKind::AnalogSwitch => m.pins.values().filter(ctrl).map(String::as_str).collect(),
        _ => (m.logic.inputs.iter().chain(m.logic.sense.values()))
            .map(String::as_str)
            .filter(|r| is_pad(r))
            .collect(),
    };
    roles.sort_unstable();
    roles.dedup();
    roles
}

/// Where a run's span sits against the band, if inside it: the words for it.
fn undefined(span: RailRange, (vil, vih): (f64, f64)) -> Option<String> {
    let inside = |v: f64| v > vil + EDGE_V && v < vih - EDGE_V;
    let (lo, hi) = (span.min_v, span.max_v);
    if (hi - lo).abs() < EDGE_V {
        inside(hi).then(|| format!("sits at {}", format_volts(hi)))
    } else if inside(lo) && inside(hi) {
        Some(format!(
            "stays between {}",
            format_series(&[lo, hi], " and ", "V")
        ))
    } else if inside(hi) {
        Some(format!(
            "peaks at only {} and never reaches VIH",
            format_volts(hi)
        ))
    } else {
        inside(lo).then(|| {
            format!(
                "bottoms at only {} and never falls to VIL",
                format_volts(lo)
            )
        })
    }
}

/// One declared logic input of a powered part: its role, net, supply rail,
/// the rail's level and the VIL/VIH band the card gives at that level.
struct Input<'a> {
    role: &'a str,
    net: String,
    rail: String,
    vcc: f64,
    band: (f64, f64),
}

/// The declared inputs of `c` whose net and supply resolve and whose supply
/// (read by `rail_v`) is up.
fn powered_inputs<'a>(
    board: &ExtractedBoard,
    c: &Component,
    m: &'a ModelEntry,
    rail_v: &dyn Fn(&str) -> Option<f64>,
) -> Vec<Input<'a>> {
    let net_of = |role: &str| {
        let id = super::envelope::role_net_id(c, m, role)?;
        board.net(id).map(|n| n.name.clone())
    };
    let input = |role: &'a str| {
        let rail = net_of(m.level_rail_of(role)?)?;
        let net = net_of(role)?;
        let vcc = rail_v(&rail).filter(|v| *v >= POWERED_V)?;
        let band = m.input_band_at(vcc)?;
        Some(Input {
            role,
            net,
            rail,
            vcc,
            band,
        })
    };
    input_roles(m).into_iter().filter_map(input).collect()
}

/// One logic input wired to a net, with the band its part guarantees at the
/// level its own supply sits at.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicLoad {
    pub reference: String,
    pub role: String,
    pub vil: f64,
    pub vih: f64,
    pub vcc: f64,
    /// The input's high-level leakage (A, `iih_a` on the card), when the card
    /// states it.
    pub leak_a: Option<f64>,
}

/// Every declared logic input on `net`, with its VIL/VIH at its own supply.
///
/// An input is an analog switch's control, a gate's input, and so on; its band
/// is read at the level `rail_v` gives its supply. Together they are the
/// levels a signal on `net` must reach for every part it drives to read it.
/// An input whose supply is not up is not a load yet and is left out.
pub fn loads_on_net(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    net: &str,
    rail_v: &dyn Fn(&str) -> Option<f64>,
) -> Vec<LogicLoad> {
    let on_net = |(c, m): (&Component, ModelEntry)| {
        let load = |i: Input<'_>| LogicLoad {
            reference: c.reference.clone(),
            role: i.role.to_string(),
            vil: i.band.0,
            vih: i.band.1,
            vcc: i.vcc,
            leak_a: super::plausible_param(&m, "iih_a").filter(|a| *a >= 0.0),
        };
        let inputs = powered_inputs(board, c, &m, rail_v).into_iter();
        inputs
            .filter(|i| i.net == net)
            .map(load)
            .collect::<Vec<_>>()
    };
    super::resolved_components(board, lib)
        .flat_map(on_net)
        .collect()
}

/// The declared logic inputs on `net` whose band cannot be read: the net is
/// theirs, but the card gives no VIL/VIH for their supply or the supply is not
/// resolved. `(reference, role)` pairs; the inputs [`loads_on_net`] leaves out
/// because their limits are unknown, not because their supply is off.
pub(crate) fn unjudged_inputs_on_net(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    net: &str,
    rail_v: &dyn Fn(&str) -> Option<f64>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (c, m) in super::resolved_components(board, lib) {
        let net_of = |role: &str| {
            let id = super::envelope::role_net_id(c, &m, role)?;
            board.net(id).map(|n| n.name.clone())
        };
        for role in input_roles(&m) {
            if net_of(role).as_deref() != Some(net) {
                continue;
            }
            let supply_v = m
                .level_rail_of(role)
                .and_then(&net_of)
                .and_then(|rail| rail_v(&rail));
            // Off is not unknown: an unpowered part has no input to judge.
            if supply_v.is_some_and(|v| v < POWERED_V) {
                continue;
            }
            if supply_v.is_none_or(|v| m.input_band_at(v).is_none()) {
                out.push((c.reference.clone(), role.to_string()));
            }
        }
    }
    out
}

/// One part's undefined inputs.
fn part_flags(
    board: &ExtractedBoard,
    c: &Component,
    m: &ModelEntry,
    span: &dyn Fn(&str) -> Option<RailRange>,
) -> Vec<Flag> {
    let rail_v = |rail: &str| span(rail).map(|r| r.max_v);
    let mut out = Vec::new();
    for Input {
        role,
        net,
        rail,
        vcc,
        band: (vil, vih),
    } in powered_inputs(board, c, m, &rail_v)
    {
        let Some(level) = span(&net) else {
            continue;
        };
        let Some(how) = undefined(level, (vil, vih)) else {
            continue;
        };
        let r = &c.reference;
        // A pad the part also drives: a pull or load can fight that drive.
        let own = (m.logic.outputs.iter().any(|o| o == role))
            .then(|| m.params.get_f64("ro"))
            .flatten()
            .map(|ro| format!(" {r} also drives this pad through {ro:.0} Ohm; while it does, a pull-up, pull-down or load on the net that fights that drive holds the pad here, so keep any such resistor far above {ro:.0} Ohm or drive the net harder."))
            .unwrap_or_default();
        let message = format!( "undefined logic level at {r} {role} (net '{net}'): it {how}, between VIL {} and VIH {} for VCC = {} on '{rail}', so the part's datasheet guarantees no logic state there and it may read either way, toggle, or draw shoot-through current. Drive the input to a valid level for this supply (a stronger or level-shifted drive, a lower supply for the part, or a part with lower thresholds); the band is the card's {} read at VCC.{own}", format_volts(vil), format_volts(vih), format_volts(vcc), if m.levels.contains_key("vih") { "VCC-scaled [models.levels] table" } else { "vil/vih corner" });
        let finding = LintFinding {
            check: LintCheck::OperatingEnvelope,
            severity: Severity::High,
            message,
            refs: vec![r.clone()],
            nets: vec![net, rail],
        };
        out.push((finding, true));
    }
    out
}

fn flags(
    board: &ExtractedBoard,
    st: &super::BoardStatics<'_>,
    span: &dyn Fn(&str) -> Option<RailRange>,
) -> Vec<Flag> {
    let each = st.parts(board);
    each.flat_map(|(c, m)| part_flags(board, c, m, span))
        .collect()
}

/// The lint surface: inputs whose `span` (a solved run's, or a static rail
/// table's single level) leaves them between VIL and VIH.
pub fn logic_level_lint(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    span: &dyn Fn(&str) -> Option<RailRange>,
) -> NetLintReport {
    let st = super::BoardStatics::of(board, lib);
    logic_level_lint_in(board, &st, span)
}

/// [`logic_level_lint`] over statics the caller already holds.
pub fn logic_level_lint_in(
    board: &ExtractedBoard,
    st: &super::BoardStatics<'_>,
    span: &dyn Fn(&str) -> Option<RailRange>,
) -> NetLintReport {
    let findings = flags(board, st, span).into_iter().map(|(f, _)| f);
    NetLintReport {
        findings: findings.collect(),
    }
}

/// The same findings as run evidence for the CI report, each a measured
/// operating-limit violation.
pub fn run_assumptions(
    board: &ExtractedBoard,
    st: &super::BoardStatics<'_>,
    span: &dyn Fn(&str) -> Option<RailRange>,
) -> Vec<Assumption> {
    let fix = "drive the input to a valid level for the part's supply, then re-run";
    super::analog_inputs::as_evidence(flags(board, st, span), "logic-level", fix)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
    use super::super::BoardStatics;
    use super::*;
    use hauksbee_ir::evidence::AssumptionKind;
    use std::collections::HashMap;

    /// A 74LVC1G66 (pads: 1 A, 2 B, 3 GND, 4 C, 5 VCC) on rail `vcc` with its
    /// control at `ctrl`, as DC points: the findings on both surfaces.
    fn one_switch(vcc: f64, ctrl: f64) -> (NetLintReport, Vec<Assumption>) {
        let nets = &[(1, "GND"), (2, "VSW"), (3, "CTRL"), (4, "A"), (5, "B")];
        let pads = &[("1", 4), ("2", 5), ("3", 1), ("4", 3), ("5", 2)];
        let b = board(nets, vec![part("U1", "SN74LVC1G66DBVR", pads)]);
        let volts = HashMap::from([("VSW", vcc), ("CTRL", ctrl), ("GND", 0.0)]);
        let point = |v: f64| RailRange { min_v: v, max_v: v };
        let span = |net: &str| volts.get(net).copied().map(point);
        let lib = ModelLibrary::builtin();
        (
            logic_level_lint(&b, &lib, &span),
            run_assumptions(&b, &BoardStatics::of(&b, &lib), &span),
        )
    }

    #[test]
    fn a_5v_1g66_control_at_2v3_is_undefined_and_4v5_or_a_3v3_rail_is_not() {
        let (lint, evidence) = one_switch(5.0, 2.3);
        assert_eq!(lint.findings.len(), 1, "{lint:?}");
        let m = &lint.findings[0].message;
        for says in [
            "U1 ctrl",
            "'CTRL'",
            "sits at 2.3 V",
            "VIL 1.5 V",
            "VIH 3.5 V",
            "VCC = 5 V",
        ] {
            assert!(m.contains(says), "{says:?} in {m}");
        }
        let kinds: Vec<_> = evidence.iter().map(Assumption::kind).collect();
        assert_eq!(kinds, [AssumptionKind::OperatingLimitViolated]);
        for (vcc, ctrl) in [(5.0, 4.5), (3.3, 2.4), (5.0, 1.4), (5.0, 0.0)] {
            let (lint, evidence) = one_switch(vcc, ctrl);
            assert!(
                lint.findings.is_empty() && evidence.is_empty(),
                "{vcc}/{ctrl}: {lint:?}"
            );
        }
        // An unpowered part's inputs are not judged.
        assert!(one_switch(0.0, 0.5).0.findings.is_empty());
    }

    /// A TXB0101 (1 VCCA, 2 GND, 3 A, 4 B, 5 OE, 6 VCCB) translating 1.8 V to
    /// 3.3 V with its B port at `b`: the findings on the B port.
    fn txb_b_port(b: f64) -> Vec<LintFinding> {
        let nets = &[(1, "GND"), (2, "VA"), (3, "VB"), (4, "A"), (5, "B")];
        let pads = &[("1", 2), ("2", 1), ("3", 4), ("4", 5), ("5", 2), ("6", 3)];
        let board = board(nets, vec![part("U1", "TXB0101DCKR", pads)]);
        let volts = HashMap::from([("VA", 1.8), ("VB", 3.3), ("A", 0.0), ("B", b), ("GND", 0.0)]);
        let span = |net: &str| volts.get(net).map(|&v| RailRange { min_v: v, max_v: v });
        logic_level_lint(&board, &ModelLibrary::builtin(), &span).findings
    }

    #[test]
    fn a_txb_port_pulled_against_its_weak_driver_is_undefined() {
        // SCES639F 7.2.2 Eq. (2): A low, a 4.7 kOhm pull-up to 3.3 V holds B at
        // 3.3 x 4.5k / (4.5k + 4.7k) = 1.614 V, inside 0.35/0.65 x 3.3 V.
        let found = txb_b_port(3.3 * 4.5 / 9.2);
        assert_eq!(found.len(), 1, "{found:?}");
        let m = &found[0].message;
        for says in ["U1 b", "'B'", "VIL 1.155 V", "VIH 2.145 V", "4500 Ohm"] {
            assert!(m.contains(says), "{says:?} in {m}");
        }
        // The datasheet's 50 kOhm floor leaves 0.273 V: a valid low.
        assert!(txb_b_port(3.3 * 4.5 / 54.5).is_empty());
        assert!(txb_b_port(3.3).is_empty());
    }

    #[test]
    fn a_span_is_judged_by_where_it_turns_not_by_its_edges() {
        let band = (1.5, 3.5);
        let span = |min_v, max_v| RailRange { min_v, max_v };
        assert!(undefined(span(0.0, 5.0), band).is_none(), "a full swing");
        assert!(undefined(span(0.0, 1.5), band).is_none(), "a valid low");
        let peak = undefined(span(1.5, 2.35), band).expect("a sawtooth below VIH");
        assert!(peak.contains("peaks at only 2.35 V"), "{peak}");
        assert!(undefined(span(2.0, 2.5), band).is_some_and(|s| s.contains("stays")));
        assert!(undefined(span(2.0, 5.0), band).is_some_and(|s| s.contains("never falls")));
    }
}
