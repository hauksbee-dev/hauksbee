//! Geometric DRC tests: hand-authored `.kicad_pcb` fixtures with one
//! deliberate violation of each geometry kind, asserting exact detection, plus
//! a corpus sweep asserting the known-good boards report zero true shorts.

use hauksbee_extract::gerber::geo::{Capsule, Shape};
use hauksbee_extract::{ClearanceRules, ExtractedBoard, NetClassRule, ViolationKind};

use crate::support::assert_short;

const KICAD_10_KEYHOLE_ANTIPAD_BOARD: &str =
    include_str!("fixtures/kicad_10_keyhole_antipad.kicad_pcb");

/// Wrap copper items in a minimal KiCad 7+ board carrying `layers`, two
/// declared signal nets (`A`, `B`) plus GND, and a default-clearance setup.
fn board_on(layers: &str, items: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers
{layers}  )
  (net 0 "")
  (net 1 "A")
  (net 2 "B")
  (net 3 "GND")
{items}
)
"#
    )
}

/// The two-layer stack most cases here only need.
fn board(items: &str) -> String {
    board_on("    (0 \"F.Cu\" signal)\n    (31 \"B.Cu\" signal)\n", items)
}

fn drc(items: &str) -> hauksbee_extract::DrcReport {
    ExtractedBoard::drc(&board(items)).expect("drc runs")
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
// A via dropped inside a stored GND pour: the fill is stale, not a short (see
// `a_via_inside_a_stored_fill_is_a_stale_fill_not_a_short`).
const VIA_ZONE: &str = r#"
  (via (at 5 5) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (zone (net 3) (net_name "GND") (layer "B.Cu")
    (polygon (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10)))
    (filled_polygon (layer "B.Cu")
      (pts (xy 0 0) (xy 10 0) (xy 10 10) (xy 0 10))
    )
  )
"#;
// A via sitting on a pad of another net, with no zone anywhere.
const VIA_PAD: &str = r#"
  (via (at 5 0) (size 1.0) (drill 0.4) (layers "F.Cu" "B.Cu") (net 1))
  (footprint "lib:fp" (layer "F.Cu") (at 5 0)
    (property "Reference" "U1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 1.5 1.5) (layers "F.Cu") (net 2))
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

/// (name, four-layer board, items, shorted nets, layer, owner)
type Overlap = (
    &'static str,
    bool,
    &'static str,
    (&'static str, &'static str),
    Option<&'static str>,
    Option<&'static str>,
);

#[rustfmt::skip]
const OVERLAPS: &[Overlap] = &[
    ("segment/segment", false, SEGMENT_SEGMENT, ("A", "B"), Some("F.Cu"), None),
    ("segment/pad", false, SEGMENT_PAD, ("A", "B"), None, Some("U1")),
    ("pad/pad", false, PAD_PAD, ("A", "B"), None, None),
    ("via/pad, no zone", false, VIA_PAD, ("A", "B"), None, Some("U1")),
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
        crate::support::assert_overlap_short(&report, name, layer, owner);
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
    // to be filed as a clearance note. The report rounds every length it prints
    // to a nanometre (`hauksbee_ir::order::quantize_mm`), which is a thousand
    // times coarser than the band, so the printed gap of such a finding is 0 and
    // cannot show the raw measurement. The raw gap is therefore measured here
    // with the same function the detector uses. The verdict is decided on the
    // raw gap before anything is rounded, so rounding cannot change it.
    let raw = hauksbee_extract::gerber::geo::shape_gap(
        &Shape::Capsule(Capsule::new(0.0, 0.0, 1.0, 0.0, 1.5)),
        &Shape::Capsule(Capsule::new(
            2.0,
            3.0,
            5.0,
            7.0,
            3.324_555_320_336_741_3 / 2.0,
        )),
    );
    assert!(
        raw > 0.0 && raw < hauksbee_extract::SHORT_TOUCH_EPS_MM,
        "the fixture must land strictly inside the touching band on the positive \
         side, measured {raw:e}"
    );
    assert!(hauksbee_extract::is_touching(raw));
    assert!(
        f.gap_mm >= 0.0 && f.gap_mm < hauksbee_extract::SHORT_TOUCH_EPS_MM,
        "the printed gap is the raw gap rounded to a nanometre, so it is 0 here; \
         measured {:e}",
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

/// An older-library solder jumper with no net-tie metadata: pads on A and GND
/// and, when `closed`, a small bridging pad of A drawn into the GND pad. A GND
/// track runs onto the GND pad and overlaps the bridge as well.
fn solder_jumper(footprint: &str, closed: bool) -> String {
    let bridge = if closed {
        r#"(pad "1" connect rect (at 0 0) (size 0.6 0.4) (layers "B.Cu") (net 1))"#
    } else {
        ""
    };
    format!(
        r#"
  (footprint "{footprint}" (layer "B.Cu") (at 20 20)
    (property "Reference" "JP1" (at 0 0))
    (property "Value" "Closed" (at 0 1))
    (attr smd)
    (pad "1" smd rect (at -0.762 0) (size 1.0 1.6) (layers "B.Cu") (net 1))
    (pad "2" smd rect (at 0.762 0) (size 1.0 1.6) (layers "B.Cu") (net 3))
    {bridge}
  )
  (segment (start 20.762 20) (end 20.1 20) (width 0.25) (layer "B.Cu") (net 3))
"#
    )
}

#[test]
fn a_closed_solder_jumper_drawn_in_copper_is_a_tie_not_a_short() {
    // The bridge is the jumper's designed state: A and GND are one node by
    // construction, so neither the bridge meeting the GND pad nor the GND
    // track reaching it is a short.
    let report = drc(&solder_jumper("Jumpers:SJ_Closed", true));
    assert_eq!(report.short_count(), 0, "{:?}", report.findings);
}

#[test]
fn foreign_copper_on_an_open_jumper_or_an_ordinary_footprint_is_still_a_short() {
    // Open jumper: no bridge, so the GND track reaching across onto pad 1 is
    // a real A/GND short.
    let open = drc(&solder_jumper("Jumpers:SJ", false).replace("(end 20.1 20)", "(end 19.2 20)"));
    assert_eq!(open.short_count(), 1, "{:?}", open.findings);
    assert_short(&open, "A", "GND");
    // The same touching copper in an ordinary footprint is a short.
    let ordinary = drc(&solder_jumper("Resistor_SMD:R_0603", true));
    assert!(ordinary.short_count() >= 1, "{:?}", ordinary.findings);
    assert_short(&ordinary, "A", "GND");
}

/// A three-pad solder jumper (centre pad 2 on GND) whose pads 1 and 2 are
/// bridged: pad 1 (A) and pad 2 (GND) share copper, pad 3 (B) sits a clearance
/// away. `footprint` names the part; `track_b_onto_pad1` adds a B track that
/// ends on pad 1, a foreign contact the bridge does not excuse.
fn bridged_12_jumper(footprint: &str, track_b_onto_pad1: bool) -> String {
    let track = if track_b_onto_pad1 {
        r#"(segment (start 19.5 17.0) (end 19.5 19.9) (width 0.25) (layer "F.Cu") (net 2))"#
    } else {
        ""
    };
    format!(
        r#"
  (footprint "{footprint}" (layer "F.Cu") (at 20 20)
    (property "Reference" "JP1" (at 0 0))
    (property "Value" "Bridged12" (at 0 1))
    (attr smd)
    (pad "1" smd rect (at -0.5 0) (size 1.0 1.6) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.5 0) (size 1.0 1.6) (layers "F.Cu") (net 3))
    (pad "3" smd rect (at 1.8 0) (size 1.0 1.6) (layers "F.Cu") (net 2))
  )
  {track}
"#
    )
}

#[test]
fn a_bridged_pair_on_a_three_pad_solder_jumper_is_a_tie_not_a_short() {
    // Pads 1 (A) and 2 (GND) share copper by design; pad 3 (B) is clear of
    // both. Only the bridged pair is excused.
    let report = drc(&bridged_12_jumper(
        "Jumper:SolderJumper-3_P1.3mm_Bridged12_Pad1.0x1.5mm",
        false,
    ));
    assert_eq!(report.short_count(), 0, "{:?}", report.findings);
}

#[test]
fn foreign_copper_on_the_unbridged_pad_of_a_three_pad_jumper_is_still_a_short() {
    // The bridge excuses A/GND only. A B track ending on pad 1 is a B/A short,
    // and the same overlapping pads in an ordinary footprint are a short too.
    let foreign = drc(&bridged_12_jumper(
        "Jumper:SolderJumper-3_P1.3mm_Bridged12_Pad1.0x1.5mm",
        true,
    ));
    assert!(foreign.short_count() >= 1, "{:?}", foreign.findings);
    assert_short(&foreign, "A", "B");
    let ordinary = drc(&bridged_12_jumper(
        "Connector_PinHeader:PinHeader_1x03",
        false,
    ));
    assert!(ordinary.short_count() >= 1, "{:?}", ordinary.findings);
    assert_short(&ordinary, "A", "GND");
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
fn a_diff_pair_gap_wider_than_clearance_never_tightens_it() {
    // A coupled pair routed at 0.130 mm meets its 0.127 mm class clearance; the
    // class's 0.145 mm diff-pair gap is a routing target, not a DRC minimum.
    // At 0.100 mm the same pair is below the clearance and is still flagged.
    for (edge_gap, flagged) in [(0.130, false), (0.100, true)] {
        let items = format!(
            r#"(segment (start 0 0) (end 10 0) (width 0.2) (layer "F.Cu") (net 1))
  (segment (start 0 {y}) (end 10 {y}) (width 0.2) (layer "F.Cu") (net 2))"#,
            y = 0.2 + edge_gap
        );
        let mut rules = ClearanceRules::new(0.127);
        rules.add_class(NetClassRule {
            name: "sense".to_string(),
            clearance_mm: 0.127,
            diff_pair_gap_mm: Some(0.145),
        });
        rules.assign_net("/USB/USB_D+", "sense");
        rules.assign_net("/USB/USB_D-", "sense");
        let doc = forge_sexpr::parse(&named_board(&items)).unwrap();
        let report = hauksbee_extract::run_drc_with_clearance_rules(&doc, Some(rules));
        assert_eq!(
            !report.findings.is_empty(),
            flagged,
            "{edge_gap} mm: {:?}",
            report.findings
        );
    }
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
    board_on(
        "    (0 \"F.Cu\" signal)\n    (1 \"In1.Cu\" signal)\n    (2 \"In2.Cu\" signal)\n    (31 \"B.Cu\" signal)\n",
        items,
    )
}

fn drc4(items: &str) -> hauksbee_extract::DrcReport {
    ExtractedBoard::drc(&board4(items)).expect("drc runs")
}

/// The PolyKybd Kailh-socket proof geometry: pad `2` of `SW_K_2` (2.55 x 1.54,
/// `chamfer_ratio` 0.5 on `bottom_left` + `bottom_right`: 0.77 mm sliced off both
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
/// via under test sits at (5,5), so `edge_x` = 5.45 puts the fill 0.45 mm from
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

/// A GND pour outline over a net-A track: `fill` is the zone's fill clause
/// and `filled` its fill polygons. KiCad writes `(fill yes ...)` with no
/// `filled_polygon` when the fill came out empty (a lower-priority pour wholly
/// covered by a higher one), so the drawn outline must not stand in for it.
#[test]
fn zone_with_an_empty_fill_has_no_copper_but_an_unfilled_zone_falls_back_to_its_outline() {
    let outline = "(polygon (pts (xy 0 -5) (xy 10 -5) (xy 10 5) (xy 0 5)))";
    let track = r#"(segment (start 8 0) (end 14 0) (width 0.3) (layer "F.Cu") (net 1))"#;
    let cases = [
        ("(fill yes)", "", false, "filled, empty result: no copper"),
        (
            "(fill yes)",
            "(filled_polygon (layer \"F.Cu\") (pts (xy 0 2) (xy 10 2) (xy 10 5) (xy 0 5)))",
            false,
            "fill clear of the track",
        ),
        (
            "",
            "",
            true,
            "never filled: outline is the best known copper",
        ),
    ];
    for (fill, filled, reported, why) in cases {
        let items = format!(
            r#"(zone (net 3) (net_name "GND") (layer "F.Cu") {fill} {outline} {filled}) {track}"#
        );
        assert_eq!(!drc(&items).findings.is_empty(), reported, "{why}");
    }
}

/// KiCad's net-tie rule: a foreign item may overlap the tie footprint only
/// where the item's own net has a tie-group pad and the contact lies inside
/// that pad. The pads here are 1 mm squares at x 0 (net A) and 0.9 (net B).
#[test]
fn net_tie_exempts_an_item_only_inside_its_own_groups_pad() {
    let tie = r#"
  (footprint "Acme:Tie" (layer "F.Cu") (at 20 20)
    (property "Reference" "NT1" (at 0 0))
    (attr net_tie)
    (net_tie_pad_groups "1, 2")
    (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0.9 0) (size 1 1) (layers "F.Cu") (net 2)))"#;
    // A vertical track at x=20.45, clear of both ends of the tie: it grazes pad 1
    // (net A, x 19.5..20.5) inside pad 2's span (net B, x 20.4..21.4). No end of
    // it touches the tie, so only the own-pad rule can waive the contact.
    let cases = [
        (2, false, "B grazes pad 1 where it overlaps B's own pad"),
        (3, true, "GND has no pad in the group to stand on"),
    ];
    for (net, shorts, why) in cases {
        let items = format!(
            r#"{tie}(segment (start 20.45 17) (end 20.45 23) (width 0.1) (layer "F.Cu") (net {net}))"#
        );
        assert_eq!(drc(&items).short_count() > 0, shorts, "{why}");
    }
}

/// A Kelvin tie seated inside a shunt's pad: the shunt pad (B) overlaps the
/// tie's B pad, and the A sense track leaving the tie's A pad runs across the
/// shunt pad. That contact is the tie's own junction, not a short. The shunt
/// pad is a 2 mm square at (20, 20); the tie pads sit at y 19.85 (A) and
/// 20.15 (B), and the A trace runs from the tie down to y 18.
#[test]
fn net_tie_inside_a_foreign_pad_exempts_only_that_pads_own_net() {
    let shunt = |y: f64, net: u32| {
        format!(
            r#"(footprint "Acme:Shunt" (layer "F.Cu") (at 20 {y})
    (property "Reference" "R1" (at 0 0))
    (pad "2" smd rect (at 0 0) (size 2 2) (layers "F.Cu" "F.Paste" "F.Mask") (net {net})))"#
        )
    };
    let tie = r#"(footprint "Acme:Tie" (layer "F.Cu") (at 20 20)
    (property "Reference" "NT1" (at 0 0))
    (attr net_tie)
    (net_tie_pad_groups "1, 2")
    (pad "1" smd rect (at 0 -0.15) (size 0.25 0.25) (layers "F.Cu") (net 1))
    (pad "2" smd rect (at 0 0.15) (size 0.25 0.25) (layers "F.Cu") (net 2)))
  (segment (start 20 19.85) (end 20 18) (width 0.2) (layer "F.Cu") (net 1))"#;
    let cases = [
        (
            20.0,
            2,
            0,
            "B shunt pad holds the tie: the A trace meets it at the tie",
        ),
        (
            18.5,
            2,
            1,
            "B pad clear of the tie: the A trace crossing it shorts",
        ),
        (
            20.0,
            3,
            1,
            "GND is outside the tie group, so GND copper there shorts",
        ),
    ];
    for (y, net, shorts, why) in cases {
        let report = drc(&format!("{}\n  {tie}", shunt(y, net)));
        assert_eq!(
            usize::from(report.short_count() > 0),
            shorts,
            "{why}: {:?}",
            report.findings
        );
    }
}

const UUID_A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const UUID_B: &str = "bbbbbbbb-0000-4000-8000-000000000002";

fn crossing_tracks() -> hauksbee_extract::DrcReport {
    drc(&format!(
        r#"
  (segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net 1) (uuid "{UUID_A}"))
  (segment (start 5 -5) (end 5 5) (width 0.5) (layer "F.Cu") (net 2) (uuid "{UUID_B}"))
"#
    ))
}

#[test]
fn project_exclusion_waives_a_matching_short_but_keeps_it_visible() {
    let mut report = crossing_tracks();
    assert_eq!(report.short_count(), 1, "the fixture has one short");
    // KiCad writes `type|x_nm|y_nm|main_uuid|aux_uuid`; the order of the two
    // items is not significant to the native finding.
    let entry = format!("shorting_items|5000000|0|{UUID_B}|{UUID_A}");
    report.apply_kicad_exclusions([entry.as_str()]);
    assert!(report.is_clean(), "an excluded short must not count");
    assert_eq!(report.excluded.len(), 1, "an excluded short stays visible");
    assert_eq!(report.excluded[0].kind, ViolationKind::Short);
}

#[test]
fn project_exclusion_needs_the_same_type_items_and_position() {
    let other = "cccccccc-0000-4000-8000-000000000003";
    for entry in [
        format!("clearance|5000000|0|{UUID_A}|{UUID_B}"),
        format!("shorting_items|5000000|0|{UUID_A}|{other}"),
        format!("shorting_items|9000000|3000000|{UUID_A}|{UUID_B}"),
        "not an exclusion".to_string(),
    ] {
        let mut report = crossing_tracks();
        report.apply_kicad_exclusions([entry.as_str()]);
        assert_eq!(report.short_count(), 1, "{entry} must not waive the short");
        assert!(report.excluded.is_empty(), "{entry}");
    }
}

// ---------------------------------------------------------------------------
// Fine-pitch pads on a rotated footprint (the USB-C receptacle shape). A pad's
// `(at x y ANGLE)` is its absolute board orientation, so a connector placed at
// 270 degrees with 0.3 x 1.45 mm pads on a 0.5 mm pitch is clear only when each
// pad carries that 270. The same pads written with angle 0 stand 1.45 mm long
// along the row and overlap their neighbours: genuine copper contact, which is
// what a converter that dropped the footprint rotation from its pads used to
// draw for a converted board.
// ---------------------------------------------------------------------------

/// A three-pad row (nets A, B, GND) on a connector placed at `footprint_rot`,
/// every pad written with `pad_angle`.
fn fine_pitch_row(footprint_rot: u32, pad_angle: u32) -> String {
    let pad = |name: &str, x: f64, net: u32| {
        format!(
            "    (pad \"{name}\" smd rect (at {x} -4 {pad_angle}) (size 0.3 1.45) \
             (layers \"F.Cu\") (net {net}))\n"
        )
    };
    format!(
        "  (footprint \"lib:fine\" (layer \"F.Cu\") (at 100 50 {footprint_rot})\n    \
         (property \"Reference\" \"J1\" (at 0 0))\n{}{}{}  )\n",
        pad("B8", -0.5, 1),
        pad("A5", 0.0, 2),
        pad("A7", 0.5, 3),
    )
}

#[test]
fn fine_pitch_pads_on_a_rotated_footprint_with_their_absolute_angle_are_clear() {
    let report = drc(&fine_pitch_row(270, 270));
    assert_eq!(report.short_count(), 0, "{:?}", report.findings);
    assert_eq!(
        report.clearance_violations().count(),
        0,
        "{:?}",
        report.findings
    );
}

#[test]
fn the_same_pads_left_unrotated_under_a_rotated_footprint_do_overlap() {
    // The faulty control: that geometry really does touch, so it stays a short.
    let report = drc(&fine_pitch_row(270, 0));
    assert!(report.short_count() >= 1, "{:?}", report.findings);
}

#[test]
fn stacked_same_net_pads_of_one_footprint_are_not_a_short() {
    // A1 and B12 of a USB-C receptacle sit at one position on one net.
    let items = r#"
  (footprint "lib:usbc" (layer "F.Cu") (at 100 50 270)
    (property "Reference" "J2" (at 0 0))
    (pad "A1" smd rect (at -3.25 -4.045 270) (size 0.6 1.45) (layers "F.Cu") (net 1))
    (pad "B12" smd rect (at -3.25 -4.045 270) (size 0.6 1.45) (layers "F.Cu") (net 1))
  )
"#;
    assert_eq!(drc(items).short_count(), 0);
}

#[test]
fn a_foreign_footprint_pad_on_a_fine_pitch_pad_is_still_a_short() {
    // J1 at (100, 50) turned 270: its pad at local (0, -4) lands on (104, 50).
    // A test point of another footprint and net sits right on it.
    let items = r#"
  (footprint "lib:fine" (layer "F.Cu") (at 100 50 270)
    (property "Reference" "J1" (at 0 0))
    (pad "A5" smd rect (at 0 -4 270) (size 0.3 1.45) (layers "F.Cu") (net 2))
  )
  (footprint "lib:test_point" (layer "F.Cu") (at 104 50)
    (property "Reference" "TP1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 0.6 0.6) (layers "F.Cu") (net 1))
  )
"#;
    assert_short(&drc(items), "A", "B");
}

// ── remove_unused_layers pad semantics ──────────────────────────────────────
// KiCad (PAD::FlashLayer) leaves a plated through-hole pad flashed on a copper
// layer only if something connects there; `keep_end_layers` keeps the outer
// pair and `zone_layer_connections` forces a layer. Elsewhere only the plated
// barrel remains, and a zone keeps its clearance from the barrel. Recorded
// against KiCad 10.0.6 `kicad-cli pcb drc` on fixtures of this shape: a fill
// edge 0.4 mm off the barrel (0.8 mm off the pad's centre) is silent when the
// pad is unflashed and a clearance error when the 1.6 mm pad copper is there.
// (KiCad also flags a fill drawn over the bare barrel; the sweep suppresses
// every Zone-Pad overlap as an antipad carve, so only the clearance side is
// asserted here.)

/// A 1.6 mm through-hole pad with a 0.8 mm drill at (5, 5), net A.
fn th_pad(flags: &str) -> String {
    format!(
        r#"
  (footprint "lib:TH" (layer "F.Cu") (at 5 5)
    (property "Reference" "J1" (at 0 0))
    (pad "1" thru_hole circle (at 0 0) (size 1.6 1.6) (drill 0.8) (layers "*.Cu") {flags} (net 1)))
"#
    )
}

const UNUSED: &str = "(remove_unused_layers yes) (keep_end_layers yes)";

fn findings_on(items: &str, layer: &str) -> Vec<(ViolationKind, f64)> {
    drc4(items)
        .findings
        .iter()
        .filter(|f| {
            f.layer == layer
                && (f.item_a.kind == hauksbee_extract::ItemKind::Zone
                    || f.item_b.kind == hauksbee_extract::ItemKind::Zone)
        })
        .map(|f| (f.kind, f.gap_mm))
        .collect()
}

#[test]
fn an_unflashed_inner_pad_layer_keeps_only_the_barrel_for_the_fill_to_clear() {
    // Fill edge 0.9 mm from the pad centre: 0.1 mm off the 1.6 mm copper (under
    // the 0.2 mm rule), 0.5 mm clear of the 0.8 mm barrel.
    let fill = gnd_fill("In1.Cu", 5.9);
    let on = |flags: &str| findings_on(&format!("{}{fill}", th_pad(flags)), "In1.Cu");
    assert!(on(UNUSED).is_empty(), "{:?}", on(UNUSED));
    // Controls: copper kept on every layer, so the fill touches it.
    assert!(!on("").is_empty(), "no removal flag");
    assert!(!on("(remove_unused_layers no)").is_empty());
    // A zone forced onto the layer flashes the pad there.
    assert!(
        !on(&format!("{UNUSED} (zone_layer_connections \"In1.Cu\")")).is_empty(),
        "zone_layer_connections flashes the layer"
    );
}

#[test]
fn a_fill_too_close_to_the_barrel_is_still_reported() {
    // 0.15 mm off the 0.8 mm barrel (0.55 from the centre): under the 0.2 mm
    // rule even with the pad copper gone. A fill that overlaps the barrel is
    // the antipad carve the sweep suppresses for every Zone-Pad pair.
    let items = format!("{}{}", th_pad(UNUSED), gnd_fill("In1.Cu", 5.55));
    let found = findings_on(&items, "In1.Cu");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, ViolationKind::Clearance);
    assert!((found[0].1 - 0.15).abs() < 1e-6, "{found:?}");
}

#[test]
fn a_track_that_reaches_the_pad_flashes_that_layer() {
    let track = r#"(segment (start 5 5) (end 5 8) (width 0.2) (layer "In1.Cu") (net 1))"#;
    let items = format!("{}{track}{}", th_pad(UNUSED), gnd_fill("In1.Cu", 5.9));
    assert!(!findings_on(&items, "In1.Cu").is_empty());
    // A track of another net on the layer connects nothing.
    let other = r#"(segment (start 5 5) (end 5 8) (width 0.2) (layer "In1.Cu") (net 2))"#;
    let items = format!("{}{other}{}", th_pad(UNUSED), gnd_fill("In1.Cu", 5.9));
    assert!(
        findings_on(&items, "In1.Cu").is_empty(),
        "another net's track connects nothing, so the fill still clears the barrel"
    );
}

#[test]
fn end_layers_keep_their_copper_with_keep_end_layers_only() {
    let top = |flags: &str| {
        findings_on(
            &format!("{}{}", th_pad(flags), gnd_fill("F.Cu", 5.9)),
            "F.Cu",
        )
    };
    assert!(!top(UNUSED).is_empty(), "keep_end_layers keeps F.Cu");
    assert!(
        top("(remove_unused_layers yes) (keep_end_layers no)").is_empty(),
        "without it F.Cu is only the barrel too"
    );
}

#[test]
fn a_kicad_10_padstack_sizes_the_inner_layers_on_their_own() {
    // Inner copper is 1.0 mm (0.5 mm radius): the 0.9 mm-from-centre fill clears
    // it by 0.4 mm. The outer pad is 1.6 mm, 0.1 mm from that fill.
    let pad = |inner: &str| {
        format!(
            r#"
  (footprint "lib:TH" (layer "F.Cu") (at 5 5)
    (property "Reference" "J1" (at 0 0))
    (pad "1" thru_hole circle (at 0 0) (size 1.6 1.6) (drill 0.8) (layers "*.Cu")
      (padstack (mode front_inner_back)
        (layer "Inner" (shape circle) (size {inner} {inner}))
        (layer "B.Cu" (shape circle) (size 1.6 1.6)))
      (net 1)))
"#
        )
    };
    let on = |inner: &str, layer: &str| {
        findings_on(&format!("{}{}", pad(inner), gnd_fill(layer, 5.9)), layer)
    };
    assert!(on("1.0", "In1.Cu").is_empty(), "{:?}", on("1.0", "In1.Cu"));
    assert!(!on("1.6", "In1.Cu").is_empty());
    assert!(!on("1.0", "F.Cu").is_empty(), "the front is the pad itself");
    assert!(!on("1.0", "B.Cu").is_empty(), "the back has its own entry");
}

// A track routed after the last refill: the zone's stored fill still covers it.
// KiCad cuts the clearance around it on the next refill, so this is an
// out-of-date fill, not copper that is shorted.

/// A GND pour over x 0..20, y 0..20 on F.Cu; `connect` is the zone's
/// `connect_pads` clause (empty leaves the clearance to the board rule).
fn stored_pour(connect: &str) -> String {
    format!(
        r#"
  (zone (net 3) (net_name "GND") (layer "F.Cu")
    {connect}
    (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))
    (filled_polygon (layer "F.Cu")
      (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20))
    )
  )
"#
    )
}

const TRACK_IN_THE_POUR: &str =
    r#"  (segment (start 5 10) (end 15 10) (width 0.25) (layer "F.Cu") (net 1))"#;
const TRACK_ACROSS_THE_EDGE: &str =
    r#"  (segment (start 15 10) (end 25 10) (width 0.25) (layer "F.Cu") (net 1))"#;

#[test]
fn a_track_inside_a_stored_fill_with_a_clearance_is_a_stale_fill_not_a_short() {
    for track in [TRACK_IN_THE_POUR, TRACK_ACROSS_THE_EDGE] {
        let report = drc(&format!(
            "{}{track}",
            stored_pour("(connect_pads (clearance 0.3))")
        ));
        assert_eq!(report.shorts().count(), 0, "{track}: not a short");
        let stale: Vec<_> = report.stale_fills().collect();
        assert!(!stale.is_empty(), "{track}: the fill is out of date");
        assert!(
            stale.iter().all(|f| f.kind == ViolationKind::StaleFill
                && f.layer == "F.Cu"
                && (f.net_a_name == "GND" || f.net_b_name == "GND")
                && (f.net_a_name == "A" || f.net_b_name == "A")),
            "{stale:?}"
        );
    }
}

#[test]
fn a_zone_clearance_of_zero_or_none_still_reads_as_stale() {
    // KiCad falls back to the board rule (0.2 mm) for a zone that names none or
    // zero, and a refill carves by it: the oracle reports "clearance 0.2000 mm;
    // actual 0.0000 mm", not a short.
    for connect in ["", "(connect_pads (clearance 0))"] {
        let report = drc(&format!("{}{TRACK_IN_THE_POUR}", stored_pour(connect)));
        assert_eq!(report.shorts().count(), 0, "{connect:?}");
        assert!(report.stale_fills().next().is_some(), "{connect:?}");
    }
}

#[test]
fn an_outline_only_zone_with_no_stored_fill_is_not_a_stale_fill() {
    // No `filled_polygon`: nothing was ever filled, so there is no fill to be
    // out of date. A track inside the drawn outline is not reported as stale.
    let outline = r#"
  (zone (net 3) (net_name "GND") (layer "F.Cu")
    (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20))))
"#;
    let report = drc(&format!("{outline}{TRACK_IN_THE_POUR}"));
    assert_eq!(report.stale_fills().count(), 0);
}

#[test]
fn a_real_short_beside_a_stale_fill_stays_a_short() {
    let items = format!(
        "{}{TRACK_IN_THE_POUR}\n{}",
        stored_pour("(connect_pads (clearance 0.3))"),
        r#"  (segment (start 2 18) (end 6 18) (width 0.25) (layer "B.Cu") (net 1))
  (segment (start 4 16) (end 4 20) (width 0.25) (layer "B.Cu") (net 2))"#
    );
    let report = drc(&items);
    assert_eq!(report.shorts().count(), 1, "the crossing tracks");
    assert!(
        report
            .shorts()
            .all(|f| f.layer == "B.Cu" && f.net_a_name != "GND" && f.net_b_name != "GND"),
        "{:?}",
        report.shorts().collect::<Vec<_>>()
    );
    assert!(report.stale_fills().next().is_some());
}

#[test]
fn a_track_over_a_pad_of_another_net_with_no_zone_is_still_a_short() {
    let report = drc(SEGMENT_PAD);
    assert!(report.shorts().next().is_some());
    assert_eq!(report.stale_fills().count(), 0);
}

const VIA_IN_THE_POUR: &str =
    r#"  (via (at 10 10) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1))"#;
const PAD_IN_THE_POUR: &str = r#"
  (footprint "lib:fp" (layer "F.Cu") (at 10 10)
    (property "Reference" "U1" (at 0 0))
    (pad "1" smd rect (at 0 0) (size 1.5 1.5) (layers "F.Cu") (net 1))
  )
"#;

/// The kicad-cli 10.0.6 oracle on the same arrangements (`pcb drc --format json
/// --severity-all`, a 16 mm GND-equivalent pour with a stored fill and zone
/// clearance 0.3 mm, a net-A copper item inside it):
///   - a pad inside the stored fill: `clearance` error "zone clearance 0.3000
///     mm; actual 0.0000 mm" (Pad [A] vs Zone [B]); with `connect_pads
///     (clearance 0)` the same error at the 0.2 mm board rule; no shorting item.
///   - a via that is connected to its own net (here a net-A track ends on it):
///     the same `clearance` error "actual 0.0000 mm" (Via [A] vs Zone [B]),
///     plus a `hole_clearance`; no shorting item.
///   - a lone via with nothing of its own net on it is re-netted by KiCad to the
///     pour it sits in and reported only as `via_dangling`; hauksbee still names
///     the declared nets, so it is the same refill reminder.
#[test]
fn a_via_inside_a_stored_fill_is_a_stale_fill_not_a_short() {
    for (name, items, layer) in [
        (
            "via in the pour",
            format!(
                "{}{VIA_IN_THE_POUR}",
                stored_pour("(connect_pads (clearance 0.3))")
            ),
            "F.Cu",
        ),
        ("via in a B.Cu pour", VIA_ZONE.to_string(), "B.Cu"),
    ] {
        let report = drc(&items);
        assert_eq!(report.shorts().count(), 0, "{name}: not a short");
        let stale: Vec<_> = report.stale_fills().collect();
        assert!(!stale.is_empty(), "{name}: the fill is out of date");
        assert!(
            stale.iter().all(|f| f.kind == ViolationKind::StaleFill
                && f.layer == layer
                && [&f.item_a, &f.item_b]
                    .iter()
                    .any(|i| i.kind == hauksbee_extract::ItemKind::Via)
                && (f.net_a_name == "GND" || f.net_b_name == "GND")
                && (f.net_a_name == "A" || f.net_b_name == "A")),
            "{name}: {stale:?}"
        );
    }
}

#[test]
fn a_pad_inside_a_stored_fill_is_a_stale_fill_not_a_short() {
    for connect in ["(connect_pads (clearance 0.3))", ""] {
        let report = drc(&format!("{}{PAD_IN_THE_POUR}", stored_pour(connect)));
        assert_eq!(report.shorts().count(), 0, "{connect:?}: not a short");
        let stale: Vec<_> = report.stale_fills().collect();
        assert!(!stale.is_empty(), "{connect:?}: the fill is out of date");
        assert!(
            stale.iter().all(|f| [&f.item_a, &f.item_b]
                .iter()
                .any(|i| { i.kind == hauksbee_extract::ItemKind::Pad && i.owner == "U1" })
                && (f.net_a_name == "GND" || f.net_b_name == "GND")),
            "{connect:?}: {stale:?}"
        );
    }
}

#[test]
fn a_via_on_a_pad_or_track_of_another_net_with_no_zone_is_still_a_short() {
    for (name, items) in [("via on a pad", VIA_PAD), ("via on a track", VIA_SEGMENT)] {
        let report = drc(items);
        assert_eq!(report.short_count(), 1, "{name}");
        assert_short(&report, "A", "B");
        assert_eq!(report.stale_fills().count(), 0, "{name}");
    }
}

#[test]
fn an_outline_only_zone_leaves_a_via_or_pad_inside_it_unreported() {
    let outline = r#"
  (zone (net 3) (net_name "GND") (layer "F.Cu")
    (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20))))
"#;
    for copper in [VIA_IN_THE_POUR, PAD_IN_THE_POUR] {
        let report = drc(&format!("{outline}{copper}"));
        assert_eq!(report.stale_fills().count(), 0, "{copper}");
    }
}
