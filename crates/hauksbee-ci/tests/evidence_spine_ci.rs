//! Production acceptance tests for W1's evidence spine in `hauksbee-ci`.
//!
//! These deliberately enter through `hauksbee_ci::run`: type-level evidence
//! tests do not prove that the spec runner attaches the evidence to the
//! assertion a pipeline actually gates on.

use std::path::{Path, PathBuf};

use hauksbee_ci::{run, RunConfig};
use serde_json::Value;

fn fixture_board() -> &'static str {
    r#"(export (version "E")
  (design (source "evidence-spine CI fixture"))
  (components
    (comp (ref "R1")
      (value "1k")
      (footprint "Resistor_SMD:R_0402_1005Metric")
      (libsource (lib "Device") (part "R"))))
  (nets
    (net (code "1") (name "RAIL")
      (node (ref "R1") (pin "1") (pintype "passive")))
    (net (code "2") (name "GND")
      (node (ref "R1") (pin "2") (pintype "passive")))))
"#
}

fn write_spec(dir: &Path, name: &str, min_v: f64) -> PathBuf {
    std::fs::write(dir.join("board.net"), fixture_board()).unwrap();
    let spec = dir.join(format!("{name}.toml"));
    std::fs::write(
        &spec,
        format!(
            "name = \"{name}\"\nboard = \"board.net\"\nduration_ms = 2\n\n\
             [[supply]]\nnet = \"RAIL\"\nkind = \"ideal\"\nvolts = 3.3\n\n\
             [[assert]]\nname = \"RAIL stays up\"\nkind = \"voltage\"\nnet = \"RAIL\"\nmin = {min_v}\n"
        ),
    )
    .unwrap();
    spec
}

fn run_json(spec: PathBuf) -> (hauksbee_ci::CiResult, Value) {
    let result = run(&RunConfig {
        spec,
        ..Default::default()
    })
    .expect("the production spec runner completes");
    let json = serde_json::from_str(&result.render_json()).expect("CI JSON parses");
    (result, json)
}

#[test]
fn production_numeric_assertion_carries_inventory_causal_map_and_error_budget() {
    let dir = tempfile::tempdir().unwrap();
    let (result, json) = run_json(write_spec(dir.path(), "numeric", 3.0));

    assert_eq!(result.exit_code(), 0, "{}", result.render_human());
    let inventory = json["inventory"].as_array().expect("run inventory");
    assert!(
        inventory.iter().any(|a| a["role"] == "netlist"),
        "the exact board input is inventoried: {inventory:?}"
    );
    assert!(
        inventory.iter().any(|a| a["role"] == "spec"),
        "the spec that defined the assertion is causal input: {inventory:?}"
    );

    let evidence = json["results"][0]["evidence"]
        .as_object()
        .expect("each production result carries its own evidence map");
    assert_eq!(evidence["assertion"], "RAIL stays up");
    assert_eq!(evidence["status"], "qualified");
    assert!(
        evidence["error_budget"].is_object(),
        "every production numeric result carries its numerical budget: {evidence:?}"
    );
    assert!(
        evidence["error_budget"]["residual"]["max_abs"]
            .as_f64()
            .is_some_and(f64::is_finite),
        "the production transient runner propagates its measured residual: {evidence:?}"
    );
    assert!(
        evidence["artifacts"]
            .as_array()
            .is_some_and(|a| a.len() >= 2),
        "the assertion cites board and spec artifacts: {evidence:?}"
    );
    let models = evidence["models"]
        .as_array()
        .expect("the causal model path is machine readable");
    assert!(
        models.iter().any(|model| {
            model["reference"] == "R1"
                && model["source"]["tier"] == "curated-library"
                && model["source"]["uncertainty"][0]["status"] == "unknown"
        }),
        "the canonical source record reaches CI JSON: {models:?}"
    );
    let human = result.render_human();
    assert!(human.contains("source=curated-library"), "{human}");
    assert!(human.contains("uncertainty unknown"), "{human}");
    assert!(
        human.contains("error budget:") && human.contains("residual="),
        "human CI output renders the same numerical qualification as JSON: {human}"
    );
}

#[test]
fn one_typed_assumption_vocabulary_reaches_every_ci_renderer() {
    let dir = tempfile::tempdir().unwrap();
    let (result, json) = run_json(write_spec(dir.path(), "ideal-source", 3.0));
    let assumption = json["assumptions"]
        .as_array()
        .and_then(|items| items.iter().find(|a| a["kind"] == "reduced_fidelity"))
        .expect("the production hollow gate is a typed assumption");
    let id = assumption["id"].as_str().unwrap();
    assert!(id.starts_with("reduced-fidelity:"), "{assumption:?}");

    let rendered = [
        result.render_human(),
        result.render_json(),
        result.render_junit(),
        result.render_github_annotations(),
    ];
    for surface in rendered {
        assert!(
            surface.contains(id),
            "the canonical assumption id must survive every CI renderer: {surface}"
        );
        assert!(
            surface.contains("held by an ideal source"),
            "renderers reuse the constructor-composed wording: {surface}"
        );
    }
}

#[test]
fn undermined_evidence_invalidates_the_run_without_erasing_the_check_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let spec = write_spec(dir.path(), "undermined", 3.0);
    std::fs::write(
        dir.path().join("board.net"),
        r#"(export (version "E")
  (design (source "undermined evidence fixture"))
  (components
    (comp (ref "U1")
      (value "UNKNOWN_REGULATOR")
      (footprint "Package_SO:SOIC-8")
      (libsource (lib "Regulator_Linear") (part "UNKNOWN_REGULATOR"))))
  (nets
    (net (code "1") (name "RAIL")
      (node (ref "U1") (pin "1") (pintype "power_in")))
    (net (code "2") (name "GND")
      (node (ref "U1") (pin "2") (pintype "power_in")))))
"#,
    )
    .unwrap();

    let (result, json) = run_json(spec);
    assert!(
        result.results[0].passed,
        "the voltage check itself still held: {}",
        result.results[0].detail
    );
    assert!(result.results[0].invalid, "its evidence is invalid");
    assert_eq!(result.exit_code(), 3, "the run must not be green");
    assert_eq!(json["assertions_passed"], true);
    assert_eq!(json["run_valid"], false);
    assert_eq!(json["passed"], false);
}

#[test]
fn a_live_waiver_is_evidence_on_the_exact_assertion_it_authorized() {
    let dir = tempfile::tempdir().unwrap();
    let spec = write_spec(dir.path(), "waived", 100.0);
    let mut text = std::fs::read_to_string(&spec).unwrap();
    text.push_str(
        "\n[[assert]]\nname = \"RAIL ordinary floor\"\nkind = \"voltage\"\nnet = \"RAIL\"\nmin = 3.0\n",
    );
    std::fs::write(&spec, text).unwrap();
    std::fs::write(
        dir.path().join("hauksbee-waivers.toml"),
        r#"[[waive]]
check = "ci"
kind = "voltage"
nets = ["RAIL"]
reason = "bench-confirmed during staged rollout"
until = "2030-01-01"
"#,
    )
    .unwrap();

    let (result, json) = run_json(spec);
    assert_eq!(result.exit_code(), 0, "an active waiver remains non-gating");
    let evidence = &json["results"][0]["evidence"];
    assert_eq!(evidence["status"], "qualified", "{evidence:?}");
    let ids = evidence["assumptions"].as_array().expect("assumption ids");
    let waiver_id = ids
        .iter()
        .filter_map(Value::as_str)
        .find(|id| id.starts_with("waived:"))
        .expect("the applied waiver is on the authorized assertion's path");
    assert!(result.render_human().contains(waiver_id));
    assert!(result.render_junit().contains(waiver_id));
    assert!(result.render_github_annotations().contains(waiver_id));
    let sibling = &json["results"][1]["evidence"];
    assert_eq!(sibling["status"], "qualified", "{sibling:?}");
    assert!(
        sibling["assumptions"]
            .as_array()
            .is_none_or(|ids| !ids.iter().any(|id| id == waiver_id)),
        "the waiver fact must not saturate a sibling assertion on the same net: {sibling:?}"
    );
    assert!(
        json["inventory"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["role"] == "waivers"),
        "the authorization file is inventoried too"
    );
}
