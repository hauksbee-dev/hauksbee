//! Connectivity lint tests. Hand-authored KiCad netlist fixtures, one per
//! check, with the true-positive *and* the false-positive cases that the
//! famous-board sweep actually hit (resistor with extra footprint pads, a
//! pull-up to a CAD-auto-named local rail, an NTS0104-class integrated-pull-up
//! translator, a connector break-out). The negatives are as important as the
//! positives: every one of them was a real false fire during the sweep.

use galvani_extract::{ExtractedBoard, LintCheck, Severity};

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

fn lint(components: &str, nets: &str) -> galvani_extract::NetLintReport {
    let text = netlist(components, nets);
    ExtractedBoard::from_kicad_netlist(&text)
        .expect("netlist parses")
        .net_lint()
}

fn count(r: &galvani_extract::NetLintReport, c: LintCheck) -> usize {
    r.of_check(c).count()
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
    assert_eq!(count(&r, LintCheck::MissingI2cPullup), 0, "named-rail pull-up should be clean");
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
    assert!(r.of_check(LintCheck::MissingI2cPullup).all(|f| f.severity == Severity::Medium));
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
    assert!(r.of_check(LintCheck::MissingI2cPullup).all(|f| f.severity == Severity::Low));
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
