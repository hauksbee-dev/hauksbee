//! `hauksbee models resolve <board>`: the per-component
//! which-entry-won-from-which-layer table (06-extensibility-sdk §3), the
//! pack-author debugging surface. Asserts the output names layers with their
//! priorities, using only temp dirs — never the machine's real ~/.hauksbee.

use hauksbee_engine::commands::models::resolve_report;
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

    // The legend states the whole priority order.
    assert!(
        out.contains("builtin(0) < pack(10) < user-dir(20) < models-dir(30) < spice(40)"),
        "legend missing:\n{out}"
    );
    // D1 resolves from the builtin db, with its layer and db-file origin.
    let d1 = out.lines().find(|l| l.starts_with("D1")).expect("D1 row");
    assert!(d1.contains("builtin(0)"), "D1 row: {d1}");
    assert!(d1.contains("diodes"), "D1 origin is the db file: {d1}");
    // D2 resolves from the --models-dir layer, naming the file it came from.
    let d2 = out.lines().find(|l| l.starts_with("D2")).expect("D2 row");
    assert!(d2.contains("my_resolve_diode"), "D2 row: {d2}");
    assert!(d2.contains("models-dir(30)"), "D2 row: {d2}");
    assert!(d2.contains("mine"), "D2 origin is the user file: {d2}");
    // The unknown part is loudly unresolved, not silently dropped.
    let u99 = out.lines().find(|l| l.starts_with("U99")).expect("U99 row");
    assert!(u99.contains("UNRESOLVED"), "U99 row: {u99}");
}
