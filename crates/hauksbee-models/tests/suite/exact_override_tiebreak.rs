//! A dedicated part override whose value_re is an exact literal must beat the
//! family entry whose character-class regex matches the same string, and the
//! winner must not depend on load order. The 1N400x rectifiers are the found
//! instance: the `1n4004` override (400 V) tied the `1n4001` family (50 V) on
//! specificity and lost by load order, so 1N4004–1N4007 all simulated with a
//! 50 V breakdown.

use hauksbee_models::{ComponentQuery, ModelLibrary, SourceLayer};

fn resolve(lib: &ModelLibrary, val: &str) -> hauksbee_models::Resolution {
    let q = ComponentQuery {
        value: Some(val.into()),
        ..Default::default()
    };
    lib.resolve(&q)
}

#[test]
fn each_1n400x_member_resolves_to_its_own_breakdown_voltage() {
    let lib = ModelLibrary::builtin();
    for (val, id, bv) in [
        ("1N4001", "1n4001", 50.0),
        ("1N4002", "1n4002", 100.0),
        ("1N4003", "1n4003", 200.0),
        ("1N4004", "1n4004", 400.0),
        ("1N4005", "1n4005", 600.0),
        ("1N4006", "1n4006", 800.0),
        ("1N4007", "1n4007", 1000.0),
    ] {
        let m = resolve(&lib, val)
            .model
            .unwrap_or_else(|| panic!("{val} did not resolve to any model"));
        assert_eq!(m.id, id, "{val} resolved to the wrong model id");
        assert_eq!(
            m.params.get_f64("bv"),
            Some(bv),
            "{val} resolved with the wrong breakdown voltage"
        );
    }
}

#[test]
fn one_n4004_rating_is_400v_not_the_family_50v() {
    let lib = ModelLibrary::builtin();
    let m = resolve(&lib, "1N4004").model.expect("1N4004 resolves");
    assert_eq!(m.ratings.max_voltage_v, Some(400.0), "1N4004 VRRM is 400 V");
}

#[test]
fn ina186_gain_suffix_beats_the_broad_family_entry() {
    // `ina186_dck_a1` (^INA186A1[A-Z0-9-]*$, gain 25) ties the broad `ina186`
    // ((?i)^INA186, gain 100) on field specificity; the exact-literal prefix
    // must win the tie on regex constrainedness, not on file position.
    let lib = ModelLibrary::builtin();
    let m = resolve(&lib, "INA186A1IDCKR")
        .model
        .expect("INA186A1 resolves");
    assert_eq!(m.id, "ina186_dck_a1");
    assert_eq!(m.params.get_f64("gain"), Some(25.0));
    // The bare value with no gain suffix still binds the broad entry.
    let m = resolve(&lib, "INA186").model.expect("INA186 resolves");
    assert_eq!(m.id, "ina186");
}

#[test]
fn exact_override_wins_regardless_of_load_order() {
    // Two synthetic same-layer entries whose regexes both match "ZZ904":
    // a family character class and an exact-literal override. Load them in
    // both directory orders; the override must win each time.
    let family = r#"
[[models]]
id = "zz_family"
kind = "diode"
description = "synthetic family"
[models.match]
value_re = "(?i)^ZZ90[0-9]$"
[models.params]
bv = 50.0
"#;
    let override_ = r#"
[[models]]
id = "zz904_override"
kind = "diode"
description = "synthetic exact override"
[models.match]
value_re = "(?i)^ZZ904$"
[models.params]
bv = 400.0
"#;

    let base = tempfile::tempdir().expect("tempdir");
    let dir_a = base.path().join("a");
    let dir_b = base.path().join("b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    std::fs::write(dir_a.join("family.toml"), family).unwrap();
    std::fs::write(dir_b.join("override.toml"), override_).unwrap();

    for order in [[&dir_a, &dir_b], [&dir_b, &dir_a]] {
        let mut lib = ModelLibrary::empty();
        for dir in order {
            let errs = lib.load_dir_layer(dir, SourceLayer::UserDir);
            assert!(errs.is_empty(), "load errors: {errs:?}");
        }
        let m = resolve(&lib, "ZZ904").model.expect("ZZ904 resolves");
        assert_eq!(
            m.id, "zz904_override",
            "exact override must win in load order {order:?}"
        );
        assert_eq!(m.params.get_f64("bv"), Some(400.0));
        // The family still owns the values the override does not claim.
        let m = resolve(&lib, "ZZ901").model.expect("ZZ901 resolves");
        assert_eq!(m.id, "zz_family");
    }
}
