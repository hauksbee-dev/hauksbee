//! The editor-schema drift test. The VS Code extension validates spec TOML
//! against `editors/vscode-hauksbee-board/schemas/hauksbee-ci-spec.schema.json`,
//! and that file is GENERATED from the Rust [`Spec`] type via schemars: the
//! serde derives (deny_unknown_fields, defaults, renames) and the doc comments
//! on each field ARE the schema, so adding a field to `Spec` updates the
//! schema instead of silently drifting from it. Same coupling pattern as
//! `hauksbee-ir/tests/compat_drift.rs` for the SPICE compatibility doc.
//!
//! * `schema_file_matches_spec_type`: after normalizing checkout line endings,
//!   the checked-in file equals what the types generate, byte for byte.
//!   Regenerate after editing `Spec` (or any type it contains) with:
//!       UPDATE_SPEC_SCHEMA=1 cargo test -p hauksbee-ci --test schema_drift
//! * `generated_schema_is_strict_and_documented`: the generator actually
//!   carries the properties the editor experience depends on (doc comments as
//!   hover descriptions, deny_unknown_fields as additionalProperties: false),
//!   so a schemars regression cannot quietly ship an empty-but-matching file.

use hauksbee_ci::Spec;
use schemars::generate::SchemaSettings;
use serde_json::{json, Value};
use std::path::PathBuf;

/// The published `$id`; kept from the original hand-written schema.
const SCHEMA_ID: &str = "https://github.com/hauksbee-dev/hauksbee/editors/vscode-hauksbee-board/schemas/hauksbee-ci-spec.schema.json";

const REGEN: &str = "UPDATE_SPEC_SCHEMA=1 cargo test -p hauksbee-ci --test schema_drift";

/// Render the schema exactly as the checked-in file must contain it.
fn generated_schema() -> String {
    // Draft-07 because the consumer is Even Better TOML / taplo via the
    // extension's `tomlValidation` contribution, and the hand-written schema
    // this replaces already published draft-07.
    let mut schema = SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<Spec>();
    let obj = schema.ensure_object();
    // Root metadata follows the original file's conventions; schemars would
    // otherwise title it "Spec" and describe it with the struct's one-liner.
    obj.insert(
        "$schema".into(),
        json!("http://json-schema.org/draft-07/schema#"),
    );
    obj.insert("$id".into(), json!(SCHEMA_ID));
    obj.insert("title".into(), json!("hauksbee-ci spec"));
    obj.insert(
        "description".into(),
        json!(
            "A hauksbee-ci TOML spec: one headless board+firmware co-simulation and the \
             assertions that must hold for the build to pass. GENERATED from `Spec` in \
             crates/hauksbee-ci/src/spec.rs (doc comments become these descriptions; \
             serde deny_unknown_fields becomes additionalProperties: false). Do not \
             hand-edit; regenerate with: UPDATE_SPEC_SCHEMA=1 cargo test -p hauksbee-ci \
             --test schema_drift"
        ),
    );
    // `assert` is required in practice (Spec::validate rejects an empty list),
    // but the field carries a serde default so load() can produce the friendly
    // "no [[assert]] blocks" error, and schemars never marks a defaulted field
    // required. Assert it into the schema here.
    let required = obj
        .get_mut("required")
        .and_then(Value::as_array_mut)
        .expect("Spec has required fields (board)");
    required.push(json!("assert"));
    required.sort_by(|a, b| a.as_str().cmp(&b.as_str()));

    let text = serde_json::to_string_pretty(&schema).expect("schema serializes");
    // rustdoc preserves CRLF from a Windows checkout in derived field
    // descriptions. Canonicalize the encoded newlines so one checked-in schema
    // is byte-identical on every supported host.
    let mut text = canonical_schema_newlines(text);
    text.push('\n');
    text
}

fn canonical_schema_newlines(text: String) -> String {
    // Git for Windows can materialize the checked-in JSON with physical CRLF
    // line endings. Separately, rustdoc can preserve CRLF from Rust source as
    // the encoded characters `\r\n` inside generated descriptions. Normalize
    // both representations while leaving all other schema bytes significant.
    text.replace("\r\n", "\n").replace(r"\r\n", r"\n")
}

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../editors/vscode-hauksbee-board/schemas/hauksbee-ci-spec.schema.json")
}

#[test]
fn schema_file_matches_spec_type() {
    let path = schema_path();
    let want = generated_schema();
    if std::env::var("UPDATE_SPEC_SCHEMA").is_ok() {
        std::fs::write(&path, &want)
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
        eprintln!("regenerated {}", path.display());
        return;
    }
    let current = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}\nRegenerate with: {REGEN}",
            path.display()
        )
    });
    let current = canonical_schema_newlines(current);
    assert!(
        current == want,
        "editors/vscode-hauksbee-board/schemas/hauksbee-ci-spec.schema.json is out of \
         sync with the `Spec` type in crates/hauksbee-ci/src/spec.rs (the file is \
         generated, not hand-maintained).\n\
         Regenerate with: {REGEN}"
    );
}

#[test]
fn generated_schema_is_strict_and_documented() {
    let schema: Value = serde_json::from_str(&generated_schema()).expect("valid JSON");

    // deny_unknown_fields on Spec must surface as additionalProperties: false,
    // top-level and on every nested table the derive covers.
    assert_eq!(schema["additionalProperties"], json!(false));
    for def in ["Assertion", "SupplySpec", "PeripheralSpec", "Scenario"] {
        assert_eq!(
            schema["definitions"][def]["additionalProperties"],
            json!(false),
            "{def} must reject unknown keys like the serde type does"
        );
    }

    // Doc comments must arrive as descriptions: that is the hover text the
    // editor shows, and the reason the schema is generated from the types.
    let props = &schema["properties"];
    for field in [
        "board",
        "bom",
        "bom_columns",
        "placement",
        "variant",
        "fit",
        "no_fit",
        "dnp",
        "duration_ms",
    ] {
        let d = props[field]["description"].as_str().unwrap_or("");
        assert!(
            !d.is_empty(),
            "`{field}` lost its doc comment; the schema would have no hover text"
        );
    }

    // The DNP trio is the drift this test was born from: the three fields and
    // the kebab-case DnpMode tokens must be present.
    let dnp = serde_json::to_string(&schema["definitions"]["DnpMode"]).unwrap();
    for token in ["fit-except-links", "fit-all", "honour"] {
        assert!(dnp.contains(token), "DnpMode is missing `{token}`: {dnp}");
    }

    // A spec needs a board and at least one assertion; keep the editor telling
    // users that before the CLI has to.
    let required = schema["required"].as_array().expect("required list");
    for f in ["board", "assert"] {
        assert!(required.iter().any(|v| v == f), "`{f}` must be required");
    }
    assert_eq!(props["assert"]["minItems"], json!(1));
}

#[test]
fn generated_schema_is_independent_of_checkout_line_endings() {
    let encoded = r#"{"description":"first\r\nsecond"}"#.to_string();
    assert_eq!(
        canonical_schema_newlines(encoded),
        r#"{"description":"first\nsecond"}"#
    );
    let physical = "{\r\n  \"description\": \"first\"\r\n}\r\n".to_string();
    assert_eq!(
        canonical_schema_newlines(physical),
        "{\n  \"description\": \"first\"\n}\n"
    );
    assert!(!generated_schema().contains(r"\r\n"));
}
