//! Geometric DRC tests for Eagle `.brd` boards: hand-authored minimal XML
//! fixtures with one deliberate violation of each geometry kind, asserting
//! exact detection, plus the design-rule-clearance and mirrored-package cases.
//!
//! The corpus sweep over the eight famous Eagle boards lives in
//! `eagle_drc_corpus.rs`.

use hauksbee_extract::{ExtractedBoard, ViolationKind};

/// Wrap board geometry in a minimal Eagle 6 `.brd`. `designrules` lets a test
/// control the embedded clearance rule; `signals` carries the copper. Eagle is
/// y-up, millimetres. Two outer copper layers (1 = Top / F.Cu, 16 = Bottom /
/// B.Cu) are declared, matching the real corpus boards.
fn board_in_library(
    library: &str,
    packages: &str,
    elements: &str,
    signals: &str,
    designrules: &str,
) -> String {
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
<library name="{library}">
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

fn board(packages: &str, elements: &str, signals: &str, designrules: &str) -> String {
    board_in_library("lib", packages, elements, signals, designrules)
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

fn drc(packages: &str, elements: &str, signals: &str) -> hauksbee_extract::DrcReport {
    let text = board(packages, elements, signals, default_rules());
    ExtractedBoard::drc(&text).expect("eagle drc runs")
}

fn drc_in_library(
    library: &str,
    packages: &str,
    elements: &str,
    signals: &str,
) -> hauksbee_extract::DrcReport {
    let text = board_in_library(library, packages, elements, signals, default_rules());
    ExtractedBoard::drc(&text).expect("eagle drc runs")
}

fn drc_rules(
    packages: &str,
    elements: &str,
    signals: &str,
    rules: &str,
) -> hauksbee_extract::DrcReport {
    let text = board(packages, elements, signals, rules);
    ExtractedBoard::drc(&text).expect("eagle drc runs")
}

/// True if a short between two named nets exists.
fn assert_short(report: &hauksbee_extract::DrcReport, a: &str, b: &str) {
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

/// Bug-hunt #8: an <element> with no `package` attribute is schema-invalid and
/// lands with zero pins (its own pad connectivity lost), but extraction must not
/// crash and the nets it touched must survive. Guards the missing-package path
/// that now also emits a diagnostic.
#[test]
fn element_missing_package_still_extracts_and_keeps_nets() {
    let packages = r#"
<package name="P">
  <smd name="1" x="0" y="0" dx="0.5" dy="0.5" layer="1"/>
</package>
"#;
    // U1 has a package; U2 omits the attribute entirely.
    let elements = r#"
<element name="U1" library="lib" package="P" value="" x="0" y="0"/>
<element name="U2" library="lib" value="" x="5" y="0"/>
"#;
    let signals = r#"
<signal name="NET1">
  <contactref element="U1" pad="1"/>
  <contactref element="U2" pad="1"/>
</signal>
"#;
    let text = board(packages, elements, signals, default_rules());
    let brd = ExtractedBoard::from_eagle_brd(&text).expect("eagle extraction succeeds");
    assert!(
        brd.nets.iter().any(|n| n.name == "NET1"),
        "the net survives even though U2 has no package: {:?}",
        brd.nets.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
}

/// Round-4 #7: an Eagle <element> marked populate="no" (do-not-populate /
/// assembly variant) must extract with dnp=true, matching the KiCad readers.
/// A populated element (no populate attr) stays dnp=false.
#[test]
fn eagle_populate_no_sets_dnp() {
    let packages = r#"
<package name="P">
  <smd name="1" x="0" y="0" dx="0.5" dy="0.5" layer="1"/>
</package>
"#;
    let elements = r#"
<element name="R1" library="lib" package="P" value="10k" x="0" y="0"/>
<element name="R2" library="lib" package="P" value="10k" x="5" y="0" populate="no"/>
"#;
    let signals = r#"
<signal name="NET1">
  <contactref element="R1" pad="1"/>
  <contactref element="R2" pad="1"/>
</signal>
"#;
    let text = board(packages, elements, signals, default_rules());
    let brd = ExtractedBoard::from_eagle_brd(&text).expect("eagle extraction succeeds");
    let r1 = brd
        .components
        .iter()
        .find(|c| c.reference == "R1")
        .expect("R1");
    let r2 = brd
        .components
        .iter()
        .find(|c| c.reference == "R2")
        .expect("R2");
    assert!(!r1.dnp, "R1 has no populate attribute -> populated");
    assert!(r2.dnp, "R2 populate=\"no\" -> do-not-populate");
}

/// Round-5: a mirrored, rotated Eagle element (`MR90`) must place its pads with
/// the corpus-validated drc.rs handedness, flip-X then rotate by `-deg`, not
/// the old `+deg` form that put pads on the wrong side of the origin whenever
/// the rotation was not a multiple of 180.
#[test]
fn eagle_mirrored_rotated_pad_uses_drc_handedness() {
    let packages = r#"
<package name="P">
  <smd name="1" x="2" y="0" dx="0.5" dy="0.5" layer="1"/>
</package>
"#;
    let elements = r#"
<element name="U1" library="lib" package="P" value="" x="0" y="0" rot="MR90"/>
"#;
    let signals = r#"
<signal name="NET1">
  <contactref element="U1" pad="1"/>
</signal>
"#;
    let text = board(packages, elements, signals, default_rules());
    let brd = ExtractedBoard::from_eagle_brd(&text).expect("eagle extraction succeeds");
    let u1 = brd
        .components
        .iter()
        .find(|c| c.reference == "U1")
        .expect("U1");
    let p1 = u1.pins.iter().find(|p| p.number == "1").expect("pad 1");
    let (x, y) = p1.position.expect("pad 1 has a position");
    // flip-X then rotate by -90°: local (2, 0) -> world (0, +2). The old +90°
    // form put it at (0, -2), on the wrong side of the package origin.
    assert!(
        (x - 0.0).abs() < 1e-6 && (y - 2.0).abs() < 1e-6,
        "MR90 pad expected at (0, 2), got ({x}, {y})"
    );
}

/// Round-7 #5: the spin-prefixed mirror form `SMR90` must be recognised as
/// mirrored, exactly like `MR90`. `starts_with('M')` missed it (the string
/// starts with 'S'); `contains('M')`, matching drc.rs, catches it.
#[test]
fn eagle_spin_mirrored_element_is_recognised_as_mirrored() {
    let packages = r#"
<package name="P">
  <smd name="1" x="2" y="0" dx="0.5" dy="0.5" layer="1"/>
</package>
"#;
    let elements = r#"
<element name="U1" library="lib" package="P" value="" x="0" y="0" rot="SMR90"/>
"#;
    let signals = r#"
<signal name="NET1">
  <contactref element="U1" pad="1"/>
</signal>
"#;
    let text = board(packages, elements, signals, default_rules());
    let brd = ExtractedBoard::from_eagle_brd(&text).expect("eagle extraction succeeds");
    let u1 = brd
        .components
        .iter()
        .find(|c| c.reference == "U1")
        .expect("U1");
    // Mirrored -> bottom copper, and mirror-then-rotate places pad 1 at (0, 2)
    // just like MR90. Un-mirrored (the bug) would leave it on F.Cu at (0, -2).
    assert_eq!(u1.layer, "B.Cu", "SMR90 is mirrored -> bottom side");
    let p1 = u1.pins.iter().find(|p| p.number == "1").expect("pad 1");
    let (x, y) = p1.position.expect("pad 1 has a position");
    assert!(
        (x - 0.0).abs() < 1e-6 && (y - 2.0).abs() < 1e-6,
        "SMR90 pad expected at (0, 2), got ({x}, {y})"
    );
}

/// Round-7 #1: Eagle namespaces packages per <library>. Two libraries each
/// defining a package named "COMMON" (with different pads) must NOT merge, an
/// element keyed to one library's package must get only that library's pads,
/// not the concatenation of both.
#[test]
fn eagle_same_named_packages_in_different_libraries_do_not_merge() {
    // liba::COMMON has pads 1,2; libb::COMMON has pads 3,4. C1 uses liba.
    let text = r#"<?xml version="1.0" encoding="utf-8"?>
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
<library name="liba">
<packages>
<package name="COMMON">
  <smd name="1" x="0" y="0" dx="0.5" dy="0.5" layer="1"/>
  <smd name="2" x="1" y="0" dx="0.5" dy="0.5" layer="1"/>
</package>
</packages>
</library>
<library name="libb">
<packages>
<package name="COMMON">
  <smd name="3" x="0" y="0" dx="0.5" dy="0.5" layer="1"/>
  <smd name="4" x="1" y="0" dx="0.5" dy="0.5" layer="1"/>
</package>
</packages>
</library>
</libraries>
<elements>
<element name="C1" library="liba" package="COMMON" value="" x="0" y="0"/>
</elements>
<signals>
<signal name="NET1">
  <contactref element="C1" pad="1"/>
</signal>
</signals>
</board>
</drawing>
</eagle>
"#;
    let brd = ExtractedBoard::from_eagle_brd(text).expect("eagle extraction succeeds");
    let c1 = brd
        .components
        .iter()
        .find(|c| c.reference == "C1")
        .expect("C1");
    let mut nums: Vec<&str> = c1.pins.iter().map(|p| p.number.as_str()).collect();
    nums.sort_unstable();
    assert_eq!(
        nums,
        vec!["1", "2"],
        "C1 must carry only liba::COMMON's pads, not the merge of both libraries"
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
    assert!(
        f.gap_mm <= 0.0,
        "overlap gap is non-positive ({})",
        f.gap_mm
    );
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
fn established_eagle_jumper_library_and_package_pair_is_local() {
    // EAGLE has no native net-tie flag. The Arduino convention uses two
    // independent class fields together: library="jumper", package="SJ".
    let packages = r#"
<package name="SJ">
  <smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/>
  <smd name="2" x="0.9" y="0" dx="1" dy="1" layer="1"/>
</package>"#;
    let elements = r#"<element name="JP1" library="jumper" package="SJ" x="20" y="20"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="JP1" pad="1"/>
  <wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="1"/>
</signal>
<signal name="B">
  <contactref element="JP1" pad="2"/>
  <wire x1="5" y1="-5" x2="5" y2="5" width="0.4" layer="1"/>
</signal>
"#;
    let report = drc_in_library("jumper", packages, elements, signals);
    assert_eq!(
        report.short_count(),
        1,
        "the conventional SJ contact is local; the remote track short remains"
    );
    assert_short(&report, "A", "B");
}

#[test]
fn eagle_jumper_does_not_hide_ordinary_copper_crossing_over_its_pads() {
    let packages = r#"
<package name="SJ">
  <smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/>
  <smd name="2" x="0.9" y="0" dx="1" dy="1" layer="1"/>
</package>"#;
    let elements = r#"<element name="JP1" library="jumper" package="SJ" x="5" y="5"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="JP1" pad="1"/>
  <wire x1="0" y1="5" x2="10" y2="5" width="0.2" layer="1"/>
</signal>
<signal name="B">
  <contactref element="JP1" pad="2"/>
  <wire x1="5.45" y1="0" x2="5.45" y2="10" width="0.2" layer="1"/>
</signal>
"#;

    assert_short(
        &drc_in_library("jumper", packages, elements, signals),
        "A",
        "B",
    );
}

#[test]
fn eagle_jumper_allows_routes_that_terminate_on_its_declared_pad_pair() {
    // EAGLE has no native net-tie primitive: the established jumper convention
    // routes one signal from pad 1 into pad 2 while the other signal terminates
    // on pad 2. Both route endpoints are anchored to their corresponding
    // jumper pads, matching the Arduino Uno GROUND tie.
    let packages = r#"
<package name="SJ">
  <smd name="1" x="-0.75" y="0" dx="1" dy="1" layer="1"/>
  <smd name="2" x="0.75" y="0" dx="1" dy="1" layer="1"/>
</package>"#;
    let elements = r#"<element name="JP1" library="jumper" package="SJ" x="5" y="5"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="JP1" pad="1"/>
  <wire x1="4.25" y1="5" x2="5.25" y2="5" width="0.2" layer="1"/>
</signal>
<signal name="B">
  <contactref element="JP1" pad="2"/>
  <wire x1="5.75" y1="5" x2="5.25" y2="5" width="0.2" layer="1"/>
</signal>
"#;

    let report = drc_in_library("jumper", packages, elements, signals);
    assert!(
        report.is_clean(),
        "routes anchored to the declared jumper pads are local tie copper: {:?}",
        report.findings
    );
}

#[test]
fn a_jumper_name_in_one_field_does_not_exempt_copper() {
    let packages = r#"
<package name="JUMPER">
  <smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/>
  <smd name="2" x="0.9" y="0" dx="1" dy="1" layer="1"/>
</package>"#;
    let elements =
        r#"<element name="JP1" library="lib" package="JUMPER" value="JUMPER" x="5" y="5"/>"#;
    let signals = r#"
<signal name="A"><contactref element="JP1" pad="1"/></signal>
<signal name="B"><contactref element="JP1" pad="2"/></signal>
"#;
    assert_short(&drc(packages, elements, signals), "A", "B");
}

#[test]
fn ordinary_component_pads_on_different_nets_still_short() {
    let packages = r#"
<package name="QFN">
  <smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/>
  <smd name="2" x="0.9" y="0" dx="1" dy="1" layer="1"/>
</package>"#;
    let elements = r#"<element name="U1" library="lib" package="QFN" x="5" y="5"/>"#;
    let signals = r#"
<signal name="A"><contactref element="U1" pad="1"/></signal>
<signal name="B"><contactref element="U1" pad="2"/></signal>
"#;

    assert_short(&drc(packages, elements, signals), "A", "B");
}

#[test]
fn long_pad_uses_board_elongation_rule() {
    let packages = r#"
<package name="LONG_PAD">
  <pad name="1" x="0" y="0" drill="0.8" diameter="2" shape="long" rot="R90"/>
</package>"#;
    let elements = r#"<element name="J1" library="lib" package="LONG_PAD" x="5" y="5"/>"#;
    let signals = r#"
<signal name="A"><contactref element="J1" pad="1"/></signal>
<signal name="B"><wire x1="0" y1="6.7" x2="10" y2="6.7" width="0.2" layer="1"/></signal>
"#;
    let rules = r#"<designrules name="narrow-long-pad">
<param name="mdWireWire" value="0.1mm"/>
<param name="mdWirePad" value="0.1mm"/>
<param name="mdPadPad" value="0.1mm"/>
<param name="psElongationLong" value="50"/>
</designrules>"#;

    let report = drc_rules(packages, elements, signals, rules);
    assert_eq!(
        report.short_count(),
        0,
        "50% elongation makes total pad length 1.5x its diameter"
    );
}

#[test]
fn shared_component_does_not_exempt_unrelated_copper() {
    // R1 legitimately has one terminal on each net. That connectivity says
    // nothing about an A/B track collision elsewhere on the board.
    let packages = r#"
<package name="R0402">
  <smd name="1" x="-1" y="0" dx="0.5" dy="0.5" layer="1"/>
  <smd name="2" x="1" y="0" dx="0.5" dy="0.5" layer="1"/>
</package>"#;
    let elements = r#"<element name="R1" library="lib" package="R0402" x="20" y="20"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="R1" pad="1"/>
  <wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="1"/>
</signal>
<signal name="B">
  <contactref element="R1" pad="2"/>
  <wire x1="5" y1="-5" x2="5" y2="5" width="0.4" layer="1"/>
</signal>
"#;

    assert_short(&drc(packages, elements, signals), "A", "B");
}

#[test]
fn dual_field_eagle_jumper_exemption_is_local_to_its_copper() {
    let packages = r#"
<package name="SMT-JUMPER_2-NC_TRACE">
  <smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/>
  <smd name="2" x="0.9" y="0" dx="1" dy="1" layer="1"/>
</package>"#;
    let elements = r#"<element name="NT1" library="SparkFun-Jumpers" package="SMT-JUMPER_2-NC_TRACE" value="JUMPER-SMT" x="20" y="20"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="NT1" pad="1"/>
  <wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="1"/>
</signal>
<signal name="B">
  <contactref element="NT1" pad="2"/>
  <wire x1="5" y1="-5" x2="5" y2="5" width="0.4" layer="1"/>
</signal>
"#;
    let report = drc_in_library("SparkFun-Jumpers", packages, elements, signals);

    assert_eq!(
        report.short_count(),
        1,
        "only the unrelated track short fires"
    );
    assert_short(&report, "A", "B");
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
    assert!(
        r2.is_clean(),
        "top-layer wire must not hit the mirrored (bottom) pad"
    );
}

#[test]
fn mirror_reflects_x_not_y_for_offset_pads() {
    // Regression for the SparkFun RP2040 Thing Plus false-short class: an `MR0`
    // mirrored element must reflect its pads about the Y axis (negate local X),
    // NOT the X axis (negate local Y). With a pad OFFSET from the element origin
    // the two conventions place it on opposite sides, so this discriminates them
    // (the origin-pad test above cannot). The element sits at x=10; the pad is
    // local (+3, +4). Correct (flip-X): world (10-3, 0+4) = (7, 4). The buggy
    // flip-Y would give (10+3, -4) = (13, -4).
    let packages = r#"
<package name="OFFPAD">
  <smd name="1" x="3" y="4" dx="1" dy="1" layer="1"/>
</package>"#;
    let elements = r#"<element name="U1" library="lib" package="OFFPAD" x="10" y="0" rot="MR0"/>"#;
    // A short bottom wire centred on the flip-X position (7, 4): must short.
    let at_flipx = r#"
<signal name="A">
  <wire x1="6.5" y1="4" x2="7.5" y2="4" width="0.4" layer="16"/>
</signal>
<signal name="B">
  <contactref element="U1" pad="1"/>
</signal>
"#;
    let r = drc(packages, elements, at_flipx);
    assert_short(&r, "A", "B");

    // A wire at the WRONG flip-Y position (13, -4) must NOT short: nothing is
    // there under the correct transform.
    let at_flipy = r#"
<signal name="A">
  <wire x1="12.5" y1="-4" x2="13.5" y2="-4" width="0.4" layer="16"/>
</signal>
<signal name="B">
  <contactref element="U1" pad="1"/>
</signal>
"#;
    let r2 = drc(packages, elements, at_flipy);
    assert!(
        r2.is_clean(),
        "the pad must be at flip-X (7,4), not flip-Y (13,-4)"
    );
}

#[test]
fn mirrored_offset_th_pad_direction_is_reflected_for_mr0_and_mr180() {
    // A real EAGLE `shape="offset"` through-hole pad is asymmetric: the hole
    // sits at one end of the capsule. Mirroring must therefore reverse the pad
    // axis, not just move its centre. With the element origin at x=10:
    //   MR0   extends toward -X;
    //   MR180 extends toward +X.
    // A symmetric round/long pad would not discriminate this sign error.
    let packages = r#"
<package name="OFFSET_TH">
  <pad name="1" x="0" y="0" drill="0.8" diameter="2" shape="offset" rot="R0"/>
</package>"#;

    for (element_rotation, expected_x, reflected_away_x) in
        [("MR0", 7.5, 12.5), ("MR180", 12.5, 7.5)]
    {
        let elements = format!(
            r#"<element name="U1" library="lib" package="OFFSET_TH" x="10" y="0" rot="{element_rotation}"/>"#
        );
        let at_expected = format!(
            r#"
<signal name="A"><contactref element="U1" pad="1"/></signal>
<signal name="B"><wire x1="{expected_x}" y1="-1" x2="{expected_x}" y2="1" width="0.2" layer="1"/></signal>
"#
        );
        assert_short(&drc(packages, &elements, &at_expected), "A", "B");

        let at_wrong_side = format!(
            r#"
<signal name="A"><contactref element="U1" pad="1"/></signal>
<signal name="B"><wire x1="{reflected_away_x}" y1="-1" x2="{reflected_away_x}" y2="1" width="0.2" layer="1"/></signal>
"#
        );
        let report = drc(packages, &elements, &at_wrong_side);
        assert!(
            report.is_clean(),
            "{element_rotation}: no copper belongs on the unreflected side: {:?}",
            report.findings
        );
    }
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
    let loose =
        r#"<designrules name="loose"><param name="mdWireWire" value="6mil"/></designrules>"#;
    let r_loose = drc_rules("", "", signals, loose);
    assert!(
        (r_loose.clearance_mm - 0.1524).abs() < 1e-3,
        "6 mil rule read as ~0.1524 mm, got {}",
        r_loose.clearance_mm
    );
    assert!(
        r_loose.findings.is_empty(),
        "0.15 mm rule: the 0.3 mm gap is clean"
    );

    let strict =
        r#"<designrules name="strict"><param name="mdWireWire" value="20mil"/></designrules>"#;
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
        (report.clearance_mm - hauksbee_extract::DEFAULT_CLEARANCE_MM).abs() < 1e-9,
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

// ---------------------------------------------------------------------------
// Copper circles. A stroked <circle> is an annulus of its stroke width; the
// interior is bare board; only a zero-width circle is Eagle-filled solid.
// ---------------------------------------------------------------------------

#[test]
fn copper_inside_a_stroked_circle_is_not_a_short() {
    // Ring: radius 3, stroke 0.4 → copper only in the 2.8..3.2 band. A wire
    // through the middle sits 1.7 mm clear of the inner edge; the old
    // solid-disc model read a full overlap short.
    let signals = r#"
<signal name="A">
  <wire x1="-1" y1="0" x2="1" y2="0" width="0.2" layer="1"/>
</signal>
<signal name="B">
  <circle x="0" y="0" radius="3" width="0.4" layer="1"/>
</signal>
"#;
    let report = drc("", "", signals);
    assert!(
        report.findings.is_empty(),
        "the annulus hole is bare board: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.gap_mm))
            .collect::<Vec<_>>()
    );
}

#[test]
fn wire_crossing_the_annulus_ring_is_a_short() {
    // Control: copper crossing the stroked band itself still shorts, so the
    // annulus fix cannot pass by dropping the circle.
    let signals = r#"
<signal name="A">
  <wire x1="2" y1="0" x2="4" y2="0" width="0.2" layer="1"/>
</signal>
<signal name="B">
  <circle x="0" y="0" radius="3" width="0.4" layer="1"/>
</signal>
"#;
    assert_short(&drc("", "", signals), "A", "B");
}

#[test]
fn zero_width_circle_stays_a_filled_disc() {
    // Eagle renders width-0 circles filled; copper at the centre is a short.
    let signals = r#"
<signal name="A">
  <wire x1="-0.4" y1="0" x2="0.4" y2="0" width="0.2" layer="1"/>
</signal>
<signal name="B">
  <circle x="0" y="0" radius="1" width="0" layer="1"/>
</signal>
"#;
    assert_short(&drc("", "", signals), "A", "B");
}

// ---------------------------------------------------------------------------
// Net classes. <classes> is a clearance matrix: class N's own row entry is its
// same-class rule, an explicit cross-class entry pins that pair (and may relax
// below the classes' own rules), and a pair with NO entry uses the larger of
// the two classes' clearances. Everything is floored at the design rules.
// ---------------------------------------------------------------------------

/// Two parallel wires with 0.3 mm of copper-edge air between them (centres
/// 0.5 mm apart, both 0.2 mm wide): clear under the 6 mil (0.1524 mm) design
/// rule, inside a 0.4 mm class rule.
fn two_wire_signals(class_a: &str, class_b: &str) -> String {
    format!(
        r#"
<signal name="P1" class="{class_a}">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.2" layer="1"/>
</signal>
<signal name="P2" class="{class_b}">
  <wire x1="0" y1="0.5" x2="10" y2="0.5" width="0.2" layer="1"/>
</signal>
"#
    )
}

/// Class 0 (default, 0.15 mm) and class 1 (power, 0.4 mm same-class rule).
const TWO_CLASSES: &str = r#"
<classes>
<class number="0" name="default" width="0" drill="0">
<clearance class="0" value="0.15"/>
</class>
<class number="1" name="power" width="0" drill="0">
<clearance class="1" value="0.4"/>
</class>
</classes>"#;

#[test]
fn same_class_clearance_rule_is_applied() {
    // Both wires in class 1 (0.4 mm): the 0.3 mm gap violates the class rule
    // even though the design rules alone would pass it.
    let rules = format!("{TWO_CLASSES}{}", default_rules());
    let report = drc_rules("", "", &two_wire_signals("1", "1"), &rules);
    assert_eq!(report.short_count(), 0, "not touching, so never a short");
    let f = report
        .clearance_violations()
        .next()
        .expect("0.3 mm gap violates the 0.4 mm class rule");
    assert!(
        (f.required_clearance_mm - 0.4).abs() < 1e-9,
        "class rule drives the requirement, got {}",
        f.required_clearance_mm
    );
}

#[test]
fn cross_class_pair_without_matrix_entry_uses_the_larger_class_clearance() {
    // P1 in class 1 (0.4), P2 in class 0 (0.15), no explicit 1-0 matrix cell:
    // Eagle's rule for nets of different classes is that the larger of the
    // two class clearances governs, so the 0.3 mm gap violates the 0.4 mm
    // power-class rule. A design-rules-only fallback (0.1524 mm) would
    // silently under-report every power-to-signal pair.
    let rules = format!("{TWO_CLASSES}{}", default_rules());
    let report = drc_rules("", "", &two_wire_signals("1", "0"), &rules);
    assert_eq!(report.short_count(), 0, "not touching, so never a short");
    let f = report
        .clearance_violations()
        .next()
        .expect("0.3 mm gap violates the larger (0.4 mm) class clearance");
    assert!(
        (f.required_clearance_mm - 0.4).abs() < 1e-9,
        "the larger class clearance drives the requirement, got {}",
        f.required_clearance_mm
    );
}

#[test]
fn explicit_cross_class_matrix_entry_can_relax_below_the_larger_class() {
    // Same pair, but class 1 explicitly declares a 0.2 mm clearance to
    // class 0: the matrix cell overrides the larger-class fallback, so the
    // 0.3 mm gap is legal. This pins that explicit cells WIN (an
    // always-take-the-max model would still flag 0.4 here).
    let classes = r#"
<classes>
<class number="0" name="default" width="0" drill="0">
<clearance class="0" value="0.15"/>
</class>
<class number="1" name="power" width="0" drill="0">
<clearance class="0" value="0.2"/>
<clearance class="1" value="0.4"/>
</class>
</classes>"#;
    let rules = format!("{classes}{}", default_rules());
    let report = drc_rules("", "", &two_wire_signals("1", "0"), &rules);
    assert!(
        report.findings.is_empty(),
        "the explicit 0.2 mm matrix cell overrides the 0.4 mm class rule: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.gap_mm, f.required_clearance_mm))
            .collect::<Vec<_>>()
    );
}

#[test]
fn explicit_cross_class_matrix_entry_is_applied() {
    // Same pair, but class 1 declares a 0.5 mm clearance to class 0: the
    // 0.3 mm gap now violates it.
    let classes = r#"
<classes>
<class number="0" name="default" width="0" drill="0">
<clearance class="0" value="0.15"/>
</class>
<class number="1" name="power" width="0" drill="0">
<clearance class="0" value="0.5"/>
<clearance class="1" value="0.4"/>
</class>
</classes>"#;
    let rules = format!("{classes}{}", default_rules());
    let report = drc_rules("", "", &two_wire_signals("1", "0"), &rules);
    let f = report
        .clearance_violations()
        .next()
        .expect("0.3 mm gap violates the explicit 0.5 mm pair entry");
    assert!(
        (f.required_clearance_mm - 0.5).abs() < 1e-9,
        "pair entry drives the requirement, got {}",
        f.required_clearance_mm
    );
}

// ---------------------------------------------------------------------------
// Copper pours. The .brd stores the requested outline plus its pour settings;
// the computed fill (with isolate antipads) is derived data. Foreign copper
// inside an outline is NOT a short (Eagle carves around it). Two overlapping
// same-rank pours of different signals get no rank arbitration and ARE reported.
// That rule over-reports, at a rate measured against real fabrication output in
// `docs/evidence/KNOWN_FAULTS_VALIDATION.md` (right about four of six
// layer-instances); narrowing it by `isolate` was tried and reverted there,
// because it fixed one over-report and created a worse miss.
// ---------------------------------------------------------------------------

fn pour(rank_attr: &str, x0: f64, y0: f64, x1: f64, y1: f64) -> String {
    format!(
        r#"<polygon width="0.2" layer="1"{rank_attr}>
<vertex x="{x0}" y="{y0}"/>
<vertex x="{x1}" y="{y0}"/>
<vertex x="{x1}" y="{y1}"/>
<vertex x="{x0}" y="{y1}"/>
</polygon>"#
    )
}

#[test]
fn overlapping_same_rank_pours_of_different_nets_are_a_short() {
    // Pour settings ride along on the finding so a reader can see what the
    // overlap was made of. They do not gate it: whether two overlapping pours
    // end up in contact is a property of the fill, which the `.brd` does not
    // carry (see `docs/about/LIMITATIONS.md`).
    let signals = format!(
        r#"
<signal name="A">{}</signal>
<signal name="B">{}</signal>
"#,
        pour(
            r#" rank="1" isolate="0.3" thermals="off" orphans="on""#,
            0.0,
            0.0,
            10.0,
            10.0
        ),
        pour(r#" rank="1""#, 5.0, 5.0, 15.0, 15.0),
    );
    let report = drc("", "", &signals);
    assert_short(&report, "A", "B");
    let f = report.shorts().next().unwrap();
    assert_eq!(f.layer, "F.Cu");
    // The pour settings ride along as finding metadata.
    assert!(
        f.item_a.owner.contains("rank 1")
            && f.item_a.owner.contains("isolate 0.3")
            && f.item_a.owner.contains("thermals off")
            && f.item_a.owner.contains("orphans on"),
        "pour settings are disclosed on the finding, got {:?}",
        f.item_a.owner
    );
}

#[test]
fn overlapping_pours_with_different_ranks_are_arbitrated_not_shorted() {
    // rank 1 vs rank 2: the higher-numbered pour carves around the lower, so
    // the overlap is legal and must stay silent.
    let signals = format!(
        r#"
<signal name="A">{}</signal>
<signal name="B">{}</signal>
"#,
        pour(r#" rank="1""#, 0.0, 0.0, 10.0, 10.0),
        pour(r#" rank="2""#, 5.0, 5.0, 15.0, 15.0),
    );
    let report = drc("", "", &signals);
    assert!(
        report.findings.is_empty(),
        "rank arbitration makes the overlap legal: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.net_a_name.clone(), f.net_b_name.clone()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_cutout_polygon_is_not_copper() {
    // pour="cutout" carves other pours and pours nothing itself: overlapping a
    // same-rank foreign pour is not a short.
    let cutout = r#"<polygon width="0.2" layer="1" rank="1" pour="cutout">
<vertex x="5" y="5"/>
<vertex x="15" y="5"/>
<vertex x="15" y="15"/>
<vertex x="5" y="15"/>
</polygon>"#;
    let signals = format!(
        r#"
<signal name="A">{}</signal>
<signal name="B">{cutout}</signal>
"#,
        pour(r#" rank="1""#, 0.0, 0.0, 10.0, 10.0),
    );
    let report = drc("", "", &signals);
    assert!(
        report.findings.is_empty(),
        "a cutout pours no copper: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.net_a_name.clone(), f.net_b_name.clone()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn foreign_copper_inside_a_pour_outline_stays_silent() {
    // A via and a wire of another net fully inside a pour's outline: Eagle's
    // fill carves max(isolate, clearance) around them, so treating the drawn
    // outline as solid copper would manufacture false shorts.
    let signals = format!(
        r#"
<signal name="A">{}</signal>
<signal name="B">
  <via x="5" y="5" drill="0.3" diameter="0.6"/>
  <wire x1="3" y1="3" x2="7" y2="3" width="0.2" layer="1"/>
</signal>
"#,
        pour(r#" rank="1" isolate="0.3""#, 0.0, 0.0, 10.0, 10.0),
    );
    let report = drc("", "", &signals);
    assert!(
        report.findings.is_empty(),
        "the pour fill is carved around foreign copper: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.net_a_name.clone(), f.net_b_name.clone()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn grazing_clearance_at_the_annulus_outer_edge_is_not_lost_to_flattening() {
    // Ring radius 3, stroke 0.4: true outer copper edge at 3.2. A radial wire
    // whose copper tip stops 0.19 mm off that edge, aimed at 11.25 degrees —
    // the mid-chord angle of a coarse 16-segment flattening, where the chord
    // sags ~0.057 mm inward and would misreport the gap as ~0.25 mm (over the
    // 0.2 mm rule: silently dropped). The sagitta-bounded chain must report
    // the true ~0.19 mm clearance violation.
    let signals = r#"
<signal name="A">
  <wire x1="3.37391" y1="0.67110" x2="4.41357" y2="0.87790" width="0.1" layer="1"/>
</signal>
<signal name="B">
  <circle x="0" y="0" radius="3" width="0.4" layer="1"/>
</signal>
"#;
    let rules = r#"<designrules name="wide">
<param name="mdWireWire" value="0.2mm"/>
</designrules>"#;
    let report = drc_rules("", "", signals, rules);
    assert_eq!(report.short_count(), 0, "0.19 mm off the copper, no short");
    let f = report
        .clearance_violations()
        .next()
        .expect("a 0.19 mm gap violates the 0.2 mm rule");
    assert!(
        (0.178..0.198).contains(&f.gap_mm),
        "true grazing gap is ~0.19 mm (chord sag would say ~0.25), got {}",
        f.gap_mm
    );
}

#[test]
fn near_but_disjoint_same_rank_pours_are_not_a_short() {
    // Two same-rank pours whose vertex rings stop 0.1 mm apart, both drawn
    // with a 0.2 mm width: inflating the rings by width/2 would fabricate an
    // overlap short here. Ring overlap is what Eagle's DRC flags; disjoint
    // rings stay silent.
    let signals = format!(
        r#"
<signal name="A">{}</signal>
<signal name="B">{}</signal>
"#,
        pour(r#" rank="1""#, 0.0, 0.0, 10.0, 10.0),
        pour(r#" rank="1""#, 10.1, 0.0, 20.0, 10.0),
    );
    let report = drc("", "", &signals);
    assert!(
        report.findings.is_empty(),
        "disjoint pour rings are not an overlap: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.net_a_name.clone(), f.net_b_name.clone(), f.gap_mm))
            .collect::<Vec<_>>()
    );
}

#[test]
fn annulus_covering_inflation_keeps_an_exact_edge_touch_a_short() {
    // Ring radius 0.1, stroke 0.05: true outer copper edge at 0.125. A wire
    // whose copper tip reaches 0.1245 (0.5 um INTO the copper) aimed at
    // 11.25 degrees, the mid-chord angle of the 16-segment floor chain used
    // at this radius. Without the covering inflation the chord sags ~1.9 um
    // inward and this overlap would read as a positive 1.4 um gap (a
    // clearance note, not a short). The covering chain must report the short.
    let signals = r#"
<signal name="A">
  <wire x1="0.131916" y1="0.026240" x2="0.490393" y2="0.097545" width="0.02" layer="1"/>
</signal>
<signal name="B">
  <circle x="0" y="0" radius="0.1" width="0.05" layer="1"/>
</signal>
"#;
    assert_short(&drc("", "", signals), "A", "B");
}

#[test]
fn pour_without_rank_attribute_defaults_to_rank_one() {
    // Eagle board polygons behave as rank 1 when the attribute is elided, so
    // an attribute-less pour overlapping an explicit rank="1" pour of another
    // net is a same-rank overlap and must short. Defaulting the absent
    // attribute to any other value would silently arbitrate it away.
    let signals = format!(
        r#"
<signal name="A">{}</signal>
<signal name="B">{}</signal>
"#,
        pour("", 0.0, 0.0, 10.0, 10.0),
        pour(r#" rank="1""#, 5.0, 5.0, 15.0, 15.0),
    );
    assert_short(&drc("", "", &signals), "A", "B");
}

// ---------------------------------------------------------------------------
// Drawn copper (<circle> / <rectangle> in a signal) is exact copper, not a
// pour fill: a pad landing on it is a real short, and must not be swallowed
// by the Zone-Pad antipad-carve suppression that guards KiCad pour fills.
// ---------------------------------------------------------------------------

const ONE_SMD_PACKAGE: &str = r#"
<package name="P1X1">
  <smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/>
</package>"#;

#[test]
fn pad_on_a_drawn_copper_ring_is_a_short() {
    // SMD pad at (2, 0) lands on the radius-2 ring band (copper 1.8..2.2).
    let elements = r#"<element name="U1" library="lib" package="P1X1" x="2" y="0"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="U1" pad="1"/>
</signal>
<signal name="B">
  <circle x="0" y="0" radius="2" width="0.4" layer="1"/>
</signal>
"#;
    let report = drc(ONE_SMD_PACKAGE, elements, signals);
    assert_short(&report, "A", "B");
}

#[test]
fn pad_inside_a_drawn_ring_hole_stays_silent() {
    // The same pad at the ring centre: 1.3 mm of air to the band.
    let elements = r#"<element name="U1" library="lib" package="P1X1" x="0" y="0"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="U1" pad="1"/>
</signal>
<signal name="B">
  <circle x="0" y="0" radius="2" width="0.4" layer="1"/>
</signal>
"#;
    let report = drc(ONE_SMD_PACKAGE, elements, signals);
    assert!(
        report.findings.is_empty(),
        "the ring hole is bare board: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.gap_mm))
            .collect::<Vec<_>>()
    );
}

#[test]
fn pad_on_a_drawn_copper_rectangle_is_a_short() {
    let elements = r#"<element name="U1" library="lib" package="P1X1" x="2" y="0"/>"#;
    let signals = r#"
<signal name="A">
  <contactref element="U1" pad="1"/>
</signal>
<signal name="B">
  <rectangle x1="1" y1="-1" x2="3" y2="1" layer="1"/>
</signal>
"#;
    let report = drc(ONE_SMD_PACKAGE, elements, signals);
    assert_short(&report, "A", "B");
}

#[test]
fn curved_pour_edges_use_dense_flattening_for_the_rank_overlap_test() {
    // Pour B's closing edge is a 90-degree arc (radius 70.7) bulging toward
    // pour A. The true arc penetrates A's corner region by ~45-70 um, but the
    // coarse 8-segment chord chain misses A entirely (its nearest vertex sits
    // outside A's y-band and the adjacent chord only reaches A's edge line
    // beyond A's top edge). Only sagitta-bounded flattening finds this
    // same-rank overlap short.
    let pour_a = r#"<polygon width="0.2" layer="1" rank="1">
<vertex x="0" y="40"/>
<vertex x="99.92" y="40"/>
<vertex x="99.92" y="47"/>
<vertex x="0" y="47"/>
</polygon>"#;
    let pour_b = r#"<polygon width="0.2" layer="1" rank="1">
<vertex x="120.5" y="0"/>
<vertex x="200" y="0"/>
<vertex x="200" y="100"/>
<vertex x="120.5" y="100" curve="90"/>
</polygon>"#;
    let signals = format!(
        r#"
<signal name="A">{pour_a}</signal>
<signal name="B">{pour_b}</signal>
"#
    );
    assert_short(&drc("", "", &signals), "A", "B");
}

#[test]
fn class_clearance_below_the_design_rules_is_floored_at_the_design_rules() {
    // Class 1 declares a 0.1 mm same-class clearance under a 0.4 mm design
    // rule: Eagle ignores class values below the rules, so two class-1 wires
    // 0.3 mm apart still violate the 0.4 mm rule. Without the design-rule
    // floor the 0.1 mm class value would silently loosen the board.
    let classes = r#"
<classes>
<class number="1" name="loose" width="0" drill="0">
<clearance class="1" value="0.1"/>
</class>
</classes>"#;
    let rules = format!(
        "{classes}{}",
        r#"<designrules name="wide">
<param name="mdWireWire" value="0.4mm"/>
</designrules>"#
    );
    let report = drc_rules("", "", &two_wire_signals("1", "1"), &rules);
    let f = report
        .clearance_violations()
        .next()
        .expect("0.3 mm gap violates the floored 0.4 mm rule");
    assert!(
        (f.required_clearance_mm - 0.4).abs() < 1e-9,
        "class values below the design rules are floored, got {}",
        f.required_clearance_mm
    );
}

#[test]
fn cross_class_matrix_cell_below_the_design_rules_is_floored() {
    // Class 1 declares a 0.1 mm clearance to class 0 under a 0.4 mm design
    // rule: the explicit cell may relax below the classes' own rules but
    // never below the design rules, so cross-class wires 0.3 mm apart still
    // violate the floored 0.4 mm requirement.
    let classes = r#"
<classes>
<class number="0" name="default" width="0" drill="0">
<clearance class="0" value="0.15"/>
</class>
<class number="1" name="power" width="0" drill="0">
<clearance class="0" value="0.1"/>
<clearance class="1" value="0.45"/>
</class>
</classes>"#;
    let rules = format!(
        "{classes}{}",
        r#"<designrules name="wide">
<param name="mdWireWire" value="0.4mm"/>
</designrules>"#
    );
    let report = drc_rules("", "", &two_wire_signals("1", "0"), &rules);
    let f = report
        .clearance_violations()
        .next()
        .expect("0.3 mm gap violates the floored 0.4 mm pair rule");
    assert!(
        (f.required_clearance_mm - 0.4).abs() < 1e-9,
        "matrix cells below the design rules are floored, got {}",
        f.required_clearance_mm
    );
}

#[test]
fn divergent_design_rule_values_resolve_to_the_tightest() {
    // mdWireWire 0.4 mm alongside mdPadPad 0.15 mm: this path models ONE
    // clearance, the tightest copper-gating rule (0.15 mm), so two wires
    // 0.3 mm apart stay silent. Taking the loosest (or reading only
    // mdWireWire) would manufacture a violation here.
    let rules = r#"<designrules name="mixed">
<param name="mdWireWire" value="0.4mm"/>
<param name="mdPadPad" value="0.15mm"/>
</designrules>"#;
    let report = drc_rules("", "", &two_wire_signals("0", "0"), rules);
    assert!(
        (report.clearance_mm - 0.15).abs() < 1e-9,
        "the tightest md* rule is the model's single clearance, got {}",
        report.clearance_mm
    );
    assert!(
        report.findings.is_empty(),
        "0.3 mm gap clears the tightest (0.15 mm) rule: {:?}",
        report
            .findings
            .iter()
            .map(|f| (f.kind, f.gap_mm, f.required_clearance_mm))
            .collect::<Vec<_>>()
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Declared net ties from a companion `.sch`
//
// An Eagle `.brd` records no net ties at all, so a deliberate join (a star
// ground drawn as one net's supply symbol placed on another's) is copper between
// two differently named nets and looks exactly like a solder bridge. The
// declaration lives in the schematic. These fixtures pin BOTH directions: a
// declared contact is reclassified and still reported, and an undeclared one
// stays a serious short even when a schematic is supplied.
// ─────────────────────────────────────────────────────────────────────────────

/// Two 0.5 mm wires of nets GND and AGND crossing on the top copper: a real,
/// measurable contact whatever the schematic says about it.
const CROSSING_GND_AGND: &str = r#"
<signal name="GND">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.5" layer="1"/>
</signal>
<signal name="AGND">
  <wire x1="5" y1="-5" x2="5" y2="5" width="0.5" layer="1"/>
</signal>
"#;

/// A minimal Eagle 6 schematic carrying `parts` and `nets`. The supply symbols
/// are declared the way Eagle declares them: a library symbol whose single pin
/// has `direction="sup"`, the pin's name being the net it imposes.
fn schematic(nets: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE eagle SYSTEM "eagle.dtd">
<eagle version="6.6.0">
<drawing>
<schematic>
<libraries>
<library name="supply1">
<symbols>
<symbol name="GND">
<pin name="GND" x="0" y="2.54" visible="off" length="short" direction="sup" rot="R270"/>
</symbol>
<symbol name="AGND">
<pin name="AGND" x="0" y="2.54" visible="off" length="short" direction="sup" rot="R270"/>
</symbol>
</symbols>
<devicesets>
<deviceset name="GND" prefix="SUPPLY">
<gates><gate name="GND" symbol="GND" x="0" y="0"/></gates>
</deviceset>
<deviceset name="AGND" prefix="AGND">
<gates><gate name="VR1" symbol="AGND" x="0" y="0"/></gates>
</deviceset>
</devicesets>
</library>
</libraries>
<parts>
<part name="SUPPLY6" library="supply1" deviceset="GND" device=""/>
<part name="AGND7" library="supply1" deviceset="AGND" device=""/>
</parts>
<sheets>
<sheet><nets>
{nets}
</nets></sheet>
</sheets>
</schematic>
</drawing>
</eagle>
"#
    )
}

/// The emonTx construct: an AGND supply symbol wired to a GND supply symbol in
/// one segment of net GND.
fn schematic_declaring_the_tie() -> String {
    schematic(
        r#"<net name="GND" class="0">
<segment>
<pinref part="SUPPLY6" gate="GND" pin="GND"/>
<pinref part="AGND7" gate="VR1" pin="AGND"/>
<junction x="0" y="0"/>
</segment>
</net>"#,
    )
}

/// The same design with its grounds kept separate: each supply symbol sits on
/// its own net, so nothing is declared.
fn schematic_declaring_nothing() -> String {
    schematic(
        r#"<net name="GND" class="0">
<segment><pinref part="SUPPLY6" gate="GND" pin="GND"/></segment>
</net>
<net name="AGND" class="0">
<segment><pinref part="AGND7" gate="VR1" pin="AGND"/></segment>
</net>"#,
    )
}

#[test]
fn an_eagle_short_names_the_schematic_as_the_unlocking_upload() {
    // Side (b): the `.brd` alone. The contact is real and stays a short, and the
    // report must say which input would settle whether it is deliberate rather
    // than leaving the reader with an unresolvable finding.
    let report = drc("", "", CROSSING_GND_AGND);
    assert_eq!(report.short_count(), 1);
    assert_eq!(
        report.undeclared_short_count(),
        1,
        "with no schematic nothing is declared, so the short still gates"
    );
    assert!(
        report.declared_tie_source.is_none(),
        "no schematic was read"
    );
    let hint = report
        .tie_declaration_hint
        .as_deref()
        .expect("an Eagle short must name the unlocking upload");
    assert!(
        hint.contains(".sch"),
        "the hint must name the schematic: {hint}"
    );
    assert!(
        hint.contains("Supply") && hint.contains("re-run"),
        "the hint must say what to do with it: {hint}"
    );
    assert!(
        report.shorts().all(|f| f.declared_tie.is_none()),
        "nothing is qualified without a schematic"
    );
}

#[test]
fn a_clean_eagle_board_gains_no_unlocking_hint() {
    // The hint exists to resolve a finding. A board with no short has no finding
    // to resolve, so asking for an upload there would be noise.
    let report = drc(
        "",
        "",
        r#"
<signal name="GND">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.5" layer="1"/>
</signal>
<signal name="AGND">
  <wire x1="0" y1="5" x2="10" y2="5" width="0.5" layer="1"/>
</signal>
"#,
    );
    assert_eq!(report.short_count(), 0);
    assert!(report.tie_declaration_hint.is_none());
}

#[test]
fn a_declared_tie_reclassifies_the_short_without_removing_it() {
    // Side (a): the schematic declares GND/AGND tied, so the contact stops being
    // a defect. It must NOT stop being reported: the copper claim, its layer,
    // its location and its measured gap all survive intact.
    let mut report = drc("", "", CROSSING_GND_AGND);
    let before = report.shorts().next().cloned().expect("one short");
    let ties = hauksbee_extract::declared_net_ties(&schematic_declaring_the_tie())
        .expect("schematic parses");
    assert_eq!(ties.len(), 1, "one declaration, got {ties:?}");

    let qualified = report.qualify_with_declared_ties("emonTx.sch", &ties);
    assert_eq!(qualified, 1);

    // Still one short, still the same measurement. This is the reclassify-not-
    // delete contract: a user must be able to see that GND and AGND touch.
    assert_eq!(
        report.short_count(),
        1,
        "the finding survives reclassification"
    );
    let after = report.shorts().next().expect("still one short");
    assert_eq!(after.net_a_name, before.net_a_name);
    assert_eq!(after.net_b_name, before.net_b_name);
    assert_eq!(after.layer, before.layer);
    assert_eq!(after.gap_mm, before.gap_mm);
    assert_eq!((after.x, after.y), (before.x, before.y));

    // What changed: it no longer gates, and it carries the declaration.
    assert_eq!(
        report.undeclared_short_count(),
        0,
        "a declared tie is not a build failure"
    );
    let tie = after
        .declared_tie
        .as_ref()
        .expect("carries the declaration");
    assert_eq!(tie.declaration, "AGND7 wired to SUPPLY6 in net GND");
    assert_eq!(tie.source, "emonTx.sch");
    // And the run records which file it read, replacing the "supply it" hint.
    let source = report
        .declared_tie_source
        .as_deref()
        .expect("records source");
    assert!(source.contains("emonTx.sch"), "{source}");
    assert!(
        report.tie_declaration_hint.is_none(),
        "the schematic was supplied, so it is no longer a missing input"
    );
}

#[test]
fn copper_the_schematic_does_not_declare_stays_a_serious_short() {
    // Side (c), the false-negative guard the reverted geometry narrowing failed.
    // The schematic IS supplied and parses; it simply does not declare this tie.
    // Supplying a schematic must never be a way to silence a short.
    let mut report = drc("", "", CROSSING_GND_AGND);
    let ties = hauksbee_extract::declared_net_ties(&schematic_declaring_nothing())
        .expect("schematic parses");
    assert!(ties.is_empty(), "this schematic declares no tie: {ties:?}");

    let qualified = report.qualify_with_declared_ties("separate-grounds.sch", &ties);
    assert_eq!(qualified, 0, "nothing to qualify");
    assert_eq!(report.short_count(), 1);
    assert_eq!(
        report.undeclared_short_count(),
        1,
        "the short still gates: the design does not claim this contact"
    );
    assert!(report.shorts().all(|f| f.declared_tie.is_none()));
    // The source is still recorded, because "the schematic was read and declares
    // nothing here" is a stronger, different statement from never having looked.
    let source = report
        .declared_tie_source
        .as_deref()
        .expect("the schematic was read");
    assert!(source.contains("0 declared net ties"), "{source}");
}

#[test]
fn a_declared_tie_qualifies_only_the_pair_it_names() {
    // A board with two contacts, one declared and one not. The declaration is
    // per net pair, so it must not spill onto the other: flattening it to "this
    // board has a tie, stop reporting" is exactly the over-reach that would turn
    // a false positive into a false negative.
    let signals = format!(
        r#"{CROSSING_GND_AGND}
<signal name="+5V">
  <wire x1="20" y1="0" x2="30" y2="0" width="0.5" layer="1"/>
</signal>
<signal name="VBAT">
  <wire x1="25" y1="-5" x2="25" y2="5" width="0.5" layer="1"/>
</signal>
"#
    );
    let mut report = drc("", "", &signals);
    assert_eq!(report.short_count(), 2);
    let ties = hauksbee_extract::declared_net_ties(&schematic_declaring_the_tie())
        .expect("schematic parses");
    assert_eq!(report.qualify_with_declared_ties("emonTx.sch", &ties), 1);

    assert_eq!(report.short_count(), 2, "both contacts still reported");
    assert_eq!(
        report.undeclared_short_count(),
        1,
        "the undeclared +5V/VBAT contact still gates"
    );
    let gating: Vec<_> = report
        .undeclared_shorts()
        .map(|f| {
            let mut n = [f.net_a_name.as_str(), f.net_b_name.as_str()];
            n.sort_unstable();
            format!("{}/{}", n[0], n[1])
        })
        .collect();
    assert_eq!(gating, ["+5V/VBAT"]);
}

#[test]
fn a_declared_tie_does_not_excuse_a_clearance_violation() {
    // A clearance finding is a near-miss, not a contact, so there is nothing
    // about it a tie could excuse. Qualifying it would silently relax spacing.
    let signals = r#"
<signal name="GND">
  <wire x1="0" y1="0" x2="10" y2="0" width="0.5" layer="1"/>
</signal>
<signal name="AGND">
  <wire x1="0" y1="0.6" x2="10" y2="0.6" width="0.5" layer="1"/>
</signal>
"#;
    let mut report = drc("", "", signals);
    assert_eq!(report.short_count(), 0, "they do not touch");
    let before = report.clearance_violations().count();
    assert!(before > 0, "but they are inside the 0.1524 mm rule");
    let ties = hauksbee_extract::declared_net_ties(&schematic_declaring_the_tie())
        .expect("schematic parses");
    report.qualify_with_declared_ties("emonTx.sch", &ties);
    assert_eq!(
        report.clearance_violations().count(),
        before,
        "clearance findings are untouched"
    );
    assert!(
        report
            .clearance_violations()
            .all(|f| f.declared_tie.is_none()),
        "and none of them is marked declared"
    );
}
