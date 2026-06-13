//! Geometric DRC tests: hand-authored `.kicad_pcb` fixtures with one
//! deliberate violation of each geometry kind, asserting exact detection, plus
//! a corpus sweep asserting the known-good boards report zero true shorts.

use hauksbee_extract::{ExtractedBoard, ViolationKind};

/// Wrap copper items in a minimal KiCad 7+ board with two declared signal nets
/// (`A`, `B`) plus GND, and a default-clearance setup.
fn board(items: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers
    (0 "F.Cu" signal)
    (31 "B.Cu" signal)
  )
  (net 0 "")
  (net 1 "A")
  (net 2 "B")
  (net 3 "GND")
{items}
)
"#
    )
}

fn drc(items: &str) -> hauksbee_extract::DrcReport {
    ExtractedBoard::drc(&board(items)).expect("drc runs")
}

/// A short between nets A and B exists with the expected item kinds.
fn assert_short(
    report: &hauksbee_extract::DrcReport,
    want_a: &str,
    want_b: &str,
) {
    let found = report.shorts().any(|f| {
        let names = [f.net_a_name.as_str(), f.net_b_name.as_str()];
        names.contains(&want_a) && names.contains(&want_b)
    });
    assert!(
        found,
        "expected a SHORT between {want_a} and {want_b}; got {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.net_a_name.clone(), f.net_b_name.clone(), f.gap_mm))
            .collect::<Vec<_>>()
    );
}

#[test]
fn segment_segment_overlap_is_a_short() {
    // Two crossing 0.5 mm-wide tracks on F.Cu, different nets: they intersect,
    // a true short.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
  (segment (start 5 -5) (end 5 5) (width 0.5) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert_eq!(report.short_count(), 1, "exactly one short");
    assert_short(&report, "A", "B");
    let f = report.shorts().next().unwrap();
    assert_eq!(f.layer, "F.Cu");
    assert!(f.gap_mm <= 0.0, "overlap gap is non-positive ({})", f.gap_mm);
}

#[test]
fn parallel_tracks_within_clearance_are_a_clearance_violation() {
    // Two parallel 0.25 mm tracks whose copper edges are 0.1 mm apart (centres
    // 0.35 mm apart, both half-widths 0.125): under the 0.2 mm default rule but
    // not touching → a clearance violation, not a short.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 0.35) (end 10 0.35) (width 0.25) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert_eq!(report.short_count(), 0, "no true short");
    let cv: Vec<_> = report.clearance_violations().collect();
    assert!(!cv.is_empty(), "a clearance violation is reported");
    let f = cv[0];
    assert_eq!(f.kind, ViolationKind::Clearance);
    assert!(
        f.gap_mm > 0.0 && f.gap_mm < report.clearance_mm,
        "gap {:.3} is positive and under the {:.3} mm rule",
        f.gap_mm,
        report.clearance_mm
    );
}

#[test]
fn well_separated_tracks_report_nothing() {
    // 5 mm apart: no finding at all.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 5) (end 10 5) (width 0.25) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert!(report.findings.is_empty(), "nothing reported: {:?}", report.findings.len());
}

#[test]
fn gap_at_the_rule_is_not_a_clearance_violation() {
    // Two 0.25 mm tracks whose copper edges are *exactly* the 0.2 mm rule apart
    // (centres 0.25 + 0.2 = 0.45 mm): routing-to-rule, not a defect. The old
    // code reported every such boundary gap, producing 137/66 spurious notes on
    // the hunt boards. It must now be silent.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 0.45) (end 10 0.45) (width 0.25) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert!(
        report.findings.is_empty(),
        "a gap at the rule is not a violation: {:?}",
        report.findings.iter().map(|f| f.gap_mm).collect::<Vec<_>>()
    );
}

#[test]
fn gap_a_micron_under_the_rule_is_not_a_clearance_violation() {
    // Edges 0.1995 mm apart: half a micron under the 0.2 mm rule, inside the
    // tolerance band. Centres 0.25 + 0.1995 = 0.4495 mm. Still silent.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 0.4495) (end 10 0.4495) (width 0.25) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert!(
        report.findings.is_empty(),
        "a sub-micron-under-rule gap is not a violation: {:?}",
        report.findings.iter().map(|f| f.gap_mm).collect::<Vec<_>>()
    );
}

#[test]
fn genuinely_sub_rule_gap_still_fires() {
    // Edges 0.15 mm apart (centres 0.25 + 0.15 = 0.40 mm): a real 25% encroach
    // past the tolerance band. This must still fire so the tolerance does not
    // blind the check to true clearance violations.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 0.40) (end 10 0.40) (width 0.25) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert_eq!(report.short_count(), 0, "not a short");
    assert_eq!(report.clearance_violations().count(), 1, "a real clearance violation fires");
    let f = report.clearance_violations().next().unwrap();
    assert!((f.gap_mm - 0.15).abs() < 1e-6, "gap ~0.15 mm, got {}", f.gap_mm);
}

#[test]
fn touching_copper_at_the_rule_is_still_a_short() {
    // The tolerance only relaxes the soft clearance band; a true overlap (gap
    // <= 0) is always a short regardless of the rule. Crossing tracks here.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
  (segment (start 5 -5) (end 5 5) (width 0.5) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert_eq!(report.short_count(), 1, "overlap is still a short");
}

#[test]
fn segment_pad_overlap_is_a_short() {
    // A track on net A driven straight through a footprint pad on net B.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "F.Cu") (net 1))
  (footprint "lib:fp" (layer "F.Cu") (at 5 0)
    (property "Reference" "U1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 1.5 1.5) (layers "F.Cu") (net 2))
  )
"#;
    let report = drc(items);
    assert_short(&report, "A", "B");
    let f = report.shorts().next().unwrap();
    // The pad owner is captured.
    let owners = [f.item_a.owner.as_str(), f.item_b.owner.as_str()];
    assert!(owners.contains(&"U1"), "pad owner U1 recorded: {owners:?}");
}

#[test]
fn pad_pad_overlap_is_a_short() {
    // Two SMD pads on different nets, in different footprints, overlapping.
    let items = r#"
  (footprint "lib:fp" (layer "F.Cu") (at 5 5)
    (property "Reference" "U1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 1))
  )
  (footprint "lib:fp" (layer "F.Cu") (at 6 5)
    (property "Reference" "U2" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 2))
  )
"#;
    // Pads centred 1 mm apart, each 2 mm wide → overlap by 1 mm.
    let report = drc(items);
    assert_eq!(report.short_count(), 1);
    assert_short(&report, "A", "B");
}

#[test]
fn same_footprint_pads_do_not_short() {
    // Two abutting different-net pads inside ONE footprint (a fuse-clip style
    // placement): the footprint author's intent, not a board short.
    let items = r#"
  (footprint "lib:fuse" (layer "F.Cu") (at 5 5)
    (property "Reference" "F1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.9 0) (size 1 1) (layers "F.Cu") (net 2))
  )
"#;
    let report = drc(items);
    assert_eq!(report.short_count(), 0, "intra-footprint abutment is not a short");
}

#[test]
fn via_zone_overlap_is_a_short() {
    // A via on net A dropped into a filled GND pour on B.Cu: the via lands
    // inside the pour polygon (containment short).
    let items = r#"
  (via (at 5 5) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (zone (net 3) (net_name "GND") (layer "B.Cu")
    (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))
    (filled_polygon (layer "B.Cu")
      (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))
    )
  )
"#;
    let report = drc(items);
    assert_short(&report, "A", "GND");
    let f = report.shorts().next().unwrap();
    assert_eq!(f.layer, "B.Cu");
}

#[test]
fn via_outside_zone_is_clean() {
    // The same pour, but the via sits well outside it: nothing reported.
    let items = r#"
  (via (at 50 50) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (zone (net 3) (net_name "GND") (layer "B.Cu")
    (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))
    (filled_polygon (layer "B.Cu")
      (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))
    )
  )
"#;
    let report = drc(items);
    assert!(report.is_clean(), "no shorts: {}", report.short_count());
}

#[test]
fn different_layers_do_not_short() {
    // Two overlapping tracks but on opposite copper layers: no short (they are
    // separated by the dielectric).
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
  (segment (start 5 -5) (end 5 5) (width 0.5) (layer "B.Cu") (net 2))
"#;
    let report = drc(items);
    assert!(report.is_clean(), "cross-layer crossings are not shorts");
}

#[test]
fn via_spans_layers_and_shorts_on_either() {
    // A via spans F.Cu and B.Cu; a track on a *different* net on F.Cu hitting
    // the via is a short even though the track is single-layer.
    let items = r#"
  (via (at 5 0) (size 1.0) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert_short(&report, "A", "B");
}

#[test]
fn clearance_override_changes_classification() {
    // Tracks 0.3 mm edge-to-edge. Under the default 0.2 mm rule: clean. Under a
    // forced 0.5 mm rule: a clearance violation.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.2) (layer "F.Cu") (net 1))
  (segment (start 0 0.5) (end 10 0.5) (width 0.2) (layer "F.Cu") (net 2))
"#;
    let doc = forge_sexpr::parse(&board(items)).unwrap();
    let lax = hauksbee_extract::run_drc(&doc, Some(0.2));
    assert!(lax.is_clean() && lax.findings.is_empty(), "0.2 mm rule: clean");
    let strict = hauksbee_extract::run_drc(&doc, Some(0.5));
    assert_eq!(strict.short_count(), 0);
    assert!(
        strict.clearance_violations().count() >= 1,
        "0.5 mm rule flags the 0.3 mm gap"
    );
}
