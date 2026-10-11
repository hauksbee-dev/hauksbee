//! A card's temperature-tagged rating rows reach the verdict end to end: a
//! user card bound through the binder, run by the spec runner at the ambient
//! the spec states, and a defaulted ambient that decides the rating qualifies
//! the clean `no_faults` it produced. Rows printed per mounting are read at
//! the least favourable one until the spec or the board names one.

use crate::support::assert_row_green;
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

/// [`CARD`] with both rows printed at 25 °C for two mountings instead: 8 mA
/// on a minimum pad and 12 mA on a square inch of copper.
fn mounted_card() -> String {
    let at = |m: &str| format!("conditions = \"TA = 25 °C\"\nmounting = \"{m}\"");
    let pad = CARD.replacen("conditions = \"TA = 70 °C\"", &at("minimum pad"), 1);
    pad.replacen("conditions = \"TA = 25 °C\"", &at("1 in2 copper"), 1)
}

/// `card` and [`BOARD`] run by a spec opening with `tail`.
fn run_with(card: &str, tail: &str) -> (hauksbee_ci::CiResult, Value) {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(dir.path().join("models")).expect("models dir");
    std::fs::write(dir.path().join("models/derated.toml"), card).expect("card");
    std::fs::write(dir.path().join("board.net"), BOARD).expect("board");
    let spec = dir.path().join("spec.toml");
    let body = format!(
        "name = \"derated\"\nboard = \"board.net\"\nmodels_dir = \"models\"\nduration_ms = 2\n{tail}\n\
         [[supply]]\nnet = \"VCC\"\nkind = \"ideal\"\nvolts = 5.0\n[[assert]]\nkind = \"no_faults\"\n"
    );
    std::fs::write(&spec, body).expect("spec");
    let result = hauksbee_ci::run(&crate::support::run_config(spec)).expect("spec runs");
    let json = serde_json::from_str(&result.render_json()).expect("CI JSON parses");
    (result, json)
}

/// How many of the run's assumption ids contain `needle`.
fn ids(json: &Value, needle: &str) -> usize {
    let all = json["results"][0]["evidence"]["assumptions"].as_array();
    let ids = all.into_iter().flatten().filter_map(Value::as_str);
    ids.filter(|id| id.contains(needle)).count()
}

#[test]
fn the_rating_row_for_the_stated_ambient_decides_and_a_default_is_disclosed() {
    let ambient_ids = |json: &Value| ids(json, "ambient_c");
    let (cool, json) = run_with(CARD, "ambient_c = 25.0");
    assert_row_green(
        &cool.results[0],
        &format!(
            "11 mA is under the 12 mA row at 25 C: {}",
            cool.render_human()
        ),
    );
    assert_eq!(ambient_ids(&json), 0, "a stated ambient is no assumption");

    let (hot, _) = run_with(CARD, "ambient_c = 70.0");
    // Deliberate negative: the numeric red at 70 C.
    assert!(
        !hot.results[0].passed,
        "11 mA is over the 8 mA row at 70 C: {}",
        hot.render_human()
    );

    let (defaulted, json) = run_with(CARD, "");
    assert_row_green(&defaulted.results[0], &defaulted.render_human());
    assert_eq!(
        json["results"][0]["evidence"]["status"], "qualified",
        "{json:#}"
    );
    assert!(
        ambient_ids(&json) > 0,
        "the defaulted ambient qualifies the pass: {json:#}"
    );
}

/// 11 mA fails the minimum-pad row nobody chose, passes the copper row an
/// `[[override]]` names (in any case) with no caveat, and a mounting the card
/// does not rate leaves the run no verdict.
#[test]
fn the_least_favourable_mounting_decides_until_the_spec_names_one() {
    let over = |m: &str| format!("[[override]]\nref = \"D1\"\nmounting = \"{m}\"");
    #[rustfmt::skip]
    let cases = [
        // (spec tail, passes, evidence status of the result, mounting ids)
        (String::new(), false, None, 1),
        (over("1 IN2 Copper "), true, None, 0),
        (over("2 in2 copper"), false, Some("undermined"), 1),
    ];
    for (tail, passes, status, said) in cases {
        let (result, json) = run_with(&mounted_card(), &tail);
        let why = result.render_human();
        assert_eq!(result.results[0].passed, passes, "{tail}: {why}");
        if passes {
            assert_row_green(&result.results[0], &format!("{tail}: {why}"));
        }
        let got = json["results"][0]["evidence"]["status"].as_str();
        assert!(status.is_none_or(|s| got == Some(s)), "{tail}: {json:#}");
        assert_eq!(ids(&json, "mounting"), said, "{tail}: {json:#}");
    }
}

/// The live stream carries the same disclosure as a note: the default
/// mounting on a bare board, none once the board's own property names it.
#[test]
fn the_live_frame_notes_the_mounting_it_defaulted() {
    use hauksbee_engine::{bind_board, HauksbeeEngine};
    use hauksbee_frontdoor_api::engine::Engine;
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("derated.toml"), mounted_card()).expect("card");
    let lib = hauksbee_models::ModelLibrary::builtin_with_user_dirs(&[dir.path()]);
    let chosen = r#"(footprint "Diode_SMD:D_SOD-123") (property (name "hauksbee_mounting") (value "1 in2 copper")))"#;
    let named = BOARD.replacen(r#"(footprint "Diode_SMD:D_SOD-123"))"#, chosen, 1);
    for (board, said) in [(BOARD.to_string(), true), (named, false)] {
        let board = hauksbee_extract::ExtractedBoard::from_auto(&board).expect("board");
        let engine = HauksbeeEngine::from_bound(bind_board(&board, &lib), None, "/live");
        let mut engine = engine.expect("engine");
        let notes = engine.step(1e-4).notes;
        let hit = notes.iter().any(|n| n.contains("D1.mounting"));
        assert_eq!(hit, said, "{notes:?}");
    }
}
