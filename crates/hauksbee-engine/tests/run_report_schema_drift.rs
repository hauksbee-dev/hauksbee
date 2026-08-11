//! Drift test for the `hauksbee run --json` output schema. The document is
//! produced by `JsonReport::to_json`, which serializes the `JsonReport` type
//! and prepends the `ok`/`verdict`/`serious_count`/`actionable_count` rollup;
//! the schema is GENERATED from the type (plus those four injected rollup
//! properties), so adding a field updates the schema instead of silently
//! drifting from the checked-in file. Same coupling pattern as
//! `hauksbee-ci/tests/schema_drift.rs` for the CI-spec input schema.
//!
//! Regenerate after changing `JsonReport` (or anything it contains) with:
//!     UPDATE_RUN_SCHEMA=1 cargo test -p hauksbee-engine --test run_report_schema_drift

use hauksbee_engine::result::{JsonReport, RUN_REPORT_SCHEMA_VERSION};
use schemars::generate::SchemaSettings;
use serde_json::json;
use std::path::PathBuf;

const REGEN: &str =
    "UPDATE_RUN_SCHEMA=1 cargo test -p hauksbee-engine --test run_report_schema_drift";

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/hauksbee-run-report.schema.json")
}

/// Render the schema exactly as the checked-in file must contain it.
fn generated_schema() -> String {
    let mut schema = SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<JsonReport>();
    let obj = schema.ensure_object();
    obj.insert(
        "$schema".into(),
        json!("http://json-schema.org/draft-07/schema#"),
    );
    obj.insert("title".into(), json!("hauksbee run --json report"));
    obj.insert(
        "description".into(),
        json!(format!(
            "The document `hauksbee run <board> --json` (and the per-report --json \
             surfaces) emits. schema_version {RUN_REPORT_SCHEMA_VERSION}. GENERATED from \
             `JsonReport` in crates/hauksbee-engine/src/result.rs plus the \
             ok/verdict/serious_count/actionable_count rollup `to_json` prepends. Do not \
             hand-edit; regenerate with: {REGEN}"
        )),
    );
    // The rollup keys are prepended by `to_json`, not fields of the struct, so
    // they are injected here; the assertion test below keeps them honest.
    let props = obj
        .get_mut("properties")
        .and_then(|p| p.as_object_mut())
        .expect("root schema has properties");
    props.insert("ok".into(), json!({ "type": "boolean" }));
    props.insert(
        "verdict".into(),
        json!({ "type": "string", "enum": ["pass", "fail", "invalid"] }),
    );
    props.insert(
        "serious_count".into(),
        json!({ "type": "integer", "minimum": 0 }),
    );
    props.insert(
        "actionable_count".into(),
        json!({ "type": "integer", "minimum": 0 }),
    );
    for key in ["ok", "verdict", "serious_count", "actionable_count"] {
        let required = obj
            .get_mut("required")
            .and_then(|r| r.as_array_mut())
            .expect("root schema has required");
        if !required.iter().any(|v| v == key) {
            required.push(json!(key));
        }
    }
    let mut text = serde_json::to_string_pretty(obj).expect("schema serializes");
    text.push('\n');
    text
}

#[test]
fn schema_file_matches_report_type() {
    let expected = generated_schema();
    let path = schema_path();
    if std::env::var_os("UPDATE_RUN_SCHEMA").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &expected).unwrap();
        return;
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing {}: {e}; regenerate with: {REGEN}", path.display()));
    assert_eq!(
        on_disk, expected,
        "schemas/hauksbee-run-report.schema.json drifted from JsonReport; regenerate with: {REGEN}"
    );
}

#[test]
fn emitted_document_carries_schema_version_and_rollup() {
    // A minimal report must carry schema_version = the published constant and
    // the four rollup keys the schema promises.
    let report = JsonReport::new(
        "board",
        hauksbee_engine::result::BindSummary::from_report(&Default::default()),
    );
    let v: serde_json::Value = serde_json::from_str(&report.to_json()).expect("one JSON document");
    assert_eq!(v["schema_version"], RUN_REPORT_SCHEMA_VERSION);
    assert_eq!(
        RUN_REPORT_SCHEMA_VERSION, 4,
        "the closed artifact-kind enum gained eagle_schematic in schema v4"
    );
    for key in [
        "ok",
        "verdict",
        "serious_count",
        "actionable_count",
        "board",
        "bind",
    ] {
        assert!(v.get(key).is_some(), "missing rollup/header key {key}");
    }
}

#[test]
fn schema_v2_keeps_the_additive_gating_field_optional_for_older_v2_documents() {
    let schema: serde_json::Value =
        serde_json::from_str(&generated_schema()).expect("generated schema parses");
    let finding = &schema["definitions"]["JsonFinding"];
    assert_eq!(finding["properties"]["gating"]["type"], "boolean");
    assert!(
        !finding["required"]
            .as_array()
            .expect("JsonFinding required list")
            .iter()
            .any(|field| field == "gating"),
        "an additive field in schema v2 cannot invalidate earlier v2 documents"
    );
}

#[test]
fn schema_v4_versions_the_closed_artifact_enum_without_rejecting_v3_numbers() {
    let schema: serde_json::Value =
        serde_json::from_str(&generated_schema()).expect("generated schema parses");
    let kinds = schema["definitions"]["ArtifactKind"]["enum"]
        .as_array()
        .expect("ArtifactKind is a closed string enum");
    assert!(
        kinds.iter().any(|kind| kind == "eagle_schematic"),
        "v4 must publish the new closed-enum value"
    );
    let version = &schema["properties"]["schema_version"];
    assert_eq!(version["type"], "integer");
    assert!(
        version.get("const").is_none(),
        "a v4-aware schema must still accept the schema_version number carried by older documents"
    );
}
