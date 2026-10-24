//! Unit tests for the SI checks: pure-physics hand values, plus synthetic
//! minimal boards exercising fire / no-fire per check.

use super::*;
use crate::ExtractedBoard;

/// Full `kicad_pcb` text around a board `body`.
fn pcb_text(body: &str) -> String {
    format!("(kicad_pcb (version 20240101) (net 0 \"\") {body})")
}

fn pcb(body: &str) -> ExtractedBoard {
    ExtractedBoard::from_kicad_pcb(&pcb_text(body)).unwrap()
}

fn root_of(body: &str) -> forge_sexpr::Document {
    forge_sexpr::parse(&pcb_text(body)).unwrap()
}

/// Run a geometry-reading `check` over full `kicad_pcb` text, so the SAME text
/// drives both extraction and geometry parsing.
fn run_text(text: &str, check: impl FnOnce(&ExtractedBoard, &List, &mut SiReport)) -> SiReport {
    let b = ExtractedBoard::from_kicad_pcb(text).unwrap();
    let doc = forge_sexpr::parse(text).unwrap();
    let mut r = SiReport::default();
    check(&b, doc.root().unwrap(), &mut r);
    r
}

/// Run the board-only check `check` over `b`.
fn run_board(b: &ExtractedBoard, check: fn(&ExtractedBoard, &mut SiReport)) -> SiReport {
    let mut r = SiReport::default();
    check(b, &mut r);
    r
}

/// The I2C rise-time check with no routing geometry.
fn i2c_unrouted(b: &ExtractedBoard, r: &mut SiReport) {
    check_i2c_rise_time(b, None, r);
}

/// The I2C rise-time check counting routed trace capacitance.
fn i2c_routed(b: &ExtractedBoard, root: &List, r: &mut SiReport) {
    check_i2c_rise_time(b, Some(root), r);
}

// ---------------------------------------------------------------------------
// Pure physics.
// ---------------------------------------------------------------------------

#[test]
fn cl_series_and_board() {
    // Two 18 pF caps in series = 9 pF; +4 pF stray = 13 pF.
    assert!((cl_series(18.0, 18.0) - 9.0).abs() < 1e-9);
    assert!((cl_board_pf(18.0, 18.0, 4.0) - 13.0).abs() < 1e-9);
    // Two 15 pF (RP2040 hint) -> 7.5 + 4 = 11.5 pF.
    assert!((cl_board_pf(15.0, 15.0, 4.0) - 11.5).abs() < 1e-9);
    // Unequal: 12 and 22 -> 7.756.. pF.
    assert!((cl_series(12.0, 22.0) - 7.7647).abs() < 1e-3);
}

#[test]
fn i2c_rise_time_hand_values() {
    // 0.8473 * 4700 ohm * 100 pF = 398 ns (the classic 4.7k/100pF ~ 400 ns).
    let t = i2c_rise_time_ns(4700.0, 100.0);
    assert!((t - 398.2).abs() < 1.0, "4.7k/100pF = {t} ns");
    // Olimex UEXT: 2.2k x ~30 pF ~ 56 ns, well under 1000.
    let t2 = i2c_rise_time_ns(2200.0, 30.0);
    assert!(t2 < 60.0 && t2 > 50.0, "2.2k/30pF = {t2} ns");
    // A weak 10k pull with a heavy 250 pF bus blows standard mode (2118 ns).
    let t3 = i2c_rise_time_ns(10000.0, 250.0);
    assert!(t3 > T_R_STANDARD_NS, "10k/250pF = {t3} ns must exceed 1000");
}

#[test]
fn parse_helpers() {
    assert_eq!(super::parse_farads("15p"), Some(15e-12));
    assert_eq!(super::parse_farads("18pF"), Some(18e-12));
    assert_eq!(super::parse_farads("4p7"), Some(4.7e-12));
    assert_eq!(super::parse_farads("0.1uF"), Some(0.1e-6));
    assert_eq!(super::parse_farads("TBD"), None);
    // A trailing dielectric / voltage / tolerance token (space- or
    // letter-separated) is metadata, not a fractional part; the base value must
    // still parse, not drop to None (which produced a false "crystal has no load
    // caps" finding on a correctly-capped board). The "4p7" fraction form (digits
    // IMMEDIATELY after the unit) still works. (Approx compare: `a*mult` differs
    // from the literal in the last bit for some values, e.g. 22e-12.)
    let farads_approx = |s: &str, want: f64| {
        let got = super::parse_farads(s);
        assert!(
            got.is_some_and(|v| (v - want).abs() <= want.abs() * 1e-9),
            "parse_farads({s:?}) = {got:?}, want ~{want:e}"
        );
    };
    farads_approx("18pF C0G", 18e-12);
    farads_approx("18pF 50V", 18e-12);
    farads_approx("10n 5%", 10e-9);
    farads_approx("22p X7R", 22e-12);
    farads_approx("4p7", 4.7e-12); // fraction still parses
    farads_approx("2n2 50V", 2.2e-9); // fraction + rating
    assert_eq!(super::parse_ohms("2.2k/R0603"), Some(2200.0));
    assert_eq!(super::parse_ohms("4k7"), Some(4700.0));
    assert_eq!(super::parse_ohms("0R"), Some(0.0));
}

#[test]
fn routed_length_sums_segments() {
    let doc = root_of(
        r#"(net 1 "USB_DP")
           (segment (start 0 0) (end 3 0) (width 0.2) (layer "F.Cu") (net 1))
           (segment (start 3 0) (end 3 4) (width 0.2) (layer "F.Cu") (net 1))"#,
    );
    let l = routed_length_mm(doc.root().unwrap(), 1);
    assert!((l - 7.0).abs() < 1e-9, "3 + 4 = 7 mm, got {l}");
}

#[test]
fn arc_length_is_the_swept_arc_not_the_chord() {
    use std::f64::consts::PI;
    let r = 50.0;
    // Semicircle from (50,0) through (0,50) to (-50,0): pi*r = 157.08 mm, whose
    // chord is only 2r = 100 mm, a 36% under-report.
    let semi = arc_length_mm((r, 0.0), (0.0, r), (-r, 0.0));
    assert!(
        PI.mul_add(-r, semi).abs() < 1e-6,
        "semicircle is {} mm, got {semi}",
        PI * r
    );
    // Quarter turn: pi*r/2 = 78.54 mm against a 70.71 mm chord.
    let quarter = arc_length_mm(
        (r, 0.0),
        (r * (PI / 8.0).cos(), r * (PI / 8.0).sin()),
        (0.0, r),
    );
    assert!(
        (quarter - PI * r / 2.0).abs() < 1e-6,
        "quarter turn is {} mm, got {quarter}",
        PI * r / 2.0
    );
    // Major arc (270 degrees): summing the two half-sweeps must not wrap back to
    // the minor arc. 3/4 of the circle is 235.62 mm.
    let major = arc_length_mm((r, 0.0), (-r, 0.0), (0.0, -r));
    // Looser tolerance here on purpose: the start-to-mid sub-chord is the
    // diameter, so its half-sweep is asin(1), the worst-conditioned point of the
    // function. ~1e-6 relative error at a 236 mm length is far inside anything
    // the rise-time or skew limits can notice.
    assert!(
        (1.5 * PI).mul_add(-r, major).abs() < 1e-4,
        "270 degree arc is {} mm, got {major}",
        1.5 * PI * r
    );
    // Collinear points are a degenerate arc: the chord IS the length.
    let flat = arc_length_mm((0.0, 0.0), (5.0, 0.0), (10.0, 0.0));
    assert!((flat - 10.0).abs() < 1e-9, "got {flat}");
}

// ---------------------------------------------------------------------------
// Crystal load-cap check.
// ---------------------------------------------------------------------------

/// A standard 4-pin crystal with two terminals each carrying a load cap to GND.
fn xtal_board(xtal_val: &str, c1: &str, c2: &str) -> ExtractedBoard {
    pcb(&format!(
        r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3225-4Pin"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y1") (property "Value" "{xtal_val}")
          (pad "1" smd rect (at -1 0) (net 1 "XIN"))
          (pad "2" smd rect (at -1 1) (net 3 "GND"))
          (pad "3" smd rect (at 1 0) (net 2 "XOUT"))
          (pad "4" smd rect (at 1 1) (net 3 "GND")))
        (footprint "Capacitor_SMD:C_0402"
          (at 8 10) (layer "F.Cu")
          (property "Reference" "C1") (property "Value" "{c1}")
          (pad "1" smd rect (at 0 0) (net 1 "XIN"))
          (pad "2" smd rect (at 1 0) (net 3 "GND")))
        (footprint "Capacitor_SMD:C_0402"
          (at 12 10) (layer "F.Cu")
          (property "Reference" "C2") (property "Value" "{c2}")
          (pad "1" smd rect (at 0 0) (net 2 "XOUT"))
          (pad "2" smd rect (at 1 0) (net 3 "GND")))"#
    ))
}

#[test]
fn crystal_known_cl_within_tolerance_is_info_not_finding() {
    // ABM8-272 specs 10 pF (RP2040 datasheet Table 257); the Pico's two 15 pF
    // caps -> 7.5 + 4 = 11.5 pF, deviation 1.5 pF < 8 pF tolerance: info only.
    let b = xtal_board("ABM8-272-T3", "15p", "15p");
    let r = run_board(&b, check_crystal_load_cap);
    assert_eq!(r.finding_count(), 0, "within tolerance must not fire");
    assert!(r
        .of_check(SiCheck::CrystalLoadCap)
        .any(|f| f.severity == SiSeverity::Info));
}

#[test]
fn crystal_known_cl_far_off_fires() {
    // ABM8-272 specs 10 pF; two 34 pF caps -> 17 + 4 = 21 pF, deviation 11 pF
    // > 8 pF: fires (over-loaded -> runs slow, less gain margin). The 18 pF the
    // table used to claim would have passed this pair.
    let b = xtal_board("ABM8-272-T3", "34p", "34p");
    let r = run_board(&b, check_crystal_load_cap);
    assert_eq!(r.finding_count(), 1, "far-off CL must fire");
    let f = r.of_check(SiCheck::CrystalLoadCap).next().unwrap();
    assert!(f.message.contains("CL = 10.0 pF"), "{}", f.message);
    assert!(f.message.contains("Table 257"), "{}", f.message);
    // The message says what the mismatch does and what would fit.
    assert!(f.message.contains("ppm low"), "{}", f.message);
    assert!(f.message.contains("Caps that fit"), "{}", f.message);
}

/// A 32.768 kHz crystal whose value states no CL is judged against the class's
/// 12.5 pF standard CL with the usual 8 pF tolerance: 48 pF caps (CL ~28 pF)
/// fire Medium with the gain-margin reason; the common 22 pF pair (~15 pF) and
/// the 33 pF edge (20.5 pF, not over) are silent notes.
#[test]
fn watch_crystal_far_above_its_class_cl_fires() {
    let faulty = run_board(
        &xtal_board("32.768kHz", "48p", "48p"),
        check_crystal_load_cap,
    );
    assert_eq!(faulty.finding_count(), 1, "{:?}", faulty.findings);
    let f = faulty.findings_only().next().unwrap();
    assert_eq!(f.severity, SiSeverity::Medium);
    for says in [
        "CL ~ 28.0 pF",
        "12.5 pF standard CL",
        "ABS07",
        "gain margin",
        "motional",
    ] {
        assert!(f.message.contains(says), "{says:?} in {}", f.message);
    }
    for (value, cap) in [("32.768kHz", "22p"), ("32768", "33p"), ("32K", "12p")] {
        let legal = run_board(&xtal_board(value, cap, cap), check_crystal_load_cap);
        assert_eq!(
            legal.finding_count(),
            0,
            "{value} {cap}: {:?}",
            legal.findings
        );
    }
    // A MHz crystal with no stated CL keeps the no-judgement note.
    let mhz = run_board(&xtal_board("16MHz", "48p", "48p"), check_crystal_load_cap);
    assert_eq!(mhz.finding_count(), 0, "{:?}", mhz.findings);
}

/// An HC-49 can is a crystal outline whatever the reference prefix: `Q1` with
/// footprint `HC49UP` is the crystal on an emonTx-style board.
#[test]
fn an_hc49_footprint_marks_a_crystal() {
    let comp = |reference: &str, footprint: &str| Component {
        reference: reference.into(),
        value: "16Mhz".into(),
        footprint: footprint.into(),
        pins: vec![
            crate::Pin {
                number: "1".into(),
                net: Some(1),
                ..Default::default()
            },
            crate::Pin {
                number: "2".into(),
                net: Some(2),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert!(is_crystal(&comp("Q1", "HC49UP")));
    assert!(is_crystal(&comp("X2", "Crystal:Crystal_HC49-U_Vertical")));
    assert!(!is_crystal(&comp("Q1", "Package_TO_SOT_SMD:SOT-23")));
}

#[test]
fn crystal_unknown_cl_is_info_only() {
    // value is just the frequency, CL not derivable: never a finding.
    let b = xtal_board("12MHz", "18p", "18p");
    let r = run_board(&b, check_crystal_load_cap);
    assert_eq!(r.finding_count(), 0, "unknown CL must never fire");
    let f = r.of_check(SiCheck::CrystalLoadCap).next().unwrap();
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(f.message.contains("CL spec unknown"));
    assert!(f.message.contains("load capacitance unknown"));
}

/// A CL the crystal's own value states is the spec: four times the right load
/// caps (27 pF -> 108 pF, board CL ~58 pF against 18 pF) fires High and names
/// the stated CL; the board's real 27 pF pair (CL ~17.5 pF) is an ok note.
#[test]
fn crystal_cl_stated_in_its_value_is_judged() {
    let faulty = run_board(
        &xtal_board("12MHz 18pF", "108p", "108p"),
        check_crystal_load_cap,
    );
    assert_eq!(faulty.finding_count(), 1, "{:?}", faulty.findings);
    let f = faulty.findings_only().next().unwrap();
    assert_eq!(f.severity, SiSeverity::High);
    for says in [
        "CL ~ 57.0-59.0 pF",
        "CL = 18.0 pF",
        "the crystal's value states",
        "C1=108p",
    ] {
        assert!(f.message.contains(says), "{says:?} in {}", f.message);
    }
    for value in ["12MHz 18pF", "Q12MHz/20pF/30ppm/2P/HC-49SM(SMD)"] {
        let legal = run_board(&xtal_board(value, "27p", "27p"), check_crystal_load_cap);
        assert_eq!(legal.finding_count(), 0, "{value}: {:?}", legal.findings);
        assert!(
            legal.findings[0].message.contains("- ok"),
            "{}",
            legal.findings[0].message
        );
    }
    // A number that is not a plausible CL, or no pF unit, is not read as one.
    assert_eq!(stated_crystal_cl("32.768kHz"), None);
    assert_eq!(stated_crystal_cl("100pF-XTAL"), None);
    assert_eq!(stated_crystal_cl("16 MHz CL=12.5 pF"), Some(12.5));
}

/// The same crystal and load caps on a board whose ground is called `ground`.
fn xtal_board_grounded(ground: &str) -> ExtractedBoard {
    pcb(&format!(
        r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "{ground}")
        (footprint "Crystal:Crystal_SMD_3225-4Pin"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y1") (property "Value" "16MHz")
          (pad "1" smd rect (at -1 0) (net 1 "XIN"))
          (pad "2" smd rect (at -1 1) (net 3 "{ground}"))
          (pad "3" smd rect (at 1 0) (net 2 "XOUT"))
          (pad "4" smd rect (at 1 1) (net 3 "{ground}")))
        (footprint "Capacitor_SMD:C_0402"
          (at 8 10) (layer "F.Cu")
          (property "Reference" "C1") (property "Value" "22p")
          (pad "1" smd rect (at 0 0) (net 1 "XIN"))
          (pad "2" smd rect (at 1 0) (net 3 "{ground}")))
        (footprint "Capacitor_SMD:C_0402"
          (at 12 10) (layer "F.Cu")
          (property "Reference" "C2") (property "Value" "22p")
          (pad "1" smd rect (at 0 0) (net 2 "XOUT"))
          (pad "2" smd rect (at 1 0) (net 3 "{ground}")))"#
    ))
}

/// A load capacitor to a ground counts as one whatever the ground is called:
/// renaming the ground changes no circuit. The control keeps the "no load
/// capacitors" finding, so the check is live on each spelling.
#[test]
fn load_caps_to_a_ground_count_whatever_the_ground_is_called() {
    for ground in [
        "GND",
        "0V",
        "GND1",
        "GROUND",
        "GNDREF",
        "AGND",
        "VSS",
        "/sheet/0V",
    ] {
        let r = run_board(&xtal_board_grounded(ground), check_crystal_load_cap);
        assert_eq!(r.finding_count(), 0, "{ground}: C1 and C2 load the crystal");
        assert!(
            r.of_check(SiCheck::CrystalLoadCap)
                .any(|f| f.message.contains("pF")),
            "{ground}: the load the caps present is stated"
        );
    }
}

#[test]
fn crystal_missing_both_caps_fires() {
    // A discrete crystal with no load caps at all on either terminal.
    let b = pcb(r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3225-4Pin"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y1") (property "Value" "16MHz")
          (pad "1" smd rect (at -1 0) (net 1 "XIN"))
          (pad "2" smd rect (at -1 1) (net 3 "GND"))
          (pad "3" smd rect (at 1 0) (net 2 "XOUT"))
          (pad "4" smd rect (at 1 1) (net 3 "GND")))"#);
    let r = run_board(&b, check_crystal_load_cap);
    assert_eq!(r.finding_count(), 1, "no load caps must fire");
    assert_eq!(
        r.findings_only().next().unwrap().severity,
        SiSeverity::Medium
    );
}

#[test]
fn crystal_caps_with_unset_values_are_present_not_missing() {
    // A load cap whose value is still a placeholder exists; its size is only
    // unknown, so the crystal gets a no-judgement note, never "no load caps".
    for (c1, c2) in [("TBD", "TBD"), ("?", "18p"), ("", "")] {
        let b = xtal_board("16MHz", c1, c2);
        let r = run_board(&b, check_crystal_load_cap);
        assert_eq!(
            r.finding_count(),
            0,
            "unvalued caps ({c1:?}, {c2:?}) must not fire"
        );
        let f = r.of_check(SiCheck::CrystalLoadCap).next().unwrap();
        assert!(f.message.contains("unset or unreadable"), "{}", f.message);
    }
}

#[test]
fn rtc_with_integrated_caps_no_cap_is_silent() {
    // A 32.768 kHz crystal on a PCF8523 RTC (integrated load caps): no external
    // caps is CORRECT, must not fire. (The MNT Reform Y4 topology.)
    let b = pcb(r#"(net 1 "OSCI") (net 2 "OSCO") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3215-2Pin"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y4") (property "Value" "32.768 kHz")
          (pad "1" smd rect (at -1 0) (net 1 "OSCI"))
          (pad "2" smd rect (at 1 0) (net 2 "OSCO")))
        (footprint "Package_SO:SOIC-8"
          (at 14 10) (layer "F.Cu")
          (property "Reference" "U5") (property "Value" "PCF8523T")
          (pad "1" smd rect (at 0 0) (net 1 "OSCI"))
          (pad "2" smd rect (at 0 1) (net 2 "OSCO")))"#);
    let r = run_board(&b, check_crystal_load_cap);
    assert_eq!(r.finding_count(), 0, "RTC integrates caps; must be silent");
}

/// A 4-pin MHz crystal hung directly on a driver IC of value `driver`, with no
/// load caps anywhere.
fn xtal_on_driver(driver: &str) -> ExtractedBoard {
    pcb(&format!(
        r#"(net 1 "XI") (net 2 "XO") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3225-4Pin"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y1") (property "Value" "12MHz")
          (pad "1" smd rect (at -1 0) (net 1 "XI"))
          (pad "2" smd rect (at -1 1) (net 3 "GND"))
          (pad "3" smd rect (at 1 0) (net 2 "XO"))
          (pad "4" smd rect (at 1 1) (net 3 "GND")))
        (footprint "Package_QFN:QFN-48"
          (at 16 10) (layer "F.Cu")
          (property "Reference" "U1") (property "Value" "{driver}")
          (pad "22" smd rect (at 0 0) (net 1 "XI"))
          (pad "23" smd rect (at 0 1) (net 2 "XO")))"#
    ))
}

#[test]
fn crystal_on_a_driver_with_on_chip_load_caps_is_silent() {
    // Datasheet-documented on-chip load capacitance: the XR22417 hub (REV1A,
    // "External Crystal": load capacitors and feedback resistance are internal)
    // and the EFM32PG22's HFXO (on-chip CTUNE). Bare crystal pins are correct.
    for driver in ["MaxLinear_XR22417-48", "EFM32PG22C200F512IM40"] {
        let r = run_board(&xtal_on_driver(driver), check_crystal_load_cap);
        assert_eq!(
            r.finding_count(),
            0,
            "{driver} integrates its load caps; must be silent: {:?}",
            r.findings
        );
    }
}

#[test]
fn crystal_on_a_driver_without_on_chip_caps_still_fires() {
    // The real-fault partner: the same bare crystal on a driver that does NOT
    // integrate load caps (an STM32F4 HSE oscillator needs two external caps).
    let r = run_board(&xtal_on_driver("STM32F405RGT6"), check_crystal_load_cap);
    assert_eq!(r.finding_count(), 1, "no caps, no integration: must fire");
}

/// A crystal `Y1` of `value` on a `Si5351C`'s XA/XB (pads 1 and 2), `cap` to ground
/// on each: the `HackRF` One's clock generator and its 25 MHz, 10 pF crystal.
fn xtal_on_si5351(value: &str, cap: &str) -> ExtractedBoard {
    pcb(&format!(
        r#"(net 1 "XA") (net 2 "XB") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3225-4Pin"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y1") (property "Value" "{value}")
          (pad "1" smd rect (at -1 0) (net 1 "XA"))
          (pad "2" smd rect (at -1 1) (net 3 "GND"))
          (pad "3" smd rect (at 1 0) (net 2 "XB"))
          (pad "4" smd rect (at 1 1) (net 3 "GND")))
        (footprint "Capacitor_SMD:C_0402"
          (at 8 10) (layer "F.Cu")
          (property "Reference" "C1") (property "Value" "{cap}")
          (pad "1" smd rect (at 0 0) (net 1 "XA"))
          (pad "2" smd rect (at 1 0) (net 3 "GND")))
        (footprint "Capacitor_SMD:C_0402"
          (at 12 10) (layer "F.Cu")
          (property "Reference" "C2") (property "Value" "{cap}")
          (pad "1" smd rect (at 0 0) (net 2 "XB"))
          (pad "2" smd rect (at 1 0) (net 3 "GND")))
        (footprint "Package_DFN_QFN:QFN-20-1EP_4x4mm_P0.5mm_EP2.5x2.5mm"
          (at 16 10) (layer "F.Cu")
          (property "Reference" "U1") (property "Value" "SI5351C")
          (pad "1" smd rect (at 0 0) (net 1 "XA"))
          (pad "2" smd rect (at 0 1) (net 2 "XB")))"#
    ))
}

/// A crystal on a Si5351 is loaded by the chip's own programmable capacitors
/// (12 to 20 pF on each of XA and XB, from its card), so the 1 pF the two 2 pF
/// caps add is right for a 10 pF crystal: the board can reach it, so nothing
/// fires, with no bound cards at all. A crystal the range cannot reach still does.
#[test]
fn a_clock_generators_programmable_load_is_read_from_its_card() {
    let ok = xtal_on_si5351("MAIN_XTAL 25MHz 10pF", "2p");
    let r = crystal_findings(&ok, &CrystalCards::new());
    assert!(fired(&r).is_empty(), "{r:?}");
    for says in ["on-chip per pin", "card 'si5351c_qfn20'", "p.60"] {
        assert!(r[0].message.contains(says), "{says:?} in {}", r[0].message);
    }
    // 12 pF is the datasheet's own example (10 pF setting plus 2 pF of external).
    let twelve = xtal_on_si5351("25MHz 12pF", "4p");
    assert!(fired(&crystal_findings(&twelve, &CrystalCards::new())).is_empty());
    // A 6 pF crystal on 22 pF caps is far past anything the setting can undo.
    let heavy = xtal_on_si5351("25MHz 6pF", "22p");
    let r = crystal_findings(&heavy, &CrystalCards::new());
    assert_eq!(fired(&r).len(), 1, "{r:?}");
    // The same board without the Si5351 is judged as a discrete crystal: the
    // 2 pF caps on a 10 pF crystal fire.
    let mut discrete = xtal_on_si5351("MAIN_XTAL 25MHz 10pF", "2p");
    set_value(&mut discrete, "U1", "SOMETHING_ELSE");
    assert_eq!(
        fired(&crystal_findings(&discrete, &CrystalCards::new())).len(),
        1
    );
}

/// The crystal findings of `b` judged with `cards`.
fn crystal_findings(b: &ExtractedBoard, cards: &CrystalCards) -> Vec<SiFinding> {
    b.si_checks_with(None, cards)
        .of_check(SiCheck::CrystalLoadCap)
        .cloned()
        .collect()
}

fn fired(f: &[SiFinding]) -> Vec<&SiFinding> {
    f.iter()
        .filter(|f| f.severity != SiSeverity::Info)
        .collect()
}

fn set_value(b: &mut ExtractedBoard, reference: &str, value: &str) {
    let c = b
        .components
        .iter_mut()
        .find(|c| c.reference == reference)
        .unwrap();
    c.value = value.into();
}

fn add_property(b: &mut ExtractedBoard, reference: &str, key: &str, value: &str) {
    let c = b
        .components
        .iter_mut()
        .find(|c| c.reference == reference)
        .unwrap();
    c.properties.push((key.into(), value.into()));
}

/// A 12 pF crystal on 27 pF caps is a real mismatch the old 8 pF tolerance let
/// through: the load is 4.5 pF heavier than the spec even at the lightest end of
/// the stray band, which pulls a typical 12 MHz crystal about 30 ppm low. The
/// message says so and names the caps that fit (2 x (12 - 4) = 16 pF, E12 15 pF),
/// and the fitting 15 pF pair is silent.
#[test]
fn a_cl_mismatch_reports_its_ppm_and_the_caps_that_fit() {
    let r = crystal_findings(
        &xtal_board("12MHz/12pF", "27p", "27p"),
        &CrystalCards::new(),
    );
    let f = fired(&r);
    assert_eq!(f.len(), 1, "{r:?}");
    assert_eq!(f[0].severity, SiSeverity::Medium);
    for says in [
        "CL ~ 16.5-18.5 pF",
        "CL = 12.0 pF",
        "4.5 pF heavier",
        "ppm low",
        "3.0-5.0 pF stray assumed",
        "hauksbee_stray_cap_pf",
        "16.0 pF each",
        "nearest E12 15.0 pF",
    ] {
        assert!(f[0].message.contains(says), "{says:?} in {}", f[0].message);
    }
    let fits = crystal_findings(
        &xtal_board("12MHz/12pF", "15p", "15p"),
        &CrystalCards::new(),
    );
    assert!(fired(&fits).is_empty(), "{fits:?}");
    assert!(fits[0].message.contains("- ok"), "{}", fits[0].message);
    // Too light a load pulls the other way.
    let light = crystal_findings(
        &xtal_board("12MHz/18pF", "6.8p", "6.8p"),
        &CrystalCards::new(),
    );
    assert!(fired(&light)[0].message.contains("ppm high"), "{light:?}");
}

/// The designer's own stray figure replaces the assumed band: 27 pF caps for a
/// 12 pF crystal are fine if the layout's stray is really 0 pF (13.5 pF), and
/// say so in the message when it is not.
#[test]
fn the_stray_property_overrides_the_assumed_band() {
    let mut b = xtal_board("12MHz/12pF", "27p", "27p");
    add_property(&mut b, "Y1", STRAY_PROPERTY, "0");
    let r = crystal_findings(&b, &CrystalCards::new());
    assert!(fired(&r).is_empty(), "{r:?}");
    assert!(
        r[0].message
            .contains("from the hauksbee_stray_cap_pf property"),
        "{}",
        r[0].message
    );
    let mut heavy = xtal_board("12MHz/12pF", "27p", "27p");
    add_property(&mut heavy, "Y1", STRAY_PROPERTY, "6");
    let r = crystal_findings(&heavy, &CrystalCards::new());
    assert_eq!(fired(&r).len(), 1, "{r:?}");
    assert!(r[0]
        .message
        .contains("from the hauksbee_stray_cap_pf property"));
}

/// A generic crystal takes its CL from its part-number property or a property
/// named for the load capacitance, not only its value.
#[test]
fn a_cl_in_the_part_number_or_a_load_property_is_the_spec() {
    // ABM8-272-T3 in the MPN property (value is only the frequency): CL 10 pF
    // from the table, so 34 pF caps (CL 20-22 pF) fire and 15 pF caps do not.
    let mut b = xtal_board("12MHz", "34p", "34p");
    add_property(&mut b, "Y1", "MPN", "ABM8-272-T3");
    let r = crystal_findings(&b, &CrystalCards::new());
    assert_eq!(fired(&r).len(), 1, "{r:?}");
    assert!(
        r[0].message.contains("from the MPN property"),
        "{}",
        r[0].message
    );
    let mut ok = xtal_board("12MHz", "15p", "15p");
    add_property(&mut ok, "Y1", "MPN", "ABM8-272-T3");
    assert!(fired(&crystal_findings(&ok, &CrystalCards::new())).is_empty());
    // A "Load Capacitance" property, bare number or with a unit.
    for text in ["8", "8 pF", "8pF"] {
        let mut b = xtal_board("12MHz", "27p", "27p");
        add_property(&mut b, "Y1", "Load Capacitance", text);
        let r = crystal_findings(&b, &CrystalCards::new());
        assert_eq!(fired(&r).len(), 1, "{text}: {r:?}");
        assert!(r[0].message.contains("CL = 8.0 pF"), "{}", r[0].message);
    }
    // No such property: nothing to judge against.
    let none = crystal_findings(&xtal_board("12MHz", "27p", "27p"), &CrystalCards::new());
    assert!(fired(&none).is_empty(), "{none:?}");
}

/// A generic crystal with no part card takes its CL from a description or
/// parameter field that writes it ("12pF") alongside the rest: heavy caps fire
/// on it, the matching caps stay silent. With nothing stating a CL the check
/// abstains with a plain "load capacitance unknown" note instead of a verdict.
#[test]
fn a_generic_crystal_takes_cl_from_its_description_and_abstains_when_unknown() {
    for key in ["Description", "Params", "Comment"] {
        let mut flawed = xtal_board("12MHz", "47p", "47p");
        add_property(&mut flawed, "Y1", key, "Crystal 12MHz 12pF 20ppm");
        let r = crystal_findings(&flawed, &CrystalCards::new());
        assert_eq!(fired(&r).len(), 1, "{key}: {r:?}");
        assert!(
            r[0].message.contains("CL = 12.0 pF"),
            "{key}: {}",
            r[0].message
        );
        let mut ok = xtal_board("12MHz", "18p", "18p");
        add_property(&mut ok, "Y1", key, "Crystal 12MHz 12pF 20ppm");
        assert!(
            fired(&crystal_findings(&ok, &CrystalCards::new())).is_empty(),
            "{key}"
        );
    }
    // A link or a footprint property that happens to contain a number is not read.
    let mut link = xtal_board("12MHz", "47p", "47p");
    add_property(&mut link, "Y1", "Datasheet", "https://x.example/12pF.pdf");
    assert!(fired(&crystal_findings(&link, &CrystalCards::new())).is_empty());
    // Nothing states the CL: abstain, and say why in plain words.
    let r = crystal_findings(&xtal_board("12MHz", "47p", "47p"), &CrystalCards::new());
    assert!(fired(&r).is_empty(), "{r:?}");
    assert!(
        r.iter()
            .any(|f| f.severity == SiSeverity::Info
                && f.message.contains("load capacitance unknown")),
        "{r:?}"
    );
}

/// A crystal's card states its CL and C0/C1; the check judges against the card
/// and cites it, using the card's C1 for the ppm instead of the assumed one.
#[test]
fn the_crystal_cards_cl_and_motional_capacitance_drive_the_judgement() {
    let b = xtal_board("12MHz", "27p", "27p");
    let mut cards = CrystalCards::new();
    cards.insert(
        "Y1".into(),
        CrystalFacts {
            cl_pf: Some((10.0, "card 'abracon_abm8_272_t3': datasheet table".into())),
            c0_pf: Some(3.0),
            c1_ff: Some(10.0),
            driver_caps: None,
        },
    );
    let r = crystal_findings(&b, &cards);
    let f = fired(&r);
    assert_eq!(f.len(), 1, "{r:?}");
    for says in [
        "CL = 10.0 pF",
        "card 'abracon_abm8_272_t3'",
        "C1 10.0 fF card",
        "C0 3.0 pF card",
    ] {
        assert!(f[0].message.contains(says), "{says:?} in {}", f[0].message);
    }
    // The same board with no card and no stated CL is not judged.
    assert!(fired(&crystal_findings(&b, &CrystalCards::new())).is_empty());
}

/// A driver whose card prints an on-chip load range: with no external caps the
/// crystal's CL is judged against series(on-chip) + stray; with external caps the
/// on-chip capacitance is added in parallel to each; a tunable bank with no
/// printed range is never judged.
#[test]
fn a_drivers_on_chip_load_caps_are_part_of_the_board_cl() {
    let lfxo = IntegratedCaps::Range {
        lo_pf: 4.0,
        hi_pf: 24.5,
        basis: "card 'efm32pg22' LFXO: Table 4.12".into(),
    };
    let facts = |caps: IntegratedCaps| {
        CrystalCards::from([(
            "Y1".to_string(),
            CrystalFacts {
                driver_caps: Some(caps),
                ..Default::default()
            },
        )])
    };
    // No external caps: band 4/4 + 3 = 5 pF to 24.5/24.5 + 5 = 17.25 pF.
    // A crystal of 12 pF or less is reachable by the bank alone: silent (the
    // bank reaches half its 24.5 pF maximum, 12.25 pF).
    for value in ["32.768kHz 9pF", "32.768kHz 12pF"] {
        let mut inside = xtal_on_driver("EFM32PG22C200F512IM40");
        set_value(&mut inside, "Y1", value);
        let r = crystal_findings(&inside, &facts(lfxo.clone()));
        assert!(fired(&r).is_empty(), "{value}: {r:?}");
        assert!(r[0].message.contains("- ok"), "{}", r[0].message);
    }
    // A 12.5 pF crystal is past that reach (EFM32PG22 Table 4.12 note 2: above
    // 12 pF needs external load capacitors): it fires and says what to do.
    let mut over = xtal_on_driver("EFM32PG22C200F512IM40");
    set_value(&mut over, "Y1", "32.768kHz 12.5pF");
    let r = crystal_findings(&over, &facts(lfxo.clone()));
    assert_eq!(fired(&r).len(), 1, "{r:?}");
    for says in [
        "no external caps",
        "reaches at most 12.25 pF",
        "needs external load capacitors",
    ] {
        assert!(r[0].message.contains(says), "{says:?} in {}", r[0].message);
    }
    // With external caps the bank is no longer alone: the same crystal is not
    // flagged for its reach.
    let mut padded = xtal_board("32.768kHz 12.5pF", "10p", "10p");
    set_value(&mut padded, "Y1", "32.768kHz 12.5pF");
    let r = crystal_findings(&padded, &facts(lfxo.clone()));
    assert!(
        r.iter().all(|f| !f.message.contains("reaches at most")),
        "{r:?}"
    );
    // A CL the on-chip bank cannot reach is a finding, naming the card.
    let mut beyond = xtal_on_driver("EFM32PG22C200F512IM40");
    set_value(&mut beyond, "Y1", "16MHz 30pF");
    let r = crystal_findings(&beyond, &facts(lfxo.clone()));
    assert_eq!(fired(&r).len(), 1, "{r:?}");
    assert!(
        r[0].message.contains("no external caps"),
        "{}",
        r[0].message
    );
    assert!(r[0].message.contains("Table 4.12"), "{}", r[0].message);
    // External caps add in parallel with the on-chip ones: 27p + 4..24.5 per pin.
    let mut with_ext = xtal_board("12MHz 12pF", "27p", "27p");
    set_value(&mut with_ext, "Y1", "12MHz 12pF");
    let r = crystal_findings(&with_ext, &facts(lfxo));
    assert!(r[0].message.contains("on-chip per pin"), "{}", r[0].message);
    // Tunable with no printed range: no judgement, with or without caps.
    let tunable = IntegratedCaps::Unspecified {
        basis: "card 'efm32pg22' HFXO".into(),
    };
    for b in [
        xtal_board("12MHz 12pF", "27p", "27p"),
        xtal_on_driver("EFM32PG22C200F512IM40"),
    ] {
        let r = crystal_findings(&b, &facts(tunable.clone()));
        assert!(fired(&r).is_empty(), "{r:?}");
    }
}

#[test]
fn dnp_crystal_is_skipped() {
    let b = pcb(r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3225-4Pin"
          (at 10 10) (layer "F.Cu") (attr smd dnp)
          (property "Reference" "Y1") (property "Value" "16MHz")
          (pad "1" smd rect (at -1 0) (net 1 "XIN"))
          (pad "3" smd rect (at 1 0) (net 2 "XOUT")))"#);
    let r = run_board(&b, check_crystal_load_cap);
    assert_eq!(r.findings.len(), 0, "DNP crystal must be entirely skipped");
}

// ---------------------------------------------------------------------------
// I2C rise-time check.
// ---------------------------------------------------------------------------

/// An I2C net with one pull-up resistor to +3V3 per `pulls` value (R1 at x 5,
/// R2 at x 7, ...) and `devices` sensor pins.
fn i2c_body(net: &str, pulls: &[&str], devices: usize) -> String {
    let mut body = format!(r#"(net 1 "{net}") (net 2 "+3V3")"#);
    for (i, pull_val) in pulls.iter().enumerate() {
        let _ = write!(
            body,
            r#"
        (footprint "Resistor_SMD:R_0402" (at {} 5) (layer "F.Cu")
          (property "Reference" "R{}") (property "Value" "{pull_val}")
          (pad "1" smd rect (at 0 0) (net 1 "{net}"))
          (pad "2" smd rect (at 1 0) (net 2 "+3V3")))"#,
            5 + 2 * i,
            i + 1
        );
    }
    for i in 0..devices {
        let _ = write!(
            body,
            r#"(footprint "Package_SO:SOIC-8" (at {} 8) (layer "F.Cu")
              (property "Reference" "U{}") (property "Value" "SENSOR")
              (pad "1" smd rect (at 0 0) (net 1 "{net}")))"#,
            10 + i,
            i + 1
        );
    }
    body
}

/// An I2C net with a pull-up resistor to +3V3 and `devices` sensor pins.
fn i2c_board(net: &str, pull_val: &str, devices: usize) -> ExtractedBoard {
    pcb(&i2c_body(net, &[pull_val], devices))
}

#[test]
fn i2c_strong_pull_low_device_count_is_ok() {
    // 2.2k pull, 3 devices (~30 pF): t_r ~ 56 ns. Far under 1000 ns: info.
    let b = i2c_board("SDA", "2.2k", 3);
    let r = run_board(&b, i2c_unrouted);
    assert_eq!(r.finding_count(), 0, "strong pull / few devices must be ok");
    assert!(r
        .of_check(SiCheck::I2cRiseTime)
        .any(|f| f.severity == SiSeverity::Info));
}

#[test]
fn i2c_weak_pull_heavy_bus_fires() {
    // 10k pull, 20 devices (~200 pF): t_r ~ 0.8473*10000*200e-3 = 1695 ns,
    // over standard-mode 1000 ns: fires.
    let b = i2c_board("SDA", "10k", 20);
    let r = run_board(&b, i2c_unrouted);
    assert_eq!(r.finding_count(), 1, "weak pull on a heavy bus must fire");
}

#[test]
fn i2c_no_pullup_is_not_our_finding() {
    // No pull-up at all: that's netlint's presence check, not the rise-time
    // sufficiency check. We stay silent.
    let b = pcb(r#"(net 1 "SDA")
        (footprint "Package_SO:SOIC-8" (at 10 8) (layer "F.Cu")
          (property "Reference" "U1") (property "Value" "S")
          (pad "1" smd rect (at 0 0) (net 1 "SDA")))
        (footprint "Package_SO:SOIC-8" (at 12 8) (layer "F.Cu")
          (property "Reference" "U2") (property "Value" "S")
          (pad "1" smd rect (at 0 0) (net 1 "SDA")))"#);
    let r = run_board(&b, i2c_unrouted);
    assert_eq!(
        r.findings.len(),
        0,
        "no pull-up -> rise-time check is silent"
    );
}

#[test]
fn i2c_dual_pullups_combine_in_parallel_not_min() {
    // A bus terminated at BOTH ends: two 10k pull-ups to +3V3 (one per end),
    // 15 sensor pins (~150 pF). Two pull-ups sit in PARALLEL, so the effective
    // R is 5k (not 10k): t_r ~ 0.8473*5000*150e-3 = 635 ns, comfortably under the
    // 1000 ns standard-mode limit -> silent. Taking the SMALLEST single
    // resistor (10k) computes ~1271 ns and fires a false-positive finding on a
    // bus that is actually in spec. min-of-parallel-resistors over-reports t_r.
    let b = pcb(&i2c_body("SDA", &["10k", "10k"], 15));
    let r = run_board(&b, i2c_unrouted);
    assert_eq!(
        r.finding_count(),
        0,
        "two 10k pull-ups are 5k in parallel: the bus is in spec, no false positive"
    );
}

/// Full `kicad_pcb` text for a 10k-pulled SDA bus with `devices` sensor pins and a
/// single routed track `track_mm` long, so the SAME text drives both extraction
/// and geometry parsing.
fn i2c_routed_text(devices: usize, track_mm: f64) -> String {
    let mut body = i2c_body("SDA", &["10k"], devices);
    let _ = write!(
        body,
        r#"(segment (start 0 0) (end {track_mm} 0) (width 0.2) (layer "F.Cu") (net 1))"#
    );
    pcb_text(&body)
}

#[test]
fn i2c_long_routing_pushes_a_marginal_bus_over() {
    // 10k pull, 10 devices = 100 pF: t_r ~ 0.8473*10000*100e-3 = 847 ns, inside
    // the 1000 ns standard-mode limit on pin capacitance ALONE. An 800 mm bus run
    // (an I2C link across a backplane, exactly where rise time bites) adds
    // 800*0.038 = 30.4 pF even at the LOW end of the plausible trace-capacitance
    // range, taking C to 130 pF and t_r to ~1105 ns: over the limit on the
    // lenient bound, which is what firing requires. Passing None for the trace
    // length silently rates this in-spec.
    let text = i2c_routed_text(10, 800.0);
    let r = run_text(&text, i2c_routed);
    assert_eq!(
        r.finding_count(),
        1,
        "800 mm of trace capacitance must push a marginal bus over the limit"
    );
    assert!(
        r.of_check(SiCheck::I2cRiseTime)
            .any(|f| f.message.contains("800 mm routing")),
        "the finding must name the routed length it counted"
    );
    // The device pins alone are inside the limit, so this verdict rests on the
    // assumed trace capacitance. It must say so, and must not claim top severity.
    let f = r.of_check(SiCheck::I2cRiseTime).next().unwrap();
    assert_eq!(
        f.severity,
        SiSeverity::Medium,
        "a trace-dependent shortfall is capped at Medium: {}",
        f.message
    );
    assert!(
        f.message
            .contains("depends on the ASSUMED trace capacitance"),
        "must disclose what the verdict rests on: {}",
        f.message
    );
}

#[test]
fn i2c_short_routing_leaves_the_same_bus_silent() {
    // The identical bus routed compactly (10 mm) is 101.5 pF / ~860 ns even at the
    // high end of the range: in spec. Counting trace copper must not turn every
    // marginal bus into a finding.
    let text = i2c_routed_text(10, 10.0);
    let r = run_text(&text, i2c_routed);
    assert_eq!(
        r.finding_count(),
        0,
        "a short-routed bus of the same devices stays in spec"
    );
}

#[test]
fn a_declared_stackup_computes_the_trace_capacitance_instead_of_assuming_it() {
    // The honest answer to "your low bound is not a bound" is to stop guessing
    // where the board says enough to compute. With a stackup and a track width,
    // C' = sqrt(Er_eff)/(c0*Z0) collapses the range to one number, and the note
    // says it was computed rather than assumed.
    let body = r#"
      (setup (stackup
        (layer "F.Cu" (type "copper") (thickness 0.035))
        (layer "dielectric 1" (type "core") (thickness 1.51) (epsilon_r 4.5))
        (layer "B.Cu" (type "copper") (thickness 0.035))
      ))
      (net 1 "SDA") (net 2 "+3V3")
      (footprint "Resistor_SMD:R_0402" (at 5 5) (layer "F.Cu")
        (property "Reference" "R1") (property "Value" "2.2k")
        (pad "1" smd rect (at 0 0) (net 1 "SDA"))
        (pad "2" smd rect (at 1 0) (net 2 "+3V3")))
      (footprint "Package_SO:SOIC-8" (at 10 8) (layer "F.Cu")
        (property "Reference" "U1") (property "Value" "SENSOR")
        (pad "1" smd rect (at 0 0) (net 1 "SDA")))
      (segment (start 0 0) (end 60 0) (width 0.25) (layer "F.Cu") (net 1))
      (zone (net 3) (net_name "GND") (layer "B.Cu")
        (filled_polygon (layer "B.Cu")
          (pts (xy -5 -5) (xy 70 -5) (xy 70 20) (xy -5 20))))
    "#;
    let text = format!("(kicad_pcb (version 20240101) (net 0 \"\") (net 3 \"GND\") {body})");
    let r = run_text(&text, i2c_routed);
    let f = r.of_check(SiCheck::I2cRiseTime).next().expect("a note");
    assert!(
        f.message.contains("computed from the board stackup"),
        "a declared stackup must be used, not assumed around: {}",
        f.message
    );
    assert!(
        !f.message.contains("ASSUMED"),
        "and must not also claim to have assumed: {}",
        f.message
    );
    // A single computed figure, so the reported range collapses to one number:
    // a 0.25 mm trace on 1.51 mm FR4 works out at 0.044 pF/mm, near the low end
    // of the assumed range, which is exactly the sort of thing worth not guessing.
    assert!(
        f.message.contains("0.044 pF/mm"),
        "the computed figure must be reported: {}",
        f.message
    );
}

// ---------------------------------------------------------------------------
// Antenna keepout check.
// ---------------------------------------------------------------------------

/// Full `kicad_pcb` text for a WROOM module at (50,50,0) plus a caller-supplied
/// intruder body, so the SAME text drives both extraction and geometry parsing.
fn wroom_text(intruder_body: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20240101) (net 0 "") (net 1 "GND") (net 2 "ANT")
        (footprint "OLIMEX_Cases-FP:ESP-WROOM-32_MODULE"
          (at 50 50 0) (layer "F.Cu")
          (property "Reference" "U3") (property "Value" "ESP32-WROOM-32E-N4")
          (pad "1" smd rect (at 0 5) (net 2 "ANT")))
        {intruder_body})"#
    )
}

fn run_keepout(text: &str) -> SiReport {
    run_text(text, check_antenna_keepout)
}

#[test]
fn antenna_keepout_clear_is_info() {
    // No copper anywhere near the keepout. The keepout (local y -27.75..-12.75,
    // x -9..9) maps to board y 22.25..37.25, x 41..59 at (50,50,0).
    let r = run_keepout(&wroom_text(""));
    assert_eq!(r.finding_count(), 0, "clear keepout must not fire");
    assert!(r
        .of_check(SiCheck::AntennaKeepout)
        .any(|f| f.severity == SiSeverity::Info));
}

#[test]
fn antenna_keepout_ground_pour_inside_fires_high() {
    // A GND zone whose fill polygon lands inside the keepout band (board y ~30).
    let intruder = r#"(zone (net 1) (net_name "GND") (layers "F.Cu")
        (filled_polygon (layer "F.Cu")
          (pts (xy 44 24) (xy 56 24) (xy 56 35) (xy 44 35))))"#;
    let r = run_keepout(&wroom_text(intruder));
    assert_eq!(r.finding_count(), 1, "ground pour in keepout must fire");
    assert_eq!(r.findings_only().next().unwrap().severity, SiSeverity::High);
}

#[test]
fn antenna_keepout_applies_to_pcb_antenna_variants_only() {
    // The same GND pour inside the keepout: fires for a PCB-antenna module,
    // and stays silent for the u.FL (`-<n>U[E]`) variants that have no trace
    // antenna.
    let pour = r#"(zone (net 1) (net_name "GND") (layers "F.Cu")
        (filled_polygon (layer "F.Cu")
          (pts (xy 44 24) (xy 56 24) (xy 56 35) (xy 44 35))))"#;
    let cases = [
        ("ESP32-WROOM-32E-N4", 1),
        ("ESP32-S3-WROOM-1-N8", 1),
        ("ESP32-WROOM-32U", 0),
        ("ESP32-S3-WROOM-1U-N8", 0),
        ("ESP32-WROOM-32UE-N4", 0),
    ];
    for (value, findings) in cases {
        let text = wroom_text(pour).replace("ESP32-WROOM-32E-N4", value);
        assert_eq!(run_keepout(&text).finding_count(), findings, "{value}");
    }
}

#[test]
fn antenna_keepout_track_outside_is_silent() {
    // A signal track far from the keepout (board y 70, well below the module).
    let intruder = r#"(segment (start 40 70) (end 60 70) (width 0.2) (layer "F.Cu") (net 1))"#;
    let r = run_keepout(&wroom_text(intruder));
    assert_eq!(
        r.finding_count(),
        0,
        "copper outside keepout must be silent"
    );
}

/// A module at (50,50,0) whose antenna-side end pad sits at local y `pad_y`,
/// 0.8 mm tall (so the pad's outer edge is at `pad_y - 0.4`), value `value`.
fn module_with_end_pad(value: &str, pad_y: f64, intruder_body: &str) -> String {
    format!(
        r#"(kicad_pcb (version 20240101) (net 0 "") (net 1 "GND") (net 2 "ANT")
        (footprint "OLIMEX_Cases-FP:ESP-WROOM-32_MODULE"
          (at 50 50 0) (layer "F.Cu")
          (property "Reference" "U3") (property "Value" "{value}")
          (pad "1" smd rect (at -8.6 {pad_y}) (size 1.8 0.8) (layers "F.Cu" "F.Mask"))
          (pad "2" smd rect (at 0 12) (size 1.8 0.8) (layers "F.Cu" "F.Mask") (net 2 "ANT")))
        {intruder_body})"#
    )
}

#[test]
fn antenna_keepout_ignores_the_modules_own_pad_row_environment() {
    // The legal control. A module whose end-pad row is at local y -5.31 (outer
    // edge -5.71): ground stitching vias and a tracks just inside that row (local
    // y -5.6), and a ground pour whose clearance contour pokes 0.5 mm past the
    // edge, are the module's own ground-pad surroundings, not copper in the
    // antenna region (the Olimex ESP32-EVB / PoE boards, whose antenna region is
    // otherwise empty). Board y = 50 + local y.
    let own = r#"(via (at 45 44.4) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1))
        (segment (start 44 44.4) (end 46 44.4) (width 0.5) (layer "F.Cu") (net 1))
        (zone (net 1) (net_name "GND") (layers "F.Cu")
          (filled_polygon (layer "F.Cu")
            (pts (xy 41 44.2) (xy 59 44.2) (xy 59 60) (xy 41 60))))"#;
    let r = run_keepout(&module_with_end_pad("ESP32-WROOM-32E-N4", -5.31, own));
    assert_eq!(
        r.finding_count(),
        0,
        "the module's own pad-row environment must stay silent: {:?}",
        r.findings
    );
}

#[test]
fn antenna_keepout_still_fires_on_ground_copper_in_the_antenna_region() {
    // The real fault, on the same module: a ground track 6 mm in front of the
    // pad row (local y -12) and a pour reaching 5 mm past it both sit squarely
    // in the 15 mm band.
    let track = r#"(segment (start 44 38) (end 56 38) (width 0.5) (layer "F.Cu") (net 1))"#;
    let pour = r#"(zone (net 1) (net_name "GND") (layers "F.Cu")
        (filled_polygon (layer "F.Cu")
          (pts (xy 41 39) (xy 59 39) (xy 59 60) (xy 41 60))))"#;
    for body in [track, pour] {
        let r = run_keepout(&module_with_end_pad("ESP32-WROOM-32E-N4", -5.31, body));
        assert_eq!(r.finding_count(), 1, "ground copper in the band must fire");
        assert_eq!(r.findings_only().next().unwrap().severity, SiSeverity::High);
    }
}

#[test]
fn antenna_keepout_starts_at_this_footprints_own_pad_edge() {
    // The same ground via 7.5 mm in front of the footprint origin (board y 42.5).
    // On a WROOM-32-style footprint (end pad at local y -5.31) that is open
    // antenna region and fires. On a longer, WROVER-style footprint whose end pad
    // sits at local y -8.26 (outer edge -8.66) the same point is among the
    // module's own ground pads and is not the antenna region at all.
    let via = r#"(via (at 50 42.5) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net 1))"#;
    let wroom = module_with_end_pad("ESP32-WROOM-32E-N4", -5.31, via);
    let wrover = module_with_end_pad("ESP32-WROOM-32E-N4", -8.26, via);
    assert_eq!(run_keepout(&wroom).finding_count(), 1);
    assert_eq!(run_keepout(&wrover).finding_count(), 0);
}

#[test]
fn antenna_keepout_has_no_geometry_for_the_wroom_02() {
    // ESP-WROOM-02's footprint is symmetric about its origin and a different
    // size; the WROOM-32 rectangle is not its keepout. The same pour that fires
    // for a WROOM-32 must not be judged against a WROOM-02.
    let pour = r#"(zone (net 1) (net_name "GND") (layers "F.Cu")
        (filled_polygon (layer "F.Cu")
          (pts (xy 44 24) (xy 56 24) (xy 56 35) (xy 44 35))))"#;
    let w32 = wroom_text(pour);
    assert_eq!(run_keepout(&w32).finding_count(), 1);
    let w02 = w32.replace("ESP32-WROOM-32E-N4", "ESP-WROOM-02");
    assert_eq!(
        run_keepout(&w02).findings.len(),
        0,
        "no keepout geometry is known for the WROOM-02"
    );
}

#[test]
fn antenna_unknown_module_no_keepout() {
    // A part with no keepout entry -> the check never considers it.
    let text = r#"(kicad_pcb (version 1) (net 0 "") (net 1 "GND")
           (footprint "Package_QFN:QFN-48" (at 50 50 0) (layer "F.Cu")
             (property "Reference" "U1") (property "Value" "STM32")
             (pad "1" smd rect (at 0 0) (net 1 "GND")))
           (segment (start 50 50) (end 51 50) (width 0.2) (layer "F.Cu") (net 1)))"#;
    let r = run_keepout(text);
    assert_eq!(r.findings.len(), 0, "unknown module -> no keepout, no note");
}

// ---------------------------------------------------------------------------
// USB diff-pair check.
// ---------------------------------------------------------------------------

/// The USB diff-pair check over a pair routed `plus_len` / `minus_len` long.
fn usb_report(plus_len: f64, minus_len: f64, plus_w: f64, minus_w: f64) -> SiReport {
    let body = format!(
        r#"(net 1 "USB_DP") (net 2 "USB_DM")
        (segment (start 0 0) (end {plus_len} 0) (width {plus_w}) (layer "F.Cu") (net 1))
        (segment (start 0 1) (end {minus_len} 1) (width {minus_w}) (layer "F.Cu") (net 2))"#
    );
    run_text(&pcb_text(&body), check_usb_diff_pair)
}

#[test]
fn usb_matched_pair_is_info() {
    let r = usb_report(20.0, 20.3, 0.2, 0.2);
    assert_eq!(r.finding_count(), 0, "0.3 mm skew is within FS budget");
    let f = r.of_check(SiCheck::UsbDiffPair).next().unwrap();
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(f.message.contains("skew"));
}

#[test]
fn usb_gross_skew_fires() {
    // 20 mm vs 40 mm: 20 mm skew, over even the lenient 15 mm FS budget.
    let r = usb_report(20.0, 40.0, 0.2, 0.2);
    assert_eq!(r.finding_count(), 1, "gross skew must fire");
}

#[test]
fn usb_polarity_classifier_rejects_non_usb() {
    // `usb_polarity` now takes an uppercased leaf and returns the stem + polarity
    // (None when not a USB data line). VDD, LED-, DDR must not classify.
    assert!(super::usb_polarity("VDD").is_none());
    assert!(super::usb_polarity("LED").is_none());
    // genuine forms classify with the right polarity:
    assert_eq!(super::usb_polarity("USB_DP").map(|(_, p)| p), Some('+'));
    assert_eq!(super::usb_polarity("D+").map(|(_, p)| p), Some('+'));
    assert_eq!(super::usb_polarity("UD-").map(|(_, p)| p), Some('-'));
    // DN (minus) is now recognised so it can pair with a DP leg.
    assert_eq!(super::usb_polarity("USB_DN").map(|(_, p)| p), Some('-'));
    // The stem is what must match between the two legs (the prefix before the
    // polarity token): USB_DP and USB_DN share stem "USB_".
    assert_eq!(
        super::usb_polarity("USB_DP").map(|(s, _)| s),
        Some("USB_".to_string())
    );
    assert_eq!(
        super::usb_polarity("USB_DN").map(|(s, _)| s),
        Some("USB_".to_string())
    );
}

// ---------------------------------------------------------------------------
// Controlled-impedance check.
// ---------------------------------------------------------------------------

use super::impedance::{
    differential_microstrip_z, microstrip_z0, read_stackup, stripline_z0, StackupSource,
};

#[test]
fn microstrip_z0_matches_reference_calculator() {
    // The published IPC-2141 reference case (verified against the chemandy /
    // mycalctools online calculators): W=0.3 mm, H=0.2 mm, T=0.035 mm, Er=4.3
    // -> Z0 = 53.5 ohm. Our closed form must match the calculator to within a
    // few percent (it is the same formula, so it matches to < 0.1%).
    let z = microstrip_z0(0.3, 0.2, 0.035, 4.3).unwrap();
    assert!(
        (z - 53.5).abs() < 0.5,
        "IPC-2141 microstrip 0.3/0.2 = {z} ohm, want ~53.5"
    );
    // A second reference point: W=0.25 mm same stack -> 59.3 ohm (calculator).
    let z2 = microstrip_z0(0.25, 0.2, 0.035, 4.3).unwrap();
    assert!(
        (z2 - 59.3).abs() < 0.6,
        "IPC-2141 microstrip 0.25/0.2 = {z2} ohm, want ~59.3"
    );
    // A near-50-ohm wide trace on 1.6 mm 2-layer FR4 (W=2.9 mm, H=1.51, Er=4.5)
    // -> ~48 ohm, the classic "wide trace on a thick board is ~50 ohm".
    let z3 = microstrip_z0(2.9, 1.51, 0.035, 4.5).unwrap();
    assert!(
        (z3 - 48.0).abs() < 2.0,
        "wide-trace 50-ohm-ish case = {z3} ohm"
    );
}

#[test]
fn stripline_z0_hand_value() {
    // IPC-2141 stripline: Z0 = (60/sqrt(Er)) * ln(4H / (0.67*pi*(0.8W+T))).
    // W=0.15, H=0.5, T=0.035, Er=4.3:
    //   0.8*0.15+0.035 = 0.155; 0.67*pi*0.155 = 0.3262; 4*0.5/0.3262 = 6.131;
    //   ln = 1.8134; 60/sqrt(4.3)=28.94; Z0 = 52.5 ohm.
    let z = stripline_z0(0.15, 0.5, 0.035, 4.3).unwrap();
    assert!((z - 52.5).abs() < 1.0, "stripline = {z} ohm, want ~52.5");
}

#[test]
fn differential_microstrip_matches_hand_value() {
    // National Semiconductor form: Zdiff = 2*Z0*(1 - 0.48*exp(-0.96*S/H)).
    // For a 90-ohm USB geometry W=0.3, S=0.2, H=0.2, T=0.035, Er=4.3:
    //   Z0(0.3,0.2) = 53.52 ohm; S/H = 1.0; exp(-0.96) = 0.3829;
    //   factor = 1 - 0.48*0.3829 = 0.8162; Zdiff = 2*53.52*0.8162 = 87.4 ohm.
    let z0 = microstrip_z0(0.3, 0.2, 0.035, 4.3).unwrap();
    let zd = differential_microstrip_z(z0, 0.2, 0.2).unwrap();
    assert!(
        (zd - 87.4).abs() < 1.0,
        "USB diff = {zd} ohm, want ~87.4 (within 90 +-15%)"
    );
    // Tighter spacing lowers Zdiff (more coupling); wider spacing raises it.
    let tight = differential_microstrip_z(z0, 0.1, 0.2).unwrap();
    let wide = differential_microstrip_z(z0, 0.4, 0.2).unwrap();
    assert!(
        tight < zd && zd < wide,
        "coupling monotonicity: {tight} < {zd} < {wide}"
    );
}

/// A USB pair routed on F.Cu over a known stackup, with a controllable trace
/// width and spacing (two parallel segments `gap` apart, edge-to-edge).
/// `controlled` sets `dielectric_constraints yes` (the board declares
/// impedance-control intent, so a deviation can be a finding).
fn impedance_usb_text_intent(w: f64, gap: f64, diel: f64, er: f64, controlled: bool) -> String {
    impedance_usb_text_plane(w, gap, diel, er, controlled, true)
}

/// As [`impedance_usb_text_intent`], with `plane` choosing whether a solid GND
/// pour on B.Cu sits under the whole run (a verified reference plane) or the
/// reference layer carries no pour at all (the plane cannot be verified).
fn impedance_usb_text_plane(
    w: f64,
    gap: f64,
    diel: f64,
    er: f64,
    controlled: bool,
    plane: bool,
) -> String {
    // Two parallel horizontal runs; centre-to-centre = gap + w (so edge-to-edge
    // spacing = gap). Stackup: F.Cu 0.035, dielectric `diel` Er `er`, B.Cu.
    let y_minus = gap + w; // centreline of D- relative to D+ at y=0.
    let dc = if controlled { "yes" } else { "no" };
    let pour = if plane {
        r#"(zone (net 3) (net_name "GND") (layer "B.Cu")
             (filled_polygon (pts (xy -5 -5) (xy 25 -5) (xy 25 5) (xy -5 5))))"#
    } else {
        ""
    };
    format!(
        r#"(kicad_pcb (version 20240101)
        (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
        (setup (stackup
          (layer "F.Cu" (type "copper") (thickness 0.035))
          (layer "dielectric 1" (type "core") (thickness {diel}) (material "FR4") (epsilon_r {er}))
          (layer "B.Cu" (type "copper") (thickness 0.035))
          (dielectric_constraints {dc})))
        (net 0 "") (net 1 "USB_DP") (net 2 "USB_DM") (net 3 "GND")
        (segment (start 0 0) (end 20 0) (width {w}) (layer "F.Cu") (net 1))
        (segment (start 0 {y_minus}) (end 20 {y_minus}) (width {w}) (layer "F.Cu") (net 2))
        {pour})"#
    )
}

/// The same board, declaring controlled-impedance intent (the common path for
/// the in-band / fire tests).
fn impedance_usb_text(w: f64, gap: f64, diel: f64, er: f64) -> String {
    impedance_usb_text_intent(w, gap, diel, er, true)
}

#[test]
fn read_stackup_from_board() {
    let text = impedance_usb_text(0.3, 0.2, 0.2, 4.3);
    let doc = forge_sexpr::parse(&text).unwrap();
    let s = read_stackup(doc.root().unwrap()).expect("stackup present");
    assert_eq!(s.source, StackupSource::Board);
    assert!((s.h_microstrip_mm - 0.2).abs() < 1e-9);
    assert!((s.t_cu_mm - 0.035).abs() < 1e-9);
    assert!((s.er - 4.3).abs() < 1e-9);
}

#[test]
fn controlled_impedance_in_band_usb_is_info() {
    // W=0.3, edge-to-edge gap 0.2, diel 0.2, Er 4.3 -> Zdiff ~ 87.4 ohm, within
    // 90 ohm +-15%: an info note, never a finding.
    let text = impedance_usb_text(0.3, 0.2, 0.2, 4.3);
    let r = run_text(&text, super::impedance::check_controlled_impedance);
    assert_eq!(r.finding_count(), 0, "in-band USB diff must not fire");
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("a note");
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(
        f.message.contains("ok"),
        "in-band must read ok: {}",
        f.message
    );
}

#[test]
fn controlled_impedance_out_of_band_usb_fires() {
    // Very narrow traces, wide spacing on a thick dielectric drive Zdiff far
    // above 90 ohm: a real finding (the link is impedance-wrong) against a real
    // file stackup. W=0.1, gap 0.5, diel 0.5 -> Zdiff well over 90+15%.
    let text = impedance_usb_text(0.1, 0.5, 0.5, 4.3);
    let r = run_text(&text, super::impedance::check_controlled_impedance);
    assert_eq!(
        r.finding_count(),
        1,
        "grossly out-of-band USB diff must fire"
    );
    let f = r.findings_only().next().unwrap();
    assert_eq!(f.check, SiCheck::ControlledImpedance);
    assert!(
        f.message.contains("deviation"),
        "finding cites the deviation: {}",
        f.message
    );
}

#[test]
fn controlled_impedance_unverified_reference_plane_abstains() {
    // The SAME grossly out-of-band geometry as the test above, but the reference
    // layer carries no pour at all, so the plane under the pair cannot be
    // verified. The closed form's H is then the height to copper nobody found,
    // and the number it gives is not an estimate of anything: the check must
    // say so as an info note and never raise a finding. (The tiny_tapeout demo's
    // USB pair on a stack whose reference layer is not determined.)
    let text = impedance_usb_text_plane(0.1, 0.5, 0.5, 4.3, true, false);
    let r = run_text(&text, super::impedance::check_controlled_impedance);
    assert_eq!(
        r.finding_count(),
        0,
        "an unverified reference plane must not fire: {:?}",
        r.findings
    );
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("an abstention note");
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(
        f.message.contains("no judgement") && f.message.contains("unverified"),
        "the note must say it did not judge, and why: {}",
        f.message
    );
}

/// A board with one routed single-ended net `name`, `w` mm wide on F.Cu over a
/// real stackup that declares impedance control.
fn single_ended_text(name: &str, w: f64) -> String {
    format!(
        r#"(kicad_pcb (version 20240101)
        (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
        (setup (stackup
          (layer "F.Cu" (type "copper") (thickness 0.035))
          (layer "dielectric 1" (type "core") (thickness 0.21) (material "FR4") (epsilon_r 4.6))
          (layer "B.Cu" (type "copper") (thickness 0.035))
          (dielectric_constraints yes)))
        (net 0 "") (net 1 "{name}")
        (segment (start 0 0) (end 20 0) (width {w}) (layer "F.Cu") (net 1)))"#
    )
}

#[test]
fn a_dc_bias_line_with_an_antenna_token_is_not_a_50_ohm_feed() {
    // `ANT_BIAS` powers an active antenna (a bias tee's DC leg). It carries the
    // `ANT` token but is routed as a power trace, so judging its 0.2 mm width
    // against 50 ohm fabricated a finding on the HackRF One's bias control line.
    for name in [
        "ANT_BIAS",
        "!ANT_BIAS",
        "/frontend/!ANT_BIAS",
        "ANT_PWR",
        "RF_VCC",
    ] {
        let r = run_text(
            &single_ended_text(name, 0.203),
            super::impedance::check_controlled_impedance,
        );
        assert_eq!(
            r.of_check(SiCheck::ControlledImpedance).count(),
            0,
            "{name} is a DC line, never judged as a 50 ohm feed"
        );
    }
}

#[test]
fn a_mistuned_antenna_feed_still_fires_as_a_50_ohm_line() {
    // The real-fault partner: the genuine feed `ANT` (and `RF_OUT`) at the same
    // 0.203 mm width over 0.21 mm of dielectric is ~66 ohm, +31% from 50: a
    // finding against the file's own stackup.
    for name in ["ANT", "RF_OUT", "/frontend/ANT1"] {
        let r = run_text(
            &single_ended_text(name, 0.203),
            super::impedance::check_controlled_impedance,
        );
        assert_eq!(
            r.finding_count(),
            1,
            "{name} is the 50 ohm feed and must still be judged: {:?}",
            r.findings
        );
    }
}

#[test]
fn controlled_impedance_no_stackup_is_info_never_finding() {
    // The SAME out-of-band geometry but with NO stackup block: the estimate uses
    // the default-assumption stackup and MUST be info only, never a finding. This
    // is the zero-false-positive guard: an unknown stackup cannot manufacture a
    // controlled-impedance finding. (The RP2040 minimal board class.)
    let text = r#"(kicad_pcb (version 20240101)
        (net 0 "") (net 1 "USB_DP") (net 2 "USB_DM")
        (segment (start 0 0) (end 20 0) (width 0.1) (layer "F.Cu") (net 1))
        (segment (start 0 0.6) (end 20 0.6) (width 0.1) (layer "F.Cu") (net 2)))"#;
    let r = run_text(text, super::impedance::check_controlled_impedance);
    assert_eq!(r.finding_count(), 0, "no stackup -> never a finding");
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("an info estimate");
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(
        f.message.contains("ASSUMED") && f.message.contains("info"),
        "must flag the assumed stackup: {}",
        f.message
    );
}

// ---------------------------------------------------------------------------
// Pad-dedup in I2C bus-capacitance and pull-up counting.
// ---------------------------------------------------------------------------

/// A component with one SDA pad listed `copies` times on net 1 (an IPC-356
/// both-sided through-hole access record lists the same pad more than once).
fn double_listed_board(reference: &str, value: &str, copies: usize) -> ExtractedBoard {
    let pins = (0..copies)
        .map(|_| crate::Pin {
            function: "SDA".into(),
            ..crate::testutil::pin("5", Some(1))
        })
        .collect();
    crate::testutil::board(
        &[(1, "SDA")],
        vec![crate::testutil::part(reference, value, "", "", pins)],
    )
}

#[test]
fn bus_capacitance_dedups_a_double_listed_pad() {
    // Counting raw net_members double-counted a both-sided through-hole pad's
    // pin capacitance (2 devices instead of 1), inflating the I2C rise time enough
    // to fire a spurious fast-mode finding. Dedup by (ref, pad number).
    let board = double_listed_board("U1", "SENSOR", 2);
    let c = super::bus_capacitance_pf(&board, 1, None, None);
    assert_eq!(
        c.devices, 1,
        "a doubly-listed pad must count as one device, not two"
    );
}

/// A controlled-impedance USB pair over a B.Cu pour, with the pour's fill given
/// as `fill` polygons. `(-5,-5)..(25,5)` covers the whole 20 mm run.
fn impedance_usb_over_plane(fills: &str) -> String {
    impedance_usb_over_plane_with(fills, "")
}

/// As above, plus `extras` emitted at top level (outside the zone), which is
/// where vias belong: `via_antipads` reads them from the board root.
fn impedance_usb_over_plane_with(fills: &str, extras: &str) -> String {
    // Same in-band geometry as `impedance_usb_text(0.3, 0.2, 0.2, 4.3)`: W 0.3,
    // edge-to-edge gap 0.2 (centres 0 and 0.5), 0.2 mm FR4 core, Zdiff ~ 87 ohm
    // against the 90 ohm USB target.
    format!(
        r#"(kicad_pcb (version 20240101)
        (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
        (setup (stackup
          (layer "F.Cu" (type "copper") (thickness 0.035))
          (layer "dielectric 1" (type "core") (thickness 0.2) (material "FR4") (epsilon_r 4.3))
          (layer "B.Cu" (type "copper") (thickness 0.035))
          (dielectric_constraints yes)))
        (net 0 "") (net 1 "USB_DP") (net 2 "USB_DM") (net 3 "GND")
        (segment (start 0 0) (end 20 0) (width 0.3) (layer "F.Cu") (net 1))
        (segment (start 0 0.5) (end 20 0.5) (width 0.3) (layer "F.Cu") (net 2))
        (zone (net 3) (net_name "GND") (layer "B.Cu") {fills})
        {extras})"#
    )
}

fn rect_fill(x0: f64, x1: f64) -> String {
    format!("(filled_polygon (pts (xy {x0} -5) (xy {x1} -5) (xy {x1} 5) (xy {x0} 5)))",)
}

#[test]
fn a_pair_over_a_solid_reference_plane_reports_its_zdiff_silently() {
    // The two-sided partner of the test below. Same geometry, same declared
    // controlled-impedance intent, but the B.Cu pour is unbroken under the whole
    // run: the microstrip formula's H is the height to copper that is really
    // there, so the estimate stands and the check says nothing about the plane.
    let text = impedance_usb_over_plane(&rect_fill(-5.0, 25.0));
    let r = run_text(&text, super::impedance::check_controlled_impedance);
    assert_eq!(
        r.finding_count(),
        0,
        "a solid reference plane must not fire"
    );
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("an impedance note");
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(
        f.message.contains("Zdiff") && f.message.contains("ok"),
        "the in-band Zdiff must still be reported: {}",
        f.message
    );
    assert!(
        !f.message.contains("reference"),
        "a verified plane is discharged silently, not narrated: {}",
        f.message
    );
}

#[test]
fn a_pair_crossing_a_plane_void_names_the_span_instead_of_a_zdiff() {
    // The formulas' H is the height to a REFERENCE PLANE, and the module used to
    // assume one existed under the trace because the stackup declared a second
    // copper layer. Here the B.Cu pour is split, leaving 6..14 mm with no copper
    // under either leg. The return current has to detour, the real impedance is
    // nothing like the closed form's, so the check must abstain and say where.
    let void = format!("{} {}", rect_fill(-5.0, 6.0), rect_fill(14.0, 25.0));
    let text = impedance_usb_over_plane(&void);
    let r = run_text(&text, super::impedance::check_controlled_impedance);
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("an abstention");
    assert!(
        f.message.contains("reference missing under trace"),
        "the abstention must say what is missing: {}",
        f.message
    );
    assert!(
        !f.message.contains("Zdiff ~"),
        "no confident Zdiff may be printed without a reference plane: {}",
        f.message
    );
    // The void is named in board coordinates and brackets the real gap: the pour
    // is split over x = 6..14, and the half-millimetre sampling pitch locates the
    // reference-less run to within one pitch of each edge. It is reported for a
    // single leg (both lose their reference here), because a void's extent along
    // the leg it undermines is what that leg's impedance responds to.
    assert!(
        f.message.contains("(6.00, 0.00)") && f.message.contains("(13.50, 0.00)"),
        "the void must bracket the 6..14 mm gap in board coordinates: {}",
        f.message
    );
    assert!(
        f.message.contains("~8.00 mm across"),
        "and its width must be stated: {}",
        f.message
    );
    assert!(
        f.message.contains("B.Cu"),
        "the span must name the layer that should carry the plane: {}",
        f.message
    );
    // And it must say what would turn this into a confident answer.
    assert!(
        f.message
            .contains("needs reference-plane copper along the pair's routed length"),
        "the abstention must name what would unlock a confident answer: {}",
        f.message
    );
    // The board declares controlled impedance, so this is a real defect class,
    // not an informational aside.
    assert_eq!(f.severity, SiSeverity::Medium);
}

#[test]
fn a_via_antipad_in_the_plane_is_not_a_missing_reference() {
    // A plane's anti-pads are a DESIGNED hole: the copper is cleared so the via
    // can pass through, and a differential pair's segments terminate at its
    // layer-transition vias, so endpoint samples land in one systematically. A
    // pinhole is not a return-path detour.
    //
    // Here the pour is solid except for a square 0.45 mm anti-pad bitten out
    // around x = 10, written the way a fill with a void in it is: the outline
    // walks in to the hole and back out. The pair drops through vias at (10, 0)
    // and (10, 0.5).
    let holed_fill = r"(filled_polygon (pts
        (xy -5 -5) (xy 25 -5) (xy 25 5) (xy 10.3 5)
        (xy 10.3 -0.3) (xy 9.7 -0.3) (xy 9.7 5) (xy -5 5)))";
    let vias = r#"(via (at 10 0) (size 0.45) (drill 0.25) (layers "F.Cu" "B.Cu") (net 1))
        (via (at 10 0.5) (size 0.45) (drill 0.25) (layers "F.Cu" "B.Cu") (net 2))"#;
    let text = impedance_usb_over_plane_with(holed_fill, vias);
    let r = run_text(&text, super::impedance::check_controlled_impedance);
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("an impedance note");
    assert!(
        !f.message.contains("reference missing"),
        "an anti-pad around the pair's own via is not a missing reference plane: {}",
        f.message
    );
    assert_eq!(r.finding_count(), 0, "an anti-pad must never fire");
}

// ---------------------------------------------------------------------------
// Which standard a differential pair is: the suffix alone does not say.
// ---------------------------------------------------------------------------

/// The class [`impedance::pair_class`] gives a pair named `plus` / `minus`,
/// with `extra` board text (footprints) beside the two nets.
fn pair_class_of(plus: &str, minus: &str, extra: &str) -> Option<impedance::ImpedanceClass> {
    let b = pcb(&format!(r#"(net 1 "{plus}") (net 2 "{minus}") {extra}"#));
    let id = |name: &str| b.nets.iter().find(|n| n.name == name).unwrap().id;
    impedance::pair_class(&b, id(plus), id(minus))
}

/// A connector footprint with one pad on each of nets 1 and 2.
fn connector_on_pair(footprint: &str, value: &str, plus: &str, minus: &str) -> String {
    format!(
        r#"(footprint "{footprint}" (at 0 0) (layer "F.Cu")
             (property "Reference" "J1") (property "Value" "{value}")
             (pad "1" smd rect (at 0 0) (net 1 "{plus}"))
             (pad "2" smd rect (at 1 0) (net 2 "{minus}")))"#
    )
}

#[test]
fn a_dp_dn_suffix_names_a_usb_pair_only_with_usb_evidence() {
    use impedance::ImpedanceClass::{DisplayDiff, EthernetDiff, UsbDiff};
    // Faulty reads, one per way the suffix used to be taken for USB.
    assert_eq!(
        pair_class_of("EDP_TX1_DP", "EDP_TX1_DN", ""),
        Some(DisplayDiff),
        "an eDP lane is a 100 ohm display lane"
    );
    assert_eq!(
        pair_class_of("/Display/HDMI_D0_DP", "/Display/HDMI_D0_DN", ""),
        Some(DisplayDiff)
    );
    assert_eq!(
        pair_class_of("LVDS0_DP", "LVDS0_DN", ""),
        Some(DisplayDiff),
        "a lane number after the token still names the standard"
    );
    assert_eq!(
        pair_class_of("MDI0_DP", "MDI0_DN", ""),
        Some(EthernetDiff),
        "an Ethernet token outranks the suffix"
    );
    assert_eq!(
        pair_class_of("LPCUD_DP", "LPCUD_DN", ""),
        None,
        "a suffix alone names no standard"
    );
    // Legal controls: the same suffix with USB evidence stays USB.
    assert_eq!(pair_class_of("USB_DP", "USB_DM", ""), Some(UsbDiff));
    assert_eq!(pair_class_of("USB3_3_DP", "USB3_3_DN", ""), Some(UsbDiff));
    assert_eq!(pair_class_of("/D+", "/D-", ""), Some(UsbDiff), "bare D+/D-");
    assert_eq!(pair_class_of("DP", "DN", ""), Some(UsbDiff), "bare DP/DN");
    assert_eq!(
        pair_class_of("/mcu/usb/power/DP", "/mcu/usb/power/DM", ""),
        Some(UsbDiff),
        "the sheet path says USB"
    );
    // A USB connector on the nets makes an oddly named pair USB.
    let jack = connector_on_pair("Connector_USB:USB_C_Receptacle", "USB_C", "PD+", "PD-");
    assert_eq!(pair_class_of("PD+", "PD-", &jack), Some(UsbDiff));
    // A non-USB connector on the same names does not.
    let header = connector_on_pair("Connector_PinHeader:PinHeader_1x02", "Conn", "PD+", "PD-");
    assert_eq!(pair_class_of("PD+", "PD-", &header), None);
}

/// A controlled-impedance board routed to Zdiff ~ 108 ohm (W 0.2 mm, gap
/// 0.2 mm over 0.2 mm FR4): +20% from a 90 ohm USB target, +8% from 100 ohm.
fn pair_text(plus: &str, minus: &str) -> String {
    impedance_usb_text(0.2, 0.2, 0.2, 4.3)
        .replace("USB_DP", plus)
        .replace("USB_DM", minus)
}

#[test]
fn an_edp_lane_is_judged_at_100_ohm_not_90_and_never_called_usb() {
    let r = run_text(
        &pair_text("EDP_TX1_DP", "EDP_TX1_DN"),
        impedance::check_controlled_impedance,
    );
    assert_eq!(r.finding_count(), 0, "108 ohm is inside 100 ohm +-15%");
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("a note");
    assert!(f.message.contains("target 100 ohm"), "{}", f.message);
    assert!(f.message.contains("ok"), "{}", f.message);
    assert!(!f.message.contains("USB"), "{}", f.message);
    // No USB skew claim either.
    let usb = run_text(&pair_text("EDP_TX1_DP", "EDP_TX1_DN"), check_usb_diff_pair);
    assert_eq!(usb.of_check(SiCheck::UsbDiffPair).count(), 0);
}

#[test]
fn the_same_geometry_on_a_usb_pair_still_fires_against_90_ohm() {
    let r = run_text(
        &pair_text("USB_DP", "USB_DM"),
        impedance::check_controlled_impedance,
    );
    assert_eq!(r.finding_count(), 1, "+20% from 90 ohm is out of band");
    let f = r.of_check(SiCheck::ControlledImpedance).next().unwrap();
    assert!(f.message.contains("90 ohm"), "{}", f.message);
}

#[test]
fn a_pair_of_unidentified_standard_gets_no_target() {
    let r = run_text(
        &pair_text("LPCUD_DP", "LPCUD_DN"),
        impedance::check_controlled_impedance,
    );
    assert_eq!(r.finding_count(), 0, "no target, so nothing to miss");
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("a note");
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(f.message.contains("unidentified standard"), "{}", f.message);
    assert!(!f.message.contains("USB differential"), "{}", f.message);
    assert!(!f.message.contains("target 90"), "{}", f.message);
}

#[test]
fn a_pair_of_unidentified_standard_still_names_a_void_under_it() {
    // The plane void is a fact about the copper, whatever the standard: dropping
    // the pair's standard must not drop the void it crosses.
    let void = format!("{} {}", rect_fill(-5.0, 6.0), rect_fill(14.0, 25.0));
    let text = impedance_usb_over_plane(&void)
        .replace("USB_DP", "LPCUD_DP")
        .replace("USB_DM", "LPCUD_DN");
    let r = run_text(&text, impedance::check_controlled_impedance);
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("a note");
    assert!(f.message.contains("unidentified standard"), "{}", f.message);
    assert!(
        f.message.contains("reference missing under trace")
            && f.message.contains("~8.00 mm across"),
        "{}",
        f.message
    );
    assert_eq!(f.severity, SiSeverity::Info, "no standard, no defect claim");
    // The same copper on a pair that says USB keeps its Medium finding.
    let usb = run_text(
        &impedance_usb_over_plane(&void),
        impedance::check_controlled_impedance,
    );
    let u = usb.of_check(SiCheck::ControlledImpedance).next().unwrap();
    assert_eq!(u.severity, SiSeverity::Medium);
}
