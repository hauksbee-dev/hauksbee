//! An open part undermines only the assertions whose path it is on.
//!
//! The board: 5 V through 390 ohm into a diode whose card rates 12 mA at 25 C
//! and 8 mA at 70 C, so it carries about 11 mA and is overstressed at 70 C
//! only. One unidentified part (`U9`, no model) sits either beside the diode
//! (on its anode net) or on a net the diode circuit never touches.
//!
//! Faults (must not hide): an overstress found on a modelled part stays a
//! failure when the open part is elsewhere. Controls (must not pass): the same
//! open part on the diode's own net still leaves it INVALID, and a clean
//! board-wide stress check over an open part is never a pass.

use serde_json::Value;
use std::fmt::Write as _;

const CARD: &str = r#"[[models]]
id = "hbtest_local_diode"
kind = "diode"
description = "Test-only diode whose current rating falls with ambient"
[models.match]
value_re = "(?i)^HBTEST-LOCAL$"
[models.params]
is = 2.52e-9
n = 1.752
rs = 0.568
[models.pins]
"1" = "cathode"
"2" = "anode"
[models.ratings]
max_current_a = 0.008
max_power_w = 0.5
max_voltage_v = 100.0
max_junction_temp_c = 150.0
theta_ja_c_per_w = 300.0
[models.source]
tier = "datasheet-derived"
[[models.source.characteristic]]
parameter = "IF forward current"
rating = "max_current_a"
max = 12.0
unit = "mA"
conditions = "TA = 25 °C"
basis = "test card"
[[models.source.characteristic]]
parameter = "IF forward current"
rating = "max_current_a"
max = 8.0
unit = "mA"
conditions = "TA = 70 °C"
basis = "test card"
"#;

/// An unidentified part: reference, and its two pins as (net, pin type).
type OpenPart<'a> = (&'a str, [(&'a str, &'a str); 2]);

/// The diode circuit plus the unidentified parts in `open` (each has no model).
fn board(open: &[OpenPart]) -> String {
    let node = |reference: &str, pin: &str, kind: &str| {
        format!("(node (ref \"{reference}\") (pin \"{pin}\") (pintype \"{kind}\"))")
    };
    let mut nets: Vec<(&str, Vec<String>)> = vec![
        ("VCC", vec![node("R1", "1", "passive")]),
        (
            "A",
            vec![node("R1", "2", "passive"), node("D1", "2", "passive")],
        ),
        ("GND", vec![node("D1", "1", "passive")]),
        ("X", Vec::new()),
    ];
    for (reference, pins) in open {
        for (pin, (net, kind)) in ["1", "2"].into_iter().zip(pins) {
            let entry = nets.iter_mut().find(|(n, _)| n == net).expect("known net");
            entry.1.push(node(reference, pin, kind));
        }
    }
    let mut net_block = String::new();
    for (code, (name, nodes)) in (1..).zip(nets) {
        if !nodes.is_empty() {
            let _ = writeln!(
                net_block,
                "    (net (code \"{code}\") (name \"{name}\") {})",
                nodes.join(" ")
            );
        }
    }
    let mut comps = String::new();
    for (reference, _) in open {
        let _ = write!(
            comps,
            "\x20   (comp (ref \"{reference}\") (value \"MYSTERY-IC\") \
             (footprint \"Package_SO:SOIC-8\")\n\
             \x20     (libsource (lib \"Mystery\") (part \"MYSTERY-IC\")))\n"
        );
    }
    format!(
        "(export (version \"E\")\n  (components\n\
         \x20   (comp (ref \"R1\") (value \"390\") (footprint \"Resistor_SMD:R_0603_1608Metric\"))\n\
         \x20   (comp (ref \"D1\") (value \"HBTEST-LOCAL\") (footprint \"Diode_SMD:D_SOD-123\"))\n\
         {comps}\
         \x20 )\n\
         \x20 (nets\n{net_block}))\n"
    )
}

/// Run the diode board with `open` unidentified parts at `ambient_c` with
/// `asserts` (TOML `[[assert]]` blocks).
fn run_with(open: &[OpenPart], ambient_c: f64, asserts: &str) -> hauksbee_ci::CiResult {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("models")).expect("models dir");
    std::fs::write(dir.path().join("models/local.toml"), CARD).expect("card");
    std::fs::write(dir.path().join("board.net"), board(open)).expect("board");
    let spec = dir.path().join("spec.toml");
    let body = format!(
        "name = \"local\"\nboard = \"board.net\"\nmodels_dir = \"models\"\nduration_ms = 2\n\
         ambient_c = {ambient_c:.1}\n\
         [[supply]]\nnet = \"VCC\"\nkind = \"ideal\"\nvolts = 5.0\n{asserts}"
    );
    std::fs::write(&spec, body).expect("spec");
    hauksbee_ci::run(&crate::support::run_config(spec)).expect("spec runs")
}

/// Run the board with the passive part `U9` across `u9_nets`.
fn run(u9_nets: (&str, &str), ambient_c: f64, asserts: &str) -> (hauksbee_ci::CiResult, Value) {
    let u9 = ("U9", [(u9_nets.0, "passive"), (u9_nets.1, "passive")]);
    let result = run_with(&[u9], ambient_c, asserts);
    let json = serde_json::from_str(&result.render_json()).expect("CI JSON parses");
    (result, json)
}

const NO_FAULTS: &str = "[[assert]]\nkind = \"no_faults\"\n";

/// Fault: the overstress on D1 is found with an unrelated open part on the
/// board, so it is a failure that names D1, not "not judged".
#[test]
fn a_found_overstress_stands_beside_an_unrelated_open_part() {
    let (result, _) = run(("X", "GND"), 70.0, NO_FAULTS);
    let row = &result.results[0];
    assert!(
        !row.passed && !row.invalid,
        "a found overstress on a modelled part is a failure: {}",
        result.render_human()
    );
    assert_eq!(result.exit_code(), 1, "{}", result.render_human());
    assert!(
        row.detail.contains("D1") || row.why.as_deref().is_some_and(|w| w.contains("D1")),
        "the failure names the overstressed part: {}",
        row.detail
    );
}

/// Control: the same open part on the diode's own net could carry or shunt
/// its current, so the found overstress there is not judged.
#[test]
fn an_open_part_on_the_overstressed_parts_net_still_undermines_it() {
    let (result, _) = run(("A", "GND"), 70.0, NO_FAULTS);
    assert!(result.results[0].invalid, "{}", result.render_human());
    assert_eq!(result.exit_code(), 3, "{}", result.render_human());
}

/// Control: with no fault found, a board-wide stress check over an open part
/// is never a pass, wherever the open part is.
#[test]
fn a_clean_board_wide_stress_check_over_an_open_part_is_never_a_pass() {
    for nets in [("X", "GND"), ("A", "GND")] {
        let (result, _) = run(nets, 25.0, NO_FAULTS);
        assert!(
            result.results[0].invalid,
            "{nets:?}: {}",
            result.render_human()
        );
        assert_eq!(result.exit_code(), 3, "{nets:?}: {}", result.render_human());
    }
}

/// The run's refusal names the open part on the INVALID assertion's path, not
/// an unrelated one the run also carries. U9 is powered from VCC and can drive
/// the anode, so a voltage claim on it is not judged; U8 sits on a net the
/// diode circuit never touches.
#[test]
fn the_refusal_names_the_open_part_on_the_path_not_an_unrelated_one() {
    let voltage = "[[assert]]\nname = \"anode up\"\nkind = \"voltage\"\nnet = \"A\"\nmin = 0.1\n";
    let u9 = ("U9", [("A", "output"), ("VCC", "power_in")]);
    let u8 = ("U8", [("X", "passive"), ("GND", "passive")]);
    let result = run_with(&[u9, u8], 25.0, voltage);
    assert!(result.results[0].invalid, "{}", result.render_human());
    assert_eq!(result.exit_code(), 3, "{}", result.render_human());
    let missing = result.refusal().expect("refused").missing_prerequisite;
    assert!(missing.contains("U9"), "{missing}");
    assert!(
        !missing.contains("U8"),
        "U8 is not on the anode's path: {missing}"
    );
}
