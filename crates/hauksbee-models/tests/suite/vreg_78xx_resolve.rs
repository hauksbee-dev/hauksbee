//! Each 78xx voltage code must resolve to its own model with the correct output
//! voltage; the family must not collapse onto the +5V model (which silently
//! simulated 9V/12V/15V rails as 5V).

use hauksbee_models::{ComponentQuery, ModelLibrary};

#[test]
fn seven_eight_xx_family_resolves_to_its_own_voltage() {
    let lib = ModelLibrary::builtin();
    for (val, id, vout) in [
        ("7805", "7805", 5.0),
        ("LM7805CT", "7805", 5.0),
        ("78L05", "7805", 5.0),
        ("7806", "7806", 6.0),
        ("7808", "7808", 8.0),
        ("L7809CV", "7809", 9.0),
        ("7812", "7812", 12.0),
        ("LM7812", "7812", 12.0),
        ("78M12", "7812", 12.0),
        ("7815", "7815", 15.0),
        ("7818", "7818", 18.0),
        ("7824", "7824", 24.0),
    ] {
        let q = ComponentQuery {
            value: Some(val.into()),
            ..Default::default()
        };
        let m = lib
            .resolve(&q)
            .model
            .unwrap_or_else(|| panic!("{val} did not resolve to any model"));
        assert_eq!(m.id, id, "{val} resolved to the wrong model id");
        assert_eq!(
            m.params.get_f64("vout"),
            Some(vout),
            "{val} resolved with the wrong output voltage"
        );
    }
}

/// The exact MNT Pocket Reform U4 value gets a source-bound nominal rail model,
/// while another fixed-output option remains unresolved rather than silently
/// inheriting 1.8 V.  The warning is part of the model contract: this is a
/// steady-state DC slice, not a claim about protection or transients.
#[test]
fn mnt_pocket_tlv1117_18_is_voltage_specific_and_explicitly_partial() {
    let lib = ModelLibrary::builtin();

    let red = lib.resolve(&ComponentQuery {
        value: Some("TLV1117-33".into()),
        ..Default::default()
    });
    assert!(
        red.model.is_none(),
        "the unfitted TLV1117-33 option must stay unresolved (RED)"
    );

    let green = lib.resolve(&ComponentQuery {
        value: Some("TLV1117-18".into()),
        mpn: Some("TLV1117-18CDCYR".into()),
        ..Default::default()
    });
    let model = green
        .model
        .expect("the exact MNT TLV1117-18 must resolve (GREEN)");
    assert_eq!(model.id, "tlv1117_18");
    assert_eq!(model.params.get_f64("vout"), Some(1.8));
    assert_eq!(model.params.get_f64("dropout_v"), Some(1.3));
    assert_eq!(model.params.get_f64("iq_a"), Some(65.0e-6));
    assert_eq!(model.ratings.max_current_a, Some(0.8));
    assert_eq!(model.ratings.max_voltage_v, Some(15.0));
    assert_eq!(model.ratings.max_junction_temp_c, Some(125.0));
    assert_eq!(model.pins.len(), 4);
    let warning = model
        .params
        .get_str("warning")
        .expect("the nominal-only model must carry an explicit behavior caveat");
    assert!(warning.starts_with("[partial model] "));
    assert!(warning.contains("not modeled"));
}
