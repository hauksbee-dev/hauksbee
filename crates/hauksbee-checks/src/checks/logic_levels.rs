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
//! roles and the `inputs` of a `[models.logic]` block. A part with neither
//! abstains, as does an input whose net or supply the oracle cannot answer for
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

/// Below this the part's supply is not up, and its inputs are not judged.
const POWERED_V: f64 = 1.0;

/// A level within this of a band edge counts as on the edge (solver tolerance).
const EDGE_V: f64 = 1e-3;

/// The roles of `m` that are logic inputs: an analog switch's control pins, or
/// the declared inputs of a `[models.logic]` block that are real pads.
fn input_roles(m: &ModelEntry) -> Vec<&str> {
    let is_pad = |r: &str| m.pins.values().any(|p| p == r);
    let ctrl = |r: &&String| {
        let n = r.strip_prefix("ctrl_").map_or(r.as_str(), |_| "ctrl");
        matches!(n, "ctrl" | "sel")
    };
    let mut roles: Vec<&str> = match m.kind {
        ComponentKind::AnalogSwitch => m.pins.values().filter(ctrl).map(String::as_str).collect(),
        _ => (m.logic.inputs.iter().map(String::as_str))
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
        inside(hi).then(|| format!("sits at {hi:.3} V"))
    } else if inside(lo) && inside(hi) {
        Some(format!("stays between {lo:.3} and {hi:.3} V"))
    } else if inside(hi) {
        Some(format!("peaks at only {hi:.3} V and never reaches VIH"))
    } else {
        inside(lo).then(|| format!("bottoms at only {lo:.3} V and never falls to VIL"))
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
        let message = format!("undefined logic level at {r} {role} (net '{net}'): it {how}, between VIL {vil:.3} V and VIH {vih:.3} V for VCC = {vcc:.3} V on '{rail}', so the part's datasheet guarantees no logic state there and it may read either way, toggle, or draw shoot-through current. Drive the input to a valid level for this supply (a stronger or level-shifted drive, a lower supply for the part, or a part with lower thresholds); the band is the card's {} read at VCC.", if m.levels.contains_key("vih") { "VCC-scaled [models.levels] table" } else { "vil/vih corner" });
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
    lib: &ModelLibrary,
    span: &dyn Fn(&str) -> Option<RailRange>,
) -> Vec<Flag> {
    let each = super::resolved_components(board, lib);
    each.flat_map(|(c, m)| part_flags(board, c, &m, span))
        .collect()
}

/// The lint surface: inputs whose `span` (a solved run's, or a static rail
/// table's single level) leaves them between VIL and VIH.
pub fn logic_level_lint(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    span: &dyn Fn(&str) -> Option<RailRange>,
) -> NetLintReport {
    let findings = flags(board, lib, span).into_iter().map(|(f, _)| f);
    NetLintReport {
        findings: findings.collect(),
    }
}

/// The same findings as run evidence for the CI report, each a measured
/// operating-limit violation.
pub fn run_assumptions(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    span: &dyn Fn(&str) -> Option<RailRange>,
) -> Vec<Assumption> {
    let fix = "drive the input to a valid level for the part's supply, then re-run";
    super::analog_inputs::as_evidence(flags(board, lib, span), "logic-level", fix)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{board, part};
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
            run_assumptions(&b, &lib, &span),
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
            "sits at 2.300 V",
            "VIL 1.500 V",
            "VIH 3.500 V",
            "VCC = 5.000 V",
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

    #[test]
    fn a_span_is_judged_by_where_it_turns_not_by_its_edges() {
        let band = (1.5, 3.5);
        let span = |min_v, max_v| RailRange { min_v, max_v };
        assert!(undefined(span(0.0, 5.0), band).is_none(), "a full swing");
        assert!(undefined(span(0.0, 1.5), band).is_none(), "a valid low");
        let peak = undefined(span(1.5, 2.35), band).expect("a sawtooth below VIH");
        assert!(peak.contains("peaks at only 2.350 V"), "{peak}");
        assert!(undefined(span(2.0, 2.5), band).is_some_and(|s| s.contains("stays")));
        assert!(undefined(span(2.0, 5.0), band).is_some_and(|s| s.contains("never falls")));
    }
}
