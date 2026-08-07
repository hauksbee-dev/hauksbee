//! Semantic source tier first, then explicit storage layers
//! (06-extensibility-sdk §3). The pack/user entries here are
//! deliberately LESS specific than the builtin they beat (a footprint-only
//! match, score 5, vs the builtin's value regex, score 30), so these tests
//! fail if layering ever silently degrades back to specificity-only.

use std::path::Path;

use hauksbee_models::pack::PackStore;
use hauksbee_models::{ComponentQuery, ModelLibrary, SourceLayer};

const MANIFEST: &str = r#"
[pack]
name = "NAME"
version = "1.0.0"
license = "MIT"
min_hauksbee_version = "0.1.0"
provenance = "vendor"
"#;

/// A diode entry matching only by footprint regex (specificity 5).
fn footprint_diode(id: &str) -> String {
    format!(
        r#"
[[models]]
id = "{id}"
kind = "diode"
[models.match]
footprint_re = "LAYERTEST_FP"
[models.params]
is = 1.0e-14
n = 1.5
rs = 1.0
"#
    )
}

fn make_pack(dir: &Path, name: &str, model_toml: &str) {
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("pack.toml"), MANIFEST.replace("NAME", name)).unwrap();
    std::fs::write(dir.join("models/m.toml"), model_toml).unwrap();
}

/// A query the builtin db resolves by value (BC847, value_re score 30) and
/// the test entries resolve by footprint only (score 5).
fn query() -> ComponentQuery {
    ComponentQuery {
        value: Some("BC847".into()),
        footprint: Some("LAYERTEST_FP:D_0805".into()),
        ..Default::default()
    }
}

#[test]
fn builtin_wins_when_alone() {
    let lib = ModelLibrary::builtin();
    let r = lib.resolve(&query());
    assert_eq!(r.layer, Some(SourceLayer::Builtin));
    assert_eq!(r.source.as_deref(), Some("builtin"));
}

#[test]
fn pack_beats_builtin_despite_lower_specificity() {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    make_pack(src.path(), "layer-pack", &footprint_diode("pack_diode"));
    let store = PackStore::in_home(home.path());
    store.install(src.path(), "test").unwrap();

    let mut lib = ModelLibrary::builtin();
    let warnings = lib.load_packs(&store);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");

    let r = lib.resolve(&query());
    assert_eq!(r.model.as_ref().unwrap().id, "pack_diode");
    assert_eq!(r.layer, Some(SourceLayer::Pack));
    assert_eq!(r.source.as_deref(), Some("pack"));
    assert_eq!(r.origin.as_deref(), Some("layer-pack@1.0.0"));
}

#[test]
fn vendor_pack_beats_datasheet_extraction_directory() {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    make_pack(src.path(), "layer-pack", &footprint_diode("pack_diode"));
    let store = PackStore::in_home(home.path());
    store.install(src.path(), "test").unwrap();

    let user_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        user_dir.path().join("mine.toml"),
        footprint_diode("user_diode"),
    )
    .unwrap();

    let mut lib = ModelLibrary::builtin();
    lib.load_packs(&store);
    assert!(lib.load_user_dir(user_dir.path()).is_empty());

    let r = lib.resolve(&query());
    assert_eq!(r.model.as_ref().unwrap().id, "pack_diode");
    assert_eq!(r.layer, Some(SourceLayer::Pack));
    assert_eq!(r.source.as_deref(), Some("pack"));
}

#[test]
fn models_dir_flag_beats_user_dir() {
    let user_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        user_dir.path().join("mine.toml"),
        footprint_diode("user_diode"),
    )
    .unwrap();
    let flag_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        flag_dir.path().join("flag.toml"),
        footprint_diode("flag_diode"),
    )
    .unwrap();

    let mut lib = ModelLibrary::builtin();
    // Load the HIGHER layer first: insertion order must not matter.
    assert!(lib
        .load_dir_layer(flag_dir.path(), SourceLayer::ModelsDirFlag)
        .is_empty());
    assert!(lib
        .load_dir_layer(user_dir.path(), SourceLayer::UserDir)
        .is_empty());

    let r = lib.resolve(&query());
    assert_eq!(r.model.as_ref().unwrap().id, "flag_diode");
    assert_eq!(r.layer, Some(SourceLayer::ModelsDirFlag));
}

#[test]
fn same_layer_conflict_between_packs_is_reported_naming_both() {
    let home = tempfile::tempdir().unwrap();
    let store = PackStore::in_home(home.path());
    for name in ["pack-alpha", "pack-beta"] {
        let src = tempfile::tempdir().unwrap();
        // Both packs ship the SAME model id.
        make_pack(src.path(), name, &footprint_diode("shared_widget"));
        store.install(src.path(), "test").unwrap();
    }

    let mut lib = ModelLibrary::builtin();
    let warnings = lib.load_packs(&store);
    let conflict = warnings
        .iter()
        .find(|w| w.contains("same-layer conflict"))
        .unwrap_or_else(|| panic!("no conflict reported; warnings: {warnings:?}"));
    assert!(
        conflict.contains("shared_widget"),
        "names the id: {conflict}"
    );
    assert!(
        conflict.contains("pack-alpha@1.0.0"),
        "names pack one: {conflict}"
    );
    assert!(
        conflict.contains("pack-beta@1.0.0"),
        "names pack two: {conflict}"
    );
}

#[test]
fn distinct_ids_across_packs_do_not_conflict() {
    let home = tempfile::tempdir().unwrap();
    let store = PackStore::in_home(home.path());
    for (name, id) in [("pack-alpha", "widget_a"), ("pack-beta", "widget_b")] {
        let src = tempfile::tempdir().unwrap();
        make_pack(src.path(), name, &footprint_diode(id));
        store.install(src.path(), "test").unwrap();
    }
    let mut lib = ModelLibrary::builtin();
    let warnings = lib.load_packs(&store);
    assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
}
