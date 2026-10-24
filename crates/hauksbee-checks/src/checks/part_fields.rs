//! A passive whose own fields disagree about which part it is.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/checks.md.
//!
//! The assembler buys by the manufacturer part number and places on the
//! footprint; the schematic's Value field and symbol are what every other
//! check reads. A part copied from a neighbour often keeps the neighbour's MPN,
//! so the board is built with a part nobody simulated: a 1 uF where the
//! schematic says 100 nF, a 6.3 V capacitor on a 12 V rail, an 0603 part on an
//! 0402 land, or a tantalum on a symbol that cannot say which way round it goes.
//!
//! The MPN is read by [`hauksbee_models::mpn::decode`], which knows the major
//! chip capacitor and resistor series and says nothing about any other part
//! number, so an unknown series is silent. A distributor order code that
//! carries the MPN (Mouser's `81-GRM...`) is read the same way; a bare
//! catalogue number (LCSC `C1525`) says nothing offline and is not looked up.
//! A part that is not fitted is never
//! judged. Each part gets at most one finding listing what disagrees: Medium
//! for a field mismatch, High when the MPN's rated voltage is below the voltage
//! its two named nets put across it. A stated voltage the MPN exceeds is a
//! safe substitute and is not reported. Polarised symbols on unmarked
//! footprints, usually ceramics drawn with the wrong symbol, share one Low
//! finding per board.

use hauksbee_extract::assembly::AssemblyState;
use hauksbee_extract::{
    capacitor_polarity_marks, Component, ExtractedBoard, LintCheck, LintFinding, NetLintReport,
    Severity, REMEDY_MARKER,
};
use hauksbee_ir::units::format_engineering;
use hauksbee_models::mpn::{decode, PassiveMpn};
use hauksbee_models::netclass::{is_ground, rail_voltage};
use hauksbee_models::refdes::RefClass;
use hauksbee_models::value::{parse_value, parse_voltage_rating_v};

/// Whether two values name the same part (within rounding of the field).
fn same(a: f64, b: f64) -> bool {
    (a - b).abs() <= a.abs().max(b.abs()) * 0.005
}

/// The magnitude the Value field names for a passive of `kind`: its first
/// token, in farads or ohms, else what a part number in it decodes to.
fn stated_value(value: &str, kind: char) -> Option<f64> {
    if let Some(d) = decode(value) {
        return (d.kind == kind).then_some(d.value);
    }
    let token = value.split([' ', '/', ',', '_']).next()?;
    let parsed = parse_value(token)?;
    let unit_fits = match (kind, parsed.unit.as_deref()) {
        (_, None) => true,
        ('C', Some(u)) => u.eq_ignore_ascii_case("F"),
        (_, Some(u)) => matches!(u, "R" | "r" | "Ω" | "ohm" | "Ohm"),
    };
    unit_fits.then_some(parsed.si)
}

/// The voltage across a two-pad part when both its nets are named rails or
/// ground.
fn voltage_across(board: &ExtractedBoard, c: &Component) -> Option<f64> {
    let mut levels = c.pins.iter().filter_map(|p| {
        let name = &board.net(p.net?)?.name;
        Some(if is_ground(name) {
            Some(0.0)
        } else {
            rail_voltage(name)
        })
    });
    let (a, b) = (levels.next()??, levels.next()??);
    (levels.next().is_none() && c.pins.len() == 2).then(|| (a - b).abs())
}

/// The imperial chip size a footprint names, if any.
fn footprint_case(footprint: &str) -> Option<&'static str> {
    let name = footprint.rsplit(':').next().unwrap_or(footprint);
    hauksbee_models::chip_size::find_imperial(|code| name.contains(code)).map(|c| c.imperial)
}

const fn unit(kind: char) -> &'static str {
    if kind == 'C' {
        "F"
    } else {
        "Ω"
    }
}

/// What disagrees between an ordering code's decoding `d` and the component's
/// other fields, and whether the rated voltage is below the rail
/// (`rail_judged` false skips that). `code` names the code in a sentence.
fn mpn_clauses(
    board: &ExtractedBoard,
    c: &Component,
    code: &str,
    d: &PassiveMpn,
    rail_judged: bool,
) -> (Vec<String>, bool) {
    let mut clauses = Vec::new();
    let class = match RefClass::of(&c.reference) {
        RefClass::Capacitor => 'C',
        RefClass::Resistor => 'R',
        _ => return (clauses, false),
    };
    let word = |k: char| if k == 'C' { "capacitor" } else { "resistor" };
    if class != d.kind {
        clauses.push(format!(
            "its {code} is a {} ({}) on a {} designator",
            word(d.kind),
            d.series,
            word(class)
        ));
        return (clauses, false);
    }
    if let Some(v) = stated_value(&c.value, d.kind).filter(|v| !same(*v, d.value)) {
        clauses.push(format!(
            "its Value field says {} but its {code} ({}) orders {}",
            format_engineering(v, unit(d.kind)),
            d.series,
            format_engineering(d.value, unit(d.kind))
        ));
    }
    let stated_v = decode(&c.value).map_or_else(|| parse_voltage_rating_v(&c.value), |v| v.rated_v);
    // A part rated above what the schematic asks for is a safe substitute.
    if let (Some(said), Some(rated)) = (stated_v, d.rated_v) {
        if rated < said && !same(said, rated) {
            clauses.push(format!(
                "its Value field says {said} V but its {code} is rated only {rated} V"
            ));
        }
    }
    if let Some(case) = footprint_case(&c.footprint).filter(|f| *f != d.case) {
        clauses.push(format!(
            "its {code} is an {} part but its footprint '{}' is an {case} land",
            d.case, c.footprint
        ));
    }
    let over = (d.rated_v.zip(voltage_across(board, c)))
        .filter(|(rated, across)| rail_judged && rated < across);
    if let Some((rated, across)) = over {
        clauses.push(format!(
            "its {code} is rated {rated} V but its nets put {across} V across it"
        ));
    }
    (clauses, over.is_some())
}

/// Whether a field holds a distributor's order code (LCSC, Digi-Key, Mouser,
/// a supplier column) rather than the manufacturer's part number.
fn is_supplier_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "lcsc",
        "jlc",
        "digi",
        "mouser",
        "farnell",
        "newark",
        "arrow",
        "supplier",
        "distributor",
        "vendor",
    ]
    .iter()
    .any(|w| key.contains(w))
}

/// Every code the part is ordered by that decodes, with how a sentence names
/// it: its MPN fields, and distributor order codes that carry an MPN (Mouser's
/// `81-GRM155...` after its prefix). A bare catalogue number (LCSC `C1525`)
/// decodes to nothing and is never looked up.
fn ordering_codes(c: &Component) -> Vec<(String, PassiveMpn)> {
    let mut out: Vec<(String, PassiveMpn)> = Vec::new();
    for (key, value) in &c.properties {
        let value = value.trim();
        let decoded = if hauksbee_extract::bom::is_mpn_property_key(key) {
            decode(value).map(|d| (format!("part number {value}"), d))
        } else if is_supplier_key(key) {
            let tail = value.split_once('-').map(|(_, t)| t);
            (decode(value).or_else(|| tail.and_then(decode)))
                .map(|d| (format!("ordering code {value} ({})", key.trim()), d))
        } else {
            None
        };
        if let Some((label, d)) = decoded.filter(|(_, d)| !out.iter().any(|(_, o)| o == d)) {
            out.push((label, d));
        }
    }
    out
}

/// A polarised footprint under a symbol that is not.
fn polarity_clause(c: &Component) -> Option<String> {
    matches!(capacitor_polarity_marks(c), (Some(true), Some(false))).then(|| {
        format!(
            "its footprint '{}' is polarised (pad 1 is +) but its symbol '{}' is not, so the \
             schematic never says which net the + terminal is on",
            c.footprint, c.lib_id
        )
    })
}

/// Reports each fitted part whose ordering code, value, footprint and symbol disagree.
///
/// A part `rail_reported` names already has its voltage overstress
/// reported elsewhere, and is not judged against its rail again.
#[must_use]
pub fn part_field_lint(
    board: &ExtractedBoard,
    rail_reported: &dyn Fn(&str) -> bool,
) -> NetLintReport {
    let mut report = NetLintReport::default();
    let mut unmarked: Vec<String> = Vec::new();
    for c in &board.components {
        if AssemblyState::of(c).fitted().is_none() {
            continue;
        }
        if matches!(capacitor_polarity_marks(c), (Some(false), Some(true))) {
            unmarked.push(c.reference.clone());
        }
        let mut clauses: Vec<String> = Vec::new();
        let mut high = false;
        for (code, d) in ordering_codes(c) {
            let (said, over) = mpn_clauses(board, c, &code, &d, !rail_reported(&c.reference));
            high |= over;
            for s in said {
                if !clauses.contains(&s) {
                    clauses.push(s);
                }
            }
        }
        if RefClass::of(&c.reference) == RefClass::Capacitor {
            clauses.extend(polarity_clause(c));
        }
        if clauses.is_empty() {
            continue;
        }
        report.findings.push(LintFinding {
            check: LintCheck::PartFieldMismatch,
            severity: if high {
                Severity::High
            } else {
                Severity::Medium
            },
            message: format!(
                "{}'s fields describe different parts: {}. {REMEDY_MARKER}decide which part {} \
                 needs and correct the part number, value, footprint or symbol to match it.",
                c.reference,
                clauses.join("; "),
                c.reference
            ),
            refs: vec![c.reference.clone()],
            nets: Vec::new(),
        });
    }
    // Often a ceramic drawn with the polarised symbol, so one Low note for all.
    if !unmarked.is_empty() {
        report.findings.push(LintFinding {
            check: LintCheck::PartFieldMismatch,
            severity: Severity::Low,
            message: format!(
                "{}: polarised capacitor symbol on a footprint with no + mark, so a polarised \
                 part could be placed either way round. {REMEDY_MARKER}if they are polarised, use a \
                 footprint that marks +; if not, use the plain capacitor symbol.",
                unmarked.join(", ")
            ),
            refs: unmarked,
            nets: Vec::new(),
        });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::test_support::{board, part};

    fn cap(reference: &str, value: &str, footprint: &str, mpn: &str) -> Component {
        Component {
            lib_id: "Device:C".into(),
            footprint: footprint.into(),
            properties: vec![("MPN".into(), mpn.into())],
            ..part(reference, value, &[("1", 1), ("2", 2)])
        }
    }

    fn lint(components: Vec<Component>, rail: &str) -> Vec<LintFinding> {
        part_field_lint(&board(&[(1, rail), (2, "GND")], components), &|_| false).findings
    }

    const C0402: &str = "Capacitor_SMD:C_0402_1005Metric";

    #[test]
    fn a_value_the_mpn_does_not_order_is_medium() {
        let found = lint(
            vec![cap("C1", "100nF", C0402, "GRM155R61A105KE15D")],
            "+3V3",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::Medium);
        assert!(
            found[0].message.contains("says 100 nF"),
            "{}",
            found[0].message
        );
        assert!(
            found[0].message.contains("orders 1 uF"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn a_crystal_load_cap_whose_mpn_is_another_value_is_reported() {
        let found = lint(
            vec![cap("C7", "18pF", C0402, "GRM1555C1H220JA01D")],
            "XTAL_IN",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].message.contains("orders 22 pF"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn a_rating_below_the_rail_is_high() {
        // Samsung CL05A105KQ5NNNC: 1 uF, 6.3 V (code Q), 0402.
        let found = lint(vec![cap("C2", "1uF", C0402, "CL05A105KQ5NNNC")], "+12V");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].severity, Severity::High);
        assert!(
            found[0].message.contains("rated 6.3 V"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn a_stated_voltage_and_a_case_the_mpn_disagrees_with_are_medium() {
        let found = lint(
            vec![cap("C3", "100nF/50V", C0402, "CL10B104KA8NNNC")],
            "+3V3",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        let m = &found[0].message;
        assert!(
            m.contains("says 50 V") && m.contains("rated only 25 V"),
            "{m}"
        );
        assert!(m.contains("0603 part") && m.contains("0402 land"), "{m}");
        assert_eq!(found[0].severity, Severity::Medium);
    }

    #[test]
    fn a_resistor_mpn_on_a_capacitor_is_reported() {
        let found = lint(vec![cap("C4", "0", C0402, "RC0402JR-070RL")], "+3V3");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].message.contains("is a resistor"),
            "{}",
            found[0].message
        );
    }

    #[test]
    fn polarity_disagreement_between_footprint_and_symbol_is_reported() {
        let mut tant = cap(
            "C5",
            "10uF",
            "Capacitor_Tantalum_SMD:CP_EIA-3216-18_Kemet-A",
            "",
        );
        tant.properties.clear();
        let mut pol = cap("C6", "10uF", "Capacitor_SMD:C_0805_2012Metric", "");
        pol.properties.clear();
        pol.lib_id = "Device:C_Polarized".into();
        let found = lint(vec![tant, pol], "+3V3");
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found[0].message.contains("is polarised (pad 1 is +)"));
        assert_eq!(found[0].severity, Severity::Medium);
        assert!(found[1].message.contains("with no + mark"));
        assert_eq!(
            (found[1].severity, &found[1].refs[..]),
            (Severity::Low, &["C6".to_string()][..])
        );
    }

    #[test]
    fn matching_unknown_and_unfitted_parts_are_silent() {
        let mut dnp = cap("C3", "1uF", C0402, "GRM155R71H104KE14D");
        dnp.dnp = true;
        let mut tant = cap(
            "C4",
            "10uF",
            "Capacitor_Tantalum_SMD:CP_EIA-3216-18_Kemet-A",
            "",
        );
        tant.properties.clear();
        tant.lib_id = "Device:C_Polarized".into();
        let found = lint(
            vec![
                cap("C1", "100nF", C0402, "GRM155R71H104KE14D"),
                cap("C2", "0.1u 50V X7R", C0402, "CL05B104KB5NNNC"),
                // Rated 50 V where the value asks for 16 V: a safe substitute.
                cap(
                    "C7",
                    "1uF 16V",
                    "Capacitor_SMD:C_0603_1608Metric",
                    "CL10A105KB8NNNC",
                ),
                cap("C5", "4.7nF", C0402, "XYZ0402-472"),
                cap(
                    "C6",
                    "10uF",
                    "Capacitor_SMD:C_0805_2012Metric",
                    "GRM21BR61A106KE19L",
                ),
                dnp,
                tant,
            ],
            "+5V",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_supplier_code_that_carries_an_mpn_is_read_and_a_catalogue_number_is_not() {
        let ordered = |key: &str, code: &str| Component {
            properties: vec![(key.into(), code.into())],
            ..cap("C8", "8pF", C0402, "")
        };
        let found = lint(
            vec![
                ordered("Mouser Part Number", "81-GRM1555C1H180JA01D"),
                ordered("LCSC Part #", "C1525"),
                ordered("Digi-Key_PN", "490-5915-1-ND"),
            ],
            "XTAL_OUT",
        );
        assert_eq!(found.len(), 1, "{found:?}");
        let m = &found[0].message;
        assert!(
            m.contains("ordering code 81-GRM1555C1H180JA01D (Mouser Part Number)"),
            "{m}"
        );
        assert!(m.contains("says 8 pF") && m.contains("orders 18 pF"), "{m}");
    }

    #[test]
    fn a_rail_already_reported_is_not_judged_again() {
        let b = board(
            &[(1, "+12V"), (2, "GND")],
            vec![cap("C2", "1uF", C0402, "CL05A105KQ5NNNC")],
        );
        assert!(part_field_lint(&b, &|r| r == "C2").findings.is_empty());
    }
}
