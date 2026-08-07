//! `hauksbee models resolve <board>`: the per-component
//! which-entry-won-from-which-layer table (06-extensibility-sdk §3), the
//! pack-author debugging surface. Asserts the output names layers with their
//! priorities, using only temp dirs, never the machine's real ~/.hauksbee.

use hauksbee_engine::commands::models::{
    model_requirement_refusals, resolve_report, resolve_report_json, ModelRequirement,
};
use hauksbee_extract::{Component, ExtractedBoard};
use hauksbee_models::{ModelLibrary, SourceLayer};

fn comp(reference: &str, value: &str, footprint: &str) -> Component {
    Component {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: String::new(),
        footprint: footprint.to_string(),
        position: None,
        layer: String::new(),
        properties: Vec::new(),
        dnp: false,
        pins: Vec::new(),
    }
}

fn board() -> ExtractedBoard {
    ExtractedBoard {
        name: "layer_test".to_string(),
        nets: Vec::new(),
        components: vec![
            comp("D1", "BAT43", "Diode_THT:D_DO-35_SOD27_P7.62mm_Horizontal"),
            comp("D2", "1N914", "RESOLVETEST_FP:D_0805"),
            comp("U99", "TOTALLY_UNKNOWN_XYZ", ""),
        ],
    }
}

#[test]
fn resolve_report_names_layers_and_origins() {
    // A --models-dir entry (layer 30) that catches D2 by footprint only.
    let flag_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        flag_dir.path().join("mine.toml"),
        r#"
[[models]]
id = "my_resolve_diode"
kind = "diode"
[models.match]
footprint_re = "RESOLVETEST_FP"
[models.params]
is = 2.5e-9
n = 1.75
rs = 0.6
"#,
    )
    .unwrap();

    let mut lib = ModelLibrary::builtin();
    assert!(lib
        .load_dir_layer(flag_dir.path(), SourceLayer::ModelsDirFlag)
        .is_empty());

    let out = resolve_report(&lib, &board());
    println!("{out}");

    // The legend states the whole priority order, all six layers.
    assert!(
        out.contains(
            "builtin(0) < pack(10) < user-dir(20) < user-config-dir(25) < models-dir(30) \
             < spice(40)"
        ),
        "legend missing:\n{out}"
    );
    // The table is the same box-drawing style as the bind report.
    assert!(
        out.contains('┌') && out.contains('│') && out.contains('└'),
        "expected a box-drawing table:\n{out}"
    );
    let row = |reference: &str| {
        out.lines()
            .find(|l| l.starts_with(&format!("│ {reference} ")))
            .unwrap_or_else(|| panic!("{reference} row missing:\n{out}"))
    };
    // D1 resolves from the builtin db, with its layer and db-file origin.
    let d1 = row("D1");
    assert!(d1.contains("builtin(0)"), "D1 row: {d1}");
    assert!(d1.contains("diodes"), "D1 origin is the db file: {d1}");
    assert!(d1.contains("curated-library"), "D1 source tier: {d1}");
    assert!(
        d1.contains("unknown"),
        "D1 uncertainty must be explicit: {d1}"
    );
    // D2 resolves from the --models-dir layer, naming the file it came from.
    let d2 = row("D2");
    assert!(d2.contains("my_resolve_diode"), "D2 row: {d2}");
    assert!(d2.contains("models-dir(30)"), "D2 row: {d2}");
    assert!(d2.contains("mine"), "D2 origin is the user file: {d2}");
    assert!(d2.contains("user-model"), "D2 source tier: {d2}");
    // The unknown part is loudly unresolved, not silently dropped.
    let u99 = row("U99");
    assert!(u99.contains("UNRESOLVED"), "U99 row: {u99}");
}

#[test]
fn resolve_json_exposes_the_canonical_source_and_accuracy_record() {
    let value: serde_json::Value =
        serde_json::from_str(&resolve_report_json(&ModelLibrary::builtin(), &board())).unwrap();
    let d1 = value["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["ref"] == "D1")
        .unwrap();
    assert_eq!(d1["source"]["tier"], "curated-library");
    assert_eq!(d1["source"]["layer"], "builtin");
    assert_eq!(d1["source"]["validation"], "physical-bounds-only");
    assert_eq!(d1["source"]["uncertainty"][0]["status"], "unknown");
}

#[test]
fn accuracy_requirements_refuse_unknown_or_unvalidated_sources_by_reference() {
    let requirements = ModelRequirement {
        minimum_tier: Some(hauksbee_ir::evidence::ModelSourceTier::CuratedLibrary),
        minimum_validation: None,
        require_intervals: true,
    };
    let refusals = model_requirement_refusals(&ModelLibrary::builtin(), &board(), requirements);
    assert!(refusals
        .iter()
        .any(|issue| issue.reference == "D1" && issue.reason.contains("interval")));
    assert!(refusals
        .iter()
        .any(|issue| issue.reference == "U99" && issue.reason.contains("open")));
    let validation_refusals = model_requirement_refusals(
        &ModelLibrary::builtin(),
        &board(),
        ModelRequirement {
            minimum_tier: None,
            minimum_validation: Some(hauksbee_ir::evidence::ModelValidation::DatasheetCurves),
            require_intervals: false,
        },
    );
    assert!(validation_refusals.iter().any(|issue| {
        issue.reference == "D1" && issue.reason.contains("validation physical-bounds-only")
    }));
}
