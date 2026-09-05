//! Geometric DRC tests: hand-authored `.kicad_pcb` fixtures with one
//! deliberate violation of each geometry kind, asserting exact detection, plus
//! a corpus sweep asserting the known-good boards report zero true shorts.

use hauksbee_extract::{ClearanceRules, ExtractedBoard, NetClassRule, ViolationKind};

const KICAD_10_KEYHOLE_ANTIPAD_BOARD: &str =
    include_str!("fixtures/kicad_10_keyhole_antipad.kicad_pcb");

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

/// One copper kind overlapping foreign copper: (name, four-layer board, items,
/// the two nets, the layer and the pad owner the finding must name).
// Two crossing 0.5 mm tracks on F.Cu, different nets.
const SEGMENT_SEGMENT: &str = r#"
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
  (segment (start 5 -5) (end 5 5) (width 0.5) (layer "F.Cu") (net 2))
"#;
// A track driven straight through a footprint pad; the owner is recorded.
const SEGMENT_PAD: &str = r#"
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "F.Cu") (net 1))
  (footprint "lib:fp" (layer "F.Cu") (at 5 0)
    (property "Reference" "U1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 1.5 1.5) (layers "F.Cu") (net 2))
  )
"#;
// Two 2 mm SMD pads of different footprints centred 1 mm apart.
const PAD_PAD: &str = r#"
  (footprint "lib:fp" (layer "F.Cu") (at 5 5)
    (property "Reference" "U1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 1))
  )
  (footprint "lib:fp" (layer "F.Cu") (at 6 5)
    (property "Reference" "U2" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 2 2) (layers "F.Cu") (net 2))
  )
"#;
// A via dropped inside a filled GND pour: the containment short.
const VIA_ZONE: &str = r#"
  (via (at 5 5) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (zone (net 3) (net_name "GND") (layer "B.Cu")
    (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))
    (filled_polygon (layer "B.Cu")
      (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))
    )
  )
"#;
// A via spans both layers, so a single-layer track of another net hits it.
const VIA_SEGMENT: &str = r#"
  (via (at 5 0) (size 1.0) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "F.Cu") (net 2))
"#;
// A through via named only (layers "F.Cu" "B.Cu") passes through the
// inner layers too; bucketing it onto the two named ends missed this.
const THROUGH_VIA_INNER_TRACK: &str = r#"
  (via (at 5 0) (size 1.0) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "In1.Cu") (net 2))
"#;
// A blind via F.Cu->In2.Cu passes through In1.Cu.
const BLIND_VIA_INNER_TRACK: &str = r#"
  (via blind (at 5 0) (size 1.0) (drill 0.4) (layers "F.Cu" "In2.Cu") (net 1))
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "In1.Cu") (net 2))
"#;
// A trapezoid pad (size 4 x 2, rect_delta (0 2)) has corners (-3, 1),
// (-1, -1), (1, -1), (3, 1): its wide edge extends 0.7 mm OUTSIDE the
// size box, where a bounding-rectangle model cleared the track by 0.6 mm.
const TRAPEZOID_WING_SEGMENT: &str = r#"
  (segment (start -2.7 -3) (end -2.7 3) (width 0.2) (layer "F.Cu") (net 1))
  (footprint "lib:trap" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1")
    (pad "1" smd trapezoid (at 0 0) (size 4 2) (rect_delta 0 2) (layers "F.Cu") (net 2))
  )
"#;
// A custom pad is its anchor plus EVERY primitive: a track through the
// second gr_poly lobe, which a first-poly-only model never stamped.
const CUSTOM_PAD_SECOND_POLYGON_SEGMENT: &str = r#"
  (segment (start -2.5 -2) (end -2.5 2) (width 0.2) (layer "F.Cu") (net 1))
  (footprint "lib:cust" (layer "F.Cu") (at 0 0)
    (property "Reference" "U2")
    (pad "1" smd custom (at 0 0) (size 1 1) (layers "F.Cu") (net 2)
      (options (clearance outline) (anchor circle))
      (primitives
        (gr_poly (pts (xy 2 -0.5) (xy 3 -0.5) (xy 3 0.5) (xy 2 0.5)) (width 0))
        (gr_poly (pts (xy -3 -0.5) (xy -2 -0.5) (xy -2 0.5) (xy -3 0.5)) (width 0))
      ))
  )
"#;

#[rustfmt::skip]
const OVERLAPS: &[(&str, bool, &str, (&str, &str), Option<&str>, Option<&str>)] = &[
    ("segment/segment", false, SEGMENT_SEGMENT, ("A", "B"), Some("F.Cu"), None),
    ("segment/pad", false, SEGMENT_PAD, ("A", "B"), None, Some("U1")),
    ("pad/pad", false, PAD_PAD, ("A", "B"), None, None),
    ("via/zone", false, VIA_ZONE, ("A", "GND"), Some("B.Cu"), None),
    ("via/segment", false, VIA_SEGMENT, ("A", "B"), None, None),
    ("through via/inner track", true, THROUGH_VIA_INNER_TRACK, ("A", "B"), Some("In1.Cu"), None),
    ("blind via/inner track", true, BLIND_VIA_INNER_TRACK, ("A", "B"), None, None),
    ("trapezoid wing/segment", false, TRAPEZOID_WING_SEGMENT, ("A", "B"), None, None),
    ("custom pad second polygon/segment", false, CUSTOM_PAD_SECOND_POLYGON_SEGMENT, ("A", "B"), None, None),
];

#[test]
fn each_copper_kind_overlapping_foreign_copper_is_one_short() {
    for &(name, four_layer, items, (a, b), layer, owner) in OVERLAPS {
        let report = if four_layer { drc4(items) } else { drc(items) };
        assert_eq!(report.short_count(), 1, "{name}: exactly one short");
        assert_short(&report, a, b);
        let f = report.shorts().next().unwrap();
        assert!(
            f.gap_mm <= 0.0,
            "{name}: overlap gap {} is non-positive",
            f.gap_mm
        );
        if let Some(layer) = layer {
            assert_eq!(f.layer, layer, "{name}: layer");
        }
        if let Some(owner) = owner {
            let owners = [f.item_a.owner.as_str(), f.item_b.owner.as_str()];
            assert!(
                owners.contains(&owner),
                "{name}: owner recorded: {owners:?}"
            );
        }
    }
}

/// Copper arrangements that must report nothing at all.
const SILENT: &[(&str, &str)] = &[
    (
        "tracks 5 mm apart",
        r#"
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 5) (end 10 5) (width 0.25) (layer "F.Cu") (net 2))
"#,
    ),
    (
        // Copper edges exactly the 0.2 mm rule apart (centres 0.25 + 0.2 mm):
        // routing-to-rule, which used to produce a carpet of boundary notes.
        "gap at the rule",
        r#"
  (segment (start 0 0) (end 10 0) (width 0.25) (layer "F.Cu") (net 1))
  (segment (start 0 0.45) (end 10 0.45) (width 0.25) (layer "F.Cu") (net 2))
"#,
    ),
    (
        "crossing tracks on opposite layers",
        r#"
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1))
  (segment (start 5 -5) (end 5 5) (width 0.5) (layer "B.Cu") (net 2))
"#,
    ),
    (
        // A pad on non-copper layers only (a fiducial's mask window) has no copper
        // and must not be stamped onto every copper layer.
        "mask-only pad over a track",
        r#"
  (footprint "Fiducial" (at 5 0)
    (property "Reference" "FID1")
    (pad "" smd rect (at 0 0) (size 2 2) (layers "F.Mask") (net 1 "A"))
  )
  (segment (start 0 0) (end 10 0) (width 0.4) (layer "F.Cu") (net 2))
"#,
    ),
];

#[test]
fn separated_or_copperless_arrangements_report_nothing() {
    for (name, items) in SILENT {
        let report = drc(items);
        assert!(
            report.findings.is_empty(),
            "{name}: nothing reported, got {:?}",
            report
                .findings
                .iter()
                .map(|f| (f.kind, f.gap_mm))
                .collect::<Vec<_>>()
        );
    }
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
  (net 3 "GND")
{items}
)
"#
    )
}

fn drc4(items: &str) -> hauksbee_extract::DrcReport {
    ExtractedBoard::drc(&board4(items)).expect("drc runs")
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

// ---------------------------------------------------------------------------
// Custom-pad primitive kinds beyond gr_poly: stroked lines, arcs, unfilled
// rings and rectangles are copper only along their strokes; filled rects are
// solid.
// ---------------------------------------------------------------------------

// ── remove_unused_layers via semantics ──────────────────────────────────────
// KiCad 7+ can strip a via's annular ring on layers it does not connect on
// (`remove_unused_layers yes`), leaving only the drilled barrel there.
// Modeling the removed ring at full pad size fabricated phantom clearance
// findings, and phantom SHORTS, against inner-layer copper the real board
// never touches: a keyboard with ~400 stitching vias produced 7 false
// SERIOUS shorts and a carpet of identical 0.150 mm warnings. On a
// ring-removed layer only the barrel (drill radius) owns spacing.

/// GND fill on `layer` whose left edge sits `edge_x` mm from the origin; the
/// via under test sits at (5,5), so edge_x = 5.45 puts the fill 0.45 mm from
/// the via center: drill radius (0.15) + a zone clearance of 0.30, exactly
/// the geometry KiCad writes when the ring is absent.
fn gnd_fill(layer: &str, edge_x: f64) -> String {
    format!(
        r#"
  (zone (net 3) (net_name "GND") (layer "{layer}")
    (polygon (pts (xy {edge_x} 0) (xy 10 0) (xy 10 10) (xy {edge_x} 10)))
    (filled_polygon (layer "{layer}")
      (pts (xy {edge_x} 0) (xy 10 0) (xy 10 10) (xy {edge_x} 10))
    )
  )
"#
    )
}

const UNUSED_RING_VIA: &str = r#"
  (via (at 5 5) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu")
       (remove_unused_layers yes) (keep_end_layers yes) (zone_layer_connections) (net 1))
"#;

#[test]
fn ring_removed_inner_layer_via_is_silent_at_real_kicad_spacing() {
    let items = format!("{UNUSED_RING_VIA}{}", gnd_fill("In1.Cu", 5.45));
    let report = drc4(&items);
    assert_eq!(report.shorts().count(), 0, "no phantom short");
    assert!(
        !report.clearance_violations().any(|v| v.layer == "In1.Cu"),
        "the bare barrel (0.15 mm radius) clears the fill by 0.30 mm; a finding here \
         would be the phantom-ring bug"
    );
}
