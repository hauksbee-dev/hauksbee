//! Each 78xx voltage code must resolve to its own model with the correct output
//! voltage; the family must not collapse onto the +5V model (which silently
//! simulated 9V/12V/15V rails as 5V).

use super::{expect_model, expect_query, resolve_value};

#[test]
fn seven_eight_xx_family_resolves_to_its_own_voltage() {
    for (val, id, vout) in [
        ("7805", "7805", 5.0),
        ("LM7805CT", "7805", 5.0),
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
        let m = expect_model(val);
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
    let red = resolve_value("TLV1117-33");
    assert!(
        red.model.is_none(),
        "the unfitted TLV1117-33 option must stay unresolved (RED)"
    );

    // The exact MNT TLV1117-18 must resolve (GREEN).
    let model = expect_query(Some("TLV1117-18"), Some("TLV1117-18CDCYR"), None);
    assert_eq!(model.id, "tlv1117_18");
    assert_eq!(model.params.get_f64("vout"), Some(1.8));
    // TI TLV1117 SLVS561O Rev. O, §8.1.1 says this order code may ship
    // legacy or new silicon. At IOUT = 800 mA the worst guaranteed maximum
    // is therefore the legacy I-grade 1.4 V over TJ = -40..125 °C (§5.6).
    assert_eq!(model.params.get_f64("dropout_v"), Some(1.4));
    // Legacy fixed-output silicon is 5 mA typical; 65 µA is new-chip-only.
    assert_eq!(model.params.get_f64("iq_a"), Some(5.0e-3));
    assert_eq!(model.ratings.max_current_a, Some(0.8));
    // SLVS561O §5.1 abs max VIN 16 V; the 15 V recommended ceiling is the envelope.
    assert_eq!(model.ratings.max_voltage_v, Some(16.0));
    assert_eq!(model.ratings.max_junction_temp_c, Some(125.0));
    assert_eq!(model.pins.len(), 4);
    let warning = model
        .params
        .get_str("warning")
        .expect("the nominal-only model must carry an explicit behavior caveat");
    assert!(warning.starts_with("[partial model] "));
    assert!(warning.contains("not modeled"));
}
