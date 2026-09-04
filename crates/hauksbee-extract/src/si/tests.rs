//! Unit tests for the SI checks: pure-physics hand values, plus synthetic
//! minimal boards exercising fire / no-fire per check.

use super::*;
use crate::ExtractedBoard;

fn pcb(body: &str) -> ExtractedBoard {
    let text = format!("(kicad_pcb (version 20240101) (net 0 \"\") {body})");
    ExtractedBoard::from_kicad_pcb(&text).unwrap()
}

fn root_of(body: &str) -> forge_sexpr::Document {
    let text = format!("(kicad_pcb (version 20240101) (net 0 \"\") {body})");
    forge_sexpr::parse(&text).unwrap()
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
        (semi - PI * r).abs() < 1e-6,
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
        (major - 1.5 * PI * r).abs() < 1e-4,
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
    // ABM8-272 specs 18 pF; two 33 pF caps -> 16.5 + 4 = 20.5 pF, deviation 2.5
    // pF < 8 pF tolerance: ok, info only.
    let b = xtal_board("ABM8-272-T3", "33p", "33p");
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 0, "within tolerance must not fire");
    assert!(r
        .of_check(SiCheck::CrystalLoadCap)
        .any(|f| f.severity == SiSeverity::Info));
}

#[test]
fn crystal_known_cl_far_off_fires() {
    // ABM8-272 specs 18 pF; two 4.7 pF caps -> 2.35 + 4 = 6.35 pF, deviation
    // 11.65 pF > 8 pF: fires (under-capacitanced -> runs fast / may not start).
    let b = xtal_board("ABM8-272-T3", "4p7", "4p7");
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 1, "far-off CL must fire");
    assert_eq!(
        r.of_check(SiCheck::CrystalLoadCap).next().unwrap().check,
        SiCheck::CrystalLoadCap
    );
}

#[test]
fn crystal_unknown_cl_is_info_only() {
    // value is just the frequency, CL not derivable: never a finding.
    let b = xtal_board("12MHz", "18p", "18p");
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 0, "unknown CL must never fire");
    let f = r.of_check(SiCheck::CrystalLoadCap).next().unwrap();
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(f.message.contains("CL spec unknown"));
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
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 1, "no load caps must fire");
    assert_eq!(
        r.findings_only().next().unwrap().severity,
        SiSeverity::Medium
    );
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
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 0, "RTC integrates caps; must be silent");
}

#[test]
fn dnp_crystal_is_skipped() {
    let b = pcb(r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3225-4Pin"
          (at 10 10) (layer "F.Cu") (attr smd dnp)
          (property "Reference" "Y1") (property "Value" "16MHz")
          (pad "1" smd rect (at -1 0) (net 1 "XIN"))
          (pad "3" smd rect (at 1 0) (net 2 "XOUT")))"#);
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.findings.len(), 0, "DNP crystal must be entirely skipped");
}

// ---------------------------------------------------------------------------
// I2C rise-time check.
// ---------------------------------------------------------------------------

/// An I2C net with a pull-up resistor to +3V3 and `devices` sensor pins.
fn i2c_board(net: &str, pull_val: &str, devices: usize) -> ExtractedBoard {
    let mut body = format!(
        r#"(net 1 "{net}") (net 2 "+3V3")
        (footprint "Resistor_SMD:R_0402" (at 5 5) (layer "F.Cu")
          (property "Reference" "R1") (property "Value" "{pull_val}")
          (pad "1" smd rect (at 0 0) (net 1 "{net}"))
          (pad "2" smd rect (at 1 0) (net 2 "+3V3")))"#
    );
    for i in 0..devices {
        body.push_str(&format!(
            r#"(footprint "Package_SO:SOIC-8" (at {} 8) (layer "F.Cu")
              (property "Reference" "U{}") (property "Value" "SENSOR")
              (pad "1" smd rect (at 0 0) (net 1 "{net}")))"#,
            10 + i,
            i + 1
        ));
    }
    pcb(&body)
}

#[test]
fn i2c_strong_pull_low_device_count_is_ok() {
    // 2.2k pull, 3 devices (~30 pF): t_r ~ 56 ns. Far under 1000 ns: info.
    let b = i2c_board("SDA", "2.2k", 3);
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, None, &mut r);
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
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, None, &mut r);
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
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, None, &mut r);
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
    let mut body = String::from(
        r#"(net 1 "SDA") (net 2 "+3V3")
        (footprint "Resistor_SMD:R_0402" (at 5 5) (layer "F.Cu")
          (property "Reference" "R1") (property "Value" "10k")
          (pad "1" smd rect (at 0 0) (net 1 "SDA"))
          (pad "2" smd rect (at 1 0) (net 2 "+3V3")))
        (footprint "Resistor_SMD:R_0402" (at 7 5) (layer "F.Cu")
          (property "Reference" "R2") (property "Value" "10k")
          (pad "1" smd rect (at 0 0) (net 1 "SDA"))
          (pad "2" smd rect (at 1 0) (net 2 "+3V3")))"#,
    );
    for i in 0..15 {
        body.push_str(&format!(
            r#"(footprint "Package_SO:SOIC-8" (at {} 8) (layer "F.Cu")
              (property "Reference" "U{}") (property "Value" "SENSOR")
              (pad "1" smd rect (at 0 0) (net 1 "SDA")))"#,
            10 + i,
            i + 1
        ));
    }
    let b = pcb(&body);
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, None, &mut r);
    assert_eq!(
        r.finding_count(),
        0,
        "two 10k pull-ups are 5k in parallel: the bus is in spec, no false positive"
    );
}

/// Full kicad_pcb text for a 10k-pulled SDA bus with `devices` sensor pins and a
/// single routed track `track_mm` long, so the SAME text drives both extraction
/// and geometry parsing.
fn i2c_routed_text(devices: usize, track_mm: f64) -> String {
    let mut body = String::from(
        r#"(net 1 "SDA") (net 2 "+3V3")
        (footprint "Resistor_SMD:R_0402" (at 5 5) (layer "F.Cu")
          (property "Reference" "R1") (property "Value" "10k")
          (pad "1" smd rect (at 0 0) (net 1 "SDA"))
          (pad "2" smd rect (at 1 0) (net 2 "+3V3")))"#,
    );
    for i in 0..devices {
        body.push_str(&format!(
            r#"(footprint "Package_SO:SOIC-8" (at {} 8) (layer "F.Cu")
              (property "Reference" "U{}") (property "Value" "SENSOR")
              (pad "1" smd rect (at 0 0) (net 1 "SDA")))"#,
            10 + i,
            i + 1
        ));
    }
    body.push_str(&format!(
        r#"(segment (start 0 0) (end {track_mm} 0) (width 0.2) (layer "F.Cu") (net 1))"#
    ));
    format!("(kicad_pcb (version 20240101) (net 0 \"\") {body})")
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
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, Some(doc.root().unwrap()), &mut r);
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
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, Some(doc.root().unwrap()), &mut r);
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
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, Some(doc.root().unwrap()), &mut r);
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

/// Full kicad_pcb text for a WROOM module at (50,50,0) plus a caller-supplied
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
    let b = ExtractedBoard::from_kicad_pcb(text).unwrap();
    let doc = forge_sexpr::parse(text).unwrap();
    let mut r = SiReport::default();
    check_antenna_keepout(&b, doc.root().unwrap(), &mut r);
    r
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

fn usb_board(
    plus_len: f64,
    minus_len: f64,
    plus_w: f64,
    minus_w: f64,
) -> (ExtractedBoard, forge_sexpr::Document) {
    let body = format!(
        r#"(net 1 "USB_DP") (net 2 "USB_DM")
        (segment (start 0 0) (end {plus_len} 0) (width {plus_w}) (layer "F.Cu") (net 1))
        (segment (start 0 1) (end {minus_len} 1) (width {minus_w}) (layer "F.Cu") (net 2))"#
    );
    (pcb(&body), root_of(&body))
}

#[test]
fn usb_matched_pair_is_info() {
    let (b, doc) = usb_board(20.0, 20.3, 0.2, 0.2);
    let mut r = SiReport::default();
    check_usb_diff_pair(&b, doc.root().unwrap(), &mut r);
    assert_eq!(r.finding_count(), 0, "0.3 mm skew is within FS budget");
    let f = r.of_check(SiCheck::UsbDiffPair).next().unwrap();
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(f.message.contains("skew"));
}

#[test]
fn usb_gross_skew_fires() {
    // 20 mm vs 40 mm: 20 mm skew, over even the lenient 15 mm FS budget.
    let (b, doc) = usb_board(20.0, 40.0, 0.2, 0.2);
    let mut r = SiReport::default();
    check_usb_diff_pair(&b, doc.root().unwrap(), &mut r);
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
    // Two parallel horizontal runs; centre-to-centre = gap + w (so edge-to-edge
    // spacing = gap). Stackup: F.Cu 0.035, dielectric `diel` Er `er`, B.Cu.
    let y_minus = gap + w; // centreline of D- relative to D+ at y=0.
    let dc = if controlled { "yes" } else { "no" };
    format!(
        r#"(kicad_pcb (version 20240101)
        (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
        (setup (stackup
          (layer "F.Cu" (type "copper") (thickness 0.035))
          (layer "dielectric 1" (type "core") (thickness {diel}) (material "FR4") (epsilon_r {er}))
          (layer "B.Cu" (type "copper") (thickness 0.035))
          (dielectric_constraints {dc})))
        (net 0 "") (net 1 "USB_DP") (net 2 "USB_DM")
        (segment (start 0 0) (end 20 0) (width {w}) (layer "F.Cu") (net 1))
        (segment (start 0 {y_minus}) (end 20 {y_minus}) (width {w}) (layer "F.Cu") (net 2)))"#
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
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    super::impedance::check_controlled_impedance(&b, doc.root().unwrap(), &mut r);
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
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    super::impedance::check_controlled_impedance(&b, doc.root().unwrap(), &mut r);
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
fn controlled_impedance_no_stackup_is_info_never_finding() {
    // The SAME out-of-band geometry but with NO stackup block: the estimate uses
    // the default-assumption stackup and MUST be info only, never a finding. This
    // is the zero-false-positive guard: an unknown stackup cannot manufacture a
    // controlled-impedance finding. (The RP2040 minimal board class.)
    let text = r#"(kicad_pcb (version 20240101)
        (net 0 "") (net 1 "USB_DP") (net 2 "USB_DM")
        (segment (start 0 0) (end 20 0) (width 0.1) (layer "F.Cu") (net 1))
        (segment (start 0 0.6) (end 20 0.6) (width 0.1) (layer "F.Cu") (net 2)))"#;
    let b = ExtractedBoard::from_kicad_pcb(text).unwrap();
    let doc = forge_sexpr::parse(text).unwrap();
    let mut r = SiReport::default();
    super::impedance::check_controlled_impedance(&b, doc.root().unwrap(), &mut r);
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
            number: "5".into(),
            net: Some(1),
            function: "SDA".into(),
            kind: String::new(),
            position: None,
        })
        .collect();
    ExtractedBoard {
        name: "b".into(),
        nets: vec![crate::Net {
            id: 1,
            name: "SDA".into(),
        }],
        components: vec![crate::Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: String::new(),
            footprint: String::new(),
            position: None,
            layer: String::new(),
            properties: Vec::new(),
            dnp: false,
            pins,
        }],
    }
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

#[test]
fn si_rail_voltage_rejects_signal_named_rails() {
    // The loose 3V3/1V8 `contains` fallbacks in si.rs rail_voltage (a
    // duplicate of netlint's) had no signal-role guard, so a `3V3_EN` enable net
    // read as a 3.3V rail, miscounting a resistor tapping it as an I2C pull-up
    // and suppressing a genuine MissingI2cPullup finding.
    assert_eq!(super::rail_voltage("3V3_EN"), None);
    assert_eq!(super::rail_voltage("1V8_PG"), None);
    assert_eq!(super::rail_voltage("3V3_SEL"), None);
    // Genuine rails still resolve.
    assert_eq!(super::rail_voltage("3V3"), Some(3.3));
    assert_eq!(super::rail_voltage("+1V8"), Some(1.8));
    assert_eq!(super::rail_voltage("MCU_3V3"), Some(3.3));
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
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    super::impedance::check_controlled_impedance(&b, doc.root().unwrap(), &mut r);
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
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    super::impedance::check_controlled_impedance(&b, doc.root().unwrap(), &mut r);
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
    // The Watchy lesson, in unit form. A plane's anti-pads are a DESIGNED hole:
    // the copper is cleared so the via can pass through. A differential pair's
    // segments terminate at its layer-transition vias, so endpoint samples land
    // in an anti-pad systematically. On Watchy that produced "reference missing
    // under trace" on a board whose In2.Cu plane is in fact solid under the pair:
    // all seven uncovered samples were within 0.36 mm of a via centre and four
    // were exactly on one. A pinhole is not a return-path detour.
    //
    // Here the pour is solid except for a 0.45 mm anti-pad punched out around the
    // via the pair drops through at x = 10.
    // A pour with a square anti-pad bitten out of it around x = 10: the outline
    // walks in to the hole and back out, which is how a fill with a void in it is
    // written. The pair drops through vias at (10, 0) and (10, 0.5).
    let holed_fill = r#"(filled_polygon (pts
        (xy -5 -5) (xy 25 -5) (xy 25 5) (xy 10.3 5)
        (xy 10.3 -0.3) (xy 9.7 -0.3) (xy 9.7 5) (xy -5 5)))"#;
    let vias = r#"(via (at 10 0) (size 0.45) (drill 0.25) (layers "F.Cu" "B.Cu") (net 1))
        (via (at 10 0.5) (size 0.45) (drill 0.25) (layers "F.Cu" "B.Cu") (net 2))"#;
    let text = impedance_usb_over_plane_with(holed_fill, vias);
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    super::impedance::check_controlled_impedance(&b, doc.root().unwrap(), &mut r);
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
