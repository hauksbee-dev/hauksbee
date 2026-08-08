//! Geometric DRC tests: hand-authored `.kicad_pcb` fixtures with one
//! deliberate violation of each geometry kind, asserting exact detection, plus
//! a corpus sweep asserting the known-good boards report zero true shorts.

use hauksbee_extract::{ClearanceRules, ExtractedBoard, NetClassRule, ViolationKind};

const KICAD_10_KEYHOLE_ANTIPAD_BOARD: &str =
    include_str!("fixtures/kicad_10_keyhole_antipad.kicad_pcb");
const EXPECTED_SOLID_POUR_SHORTS: usize = 1;

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
fn assert_short(report: &hauksbee_extract::DrcReport, want_a: &str, want_b: &str) {
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
    assert!(
        f.gap_mm <= 0.0,
        "overlap gap is non-positive ({})",
        f.gap_mm
    );
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

// ---------------------------------------------------------------------------
// The touching band. Copper that meets is a short; the test for "meets" cannot
// be `gap > 0.0`, because the gap is measured through a square root in
// millimetres and an exact meeting can come out a hair positive. A corpus board
// produced 9.77e-15 mm between two nets and the bare test filed it as a
// clearance note. These two pin both sides of SHORT_TOUCH_EPS_MM: inside the
// band is a short, and the finest gap KiCad's nanometre grid can express is
// still comfortably outside it.
//
// The geometry: segment A ends at (1,0), segment B starts at (2,3), so the
// centreline distance is sqrt(10) = 3.1622776601683795, and the closest approach
// is endpoint-to-endpoint. The two half-widths are chosen to sum to (almost
// exactly) that distance, which is how the gap lands in the noise instead of on
// a round number.
// ---------------------------------------------------------------------------

#[test]
fn a_gap_inside_the_touching_band_is_a_short_not_a_clearance_note() {
    // Half-widths 1.5 + 1.66227766016837065 = sqrt(10) to within ~9e-15 mm.
    // That is copper meeting copper by any physical reading.
    let items = r#"
  (segment (start 0 0) (end 1 0) (width 3.0) (layer "F.Cu") (net 1))
  (segment (start 2 3) (end 5 7) (width 3.3245553203367413) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    let f = report
        .findings
        .iter()
        .find(|f| {
            let n = [f.net_a_name.as_str(), f.net_b_name.as_str()];
            n.contains(&"A") && n.contains(&"B")
        })
        .expect("A and B are reported against each other");
    // Strictly POSITIVE and inside the band is the whole point. A gap of exactly
    // zero would be caught by the old `gap <= 0.0` test too, so this fixture
    // would not discriminate; a positive one under the band is the case that used
    // to be filed as a clearance note.
    assert!(
        f.gap_mm > 0.0 && f.gap_mm < hauksbee_extract::SHORT_TOUCH_EPS_MM,
        "the fixture must land strictly inside the touching band on the positive \
         side, measured {:e}",
        f.gap_mm
    );
    assert_eq!(
        f.kind,
        ViolationKind::Short,
        "copper meeting to within {:e} mm is a SHORT, got {:?}",
        f.gap_mm,
        f.kind
    );
    assert_eq!(report.short_count(), 1);
}

#[test]
fn a_one_nanometre_gap_is_outside_the_touching_band() {
    // The same geometry with segment B narrowed so the gap is 1e-6 mm: one
    // nanometre, the smallest non-zero gap a KiCad file can express, and a
    // thousand times the touching band. It must stay a clearance violation, or
    // the band would be swallowing real separation.
    let items = r#"
  (segment (start 0 0) (end 1 0) (width 3.0) (layer "F.Cu") (net 1))
  (segment (start 2 3) (end 5 7) (width 3.324553320336759) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert_eq!(
        report.short_count(),
        0,
        "a one-nanometre gap is separation, not contact: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.gap_mm))
            .collect::<Vec<_>>()
    );
    let f = report
        .clearance_violations()
        .find(|f| {
            let n = [f.net_a_name.as_str(), f.net_b_name.as_str()];
            n.contains(&"A") && n.contains(&"B")
        })
        .expect("still under the 0.2 mm rule, so still a clearance violation");
    assert!(
        f.gap_mm > hauksbee_extract::SHORT_TOUCH_EPS_MM,
        "gap {:e} must be outside the touching band",
        f.gap_mm
    );
}

#[test]
fn the_touching_predicate_covers_overlap_abutment_and_float_noise() {
    use hauksbee_extract::{is_touching, SHORT_TOUCH_EPS_MM};
    // Overlap and exact abutment: contact, and always were.
    assert!(is_touching(-0.01));
    assert!(is_touching(0.0));
    // The measurement that started this: 9.77e-15 mm between two nets on a
    // corpus board, which the old `gap <= 0.0` test called a clearance note.
    assert!(is_touching(9.77e-15));
    // The band is inclusive at its edge, and one ulp past it is separation.
    assert!(is_touching(SHORT_TOUCH_EPS_MM));
    assert!(!is_touching(SHORT_TOUCH_EPS_MM * 1.000_001));
    // KiCad's finest expressible gap, and a real fab clearance: separation.
    assert!(!is_touching(1e-6));
    assert!(!is_touching(0.075));
}

#[test]
fn well_separated_tracks_report_nothing() {
    // 5 mm apart: no finding at all.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 5) (end 10 5) (width 0.25) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert!(
        report.findings.is_empty(),
        "nothing reported: {:?}",
        report.findings.len()
    );
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
    assert_eq!(
        report.clearance_violations().count(),
        1,
        "a real clearance violation fires"
    );
    let f = report.clearance_violations().next().unwrap();
    assert!(
        (f.gap_mm - 0.15).abs() < 1e-6,
        "gap ~0.15 mm, got {}",
        f.gap_mm
    );
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
fn ordinary_same_footprint_pads_still_short() {
    // Sharing an ordinary component owner is not evidence that two different
    // nets are intentionally tied.
    let items = r#"
  (footprint "lib:fuse" (layer "F.Cu") (at 5 5)
    (property "Reference" "F1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.9 0) (size 1 1) (layers "F.Cu") (net 2))
  )
"#;
    let report = drc(items);
    assert_short(&report, "A", "B");
}

#[test]
fn native_net_tie_groups_work_with_house_footprint_names_and_stay_local() {
    let items = r#"
  (footprint "Acme:KelvinBridge" (layer "F.Cu") (at 20 20)
    (property "Reference" "NT1" (at 0 0))
    (property "Value" "HOUSE_PART_42" (at 0 1))
    (attr net_tie)
    (net_tie_pad_groups "1, 2")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.9 0) (size 1 1) (layers "F.Cu") (net 2))
  )
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "F.Cu") (net 1))
  (segment (start 5 -5) (end 5 5) (width 0.4) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert_eq!(
        report.short_count(),
        1,
        "only the remote A/B collision fires"
    );
    assert_short(&report, "A", "B");
}

#[test]
fn native_net_tie_does_not_hide_ordinary_copper_crossing_over_its_pads() {
    // The ordinary A/B tracks cross exactly on NT1's overlapping copper.
    // Sharing the tie's net pair and location is still not enough: neither
    // track belongs to the explicitly identified link footprint.
    let items = r#"
  (footprint "Acme:KelvinBridge" (layer "F.Cu") (at 5 5)
    (property "Reference" "NT1" (at 0 0))
    (attr net_tie)
    (net_tie_pad_groups "1, 2")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.9 0) (size 1 1) (layers "F.Cu") (net 2))
  )
  (segment (start 0 5) (end 10 5) (width 0.2) (layer "F.Cu") (net 1))
  (segment (start 5.45 0) (end 5.45 10) (width 0.2) (layer "F.Cu") (net 2))
"#;
    assert_short(&drc(items), "A", "B");
}

#[test]
fn native_net_tie_pad_groups_never_exempt_cross_group_contacts() {
    // KiCad permits several independent ties in one footprint. Pads 1/2 and
    // 3/4 are legal contacts, but the B/GND contact between pads 2 and 3 is a
    // real cross-group short and must remain visible.
    let items = r#"
  (net 4 "D")
  (footprint "Acme:FourTerminalBridge" (layer "F.Cu") (at 20 20)
    (property "Reference" "NT1" (at 0 0))
    (attr net_tie)
    (net_tie_pad_groups "1, 2" "3, 4")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.9 0) (size 1 1) (layers "F.Cu") (net 2))
    (pad "3" smd rect (at 1.8 0) (size 1 1) (layers "F.Cu") (net 3))
    (pad "4" smd rect (at 2.7 0) (size 1 1) (layers "F.Cu") (net 4))
  )
"#;
    let report = drc(items);
    assert_eq!(
        report.short_count(),
        1,
        "only the cross-group B/GND contact is illegal: {:?}",
        report.findings
    );
    assert_short(&report, "B", "GND");
}

#[test]
fn legacy_closed_pair_metadata_keeps_other_jumper_pads_separate() {
    // Older EAGLE-to-KiCad conversions can retain a structured closed-pair
    // declaration but no native net_tie attr. Both fields are required, and
    // only the declared 1/2 pair is legal; the 2/3 contact still fires.
    let items = r#"
  (footprint "Vendor:SJ_2_SMALL_12_TIED" (layer "F.Cu") (at 20 20)
    (property "Reference" "JP1" (at 0 0))
    (property "Value" "Closed(1-2)/Opened(2-3)" (at 0 1))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.9 0) (size 1 1) (layers "F.Cu") (net 2))
    (pad "3" smd rect (at 1.8 0) (size 1 1) (layers "F.Cu") (net 3))
  )
"#;
    let report = drc(items);
    assert_eq!(report.short_count(), 1, "only 2/3 is illegal");
    assert_short(&report, "B", "GND");
}

#[test]
fn jumper_like_names_without_native_semantics_do_not_exempt_copper() {
    // A library/package/value name is descriptive text, not a KiCad DRC
    // declaration. Without attr/groups this ordinary part is checked.
    let items = r#"
  (footprint "Jumper:SolderJumper_2_Open" (layer "F.Cu") (at 5 5)
    (property "Reference" "JP1" (at 0 0))
    (property "Value" "SOLDER_JUMPER" (at 0 1))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.9 0) (size 1 1) (layers "F.Cu") (net 2))
  )
"#;
    assert_short(&drc(items), "A", "B");
}

#[test]
fn explicit_zero_ohm_copper_link_exemption_is_local() {
    // Some legacy footprints state their copper-link semantics explicitly in
    // both footprint and value, and include an auxiliary same-number copper pad
    // that reaches the opposite terminal. Only that local geometry is waived.
    let items = r#"
  (footprint "Vendor:0R_0603" (layer "F.Cu") (at 20 20)
    (property "Reference" "R25" (at 0 0))
    (fp_text value "0R(board_mounted)" (at 0 1) (layer "F.Fab"))
    (pad "1" smd rect (at -0.889 0) (size 1.016 1.016) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.889 0) (size 1.016 1.016) (layers "F.Cu") (net 2))
    (pad "1" smd rect (at 0 0) (size 0.78 0.5) (layers "F.Cu") (net 1))
  )
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "F.Cu") (net 1))
  (segment (start 5 -5) (end 5 5) (width 0.4) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert_eq!(
        report.short_count(),
        1,
        "the explicit link is local; the remote collision still fires"
    );
    assert_short(&report, "A", "B");
}

#[test]
fn zero_ohm_drc_exception_requires_value_and_dedicated_footprint() {
    for (footprint, value) in [("Device:R_0603", "0R"), ("Vendor:0R_0603", "10k")] {
        let items = format!(
            r#"
  (footprint "{footprint}" (layer "F.Cu") (at 5 5)
    (property "Reference" "R1" (at 0 0))
    (property "Value" "{value}" (at 0 1))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.9 0) (size 1 1) (layers "F.Cu") (net 2))
  )
"#
        );
        assert_short(&drc(&items), "A", "B");
    }
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
fn kicad_10_keyhole_antipad_keeps_the_isolated_pad_silent() {
    let report = ExtractedBoard::drc(KICAD_10_KEYHOLE_ANTIPAD_BOARD).expect("drc runs");

    assert!(
        report.shorts().all(|finding| {
            finding.net_a_name != "ANTIPAD_OK" && finding.net_b_name != "ANTIPAD_OK"
        }),
        "the pad enclosed by a real KiCad-10 keyhole antipad remains isolated: {:?}",
        report.findings
    );
}

#[test]
fn a_suppressed_zone_pad_class_is_disclosed_not_silently_dropped() {
    // The keyhole board is exactly the case the suppression exists for. Dropping
    // the class is correct, but the run must say it dropped it: "no shorts" and
    // "no shorts, having declined to look at a whole category" are different
    // claims, and a reader cannot audit a rule nobody told them was applied.
    // A pad straddling a different-net pour boundary: the body crosses the edge
    // (negative gap) while its centre sits outside the fill, so this lands on
    // the Zone<->Pad overlap rule rather than the containment path.
    let items = r#"
  (footprint "lib:fp" (layer "F.Cu") (at 10.4 5)
    (property "Reference" "U1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
  )
  (zone (net 3) (net_name "GND") (layer "F.Cu")
    (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))
    (filled_polygon (layer "F.Cu")
      (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))
    )
  )
"#;
    let report = drc(items);
    assert!(
        report.zone_pad_overlaps_suppressed.unwrap_or(0) > 0,
        "this fixture is meant to exercise the antipad suppression"
    );
    assert!(
        report.shorts().all(|f| {
            let n = [f.net_a_name.as_str(), f.net_b_name.as_str()];
            !(n.contains(&"A") && n.contains(&"GND"))
        }),
        "the overlap itself stays suppressed: {:?}",
        report.findings
    );
    let note = report
        .suppression_note()
        .expect("a suppressing run must disclose it");
    assert!(note.contains("suppressed"), "{note}");
    assert!(
        note.contains("upper bound"),
        "the count is primitive pairs, not distinct overlaps, and must say so: {note}"
    );
    assert!(
        note.contains("antipad") && note.contains("KiCad"),
        "the note must say why: {note}"
    );
    assert!(
        note.contains("track, via or arc"),
        "the note must bound what IS still checked: {note}"
    );
}

#[test]
fn an_unmeasured_suppression_count_reads_as_unknown_not_none() {
    // Some(0) means "measured, nothing suppressed". None means the payload predates
    // the counter, and the run behind it DID suppress the class, so defaulting to
    // zero would assert something that run never measured.
    let mut report = ExtractedBoard::drc(&board(
        r#"(via (at 50 50) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))"#,
    ))
    .expect("drc runs");
    report.zone_pad_overlaps_suppressed = None;
    let note = report
        .suppression_note()
        .expect("an unmeasured run must not read as a clean one");
    assert!(note.contains("unknown"), "{note}");
    // The sentence must cover both ways of getting here (an old payload, or a
    // result with no copper DRC pass) without asserting either.
    assert!(
        note.contains("predates") && note.contains("no copper DRC pass"),
        "{note}"
    );

    // A default-constructed report is the "no DRC pass" case, not a clean one.
    assert!(
        hauksbee_extract::DrcReport::default()
            .suppression_note()
            .is_some_and(|n| n.contains("unknown")),
        "a report with no DRC pass must not read as measured-and-clean"
    );
}

#[test]
fn a_board_with_no_suppression_gains_no_note() {
    // The disclosure must not appear on boards where nothing was dropped, or it
    // becomes boilerplate everyone learns to skip.
    let items = r#"
  (via (at 50 50) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
"#;
    let report = ExtractedBoard::drc(&board(items)).expect("drc runs");
    assert_eq!(report.zone_pad_overlaps_suppressed, Some(0));
    assert!(report.suppression_note().is_none());
}

#[test]
fn kicad_10_solid_fill_over_a_pad_remains_reportable() {
    let report = ExtractedBoard::drc(KICAD_10_KEYHOLE_ANTIPAD_BOARD).expect("drc runs");

    assert_eq!(
        report.short_count(),
        EXPECTED_SOLID_POUR_SHORTS,
        "the solid filled polygon over PAD_ONLY_SHORT must not inherit the keyhole exemption: {:?}",
        report.findings
    );
    assert_short(&report, "PAD_ONLY_SHORT", "GND");
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
fn unfilled_multilayer_zone_outline_is_kept_on_every_layer() {
    // R11: a zone declared over BOTH copper layers with `(layers "F.Cu" "B.Cu")`
    // and no computed fill. The outline must be kept on each layer for clearance;
    // the old code read only a single `(layer ...)`, found none, and dropped
    // the whole zone, so a track crossing the pour boundary on B.Cu went unseen.
    // A track on net A crossing the left outline edge (x=10) on B.Cu now shorts
    // against the GND zone edge.
    let items = r#"
  (zone (net 3) (net_name "GND") (layers "F.Cu" "B.Cu")
    (polygon (pts (xy 10 10) (xy 30 10) (xy 30 30) (xy 10 30)))
  )
  (segment (start 5 17) (end 15 17) (width 0.4) (layer "B.Cu") (net 1))
"#;
    let report = drc(items);
    assert_short(&report, "A", "GND");
    let f = report.shorts().next().unwrap();
    assert_eq!(f.layer, "B.Cu", "the B.Cu outline edge was kept");
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
    assert!(
        lax.is_clean() && lax.findings.is_empty(),
        "0.2 mm rule: clean"
    );
    let strict = hauksbee_extract::run_drc(&doc, Some(0.5));
    assert_eq!(strict.short_count(), 0);
    assert!(
        strict.clearance_violations().count() >= 1,
        "0.5 mm rule flags the 0.3 mm gap"
    );
}

fn named_board(items: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20260206) (generator pcbnew)
  (layers
    (0 "F.Cu" signal)
    (31 "B.Cu" signal)
  )
  (net 0 "")
  (net 1 "/USB/USB_D+")
  (net 2 "/USB/USB_D-")
  (net 3 "+BATT")
{items}
)
"#
    )
}

#[test]
fn per_netclass_clearance_uses_max_of_the_two_nets() {
    // Copper edges are 0.150 mm apart. The board default is 0.127 mm, but
    // +BATT belongs to a 0.200 mm power class, so the pair must be reported.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.2) (layer "F.Cu") (net 1))
  (segment (start 0 0.35) (end 10 0.35) (width 0.2) (layer "F.Cu") (net 3))
"#;
    let mut rules = ClearanceRules::new(0.127);
    rules.add_class(NetClassRule {
        name: "power".to_string(),
        clearance_mm: 0.200,
        diff_pair_gap_mm: None,
    });
    rules.assign_net("+BATT", "power");

    let doc = forge_sexpr::parse(&named_board(items)).unwrap();
    let report = hauksbee_extract::run_drc_with_clearance_rules(&doc, Some(rules));
    assert_eq!(report.short_count(), 0);
    assert_eq!(
        report.clearance_violations().count(),
        1,
        "0.150 mm is legal for Default but illegal against +BATT's power class"
    );
    let f = report.clearance_violations().next().unwrap();
    assert!((f.required_clearance_mm - 0.200).abs() < 1e-9);
}

#[test]
fn same_class_diff_pair_uses_diff_pair_gap_not_full_clearance() {
    // USB_D+ and USB_D- edges are 0.110 mm apart. Their class clearance is
    // 0.200 mm, but the differential-pair gap is 0.090 mm, so this is legal.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.2) (layer "F.Cu") (net 1))
  (segment (start 0 0.31) (end 10 0.31) (width 0.2) (layer "F.Cu") (net 2))
"#;
    let mut rules = ClearanceRules::new(0.127);
    rules.add_class(NetClassRule {
        name: "usb".to_string(),
        clearance_mm: 0.200,
        diff_pair_gap_mm: Some(0.090),
    });
    rules.assign_net("/USB/USB_D+", "usb");
    rules.assign_net("/USB/USB_D-", "usb");

    let doc = forge_sexpr::parse(&named_board(items)).unwrap();
    let report = hauksbee_extract::run_drc_with_clearance_rules(&doc, Some(rules));
    assert!(
        report.findings.is_empty(),
        "same-class diff pair at 0.110 mm must not be flagged against 0.200 mm clearance"
    );
}

#[test]
fn kicad_pro_rules_apply_assignments_and_wildcard_patterns() {
    let pro = r#"{
      "net_settings": {
        "classes": [
          {"name": "Default", "clearance": 0.127, "diff_pair_gap": 0.09},
          {"name": "usb", "clearance": 0.2, "diff_pair_gap": 0.11}
        ],
        "netclass_assignments": {
          "+BATT": ["Default"]
        },
        "netclass_patterns": [
          {"netclass": "usb", "pattern": "/USB/USB_D?"},
          {"netclass": "usb", "pattern": "/DDR/ddr-a[0-9]"}
        ]
      }
    }"#;
    let rules = hauksbee_extract::clearance_rules_from_kicad_pro(
        pro,
        ["/USB/USB_D+", "/USB/USB_D-", "+BATT", "/DDR/ddr-a7"],
    )
    .expect("project rules parse");
    assert!((rules.clearance_for_net("/USB/USB_D+") - 0.2).abs() < 1e-9);
    assert!((rules.clearance_for_net("/DDR/ddr-a7") - 0.2).abs() < 1e-9);
    assert!((rules.clearance_for_net("+BATT") - 0.127).abs() < 1e-9);
    assert!((rules.effective_clearance("/USB/USB_D+", "/USB/USB_D-") - 0.11).abs() < 1e-9);
}

#[test]
fn diff_pair_class_with_inherited_clearance_is_kept() {
    // R14: a diff-pair class that leaves `clearance` at 0 (KiCad's "inherit the
    // board default") must NOT be discarded. The old `clearance_mm > 0` gate
    // dropped it wholesale, losing the diff_pair_gap AND making its nets fall
    // back to the wider default, so the pair at its own gap was falsely flagged.
    let pro = r#"{
      "net_settings": {
        "classes": [
          {"name": "Default", "clearance": 0.2},
          {"name": "HS", "clearance": 0.0, "diff_pair_gap": 0.1}
        ],
        "netclass_patterns": [
          {"netclass": "HS", "pattern": "/USB/USB_D?"}
        ]
      }
    }"#;
    let rules =
        hauksbee_extract::clearance_rules_from_kicad_pro(pro, ["/USB/USB_D+", "/USB/USB_D-"])
            .expect("project rules parse");
    // The class is retained: its nets got assigned and use the diff-pair gap.
    assert!(
        (rules.effective_clearance("/USB/USB_D+", "/USB/USB_D-") - 0.1).abs() < 1e-9,
        "the diff-pair gap must survive a clearance-0 class"
    );
    // Its clearance-0 resolves to the board default, not a literal 0.
    assert!((rules.clearance_for_net("/USB/USB_D+") - 0.2).abs() < 1e-9);
}

#[test]
fn one_malformed_class_does_not_discard_the_whole_ruleset() {
    // R14: a single class object missing its "name" must be skipped, not abort
    // the whole parse. The old `?` propagated None out of the function, so one
    // bad entry silently dropped EVERY class and diff-pair gap, collapsing DRC
    // to the bare default everywhere.
    let pro = r#"{
      "net_settings": {
        "classes": [
          {"clearance": 0.5},
          {"name": "usb", "clearance": 0.2, "diff_pair_gap": 0.11}
        ],
        "netclass_patterns": [
          {"netclass": "usb", "pattern": "/USB/USB_D?"}
        ]
      }
    }"#;
    let rules = hauksbee_extract::clearance_rules_from_kicad_pro(pro, ["/USB/USB_D+"])
        .expect("a malformed class must not abort the whole parse");
    assert!(
        (rules.clearance_for_net("/USB/USB_D+") - 0.2).abs() < 1e-9,
        "the well-formed usb class must survive its malformed sibling"
    );
}

/// Wrap copper items in a 4-layer board (F/In1/In2/B) with nets A and B.
fn board4(items: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers
    (0 "F.Cu" signal)
    (1 "In1.Cu" signal)
    (2 "In2.Cu" signal)
    (31 "B.Cu" signal)
  )
  (net 0 "")
  (net 1 "A")
  (net 2 "B")
{items}
)
"#
    )
}

fn drc4(items: &str) -> hauksbee_extract::DrcReport {
    ExtractedBoard::drc(&board4(items)).expect("drc runs")
}

#[test]
fn through_via_shorts_inner_layer_copper_on_4_layer_board() {
    // A through via named only (layers "F.Cu" "B.Cu") physically passes
    // through In1.Cu/In2.Cu too: a different-net track on In1.Cu hitting the
    // barrel is a short (this was silently missed when the via was bucketed
    // only onto the two named end layers).
    let items = r#"
  (via (at 5 0) (size 1.0) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "In1.Cu") (net 2))
"#;
    let report = drc4(items);
    assert_short(&report, "A", "B");
    let f = report.shorts().next().unwrap();
    assert_eq!(f.layer, "In1.Cu", "the short is found on the inner layer");
}

#[test]
fn blind_via_fills_its_inner_span() {
    // A blind via F.Cu→In2.Cu passes through In1.Cu: a different-net In1.Cu
    // track through it is a short.
    let items = r#"
  (via blind (at 5 0) (size 1.0) (drill 0.4) (layers "F.Cu" "In2.Cu") (net 1))
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "In1.Cu") (net 2))
"#;
    assert_short(&drc4(items), "A", "B");
}

#[test]
fn buried_via_stays_off_layers_outside_its_span() {
    // A buried In1.Cu→In2.Cu via does NOT reach F.Cu: a crossing F.Cu track on
    // another net is clean.
    let items = r#"
  (via blind (at 5 0) (size 1.0) (drill 0.4) (layers "In1.Cu" "In2.Cu") (net 1))
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "F.Cu") (net 2))
"#;
    let report = drc4(items);
    assert!(
        report.is_clean(),
        "buried via must not appear on F.Cu: {:?}",
        report.findings
    );
}

#[test]
fn mask_only_pad_carries_no_copper() {
    // A pad whose (layers ...) names only non-copper layers (a mask opening,
    // e.g. a fiducial window) has NO copper: it must not be stamped onto every
    // copper layer and shorted against a track running underneath.
    let items = r#"
  (footprint "Fiducial" (at 5 0)
    (property "Reference" "FID1")
    (pad "" smd rect (at 0 0) (size 2 2) (layers "F.Mask") (net 1 "A"))
  )
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert!(
        report.is_clean(),
        "mask-only pad is not copper: {:?}",
        report.findings
    );
}

/// The PolyKybd Kailh-socket proof geometry: pad `2` of SW_K_2 (2.55 x 1.54,
/// chamfer_ratio 0.5 on bottom_left + bottom_right: 0.77 mm sliced off both
/// bottom corners) exactly as the board places it, world centre
/// (64.744, 49.3715) on B.Cu.
const KAILH_CHAMFERED_PAD: &str = r#"
  (footprint "Kailh:socket" (layer "B.Cu") (at 72.304 50.6615)
    (property "Reference" "SW1")
    (pad "2" smd roundrect (at -7.56 -1.29) (size 2.55 1.54) (layers "B.Cu")
      (roundrect_rratio 0) (chamfer_ratio 0.5) (chamfer bottom_left bottom_right)
      (net 2))
  )
"#;

#[test]
fn chamfered_pad_notch_is_not_a_short() {
    // PolyKybd false-short proof case: the 45-degree /K_4/CS track threads the
    // notch the chamfer opens under the pad. As a full rectangle the pad reads
    // a -0.127 mm overlap (the false [SERIOUS] short that gated --strict);
    // the true chamfered outline clears the track by ~0.225 mm, matching
    // KiCad 9.0.3 DRC (zero shorting items).
    let track = r#"
  (segment (start 61.4 47.8) (end 66.9936 53.3936) (width 0.254) (layer "B.Cu") (net 1))
"#;
    let items = format!("{track}{KAILH_CHAMFERED_PAD}");
    let report = drc(&items);
    assert_eq!(
        report.short_count(),
        0,
        "the notch track must not short the chamfered pad: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.gap_mm))
            .collect::<Vec<_>>()
    );
    // Sharpness guard: under a deliberately wide 0.6 mm rule the pair IS
    // reported, with the true small-positive gap. A lazy outline (e.g. the
    // pad shrunk to nothing) would clear by far more than 0.6 mm.
    let doc = forge_sexpr::parse(&board(&items)).unwrap();
    let wide = hauksbee_extract::run_drc(&doc, Some(0.6));
    assert_eq!(wide.short_count(), 0);
    let f = wide
        .clearance_violations()
        .next()
        .expect("the notch gap is under 0.6 mm, so a wide rule must flag it");
    assert!(
        f.gap_mm > 0.0 && f.gap_mm < 0.6,
        "true gap is small-positive, got {}",
        f.gap_mm
    );
    assert!(
        (f.gap_mm - 0.2248).abs() < 0.005,
        "gap matches the forensic reference (+0.2248 mm), got {}",
        f.gap_mm
    );
}

#[test]
fn track_through_chamfered_pad_body_is_still_a_short() {
    // Control: the same pad, but the track runs through the un-chamfered body
    // (horizontally through the pad centre). This must stay a short, so the
    // chamfer fix cannot pass by under-sizing the pad.
    let track = r#"
  (segment (start 60 49.3715) (end 70 49.3715) (width 0.254) (layer "B.Cu") (net 1))
"#;
    let items = format!("{track}{KAILH_CHAMFERED_PAD}");
    assert_short(&drc(&items), "A", "B");
}

#[test]
fn chamfered_pad_still_shorts_at_its_unchamfered_corner() {
    // Copper clipping the pad's top-left corner, which is NOT chamfered
    // (only the two bottom corners are). The 45-degree track's centerline
    // passes 0.07 mm inside that corner; if the fix wrongly chamfered every
    // corner this would clear by ~0.47 mm and go silent.
    let track = r#"
  (segment (start 62.869 49.3015) (end 64.169 48.0015) (width 0.254) (layer "B.Cu") (net 1))
"#;
    let items = format!("{track}{KAILH_CHAMFERED_PAD}");
    assert_short(&drc(&items), "A", "B");
}

#[test]
fn short_location_is_the_closest_approach_point_not_the_segment_midpoint() {
    // A vertical stub ending 0.3 mm short of a long horizontal track, both
    // 0.5 mm wide: the copper overlaps (gap -0.2 mm) at (8, -0.15), the
    // midpoint of the centerline closest-approach span. The old code reported
    // the arithmetic midpoint of the four segment endpoints, (6.5, -0.575),
    // 1.6 mm away from the contact.
    let items = r#"
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
  (segment (start 8 -2) (end 8 -0.3) (width 0.5) (layer "F.Cu") (net 2))
"#;
    let report = drc(items);
    assert_eq!(report.short_count(), 1);
    let f = report.shorts().next().unwrap();
    assert!(
        (f.x - 8.0).abs() < 1e-6 && (f.y + 0.15).abs() < 1e-6,
        "short located at the contact point (8, -0.15), got ({}, {})",
        f.x,
        f.y
    );
}

#[test]
fn track_pad_short_location_is_at_the_pad_not_the_track_midpoint() {
    // A 20 mm track driven through a pad near its start: the reported point
    // must sit at the pad (where the copper actually meets), not at the track
    // segment's midpoint (10, 0) as the old code had it.
    let items = r#"
  (segment (start 0 0) (end 20 0) (width 0.4) (layer "F.Cu") (net 1))
  (footprint "lib:fp" (layer "F.Cu") (at 2 0)
    (property "Reference" "U1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 1.5 1.5) (layers "F.Cu") (net 2))
  )
"#;
    let report = drc(items);
    assert_short(&report, "A", "B");
    let f = report.shorts().next().unwrap();
    assert!(
        (1.0..=3.0).contains(&f.x) && f.y.abs() <= 1.0,
        "short located at the pad (x in [1, 3]), got ({}, {})",
        f.x,
        f.y
    );
}

#[test]
fn pad_without_layers_list_still_gets_all_copper() {
    // No (layers ...) list at all: keep the through-hole-style fallback that
    // places the pad on every copper layer, here it shorts a B.Cu track.
    let items = r#"
  (footprint "R" (at 5 0)
    (property "Reference" "R1")
    (pad "1" thru_hole circle (at 0 0) (size 2 2) (drill 1) (net 1 "A"))
  )
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "B.Cu") (net 2))
"#;
    assert_short(&drc(items), "A", "B");
}
