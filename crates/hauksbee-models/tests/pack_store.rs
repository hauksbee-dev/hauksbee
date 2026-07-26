//! PackStore install / list / remove round-trip, entirely inside a temp
//! "home"; the real ~/.hauksbee is never touched.

use std::path::Path;

use hauksbee_models::pack::{Pack, PackError, PackStore};

const MANIFEST: &str = r#"
[pack]
name = "acme-diodes"
version = "1.2.0"
license = "MIT"
min_hauksbee_version = "0.1.0"
provenance = "datasheet-extracted"
"#;

const MODEL: &str = r#"
[[models]]
id = "acme_1n914"
kind = "diode"
[models.match]
value_re = "1N914"
[models.params]
is = 2.5e-9
n = 1.75
rs = 0.6
"#;

fn write_pack(dir: &Path) {
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("pack.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("models/diodes.toml"), MODEL).unwrap();
    // Optional firmware fixtures ride along verbatim.
    std::fs::create_dir_all(dir.join("firmware")).unwrap();
    std::fs::write(dir.join("firmware/fixture.txt"), "hello").unwrap();
}

#[test]
fn install_list_remove_round_trip() {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    write_pack(src.path());

    let store = PackStore::in_home(home.path());
    assert!(
        store.list().unwrap().is_empty(),
        "fresh store lists nothing"
    );

    // Install: dir contents copied, record written.
    let rec = store.install(src.path(), "local-test").unwrap();
    assert_eq!(rec.name, "acme-diodes");
    assert_eq!(rec.version, "1.2.0");
    let installed = home.path().join(".hauksbee/packs/acme-diodes@1.2.0");
    assert!(installed.join("pack.toml").is_file());
    assert!(installed.join("models/diodes.toml").is_file());
    assert!(
        installed.join("firmware/fixture.txt").is_file(),
        "fixtures copied"
    );
    // The installed copy is itself a loadable pack.
    Pack::load(&installed).expect("installed pack must re-validate");

    // packs.toml sits ALONGSIDE the packs dir and records the install.
    let record_path = home.path().join(".hauksbee/packs.toml");
    assert_eq!(store.record_path(), record_path.as_path());
    let record_text = std::fs::read_to_string(&record_path).unwrap();
    assert!(
        record_text.contains("acme-diodes"),
        "packs.toml: {record_text}"
    );
    assert!(
        record_text.contains("local-test"),
        "source recorded: {record_text}"
    );

    // List sees it.
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].provenance, "datasheet-extracted");

    // Double-install of the same name is refused, loudly.
    let e = store.install(src.path(), "again").unwrap_err();
    assert!(matches!(e, PackError::AlreadyInstalled(..)), "got {e:?}");

    // Remove: dir gone, record gone.
    store.remove("acme-diodes").unwrap();
    assert!(!installed.exists(), "pack dir removed");
    assert!(store.list().unwrap().is_empty());
    let e = store.remove("acme-diodes").unwrap_err();
    assert!(matches!(e, PackError::NotInstalled(_)), "got {e:?}");
}

#[test]
fn install_refuses_invalid_pack_before_copying() {
    let home = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    std::fs::write(src.path().join("pack.toml"), "[pack]\nname = \"x\"").unwrap();

    let store = PackStore::in_home(home.path());
    let e = store.install(src.path(), "bad").unwrap_err();
    assert!(matches!(e, PackError::MissingField(_)), "got {e:?}");
    assert!(
        !home.path().join(".hauksbee").exists(),
        "nothing may be created for a failed install"
    );
}
