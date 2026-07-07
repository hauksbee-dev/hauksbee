//! Pack-format validation (06-extensibility-sdk §3): every failure category
//! is a named `PackError` variant, exercised here one by one.

use std::path::Path;

use hauksbee_models::pack::{Pack, PackError, PackManifest, Provenance, HAUKSBEE_VERSION};

const GOOD_MANIFEST: &str = r#"
[pack]
name = "acme-diodes"
version = "1.2.0"
license = "MIT"
min_hauksbee_version = "0.1.0"
provenance = "hand-written"
description = "test pack"
"#;

const GOOD_MODEL: &str = r#"
[[models]]
id = "acme_1n914"
kind = "diode"
description = "pack diode"
[models.match]
value_re = "1N914"
[models.params]
is = 2.5e-9
n = 1.75
rs = 0.6
"#;

fn write_pack(dir: &Path, manifest: &str, model: Option<&str>) {
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("pack.toml"), manifest).unwrap();
    if let Some(m) = model {
        std::fs::write(dir.join("models").join("diodes.toml"), m).unwrap();
    }
}

#[test]
fn valid_pack_loads() {
    let tmp = tempfile::tempdir().unwrap();
    write_pack(tmp.path(), GOOD_MANIFEST, Some(GOOD_MODEL));
    let pack = Pack::load(tmp.path()).expect("valid pack must load");
    assert_eq!(pack.manifest.name, "acme-diodes");
    assert_eq!(pack.manifest.version, "1.2.0");
    assert_eq!(pack.manifest.provenance, Provenance::HandWritten);
    assert_eq!(pack.manifest.dir_name(), "acme-diodes@1.2.0");
    assert_eq!(pack.model_files.len(), 1);
}

#[test]
fn missing_dir_is_named() {
    let e = Pack::load(Path::new("/nonexistent/definitely-not-a-pack")).unwrap_err();
    assert!(matches!(e, PackError::NotADirectory(_)), "got {e:?}");
}

#[test]
fn missing_manifest_is_named() {
    let tmp = tempfile::tempdir().unwrap();
    let e = Pack::load(tmp.path()).unwrap_err();
    assert!(matches!(e, PackError::MissingManifest(_)), "got {e:?}");
}

#[test]
fn manifest_parse_error_is_named() {
    let m = PackManifest::from_toml("this is not toml [", HAUKSBEE_VERSION).unwrap_err();
    assert!(matches!(m, PackError::ManifestParse(_)), "got {m:?}");
    let m = PackManifest::from_toml("[not_pack]\nx = 1", HAUKSBEE_VERSION).unwrap_err();
    assert!(matches!(m, PackError::ManifestParse(_)), "got {m:?}");
}

#[test]
fn each_missing_field_is_named() {
    for field in ["name", "version", "license", "min_hauksbee_version", "provenance"] {
        let src: String = GOOD_MANIFEST
            .lines()
            .filter(|l| !l.trim_start().starts_with(&format!("{field} =")))
            .collect::<Vec<_>>()
            .join("\n");
        let e = PackManifest::from_toml(&src, HAUKSBEE_VERSION).unwrap_err();
        match e {
            PackError::MissingField(f) => assert_eq!(f, field),
            other => panic!("dropping '{field}' gave {other:?}"),
        }
    }
}

#[test]
fn bad_name_is_named() {
    let src = GOOD_MANIFEST.replace("\"acme-diodes\"", "\"Acme Diodes!\"");
    let e = PackManifest::from_toml(&src, HAUKSBEE_VERSION).unwrap_err();
    assert!(matches!(e, PackError::InvalidName(_)), "got {e:?}");
}

#[test]
fn bad_version_syntax_is_named() {
    for bad in ["1.2", "1.2.x", "v1.2.0", "1.2.3.4", ""] {
        let src = GOOD_MANIFEST.replace("version = \"1.2.0\"", &format!("version = \"{bad}\""));
        let e = PackManifest::from_toml(&src, HAUKSBEE_VERSION).unwrap_err();
        assert!(
            matches!(e, PackError::InvalidVersion { .. }),
            "version {bad:?} gave {e:?}"
        );
    }
}

#[test]
fn unknown_provenance_is_named() {
    let src = GOOD_MANIFEST.replace("\"hand-written\"", "\"scraped-from-a-forum\"");
    let e = PackManifest::from_toml(&src, HAUKSBEE_VERSION).unwrap_err();
    assert!(matches!(e, PackError::UnknownProvenance(_)), "got {e:?}");
}

#[test]
fn min_version_newer_than_build_is_named() {
    let src = GOOD_MANIFEST.replace(
        "min_hauksbee_version = \"0.1.0\"",
        "min_hauksbee_version = \"999.0.0\"",
    );
    let e = PackManifest::from_toml(&src, HAUKSBEE_VERSION).unwrap_err();
    assert!(matches!(e, PackError::IncompatibleVersion { .. }), "got {e:?}");
}

#[test]
fn unknown_manifest_field_is_rejected() {
    // Typos ("licence") must not silently vanish.
    let src = format!("{GOOD_MANIFEST}\nsigned_by = \"nobody\"\n");
    let e = PackManifest::from_toml(&src, HAUKSBEE_VERSION).unwrap_err();
    assert!(matches!(e, PackError::ManifestParse(_)), "got {e:?}");
}

#[test]
fn pack_without_models_is_named() {
    let tmp = tempfile::tempdir().unwrap();
    write_pack(tmp.path(), GOOD_MANIFEST, None); // models/ exists but empty
    let e = Pack::load(tmp.path()).unwrap_err();
    assert!(matches!(e, PackError::NoModels(_)), "got {e:?}");
}

#[test]
fn model_file_failing_lint_is_named() {
    // Diode missing its required params fails the same validation
    // `models lint` applies, tied to the file name.
    let bad = r#"
[[models]]
id = "acme_broken"
kind = "diode"
[models.match]
value_re = "BROKEN1"
"#;
    let tmp = tempfile::tempdir().unwrap();
    write_pack(tmp.path(), GOOD_MANIFEST, Some(bad));
    let e = Pack::load(tmp.path()).unwrap_err();
    match e {
        PackError::ModelFileInvalid { file, message } => {
            assert_eq!(file, "diodes.toml");
            assert!(message.contains("acme_broken"), "message: {message}");
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn model_file_bad_toml_is_named() {
    let tmp = tempfile::tempdir().unwrap();
    write_pack(tmp.path(), GOOD_MANIFEST, Some("[[models]\nnot toml"));
    let e = Pack::load(tmp.path()).unwrap_err();
    assert!(matches!(e, PackError::ModelFileInvalid { .. }), "got {e:?}");
}

#[test]
fn model_file_with_no_entries_is_named() {
    let tmp = tempfile::tempdir().unwrap();
    write_pack(tmp.path(), GOOD_MANIFEST, Some("# empty\n"));
    let e = Pack::load(tmp.path()).unwrap_err();
    assert!(matches!(e, PackError::ModelFileInvalid { .. }), "got {e:?}");
}
