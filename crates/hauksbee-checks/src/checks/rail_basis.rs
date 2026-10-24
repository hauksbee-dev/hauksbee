//! Rails the analysis powers on an assumption: the verdict states it, and a
//! rail only an unfitted part would power is a gating finding.
//!
//! With no supply declared, a rail no modelled part drives takes an ideal
//! source at the voltage its name states, so its loads are judged as if
//! powered (`hauksbee_bind::rail_basis` says what that rests on). Three
//! outcomes follow, each generic over boards and parts:
//! * a rail only do-not-populate parts would power (its sole regulator, or the
//!   ferrite or 0 Ω link to a powered rail), with no connector, test point or
//!   battery to bring it in and no other fitted part that may source it, never
//!   comes up: a High `rail_unpowered` finding naming the unfitted part, the
//!   rail and the parts left unpowered, with the fix;
//! * the same rail with an off-board entry, or a fitted part that may source
//!   it, is a non-gating note naming the unfitted part, and the pass is
//!   conditional on the entry;
//! * a name-powered rail with nothing at all behind it (no entry, no fitted or
//!   unfitted source, no series part bringing it in from a net that has one)
//!   and parts drawing supply from it is a Medium `rail_unpowered` finding
//!   naming the active ICs that rely on it first: the rail is a name on a net,
//!   not a supply, and a chip drawing from it is a real fault (only on a board
//!   that takes power in somewhere else; a fragment with no way in says nothing
//!   about its supply). It gates. A board that really is fed another way (pads,
//!   spring contacts or pogo pins the design does not draw) overrules it with a
//!   waiver for that rail, which the message quotes with the rail's name filled
//!   in; the waived finding stays listed, never hidden;
//! * every other name-powered rail is a verdict condition: assumed supplied
//!   from off the board through the named connector, or held at its named
//!   voltage though nothing modelled sources it. Never silent.
//!
//! A run that powers no rail at all judged no level, rating or rail check, so
//! it says so in the not-judged vocabulary and the verdict reads PARTLY
//! JUDGED, never a bare pass.
//!

use hauksbee_bind::binder::BoundBoard;
use hauksbee_bind::rail_basis::{rail_bases, BareRail, RailBases, RailBasis, Unfitted};
use hauksbee_extract::{
    ExtractedBoard, LintCheck, LintFinding, NetLintReport, Severity, INFORMATIONAL_MARKER,
    REMEDY_MARKER,
};
use hauksbee_ir::units::format_volts;
use hauksbee_models::ModelLibrary;

/// How many parts a message lists before it counts the rest.
const SHOWN: usize = 4;

/// The first few references, then a count of the rest.
fn listed(refs: &[String]) -> String {
    match refs.split_at_checked(SHOWN) {
        Some((head, rest)) if !rest.is_empty() => {
            format!("{} and {} more", head.join(", "), rest.len())
        }
        _ => refs.join(", "),
    }
}

/// What is not fitted, one clause per part.
fn unfitted_clause(unfitted: &[Unfitted]) -> String {
    let one = |u: &Unfitted| {
        let value = u.value.trim();
        let part = if value.is_empty() {
            u.reference.clone()
        } else {
            format!("{} ({value})", u.reference)
        };
        u.link_to.as_ref().map_or_else(
            || format!("{part}, which would source it, is not fitted"),
            |to| format!("{part}, the link that joins it to '{to}', is not fitted"),
        )
    };
    unfitted.iter().map(one).collect::<Vec<_>>().join("; ")
}

fn volts_clause(rail: &RailBasis) -> String {
    rail.volts.map_or_else(
        || "The analysis leaves it unpowered".to_string(),
        |v| {
            format!(
                "The analysis held it at {} by its name alone",
                format_volts(v)
            )
        },
    )
}

/// The gating finding for a rail that never comes up.
fn never_up_finding(rail: &RailBasis) -> LintFinding {
    let net = rail.net();
    let also = match &rail.nets[1.min(rail.nets.len())..] {
        [] => String::new(),
        rest => format!(
            " (with {})",
            rest.iter()
                .map(|n| format!("'{n}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let fit: Vec<&str> = rail.unfitted.iter().map(|u| u.reference.as_str()).collect();
    let stays = if rail.loads.len() == 1 {
        "stays"
    } else {
        "stay"
    };
    let message = format!(
        "'{net}'{also} never comes up: {}, and nothing else on the board can power it (no fitted \
         regulator or source, connector, test point or battery on it), so {} {stays} unpowered. \
         {}, so no result for the parts on it stands. {REMEDY_MARKER}Fit {}, or bring '{net}' in \
         on a connector or test point.",
        unfitted_clause(&rail.unfitted),
        listed(&rail.loads),
        volts_clause(rail),
        fit.join(" and "),
    );
    let mut refs: Vec<String> = fit.iter().map(|r| (*r).to_string()).collect();
    refs.extend(rail.loads.iter().take(SHOWN).cloned());
    LintFinding {
        check: LintCheck::RailUnpowered,
        severity: Severity::High,
        message,
        refs,
        nets: rail.nets.clone(),
    }
}

/// A string as a TOML basic string.
fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The waiver that overrules a finding on `net`, as the one line to paste into
/// `hauksbee-waivers.toml` (see `hauksbee_bind::waiver`: a reason and an expiry
/// are required). `reason` and `until` are placeholders to fill in.
fn waiver_line(net: &str) -> String {
    let net = toml_str(net);
    format!(
        "waive = [{{ check = \"lint\", kind = \"rail_unpowered\", nets = [{net}], reason = \
         \"<how this rail is fed>\", until = \"YYYY-MM-DD\" }}] (put the date it is next \
         reviewed in place of YYYY-MM-DD)."
    )
}

/// The finding for a rail held at its name's voltage with no source behind it.
fn unsourced_finding(rail: &RailBasis) -> LintFinding {
    let net = rail.net();
    let volts = rail.volts.map_or_else(String::new, format_volts);
    let drawn = if rail.active_loads.is_empty() {
        &rail.supply_loads
    } else {
        &rail.active_loads
    };
    let stays = if drawn.len() == 1 { "relies" } else { "rely" };
    let message = format!(
        "'{net}' is named for {volts} but nothing on the board brings it in: no connector, test \
         point or battery on it, no regulator or other part that can drive it, and no fuse, \
         diode, switch or link to a net that has one. The analysis held it at {volts} by its \
         name alone, so the result for {} {stays} on a supply that may not exist. \
         {REMEDY_MARKER}Bring '{net}' in on a connector or test point, or add the regulator or \
         link that sources it (and fit it). If the board really is fed another way (pads, spring \
         contacts or pogo pins the design does not draw), overrule this finding in \
         hauksbee-waivers.toml beside the board with {}",
        listed(drawn),
        waiver_line(net),
    );
    LintFinding {
        check: LintCheck::RailUnpowered,
        severity: Severity::Medium,
        message,
        refs: drawn.iter().take(SHOWN).cloned().collect(),
        nets: rail.nets.clone(),
    }
}

/// The note for an unfitted source on a rail that may still be powered.
fn unfitted_note(rail: &RailBasis) -> LintFinding {
    let net = rail.net();
    let how = if rail.entries.is_empty() {
        format!(
            "{} may source it, though the analysis models none of them as a source",
            listed(&rail.feeds)
        )
    } else {
        format!(
            "It can come in from off the board through {}",
            listed(&rail.entries)
        )
    };
    let held = rail.volts.map_or_else(String::new, |v| {
        format!(", and the run assumes it is held at {}", format_volts(v))
    });
    let message = format!(
        "'{net}' has no fitted source of its own: {}. {how}{held}{INFORMATIONAL_MARKER}.",
        unfitted_clause(&rail.unfitted),
    );
    let mut refs: Vec<String> = rail.unfitted.iter().map(|u| u.reference.clone()).collect();
    refs.extend(rail.entries.iter().chain(&rail.feeds).take(SHOWN).cloned());
    LintFinding {
        check: LintCheck::RailUnpowered,
        severity: Severity::Low,
        message,
        refs,
        nets: rail.nets.clone(),
    }
}

/// The note for a run that powered no rail at all.
fn no_rail_note(bases: &RailBases) -> LintFinding {
    let what = format!(
        "no rail on the board is powered in the analysis (no net's name states its voltage, such \
         as +3V3 or 5V, and no modelled part sources one), so the levels, ratings and rails of {} \
         were not checked",
        listed(&bases.active_parts)
    );
    LintFinding {
        check: LintCheck::RailUnpowered,
        severity: Severity::Low,
        message: format!(
            "{}. {REMEDY_MARKER}Name each supply net with its voltage (+3V3, +5V), or declare it \
             with a [[supply]], then re-check.",
            hauksbee_models::rating_basis::not_judged(&what)
        ),
        refs: bases.active_parts.iter().take(SHOWN).cloned().collect(),
        nets: Vec::new(),
    }
}

/// The note for a rail named with no voltage whose consumers rule out the level
/// the name is conventionally taken for.
fn bare_rail_note(bare: &BareRail) -> LintFinding {
    let net = &bare.net;
    let guess = format_volts(bare.conventional);
    let (lo, hi) = (bare.low_v, bare.high_v);
    let state = if lo > hi + 1e-6 {
        format!(
            "no single supply satisfies every consumer ({} needs at least {}, {} allows at most {})",
            listed(&bare.low_from),
            format_volts(lo),
            listed(&bare.high_from),
            format_volts(hi),
        )
    } else if (hi - lo).abs() <= 1e-6 {
        format!(
            "its consumers allow {} only ({} minimum, {} maximum), with no margin",
            format_volts(lo),
            listed(&bare.low_from),
            listed(&bare.high_from),
        )
    } else {
        format!(
            "its consumers allow {} to {} ({} minimum, {} maximum)",
            format_volts(lo),
            format_volts(hi),
            listed(&bare.low_from),
            listed(&bare.high_from),
        )
    };
    let message = format!(
        "'{net}' states no voltage, and {state}, which rules out the {guess} a bare name is \
         conventionally taken for. The analysis holds no level on '{net}', so the parts on it \
         ({}) are not judged against a supply level{INFORMATIONAL_MARKER}. {REMEDY_MARKER}Name \
         the rail with its voltage (+3V3) or declare it with a [[supply]], then re-check.",
        listed(&bare.consumers),
    );
    let mut refs = bare.consumers.clone();
    refs.truncate(SHOWN);
    LintFinding {
        check: LintCheck::RailUnpowered,
        severity: Severity::Low,
        message,
        refs,
        nets: vec![bare.net.clone()],
    }
}

/// The findings for `bases`: a gating finding per rail that never comes up, a
/// note per rail whose own source is unfitted but may still be powered, and
/// the not-judged note when no rail is powered at all.
#[must_use]
pub fn rail_basis_findings(bases: &RailBases) -> NetLintReport {
    let mut findings = Vec::new();
    for rail in &bases.rails {
        if rail.never_up() {
            findings.push(never_up_finding(rail));
        } else if !rail.unfitted.is_empty() && rail.volts.is_some() {
            findings.push(unfitted_note(rail));
        } else if rail.unsourced() {
            findings.push(unsourced_finding(rail));
        }
    }
    findings.extend(bases.unleveled.iter().map(bare_rail_note));
    if !bases.any_powered && !bases.active_parts.is_empty() {
        findings.push(no_rail_note(bases));
    }
    NetLintReport { findings }
}

/// [`rail_basis_findings`] over `board` bound on its own.
#[must_use]
pub fn rail_basis_lint(board: &ExtractedBoard, lib: &ModelLibrary) -> NetLintReport {
    let bound = hauksbee_bind::binder::bind_board(board, lib);
    rail_basis_findings(&rail_bases(board, lib, &bound))
}

/// The conditions a pass rests on for the rails powered by name alone.
///
/// Two sentences: the rails assumed supplied from off the board through a
/// connector, test point or battery, and those held at their named voltage
/// though nothing the analysis models sources them. A rail that never comes up is a finding,
/// not a condition, and a rail no fitted part draws from or may drive states
/// none.
#[must_use]
pub fn rail_conditions(
    board: &ExtractedBoard,
    lib: &ModelLibrary,
    bound: &BoundBoard,
) -> [Option<String>; 2] {
    let bases = rail_bases(board, lib, bound);
    let mut entered = Vec::new();
    let mut held = Vec::new();
    // A rail nothing fitted draws from or may drive (an unused header pin, a
    // net with only capacitors) carries no result worth a condition.
    // One already reported as a finding (it never comes up, or has no source at
    // all) is not repeated as a condition.
    let matters = |r: &&RailBasis| {
        !(r.never_up() || r.unsourced() || (r.loads.is_empty() && r.feeds.is_empty()))
    };
    for rail in bases.rails.iter().filter(matters) {
        let Some(v) = rail.volts else { continue };
        let own = if rail.unfitted.is_empty() {
            String::new()
        } else {
            let refs: Vec<&str> = rail.unfitted.iter().map(|u| u.reference.as_str()).collect();
            format!(", its own source {} not fitted", refs.join(", "))
        };
        if rail.entries.is_empty() {
            let may = if rail.feeds.is_empty() {
                String::new()
            } else {
                format!(" (possible source: {})", listed(&rail.feeds))
            };
            held.push(format!("'{}' at {}{may}{own}", rail.net(), format_volts(v)));
        } else {
            entered.push(format!(
                "'{}' at {} through {}{own}",
                rail.net(),
                format_volts(v),
                listed(&rail.entries)
            ));
        }
    }
    let sentence = |items: &[String], lead: &str| {
        (!items.is_empty()).then(|| format!("{lead}: {}.", items.join("; ")))
    };
    [
        sentence(
            &entered,
            "Rails assumed supplied from off the board at their named voltage (the off-board \
             supply itself is not judged)",
        ),
        sentence(
            &held,
            "Rails held at their named voltage though nothing the analysis models on the board \
             sources them",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};
    use hauksbee_extract::Component;

    const VBUS: i64 = 1;
    const V3: i64 = 2;
    const GND: i64 = 3;
    const AV: i64 = 4;
    const SDA: i64 = 5;
    const EXT: i64 = 6;
    const RAW: i64 = 7;

    fn nets() -> Vec<(i64, &'static str)> {
        vec![
            (VBUS, "+5V"),
            (V3, "+3V3"),
            (GND, "GND"),
            (AV, "+3V3A"),
            (SDA, "SDA"),
            (EXT, "VEXT"),
            (RAW, "RAW_IN"),
        ]
    }

    fn dnp(mut c: Component) -> Component {
        c.dnp = true;
        c
    }

    /// AP2112K-3.3 (SOT25: 1 IN, 2 GND, 3 EN, 4 NC, 5 OUT; Diodes DS39724
    /// Rev. 2-2 Pin Descriptions p.2) from +5V to +3V3, EN tied to IN.
    fn ldo() -> Component {
        part(
            "U1",
            "AP2112K-3.3",
            &[("1", VBUS), ("2", GND), ("3", VBUS), ("5", V3)],
        )
    }

    /// A load on +3V3: an unmodelled sensor with a VDD supply pin.
    fn sensor(reference: &str, rail: i64) -> Component {
        let mut c = part(reference, "XYZ123", &[("1", rail), ("2", GND), ("3", SDA)]);
        c.pins[0].function = "VDD".into();
        c.pins[0].kind = "power_in".into();
        c.pins[1].function = "GND".into();
        c.pins[1].kind = "power_in".into();
        c.pins[2].function = "SDA".into();
        c.pins[2].kind = "bidirectional".into();
        c
    }

    fn usb() -> Component {
        part("J1", "USB_B_Micro", &[("1", VBUS), ("5", GND)])
    }

    fn run(parts: Vec<Component>) -> Vec<LintFinding> {
        let lib = ModelLibrary::builtin();
        rail_basis_lint(&board(&nets(), parts), &lib).findings
    }

    fn conditions(parts: Vec<Component>) -> [Option<String>; 2] {
        let lib = ModelLibrary::builtin();
        let b = board(&nets(), parts);
        let bound = hauksbee_bind::binder::bind_board(&b, &lib);
        rail_conditions(&b, &lib, &bound)
    }

    fn gating(found: &[LintFinding]) -> Vec<&LintFinding> {
        found
            .iter()
            .filter(|f| f.check == LintCheck::RailUnpowered && f.severity == Severity::High)
            .collect()
    }

    /// Faulty: the sole regulator of +3V3 is do-not-populate and nothing else
    /// brings +3V3 in, so the sensor on it is never powered.
    #[test]
    fn sole_regulator_dnp_never_comes_up() {
        let found = run(vec![
            usb(),
            dnp(ldo()),
            sensor("U2", V3),
            part("C1", "1u", &[("1", V3), ("2", GND)]),
        ]);
        let g = gating(&found);
        assert_eq!(g.len(), 1, "{found:#?}");
        let m = &g[0].message;
        assert!(m.contains("'+3V3'") && m.contains("never comes up"), "{m}");
        assert!(m.contains("U1 (AP2112K-3.3)") && m.contains("U2"), "{m}");
        assert!(m.contains(REMEDY_MARKER) && m.contains("Fit U1"), "{m}");
        assert!(g[0].refs.contains(&"U1".to_string()), "{:?}", g[0].refs);
    }

    /// Legal control: the regulator fitted. No finding; +5V is a condition
    /// (from J1), +3V3 is not (the regulator holds it).
    #[test]
    fn fitted_regulator_is_silent_and_the_connector_rail_is_a_condition() {
        let parts = vec![usb(), ldo(), sensor("U2", V3)];
        let found = run(parts.clone());
        assert!(
            found.iter().all(|f| f.check != LintCheck::RailUnpowered),
            "{found:#?}"
        );
        let [entered, held] = conditions(parts);
        let entered = entered.expect("the connector rail is a condition");
        assert!(entered.contains("'+5V' at 5 V through J1"), "{entered}");
        assert!(!entered.contains("+3V3"), "{entered}");
        assert!(held.is_none(), "{held:?}");
    }

    /// Legal control: the regulator is do-not-populate but +3V3 has a header
    /// pin, so it may be supplied from off the board: a note and a condition,
    /// never a gating finding.
    #[test]
    fn unfitted_regulator_with_a_header_on_the_rail_is_a_stated_condition() {
        let parts = vec![
            usb(),
            dnp(ldo()),
            sensor("U2", V3),
            part("J2", "Conn_01x02", &[("1", V3), ("2", GND)]),
        ];
        let found = run(parts.clone());
        assert!(gating(&found).is_empty(), "{found:#?}");
        let note = found
            .iter()
            .find(|f| f.check == LintCheck::RailUnpowered)
            .expect("a note names the unfitted source");
        assert_eq!(note.severity, Severity::Low);
        assert!(
            note.message.contains("U1") && note.message.contains("J2"),
            "{}",
            note.message
        );
        let [entered, _] = conditions(parts);
        let entered = entered.expect("condition");
        assert!(
            entered.contains("'+3V3' at 3.3 V through J2, its own source U1 not fitted"),
            "{entered}"
        );
    }

    /// Legal control: an unfitted alternate regulator beside a fitted one
    /// (two footprints, one fitted) leaves the rail driven: silent.
    #[test]
    fn unfitted_alternate_beside_a_fitted_regulator_is_silent() {
        let mut alt = ldo();
        alt.reference = "U9".into();
        let found = run(vec![usb(), ldo(), dnp(alt), sensor("U2", V3)]);
        assert!(
            found.iter().all(|f| f.check != LintCheck::RailUnpowered),
            "{found:#?}"
        );
    }

    /// Faulty: the ferrite joining +3V3A to the regulated +3V3 is not fitted,
    /// so the analog rail is an island.
    #[test]
    fn unfitted_ferrite_cuts_a_rail_island() {
        let found = run(vec![
            usb(),
            ldo(),
            sensor("U2", V3),
            dnp(part("FB1", "600R@100MHz", &[("1", V3), ("2", AV)])),
            sensor("U3", AV),
        ]);
        let g = gating(&found);
        assert_eq!(g.len(), 1, "{found:#?}");
        let m = &g[0].message;
        assert!(m.contains("'+3V3A' never comes up"), "{m}");
        assert!(
            m.contains("FB1 (600R@100MHz), the link that joins it to '+3V3'"),
            "{m}"
        );
        assert!(m.contains("U3"), "{m}");
    }

    /// Faulty: an unfitted inductor link whose value names no part ("DNP",
    /// as many boards mark it) still cuts the island off.
    #[test]
    fn unfitted_inductor_valued_dnp_cuts_a_rail_island() {
        let found = run(vec![
            usb(),
            ldo(),
            sensor("U2", V3),
            dnp(part("L5", "DNP", &[("1", V3), ("2", AV)])),
            sensor("U3", AV),
        ]);
        let g = gating(&found);
        assert_eq!(g.len(), 1, "{found:#?}");
        let m = &g[0].message;
        assert!(m.contains("'+3V3A' never comes up"), "{m}");
        assert!(m.contains("L5"), "{m}");
    }

    /// Faulty: the ferrite from a connector's supply pin (a net whose name
    /// states no voltage) to +3V3A is not fitted: the island never comes up
    /// although the connector side is unpowered in the analysis too.
    #[test]
    fn unfitted_ferrite_from_a_connector_net_cuts_a_rail_island() {
        let found = run(vec![
            part("J3", "Conn_01x02", &[("1", EXT), ("2", GND)]),
            dnp(part("FB1", "600R@100MHz", &[("1", EXT), ("2", AV)])),
            sensor("U3", AV),
        ]);
        let g = gating(&found);
        assert_eq!(g.len(), 1, "{found:#?}");
        assert!(
            g[0].message.contains("joins it to 'VEXT'"),
            "{}",
            g[0].message
        );
    }

    /// Legal control: the same ferrite fitted, the island comes in through J3.
    #[test]
    fn fitted_ferrite_from_a_connector_net_is_not_gating() {
        let found = run(vec![
            part("J3", "Conn_01x02", &[("1", EXT), ("2", GND)]),
            part("FB1", "600R@100MHz", &[("1", EXT), ("2", AV)]),
            sensor("U3", AV),
        ]);
        assert!(gating(&found).is_empty(), "{found:#?}");
    }

    /// Legal control: the same ferrite fitted joins the island to +3V3.
    #[test]
    fn fitted_ferrite_is_silent() {
        let found = run(vec![
            usb(),
            ldo(),
            sensor("U2", V3),
            part("FB1", "600R@100MHz", &[("1", V3), ("2", AV)]),
            sensor("U3", AV),
        ]);
        assert!(
            found.iter().all(|f| f.check != LintCheck::RailUnpowered),
            "{found:#?}"
        );
    }

    /// Legal control: the island has its own fitted (unmodelled) source, a part
    /// with an output pin on it: may be powered, so never gating.
    #[test]
    fn unfitted_link_beside_an_unmodelled_source_is_not_gating() {
        let mut src = part("U4", "ZZ9000", &[("1", VBUS), ("2", GND), ("3", AV)]);
        src.pins[2].function = "OUT".into();
        let found = run(vec![
            usb(),
            ldo(),
            dnp(part("FB1", "600R@100MHz", &[("1", V3), ("2", AV)])),
            sensor("U3", AV),
            src,
        ]);
        assert!(gating(&found).is_empty(), "{found:#?}");
    }

    fn unsourced(found: &[LintFinding]) -> Vec<&LintFinding> {
        found
            .iter()
            .filter(|f| f.check == LintCheck::RailUnpowered && f.severity == Severity::Medium)
            .collect()
    }

    /// Faulty: +3V3 is only a name. No connector, regulator or link behind it,
    /// and an IC draws from it: the verdict would otherwise rest on a supply
    /// that does not exist. A gating Medium finding (not the High never-comes-up one), naming the IC first.
    #[test]
    fn a_named_rail_with_nothing_behind_it_and_an_ic_on_it_is_flagged() {
        let found = run(vec![
            usb(),
            sensor("U2", V3),
            part("D1", "LED", &[("1", V3), ("2", GND)]),
            part("C1", "1u", &[("1", V3), ("2", GND)]),
        ]);
        assert!(
            gating(&found).is_empty(),
            "a Medium finding, not the High one: {found:#?}"
        );
        let g = unsourced(&found);
        assert_eq!(g.len(), 1, "{found:#?}");
        assert_eq!(g[0].refs, ["U2"], "the IC is named, not the LED");
        let m = &g[0].message;
        assert!(
            m.contains("'+3V3' is named for 3.3 V but nothing on the board brings it in"),
            "{m}"
        );
        assert!(!m.contains("never comes up"), "{m}");
        assert!(m.contains(REMEDY_MARKER) && m.contains("U2"), "{m}");
        assert_eq!(g[0].severity, Severity::Medium);
        let [entered, held] = conditions(vec![usb(), sensor("U2", V3)]);
        assert!(
            entered.as_deref().is_none_or(|e| !e.contains("+3V3")),
            "the unsourced rail is not reported as entered: {entered:?}"
        );
        assert!(held.is_none(), "{held:?}");
    }

    /// The waiver line the message quotes, with the date filled in.
    fn quoted_waiver(message: &str) -> String {
        let at = message
            .find("waive = [")
            .expect("the message quotes a waiver");
        let line = &message[at..];
        let end = line.find("}]").expect("the quoted waiver closes") + 2;
        line[..end].replace("YYYY-MM-DD", "2999-12-31")
    }

    fn waiver_file(name: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hb-rail-basis-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hauksbee-waivers.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    fn island_with_u6() -> Vec<LintFinding> {
        run(vec![
            usb(),
            sensor("U2", V3),
            part("D1", "LED", &[("1", V3), ("2", GND)]),
        ])
    }

    /// The message gives the override at the moment of need: the line it quotes
    /// loads through the real waiver loader, and a waiver written from it stops
    /// the finding gating while it stays listed as waived.
    #[test]
    fn the_quoted_waiver_loads_and_waives_the_finding() {
        let found = island_with_u6();
        let f = unsourced(&found)[0].clone();
        assert!(
            f.message.contains("hauksbee-waivers.toml")
                && f.message.contains(r#"nets = ["+3V3"]"#)
                && f.message.contains(r#"kind = "rail_unpowered""#),
            "{}",
            f.message
        );
        let toml = quoted_waiver(&f.message);
        let path = waiver_file("loads", &toml);
        let today = hauksbee_ir::evidence::RunDate::from_epoch_days(20_663);
        let mut set = hauksbee_bind::waiver::WaiverSet::load_at(&path, today)
            .unwrap_or_else(|e| panic!("the quoted waiver must load: {e}\n{toml}"));
        assert_eq!(set.len(), 1);
        let (kept, waived) = set.partition("lint", found, |f| {
            (
                f.check.as_str().to_string(),
                f.nets.clone(),
                f.refs.clone(),
                f.message.clone(),
            )
        });
        assert!(
            kept.iter().all(|k| k.severity != Severity::Medium),
            "the waived finding no longer gates: {kept:#?}"
        );
        assert_eq!(waived.len(), 1, "it stays listed as waived");
        assert_eq!(waived[0].kind, "rail_unpowered");
        assert!(
            waived[0].subject.contains("'+3V3'"),
            "{}",
            waived[0].subject
        );
        assert!(set.stale().is_empty());
    }

    /// A waiver for a different rail does not excuse this one.
    #[test]
    fn a_waiver_for_another_rail_leaves_the_finding_gating() {
        let found = island_with_u6();
        let toml = quoted_waiver(&unsourced(&found)[0].message).replace("+3V3", "+9V");
        let path = waiver_file("other", &toml);
        let today = hauksbee_ir::evidence::RunDate::from_epoch_days(20_663);
        let mut set = hauksbee_bind::waiver::WaiverSet::load_at(&path, today).unwrap();
        let (kept, waived) = set.partition("lint", found, |f| {
            (
                f.check.as_str().to_string(),
                f.nets.clone(),
                f.refs.clone(),
                f.message.clone(),
            )
        });
        assert!(waived.is_empty());
        assert_eq!(unsourced(&kept).len(), 1);
    }

    /// Legal control: a fragment with no way in at all (a sub-circuit linted on
    /// its own, or a minimal test board) says nothing about where its supply
    /// comes from: silent.
    #[test]
    fn a_board_with_no_power_entry_at_all_is_not_flagged() {
        let found = run(vec![sensor("U2", V3)]);
        assert!(
            found.iter().all(|f| f.check != LintCheck::RailUnpowered),
            "{found:#?}"
        );
    }

    /// Legal control: a bias or reference level named for its voltage (a
    /// divider-derived 1.65 V) feeding op-amp inputs is a signal, not a supply
    /// nothing sources: silent.
    #[test]
    fn a_named_level_that_only_feeds_signal_pins_is_not_a_rail() {
        let mut amp = part("U2", "OPA340", &[("3", V3), ("2", SDA), ("4", GND)]);
        let usb = usb();
        amp.pins[0].function = "IN+".into();
        amp.pins[0].kind = "input".into();
        let found = run(vec![usb, amp]);
        assert!(
            found.iter().all(|f| f.check != LintCheck::RailUnpowered),
            "{found:#?}"
        );
    }

    /// Legal control: the pin that carries a chip's own core regulator out for
    /// decoupling (`VDDCORE`) is sourced by the chip, not a supply input nothing
    /// feeds.
    #[test]
    fn a_core_regulator_decoupling_pin_is_not_an_unsourced_rail() {
        let mut mcu = part("U2", "ATSAMD21G18", &[("1", V3), ("2", GND)]);
        mcu.pins[0].function = "VDDCORE".into();
        mcu.pins[0].kind = "power_in".into();
        let found = run(vec![usb(), mcu, part("C1", "1u", &[("1", V3), ("2", GND)])]);
        assert!(
            found.iter().all(|f| f.check != LintCheck::RailUnpowered),
            "{found:#?}"
        );
    }

    const V12: i64 = 8;

    /// An MCU with no card: `core` is the function of its pins on `+1V2`, and
    /// `supply` is the net its ordinary supply pin is on.
    fn mcu_with_core_node(core: &str, supply: i64) -> Component {
        let mut mcu = part(
            "U2",
            "XYZMCU-9000",
            &[("1", supply), ("2", GND), ("3", V12), ("4", V12)],
        );
        for (i, (function, kind)) in [
            ("VDD", "power_in"),
            ("VSS", "power_in"),
            (core, "power_in"),
            (core, "power_in"),
        ]
        .into_iter()
        .enumerate()
        {
            mcu.pins[i].function = function.into();
            mcu.pins[i].kind = kind.into();
        }
        mcu
    }

    fn run_with_core_node(parts: Vec<Component>) -> Vec<LintFinding> {
        let mut named = nets();
        named.push((V12, "+1V2"));
        rail_basis_lint(&board(&named, parts), &ModelLibrary::builtin()).findings
    }

    fn caps_on_core_node() -> Vec<Component> {
        vec![
            part("C1", "1u", &[("1", V12), ("2", GND)]),
            part("C2", "1u", &[("1", V12), ("2", GND)]),
            part("C3", "100n", &[("1", V12), ("2", GND)]),
        ]
    }

    /// Legal control: a net holding only one chip's pins and capacitors, on a
    /// chip that is powered from a real rail on another pin, is that chip's own
    /// regulator node, whatever the pin or the net is called.
    #[test]
    fn a_net_of_one_chips_pins_and_capacitors_is_its_own_regulator_node() {
        for core in ["VDD12", "VDDC", "VDDC_87", "CPU_PWR", "LDO_OUT"] {
            let mut parts = vec![usb(), ldo(), mcu_with_core_node(core, V3)];
            parts.extend(caps_on_core_node());
            let found = run_with_core_node(parts);
            assert!(
                found.iter().all(|f| f.check != LintCheck::RailUnpowered),
                "{core}: {found:#?}"
            );
        }
    }

    /// Faulty: the same net is a real missing supply when the chip's only
    /// supply pin is on it, when the chip is not powered anywhere else, or when
    /// something besides capacitors loads it.
    #[test]
    fn a_chips_only_supply_net_with_only_capacitors_is_still_unsourced() {
        // Its one supply pin is on the net.
        let mut lone = part("U2", "XYZMCU-9000", &[("1", V12), ("2", GND)]);
        lone.pins[0].function = "VDD".into();
        lone.pins[0].kind = "power_in".into();
        let mut parts = vec![usb(), ldo(), lone];
        parts.extend(caps_on_core_node());
        let found = run_with_core_node(parts);
        assert_eq!(unsourced(&found).len(), 1, "{found:#?}");
        // A second supply pin, but on a net that is also bare: both are unsourced.
        let mut parts = vec![usb(), mcu_with_core_node("VDD12", V3)];
        parts.push(part("C4", "1u", &[("1", V3), ("2", GND)]));
        parts.extend(caps_on_core_node());
        let found = run_with_core_node(parts);
        assert!(!unsourced(&found).is_empty(), "{found:#?}");
        assert!(
            unsourced(&found)
                .iter()
                .any(|f| f.nets.iter().any(|n| n == "+1V2")),
            "{found:#?}"
        );
        // Another chip draws from the net.
        let mut parts = vec![usb(), ldo(), mcu_with_core_node("VDD12", V3)];
        parts.push(sensor("U7", V12));
        parts.extend(caps_on_core_node());
        let found = run_with_core_node(parts);
        assert!(
            unsourced(&found)
                .iter()
                .any(|f| f.nets.iter().any(|n| n == "+1V2")),
            "{found:#?}"
        );
    }

    /// Legal control: a connector on the rail brings it in; the rail is a
    /// stated condition, not a finding.
    #[test]
    fn a_named_rail_with_a_connector_is_not_flagged() {
        let found = run(vec![
            part("J2", "Conn_01x02", &[("1", V3), ("2", GND)]),
            sensor("U2", V3),
        ]);
        assert!(
            found.iter().all(|f| f.check != LintCheck::RailUnpowered),
            "{found:#?}"
        );
    }

    /// Legal control: the rail comes in through a fuse from a connector net
    /// whose name states no voltage. The fuse carries the connector's supply,
    /// so nothing is flagged.
    #[test]
    fn a_rail_fed_through_a_fuse_from_a_connector_is_not_flagged() {
        let found = run(vec![
            part("J2", "Conn_01x02", &[("1", RAW), ("2", GND)]),
            part("F1", "500mA", &[("1", RAW), ("2", V3)]),
            sensor("U2", V3),
        ]);
        assert!(
            found.iter().all(|f| f.check != LintCheck::RailUnpowered),
            "{found:#?}"
        );
    }

    /// Legal control: a resistor divider is not a way in: a fitted 10k from the
    /// connector net does not make the rail sourced.
    #[test]
    fn a_pull_resistor_to_a_connector_net_is_not_a_supply() {
        let found = run(vec![
            part("J2", "Conn_01x02", &[("1", RAW), ("2", GND)]),
            usb(),
            part("R5", "10k", &[("1", RAW), ("2", V3)]),
            sensor("U2", V3),
        ]);
        assert_eq!(unsourced(&found).len(), 1, "{found:#?}");
    }

    /// Faulty: every rail of the board is gone (its only supply net shorted
    /// into GND), so nothing was judged: the not-judged note, never a bare pass.
    #[test]
    fn a_board_with_no_powered_rail_is_not_judged() {
        let found = run(vec![
            part("J1", "Conn_01x02", &[("1", GND), ("2", GND)]),
            sensor("U2", GND),
            sensor("U3", GND),
        ]);
        let note = found
            .iter()
            .find(|f| f.check == LintCheck::RailUnpowered)
            .expect("the not-judged note");
        assert!(
            hauksbee_models::rating_basis::is_not_judged(&note.message),
            "{}",
            note.message
        );
        assert!(note.message.contains("U2, U3"), "{}", note.message);
    }

    /// Legal control: the same parts on a named rail from the connector are
    /// judged: no not-judged note.
    #[test]
    fn a_board_with_a_powered_rail_has_no_not_judged_note() {
        let found = run(vec![
            part("J1", "Conn_01x02", &[("1", V3), ("2", GND)]),
            sensor("U2", V3),
            sensor("U3", V3),
        ]);
        assert!(
            found.iter().all(|f| f.check != LintCheck::RailUnpowered),
            "{found:#?}"
        );
    }

    // ── Name-only rails that real sources feed (diode-OR, module output, pads) ──

    const SYS: i64 = 8;
    const BAT: i64 = 9;
    const BOOST: i64 = 10;
    const MID: i64 = 11;
    const NEG: i64 = 12;
    const NEG_IN: i64 = 13;

    fn nets2() -> Vec<(i64, &'static str)> {
        let mut n = nets();
        n.extend([
            (SYS, "+5V_SYS"),
            (BAT, "VBAT_RAW"),
            (BOOST, "+12V_BOOST"),
            (MID, "MID"),
            (NEG, "-12V"),
            (NEG_IN, "NEG_IN"),
        ]);
        n
    }

    fn run2(parts: Vec<Component>) -> Vec<LintFinding> {
        let lib = ModelLibrary::builtin();
        rail_basis_lint(&board(&nets2(), parts), &lib).findings
    }

    /// A two-pad diode, KiCad numbering (pad 1 cathode, pad 2 anode).
    fn diode(reference: &str, value: &str, anode: i64, cathode: i64) -> Component {
        let mut c = part(reference, value, &[("1", cathode), ("2", anode)]);
        c.pins[0].function = "K".into();
        c.pins[1].function = "A".into();
        c
    }

    fn bat_conn() -> Component {
        part("J2", "Conn_01x02", &[("1", BAT), ("2", GND)])
    }

    /// Legal control: two sources (USB connector, battery connector) each behind
    /// its own diode onto one named rail: a diode-OR, powered.
    #[test]
    fn diode_or_of_two_connector_sources_is_powered() {
        let found = run2(vec![
            usb(),
            bat_conn(),
            diode("D1", "SS14", VBUS, SYS),
            diode("D2", "SS14", BAT, SYS),
            sensor("U2", SYS),
        ]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    /// Legal control: a diode-OR whose source is a regulator output.
    #[test]
    fn diode_or_of_regulator_outputs_is_powered() {
        let found = run2(vec![
            usb(),
            ldo(),
            bat_conn(),
            diode("D1", "SS14", V3, SYS),
            diode("D2", "SS14", BAT, SYS),
            sensor("U2", SYS),
        ]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    /// Legal control: the diode's anode rail is held by an unmodelled converter
    /// (an OUT pin on it), not by an entry or a modelled regulator.
    #[test]
    fn diode_from_a_rail_an_unmodelled_converter_drives_is_powered() {
        let mut conv = part("U4", "ZZ9000", &[("1", VBUS), ("2", GND), ("3", BOOST)]);
        conv.pins[2].function = "VOUT".into();
        let found = run2(vec![
            usb(),
            conv,
            diode("D1", "SS14", BOOST, SYS),
            sensor("U2", SYS),
        ]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    /// Legal control: a common-cathode dual diode (three pads) ORs two sources.
    #[test]
    fn dual_common_cathode_diode_or_is_powered() {
        let mut d = part("D3", "BAT54C", &[("1", VBUS), ("2", BAT), ("3", SYS)]);
        d.pins[0].function = "A1".into();
        d.pins[1].function = "A2".into();
        d.pins[2].function = "K".into();
        let found = run2(vec![usb(), bat_conn(), d, sensor("U2", SYS)]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    /// Legal control: a chain through a diode and a fuse onto the rail.
    #[test]
    fn diode_then_fuse_chain_from_a_connector_is_powered() {
        let found = run2(vec![
            usb(),
            diode("D1", "SS14", VBUS, MID),
            part("F1", "1A", &[("1", MID), ("2", SYS)]),
            sensor("U2", SYS),
        ]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    /// Faulty: the diode points the wrong way (cathode toward the connector):
    /// it blocks, so the rail is still only a name.
    #[test]
    fn a_reversed_diode_does_not_feed_the_rail() {
        let found = run2(vec![
            usb(),
            diode("D1", "SS14", SYS, VBUS),
            sensor("U2", SYS),
        ]);
        assert_eq!(unsourced(&found).len(), 1, "{found:#?}");
    }

    /// Legal control: a negative rail (-12V) behind a reverse-protection diode
    /// from its connector. Current flows from the cathode (connector side) to
    /// the anode (board rail), the opposite way round to a positive rail.
    #[test]
    fn a_negative_rail_behind_a_polarity_diode_from_a_connector_is_powered() {
        let found = run2(vec![
            usb(),
            part("J3", "Conn_01x02", &[("1", NEG_IN), ("2", GND)]),
            diode("D1", "1N5819", NEG, NEG_IN),
            sensor("U2", NEG),
        ]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    /// Faulty: the same negative rail with the diode turned the other way
    /// blocks the connector's feed, so the rail is only a name.
    #[test]
    fn a_negative_rail_behind_a_reversed_diode_is_still_unsourced() {
        let found = run2(vec![
            usb(),
            part("J3", "Conn_01x02", &[("1", NEG_IN), ("2", GND)]),
            diode("D1", "1N5819", NEG_IN, NEG),
            sensor("U2", NEG),
        ]);
        assert_eq!(unsourced(&found).len(), 1, "{found:#?}");
    }

    /// Legal control: a module card that sources the rail on its own pin.
    #[test]
    fn module_supply_output_pin_powers_its_rail() {
        let found = run2(vec![
            usb(),
            part(
                "A1",
                "Arduino Nano",
                &[("17", V3), ("4", GND), ("30", VBUS)],
            ),
            sensor("U2", V3),
        ]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    /// Legal control: the rail reaches a test point pad.
    #[test]
    fn a_test_point_on_the_rail_is_an_entry() {
        let found = run2(vec![
            usb(),
            part("TP1", "TestPoint", &[("1", SYS)]),
            sensor("U2", SYS),
        ]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    /// A board with a +12V edge connector (footprint of fingers), a sense
    /// resistor to the fused rail, a buck module on it and a test point on
    /// the unfused side, run through the default fit policy as a real run is.
    /// A USB connector on another rail makes this a board that takes power in
    /// somewhere, which is when a rail with no way in is a finding.
    fn edge_board(connector_dnp: bool) -> Vec<LintFinding> {
        const V12: i64 = 8;
        const V12F: i64 = 9;
        let mut nets = nets();
        nets.extend([(V12, "+12V_EDGE"), (V12F, "+12V")]);
        let mut p1 = part("P1", "Edge_Fingers", &[("1", V12), ("2", GND)]);
        p1.footprint = "Board:CardEdge_Fingers".into();
        p1.properties.push((
            hauksbee_extract::dnp::EXCLUDED_FROM_BOM_KEY.into(),
            String::new(),
        ));
        p1.dnp = connector_dnp;
        let mut tp = part("TP3", "TestPoint", &[("1", V12)]);
        tp.dnp = true;
        let mut buck = part("U4", "XYZ123", &[("1", V12F), ("2", GND), ("3", SDA)]);
        buck.pins[0].function = "VIN".into();
        buck.pins[0].kind = "power_in".into();
        buck.pins[1].function = "GND".into();
        buck.pins[1].kind = "power_in".into();
        let mut b = board(
            &nets,
            vec![
                usb(),
                p1,
                part("R5", "0.01", &[("1", V12), ("2", V12F)]),
                tp,
                buck,
                part("C1", "22u", &[("1", V12F), ("2", GND)]),
            ],
        );
        b.apply_dnp_policy(hauksbee_extract::dnp::DnpPolicy::default(), &[], &[])
            .unwrap();
        rail_basis_lint(&b, &ModelLibrary::builtin()).findings
    }

    /// Legal control: "excluded from the BOM" is not "not fitted". Edge
    /// fingers are bare board copper that no assembler orders; they are the
    /// rail's entry even though a test point beside them is not fitted.
    #[test]
    fn a_bom_excluded_edge_connector_is_still_the_rails_entry() {
        let found = edge_board(false);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
        assert!(gating(&found).is_empty(), "{found:#?}");
    }

    /// Faulty twin: the same connector marked do-not-populate really is
    /// absent, so the rail has no way in.
    #[test]
    fn a_dnp_edge_connector_leaves_the_rail_unsourced() {
        let found = edge_board(true);
        assert_eq!(unsourced(&found).len(), 1, "{found:#?}");
    }

    /// A rail with no entry and no unfitted source is held by its name: a
    /// condition naming what may source it, never a finding.
    #[test]
    fn a_rail_nothing_modelled_sources_is_a_held_condition() {
        let mut src = part("U4", "ZZ9000", &[("1", GND), ("2", V3), ("3", SDA)]);
        src.pins[1].function = "VOUT".into();
        let parts = vec![src, sensor("U2", V3)];
        assert!(run(parts.clone())
            .iter()
            .all(|f| f.check != LintCheck::RailUnpowered));
        let [entered, held] = conditions(parts);
        assert!(entered.is_none(), "{entered:?}");
        let held = held.expect("held condition");
        assert!(
            held.contains("'+3V3' at 3.3 V (possible source: U4)"),
            "{held}"
        );
    }

    /// A library holding the given card text.
    fn user_lib(cards: &str) -> ModelLibrary {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("hauksbee_rail_basis_{}_{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("models.toml"), cards).unwrap();
        let mut lib = ModelLibrary::empty();
        let errors = lib.load_user_dir(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        lib
    }

    /// A USB-serial bridge in the shape of the dual-mode parts: pin 1 `vregin`
    /// takes 5 V (or 3.3 V), pin 2 `vccd` is the on-chip regulator's output while
    /// `vregin` is above 3.6 V and an input otherwise. `when` is the card's
    /// condition text for the `vccd` row.
    fn dual_role_lib(when: &str) -> ModelLibrary {
        user_lib(&format!(
            "[[models]]\nid = \"dual_role_bridge\"\nkind = \"digital\"\n\
             description = \"dual-role supply fixture\"\n[models.match]\nvalue_re = \"^DUALBRIDGE$\"\n\
             [models.params]\nidentity_only = true\nwarning = \"identity only\"\n\
             unlocked_by = \"validated behavior\"\n\
             [[models.supply_outputs]]\nrole = \"vccd\"\nnominal = 3.3\nmin = 3.0\nmax = 3.6\n\
             basis = \"section 6, reference circuit: VCCD becomes an output when VREGIN is 5 V\"\n\
             {when}\n[models.pins]\n\"1\" = \"vregin\"\n\"2\" = \"vccd\"\n\"3\" = \"gnd\"\n\"4\" = \"io\"\n"
        ))
    }

    const WHEN_5V: &str = "[models.supply_outputs.when]\npin = \"vregin\"\nmin_v = 3.6\n";

    /// The bridge: `vregin` on `vregin_net`, `vccd` on +3V3.
    fn bridge(vregin_net: i64) -> Component {
        // Four connected pads: a part of three or fewer reads as a series pass
        // element, which would feed the rail through itself.
        let mut u = part(
            "U1",
            "DUALBRIDGE",
            &[("1", vregin_net), ("3", GND), ("2", V3), ("4", SDA)],
        );
        u.pins[0].function = "VREGIN".into();
        u.pins[1].function = "GND".into();
        u.pins[2].function = "VCCD".into();
        u.pins[2].kind = "power_in".into();
        u
    }

    fn run_with(lib: &ModelLibrary, parts: Vec<Component>) -> Vec<LintFinding> {
        rail_basis_lint(&board(&nets(), parts), lib).findings
    }

    /// An on-chip regulator's output pin that is an input in another mode is a
    /// rail source in the mode the board uses: VREGIN on the 5 V supply, VCCD
    /// the only thing on +3V3 that sources it.
    #[test]
    fn a_dual_role_output_pin_in_its_output_mode_sources_the_rail() {
        let lib = dual_role_lib(WHEN_5V);
        let found = run_with(&lib, vec![usb(), bridge(VBUS), sensor("U2", V3)]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    /// Silent control: the pin is an input in the other mode, so with VREGIN on
    /// the 3.3 V rail itself nothing sources +3V3 and it is still reported.
    #[test]
    fn a_dual_role_pin_in_its_input_mode_leaves_the_rail_unsourced() {
        let lib = dual_role_lib(WHEN_5V);
        let found = run_with(&lib, vec![usb(), bridge(V3), sensor("U2", V3)]);
        assert_eq!(unsourced(&found).len(), 1, "{found:#?}");
        assert!(unsourced(&found)[0].message.contains("'+3V3'"));
    }

    /// Silent control: the same part with no card row for the pin sources
    /// nothing, so its rail is reported (a pin named like a supply input is a
    /// load until a card says it is an output).
    #[test]
    fn a_supply_pin_with_no_output_row_is_not_a_source() {
        let lib = user_lib(
            "[[models]]\nid = \"plain_bridge\"\nkind = \"digital\"\ndescription = \"fixture\"\n\
             [models.match]\nvalue_re = \"^DUALBRIDGE$\"\n[models.params]\nidentity_only = true\n\
             warning = \"identity only\"\nunlocked_by = \"validated behavior\"\n\
             [models.pins]\n\"1\" = \"vregin\"\n\"2\" = \"vccd\"\n\"3\" = \"gnd\"\n\"4\" = \"io\"\n",
        );
        let found = run_with(&lib, vec![usb(), bridge(VBUS), sensor("U2", V3)]);
        assert_eq!(unsourced(&found).len(), 1, "{found:#?}");
    }

    /// A row with no condition sources whenever the part is fitted.
    #[test]
    fn an_unconditional_output_row_sources_in_either_mode() {
        let lib = dual_role_lib("");
        let found = run_with(&lib, vec![usb(), bridge(V3), sensor("U2", V3)]);
        assert!(unsourced(&found).is_empty(), "{found:#?}");
    }

    const VCC_BARE: i64 = 9;

    /// A part with a recommended supply row `min..max` on its `vdd` pin.
    fn windowed_lib() -> ModelLibrary {
        let card = |id: &str, value: &str, lo: f64, hi: f64| {
            format!(
                "[[models]]\nid = \"{id}\"\nkind = \"digital\"\ndescription = \"fixture\"\n\
                 [models.match]\nvalue_re = \"^{value}$\"\n[models.params]\nidentity_only = true\n\
                 warning = \"identity only\"\nunlocked_by = \"validated behavior\"\n\
                 [[models.envelope]]\nkind = \"supply_range\"\npin = \"vdd\"\nmin_v = {lo}\n\
                 max_v = {hi}\nbasis = \"Recommended Operating Conditions\"\n\
                 [models.pins]\n\"1\" = \"vdd\"\n\"2\" = \"gnd\"\n"
            )
        };
        user_lib(
            &[
                card("mcu_fx", "FXMCU", 2.0, 3.6),
                card("xcvr_fx", "FXXCVR", 3.0, 3.6),
                card("tof_fx", "FXTOF", 2.6, 3.0),
                card("wide_fx", "FXWIDE", 2.7, 5.5),
            ]
            .join("\n"),
        )
    }

    fn consumer(reference: &str, value: &str, rail: i64) -> Component {
        let mut c = part(reference, value, &[("1", rail), ("2", GND)]);
        c.pins[0].function = "VDD".into();
        c.pins[0].kind = "power_in".into();
        c
    }

    fn bare_board(
        rail_name: &'static str,
        consumers: &[(&str, &str)],
    ) -> (ExtractedBoard, ModelLibrary) {
        let mut named = nets();
        named.push((VCC_BARE, rail_name));
        let mut parts = vec![part("J1", "Conn_01x02", &[("1", VCC_BARE), ("2", GND)])];
        for (r, v) in consumers {
            parts.push(consumer(r, v, VCC_BARE));
        }
        (board(&named, parts), windowed_lib())
    }

    fn bare_notes(found: &[LintFinding]) -> Vec<&LintFinding> {
        found
            .iter()
            .filter(|f| {
                f.check == LintCheck::RailUnpowered && f.message.contains("states no voltage")
            })
            .collect()
    }

    /// A bare `VCC` on a connector whose consumers cannot all run at the 5 V its
    /// name is conventionally taken for: no level is guessed, and one note
    /// names what the consumers allow.
    #[test]
    fn a_bare_rail_the_consumers_rule_out_holds_no_level_and_says_so() {
        let (b, lib) = bare_board("VCC", &[("U1", "FXMCU"), ("U2", "FXXCVR"), ("U3", "FXTOF")]);
        let bound = hauksbee_bind::binder::bind_board(&b, &lib);
        assert!(
            bound.supplies.iter().all(|l| l.net_name != "VCC"),
            "no 5 V level may be held on the bare rail"
        );
        let found = rail_basis_findings(&rail_bases(&b, &lib, &bound)).findings;
        let notes = bare_notes(&found);
        assert_eq!(notes.len(), 1, "{found:#?}");
        assert_eq!(notes[0].severity, Severity::Low);
        assert!(
            notes[0].message.contains("3 V only"),
            "{}",
            notes[0].message
        );
        assert!(
            notes[0].message.contains("U2 minimum"),
            "{}",
            notes[0].message
        );
        assert!(
            notes[0].message.contains("U3 maximum"),
            "{}",
            notes[0].message
        );
    }

    /// Windows that do not meet at all are their own statement.
    #[test]
    fn a_bare_rail_no_single_supply_satisfies_says_so() {
        let mut named = nets();
        named.push((VCC_BARE, "VCC"));
        let lib = user_lib(&format!(
            "{}\n{}",
            "[[models]]\nid = \"a_fx\"\nkind = \"digital\"\ndescription = \"f\"\n[models.match]\nvalue_re = \"^FXA$\"\n[models.params]\nidentity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"\n[[models.envelope]]\nkind = \"supply_range\"\npin = \"vdd\"\nmin_v = 3.0\nmax_v = 3.6\nbasis = \"ROC\"\n[models.pins]\n\"1\" = \"vdd\"\n\"2\" = \"gnd\"\n",
            "[[models]]\nid = \"b_fx\"\nkind = \"digital\"\ndescription = \"f\"\n[models.match]\nvalue_re = \"^FXB$\"\n[models.params]\nidentity_only = true\nwarning = \"w\"\nunlocked_by = \"u\"\n[[models.envelope]]\nkind = \"supply_range\"\npin = \"vdd\"\nmin_v = 1.65\nmax_v = 1.95\nbasis = \"ROC\"\n[models.pins]\n\"1\" = \"vdd\"\n\"2\" = \"gnd\"\n"
        ));
        let parts = vec![
            part("J1", "Conn_01x02", &[("1", VCC_BARE), ("2", GND)]),
            consumer("U1", "FXA", VCC_BARE),
            consumer("U2", "FXB", VCC_BARE),
        ];
        let found = rail_basis_lint(&board(&named, parts), &lib).findings;
        let notes = bare_notes(&found);
        assert_eq!(notes.len(), 1, "{found:#?}");
        assert!(
            notes[0]
                .message
                .contains("no single supply satisfies every consumer"),
            "{}",
            notes[0].message
        );
    }

    /// Silent control: the same consumers on a rail whose name states 5 V keep
    /// the name's level, and nothing here is said about it.
    #[test]
    fn a_rail_named_for_its_voltage_keeps_the_level() {
        let (b, lib) = bare_board("5V", &[("U1", "FXMCU"), ("U2", "FXXCVR"), ("U3", "FXTOF")]);
        let bound = hauksbee_bind::binder::bind_board(&b, &lib);
        assert!(bound.supplies.iter().any(|l| l.net_name == "5V"));
        let found = rail_basis_findings(&rail_bases(&b, &lib, &bound)).findings;
        assert!(bare_notes(&found).is_empty(), "{found:#?}");
    }

    /// Silent control: a bare `VCC` whose consumers all allow 5 V keeps the
    /// conventional level.
    #[test]
    fn a_bare_rail_the_consumers_allow_at_5v_keeps_the_convention() {
        let (b, lib) = bare_board("VCC", &[("U1", "FXWIDE")]);
        let bound = hauksbee_bind::binder::bind_board(&b, &lib);
        assert!(bound.supplies.iter().any(|l| l.net_name == "VCC"));
        let found = rail_basis_findings(&rail_bases(&b, &lib, &bound)).findings;
        assert!(bare_notes(&found).is_empty(), "{found:#?}");
    }
}
