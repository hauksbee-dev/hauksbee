//! A card's temperature-tagged rating rows reach the verdict end to end: a
//! user card bound through the binder, run by the spec runner at the ambient
//! the spec states, and a defaulted ambient that decides the rating qualifies
//! the clean `no_faults` it produced.

use serde_json::Value;

/// A diode rated 12 mA at TA = 25 °C and 8 mA at 70 °C (scalar: the 8 mA row).
const CARD: &str = r#"[[models]]
id = "hbtest_derated_diode"
kind = "diode"
description = "Test-only diode whose current rating falls with ambient"
[models.match]
value_re = "(?i)^HBTEST-DERATED$"
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

/// 5 V through 390 Ω into the diode: about 11 mA.
const BOARD: &str = r#"(export (version "E")
  (components
    (comp (ref "R1") (value "390") (footprint "Resistor_SMD:R_0603_1608Metric"))
    (comp (ref "D1") (value "HBTEST-DERATED") (footprint "Diode_SMD:D_SOD-123")))
  (nets
    (net (code "1") (name "VCC") (node (ref "R1") (pin "1")))
    (net (code "2") (name "A") (node (ref "R1") (pin "2")) (node (ref "D1") (pin "2")))
    (net (code "3") (name "GND") (node (ref "D1") (pin "1")))))
"#;

fn run(ambient: &str) -> (hauksbee_ci::CiResult, Value) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("models")).expect("models dir");
    std::fs::write(dir.path().join("models/derated.toml"), CARD).expect("card");
    std::fs::write(dir.path().join("board.net"), BOARD).expect("board");
    let spec = dir.path().join("spec.toml");
    let body = format!(
        "name = \"derated\"\nboard = \"board.net\"\nmodels_dir = \"models\"\nduration_ms = 2\n{ambient}\n\
         [[supply]]\nnet = \"VCC\"\nkind = \"ideal\"\nvolts = 5.0\n[[assert]]\nkind = \"no_faults\"\n"
    );
    std::fs::write(&spec, body).expect("spec");
    let result = hauksbee_ci::run(&crate::support::run_config(spec)).expect("spec runs");
    let json = serde_json::from_str(&result.render_json()).expect("CI JSON parses");
    (result, json)
}

#[test]
fn the_rating_row_for_the_stated_ambient_decides_and_a_default_is_disclosed() {
    let ambient_ids = |json: &Value| {
        let ids = json["results"][0]["evidence"]["assumptions"]
            .as_array()
            .cloned();
        let ids = ids
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| id.as_str().map(String::from));
        ids.filter(|id| id.contains("ambient_c"))
            .collect::<Vec<_>>()
    };
    let (cool, json) = run("ambient_c = 25.0");
    assert!(
        cool.results[0].passed,
        "11 mA is under the 12 mA row at 25 C: {}",
        cool.render_human()
    );
    assert!(
        ambient_ids(&json).is_empty(),
        "a stated ambient is no assumption"
    );

    let (hot, _) = run("ambient_c = 70.0");
    assert!(
        !hot.results[0].passed,
        "11 mA is over the 8 mA row at 70 C: {}",
        hot.render_human()
    );

    let (defaulted, json) = run("");
    assert!(defaulted.results[0].passed, "{}", defaulted.render_human());
    assert_eq!(
        json["results"][0]["evidence"]["status"], "qualified",
        "{json:#}"
    );
    assert!(
        !ambient_ids(&json).is_empty(),
        "the defaulted ambient qualifies the pass: {json:#}"
    );
}
