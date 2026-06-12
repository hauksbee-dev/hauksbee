//! Geometric DRC tests for Eagle `.brd` boards: hand-authored minimal XML
//! fixtures with one deliberate violation of each geometry kind, asserting
//! exact detection, plus the design-rule-clearance and mirrored-package cases.
//!
//! The corpus sweep over the eight famous Eagle boards lives in
//! `eagle_drc_corpus.rs`.

use galvani_extract::{ExtractedBoard, ViolationKind};

/// Wrap board geometry in a minimal Eagle 6 `.brd`. `designrules` lets a test
/// control the embedded clearance rule; `signals` carries the copper. Eagle is
/// y-up, millimetres. Two outer copper layers (1 = Top / F.Cu, 16 = Bottom /
/// B.Cu) are declared, matching the real corpus boards.
fn board(packages: &str, elements: &str, signals: &str, designrules: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE eagle SYSTEM "eagle.dtd">
<eagle version="6.6.0">
<drawing>
<layers>
<layer number="1" name="Top" color="4" fill="1" visible="yes" active="yes"/>
<layer number="16" name="Bottom" color="1" fill="1" visible="yes" active="yes"/>
</layers>
<board>
<plain>
</plain>
<libraries>
<library name="lib">
<packages>
{packages}
</packages>
</library>
</libraries>
{designrules}
<elements>
{elements}
</elements>
<signals>
{signals}
</signals>
</board>
</drawing>
</eagle>
"#
    )
}

/// The default Eagle design-rule block (6 mil = 0.1524 mm wire-wire clearance).
fn default_rules() -> &'static str {
    r#"<designrules name="default">
<param name="mdWireWire" value="6mil"/>
<param name="mdWirePad" value="6mil"/>
<param name="mdPadPad" value="6mil"/>
<param name="mdSmdSmd" value="6mil"/>
</designrules>"#
}

fn drc(packages: &str, elements: &str, signals: &str) -> galvani_extract::DrcReport {
    let text = board(packages, elements, signals, default_rules());
    ExtractedBoard::drc(&text).expect("eagle drc runs")
}

fn drc_rules(
    packages: &str,
    elements: &str,
    signals: &str,
    rules: &str,
) -> galvani_extract::DrcReport {
    let text = board(packages, elements, signals, rules);
    ExtractedBoard::drc(&text).expect("eagle drc runs")
}

/// True if a short between two named nets exists.
fn assert_short(report: &galvani_extract::DrcReport, a: &str, b: &str) {
    let found = report.shorts().any(|f| {
        let names = [f.net_a_name.as_str(), f.net_b_name.as_str()];
        names.contains(&a) && names.contains(&b)
    });
    assert!(
        found,
        "expected a SHORT between {a} and {b}; got {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.net_a_name.clone(), f.net_b_name.clone(), f.gap_mm))
            .collect::<Vec<_>>()
    );
}

#[test]
fn dispatch_recognises_eagle() {
    // A board with two crossing wires on different nets dispatches to the Eagle
    // engine (not the empty default).
    let signals = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.5" layer="1"/>
</signal>
<signal name="B">
  <wire x1="5" y1="-5" x2="5" y2="5" width="0.5" layer="1"/>
</signal>
"#;
    let report = drc("", "", signals);
    assert!(report.primitive_count > 0, "Eagle geometry was extracted");
}

#[test]
fn wire_wire_crossing_is_a_short() {
    // Two 0.5 mm wires crossing on the top copper, different nets.
    let signals = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.5" layer="1"/>
</signal>
<signal name="B">
  <wire x1="5" y1="-5" x2="5" y2="5" width="0.5" layer="1"/>
</signal>
"#;
    let report = drc("", "", signals);
    assert_eq!(report.short_count(), 1, "exactly one short");
    assert_short(&report, "A", "B");
    let f = report.shorts().next().unwrap();
    assert_eq!(f.layer, "F.Cu");
    assert!(f.gap_mm <= 0.0, "overlap gap is non-positive ({})", f.gap_mm);
}

#[test]
fn parallel_wires_within_clearance_are_a_clearance_violation() {
    // Two 0.25 mm wires whose copper edges are 0.1 mm apart (centres 0.35 mm,
    // both half-widths 0.125). Under the 6 mil (0.1524 mm) rule: a clearance
    // violation, not a short.
    let signals = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.25" layer="1"/>
</signal>
<signal name="B">
  <wire x1="0" y1="0.35" x2="10" y2="0.35" width="0.25" layer="1"/>
</signal>
"#;
    let report = drc("", "", signals);
    assert_eq!(report.short_count(), 0, "no true short");
    let cv: Vec<_> = report.clearance_violations().collect();
    assert!(!cv.is_empty(), "a clearance violation is reported");
    assert_eq!(cv[0].kind, ViolationKind::Clearance);
    assert!(
        cv[0].gap_mm > 0.0 && cv[0].gap_mm < report.clearance_mm,
        "gap {:.3} positive and under the {:.3} mm rule",
        cv[0].gap_mm,
        report.clearance_mm
    );
}

#[test]
fn well_separated_wires_report_nothing() {
    let signals = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.25" layer="1"/>
</signal>
<signal name="B">
  <wire x1="0" y1="5" x2="10" y2="5" width="0.25" layer="1"/>
</signal>
"#;
    let report = drc("", "", signals);
    assert!(report.findings.is_empty(), "nothing reported");
}

#[test]
fn wire_smd_overlap_is_a_short() {
    // A wire on net A driven straight through an SMD pad on net B. The package
    // defines pad "1"; element U1 places it; the contactref puts it on net B.
    let packages = r#"
<package name="PAD1">
  <smd name="1" x="0" y="0" dx="1.5" dy="1.5" layer="1"/>
</package>"#;
    let elements = r#"<element name="U1" library="lib" package="PAD1" x="5" y="0"/>"#;
    let signals = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="1"/>
</signal>
<signal name="B">
  <contactref element="U1" pad="1"/>
</signal>
"#;
    let report = drc(packages, elements, signals);
    assert_short(&report, "A", "B");
    let f = report.shorts().next().unwrap();
    let owners = [f.item_a.owner.as_str(), f.item_b.owner.as_str()];
    assert!(owners.contains(&"U1"), "pad owner U1 recorded: {owners:?}");
}

#[test]
fn smd_smd_overlap_in_different_packages_is_a_short() {
    // Two SMD pads on different nets, in different elements, overlapping. Pads
    // 2 mm wide, centres 1 mm apart → overlap by 1 mm.
    let packages = r#"
<package name="PAD2">
  <smd name="1" x="0" y="0" dx="2" dy="2" layer="1"/>
</package>"#;
    let elements = r#"
<element name="U1" library="lib" package="PAD2" x="5" y="5"/>
<element name="U2" library="lib" package="PAD2" x="6" y="5"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="U1" pad="1"/>
</signal>
<signal name="B">
  <contactref element="U2" pad="1"/>
</signal>
"#;
    let report = drc(packages, elements, signals);
    assert_eq!(report.short_count(), 1);
    assert_short(&report, "A", "B");
}

#[test]
fn shared_footprint_pads_do_not_short() {
    // Two abutting different-net pads inside ONE element (a solder-jumper style
    // placement): the footprint author's intent, not a board short. The two nets
    // share element JP1, so the sweep exempts their copper.
    let packages = r#"
<package name="JUMPER">
  <smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/>
  <smd name="2" x="0.9" y="0" dx="1" dy="1" layer="1"/>
</package>"#;
    let elements = r#"<element name="JP1" library="lib" package="JUMPER" x="5" y="5"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="JP1" pad="1"/>
</signal>
<signal name="B">
  <contactref element="JP1" pad="2"/>
</signal>
"#;
    let report = drc(packages, elements, signals);
    assert_eq!(report.short_count(), 0, "intra-footprint abutment is not a short");
}

#[test]
fn via_wire_overlap_is_a_short() {
    // A via on net A dropped onto a wire on net B. The via has an explicit
    // diameter and spans all copper layers, so it shorts the bottom-layer wire.
    let signals = r#"
<signal name="A">
  <via x="5" y="0" extent="1-16" drill="0.4" diameter="1.0"/>
</signal>
<signal name="B">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="16"/>
</signal>
"#;
    let report = drc("", "", signals);
    assert_short(&report, "A", "B");
    let f = report.shorts().next().unwrap();
    assert_eq!(f.layer, "B.Cu", "via reaches the bottom layer");
}

#[test]
fn via_without_diameter_uses_restring_rule() {
    // A via with no `diameter` attribute: the outer diameter is derived from the
    // drill and the restring rule. With drill 0.4 mm and the default restring,
    // the ring clamps to 0.2032 mm so the diameter is ~0.8 mm; a wire 0.35 mm
    // from the via centre (inside that radius) is shorted.
    let signals = r#"
<signal name="A">
  <via x="5" y="0" extent="1-16" drill="0.4"/>
</signal>
<signal name="B">
  <wire x1="0" y1="0.35" x2="10" y2="0.35" width="0.1" layer="1"/>
</signal>
"#;
    let report = drc("", "", signals);
    assert_short(&report, "A", "B");
}

#[test]
fn via_outside_wire_is_clean() {
    // The same via, but the wire is well clear of it.
    let signals = r#"
<signal name="A">
  <via x="50" y="50" extent="1-16" drill="0.4" diameter="1.0"/>
</signal>
<signal name="B">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="16"/>
</signal>
"#;
    let report = drc("", "", signals);
    assert!(report.is_clean(), "no shorts: {}", report.short_count());
}

#[test]
fn different_layers_do_not_short() {
    // Two crossing wires on opposite copper layers: separated by the dielectric,
    // not a short.
    let signals = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.5" layer="1"/>
</signal>
<signal name="B">
  <wire x1="5" y1="-5" x2="5" y2="5" width="0.5" layer="16"/>
</signal>
"#;
    let report = drc("", "", signals);
    assert!(report.is_clean(), "cross-layer crossings are not shorts");
}

#[test]
fn mirrored_package_pad_is_placed_on_the_bottom() {
    // A mirrored element (rot="MR0") flips its top-layer SMD onto the bottom
    // copper. A bottom-layer wire on a different net hitting that pad is a short;
    // the SAME geometry on the top layer must NOT short (the pad moved off it).
    let packages = r#"
<package name="PADM">
  <smd name="1" x="0" y="0" dx="1.5" dy="1.5" layer="1"/>
</package>"#;
    let elements = r#"<element name="U1" library="lib" package="PADM" x="5" y="0" rot="MR0"/>"#;
    // Bottom-layer wire crossing the (now bottom) pad: a short.
    let bottom = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="16"/>
</signal>
<signal name="B">
  <contactref element="U1" pad="1"/>
</signal>
"#;
    let r = drc(packages, elements, bottom);
    assert_short(&r, "A", "B");
    assert_eq!(r.shorts().next().unwrap().layer, "B.Cu");

    // The identical wire on the TOP layer does not short: the mirrored pad is no
    // longer there.
    let top = bottom.replace(r#"layer="16""#, r#"layer="1""#);
    let r2 = drc(packages, elements, &top);
    assert!(r2.is_clean(), "top-layer wire must not hit the mirrored (bottom) pad");
}

#[test]
fn designrules_clearance_is_respected() {
    // Two wires 0.3 mm edge-to-edge. Under a loose 6 mil (0.1524 mm) rule: clean.
    // Under a strict 20 mil (0.508 mm) rule embedded in the board: a clearance
    // violation. The board's own rule is read, not a hardcoded default.
    let signals = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.2" layer="1"/>
</signal>
<signal name="B">
  <wire x1="0" y1="0.5" x2="10" y2="0.5" width="0.2" layer="1"/>
</signal>
"#;
    let loose = r#"<designrules name="loose"><param name="mdWireWire" value="6mil"/></designrules>"#;
    let r_loose = drc_rules("", "", signals, loose);
    assert!(
        (r_loose.clearance_mm - 0.1524).abs() < 1e-3,
        "6 mil rule read as ~0.1524 mm, got {}",
        r_loose.clearance_mm
    );
    assert!(r_loose.findings.is_empty(), "0.15 mm rule: the 0.3 mm gap is clean");

    let strict = r#"<designrules name="strict"><param name="mdWireWire" value="20mil"/></designrules>"#;
    let r_strict = drc_rules("", "", signals, strict);
    assert!(
        (r_strict.clearance_mm - 0.508).abs() < 1e-3,
        "20 mil rule read as ~0.508 mm, got {}",
        r_strict.clearance_mm
    );
    assert_eq!(r_strict.short_count(), 0);
    assert!(
        r_strict.clearance_violations().count() >= 1,
        "0.508 mm rule flags the 0.3 mm gap"
    );
}

#[test]
fn no_designrules_falls_back_to_default() {
    // With no <designrules> block, the clearance falls back to 0.2 mm.
    let signals = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.2" layer="1"/>
</signal>
<signal name="B">
  <wire x1="0" y1="5" x2="10" y2="5" width="0.2" layer="1"/>
</signal>
"#;
    let report = drc_rules("", "", signals, "");
    assert!(
        (report.clearance_mm - galvani_extract::DEFAULT_CLEARANCE_MM).abs() < 1e-9,
        "fallback clearance is the 0.2 mm default, got {}",
        report.clearance_mm
    );
}

#[test]
fn curved_wire_is_flattened_and_detected() {
    // A wire with a 90-degree curve attribute sweeps an arc. A straight wire on a
    // different net crossing the arc's path is a short, proving the arc is
    // flattened (a chord-only approximation would miss the bulge).
    let signals = r#"
<signal name="A">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="1" curve="90"/>
</signal>
<signal name="B">
  <wire x1="5" y1="-6" x2="5" y2="-2" width="0.4" layer="1"/>
</signal>
"#;
    // The +90 arc from (0,0) to (10,0) bulges downward (to y<0); the vertical B
    // wire at x=5 reaches up to y=-2, into the arc.
    let report = drc("", "", signals);
    assert_short(&report, "A", "B");
}

#[test]
fn octagon_pad_shape_is_detected() {
    // A through-hole octagon pad on net A overlapping a wire on net B.
    let packages = r#"
<package name="OCT">
  <pad name="1" x="0" y="0" drill="0.6" diameter="1.6" shape="octagon"/>
</package>"#;
    let elements = r#"<element name="U1" library="lib" package="OCT" x="5" y="0"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="U1" pad="1"/>
</signal>
<signal name="B">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="16"/>
</signal>
"#;
    let report = drc(packages, elements, signals);
    assert_short(&report, "A", "B");
}
