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
/// through 100k, so the diode blocks the whole rail.
fn diode_board(value: &str, rail: u32) -> String {
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
    (comp (ref "R1") (value "100k")
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

#[test]
fn schottkys_with_cards_fail_above_their_own_vrrm() {
    // Vishay 88746 / 88526, Diodes DS23022, semiware MBR0520L: VRRM per part.
    for (value, rail) in [
        ("SS12", 24),
        ("SS13", 36),
        ("SS14", 60),
        ("SS15", 72),
        ("SS16", 100),
        ("1N5820", 36),
        ("1N5821", 60),
        ("MBR0520", 25),
        ("MBR0520L", 36),
        ("SB140", 60),
    ] {
        let v = check(&format!("{value}_{rail}"), &diode_board(value, rail));
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
    for (value, rail) in [
        ("SS12", 12),
        ("SS14", 24),
        ("SS16", 48),
        ("1N5820", 12),
        ("1N5821", 24),
        ("MBR0520", 12),
        ("SB140", 24),
    ] {
        let v = check(&format!("{value}_{rail}"), &diode_board(value, rail));
        assert_eq!(v.outcome, "pass", "{value} on {rail} V: {}", v.why());
        assert!(
            !names("D1", &v.findings),
            "{value} is judged on its own card, not a stand-in: {}",
            v.why()
        );
    }
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
    for rail in [12, 24, 48] {
        let v = check(&format!("cap_nov_{rail}"), &cap_board("10uF", rail, ""));
        assert_eq!(v.outcome, "invalid", "{rail} V: {}", v.why());
        assert!(
            v.findings.iter().any(|m| m.contains("C1")
                && m.contains("assumed")
                && m.contains("state C1's rated voltage")),
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
