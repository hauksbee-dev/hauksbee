//! A part the library cannot match is never granted a rating it may not have.
//!
//! Two gaps let an under-rated board pass: a rectifier or Schottky diode with
//! no card (SS12, 20 V) bound to the 1N4148 fallback and was held to no
//! blocking voltage at all, and a capacitor whose value states no voltage was
//! held to none either. Each board here is one source, one part and one
//! resistor; the part sits across the rail so it carries the full rail voltage
//! in reverse (the diode) or continuously (the capacitor).
//!
//! Faulty cases: the part's own datasheet rating is below the rail (carded
//! diodes, a stated capacitor rating). Legal controls: the same part under its
//! rating, and the unrecognised or unrated part under the lowest class floor.
//! The unknown part past the floor is a stated condition, never a failure and
//! never a silent pass.

use crate::support::{run, stdout};
use std::path::PathBuf;

/// SOD-123 diode D1: pin 1 (cathode) on the rail, pin 2 (anode) to ground
/// through 10k, so the diode blocks the whole rail. 10k rather than 100k: a
/// carded Schottky's modelled leakage (tens of microamps on a 1N5820) would
/// drop volts across 100k and the junction would never see the rail.
fn diode_board(value: &str, rail: u32) -> String {
    diode_board_at(value, &rail.to_string())
}

/// [`diode_board`] on a rail named `+{rail}V`, `rail` a decimal string.
fn diode_board_at(value: &str, rail: &str) -> String {
    format!(
        r#"(export (version "E")
  (design (source "parts fallback probe"))
  (components
    (comp (ref "J1") (value "PWR")
      (footprint "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical")
      (libsource (lib "Device") (part "X")))
    (comp (ref "D1") (value "{value}")
      (footprint "Diode_SMD:D_SOD-123")
      (libsource (lib "Device") (part "X")))
    (comp (ref "R1") (value "10k")
      (footprint "Resistor_SMD:R_0805_2012Metric")
      (libsource (lib "Device") (part "X"))))
  (nets
    (net (code "1") (name "+{rail}V")
      (node (ref "J1") (pin "1") (pintype "passive"))
      (node (ref "D1") (pin "1") (pintype "passive")))
    (net (code "2") (name "GND")
      (node (ref "J1") (pin "2") (pintype "passive"))
      (node (ref "R1") (pin "2") (pintype "passive")))
    (net (code "3") (name "N1")
      (node (ref "D1") (pin "2") (pintype "passive"))
      (node (ref "R1") (pin "1") (pintype "passive")))))
"#
    )
}

/// Capacitor C1 straight across the rail with a bleed resistor; `extra` is
/// spliced into the component (a field such as a Voltage property).
fn cap_board(value: &str, rail: u32, extra: &str) -> String {
    format!(
        r#"(export (version "E")
  (design (source "parts fallback probe"))
  (components
    (comp (ref "J1") (value "PWR")
      (footprint "Connector_PinHeader_2.54mm:PinHeader_1x02_P2.54mm_Vertical")
      (libsource (lib "Device") (part "X")))
    (comp (ref "C1") (value "{value}")
      (footprint "Capacitor_SMD:C_0805_2012Metric")
      (libsource (lib "Device") (part "X")){extra})
    (comp (ref "R1") (value "100k")
      (footprint "Resistor_SMD:R_0805_2012Metric")
      (libsource (lib "Device") (part "X"))))
  (nets
    (net (code "1") (name "+{rail}V")
      (node (ref "J1") (pin "1") (pintype "passive"))
      (node (ref "C1") (pin "1") (pintype "passive"))
      (node (ref "R1") (pin "1") (pintype "passive")))
    (net (code "2") (name "GND")
      (node (ref "J1") (pin "2") (pintype "passive"))
      (node (ref "C1") (pin "2") (pintype "passive"))
      (node (ref "R1") (pin "2") (pintype "passive")))))
"#
    )
}

struct Probe {
    outcome: String,
    reason: String,
    findings: Vec<String>,
    conditions: Vec<String>,
}

fn check(tag: &str, netlist: &str) -> Probe {
    let dir = hauksbee_testkit::fresh_scratch_dir("parts_fallback_ratings", tag);
    let path: PathBuf = dir.join("probe.net");
    std::fs::write(&path, netlist).expect("write probe netlist");
    let out = run(&["check", path.to_str().unwrap(), "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&stdout(&out))
        .unwrap_or_else(|e| panic!("{tag}: --json output is JSON ({e}): {}", stdout(&out)));
    let strings = |key: &str, field: Option<&str>| -> Vec<String> {
        let items = doc[key].as_array().cloned().unwrap_or_default();
        items
            .iter()
            .filter_map(|x| field.map_or_else(|| x.as_str(), |f| x[f].as_str()))
            .map(str::to_owned)
            .collect()
    };
    Probe {
        outcome: doc["verdict"].as_str().unwrap_or_default().to_owned(),
        reason: doc["verdict_reason"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        findings: strings("findings", Some("message")),
        conditions: strings("verdict_conditions", None),
    }
}

fn names(part: &str, over: &[String]) -> bool {
    over.iter().any(|m| m.contains(part))
}

impl Probe {
    fn why(&self) -> String {
        format!(
            "{} ({}): findings {:#?}, conditions {:#?}",
            self.outcome, self.reason, self.findings, self.conditions
        )
    }
}

/// Each carded part with its datasheet VRRM (V): Vishay 88746 / 88526,
/// Diodes DS23022, semiware MBR0520L.
const CARDED_VRRM: [(&str, u32); 10] = [
    ("SS12", 20),
    ("SS13", 30),
    ("SS14", 40),
    ("SS15", 50),
    ("SS16", 60),
    ("1N5820", 20),
    ("1N5821", 30),
    ("MBR0520", 20),
    ("MBR0520L", 20),
    ("SB140", 40),
];

/// `vrrm * percent / 100` as a rail name's decimal, to a tenth of a volt.
fn rail_at(vrrm: u32, percent: u32) -> String {
    let tenths = vrrm * percent / 10;
    if tenths.is_multiple_of(10) {
        (tenths / 10).to_string()
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

#[test]
fn schottkys_with_cards_fail_above_their_own_vrrm() {
    // The rails the false-safe was found on, then 5, 10 and 20 % over VRRM.
    // A junction past its knee clamps within tens of millivolts of VRRM, so
    // these trip on the monitor's breakdown rule, not the plain ratio.
    let original = [
        ("SS12", 24),
        ("SS13", 36),
        ("SS14", 48),
        ("SS15", 60),
        ("SS16", 72),
        ("1N5820", 24),
        ("1N5821", 36),
        ("MBR0520", 24),
        ("MBR0520L", 24),
        ("SB140", 48),
    ]
    .map(|(value, rail)| (value, rail.to_string()));
    let over = CARDED_VRRM
        .iter()
        .flat_map(|&(value, vrrm)| [105, 110, 120].map(|p| (value, rail_at(vrrm, p))));
    for (value, rail) in original.into_iter().chain(over) {
        let v = check(&format!("{value}_{rail}"), &diode_board_at(value, &rail));
        assert_eq!(v.outcome, "fail", "{value} on {rail} V: {}", v.why());
        assert!(
            names("D1", &v.findings),
            "{value} on {rail} V names D1: {}",
            v.why()
        );
    }
}

#[test]
fn schottkys_with_cards_pass_below_their_own_vrrm() {
    let legal = CARDED_VRRM
        .iter()
        .map(|&(value, vrrm)| (value, rail_at(vrrm, 80)))
        .chain([("SS14", "24".to_owned()), ("1N5821", "24".to_owned())]);
    for (value, rail) in legal {
        let v = check(&format!("{value}_{rail}"), &diode_board_at(value, &rail));
        assert_eq!(v.outcome, "pass", "{value} on {rail} V: {}", v.why());
        assert!(
            !names("D1", &v.findings),
            "{value} is judged on its own card, not a stand-in: {}",
            v.why()
        );
    }
}

#[test]
fn a_rail_exactly_at_a_diodes_vrrm_is_not_over_it() {
    // The boundary: VRRM is the most a diode may block, so a rail at it is
    // legal, and the breakdown rule must not read the junction sitting at its
    // knee as the circuit exceeding it. 1.05x is over (see
    // `schottkys_with_cards_fail_above_their_own_vrrm`).
    for &(value, vrrm) in &CARDED_VRRM {
        let rail = vrrm.to_string();
        let v = check(&format!("{value}_at_{rail}"), &diode_board_at(value, &rail));
        assert_eq!(v.outcome, "pass", "{value} on {rail} V: {}", v.why());
        assert!(
            !names("D1", &v.findings),
            "{value} at its VRRM: {}",
            v.why()
        );
    }
}

#[test]
fn a_tvs_named_by_its_standoff_is_quiet_at_or_under_it() {
    // SMF28A stands off 28 V (Vishay SMF series). Rails at and under that
    // are what it is for; a rail past it is a stated condition, never a pass
    // of the stand-in and never a failure.
    for rail in ["12", "24", "28"] {
        let v = check(&format!("tvs_{rail}"), &diode_board_at("SMF28A", rail));
        assert_eq!(v.outcome, "pass", "SMF28A on {rail} V: {}", v.why());
        assert!(!names("D1", &v.findings), "SMF28A on {rail} V: {}", v.why());
    }
    let v = check("tvs_36", &diode_board_at("SMF28A", "36"));
    assert_eq!(v.outcome, "invalid", "SMF28A on 36 V: {}", v.why());
    assert!(names("D1", &v.findings), "{}", v.why());
}

#[test]
fn an_unrecognised_diode_past_the_class_floor_is_a_condition_not_a_pass() {
    // The verdict is "partly judged" (invalid): neither a failure of a stand-in
    // rating nor a pass; a low-severity note names the assumed rating.
    let v = check("unknown_diode_25", &diode_board("ZQ-9000", 25));
    assert_eq!(v.outcome, "invalid", "never a pass or a fail: {}", v.why());
    assert!(
        v.findings
            .iter()
            .any(|m| m.contains("D1") && m.contains("so this is a condition, not a fault")),
        "the 25 V rail past the assumed 20 V floor is named as a condition: {}",
        v.why()
    );
}

#[test]
fn an_unrecognised_diode_under_the_class_floor_raises_no_condition() {
    let v = check("unknown_diode_12", &diode_board("ZQ-9000", 12));
    assert_eq!(v.outcome, "pass", "{}", v.why());
    assert!(!names("D1", &v.findings), "{}", v.why());
}

#[test]
fn a_capacitor_with_no_stated_voltage_is_named_as_unverified_on_a_high_rail() {
    // The part is its own card and only its rating is unknown, so this is a
    // condition the pass rests on (named in `verdict_conditions`), not a
    // partly judged run: an unrated decoupling cap is on nearly every board.
    for rail in [12, 24, 48] {
        let v = check(&format!("cap_nov_{rail}"), &cap_board("10uF", rail, ""));
        assert_eq!(v.outcome, "pass", "{rail} V: {}", v.why());
        assert!(
            v.conditions.iter().any(|m| m.contains("C1")
                && m.contains("rated voltage not stated")
                && m.contains("assumed")
                && m.contains("10uF 25V")),
            "{rail} V: the unstated rating is a named condition: {}",
            v.why()
        );
    }
}

#[test]
fn a_capacitor_with_no_stated_voltage_on_a_low_rail_is_quiet() {
    let v = check("cap_nov_5", &cap_board("10uF", 5, ""));
    assert_eq!(v.outcome, "pass", "{}", v.why());
    assert!(!names("C1", &v.findings), "{}", v.why());
}

#[test]
fn a_capacitor_that_states_its_voltage_is_judged_against_it() {
    // Faulty: 16 V on 24 V and 48 V rails.
    for (value, rail) in [("10uF 16V", 24), ("10uF/16V", 48), ("10uF (16 V) X7R", 24)] {
        let v = check(
            &format!("cap_{rail}_{}", value.len()),
            &cap_board(value, rail, ""),
        );
        assert_eq!(v.outcome, "fail", "{value} on {rail} V: {}", v.why());
        assert!(names("C1", &v.findings), "{}", v.why());
    }
    // Legal controls: a rating above the rail passes with no condition on C1.
    for (value, rail) in [("10uF 50V", 24), ("10uF/100V", 48), ("10uF 25 V", 12)] {
        let v = check(
            &format!("cap_ok_{rail}_{}", value.len()),
            &cap_board(value, rail, ""),
        );
        assert_eq!(v.outcome, "pass", "{value} on {rail} V: {}", v.why());
        assert!(!names("C1", &v.findings), "{}", v.why());
    }
}

#[test]
fn a_voltage_field_states_the_capacitor_rating() {
    let field = |v: &str| format!(r#"(property (name "Voltage") (value "{v}"))"#);
    let bad = check("cap_field_16", &cap_board("10uF", 24, &field("16V")));
    assert_eq!(bad.outcome, "fail", "{}", bad.why());
    let good = check("cap_field_50", &cap_board("10uF", 24, &field("50V")));
    assert_eq!(good.outcome, "pass", "{}", good.why());
    assert!(!names("C1", &good.findings), "{}", good.why());
}
