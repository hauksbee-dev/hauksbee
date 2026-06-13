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
    assert!(r.of_check(SiCheck::CrystalLoadCap).any(|f| f.severity == SiSeverity::Info));
}

#[test]
fn crystal_known_cl_far_off_fires() {
    // ABM8-272 specs 18 pF; two 4.7 pF caps -> 2.35 + 4 = 6.35 pF, deviation
    // 11.65 pF > 8 pF: fires (under-capacitanced -> runs fast / may not start).
    let b = xtal_board("ABM8-272-T3", "4p7", "4p7");
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 1, "far-off CL must fire");
    assert_eq!(r.of_check(SiCheck::CrystalLoadCap).next().unwrap().check, SiCheck::CrystalLoadCap);
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
    let b = pcb(
        r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3225-4Pin"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y1") (property "Value" "16MHz")
          (pad "1" smd rect (at -1 0) (net 1 "XIN"))
          (pad "2" smd rect (at -1 1) (net 3 "GND"))
          (pad "3" smd rect (at 1 0) (net 2 "XOUT"))
          (pad "4" smd rect (at 1 1) (net 3 "GND")))"#,
    );
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 1, "no load caps must fire");
    assert_eq!(r.findings_only().next().unwrap().severity, SiSeverity::Medium);
}

#[test]
fn rtc_with_integrated_caps_no_cap_is_silent() {
    // A 32.768 kHz crystal on a PCF8523 RTC (integrated load caps): no external
    // caps is CORRECT, must not fire. (The MNT Reform Y4 topology.)
    let b = pcb(
        r#"(net 1 "OSCI") (net 2 "OSCO") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3215-2Pin"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y4") (property "Value" "32.768 kHz")
          (pad "1" smd rect (at -1 0) (net 1 "OSCI"))
          (pad "2" smd rect (at 1 0) (net 2 "OSCO")))
        (footprint "Package_SO:SOIC-8"
          (at 14 10) (layer "F.Cu")
          (property "Reference" "U5") (property "Value" "PCF8523T")
          (pad "1" smd rect (at 0 0) (net 1 "OSCI"))
          (pad "2" smd rect (at 0 1) (net 2 "OSCO")))"#,
    );
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 0, "RTC integrates caps; must be silent");
}

#[test]
fn ceramic_resonator_no_caps_is_silent() {
    // A 3-terminal ceramic resonator (CSTCE / RESONATOR footprint) integrates
    // its load caps; no external caps is correct. (Arduino Uno Y2 topology.)
    let b = pcb(
        r#"(net 1 "XTAL1") (net 2 "XTAL2") (net 3 "GND")
        (footprint "Resonator:RESONATOR"
          (at 10 10) (layer "F.Cu")
          (property "Reference" "Y2") (property "Value" "CSTCE16M0V53-R0 16MHZ")
          (pad "1" smd rect (at -1 0) (net 1 "XTAL1"))
          (pad "2" smd rect (at 0 0) (net 3 "GND"))
          (pad "3" smd rect (at 1 0) (net 2 "XTAL2")))"#,
    );
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.findings.len(), 0, "ceramic resonator must be entirely silent");
}

#[test]
fn split_keyboard_mirror_prefix_caps_are_traced() {
    // The right half of a Corne carries `r`-prefixed mirror refs (rY1, rC1,
    // rC2). The type classifiers must see Y1/C1/C2 underneath, so the load caps
    // trace and the crystal is INFO (not a false "no caps" finding).
    let b = pcb(
        r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
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
          (pad "2" smd rect (at 1 0) (net 3 "GND")))"#,
    );
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 0, "mirror-prefix caps must trace, no false finding");
    assert!(r
        .of_check(SiCheck::CrystalLoadCap)
        .any(|f| f.severity == SiSeverity::Info && f.message.contains("17.5")));
}

#[test]
fn eagle_double_pad_capacitor_still_counts_as_two_terminal() {
    // The Eagle .brd extractor lists each pad once per signal contact, so a
    // 2-terminal cap can show four pin entries (pad 1 x2, pad 2 x2). The
    // distinct-pad count must still see two terminals so the load cap traces.
    let b = pcb(
        r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
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
          (pad "2" smd rect (at 1 0) (net 3 "GND")))"#,
    );
    let mut r = SiReport::default();
    check_crystal_load_cap(&b, &mut r);
    assert_eq!(r.finding_count(), 0, "double-pad caps must trace, no false 'no caps' finding");
    assert!(r.of_check(SiCheck::CrystalLoadCap).any(|f| f.severity == SiSeverity::Info));
}

#[test]
fn dnp_crystal_is_skipped() {
    let b = pcb(
        r#"(net 1 "XIN") (net 2 "XOUT") (net 3 "GND")
        (footprint "Crystal:Crystal_SMD_3225-4Pin"
          (at 10 10) (layer "F.Cu") (attr smd dnp)
          (property "Reference" "Y1") (property "Value" "16MHz")
          (pad "1" smd rect (at -1 0) (net 1 "XIN"))
          (pad "3" smd rect (at 1 0) (net 2 "XOUT")))"#,
    );
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
    check_i2c_rise_time(&b, &mut r);
    assert_eq!(r.finding_count(), 0, "strong pull / few devices must be ok");
    assert!(r.of_check(SiCheck::I2cRiseTime).any(|f| f.severity == SiSeverity::Info));
}

#[test]
fn i2c_weak_pull_heavy_bus_fires() {
    // 10k pull, 20 devices (~200 pF): t_r ~ 0.8473*10000*200e-3 = 1695 ns,
    // over standard-mode 1000 ns: fires.
    let b = i2c_board("SDA", "10k", 20);
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, &mut r);
    assert_eq!(r.finding_count(), 1, "weak pull on a heavy bus must fire");
}

#[test]
fn i2c_no_pullup_is_not_our_finding() {
    // No pull-up at all: that's netlint's presence check, not the rise-time
    // sufficiency check. We stay silent.
    let b = pcb(
        r#"(net 1 "SDA")
        (footprint "Package_SO:SOIC-8" (at 10 8) (layer "F.Cu")
          (property "Reference" "U1") (property "Value" "S")
          (pad "1" smd rect (at 0 0) (net 1 "SDA")))
        (footprint "Package_SO:SOIC-8" (at 12 8) (layer "F.Cu")
          (property "Reference" "U2") (property "Value" "S")
          (pad "1" smd rect (at 0 0) (net 1 "SDA")))"#,
    );
    let mut r = SiReport::default();
    check_i2c_rise_time(&b, &mut r);
    assert_eq!(r.findings.len(), 0, "no pull-up -> rise-time check is silent");
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
    assert!(r.of_check(SiCheck::AntennaKeepout).any(|f| f.severity == SiSeverity::Info));
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
    assert_eq!(r.finding_count(), 0, "copper outside keepout must be silent");
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

fn usb_board(plus_len: f64, minus_len: f64, plus_w: f64, minus_w: f64) -> (ExtractedBoard, forge_sexpr::Document) {
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
    assert_eq!(r.finding_count(), 0, "a width mismatch alone must not be a finding");
    let f = r.of_check(SiCheck::UsbDiffPair).next().unwrap();
    assert_eq!(f.severity, SiSeverity::Info);
    assert!(f.message.contains("width mismatch"));
}

#[test]
fn usb_polarity_classifier_rejects_non_usb() {
    // VDD, LED-, DDR must not be classified as USB polarity.
    assert_eq!(super::usb_polarity("VDD").1, None);
    assert_eq!(super::usb_polarity("LED").1, None);
    // genuine forms:
    assert_eq!(super::usb_polarity("USB_DP").1, Some('+'));
    assert_eq!(super::usb_polarity("D+").1, Some('+'));
    assert_eq!(super::usb_polarity("UD-").1, Some('-'));
}
