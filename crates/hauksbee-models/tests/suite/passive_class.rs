//! The builtin DB's `passive_class` declarations.
//!
//! `ComponentKind::Passive` covers R, C, L, ferrite beads, crystals, fuses and
//! net ties, because they share a solver path shape. `passive_class` is the finer
//! answer, and it is what `hauksbee-extract`'s `part_class` module keys resistor
//! classification on instead of the reference designator a CAD user typed. An
//! entry that omits it silently drops that consumer back down its evidence ladder
//! to the designator string, which is the bug the field exists to fix, so the
//! omission must fail loudly here rather than degrade quietly on a real board.

use hauksbee_models::schema::{DbFile, PassiveClass};
use hauksbee_models::{ComponentKind, ComponentQuery, ModelLibrary};

/// Every passive-kind entry in every shipped DB file declares its class.
///
/// Reads the `.toml` files the way the `models lint` command does, so a new
/// entry is covered the moment it is added rather than whenever someone
/// remembers to extend a hand-written list.
#[test]
fn every_builtin_passive_entry_declares_its_class() {
    let db_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db");
    let mut checked = 0usize;
    let mut missing: Vec<String> = Vec::new();
    for file in std::fs::read_dir(&db_dir).expect("db dir readable") {
        let path = file.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        // Some db files hold other schemas entirely: `pin_rules.toml` is a rule
        // table, `unmodelled.toml` an abstention list, and `load_profiles.toml`
        // reuses the `[[models]]` array name for piecewise current profiles whose
        // entries have no `kind`. Those contribute nothing here. A file that is a
        // model file and fails to parse must FAIL, not be skipped: skipping it turns
        // this sweep off for every entry in it while still reporting green, which is
        // how an invariant quietly stops being enforced.
        let text = std::fs::read_to_string(&path).expect("db file readable");
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let non_model = ["pin_rules.toml", "load_profiles.toml", "unmodelled.toml"];
        let db = match toml::from_str::<DbFile>(&text) {
            Ok(db) => db,
            Err(e) => {
                assert!(
                    non_model.contains(&name.as_str()),
                    "{name} is a model db file that does not deserialize, so every \
                     passive entry in it goes unchecked: {e}"
                );
                continue;
            }
        };
        for entry in &db.models {
            if entry.kind != ComponentKind::Passive {
                continue;
            }
            checked += 1;
            if entry.passive_class.is_none() {
                missing.push(format!("{}:{}", path.display(), entry.id));
            }
        }
    }
    assert!(
        checked >= 28,
        "expected to check at least the 28 passives.toml entries, saw {checked}"
    );
    assert!(
        missing.is_empty(),
        "these passive entries declare no passive_class, so a consumer asking \
         'is this a resistor?' falls back to the reference designator: {missing:?}"
    );
}

/// The classes the extract-side resistor test depends on, pinned at the source.
#[test]
fn generic_passives_resolve_to_the_right_class() {
    let lib = ModelLibrary::builtin_shared();
    let class_of = |lib_id: &str, value: &str| {
        lib.resolve(&ComponentQuery::new(
            Some(lib_id.to_string()),
            Some(value.to_string()),
            None,
        ))
        .model
        .and_then(|e| e.passive_class)
    };
    assert_eq!(class_of("Device:R", "10k"), Some(PassiveClass::Resistor));
    assert_eq!(class_of("Device:C", "100nF"), Some(PassiveClass::Capacitor));
    assert_eq!(class_of("Device:L", "10uH"), Some(PassiveClass::Inductor));
    assert_eq!(
        class_of("Device:Ferrite_Bead", "600R"),
        Some(PassiveClass::FerriteBead),
        "a bead's value looks ohmic, so its class is the only thing keeping it \
         out of the pull-up path"
    );
    assert_eq!(
        class_of("Device:Crystal", "16MHz"),
        Some(PassiveClass::Crystal)
    );
    assert_eq!(class_of("Device:Fuse", "1A"), Some(PassiveClass::Fuse));
    // A resistor is a resistor and nothing else: the one predicate every caller
    // that asks "can this be a pull-up?" uses.
    assert!(PassiveClass::Resistor.is_resistor());
    for other in [
        PassiveClass::Capacitor,
        PassiveClass::Inductor,
        PassiveClass::FerriteBead,
        PassiveClass::Crystal,
        PassiveClass::Fuse,
        PassiveClass::NetTie,
    ] {
        assert!(
            !other.is_resistor(),
            "{other:?} must not read as a resistor"
        );
    }
}
