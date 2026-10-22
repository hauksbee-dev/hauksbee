//! Layout wiring checks on hand-authored boards: each fault next to its legal
//! twin, so a check that stops firing (or fires on the legal form) fails here.

use std::fmt::Write as _;

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, Severity, CHECK_BEFORE_ORDERING};

fn board(items: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "A")
  (net 2 "GND")
{items}
)
"#
    )
}

/// A two-pad part `reference` at (x, 0): pad 1 on `net1`, pad 2 on `net2`.
fn part(reference: &str, x: f64, net1: &str, net2: &str, extra: &str) -> String {
    format!(
        r#"  (footprint "lib:R" (layer "F.Cu") (at {x} 0) {extra}
    (property "Reference" "{reference}")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") {net1})
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") {net2}))
"#
    )
}

const A: &str = r#"(net 1 "A")"#;
const GND: &str = r#"(net 2 "GND")"#;

fn lint(text: &str) -> Vec<LintFinding> {
    let pcb = ExtractedBoard::from_kicad_pcb(text).expect("board parses");
    pcb.layout_lint(text, None).findings
}

fn high(findings: &[LintFinding]) -> Vec<&LintFinding> {
    findings
        .iter()
        .filter(|f| f.severity == Severity::High)
        .collect()
}

/// R1.1 at x=0 and R2.1 at x=10, both on A.
fn two_parts(extra: &str) -> String {
    board(&format!(
        "{}{}{extra}",
        part("R1", 0.0, A, GND, ""),
        part("R2", 10.0, A, GND, "")
    ))
}

const TRACK: &str = r#"  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
"#;
const GND_TRACK: &str = r#"  (segment (start 2 0) (end 12 0) (width 0.25) (layer "F.Cu") (net 2))
"#;

#[test]
fn unrouted_pad_is_reported_and_a_routed_one_is_not() {
    let open = lint(&two_parts(GND_TRACK));
    let open = high(&open);
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0].check, LintCheck::UnroutedPad);
    assert_eq!(open[0].nets, ["A"]);
    assert!(open[0].message.contains("R2.1") || open[0].message.contains("R1.1"));

    assert!(lint(&two_parts(&format!("{TRACK}{GND_TRACK}"))).is_empty());
}

#[test]
fn a_via_joins_layers_and_a_missing_via_leaves_an_island() {
    let back = r#"  (segment (start 0 0) (end 5 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 5 0) (end 10 0) (width 0.25) (layer "B.Cu") (net 1))
  (via (at 10 0) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1))
"#;
    let via = r#"  (via (at 5 0) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1))
"#;
    let without = lint(&two_parts(&format!("{back}{GND_TRACK}")));
    assert!(!high(&without).is_empty(), "{without:?}");
    let with = lint(&two_parts(&format!("{back}{via}{GND_TRACK}")));
    assert!(high(&with).is_empty(), "{with:?}");
}

const ZONE_OUTLINE: &str = r#"  (zone (net 1) (net_name "A") (layer "F.Cu")
    (polygon (pts (xy -1 -1) (xy 11 -1) (xy 11 1) (xy -1 1)))"#;

#[test]
fn a_filled_zone_connects_and_an_unfilled_one_is_unknown_not_open() {
    let filled = format!(
        "{ZONE_OUTLINE}\n    (filled_polygon (layer \"F.Cu\") (pts (xy -1 -0.6) (xy 11 -0.6) (xy 11 0.6) (xy -1 0.6))))\n{GND_TRACK}"
    );
    assert!(lint(&two_parts(&filled)).is_empty());

    // Never filled, and `(fill yes)` committed without its saved polygons.
    for fill in ["", "\n    (fill yes (thermal_gap 0.5))"] {
        let unfilled = lint(&two_parts(&format!("{ZONE_OUTLINE}{fill})\n{GND_TRACK}")));
        assert!(high(&unfilled).is_empty(), "{unfilled:?}");
        assert_eq!(unfilled.len(), 1, "{unfilled:?}");
        assert!(unfilled[0].message.starts_with("zones not filled"));
    }
}

/// A KiCad 10 board (nets named on each item, no net table) whose two A pads
/// meet a same-net pour only through thermal-relief spokes: the fill is cut
/// back from each pad by the thermal gap, so neither pad centre lies inside
/// the filled polygon and only a spoke touches the pad's edge. KiCad counts
/// such a pad as connected, and so must this. The fault twin drops R2's spoke
/// (a pad isolated inside its clearance, an unconnected item to KiCad); R2.1
/// is then an island. A zone outline with no saved fill proves nothing either
/// way (`a_filled_zone_connects_and_an_unfilled_one_is_unknown_not_open`).
#[test]
fn a_pad_reached_only_by_thermal_spokes_of_a_fill_is_connected() {
    let board10 = |fill_pts: &str| {
        format!(
            r#"(kicad_pcb (version 20260206) (generator "pcbnew") (generator_version "10.0")
  (layers (0 "F.Cu" signal) (2 "B.Cu" signal))
  (footprint "lib:R" (layer "F.Cu") (at 0 0)
    (property "Reference" "R1")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "A"))
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net "GND")))
  (footprint "lib:R" (layer "F.Cu") (at 10 0)
    (property "Reference" "R2")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "A"))
    (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net "GND")))
  (segment (start 2 0) (end 12 0) (width 0.25) (layer "F.Cu") (net "GND"))
  (zone (net "A") (layer "F.Cu") (connect_pads (clearance 0.5)) (min_thickness 0.25)
    (fill yes (thermal_gap 0.5) (thermal_bridge_width 0.5))
    (polygon (pts (xy -1 -1) (xy 11 -1) (xy 11 3) (xy -1 3)))
    (filled_polygon (layer "F.Cu") (pts {fill_pts})))
)
"#
        )
    };
    // A strip 0.5 mm above the pads, with a 0.5 mm spoke down onto each A pad.
    let both =
        "(xy -1 3) (xy 11 3) (xy 11 1) (xy 10.25 1) (xy 10.25 0.4) (xy 9.75 0.4) (xy 9.75 1) \
                (xy 0.25 1) (xy 0.25 0.4) (xy -0.25 0.4) (xy -0.25 1) (xy -1 1)";
    let spoked = lint(&board10(both));
    assert!(high(&spoked).is_empty(), "{spoked:?}");
    assert!(
        spoked.iter().all(|f| f.check != LintCheck::UnroutedPad),
        "a saved fill is read, not reported as unfilled: {spoked:?}"
    );

    let r1_only = "(xy -1 3) (xy 11 3) (xy 11 1) (xy 0.25 1) (xy 0.25 0.4) (xy -0.25 0.4) (xy -0.25 1) (xy -1 1)";
    let starved = lint(&board10(r1_only));
    let open = high(&starved);
    assert_eq!(open.len(), 1, "{starved:?}");
    assert_eq!(open[0].check, LintCheck::UnroutedPad);
    assert_eq!(open[0].nets, ["A"]);
    assert!(open[0].message.contains("R2.1"), "{}", open[0].message);
}

#[test]
fn npth_no_connect_and_netless_pads_are_never_islands() {
    let odd = r#"  (footprint "lib:odd" (layer "F.Cu") (at 30 0)
    (property "Reference" "J1")
    (pad "" np_thru_hole circle (at 0 0) (size 1 1) (drill 1) (layers "*.Cu") (net 1 "A"))
    (pad "2" smd rect (at 3 0) (size 1 1) (layers "F.Cu") (net 1 "A") (pintype "no_connect"))
    (pad "3" smd rect (at 6 0) (size 1 1) (layers "F.Cu") (net 0 "")))
"#;
    assert!(lint(&two_parts(&format!("{TRACK}{GND_TRACK}{odd}"))).is_empty());
    // The same pad as an ordinary plated pad is an island.
    let plain = odd.replace(r#" (pintype "no_connect")"#, "");
    assert_eq!(
        high(&lint(&two_parts(&format!("{TRACK}{GND_TRACK}{plain}")))).len(),
        1
    );
}

#[test]
fn a_jumper_footprint_joins_its_same_net_pads() {
    let jumper = |groups: &str| {
        format!(
            r#"  (footprint "lib:J" (layer "F.Cu") (at 30 0) {groups}
    (property "Reference" "JP1")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "A"))
    (pad "2" smd rect (at 3 0) (size 1 1) (layers "F.Cu") (net 1 "A")))
  (segment (start 10 0) (end 30 0) (width 0.25) (layer "F.Cu") (net 1))
"#
        )
    };
    let tied = jumper(r#"(jumper_pad_groups ("1" "2"))"#);
    assert!(lint(&two_parts(&format!("{TRACK}{GND_TRACK}{tied}"))).is_empty());
    let loose = jumper("");
    assert_eq!(
        high(&lint(&two_parts(&format!("{TRACK}{GND_TRACK}{loose}")))).len(),
        1
    );
}

#[test]
fn lands_sharing_a_number_are_one_pin_unless_the_footprint_says_not() {
    // A connector shell: two tabs numbered alike, one routed to the net.
    let shell = |flag: &str, second: &str| {
        format!(
            r#"  (footprint "lib:USB" (layer "F.Cu") (at 30 0) {flag}
    (property "Reference" "J1")
    (pad "0" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "A"))
    (pad "{second}" smd rect (at 0 4) (size 1 1) (layers "F.Cu") (net 1 "A")))
  (segment (start 10 0) (end 30 0) (width 0.25) (layer "F.Cu") (net 1))
"#
        )
    };
    let legacy = shell("", "0");
    assert!(lint(&two_parts(&format!("{TRACK}{GND_TRACK}{legacy}"))).is_empty());
    // KiCad 9 says outright that duplicate numbers are not jumpers, and a land
    // with its own number is its own pin: both are left open.
    for open in [
        shell("(duplicate_pad_numbers_are_jumpers no)", "0"),
        shell("", "1"),
    ] {
        let found = lint(&two_parts(&format!("{TRACK}{GND_TRACK}{open}")));
        assert_eq!(high(&found).len(), 1, "{found:?}");
    }
}

#[test]
fn copper_graphic_with_a_net_conducts() {
    let poly = |net: &str| {
        format!(
            r#"  (gr_poly (pts (xy -0.5 -0.3) (xy 10.5 -0.3) (xy 10.5 0.3) (xy -0.5 0.3)) (stroke (width 0) (type solid)) (fill solid) (layer "F.Cu") {net})
"#
        )
    };
    assert!(lint(&two_parts(&format!("{}{GND_TRACK}", poly("(net 1)")))).is_empty());
    assert_eq!(
        high(&lint(&two_parts(&format!("{}{GND_TRACK}", poly(""))))).len(),
        1
    );
}

#[test]
fn stray_via_is_a_low_note() {
    let stray = r#"  (via (at 50 50) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1))
"#;
    let found = lint(&two_parts(&format!("{TRACK}{GND_TRACK}{stray}")));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::Low);
    assert!(lint(&two_parts(&format!("{TRACK}{GND_TRACK}"))).is_empty());
}

#[test]
fn renamed_copper_layers_are_not_judged() {
    let text = two_parts(GND_TRACK).replace(r#"(0 "F.Cu" signal)"#, r#"(0 "Front" signal)"#);
    assert!(lint(&text).is_empty());
}

// --- PCB versus schematic -------------------------------------------------

/// Boards standing for a layout and its schematic: only the netlists matter to
/// the comparison, so both sides are written as layouts.
fn parity(layout: &str, schematic: &str) -> Vec<LintFinding> {
    let (pcb_text, sch_text) = (board(layout), board(schematic));
    let pcb = ExtractedBoard::from_kicad_pcb(&pcb_text).unwrap();
    let sch = ExtractedBoard::from_kicad_pcb(&sch_text).unwrap();
    pcb.layout_lint("", Some(&sch))
        .findings
        .into_iter()
        .filter(|f| f.check == LintCheck::PcbSchematicMismatch)
        .collect()
}

fn three(r3_pad2: &str, r3_extra: &str) -> String {
    format!(
        "{}{}{}",
        part("R1", 0.0, A, GND, ""),
        part("R2", 10.0, A, GND, ""),
        part("R3", 20.0, A, r3_pad2, r3_extra)
    )
}

#[test]
fn a_pin_left_off_ground_on_the_layout_is_a_mismatch() {
    let found = parity(&three(r#"(net 0 "")"#, ""), &three(GND, ""));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].severity, Severity::High);
    assert!(
        found[0].message.starts_with("R3.2 "),
        "{}",
        found[0].message
    );
    assert!(found[0].message.contains("'GND'"), "{}", found[0].message);
    assert!(parity(&three(GND, ""), &three(GND, "")).is_empty());
}

/// A two-pin switch or resistor whose pads land on each other's schematic
/// nets wires the same circuit; a diode turned round does not.
#[test]
fn swapped_ends_mismatch_only_on_a_polarised_part() {
    for (reference, want) in [("SW3", 0), ("R3", 0), ("D3", 1)] {
        let parts = |a: &str, b: &str| {
            format!(
                "{}{}{}",
                part("R1", 0.0, A, GND, ""),
                part("R2", 10.0, A, GND, ""),
                part(reference, 20.0, a, b, "")
            )
        };
        let found = parity(&parts(GND, A), &parts(A, GND));
        assert_eq!(found.len(), want, "{reference}: {found:?}");
    }
}

#[test]
fn a_renamed_net_with_the_same_pins_is_not_a_mismatch() {
    let renamed = board(&three(GND, "")).replace(r#""GND""#, r#""VSS""#);
    let pcb = ExtractedBoard::from_kicad_pcb(&renamed).unwrap();
    let sch = ExtractedBoard::from_kicad_pcb(&board(&three(GND, ""))).unwrap();
    assert!(pcb.layout_lint("", Some(&sch)).findings.is_empty());
}

/// An IC `U1` with the given pads, each `(number, net clause)`, beside R1 and
/// R2 so the nets carry other pins; pads sit 1.5 mm apart.
fn ic_with(pads: &[(&str, &str)]) -> String {
    let mut x = 0.0;
    let mut pad_text = String::new();
    for (number, net) in pads {
        writeln!(
            pad_text,
            "    (pad \"{number}\" smd rect (at {x} 0) (size 1 1) (layers \"F.Cu\") {net})"
        )
        .unwrap();
        x += 1.5;
    }
    format!(
        "{}{}  (net 3 \"unconnected-(U1-Pad3)\")\n  (footprint \"lib:IC\" (layer \"F.Cu\") (at 30 0)\n    (property \"Reference\" \"U1\")\n{pad_text}  )\n",
        part("R1", 0.0, A, GND, ""),
        part("R2", 10.0, A, GND, "")
    )
}

const UNCONNECTED: &str = r#"(net 3 "unconnected-(U1-Pad3)")"#;

#[test]
fn exposed_pad_numbering_artefacts_are_not_a_mismatch() {
    // The symbol has pins 1, 2 and EP (on ground). The footprint numbers its
    // thermal pad "EP", splits it into a second "EP" pad with no net (a paste
    // aperture), and adds a mounting pad "0" the symbol does not have.
    let schematic = ic_with(&[("1", A), ("2", GND), ("EP", GND)]);
    let layout = ic_with(&[
        ("1", A),
        ("2", GND),
        ("EP", GND),
        ("EP", r#"(net 0 "")"#),
        ("0", r#"(net 0 "")"#),
    ]);
    let found = parity(&layout, &schematic);
    assert!(found.is_empty(), "{found:?}");
    // The same artefacts do not hide a real move: the thermal pad on the
    // wrong net is still reported.
    let moved = ic_with(&[("1", A), ("2", GND), ("EP", A), ("EP", r#"(net 0 "")"#)]);
    let found = parity(&moved, &schematic);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].message.contains("U1.EP"), "{}", found[0].message);
}

#[test]
fn no_connect_pad_artefacts_are_not_a_mismatch() {
    // Pin 3 is a no-connect on the schematic; the layout gives it KiCad's own
    // single-pad "unconnected" net, and carries a pad 4 the symbol lacks.
    let schematic = ic_with(&[("1", A), ("2", GND), ("3", r#"(net 0 "")"#)]);
    let layout = ic_with(&[("1", A), ("2", GND), ("3", UNCONNECTED), ("4", UNCONNECTED)]);
    let found = parity(&layout, &schematic);
    assert!(found.is_empty(), "{found:?}");
    // A no-connect pin that the layout ties into a live net is a real fault.
    let tied = ic_with(&[("1", A), ("2", GND), ("3", A)]);
    let found = parity(&tied, &schematic);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].message.starts_with("U1.3 "),
        "{}",
        found[0].message
    );
}

#[test]
fn unfitted_and_one_sided_parts_are_not_compared() {
    // R3 differs, but it is do-not-populate on the layout.
    let dnp = parity(&three(r#"(net 0 "")"#, "(attr smd dnp)"), &three(GND, ""));
    assert!(dnp.is_empty(), "{dnp:?}");
    // A part only the layout has is not compared either.
    let extra = format!(
        "{}{}",
        three(GND, ""),
        part("R9", 40.0, A, r#"(net 0 "")"#, "")
    );
    assert!(parity(&extra, &three(GND, "")).is_empty());
}

#[test]
fn a_through_hole_pad_joins_every_copper_layer_whatever_it_lists() {
    // R2 sits at x=10; the route reaches it on the back layer only.
    let back = r#"  (segment (start 0 0) (end 5 0) (width 0.25) (layer "F.Cu") (net 1))
  (via (at 5 0) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1))
  (segment (start 5 0) (end 10 0) (width 0.25) (layer "B.Cu") (net 1))
"#;
    let text = two_parts(&format!("{back}{GND_TRACK}"));
    assert!(!high(&lint(&text)).is_empty(), "an SMD pad on F.Cu is open");
    let through = text.replace(
        r#"(pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "A"))"#,
        r#"(pad "1" thru_hole circle (at 0 0) (size 1 1) (drill 0.5) (layers "F.Cu") (net 1 "A"))"#,
    );
    let found = lint(&through);
    assert!(high(&found).is_empty(), "{found:?}");
}

#[test]
fn a_layout_with_no_routing_at_all_is_one_blocking_finding_not_one_per_pad() {
    let bare = lint(&two_parts(""));
    assert_eq!(bare.len(), 1, "{bare:?}");
    // Blocking: an unrouted board must never read as ready to order.
    assert_eq!(bare[0].severity, Severity::High);
    assert!(bare[0].message.contains("not routed yet"));
    assert!(bare[0].message.contains("cannot be ordered"));
    // Once routing has started, an open pad is a real finding.
    assert_eq!(high(&lint(&two_parts(GND_TRACK))).len(), 1);
}

// --- Part identity across the two files ------------------------------------

/// R1, R2 and an IC U1 whose value field is `u1_value`.
fn with_ic(u1_value: &str) -> String {
    let u1 = part("U1", 20.0, A, GND, "").replacen(
        "(property \"Reference\" \"U1\")",
        &format!("(property \"Reference\" \"U1\") (property \"Value\" \"{u1_value}\")"),
        1,
    );
    format!(
        "{}{}{u1}",
        part("R1", 0.0, A, GND, ""),
        part("R2", 10.0, A, GND, "")
    )
}

#[test]
fn a_part_whose_value_differs_between_the_files_is_one_warning() {
    let found = parity(&with_ic("TPS62130"), &with_ic("TPS62160"));
    assert_eq!(found.len(), 1, "{found:?}");
    // The layout's part is what gets built and what was simulated: a
    // non-gating item to confirm before ordering, never a failing warning.
    assert_eq!(found[0].severity, Severity::Low);
    assert!(found[0].message.starts_with(CHECK_BEFORE_ORDERING));
    assert!(found[0].message.contains("part that gets built"));
    assert_eq!(found[0].refs, vec!["U1".to_string()]);
    let m = &found[0].message;
    assert!(m.contains("'TPS62160' in the schematic"), "{m}");
    assert!(m.contains("'TPS62130' on the PCB"), "{m}");
    assert!(m.contains("analysed the PCB's 'TPS62130'"), "{m}");
}

#[test]
fn identical_parts_and_case_or_spacing_differences_are_not_reported() {
    assert!(parity(&with_ic("TPS62130"), &with_ic("TPS62130")).is_empty());
    assert!(parity(&with_ic("tps62130 "), &with_ic("TPS 62130")).is_empty());
    // A blank field on one side is missing data, not a disagreement.
    assert!(parity(&with_ic(""), &with_ic("TPS62130")).is_empty());
}

// --- What a value disagreement means electrically ---------------------------

/// A board holding R9 plus one part `reference` carrying `value` and, when
/// given, a manufacturer part number property.
fn fields(reference: &str, value: &str, mpn: Option<&str>) -> String {
    let mpn = mpn.map_or(String::new(), |m| format!(r#"(property "MPN" "{m}")"#));
    let p = part(reference, 20.0, A, GND, "").replacen(
        &format!("(property \"Reference\" \"{reference}\")"),
        &format!("(property \"Reference\" \"{reference}\") (property \"Value\" \"{value}\") {mpn}"),
        1,
    );
    format!("{}{p}", part("R9", 0.0, A, GND, ""))
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Nothing,
    Note,
    Warning,
}

fn outcome(found: &[LintFinding]) -> Outcome {
    assert!(found.len() <= 1, "{found:?}");
    match found.first().map(|f| f.severity) {
        None => Outcome::Nothing,
        Some(Severity::Low) if found[0].message.starts_with(CHECK_BEFORE_ORDERING) => {
            Outcome::Warning
        }
        Some(Severity::Low) => Outcome::Note,
        Some(Severity::Medium) => panic!("a part identity mismatch must not gate: {found:?}"),
        Some(Severity::High) => panic!("a part identity mismatch must not be high: {found:?}"),
    }
}

#[test]
fn value_pairs_are_judged_by_electrical_meaning() {
    use Outcome::{Note, Nothing, Warning};
    // (reference, schematic value, layout value, outcome)
    let table = [
        // The same quantity with ratings, tolerance or dielectric on one side.
        ("C1", "100uF 50V", "100uF/50V", Nothing),
        ("R1", "100k 1%", "100k", Nothing),
        ("L1", "33uH 2A", "33uH", Nothing),
        ("C2", "10nf", "10n", Nothing),
        ("C3", "1n 50V C0G", "1n", Nothing),
        ("C4", "10nF", "10n", Nothing),
        ("C5", "0.1uF", "100n", Nothing),
        // The same quantity spelled with a different SI prefix, or with the
        // unit in its own word: 0.22 nF is 220 pF, 1000 nF is 1 uF.
        ("C8", "0.22nF", "220 pF", Nothing),
        ("C9", "1000nF", "1 uF", Nothing),
        ("C10", "100 nF", "100000pF", Nothing),
        ("C11", "10uF 25V", "10 uF", Nothing),
        ("C12", "100 nF", "0.1uF", Nothing),
        ("R8", "1780", "1.78 kΩ", Nothing),
        ("R10", "100000", "100 kΩ", Nothing),
        ("R11", "100k 1%", "100 kΩ", Nothing),
        ("R13", "4.7 k", "4k7", Nothing),
        // And the same spellings of different quantities still gate.
        ("C13", "0.22nF", "2.2 nF", Warning),
        ("C14", "1 uF", "1 nF", Warning),
        ("C15", "100 nF", "100 pF", Warning),
        ("R14", "1780", "1.8 kΩ", Warning),
        ("R15", "100000", "10 kΩ", Warning),
        // A different primary quantity gates, in both directions.
        ("R2", "12k", "5k6", Warning),
        ("R3", "5k6", "12k", Warning),
        ("C6", "100nF", "10nF", Warning),
        ("C7", "10uF", "10uH", Warning),
        // An extra word that is not a rating: the same value, one note.
        ("RV1", "50k/log SPST", "50k/log", Note),
        // Descriptions, codes and placeholders cannot be compared.
        ("F1", "PTC 750mA / 33V", "1812L075/33DR", Note),
        (
            "J1",
            "Conn 2x5 shrouded header",
            "Pin header 2.54mm 2x5",
            Note,
        ),
        ("U2", "CAT24M01YI-GT3", "C416112", Note),
        ("FB1", "Ferrite Bead", "100MHz", Note),
        ("R4", "1k", "?", Note),
        ("R5", "?", "10k", Note),
        // Different part numbers: both sides name a part, and not the same one.
        ("U3", "CD74HCT640M", "74HCT245", Warning),
        ("U4", "CAT24M01YI-GT3", "CAT24M01XI-T2", Warning),
        ("U5", "TPS62130", "TPS62160", Warning),
        // The same base part, different ordering or packaging.
        ("U6", "CAT24M01YI-GT3", "CAT24M01YI-T2", Note),
        ("U7", "SN74LVC1G14DBVR", "SN74LVC1G14DBVT", Note),
        ("U8", "TPS62130RGTR", "tps62130rgtr", Nothing),
        // Blank fields and case or spacing are not disagreements.
        ("R6", "", "10k", Nothing),
        ("R7", "10 K", "10k", Nothing),
    ];
    for (reference, schematic, layout, expected) in table {
        let found = parity(
            &fields(reference, layout, None),
            &fields(reference, schematic, None),
        );
        assert_eq!(
            outcome(&found),
            expected,
            "{reference}: schematic '{schematic}' vs layout '{layout}': {found:?}"
        );
        if let Some(f) = found.first() {
            assert!(
                f.message.contains(schematic.trim()) && f.message.contains(layout.trim()),
                "the finding names both strings: {}",
                f.message
            );
            assert_eq!(
                f.message.contains("analysed the PCB's"),
                expected == Warning,
                "only a check-before-ordering item says the verdict used the PCB's part: {}",
                f.message
            );
        }
    }
}

#[test]
fn a_note_names_the_extra_spec_it_found() {
    let found = parity(
        &fields("RV1", "50k/log", None),
        &fields("RV1", "50k/log SPST", None),
    );
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].message.contains("spst"), "{}", found[0].message);
}

#[test]
fn part_number_properties_follow_the_same_boundary() {
    use Outcome::{Note, Nothing, Warning};
    // (schematic MPN, layout MPN, outcome), with an agreeing value field.
    let table = [
        ("CAT24M01YI-GT3", "CAT24M01XI-T2", Warning),
        ("CD74HCT640M", "74HCT245", Warning),
        ("CAT24M01YI-GT3", "CAT24M01YI-T2", Note),
        ("CAT24M01YI-GT3", "cat24m01yi-gt3", Nothing),
        ("CAT24M01YI-GT3", "C416112", Note),
    ];
    for (schematic, layout, expected) in table {
        let found = parity(
            &fields("U1", "EEPROM", Some(layout)),
            &fields("U1", "EEPROM", Some(schematic)),
        );
        assert_eq!(
            outcome(&found),
            expected,
            "MPN '{schematic}' vs '{layout}': {found:?}"
        );
    }
}
