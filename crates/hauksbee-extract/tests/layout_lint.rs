//! Layout wiring checks on hand-authored boards: each fault next to its legal
//! twin, so a check that stops firing (or fires on the legal form) fails here.

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, Severity};

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

#[test]
fn a_renamed_net_with_the_same_pins_is_not_a_mismatch() {
    let renamed = board(&three(GND, "")).replace(r#""GND""#, r#""VSS""#);
    let pcb = ExtractedBoard::from_kicad_pcb(&renamed).unwrap();
    let sch = ExtractedBoard::from_kicad_pcb(&board(&three(GND, ""))).unwrap();
    assert!(pcb.layout_lint("", Some(&sch)).findings.is_empty());
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
