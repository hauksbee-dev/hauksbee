//! Kelvin sensing of current-sense shunts on hand-authored layouts: a sense
//! connection that joins the load pour far from the shunt next to one that
//! leaves the shunt pad on its own track, and resistors that are not shunts.

use hauksbee_extract::{ExtractedBoard, LintCheck, LintFinding, Severity};

/// A board with a 2512 shunt `R1` (pad 1 on `SHUNT_HI` at (47, 50), pad 2 on
/// GND), a solid-state relay `K1` switching the load onto `SHUNT_HI` at
/// (20, 50), a 1k input resistor `R2` from `SHUNT_HI` (pad 2 at `tap_at`) to
/// `AMP_IN`, and an op-amp `U1` whose non-inverting input is on `AMP_IN`.
fn board(shunt_value: &str, tap_at: (f64, f64), copper: &str) -> String {
    let (tx, ty) = tap_at;
    format!(
        r#"(kicad_pcb (version 20221018) (generator pcbnew)
  (layers (0 "F.Cu" signal) (31 "B.Cu" signal))
  (net 0 "")
  (net 1 "SHUNT_HI")
  (net 2 "GND")
  (net 3 "AMP_IN")
  (net 4 "VBAT")
  (net 5 "AMP_OUT")
  (net 6 "VCC")
  (footprint "Resistor_SMD:R_2512_6332Metric" (layer "F.Cu") (at 50 50)
    (property "Reference" "R1") (property "Value" "{shunt_value}")
    (pad "1" smd rect (at -3 0) (size 1.6 3.4) (layers "F.Cu") (net 1 "SHUNT_HI"))
    (pad "2" smd rect (at 3 0) (size 1.6 3.4) (layers "F.Cu") (net 2 "GND")))
  (footprint "Relay_SMD:SSR_SOP-4" (layer "F.Cu") (at 20 50)
    (property "Reference" "K1") (property "Value" "SSR")
    (pad "1" smd rect (at 0 0) (size 1.5 1.5) (layers "F.Cu") (net 1 "SHUNT_HI"))
    (pad "2" smd rect (at 0 8) (size 1.5 1.5) (layers "F.Cu") (net 4 "VBAT")))
  (footprint "Resistor_SMD:R_0603_1608Metric" (layer "F.Cu") (at {tx} {ty})
    (property "Reference" "R2") (property "Value" "1k")
    (pad "1" smd rect (at 0 -6) (size 1 1) (layers "F.Cu") (net 3 "AMP_IN"))
    (pad "2" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net 1 "SHUNT_HI")))
  (footprint "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm" (layer "F.Cu") (at 70 75)
    (property "Reference" "U1") (property "Value" "OPA_GENERIC")
    (pad "1" smd rect (at -2.5 -1.9) (size 1.5 0.6) (layers "F.Cu") (net 5 "AMP_OUT") (pinfunction "OUT"))
    (pad "2" smd rect (at -2.5 -0.6) (size 1.5 0.6) (layers "F.Cu") (net 5 "AMP_OUT") (pinfunction "-IN"))
    (pad "3" smd rect (at -2.5 0.6) (size 1.5 0.6) (layers "F.Cu") (net 3 "AMP_IN") (pinfunction "+IN"))
    (pad "4" smd rect (at -2.5 1.9) (size 1.5 0.6) (layers "F.Cu") (net 2 "GND") (pinfunction "V-"))
    (pad "8" smd rect (at 2.5 -1.9) (size 1.5 0.6) (layers "F.Cu") (net 6 "VCC") (pinfunction "V+")))
{copper}
)
"#
    )
}

/// A `SHUNT_HI` pour on F.Cu from x 15 to 50 and y 45 to 55: it takes in the
/// shunt's pad 1 and the relay's pad 1.
const POUR: &str = r#"  (zone (net 1) (net_name "SHUNT_HI") (layer "F.Cu") (min_thickness 0.25)
    (fill yes)
    (polygon (pts (xy 15 45) (xy 50 45) (xy 50 55) (xy 15 55)))
    (filled_polygon (layer "F.Cu") (pts (xy 15 45) (xy 50 45) (xy 50 55) (xy 15 55))))"#;

fn kelvin(text: &str) -> Vec<LintFinding> {
    let pcb = ExtractedBoard::from_kicad_pcb(text).expect("board parses");
    pcb.layout_lint(text, None)
        .findings
        .into_iter()
        .filter(|f| f.check == LintCheck::KelvinSense)
        .collect()
}

/// The percentage the finding gives for the shared copper.
fn percent(f: &LintFinding) -> f64 {
    let (head, _) = f
        .message
        .split_once("% of the shunt")
        .expect("a percentage");
    head.rsplit(' ').next().unwrap().parse().unwrap()
}

#[test]
fn a_sense_connection_joining_the_load_pour_far_from_the_shunt_is_reported() {
    // The input resistor's pad sits in the pour about 25 mm from the shunt pad,
    // next to where the relay feeds the load current in.
    let found = kelvin(&board("5m", (22.0, 52.0), POUR));
    assert_eq!(found.len(), 1, "{found:#?}");
    let f = &found[0];
    assert_eq!(f.severity, Severity::Medium);
    assert_eq!(f.refs, ["R1", "R2"]);
    assert_eq!(f.nets, ["SHUNT_HI"]);
    assert!(f.message.contains("a copper pour"), "{}", f.message);
    // Two point contacts 25 mm apart in a 1 oz sheet: a few tenths of a
    // milliohm, several per cent of 5 mOhm.
    let pct = percent(f);
    assert!((3.0..30.0).contains(&pct), "{pct}: {}", f.message);
}

#[test]
fn a_sense_track_from_the_shunt_pad_itself_is_silent() {
    // The input resistor sits above the pour and a track of its own runs from
    // the shunt pad to it; the track crosses the pour, but the load current
    // reaches the pad through the pour, not along the track.
    let track = format!(
        "{POUR}\n  (segment (start 47 50) (end 47 40) (width 0.25) (layer \"F.Cu\") (net 1))"
    );
    assert!(kelvin(&board("5m", (47.0, 40.0), &track)).is_empty());
    // The same track started from the pour 17 mm along instead of at the pad
    // is no Kelvin connection.
    let from_pour = format!(
        "{POUR}\n  (segment (start 30 50) (end 30 40) (width 0.25) (layer \"F.Cu\") (net 1))"
    );
    let found = kelvin(&board("5m", (30.0, 40.0), &from_pour));
    assert_eq!(found.len(), 1, "{found:#?}");
}

#[test]
fn a_sense_connection_on_the_load_track_far_from_the_shunt_is_reported() {
    // No pour: a 2 mm load track runs from the relay to the shunt, and the
    // input resistor's pad touches it 25 mm from the shunt.
    let track = "  (segment (start 47 50) (end 20 50) (width 2) (layer \"F.Cu\") (net 1))";
    let found = kelvin(&board("5m", (22.0, 51.5), track));
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(
        found[0].message.contains("track copper"),
        "{}",
        found[0].message
    );
    // About 12 squares of 1 oz copper against 5 mOhm.
    assert!(percent(&found[0]) > 50.0, "{}", found[0].message);
}

#[test]
fn a_resistor_that_is_not_a_shunt_is_silent() {
    assert!(kelvin(&board("10", (22.0, 52.0), POUR)).is_empty());
    assert!(kelvin(&board("1k", (22.0, 52.0), POUR)).is_empty());
}

#[test]
fn a_pour_saved_without_its_fill_is_not_judged() {
    let unfilled = POUR
        .split("\n    (filled_polygon")
        .next()
        .map(|z| format!("{z})"))
        .unwrap();
    assert!(kelvin(&board("5m", (22.0, 52.0), &unfilled)).is_empty());
}

#[test]
fn a_shunt_with_no_measurement_input_is_silent() {
    // The same layout with the op-amp's inputs named as supplies: nothing
    // measures the shunt, so it is just a low-value resistor.
    let text = board("5m", (22.0, 52.0), POUR)
        .replace("(pinfunction \"+IN\")", "(pinfunction \"NC\")")
        .replace("(pinfunction \"-IN\")", "(pinfunction \"NC\")");
    assert!(kelvin(&text).is_empty());
}
