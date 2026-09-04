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

// ---------------------------------------------------------------------------
// Trapezoid pads. `(rect_delta dx dy)` makes one parallel edge size + delta
// long and the other size - delta: the true outline both extends BEYOND the
// size box (the wide edge) and recedes inside it (the narrow edge), so neither
// direction survives a bounding-rectangle approximation.
// ---------------------------------------------------------------------------

/// A trapezoid pad at the origin: size 4 x 2, rect_delta (0 2). True corners
/// (pad-local, y-down): (-3, 1), (-1, -1), (1, -1), (3, 1): the y = +1 edge
/// is 6 mm wide, the y = -1 edge 2 mm.
const TRAPEZOID_PAD: &str = r#"
  (footprint "lib:trap" (layer "F.Cu") (at 0 0)
    (property "Reference" "U1")
    (pad "1" smd trapezoid (at 0 0) (size 4 2) (rect_delta 0 2) (layers "F.Cu") (net 2))
  )
"#;

#[test]
fn trapezoid_wing_beyond_the_size_box_is_a_short() {
    // A vertical track at x = -2.7 crosses the trapezoid's wide-edge wing,
    // which extends to x = -3, i.e. 0.7 mm OUTSIDE the (size 4 2) box. The old
    // bounding-rectangle model cleared this by 0.6 mm and stayed silent.
    let track = r#"
  (segment (start -2.7 -3) (end -2.7 3) (width 0.2) (layer "F.Cu") (net 1))
"#;
    let items = format!("{track}{TRAPEZOID_PAD}");
    assert_short(&drc(&items), "A", "B");
}

// ---------------------------------------------------------------------------
// Custom pads: the copper is the anchor shape plus EVERY primitive. The old
// code kept only the first gr_poly, silently un-checking the anchor disc and
// all further primitives.
// ---------------------------------------------------------------------------

/// A custom pad: 1 mm circle anchor at the origin plus two 1 x 1 polygon
/// lobes at x in [2, 3] and x in [-3, -2] (y in [-0.5, 0.5]).
const CUSTOM_TWO_LOBE_PAD: &str = r#"
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

#[test]
fn custom_pad_second_polygon_is_copper() {
    // A track through the SECOND gr_poly lobe: the old first-poly-only model
    // never stamped it.
    let track = r#"
  (segment (start -2.5 -2) (end -2.5 2) (width 0.2) (layer "F.Cu") (net 1))
"#;
    let items = format!("{track}{CUSTOM_TWO_LOBE_PAD}");
    assert_short(&drc(&items), "A", "B");
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

/// A 4-layer wrapper: F.Cu / In1.Cu / In2.Cu / B.Cu.
fn board4_ring(items: &str) -> String {
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
    let report = hauksbee_extract::ExtractedBoard::drc(&board4_ring(&items)).expect("drc runs");
    assert_eq!(report.shorts().count(), 0, "no phantom short");
    assert!(
        !report.clearance_violations().any(|v| v.layer == "In1.Cu"),
        "the bare barrel (0.15 mm radius) clears the fill by 0.30 mm; a finding here \
         would be the phantom-ring bug"
    );
}
