//! Connectivity lint tests. Hand-authored KiCad netlist fixtures, one per
//! check, with the true-positive *and* the false-positive cases that the
//! famous-board sweep actually hit (resistor with extra footprint pads, a
//! pull-up to a CAD-auto-named local rail, an NTS0104-class integrated-pull-up
//! translator, a connector break-out). The negatives are as important as the
//! positives: every one of them was a real false fire during the sweep.

use hauksbee_extract::{ExtractedBoard, LintCheck, Severity};

/// Minimal KiCad netlist (`export` form) wrapper: takes `components` and `nets`
/// s-expression bodies.
fn netlist(components: &str, nets: &str) -> String {
    format!(
        r#"(export (version "E")
  (components
{components}
  )
  (nets
{nets}
  )
)
"#
    )
}

fn lint(components: &str, nets: &str) -> hauksbee_extract::NetLintReport {
    let text = netlist(components, nets);
    ExtractedBoard::from_kicad_netlist(&text)
        .expect("netlist parses")
        .net_lint()
}

fn count(r: &hauksbee_extract::NetLintReport, c: LintCheck) -> usize {
    r.of_check(c).count()
}

// ---------------------------------------------------------------------------
// Model-free design-file QC checks.
// ---------------------------------------------------------------------------

#[test]
fn resistor_designator_on_capacitor_footprint_is_flagged() {
    let comps = r#"
    (comp (ref R1) (value 4k7) (footprint Capacitor_SMD:C_0603))"#;
    let r = lint(comps, "");
    assert_eq!(count(&r, LintCheck::DesignatorFootprintMismatch), 1);
    let f = r
        .of_check(LintCheck::DesignatorFootprintMismatch)
        .next()
        .unwrap();
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.message.contains("R1"));
}

#[test]
fn impossible_capacitance_for_0603_is_flagged() {
    let comps = r#"
    (comp (ref C6) (value 220uF) (footprint Capacitor_SMD:C_0603))"#;
    let r = lint(comps, "");
    assert_eq!(count(&r, LintCheck::ValuePackageSanity), 1);
    let f = r.of_check(LintCheck::ValuePackageSanity).next().unwrap();
    assert_eq!(f.severity, Severity::Medium);
    assert!(f.message.contains("220uF"));
}

#[test]
fn capacitor_value_with_voltage_rating_or_package_suffix_is_not_misparsed() {
    // Regression: parse_capacitance_uf used to collect every digit in the whole
    // string, so "10uF25V" -> 1025 uF and "10u_0402" -> 100402 uF, both falsely
    // tripping the 0402 ceiling (a zero-false-positive violation seen on a real
    // board's "10u_0402" value). Only the leading number+unit token counts, so
    // these are correctly read as 10 uF and not flagged.
    let comps = r#"
    (comp (ref C1) (value 10uF25V) (footprint Capacitor_SMD:C_0402))
    (comp (ref C2) (value 10u_0402) (footprint Capacitor_SMD:C_0402))"#;
    let r = lint(comps, "");
    assert_eq!(
        count(&r, LintCheck::ValuePackageSanity),
        0,
        "10 uF is within the 0402 ceiling regardless of a trailing voltage rating or package suffix"
    );
}

#[test]
fn placeholder_passive_value_is_flagged() {
    let comps = r#"
    (comp (ref R13) (value R) (footprint Resistor_SMD:R_0402))
    (comp (ref C7) (value C) (footprint Capacitor_SMD:C_0402))"#;
    let r = lint(comps, "");
    assert_eq!(count(&r, LintCheck::PlaceholderValue), 2);
}

#[test]
fn ordinary_passive_values_and_matching_footprints_are_clean() {
    let comps = r#"
    (comp (ref R1) (value 4k7) (footprint Resistor_SMD:R_0603))
    (comp (ref C1) (value 10uF) (footprint Capacitor_SMD:C_0603))"#;
    let r = lint(comps, "");
    assert_eq!(count(&r, LintCheck::DesignatorFootprintMismatch), 0);
    assert_eq!(count(&r, LintCheck::ValuePackageSanity), 0);
    assert_eq!(count(&r, LintCheck::PlaceholderValue), 0);
}

// ---------------------------------------------------------------------------
// I2C pull-up check.
// ---------------------------------------------------------------------------

/// A pull-up resistor to a named rail is recognised: no finding.
#[test]
fn i2c_with_named_rail_pullup_is_clean() {
    let comps = r#"
    (comp (ref U1) (value MCU) (footprint Package:QFN))
    (comp (ref U2) (value SENSOR) (footprint Package:SON))
    (comp (ref R1) (value 4k7) (footprint Resistor_SMD:R_0402))
    (comp (ref R2) (value 4k7) (footprint Resistor_SMD:R_0402))"#;
    let nets = r#"
    (net (code 1) (name "+3V3")
      (node (ref R1) (pin 1)) (node (ref R2) (pin 1)))
    (net (code 2) (name "SDA")
      (node (ref U1) (pin 1)) (node (ref U2) (pin 1)) (node (ref R1) (pin 2)))
    (net (code 3) (name "SCL")
      (node (ref U1) (pin 2)) (node (ref U2) (pin 2)) (node (ref R2) (pin 2)))"#;
    let r = lint(comps, nets);
    assert_eq!(
        count(&r, LintCheck::MissingI2cPullup),
        0,
        "named-rail pull-up should be clean"
    );
}

/// An on-board bus with no pull-up anywhere fires at medium.
#[test]
fn i2c_without_pullup_on_board_fires_medium() {
    let comps = r#"
    (comp (ref U1) (value MCU) (footprint Package:QFN))
    (comp (ref U2) (value SENSOR) (footprint Package:SON))"#;
    let nets = r#"
    (net (code 2) (name "SDA")
      (node (ref U1) (pin 1)) (node (ref U2) (pin 1)))
    (net (code 3) (name "SCL")
      (node (ref U1) (pin 2)) (node (ref U2) (pin 2)))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::MissingI2cPullup), 2);
    assert!(r
        .of_check(LintCheck::MissingI2cPullup)
        .all(|f| f.severity == Severity::Medium));
}

/// The real Olimex case: the pull-up's far pad is a CAD-auto-named local rail
/// (`Net-(C5-Pad2)`) carrying a bypass cap to ground. Must be recognised as a
/// rail so the pull-up counts. No finding.
#[test]
fn i2c_pullup_to_structural_local_rail_is_clean() {
    let comps = r#"
    (comp (ref U3) (value ESP32) (footprint Module:ESP))
    (comp (ref R18) (value 2k2) (footprint Resistor_SMD:R_0603))
    (comp (ref R21) (value 2k2) (footprint Resistor_SMD:R_0603))
    (comp (ref C5) (value 22uF) (footprint Capacitor_SMD:C_0603))"#;
    let nets = r#"
    (net (code 7) (name "Net-(C5-Pad2)")
      (node (ref R18) (pin 1)) (node (ref R21) (pin 1)) (node (ref C5) (pin 2)))
    (net (code 1) (name "GND") (node (ref C5) (pin 1)))
    (net (code 75) (name "I2C-SCL")
      (node (ref U3) (pin 27)) (node (ref R18) (pin 2)))
    (net (code 72) (name "I2C-SDA")
      (node (ref U3) (pin 16)) (node (ref R21) (pin 2)))"#;
    let r = lint(comps, nets);
    assert_eq!(
        count(&r, LintCheck::MissingI2cPullup),
        0,
        "pull-up to a bypassed local rail must be recognised"
    );
}

/// The real ZSWatch case: an NTS0104 open-drain translator supplies its own
/// internal pull-ups. A bus whose only members are the MCU and the translator
/// must not be flagged.
#[test]
fn i2c_with_integrated_pullup_translator_is_clean() {
    let comps = r#"
    (comp (ref M601) (value NORA-B106) (footprint uBlox:NORA))
    (comp (ref IC604) (value NTS0104GU12) (footprint Package_QFN:XQFN-12))"#;
    let nets = r#"
    (net (code 14) (name "TOUCH-SCL")
      (node (ref M601) (pin A5)) (node (ref IC604) (pin 4)))
    (net (code 15) (name "TOUCH-SDA")
      (node (ref M601) (pin B4)) (node (ref IC604) (pin 3)))"#;
    let r = lint(comps, nets);
    assert_eq!(
        count(&r, LintCheck::MissingI2cPullup),
        0,
        "NTS0104 integrated pull-ups must suppress the finding"
    );
}

/// A bus that only breaks out to a header is the intentional "pulls on the
/// module" pattern: downgraded to Low, not Medium.
#[test]
fn i2c_header_breakout_is_low() {
    let comps = r#"
    (comp (ref U1) (value MCU) (footprint Package:QFN))
    (comp (ref J1) (value HEADER) (footprint Connector:Header_1x04))"#;
    let nets = r#"
    (net (code 2) (name "SDA")
      (node (ref U1) (pin 1)) (node (ref J1) (pin 1)))
    (net (code 3) (name "SCL")
      (node (ref U1) (pin 2)) (node (ref J1) (pin 2)))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::MissingI2cPullup), 2);
    assert!(r
        .of_check(LintCheck::MissingI2cPullup)
        .all(|f| f.severity == Severity::Low));
}

/// A resistor with extra net-less footprint pads (the 0201 case that hid the
/// ZSWatch pull-ups) is still recognised as a two-terminal pull-up.
#[test]
fn pullup_resistor_with_extra_footprint_pads_is_recognised() {
    // The netlist form lists only connected nodes, so this models the same
    // "two connected pads, rest dangling" shape the layout produced.
    let comps = r#"
    (comp (ref U1) (value MCU) (footprint Package:QFN))
    (comp (ref U2) (value RTC) (footprint Package:SON))
    (comp (ref R1) (value 3k3) (footprint Resistor_SMD:R_0201_0603Metric))
    (comp (ref R2) (value 3k3) (footprint Resistor_SMD:R_0201_0603Metric))"#;
    let nets = r#"
    (net (code 5) (name "VBAT")
      (node (ref R1) (pin 1)) (node (ref R2) (pin 1)))
    (net (code 93) (name "Net-(IC506-SDA)")
      (node (ref U1) (pin 5)) (node (ref U2) (pin 1)) (node (ref R1) (pin 2)))
    (net (code 94) (name "Net-(IC506-SCL)")
      (node (ref U1) (pin 6)) (node (ref U2) (pin 8)) (node (ref R2) (pin 2)))"#;
    let r = lint(comps, nets);
    assert_eq!(
        count(&r, LintCheck::MissingI2cPullup),
        0,
        "pull-up to VBAT must be recognised"
    );
}

// ---------------------------------------------------------------------------
// Floating control pin check.
// ---------------------------------------------------------------------------

/// A dedicated enable pin on a single-pin (degree-1) named net is floating.
#[test]
fn floating_enable_pin_fires_high() {
    let comps = r#"
    (comp (ref U1) (value LOADSWITCH) (footprint Package:SOT))"#;
    let nets = r#"
    (net (code 10) (name "EN_STUB")
      (node (ref U1) (pin 4) (pinfunction "EN") (pintype "input")))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::FloatingControlPin), 1);
    assert_eq!(r.findings[0].severity, Severity::High);
}

/// The same enable pin driven by another part (degree 2) is fine.
#[test]
fn driven_enable_pin_is_clean() {
    let comps = r#"
    (comp (ref U1) (value LOADSWITCH) (footprint Package:SOT))
    (comp (ref U2) (value MCU) (footprint Package:QFN))"#;
    let nets = r#"
    (net (code 10) (name "EN")
      (node (ref U1) (pin 4) (pinfunction "EN") (pintype "input"))
      (node (ref U2) (pin 9) (pinfunction "GPIO5") (pintype "output")))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::FloatingControlPin), 0);
}

/// A multiplexed GPIO name that merely contains "EN" (an Ethernet TX_EN, the
/// real Olimex false positive) is not a control pin.
#[test]
fn muxed_signal_name_is_not_a_control_pin() {
    let comps = r#"
    (comp (ref U3) (value ESP32) (footprint Module:ESP))"#;
    let nets = r#"
    (net (code 33) (name "Net-(U3-Pad33)")
      (node (ref U3) (pin 33) (pinfunction "GPIO21/VSPIHD/EMAC_TX_EN") (pintype "bidirectional")))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::FloatingControlPin), 0);
}

/// An explicit no-connect (pintype no_connect, or an `unconnected-(...)` net) is
/// the designer's choice, never a floating-pin fault.
#[test]
fn explicit_no_connect_does_not_fire() {
    let comps = r#"
    (comp (ref U6) (value SENSOR) (footprint Package:QFN))"#;
    let nets = r#"
    (net (code 83) (name "unconnected-(U6-EN-Pad11)")
      (node (ref U6) (pin 11) (pinfunction "EN") (pintype "input+no_connect")))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::FloatingControlPin), 0);
}

// ---------------------------------------------------------------------------
// LED current sanity check.
// ---------------------------------------------------------------------------

/// An LED from a 5 V rail through a 100 ohm series resistor to ground draws
/// ~30 mA, above the indicator band: fires.
#[test]
fn led_overcurrent_fires() {
    // 5 V rail, 68 ohm series, Vf 2 V -> ~44 mA, clearly over the 30 mA band.
    let comps = r#"
    (comp (ref D1) (value LED) (footprint LED_SMD:LED_0603))
    (comp (ref R1) (value 68) (footprint Resistor_SMD:R_0402))"#;
    let nets = r#"
    (net (code 1) (name "+5V") (node (ref R1) (pin 1)))
    (net (code 2) (name "LEDA") (node (ref R1) (pin 2)) (node (ref D1) (pin 1)))
    (net (code 3) (name "GND") (node (ref D1) (pin 2)))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::LedCurrentSanity), 1);
}

/// R15: the LED-current check must not abandon its rail search on the first
/// co-located R-ref part whose value does not parse. Here a DNP option resistor
/// (R0, value "DNP") sits on the anode net BEFORE the real 68 ohm series
/// resistor; the old `?` on parse_ohms returned None for the whole search, so
/// the over-current LED was silently passed. The genuine series resistor must
/// still be found and the finding must fire.
#[test]
fn led_current_search_skips_unparseable_resistor_and_still_fires() {
    let comps = r#"
    (comp (ref D1) (value LED) (footprint LED_SMD:LED_0603))
    (comp (ref R0) (value DNP) (footprint Resistor_SMD:R_0402))
    (comp (ref R1) (value 68) (footprint Resistor_SMD:R_0402))"#;
    let nets = r#"
    (net (code 1) (name "+5V") (node (ref R1) (pin 1)))
    (net (code 2) (name "LEDA") (node (ref R0) (pin 1)) (node (ref R1) (pin 2)) (node (ref D1) (pin 1)))
    (net (code 3) (name "GND") (node (ref R0) (pin 2)) (node (ref D1) (pin 2)))"#;
    let r = lint(comps, nets);
    assert_eq!(
        count(&r, LintCheck::LedCurrentSanity),
        1,
        "the DNP resistor must not abort the search before the real 68 ohm series R"
    );
}

/// A sane 1 kohm series resistor from 3.3 V (~1.3 mA) does not fire.
#[test]
fn led_sane_current_is_clean() {
    let comps = r#"
    (comp (ref D1) (value LED) (footprint LED_SMD:LED_0603))
    (comp (ref R1) (value 1k) (footprint Resistor_SMD:R_0402))"#;
    let nets = r#"
    (net (code 1) (name "+3V3") (node (ref R1) (pin 1)))
    (net (code 2) (name "LEDA") (node (ref R1) (pin 2)) (node (ref D1) (pin 1)))
    (net (code 3) (name "GND") (node (ref D1) (pin 2)))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::LedCurrentSanity), 0);
}

// ---------------------------------------------------------------------------
// Output-vs-output contention check (Round-4 schematic ERC).
//
// True-positive plus every false-positive shape the Round-4 calibration hit on
// the known-good corpus: a series resistor between two outputs, an input/
// bidirectional member reframing the net, an IRQ/wired-OR name, two pins of a
// single part, and an off-board connector. The check must fire ONLY on the bare
// two-part push-pull short.
// ---------------------------------------------------------------------------

/// Two distinct ICs each driving one net with a push-pull `output` pin, nothing
/// to resolve them: a real bus fight. Fires at medium.
#[test]
fn two_outputs_tied_directly_fires_medium() {
    let comps = r#"
    (comp (ref U1) (value DRIVER_A) (footprint Package:SOT23))
    (comp (ref U2) (value DRIVER_B) (footprint Package:SOT23))"#;
    let nets = r#"
    (net (code 1) (name "DRIVE")
      (node (ref U1) (pin 1) (pinfunction OUT) (pintype output))
      (node (ref U2) (pin 1) (pinfunction OUT) (pintype output)))"#;
    let r = lint(comps, nets);
    assert_eq!(
        count(&r, LintCheck::OutputContention),
        1,
        "two bare push-pull outputs on one net must fire"
    );
    let f = r.of_check(LintCheck::OutputContention).next().unwrap();
    assert_eq!(f.severity, Severity::Medium);
}

/// Two `power_out` pins of different supplies tied together: a hard rail-vs-rail
/// short. Fires at HIGH.
#[test]
fn two_power_outputs_tied_directly_fires_high() {
    let comps = r#"
    (comp (ref U1) (value REG_A) (footprint Package:SOT23))
    (comp (ref U2) (value REG_B) (footprint Package:SOT23))"#;
    let nets = r#"
    (net (code 1) (name "RAILX")
      (node (ref U1) (pin 1) (pinfunction VOUT) (pintype power_out))
      (node (ref U2) (pin 1) (pinfunction VOUT) (pintype power_out)))"#;
    let r = lint(comps, nets);
    let fs: Vec<_> = r.of_check(LintCheck::OutputContention).collect();
    assert_eq!(fs.len(), 1, "two power_out pins shorted must fire");
    assert_eq!(fs[0].severity, Severity::High);
}

/// A series resistor between the two outputs resolves the contention: clean.
/// (This is the dominant benign shape: a wired-OR through series R.)
#[test]
fn series_resistor_between_outputs_is_clean() {
    let comps = r#"
    (comp (ref U1) (value DRIVER_A) (footprint Package:SOT23))
    (comp (ref U2) (value DRIVER_B) (footprint Package:SOT23))
    (comp (ref R1) (value 100) (footprint Resistor_SMD:R_0402))"#;
    let nets = r#"
    (net (code 1) (name "DRIVE_A")
      (node (ref U1) (pin 1) (pintype output))
      (node (ref R1) (pin 1) (pintype passive)))
    (net (code 2) (name "DRIVE_B")
      (node (ref U2) (pin 1) (pintype output))
      (node (ref R1) (pin 2) (pintype passive)))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::OutputContention), 0);
}

/// An `input` member on the net reframes it as a driven input the symbol author
/// also typed `output` (the shared-IO / repurposed-pin case, e.g. Reform's
/// EDP_IRQ where a SoM pin is typed output): do not fire.
#[test]
fn output_with_input_member_is_clean() {
    let comps = r#"
    (comp (ref U1) (value SOM) (footprint Package:BGA))
    (comp (ref U2) (value BRIDGE) (footprint Package:QFP))
    (comp (ref U3) (value MCU) (footprint Package:QFN))"#;
    let nets = r#"
    (net (code 1) (name "SHARED_IO")
      (node (ref U1) (pin 1) (pintype output))
      (node (ref U2) (pin 1) (pintype output))
      (node (ref U3) (pin 1) (pintype input)))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::OutputContention), 0);
}

/// An IRQ-named net carries open-drain "outputs" by convention: excluded by name.
#[test]
fn irq_named_net_is_excluded() {
    let comps = r#"
    (comp (ref U1) (value SOM) (footprint Package:BGA))
    (comp (ref U2) (value BRIDGE) (footprint Package:QFP))"#;
    let nets = r#"
    (net (code 1) (name "EDP_IRQ")
      (node (ref U1) (pin 1) (pintype output))
      (node (ref U2) (pin 1) (pintype output)))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::OutputContention), 0);
}

/// Two output pins of a SINGLE part on one net is internal to that part's
/// symbol, not an inter-part fight: do not fire.
#[test]
fn two_outputs_of_one_part_is_clean() {
    let comps = r#"
    (comp (ref U1) (value DUAL) (footprint Package:QFP))"#;
    let nets = r#"
    (net (code 1) (name "DRIVE")
      (node (ref U1) (pin 1) (pintype output))
      (node (ref U1) (pin 2) (pintype output)))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::OutputContention), 0);
}

/// An off-board connector on the net is a potential external sink/driver, not an
/// on-board contention: do not fire.
#[test]
fn output_to_connector_is_clean() {
    let comps = r#"
    (comp (ref U1) (value DRIVER) (footprint Package:SOT23))
    (comp (ref U2) (value DRIVER) (footprint Package:SOT23))
    (comp (ref J1) (value HEADER) (footprint Connector:PinHeader_1x03))"#;
    let nets = r#"
    (net (code 1) (name "OUT_TO_J1")
      (node (ref U1) (pin 1) (pintype output))
      (node (ref U2) (pin 1) (pintype output))
      (node (ref J1) (pin 1) (pintype passive)))"#;
    let r = lint(comps, nets);
    assert_eq!(count(&r, LintCheck::OutputContention), 0);
}

/// R35: the output-contention finding's `refs` were collected straight from a
/// randomized HashSet, so `--lint --json` emitted them in a run-varying order.
/// With six distinct drivers on one net the array must come out sorted every
/// run (the fix sorts it). On the un-sorted base this holds only ~1/720 of runs,
/// so running this binary a few times reliably catches the disorder.
#[test]
fn output_contention_refs_are_sorted_deterministically() {
    let comps = r#"
    (comp (ref U6) (value D) (footprint Package:SOT23))
    (comp (ref U3) (value D) (footprint Package:SOT23))
    (comp (ref U1) (value D) (footprint Package:SOT23))
    (comp (ref U5) (value D) (footprint Package:SOT23))
    (comp (ref U2) (value D) (footprint Package:SOT23))
    (comp (ref U4) (value D) (footprint Package:SOT23))"#;
    let nets = r#"
    (net (code 1) (name "BUSFIGHT")
      (node (ref U6) (pin 1) (pintype output))
      (node (ref U3) (pin 1) (pintype output))
      (node (ref U1) (pin 1) (pintype output))
      (node (ref U5) (pin 1) (pintype output))
      (node (ref U2) (pin 1) (pintype output))
      (node (ref U4) (pin 1) (pintype output)))"#;
    let f = lint(comps, nets)
        .of_check(LintCheck::OutputContention)
        .next()
        .cloned()
        .expect("six push-pull drivers must fire contention");
    assert_eq!(
        f.refs,
        vec!["U1", "U2", "U3", "U4", "U5", "U6"],
        "refs must be emitted in sorted order for reproducible JSON, got {:?}",
        f.refs
    );
}

/// R35: `control_role` promised (in its own doc/inline comments) to handle the
/// bare trailing-N active-low reset spelling, but only stripped the "_N" form,
/// so `RESETN` / `RSTN` normalised to themselves and matched no reset arm —
/// silently skipping a floating active-low reset. A degree-1 RESETN pin on an
/// active IC must now fire the High floating-control finding.
#[test]
fn floating_bare_trailing_n_reset_fires_high() {
    for func in ["RESETN", "RSTN"] {
        let comps = r#"
    (comp (ref U3) (value MCU) (footprint Package:QFN))"#;
        let nets = format!(
            r#"
    (net (code 10) (name "MCU_RST")
      (node (ref U3) (pin 7) (pinfunction "{func}") (pintype "input")))"#
        );
        let r = lint(comps, &nets);
        assert_eq!(
            count(&r, LintCheck::FloatingControlPin),
            1,
            "a floating active-low reset spelled '{func}' must fire"
        );
        assert_eq!(r.findings[0].severity, Severity::High);
    }
}

/// R46: control_role listed active-low reset (RSTN/RESETN) but not the equally
/// common active-low chip-select CSN/SSN (the nRF24L01 SPI select pin name), so a
/// floating CSN input was silently unflagged while an identically-structured RSTN
/// on the same net fired. Both must fire.
#[test]
fn floating_active_low_chip_select_fires_high() {
    for func in ["CSN", "SSN"] {
        let comps = r#"
    (comp (ref U4) (value NRF24L01) (footprint Package:QFN))"#;
        let nets = format!(
            r#"
    (net (code 11) (name "SPI_CS")
      (node (ref U4) (pin 8) (pinfunction "{func}") (pintype "input")))"#
        );
        let r = lint(comps, &nets);
        assert_eq!(
            count(&r, LintCheck::FloatingControlPin),
            1,
            "a floating active-low chip-select spelled '{func}' must fire"
        );
        assert_eq!(r.findings[0].severity, Severity::High);
    }
}
