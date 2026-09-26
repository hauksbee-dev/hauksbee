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
            Err(_) if non_model.contains(&name.as_str()) => continue,
            Err(e) => panic!("{name} is a model db file that does not deserialize: {e}"),
        };
        for entry in db
            .models
            .iter()
            .filter(|e| e.kind == ComponentKind::Passive)
        {
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
    use PassiveClass::{Capacitor, Crystal, FerriteBead, Fuse, Inductor, NetTie, Resistor};
    let lib = ModelLibrary::builtin_shared();
    for (lib_id, value, footprint, expected) in [
        ("Device:R", "10k", "", Some(Resistor)),
        ("Device:C", "100nF", "", Some(Capacitor)),
        ("Device:L", "10uH", "", Some(Inductor)),
        ("Device:Ferrite_Bead", "600R", "", Some(FerriteBead)),
        ("Device:Crystal", "16MHz", "", Some(Crystal)),
        ("Device:Fuse", "1A", "", Some(Fuse)),
        ("", "600@100MHz", "", Some(FerriteBead)),
        ("", "120 @ 100 MHz", "", Some(FerriteBead)),
        ("", "25A", "Fuse:Fuse_1812", Some(Fuse)),
        ("", "0.2A", "Fuse:Fuse_1812", Some(Fuse)),
        ("", "32V/2A", "Fuse:Fuse_1812", Some(Fuse)),
        ("", "25A", "Package_SO:SOIC-8", None),
        ("", "-25A", "Fuse:Fuse_1812", None),
        ("", "0x2A", "Fuse:Fuse_1812", None),
        ("", "600@100kHz", "", None),
        ("", "-600@100MHz", "", None),
    ] {
        let result = lib.resolve(&ComponentQuery::new(
            Some(lib_id.into()),
            Some(value.into()),
            Some(footprint.into()),
        ));
        // Numeric cards must not claim a rating from these negative controls,
        // even when an independent footprint-only card identifies a fuse.
        let actual = result
            .model
            .filter(|e| {
                !lib_id.is_empty()
                    || matches!(e.id.as_str(), "fuse_rated" | "ferrite_bead_generic_rated")
            })
            .and_then(|e| e.passive_class);
        assert_eq!(actual, expected, "{value} / {footprint}");
    }
    // A resistor is a resistor and nothing else: the one predicate every caller
    // that asks "can this be a pull-up?" uses.
    for class in [
        Resistor,
        Capacitor,
        Inductor,
        FerriteBead,
        Crystal,
        Fuse,
        NetTie,
    ] {
        assert_eq!(class.is_resistor(), class == Resistor, "{class:?}");
    }
}
