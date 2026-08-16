//! Exact passive-part ratings used by decision-grade board checks.

use hauksbee_models::{ComponentQuery, ModelLibrary};

#[test]
fn exact_ucc_ekyb_bulk_cap_resolves_its_datasheet_ripple_rating() {
    let lib = ModelLibrary::builtin();
    let resolved = lib.resolve(&ComponentQuery {
        value: Some("1200uF".into()),
        mpn: Some("EKYB630ELL122MLN3S".into()),
        ..Default::default()
    });
    let model = resolved
        .model
        .expect("the exact UCC capacitor MPN resolves");

    assert_eq!(model.id, "ucc_ekyb630ell122mln3s");
    assert_eq!(model.ratings.max_voltage_v, Some(63.0));
    assert_eq!(model.ratings.max_ripple_current_a, Some(3.0));
    assert!(model.ratings.polarized);
}

#[test]
fn exact_littelfuse_order_code_unlocks_the_executable_trip_envelope() {
    let lib = ModelLibrary::builtin();
    let board_query = ComponentQuery {
        value: Some("Polyfuse 1.8A".into()),
        footprint: Some("Fuse:Fuse_1812_4532Metric".into()),
        properties: vec![(
            "KiLib Generator".into(),
            "https://www.digikey.ie/en/products/detail/littelfuse-inc/1812L150-24MR/295817".into(),
        )],
        ..Default::default()
    };
    let resolution = lib.resolve(&board_query);
    assert_eq!(resolution.references.len(), 1);
    assert_eq!(
        resolution.references[0].locator,
        "Electrical Characteristics table, 1812L150/24 row; Temperature Rerating table"
    );
    let model = resolution
        .model
        .expect("the retained exact supplier order code resolves");

    assert_eq!(model.id, "littelfuse_1812l150_24mr");
    assert_eq!(model.ratings.max_current_a, Some(1.5));
    assert_eq!(model.ratings.max_voltage_v, Some(24.0));
    assert!(model
        .coverage
        .implements
        .iter()
        .any(|capability| capability == "trip_by_8a_20c_envelope"));
    let path = model
        .behavioral
        .series_paths
        .first()
        .expect("the protection path is executable");
    assert_eq!(path.default_ohms, 0.040);
    assert_eq!(path.state_ohms.get("tripped"), Some(&1.0e9));
    assert_eq!(
        model.behavioral.fsm.as_ref().unwrap().transitions[0].guard_dwell_s,
        Some(1.50)
    );

    let ambiguous = lib
        .resolve(&ComponentQuery {
            properties: Vec::new(),
            ..board_query
        })
        .model
        .expect("the family identity still resolves without the order code");
    assert_eq!(ambiguous.id, "polyfuse_1812_1p8a_identity");
    assert_eq!(ambiguous.params.get_bool("identity_only"), Some(true));
    assert!(ambiguous.behavioral.is_empty());
}
