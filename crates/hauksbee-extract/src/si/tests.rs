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
fn trace_capacitance_per_mm_matches_transmission_line_physics() {
    // The constant is the whole trace term, so it is pinned against the closed
    // form rather than only against tests that use it: C' = sqrt(Er_eff)/(c0*Z0).
    // A units slip here (pF/inch or pF/cm written as pF/mm) inflates every bus by
    // an order of magnitude while leaving every other test self-consistent.
    const C0_MM_PER_S: f64 = 2.998e11; // mm/s
    let c_per_mm = |er_eff: f64, z0: f64| 1e12 * er_eff.sqrt() / (C0_MM_PER_S * z0);

    // FR4 Er_eff ~ 3: a 50 ohm line is ~0.116 pF/mm, a 100 ohm line ~0.057.
    let fifty = c_per_mm(3.0, 50.0);
    assert!(
        (fifty - 0.116).abs() < 0.005,
        "50 ohm microstrip is ~0.116 pF/mm, got {fifty}"
    );
    // The widest, closest-coupled realistic case bounds the constant from above.
    let worst = c_per_mm(3.2, 40.0);
    assert!(
        (worst - 0.149).abs() < 0.005,
        "40 ohm worst case is ~0.149 pF/mm, got {worst}"
    );
    // The reported range must bracket the real one: the low end at the
    // high-impedance (100 ohm) figure that gates findings, the high end at the
    // worst realistic case. Neither may drift an order of magnitude.
    let hundred = c_per_mm(2.9, 100.0);
    assert!(
        (super::C_TRACE_PF_PER_MM_LOW - hundred).abs() < 0.005,
        "the firing bound {} must be the ~100 ohm figure {hundred}",
        super::C_TRACE_PF_PER_MM_LOW
    );
    assert!(
        (super::C_TRACE_PF_PER_MM_HIGH - worst).abs() < 0.005,
        "the reported ceiling {} must be the worst-case figure {worst}",
        super::C_TRACE_PF_PER_MM_HIGH
    );
    // And the 50 ohm nominal must fall inside the reported range, or the range
    // does not describe real routing at all.
    assert!(
        super::C_TRACE_PF_PER_MM_LOW < fifty && fifty < super::C_TRACE_PF_PER_MM_HIGH,
        "the 50 ohm nominal {fifty} must lie inside the reported range"
    );
}

#[test]
fn parse_helpers() {
    assert_eq!(super::parse_farads("15p"), Some(15e-12));
    assert_eq!(super::parse_farads("18pF"), Some(18e-12));
    assert_eq!(super::parse_farads("4p7"), Some(4.7e-12));
    assert_eq!(super::parse_farads("0.1uF"), Some(0.1e-6));
    assert_eq!(super::parse_farads("TBD"), None);
    // R33: a trailing dielectric / voltage / tolerance token (space- or
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
fn parse_helpers_handle_unicode_and_spice_multipliers() {
    // R24: both micro glyphs; the micro sign U+00B5 and the Greek small-letter
    // mu U+03BC, must parse as 1e-6 (libraries write "4.7µF" with either).
    assert_eq!(super::parse_farads("4.7\u{00b5}F"), Some(4.7e-6));
    assert_eq!(super::parse_farads("4.7\u{03bc}F"), Some(4.7e-6));
    assert_eq!(super::parse_farads("0.1\u{03bc}F"), Some(0.1e-6));
    // Both ohm glyphs, Greek capital omega U+03A9 and the ohm sign U+2126.
    assert_eq!(super::parse_ohms("10\u{03a9}"), Some(10.0));
    assert_eq!(super::parse_ohms("10\u{2126}"), Some(10.0));
    // SPICE-style MEG/GIG multipliers, matched before the single-letter scan.
    assert_eq!(super::parse_ohms("10MEG"), Some(1e7));
    assert_eq!(super::parse_ohms("2GIG"), Some(2e9));
    // The 4M7 single-letter decimal notation still means 4.7 MΩ (not MEG).
    assert_eq!(super::parse_ohms("4M7"), Some(4.7e6));
}

#[test]
fn parse_ohms_no_longer_drifts_from_the_canonical_parser() {
    // R25 (DRIFT-2): lowercase 'm' is MILLIohm, not mega, "2m2" is 2.2 mΩ, a
    // current-sense shunt marking. The hand-rolled parser uppercased first and
    // read it as 2.2 MΩ (a 1e9 error).
    assert_eq!(super::parse_ohms("2m2"), Some(0.0022));
    assert_eq!(super::parse_ohms("1m"), Some(0.001));
    // R25 (DRIFT-3): leading-R sub-ohm shunt marks parse (were None in si.rs).
    assert_eq!(super::parse_ohms("R47"), Some(0.47));
    assert_eq!(super::parse_ohms("R1"), Some(0.1));
    // R25 (DRIFT-4): an inline tolerance annotation must not reject the value.
    assert_eq!(super::parse_ohms("10k 1%"), Some(10_000.0));
    assert_eq!(super::parse_ohms("4.7k 1%"), Some(4700.0));
    // Uppercase 'M' is still mega (SPICE convention); the milli fix must not
    // regress this.
    assert_eq!(super::parse_ohms("4M7"), Some(4.7e6));
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
fn routed_length_resolves_name_only_nets() {
    // A KiCad-10 board that references the net by name on the segment - `(net
    // "USB_DP")` with no numeric id. arg_i64(0) is None on the string token, so
    // a numeric-only lookup counts zero length for net 1. The name must resolve
    // through the (net 1 "USB_DP") table.
    let doc = root_of(
        r#"(net 1 "USB_DP")
           (segment (start 0 0) (end 3 0) (width 0.2) (layer "F.Cu") (net "USB_DP"))
           (segment (start 3 0) (end 3 4) (width 0.2) (layer "F.Cu") (net "USB_DP"))"#,
    );
    let l = routed_length_mm(doc.root().unwrap(), 1);
    assert!(
        (l - 7.0).abs() < 1e-9,
        "name-only nets must resolve: got {l}"
    );
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

/// Two-sided assembly contract: the identical topology that is clean when the
/// caps are fitted (`crystal_known_cl_within_tolerance_is_info_not_finding`)
/// must fire missing-load-caps when the caps are DNP or identity-refused,
/// because an absent or untrusted cap loads nothing.
#[test]
fn dnp_or_refused_load_caps_do_not_satisfy_crystal_topology() {
    let refuse_caps = |b: &mut ExtractedBoard, f: &dyn Fn(&mut crate::Component)| {
        for c in b
            .components
            .iter_mut()
            .filter(|c| c.reference.starts_with('C'))
        {
            f(c);
        }
    };

    let mut dnp = xtal_board("ABM8-272-T3", "33p", "33p");
    refuse_caps(&mut dnp, &|c| c.dnp = true);
    let mut r = SiReport::default();
    check_crystal_load_cap(&dnp, &mut r);
    assert_eq!(r.finding_count(), 1, "DNP caps must read as missing caps");

    let mut refused = xtal_board("ABM8-272-T3", "33p", "33p");
    refuse_caps(&mut refused, &|c| {
        c.properties.push((
            crate::DUPLICATE_REFERENCE_CONFLICT_KEY.to_string(),
            "two records with different values".to_string(),
        ));
    });
    let mut r = SiReport::default();
    check_crystal_load_cap(&refused, &mut r);
    assert_eq!(
        r.finding_count(),
        1,
        "identity-refused caps must read as missing caps"
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
fn ceramic_resonator_no_caps_is_silent() {
    // A 3-terminal ceramic resonator (CSTCE / RESONATOR footprint) integrates
    // its load caps; no external caps is correct. (Arduino Uno Y2 topology.)
    let b = pcb(r#"(net 1 "XTAL1") (net 2 "XTAL2") (net 3 "GND")
        (footprint "Resonator:RESONATOR"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y2") (property "Value" "CSTCE16M0V53-R0 16MHZ")
          (pad "1" smd rect (at -1 0) (net 1 "XTAL1"))
          (pad "2" smd rect (at 0 0) (net 3 "GND"))
          (pad "3" smd rect (at 1 0) (net 2 "XTAL2")))"#);
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(
        r.findings.len(),
        0,
        "ceramic resonator must be entirely silent"
    );
}

#[test]
fn split_keyboard_mirror_prefix_caps_are_traced() {
    // The right half of a Corne carries `r`-prefixed mirror refs (rY1, rC1,
    // rC2). The type classifiers must see Y1/C1/C2 underneath, so the load caps
    // trace and the crystal is INFO (not a false "no caps" finding).
    let b = pcb(r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3225-4Pin" (at 10 10) (layer "F.Cu")
          (property "Reference" "rY1") (property "Value" "12MHz")
          (pad "1" smd rect (at -1 0) (net 1 "XIN"))
          (pad "2" smd rect (at -1 1) (net 3 "GND"))
          (pad "3" smd rect (at 1 0) (net 2 "XOUT"))
          (pad "4" smd rect (at 1 1) (net 3 "GND")))
        (footprint "Capacitor_SMD:C_0402" (at 8 10) (layer "F.Cu")
          (property "Reference" "rC1") (property "Value" "27p")
          (pad "1" smd rect (at 0 0) (net 1 "XIN"))
          (pad "2" smd rect (at 1 0) (net 3 "GND")))
        (footprint "Capacitor_SMD:C_0402" (at 12 10) (layer "F.Cu")
          (property "Reference" "rC2") (property "Value" "27p")
          (pad "1" smd rect (at 0 0) (net 2 "XOUT"))
          (pad "2" smd rect (at 1 0) (net 3 "GND")))"#);
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(
        r.finding_count(),
        0,
        "mirror-prefix caps must trace, no false finding"
    );
    assert!(r
        .of_check(SiCheck::CrystalLoadCap)
        .any(|f| f.severity == SiSeverity::Info && f.message.contains("17.5")));
}

#[test]
fn eagle_double_pad_capacitor_still_counts_as_two_terminal() {
    // The Eagle .brd extractor lists each pad once per signal contact, so a
    // 2-terminal cap can show four pin entries (pad 1 x2, pad 2 x2). The
    // distinct-pad count must still see two terminals so the load cap traces.
    let b = pcb(r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_2Pin" (at 10 10) (layer "F.Cu")
          (property "Reference" "Y2") (property "Value" "16MHz")
          (pad "1" smd rect (at -1 0) (net 1 "XIN"))
          (pad "3" smd rect (at 1 0) (net 2 "XOUT")))
        (footprint "Capacitor_SMD:C_0402" (at 8 10) (layer "F.Cu")
          (property "Reference" "C4") (property "Value" "22pF")
          (pad "1" smd rect (at 0 0) (net 1 "XIN"))
          (pad "1" smd rect (at 0 0) (net 1 "XIN"))
          (pad "2" smd rect (at 1 0) (net 3 "GND"))
          (pad "2" smd rect (at 1 0) (net 3 "GND")))
        (footprint "Capacitor_SMD:C_0402" (at 12 10) (layer "F.Cu")
          (property "Reference" "C2") (property "Value" "22pF")
          (pad "1" smd rect (at 0 0) (net 2 "XOUT"))
          (pad "1" smd rect (at 0 0) (net 2 "XOUT"))
          (pad "2" smd rect (at 1 0) (net 3 "GND"))
          (pad "2" smd rect (at 1 0) (net 3 "GND")))"#);
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(
        r.finding_count(),
        0,
        "double-pad caps must trace, no false 'no caps' finding"
    );
    assert!(r
        .of_check(SiCheck::CrystalLoadCap)
        .any(|f| f.severity == SiSeverity::Info));
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
    // the 1000 ns standard-mode limit on pin capacitance ALONE. A 500 mm bus run
    // (an I2C link across a backplane, exactly where rise time bites) adds
    // 500*0.057 = 28.5 pF even at the LOW end of the trace-capacitance range,
    // taking C to 128 pF and t_r to ~1089 ns: over the limit on the lenient
    // bound, which is what firing requires. Passing None for the trace length
    // silently rates this in-spec.
    let text = i2c_routed_text(10, 500.0);
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, Some(doc.root().unwrap()), &mut r);
    assert_eq!(
        r.finding_count(),
        1,
        "100 mm of trace capacitance must push a marginal bus over the limit"
    );
    assert!(
        r.of_check(SiCheck::I2cRiseTime)
            .any(|f| f.message.contains("500 mm routing")),
        "the finding must name the routed length it counted"
    );
}

#[test]
fn i2c_short_routing_leaves_the_same_bus_silent() {
    // The identical bus routed compactly (10 mm) is 101 pF / ~852 ns even at the
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
fn i2c_without_layout_says_routing_was_not_counted() {
    // No layout: the pin-count model is a floor, not an answer, and the note must
    // name the upload that would complete it rather than implying completeness.
    let b = i2c_board("SDA", "2.2k", 3);
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, None, &mut r);
    assert!(
        r.of_check(SiCheck::I2cRiseTime)
            .any(|f| f.message.contains("routing capacitance NOT counted")
                && f.message.contains(".kicad_pcb")),
        "a layout-less run must disclose the missing routing term and the upload"
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
fn antenna_keepout_ground_pour_fires_even_when_module_has_a_bonded_gnd_pad() {
    // R39: a real WROOM module bonds many pads to the board GND net. Building
    // `own_nets` from ALL of the antenna's pad nets and skipping any intrusion
    // on an own net puts the board GND in own_nets whenever a GND pad exists, so
    // a ground pour flooding the keepout is silently skipped, a false all-clear
    // on the exact detuning case. Only the NON-ground own nets are excluded, so a
    // ground pour still fires even though the module has a GND pad.
    let text = r#"(kicad_pcb (version 20240101) (net 0 "") (net 1 "GND") (net 2 "ANT")
        (footprint "OLIMEX_Cases-FP:ESP-WROOM-32_MODULE"
          (at 50 50 0) (layer "F.Cu")
          (property "Reference" "U3") (property "Value" "ESP32-WROOM-32E-N4")
          (pad "1" smd rect (at 0 5) (net 2 "ANT"))
          (pad "2" smd rect (at 0 6) (net 1 "GND")))
        (zone (net 1) (net_name "GND") (layers "F.Cu")
          (filled_polygon (layer "F.Cu")
            (pts (xy 44 24) (xy 56 24) (xy 56 35) (xy 44 35)))))"#;
    let r = run_keepout(text);
    assert_eq!(
        r.finding_count(),
        1,
        "a ground pour in the keepout must fire even though the module has a bonded GND pad"
    );
    assert_eq!(r.findings_only().next().unwrap().severity, SiSeverity::High);
}

#[test]
fn antenna_keepout_finding_kinds_are_sorted_deterministically() {
    // R41: the intrusion `kinds` were collected into a HashSet and formatted with
    // {:?}, so a multi-kind intrusion's message order varied run-to-run (a HashSet
    // Debug order is randomized per process), non-reproducible SI output. Sorting
    // (like the sibling `nets`) makes it byte-stable. A track + via + zone on the
    // same non-ground net inside the keepout must render as ["track", "via", "zone"].
    let text = r#"(kicad_pcb (version 20240101) (net 0 "") (net 1 "GND") (net 2 "ANT") (net 3 "SIG")
        (footprint "OLIMEX_Cases-FP:ESP-WROOM-32_MODULE"
          (at 50 50 0) (layer "F.Cu")
          (property "Reference" "U3") (property "Value" "ESP32-WROOM-32E-N4")
          (pad "1" smd rect (at 0 5) (net 2 "ANT")))
        (segment (start 45 30) (end 55 30) (net 3) (layer "F.Cu"))
        (via (at 50 30) (size 0.8) (drill 0.4) (layers "F.Cu" "B.Cu") (net 3))
        (zone (net 3) (net_name "SIG") (layers "F.Cu")
          (filled_polygon (layer "F.Cu")
            (pts (xy 44 24) (xy 56 24) (xy 56 35) (xy 44 35)))))"#;
    let r = run_keepout(text);
    let msg = &r
        .findings_only()
        .next()
        .expect("a keepout intrusion finding")
        .message;
    assert!(
        msg.contains("[\"track\", \"via\", \"zone\"]"),
        "intrusion kinds must be sorted for reproducible output, got: {msg}"
    );
}

#[test]
fn antenna_keepout_engulfing_pour_fires_high() {
    // A board-wide ground plane whose fill polygon covers the whole board. Every
    // fill vertex is OUTSIDE the small keepout rectangle (board x 41..59, y
    // 22.25..37.25), so vertex-sampling alone reported a false all-clear. The
    // pour still fully engulfs the antenna keepout - the exact bad-WiFi case the
    // check exists to catch. Detected by testing the keepout corners against the
    // fill polygon.
    let intruder = r#"(zone (net 1) (net_name "GND") (layers "F.Cu")
        (filled_polygon (layer "F.Cu")
          (pts (xy 0 0) (xy 100 0) (xy 100 100) (xy 0 100))))"#;
    let r = run_keepout(&wroom_text(intruder));
    assert_eq!(
        r.finding_count(),
        1,
        "a pour that engulfs the keepout must fire"
    );
    assert_eq!(r.findings_only().next().unwrap().severity, SiSeverity::High);
}

#[test]
fn antenna_keepout_nonconvex_engulfing_pour_fires_high() {
    // A REAL KiCad pour outline is deeply non-convex (it weaves around vias, pads
    // and thermal reliefs). This plane covers the whole antenna keepout (board x
    // 41..59, y 22.25..37.25) but carries a notch far from it (x 85..95, y 60..100),
    // giving the outline reflex vertices. A convex-only `point_in_poly`
    // winding test returns false for a point inside such a polygon the moment two
    // edges disagree in sign, silently missing the engulf and reporting a
    // false all-clear on exactly the copper-under-antenna geometry the check
    // exists to catch. The even-odd ray cast handles arbitrary polygons.
    let intruder = r#"(zone (net 1) (net_name "GND") (layers "F.Cu")
        (filled_polygon (layer "F.Cu")
          (pts (xy 0 0) (xy 100 0) (xy 100 100) (xy 95 100)
               (xy 95 60) (xy 85 60) (xy 85 100) (xy 0 100))))"#;
    let r = run_keepout(&wroom_text(intruder));
    assert_eq!(
        r.finding_count(),
        1,
        "a non-convex pour that engulfs the keepout must fire"
    );
    assert_eq!(r.findings_only().next().unwrap().severity, SiSeverity::High);
}

#[test]
fn antenna_keepout_name_only_net_pour_fires_high() {
    // KiCad-10 elements can reference their net by NAME only - `(net "GND")` with
    // no leading numeric id. arg_i64(0) returns None on the string token, so the
    // old code skipped the pour and reported a false all-clear. The name must
    // resolve through the (net 1 "GND") table before the keepout is judged.
    let intruder = r#"(zone (net "GND") (layers "F.Cu")
        (filled_polygon (layer "F.Cu")
          (pts (xy 44 24) (xy 56 24) (xy 56 35) (xy 44 35))))"#;
    let r = run_keepout(&wroom_text(intruder));
    assert_eq!(
        r.finding_count(),
        1,
        "a name-only-net pour in the keepout must still fire"
    );
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
fn usb_width_mismatch_is_info_note_not_a_finding() {
    // Matched length but different widths -> the width note is INFO, never a
    // finding: trace neck-down at pad entry is universal and benign (it fired
    // Low on all three ZSWatch DevKit revisions before this was demoted).
    let (b, doc) = usb_board(20.0, 20.1, 0.2, 0.3);
    let mut r = SiReport::default();
    check_usb_diff_pair(&b, doc.root().unwrap(), &mut r);
    assert_eq!(
        r.finding_count(),
        0,
        "a width mismatch alone must not be a finding"
    );
    let f = r.of_check(SiCheck::UsbDiffPair).next().unwrap();
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(f.message.contains("width mismatch"));
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

#[test]
fn usb_pair_key_scopes_by_sheet_and_stem() {
    // Two legs of the SAME logical pair (same sheet, same stem, opposite
    // polarity) must produce keys that differ only in polarity, so they pair.
    let (k_dp, p_dp) = super::usb_pair_key("/USB_DP").unwrap();
    let (k_dn, p_dn) = super::usb_pair_key("/USB_DN").unwrap();
    assert_eq!(k_dp, k_dn, "connector-side DP/DN share a scope key");
    assert_eq!((p_dp, p_dn), ('+', '-'));

    // The MCU-side legs on a different sheet must produce a DIFFERENT key, so the
    // matcher can never pair across the series ESD device.
    let (k_mcu_p, _) = super::usb_pair_key("/ESP32-C3-02/USB_D+").unwrap();
    let (k_mcu_m, _) = super::usb_pair_key("/ESP32-C3-02/USB_D-").unwrap();
    assert_eq!(k_mcu_p, k_mcu_m, "MCU-side D+/D- share a scope key");
    assert_ne!(
        k_dp, k_mcu_p,
        "connector-side and MCU-side legs (across the ESD array) must NOT share a key"
    );
    assert_ne!(
        k_dp, k_mcu_m,
        "the exact false-positive cross-pair (/USB_DP x /ESP32-C3-02/USB_D-) must never key-match"
    );

    // A non-USB net yields no key.
    assert!(super::usb_pair_key("/VBUS").is_none());
    assert!(super::usb_pair_key("GND").is_none());
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
fn microstrip_z0_declines_degenerate_geometry() {
    // A trace so wide the log argument falls to <= 1 (formula invalid): decline,
    // do not return a bogus negative impedance.
    assert!(microstrip_z0(50.0, 0.2, 0.035, 4.3).is_none());
    assert!(microstrip_z0(0.0, 0.2, 0.035, 4.3).is_none());
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
fn controlled_impedance_uncontrolled_board_is_info_even_out_of_band() {
    // The SAME grossly-out-of-band geometry but the board declares
    // `dielectric_constraints no` (it did NOT intend to control these nets, like
    // every full-speed USB keyboard in the corpus). Must be info, never a fire:
    // the designer chose not to control impedance, so a high reading is not a
    // defect. This is the corpus zero-false-positive gate in unit form.
    let text = impedance_usb_text_intent(0.1, 0.5, 0.5, 4.3, false);
    let b = ExtractedBoard::from_kicad_pcb(&text).unwrap();
    let doc = forge_sexpr::parse(&text).unwrap();
    let mut r = SiReport::default();
    super::impedance::check_controlled_impedance(&b, doc.root().unwrap(), &mut r);
    assert_eq!(r.finding_count(), 0, "uncontrolled board must never fire");
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("an info note");
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(
        f.message.contains("does not declare controlled impedance"),
        "must explain why it is info: {}",
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

#[test]
fn controlled_impedance_ethernet_pair_targets_100_ohm() {
    // An Ethernet-named pair (TRD0_P/TRD0_N) is judged against 100 ohm, not 90.
    let text = r#"(kicad_pcb (version 20240101)
        (setup (stackup
          (layer "F.Cu" (type "copper") (thickness 0.035))
          (layer "dielectric 1" (type "core") (thickness 0.2) (material "FR4") (epsilon_r 4.3))
          (layer "B.Cu" (type "copper") (thickness 0.035))))
        (net 0 "") (net 1 "TRD0_P") (net 2 "TRD0_N")
        (segment (start 0 0) (end 20 0) (width 0.25) (layer "F.Cu") (net 1))
        (segment (start 0 0.45) (end 20 0.45) (width 0.25) (layer "F.Cu") (net 2)))"#;
    let b = ExtractedBoard::from_kicad_pcb(text).unwrap();
    let doc = forge_sexpr::parse(text).unwrap();
    let mut r = SiReport::default();
    super::impedance::check_controlled_impedance(&b, doc.root().unwrap(), &mut r);
    let f = r
        .of_check(SiCheck::ControlledImpedance)
        .next()
        .expect("a note");
    assert!(
        f.message.contains("100 ohm") && f.message.contains("Ethernet"),
        "Ethernet pair must target 100 ohm: {}",
        f.message
    );
}

// ---------------------------------------------------------------------------
// R45: pad-dedup in I2C bus-capacitance and pull-up counting.
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
    // R45: counting raw net_members double-counted a both-sided through-hole pad's
    // pin capacitance (2 devices instead of 1), inflating the I2C rise time enough
    // to fire a spurious fast-mode finding. Dedup by (ref, pad number).
    let board = double_listed_board("U1", "SENSOR", 2);
    let c = super::bus_capacitance_pf(&board, 1, None);
    assert_eq!(
        c.devices, 1,
        "a doubly-listed pad must count as one device, not two"
    );
}

#[test]
fn fast_mode_name_is_whole_token_not_substring() {
    // R48: `contains("FM")`/`contains("FAST")` over-matched, an FPGA Mezzanine
    // Connector I2C bus `FMC_SDA` embeds "FM" inside the token "FMC", so a
    // standard-mode bus was judged against the 3.3x-tighter fast-mode limit,
    // firing a false rise-time finding. Only a whole `FM`/`FAST` token selects
    // fast mode.
    assert!(
        super::is_fast_mode_name("I2C_FM_SDA"),
        "explicit FM token is fast-mode"
    );
    assert!(
        super::is_fast_mode_name("SDA_FAST"),
        "explicit FAST token is fast-mode"
    );
    assert!(
        !super::is_fast_mode_name("FMC_SDA"),
        "FMC (mezzanine connector) is NOT fast-mode"
    );
    assert!(
        !super::is_fast_mode_name("CONFIRM_SCL"),
        "CONFIRM must not match FM"
    );
    assert!(
        !super::is_fast_mode_name("SDA"),
        "a plain SDA bus defaults to standard mode"
    );
}

#[test]
fn si_rail_voltage_rejects_signal_named_rails() {
    // R50: the loose 3V3/1V8 `contains` fallbacks in si.rs rail_voltage (a
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

#[test]
fn si_rail_voltage_resolves_numeric_rails_like_netlint() {
    // R51: si.rs rail_voltage handled bare "3V0" but not "5V0" and lacked
    // netlint's numeric_rail_magnitude, so a pull-up returning to a bare "5V0" /
    // "+12V" / "24V" rail was not seen as rail-like and the I2C rise-time audit
    // was silently skipped, a --si vs --lint disagreement.
    assert_eq!(super::rail_voltage("5V0"), Some(5.0));
    assert_eq!(super::rail_voltage("+12V"), Some(12.0));
    assert_eq!(super::rail_voltage("24V"), Some(24.0));
    assert_eq!(super::rail_voltage("+15V0"), Some(15.0));
    // The numeric grammar must still reject signal-tagged names.
    assert_eq!(super::rail_voltage("5V0_EN"), None);
    assert_eq!(super::rail_voltage("12V_PG"), None);
}

#[test]
fn si_rail_voltage_recognises_the_same_tokens_as_netlint() {
    // R53: si.rs rail_voltage drifted from netlint's table, VCC5V/VCC5 (5V) and
    // VPP/VDD_IO (battery/IO rails) were recognised by --lint but not --si, so the
    // same net was a rail for the pull-up-presence check but not the mirroring
    // rise-time audit (a --si vs --lint disagreement).
    assert_eq!(super::rail_voltage("VCC5V"), Some(5.0));
    assert_eq!(super::rail_voltage("VCC5"), Some(5.0));
    assert_eq!(super::rail_voltage("VPP"), Some(3.7));
    assert_eq!(super::rail_voltage("VDD_IO"), Some(3.7));
    // The loose 5V fallback (with rail context) matches too.
    assert_eq!(super::rail_voltage("VCC_5V_MCU"), Some(5.0));
}
