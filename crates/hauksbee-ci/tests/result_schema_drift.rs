//! Drift gate for the published `hauksbee-ci run --json` document.

use hauksbee_ci::report::CiJsonReport;
use schemars::generate::SchemaSettings;
use serde_json::json;
use std::path::PathBuf;

const SCHEMA_ID: &str =
    "https://github.com/hauksbee-dev/hauksbee/crates/hauksbee-ci/schemas/hauksbee-ci-result.schema.json";
const REGEN: &str =
    "UPDATE_CI_RESULT_SCHEMA=1 cargo test -p hauksbee-ci --test result_schema_drift";

fn generated_schema() -> String {
    let mut schema = SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<CiJsonReport>();
    let object = schema.ensure_object();
    object.insert(
        "$schema".into(),
        json!("http://json-schema.org/draft-07/schema#"),
    );
    object.insert("$id".into(), json!(SCHEMA_ID));
    object.insert("title".into(), json!("hauksbee-ci run result"));
    object.insert(
        "description".into(),
        json!(
            "The stable JSON document emitted by `hauksbee-ci run --json`, including the exact input inventory, canonical assumption registry, and one causal evidence map per assertion. Generated from CiJsonReport; do not hand-edit."
        ),
    );
    let mut text = serde_json::to_string_pretty(&schema).expect("schema serializes");
    text.push('\n');
    text
}

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/hauksbee-ci-result.schema.json")
}

#[test]
fn checked_in_ci_result_schema_matches_the_rust_type() {
    let path = schema_path();
    let expected = generated_schema();
    if std::env::var("UPDATE_CI_RESULT_SCHEMA").is_ok() {
        std::fs::write(&path, &expected)
            .unwrap_or_else(|error| panic!("cannot write {}: {error}", path.display()));
        return;
    }
    let actual = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}\n{REGEN}", path.display()));
    assert_eq!(
        actual, expected,
        "CI result schema drifted; regenerate with {REGEN}"
    );
}

#[test]
fn schema_requires_evidence_at_run_and_assertion_levels() {
    let schema: serde_json::Value =
        serde_json::from_str(&generated_schema()).expect("generated schema parses");
    let required = schema["required"].as_array().expect("root required fields");
    for field in ["inventory", "assumptions", "evidence", "results"] {
        assert!(required.iter().any(|value| value == field), "{field}");
    }
    let assertion = &schema["definitions"]["CiJsonAssertion"];
    assert!(assertion["properties"].get("evidence").is_some());
}
