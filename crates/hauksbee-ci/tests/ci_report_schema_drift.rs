//! Drift test for the `hauksbee-ci run --json` output schema. The document is
//! produced by `CiResult::render_json`, which serializes [`CiJsonReport`] (or
//! [`CiJsonError`] when the spec never ran); the schema is GENERATED from
//! [`CiJsonLine`], the union of the two, so adding a field updates the schema
//! instead of silently drifting from the checked-in file. Same coupling pattern
//! as `hauksbee-engine/tests/run_report_schema_drift.rs` for the engine's run
//! report and `hauksbee-ci/tests/schema_drift.rs` for the CI-spec input schema.
//!
//! Regenerate after changing `CiJsonReport`, `CiJsonError`, or `AssertResult`
//! with:
//!     UPDATE_CI_REPORT_SCHEMA=1 cargo test -p hauksbee-ci --test ci_report_schema_drift
//!
//! Four tests, because a generated schema can be wrong in four different ways:
//!
//! * `schema_file_matches_report_types`: the checked-in file equals what the
//!   types generate, byte for byte.
//! * `generated_schema_pins_the_optionality`: the required lists say what is
//!   ALWAYS present and what a consumer may find absent. A schema that marks
//!   everything optional documents nothing, and one that claims a
//!   `skip_serializing_if` field is always there is worse than none.
//! * `emitted_bytes_validate_against_the_schema`: the real binary is run over
//!   real specs and every line of its stdout is validated. The derived schema
//!   and the emitted bytes can diverge through a custom `Serialize`, a
//!   `flatten`, or a skip condition, and the drift test alone would not notice.
//! * `refusal_and_abort_documents_validate`: exit 3 (invalid for analysis) is
//!   not reachable from any spec a person can write (the search and every
//!   closed route are in `tests/exit3_reachability.rs`), so its document is
//!   produced from a constructed `CiResult` through the same `render_json` the
//!   binary prints, and validated the same way.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use hauksbee_ci::assertions::AssertResult;
use hauksbee_ci::report::{CiJsonLine, CiResult, CI_REPORT_SCHEMA_VERSION};
use schemars::generate::SchemaSettings;
use serde_json::{json, Value};

const REGEN: &str =
    "UPDATE_CI_REPORT_SCHEMA=1 cargo test -p hauksbee-ci --test ci_report_schema_drift";

fn schema_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas/hauksbee-ci-report.schema.json")
}

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples")
}

/// Render the schema exactly as the checked-in file must contain it.
fn generated_schema() -> String {
    // Draft-07 to match the two schemas this crate and the engine already
    // publish (the spec-input schema and the run-report schema), so a consumer
    // needs one validator dialect for all three.
    let mut schema = SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<CiJsonLine>();
    let obj = schema.ensure_object();
    obj.insert(
        "$schema".into(),
        json!("http://json-schema.org/draft-07/schema#"),
    );
    obj.insert(
        "title".into(),
        json!("hauksbee-ci run --json output line"),
    );
    obj.insert(
        "description".into(),
        json!(format!(
            "One line of `hauksbee-ci run <spec>... --json`: a run report (`ok: true`) or a \
             spec/board error (`ok: false`). A multi-spec invocation prints one line per \
             spec (NDJSON) and may mix the two. schema_version \
             {CI_REPORT_SCHEMA_VERSION}. GENERATED from `CiJsonLine` in \
             crates/hauksbee-ci/src/report.rs and `AssertResult` in \
             crates/hauksbee-ci/src/assertions.rs. Do not hand-edit; regenerate with: {REGEN}"
        )),
    );
    // `why` and `waived` carry `skip_serializing_if`, so they are absent rather
    // than null. Pinning their type to a bare string (`schema_absent_or_string`)
    // says the "never null" half, but it also makes schemars treat them as
    // non-Option and therefore required, which is the opposite of the truth.
    // Drop them from `required` here; `generated_schema_pins_the_optionality`
    // is the assertion that keeps this honest in both directions.
    let required = obj["definitions"]["AssertResult"]["required"]
        .as_array_mut()
        .expect("AssertResult has required fields");
    required.retain(|f| f != "why" && f != "waived");

    let mut text = serde_json::to_string_pretty(obj).expect("schema serializes");
    text.push('\n');
    text
}

/// The compiled validator for the generated schema.
fn validator() -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(&generated_schema()).expect("schema is valid JSON");
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft7)
        .build(&schema)
        .expect("the generated schema compiles as draft-07")
}

/// Validate one emitted document, naming the shape when it fails.
fn assert_valid(v: &jsonschema::Validator, doc: &str, shape: &str) {
    let value: Value = serde_json::from_str(doc)
        .unwrap_or_else(|e| panic!("{shape}: emitted line is not JSON: {e}\n{doc}"));
    let errors: Vec<String> = v.iter_errors(&value).map(|e| e.to_string()).collect();
    assert!(
        errors.is_empty(),
        "{shape}: the bytes hauksbee-ci emits do not validate against the schema \
         generated from its own types. This is the divergence the drift test cannot \
         see (a custom Serialize, a flatten, a skip condition).\nerrors: {errors:#?}\n\
         document: {doc}"
    );
}

#[test]
fn schema_file_matches_report_types() {
    let expected = generated_schema();
    let path = schema_path();
    if std::env::var_os("UPDATE_CI_REPORT_SCHEMA").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &expected).unwrap();
        eprintln!("regenerated {}", path.display());
        return;
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing {}: {e}; regenerate with: {REGEN}", path.display()));
    assert_eq!(
        on_disk, expected,
        "crates/hauksbee-ci/schemas/hauksbee-ci-report.schema.json drifted from the \
         `--json` types (the file is generated, not hand-maintained); regenerate with: {REGEN}"
    );
}

#[test]
fn generated_schema_pins_the_optionality() {
    let schema: Value = serde_json::from_str(&generated_schema()).expect("valid JSON");
    // The union: exactly two variants, each a reference to one definition.
    let refs: Vec<&str> = schema["anyOf"]
        .as_array()
        .expect("CiJsonLine generates a two-variant union")
        .iter()
        .map(|v| {
            v["allOf"][0]["$ref"]
                .as_str()
                .expect("each variant references a definition")
        })
        .collect();
    assert_eq!(
        refs,
        ["#/definitions/CiJsonReport", "#/definitions/CiJsonError"],
        "the union is the report line then the error line"
    );
    let report = &schema["definitions"]["CiJsonReport"];
    let error = &schema["definitions"]["CiJsonError"];

    // The discriminator is pinned on both sides, so `ok` alone tells the two
    // apart. Without the const a validator cannot, and a consumer branching on
    // `ok` would be relying on undocumented behaviour.
    assert_eq!(report["properties"]["ok"]["const"], json!(true));
    assert_eq!(error["properties"]["ok"]["const"], json!(false));

    // Unknown keys must stay LEGAL. This is the whole additive-change promise:
    // a consumer holding an older copy of this schema has to keep validating
    // documents from a newer hauksbee-ci that added a field.
    for (name, def) in [("report", report), ("error", error)] {
        assert!(
            def.get("additionalProperties").is_none(),
            "the {name} line must not close additionalProperties, or every additive \
             field becomes a breaking change for a consumer validating with an older \
             copy of this schema"
        );
    }

    // The error line is two keys and nothing else. A consumer that sees
    // `ok: false` must not go looking for a verdict.
    let mut error_required: Vec<&str> = error["required"]
        .as_array()
        .expect("error required list")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    error_required.sort_unstable();
    assert_eq!(error_required, ["error", "ok"]);

    // Every field of the report is ALWAYS present, `coverage` included: it is
    // an Option that serializes as null rather than being skipped, and schemars
    // would leave it out of `required` without the explicit annotation.
    let required: Vec<&str> = report["required"]
        .as_array()
        .expect("report required list")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for field in [
        "schema_version",
        "ok",
        "spec_name",
        "board",
        "passed",
        "assertions_passed",
        "run_valid",
        "exit_code",
        "analog_abort",
        "seeds",
        "elapsed_s",
        "coverage",
        "substitutions",
        "coverage_warnings",
        "dead_rails",
        "waiver_notes",
        "results",
    ] {
        assert!(
            required.contains(&field),
            "`{field}` is always emitted and must be required; a consumer that has to \
             null-check every key gets nothing from this schema"
        );
    }
    // Nullable-but-present is a different promise from absent, and the schema
    // has to make it: `coverage` is null on a non-ensemble run.
    let coverage = serde_json::to_string(&report["properties"]["coverage"]).unwrap();
    assert!(
        coverage.contains("null"),
        "`coverage` is null on a non-ensemble run, so null must be in its type: {coverage}"
    );

    // Per-assertion: `why` and `waived` carry skip_serializing_if and are the
    // only two keys in the whole document a consumer may find ABSENT.
    let assertion = &schema["definitions"]["AssertResult"];
    let a_required: Vec<&str> = assertion["required"]
        .as_array()
        .expect("AssertResult required list")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for field in [
        "label",
        "kind",
        "passed",
        "invalid",
        "detail",
        "failing_seed",
        "failing_seeds",
        "seeds_total",
    ] {
        assert!(a_required.contains(&field), "`{field}` is always emitted");
    }
    for field in ["why", "waived"] {
        assert!(
            !a_required.contains(&field),
            "`{field}` is omitted entirely (skip_serializing_if) rather than set to null, \
             so it must NOT be required; claiming otherwise breaks every consumer that \
             validates a passing run"
        );
        assert_eq!(
            assertion["properties"][field]["type"],
            json!("string"),
            "`{field}` is absent-or-string, never null; a schema that also allows null \
             tells a consumer to null-check a key that is simply not there"
        );
    }
    // The other side of that distinction: `failing_seed` IS emitted as null.
    assert_eq!(
        assertion["properties"]["failing_seed"]["type"],
        json!(["integer", "null"]),
        "`failing_seed` is null on a pass, not absent"
    );
    // The waiver-matching fields are #[serde(skip)]: not on the wire at all,
    // and so not in the schema either.
    for field in ["subject_nets", "subject_refs"] {
        assert!(
            assertion["properties"].get(field).is_none(),
            "`{field}` is #[serde(skip)] and must not appear in the published shape"
        );
    }
}

#[test]
fn emitted_bytes_validate_against_the_schema() {
    let v = validator();
    let bin = env!("CARGO_BIN_EXE_hauksbee-ci");
    let ex = examples();

    // Five specs, chosen for the five outcomes a consumer has to handle. All
    // are analog-only (no emulator needed) and run in well under a second.
    let cases: Vec<(&str, Vec<PathBuf>)> = vec![
        // A green run: every honesty array empty, `coverage` null.
        ("pass", vec![ex.join("power_resistor_cool.toml")]),
        // A red run: `why` appears, `failing_seed` is a number.
        ("fail", vec![ex.join("power_resistor_hot.toml")]),
        // A tolerance ensemble: `coverage` is a string, `failing_seeds` has
        // more than one member, `seeds` > 1.
        (
            "ensemble",
            vec![ex.join("tolerance_divider_corners.toml")],
        ),
        // A spec that never ran: the error variant.
        ("spec error", vec![ex.join("no-such-spec.toml")]),
        // Multi-spec: one line per spec, report and error lines mixed in the
        // one stream, which is the shape a pipeline running `ci/*.toml` sees.
        (
            "multi-spec",
            vec![
                ex.join("power_resistor_cool.toml"),
                ex.join("power_resistor_hot.toml"),
                ex.join("no-such-spec.toml"),
            ],
        ),
    ];

    for (shape, specs) in cases {
        let out = Command::new(bin)
            .arg("run")
            .args(&specs)
            .arg("--json")
            .output()
            .unwrap_or_else(|e| panic!("{shape}: cannot run hauksbee-ci: {e}"));
        let stdout = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            specs.len(),
            "{shape}: --json must print exactly one line per spec, got {}:\n{stdout}",
            lines.len()
        );
        for line in lines {
            assert_valid(&v, line, shape);
        }
    }
}

#[test]
fn refusal_and_abort_documents_validate() {
    let v = validator();

    let invalid_assertion = AssertResult {
        label: "VOUT >= 3.0 V".to_string(),
        kind: "voltage".to_string(),
        passed: false,
        invalid: true,
        detail: "INVALID: the evaluation window overlaps a failed analog span".to_string(),
        failing_seed: None,
        failing_seeds: Vec::new(),
        seeds_total: 1,
        why: None,
        waived: None,
        subject_nets: vec!["VOUT".to_string()],
        subject_refs: Vec::new(),
    };

    // Two flavours of the refusal, because they reach exit 3 by different
    // routes: an INVALID assertion, and an abort no assertion happened to
    // cover (every result left pass-but-untrustworthy).
    let refusal = CiResult {
        spec_name: "refusal".to_string(),
        board: "board.net".to_string(),
        results: vec![invalid_assertion],
        seeds: 1,
        elapsed: Duration::from_millis(12),
        analog_abort: false,
        coverage: None,
        substitutions: Vec::new(),
        coverage_warnings: Vec::new(),
        dead_rails: Vec::new(),
        waiver_notes: Vec::new(),
    };
    // The abort case doubles as the every-honesty-array-populated document: a
    // consumer's worst case is all five qualifier lists non-empty at once.
    let bare_abort = CiResult {
        spec_name: "bare abort".to_string(),
        board: "board.net".to_string(),
        results: Vec::new(),
        seeds: 4,
        elapsed: Duration::from_millis(12),
        analog_abort: true,
        coverage: Some(hauksbee_ci::report::EnsembleCoverage::Corners {
            corners: 4,
            components: 2,
        }),
        substitutions: vec!["U1: ESP32-S3 requested, ran on ESP32".to_string()],
        coverage_warnings: vec![
            "co-sim: ADC channel 0 on U1 never received a sample".to_string()
        ],
        dead_rails: vec!["ANALOG_VDD".to_string()],
        waiver_notes: vec!["waiver for max_temp on R1 lapsed on 2026-01-01".to_string()],
    };

    for (shape, result) in [("refusal", &refusal), ("bare abort", &bare_abort)] {
        assert_eq!(
            result.exit_code(),
            hauksbee_engine::result::EXIT_INVALID_FOR_ANALYSIS,
            "{shape} must be exit 3"
        );
        let doc = result.render_json();
        assert_valid(&v, &doc, shape);
        let parsed: Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(parsed["run_valid"], json!(false), "{shape} is not valid");
        assert_eq!(parsed["passed"], json!(false), "{shape} is never green");
        assert_eq!(parsed["schema_version"], json!(CI_REPORT_SCHEMA_VERSION));
    }
}
