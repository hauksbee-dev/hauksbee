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
