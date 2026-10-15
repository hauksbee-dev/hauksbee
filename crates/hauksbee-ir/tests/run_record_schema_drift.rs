//! Drift tests for the two generated contracts this crate publishes:
//!
//! * `schemas/hauksbee-run-record.schema.json`, GENERATED from [`RunRecord`];
//! * `schemas/hauksbee-vocabulary.json`, the serialized [`vocabulary()`]: the
//!   data `explain` prints and the app reads.
//!
//! Same pattern as `hauksbee-ci/tests/ci_report_schema_drift.rs`: the checked-in
//! file must equal what the types generate, byte for byte, and a real record
//! must validate against the generated schema (a custom `Serialize`, a
//! `flatten` or a skip condition can split the two). Regenerate with:
//!     `UPDATE_RUN_RECORD_SCHEMA=1` cargo test -p hauksbee-ir --test it `run_record_schema_drift`

use std::path::PathBuf;

use hauksbee_ir::run_record::{RunRecord, RUN_RECORD_SCHEMA};
use hauksbee_ir::vocabulary::vocabulary;
use schemars::generate::SchemaSettings;
use serde_json::{json, Value};

const REGEN: &str =
    "UPDATE_RUN_RECORD_SCHEMA=1 cargo test -p hauksbee-ir --test it run_record_schema_drift";

fn schemas_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas")
}

fn generated_schema() -> String {
    // Draft-07, the dialect of every other schema the workspace publishes.
    let mut schema = SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<RunRecord>();
    let obj = schema.ensure_object();
    obj.insert(
        "$schema".into(),
        json!("http://json-schema.org/draft-07/schema#"),
    );
    obj.insert("$id".into(), json!(RUN_RECORD_SCHEMA));
    obj.insert("title".into(), json!("hauksbee run record"));
    obj.insert(
        "description".into(),
        json!(format!(
            "{RUN_RECORD_SCHEMA}: one run of hauksbee or hauksbee-ci, the single document \
             every report renders from. GENERATED from `RunRecord` in \
             crates/hauksbee-ir/src/run_record.rs. Do not hand-edit; regenerate with: {REGEN}"
        )),
    );
    let mut text = serde_json::to_string_pretty(obj).expect("schema serializes");
    text.push('\n');
    text
}

fn generated_vocabulary() -> String {
    let mut text = serde_json::to_string_pretty(&vocabulary()).expect("vocabulary serializes");
    text.push('\n');
    text
}

/// Compare a checked-in generated file, or rewrite it under the regen flag.
fn check_file(name: &str, expected: &str) {
    let path = schemas_dir().join(name);
    if std::env::var_os("UPDATE_RUN_RECORD_SCHEMA").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, expected).unwrap();
        eprintln!("regenerated {}", path.display());
        return;
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert!(
        on_disk == expected,
        "crates/hauksbee-ir/schemas/{name} drifted from the types (it is generated, not \
         hand-maintained); regenerate with: {REGEN}\n--- generated {name} ---\n{expected}--- end ---"
    );
}

#[test]
fn schema_file_matches_record_types() {
    check_file("hauksbee-run-record.schema.json", &generated_schema());
}

#[test]
fn vocabulary_file_matches_vocabulary() {
    check_file("hauksbee-vocabulary.json", &generated_vocabulary());
}

#[test]
fn emitted_record_validates_against_the_schema() {
    let schema: Value = serde_json::from_str(&generated_schema()).unwrap();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft7)
        .build(&schema)
        .expect("the generated schema compiles as draft-07");
    let mut record = super::run_record_tests::sample();
    record.sign("A. Reviewer", "2026-10-16T10:00:00Z", "Approved.");
    let value = serde_json::to_value(&record).unwrap();
    let errors: Vec<String> = validator
        .iter_errors(&value)
        .map(|e| e.to_string())
        .collect();
    assert!(errors.is_empty(), "{errors:#?}");

    // A manifest without its identity keys is refused.
    let mut bad = value;
    bad["manifest"] = json!({"argv": []});
    assert!(!validator.is_valid(&bad));
}
