//! Geometric DRC tests for Eagle `.brd` boards: hand-authored minimal XML
//! fixtures with one deliberate violation of each geometry kind, asserting
//! exact detection, plus the design-rule-clearance and mirrored-package cases.
//!
//! The corpus sweep over the eight famous Eagle boards lives in
//! `eagle_drc_corpus.rs`.

use hauksbee_extract::{ExtractedBoard, ViolationKind};

use crate::support::assert_short;

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
    board_with_libraries(
        &library_xml(library, packages),
        elements,
        signals,
        designrules,
    )
}

/// One `<library>` block carrying `packages`.
fn library_xml(name: &str, packages: &str) -> String {
    format!("<library name=\"{name}\">\n<packages>\n{packages}\n</packages>\n</library>")
}

/// [`board_in_library`] over any number of pre-built `<library>` blocks.
fn board_with_libraries(
    libraries: &str,
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
{libraries}
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
const fn default_rules() -> &'static str {
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

/// Signals `A` and `B` carrying the given copper/contact bodies.
fn ab(a: &str, b: &str) -> String {
    named_pair("A", a, "B", b)
}

/// Two signals with the given names and bodies.
fn named_pair(name_a: &str, a: &str, name_b: &str, b: &str) -> String {
    format!("<signal name=\"{name_a}\">\n  {a}\n</signal>\n<signal name=\"{name_b}\">\n  {b}\n</signal>\n")
}

/// Package `P`: a single 0.5 mm SMD pad `1` at the origin.
const ONE_PAD_PKG: &str = r#"
<package name="P">
  <smd name="1" x="0" y="0" dx="0.5" dy="0.5" layer="1"/>
</package>
"#;

/// An `<element>` with no `package` attribute is schema-invalid and lands with
/// zero pins (its own pad connectivity lost), but extraction must not crash and
/// the nets it touched must survive.
#[test]
fn element_missing_package_still_extracts_and_keeps_nets() {
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
    let text = board(ONE_PAD_PKG, elements, signals, default_rules());
    let brd = ExtractedBoard::from_eagle_brd(&text).expect("eagle extraction succeeds");
    assert!(
        brd.nets.iter().any(|n| n.name == "NET1"),
        "the net survives even though U2 has no package: {:?}",
        brd.nets.iter().map(|n| &n.name).collect::<Vec<_>>()
    );
}

/// An Eagle <element> marked populate="no" (do-not-populate /
/// assembly variant) must extract with dnp=true, matching the KiCad readers.
/// A populated element (no populate attr) stays dnp=false.
#[test]
fn eagle_populate_no_sets_dnp() {
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
    let text = board(ONE_PAD_PKG, elements, signals, default_rules());
    let brd = ExtractedBoard::from_eagle_brd(&text).expect("eagle extraction succeeds");
    let r1 = brd.component("R1").expect("R1");
    let r2 = brd.component("R2").expect("R2");
    assert!(!r1.dnp, "R1 has no populate attribute -> populated");
    assert!(r2.dnp, "R2 populate=\"no\" -> do-not-populate");
}

/// A mirrored, rotated Eagle element (`MR90`) must place its pads with
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
    let u1 = brd.component("U1").expect("U1");
    let p1 = u1.pins.iter().find(|p| p.number == "1").expect("pad 1");
    let (x, y) = p1.position.expect("pad 1 has a position");
    // flip-X then rotate by -90°: local (2, 0) -> world (0, +2). The old +90°
    // form put it at (0, -2), on the wrong side of the package origin.
    assert!(
        (x - 0.0).abs() < 1e-6 && (y - 2.0).abs() < 1e-6,
        "MR90 pad expected at (0, 2), got ({x}, {y})"
    );
}

/// Eagle namespaces packages per <library>. Two libraries each
/// defining a package named "COMMON" (with different pads) must NOT merge, an
/// element keyed to one library's package must get only that library's pads,
/// not the concatenation of both.
#[test]
fn eagle_same_named_packages_in_different_libraries_do_not_merge() {
    // liba::COMMON has pads 1,2; libb::COMMON has pads 3,4. C1 uses liba.
    let common = |a: &str, b: &str| {
        format!(
            r#"<package name="COMMON">
  <smd name="{a}" x="0" y="0" dx="0.5" dy="0.5" layer="1"/>
  <smd name="{b}" x="1" y="0" dx="0.5" dy="0.5" layer="1"/>
</package>"#
        )
    };
    let libraries =
        library_xml("liba", &common("1", "2")) + &library_xml("libb", &common("3", "4"));
    let elements = r#"<element name="C1" library="liba" package="COMMON" value="" x="0" y="0"/>"#;
    let signals = r#"<signal name="NET1"><contactref element="C1" pad="1"/></signal>"#;
    let text = board_with_libraries(&libraries, elements, signals, "");
    let brd = ExtractedBoard::from_eagle_brd(&text).expect("eagle extraction succeeds");
    let c1 = brd.component("C1").expect("C1");
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
    let [a, b] = WIRE_WIRE;
    let report = drc("", "", &ab(a, b));
    assert!(report.primitive_count > 0, "Eagle geometry was extracted");
}

/// One Eagle copper kind overlapping foreign copper: (name, packages, elements,
/// signals, whether exactly one contact is made (a flattened arc can touch a
/// crossing wire in two links), the layer and the pad owner the finding must name).
const WIRE_WIRE: [&str; 2] = [
    r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.5" layer="1"/>"#,
    r#"<wire x1="5" y1="-5" x2="5" y2="5" width="0.5" layer="1"/>"#,
];
// The package defines pad "1", U1 places it, the contactref nets it.
const SMD_PKG: &str = r#"
<package name="PAD1">
  <smd name="1" x="0" y="0" dx="1.5" dy="1.5" layer="1"/>
</package>"#;
const SMD_EL: &str = r#"<element name="U1" library="lib" package="PAD1" x="5" y="0"/>"#;
const WIRE_SMD: [&str; 2] = [
    r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="1"/>"#,
    r#"<contactref element="U1" pad="1"/>"#,
];
// An explicit-diameter via spans every copper layer, so it shorts the
// bottom-layer wire.
const VIA_WIRE: [&str; 2] = [
    r#"<via x="5" y="0" extent="1-16" drill="0.4" diameter="1.0"/>"#,
    r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="16"/>"#,
];
// The +90 arc from (0,0) to (10,0) bulges to y<0; the vertical wire at
// x=5 reaches up to y=-2, into the arc. A chord-only approximation
// would miss the bulge.
const CURVED_WIRE: [&str; 2] = [
    r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="1" curve="90"/>"#,
    r#"<wire x1="5" y1="-6" x2="5" y2="-2" width="0.4" layer="1"/>"#,
];
const OCT_PKG: &str = r#"
<package name="OCT">
  <pad name="1" x="0" y="0" drill="0.6" diameter="1.6" shape="octagon"/>
</package>"#;
const OCT_EL: &str = r#"<element name="U1" library="lib" package="OCT" x="5" y="0"/>"#;
const OCT_WIRE: [&str; 2] = [
    r#"<contactref element="U1" pad="1"/>"#,
    r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="16"/>"#,
];

/// (name, packages, elements, signals, exactly one short, layer, owner)
type Overlap = (
    &'static str,
    &'static str,
    &'static str,
    [&'static str; 2],
    bool,
    Option<&'static str>,
    Option<&'static str>,
);

#[rustfmt::skip]
const OVERLAPS: &[Overlap] = &[
    ("wire/wire crossing on top copper", "", "", WIRE_WIRE, true, Some("F.Cu"), None),
    ("wire/smd pad", SMD_PKG, SMD_EL, WIRE_SMD, true, None, Some("U1")),
    ("via/bottom wire", "", "", VIA_WIRE, true, Some("B.Cu"), None),
    ("curved wire/straight wire", "", "", CURVED_WIRE, false, None, None),
    ("octagon through-hole pad/wire", OCT_PKG, OCT_EL, OCT_WIRE, false, None, None),
];

#[test]
fn each_copper_kind_overlapping_foreign_copper_is_a_short() {
    for &(name, packages, elements, signals, exactly_one, layer, owner) in OVERLAPS {
        let report = drc(packages, elements, &ab(signals[0], signals[1]));
        assert_short(&report, "A", "B");
        if exactly_one {
            assert_eq!(report.short_count(), 1, "{name}: exactly one short");
        }
        crate::support::assert_overlap_short(&report, name, layer, owner);
    }
}

#[test]
fn separated_wires_report_nothing() {
    let cases = [
        (
            "wires 5 mm apart",
            ab(
                r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.25" layer="1"/>"#,
                r#"<wire x1="0" y1="5" x2="10" y2="5" width="0.25" layer="1"/>"#,
            ),
        ),
        (
            "crossing wires on opposite layers",
            ab(
                r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.5" layer="1"/>"#,
                r#"<wire x1="5" y1="-5" x2="5" y2="5" width="0.5" layer="16"/>"#,
            ),
        ),
    ];
    for (name, signals) in cases {
        let report = drc("", "", &signals);
        assert!(report.findings.is_empty(), "{name}: nothing reported");
    }
}

#[test]
fn parallel_wires_within_clearance_are_a_clearance_violation() {
    // Two 0.25 mm wires whose copper edges are 0.1 mm apart (centres 0.35 mm,
    // both half-widths 0.125). Under the 6 mil (0.1524 mm) rule: a clearance
    // violation, not a short.
    let signals = ab(
        r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.25" layer="1"/>"#,
        r#"<wire x1="0" y1="0.35" x2="10" y2="0.35" width="0.25" layer="1"/>"#,
    );
    let report = drc("", "", &signals);
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

/// The Arduino-convention solder jumper package: two pads bridged by copper.
const SJ_PKG: &str = r#"
<package name="SJ">
  <smd name="1" x="0" y="0" dx="1" dy="1" layer="1"/>
  <smd name="2" x="0.9" y="0" dx="1" dy="1" layer="1"/>
</package>"#;

/// Signals A and B each contacting one JP1 pad and carrying one wire.
fn jumper_signals(wire_a: &str, wire_b: &str) -> String {
    ab(
        &format!("<contactref element=\"JP1\" pad=\"1\"/>\n  {wire_a}"),
        &format!("<contactref element=\"JP1\" pad=\"2\"/>\n  {wire_b}"),
    )
}

#[test]
fn established_eagle_jumper_library_and_package_pair_is_local() {
    // EAGLE has no native net-tie flag. The Arduino convention uses two
    // independent class fields together: library="jumper", package="SJ".
    let elements = r#"<element name="JP1" library="jumper" package="SJ" x="20" y="20"/>"#;
    let signals = jumper_signals(
        r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="1"/>"#,
        r#"<wire x1="5" y1="-5" x2="5" y2="5" width="0.4" layer="1"/>"#,
    );
    let report = drc_in_library("jumper", SJ_PKG, elements, &signals);
    assert_eq!(
        report.short_count(),
        1,
        "the conventional SJ contact is local; the remote track short remains"
    );
    assert_short(&report, "A", "B");
}

#[test]
fn eagle_jumper_does_not_hide_ordinary_copper_crossing_over_its_pads() {
    let elements = r#"<element name="JP1" library="jumper" package="SJ" x="5" y="5"/>"#;
    let signals = jumper_signals(
        r#"<wire x1="0" y1="5" x2="10" y2="5" width="0.2" layer="1"/>"#,
        r#"<wire x1="5.45" y1="0" x2="5.45" y2="10" width="0.2" layer="1"/>"#,
    );

    assert_short(
        &drc_in_library("jumper", SJ_PKG, elements, &signals),
        "A",
        "B",
    );
}

/// EAGLE has no net-tie flag; a `SHORT`-style part is two abutting SMD pads
/// with `cream="no"`. Paste-free abutting pads are a local tie whatever the
/// library is called; the same pads with paste, or apart, are ordinary pads.
#[test]
fn eagle_paste_free_abutting_smds_are_a_local_tie() {
    let pkg = |cream: &str, x2: f64| {
        format!(
            r#"<package name="LINK">
  <smd name="1" x="0" y="0" dx="1" dy="1" layer="1"{cream}/>
  <smd name="2" x="{x2}" y="0" dx="1" dy="1" layer="1"{cream}/>
</package>"#
        )
    };
    let elements = r#"<element name="JP1" library="lib" package="LINK" x="20" y="20"/>"#;
    let wire_a = r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="1"/>"#;
    let remote = r#"<wire x1="5" y1="-5" x2="5" y2="5" width="0.4" layer="1"/>"#;
    // A second B wire over the A pad, used only on the open link.
    let over_pad =
        format!(r#"{remote}<wire x1="20" y1="17" x2="20" y2="23" width="0.2" layer="1"/>"#);
    let no_cream = r#" cream="no""#;
    let cases = [
        (
            pkg(no_cream, 0.9),
            remote,
            1,
            "only the remote crossing remains",
        ),
        (
            pkg("", 0.9),
            remote,
            2,
            "pasted abutting pads short as well",
        ),
        (
            pkg(no_cream, 1.2),
            over_pad.as_str(),
            2,
            "an open link is no tie: B over its A pad shorts",
        ),
    ];
    for (packages, wire_b, shorts, why) in cases {
        let report = drc(&packages, elements, &jumper_signals(wire_a, wire_b));
        assert_eq!(report.short_count(), shorts, "{why}: {:?}", report.findings);
        assert_short(&report, "A", "B");
    }
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
    let bottom = ab(
        r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.4" layer="16"/>"#,
        r#"<contactref element="U1" pad="1"/>"#,
    );
    let r = drc(packages, elements, &bottom);
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
fn designrules_clearance_is_respected() {
    // Two wires 0.3 mm edge-to-edge. Under a loose 6 mil (0.1524 mm) rule: clean.
    // Under a strict 20 mil (0.508 mm) rule embedded in the board: a clearance
    // violation. The board's own rule is read, not a hardcoded default.
    let signals = ab(
        r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.2" layer="1"/>"#,
        r#"<wire x1="0" y1="0.5" x2="10" y2="0.5" width="0.2" layer="1"/>"#,
    );
    for (rule, mm, violates) in [("6mil", 0.1524, false), ("20mil", 0.508, true)] {
        let rules = format!(
            r#"<designrules name="r"><param name="mdWireWire" value="{rule}"/></designrules>"#
        );
        let r = drc_rules("", "", &signals, &rules);
        assert!(
            (r.clearance_mm - mm).abs() < 1e-3,
            "{rule} rule read as ~{mm} mm, got {}",
            r.clearance_mm
        );
        assert_eq!(r.short_count(), 0, "{rule}: the wires never touch");
        if violates {
            let n = r.clearance_violations().count();
            assert!(n >= 1, "{mm} mm rule flags the 0.3 mm gap");
        } else {
            assert!(
                r.findings.is_empty(),
                "{mm} mm rule: the 0.3 mm gap is clean"
            );
        }
    }
}

#[test]
fn no_designrules_falls_back_to_default() {
    // With no <designrules> block, the clearance falls back to 0.2 mm.
    let signals = ab(
        r#"<wire x1="0" y1="0" x2="10" y2="0" width="0.2" layer="1"/>"#,
        r#"<wire x1="0" y1="5" x2="10" y2="5" width="0.2" layer="1"/>"#,
    );
    let report = drc_rules("", "", &signals, "");
    assert!(
        (report.clearance_mm - hauksbee_extract::DEFAULT_CLEARANCE_MM).abs() < 1e-9,
        "fallback clearance is the 0.2 mm default, got {}",
        report.clearance_mm
    );
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

/// The explicit 1-0 pair entry (0.5 mm) on top of [`TWO_CLASSES`].
const CROSS_ENTRY_CLASSES: &str = r#"
<classes>
<class number="0" name="default" width="0" drill="0">
<clearance class="0" value="0.15"/>
</class>
<class number="1" name="power" width="0" drill="0">
<clearance class="0" value="0.5"/>
<clearance class="1" value="0.4"/>
</class>
</classes>"#;

/// (case, classes, class of P1, class of P2, required clearance in mm)
#[rustfmt::skip]
const CLASS_CASES: &[(&str, &str, &str, &str, f64)] = &[
    // Both wires in class 1 (0.4 mm): the 0.3 mm gap violates the class rule
    // even though the design rules alone would pass it.
    ("same-class rule", TWO_CLASSES, "1", "1", 0.4),
    // Class 1 (0.4) vs class 0 (0.15), no explicit 1-0 cell: Eagle's rule is
    // that the larger class clearance governs. A design-rules-only fallback
    // (0.1524 mm) would silently under-report every power-to-signal pair.
    ("cross-class pair, no matrix entry", TWO_CLASSES, "1", "0", 0.4),
    // Same pair, but class 1 declares 0.5 mm to class 0: that entry governs.
    ("explicit cross-class entry", CROSS_ENTRY_CLASSES, "1", "0", 0.5),
];

#[test]
fn class_clearance_matrix_drives_the_required_clearance() {
    for &(name, classes, class_a, class_b, required) in CLASS_CASES {
        let rules = format!("{classes}{}", default_rules());
        let report = drc_rules("", "", &two_wire_signals(class_a, class_b), &rules);
        assert_eq!(report.short_count(), 0, "{name}: never a short");
        let f = report
            .clearance_violations()
            .next()
            .unwrap_or_else(|| panic!("{name}: 0.3 mm gap violates the {required} mm rule"));
        assert!(
            (f.required_clearance_mm - required).abs() < 1e-9,
            "{name}: the class rule drives the requirement, got {}",
            f.required_clearance_mm
        );
    }
}

// ---------------------------------------------------------------------------
// Copper pours. The .brd stores the requested outline plus its pour settings;
// the computed fill (with isolate antipads) is derived data. Foreign copper
// inside an outline is NOT a short (Eagle carves around it). Two overlapping
// same-rank pours of different signals get no rank arbitration and ARE reported.
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
    // carry.
    let settings = r#" rank="1" isolate="0.3" thermals="off" orphans="on""#;
    let signals = ab(
        &pour(settings, 0.0, 0.0, 10.0, 10.0),
        &pour(r#" rank="1""#, 5.0, 5.0, 15.0, 15.0),
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
fn arbitrated_or_cutout_pour_overlaps_are_silent() {
    let cases = [
        // rank 1 vs rank 2: the higher-numbered pour carves around the lower, so
        // the overlap is legal and must stay silent.
        ("rank arbitration makes the overlap legal", r#" rank="2""#),
        // pour="cutout" carves other pours and pours nothing itself: overlapping
        // a same-rank foreign pour is not a short.
        ("a cutout pours no copper", r#" rank="1" pour="cutout""#),
    ];
    for (name, b_attrs) in cases {
        let signals = ab(
            &pour(r#" rank="1""#, 0.0, 0.0, 10.0, 10.0),
            &pour(b_attrs, 5.0, 5.0, 15.0, 15.0),
        );
        let report = drc("", "", &signals);
        assert!(
            report.findings.is_empty(),
            "{name}: {:?}",
            report
                .findings
                .iter()
                .map(|f| (f.kind, f.net_a_name.clone(), f.net_b_name.clone()))
                .collect::<Vec<_>>()
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Declared net ties from a companion `.sch`
//
// An Eagle `.brd` records no net ties at all, so a deliberate join (a star
// ground drawn as one net's supply symbol placed on another's) is copper between
// two differently named nets and looks exactly like a solder bridge. The
// declaration lives in the schematic. These fixtures pin BOTH directions: a
// declared contact gains scoped context but stays serious, and an undeclared
// one gains no invented context.
// ─────────────────────────────────────────────────────────────────────────────

/// Two 0.5 mm wires of nets GND and AGND crossing on the top copper: a real,
/// measurable contact whatever the schematic says about it.
fn crossing_gnd_agnd() -> hauksbee_extract::DrcReport {
    let [gnd, agnd] = WIRE_WIRE;
    drc("", "", &named_pair("GND", gnd, "AGND", agnd))
}

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
fn a_declared_pair_adds_context_without_authorizing_the_contact() {
    let report = crossing_gnd_agnd();
    let before = report.shorts().next().cloned().expect("one short");
    let ties = hauksbee_extract::declared_net_ties(&schematic_declaring_the_tie())
        .expect("schematic parses");
    assert_eq!(ties.len(), 1, "one declaration, got {ties:?}");

    let qualified = report.qualify_with_declared_ties("emonTx.sch", &ties);
    assert_eq!(qualified.qualified_count(), 0);
    assert_eq!(qualified.matched_declaration_count(), 1);

    // Still one short, still the same measurement: schematic context cannot
    // change the physical observation.
    assert_eq!(report.short_count(), 1, "the measured finding survives");
    let after = report.shorts().next().expect("still one short");
    assert_eq!(after.net_a_name, before.net_a_name);
    assert_eq!(after.net_b_name, before.net_b_name);
    assert_eq!(after.layer, before.layer);
    assert_eq!(after.gap_mm.to_bits(), before.gap_mm.to_bits());
    assert_eq!((after.x, after.y), (before.x, before.y));

    // The schematic carries useful pair context but no board coordinate, so
    // the finding still gates and the context uses the non-authorizing API.
    assert_eq!(
        qualified.undeclared_shorts(&report).count(),
        1,
        "a coordinate-free declaration cannot excuse a physical contact"
    );
    let tie = qualified
        .declaration_for(after)
        .expect("carries the schematic context");
    assert_eq!(tie.declaration, "AGND7 wired to SUPPLY6 in net GND");
    assert_eq!(tie.source, "emonTx.sch");
    // And the run records which file it read, replacing the "supply it" hint.
    let source = qualified.source_summary();
    assert!(source.contains("emonTx.sch"), "{source}");
}

#[test]
fn copper_the_schematic_does_not_declare_stays_a_serious_short() {
    // Side (c), the false-negative guard the reverted geometry narrowing failed.
    // The schematic IS supplied and parses; it simply does not declare this tie.
    // Supplying a schematic must never be a way to silence a short.
    let report = crossing_gnd_agnd();
    let ties = hauksbee_extract::declared_net_ties(&schematic_declaring_nothing())
        .expect("schematic parses");
    assert!(ties.is_empty(), "this schematic declares no tie: {ties:?}");

    let qualified = report.qualify_with_declared_ties("separate-grounds.sch", &ties);
    assert_eq!(qualified.qualified_count(), 0, "nothing to qualify");
    assert_eq!(qualified.matched_declaration_count(), 0);
    assert_eq!(report.short_count(), 1);
    assert_eq!(
        qualified.undeclared_shorts(&report).count(),
        1,
        "the short still gates: the design does not claim this contact"
    );
    assert!(report.shorts().all(|f| qualified.tie_for(f).is_none()));
    // The source is still recorded, because "the schematic was read and declares
    // nothing here" is a stronger, different statement from never having looked.
    let source = qualified.source_summary();
    assert!(source.contains("0 declared net ties"), "{source}");
}
