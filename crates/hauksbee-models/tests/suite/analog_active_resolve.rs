use super::expect_query;
use hauksbee_models::ComponentKind;

#[test]
fn ina186_family_resolves_to_current_sense_amplifier_model() {
    let model = expect_query(
        Some("INA186"),
        None,
        Some("Package_TO_SOT_SMD:SOT-363_SC-70-6"),
    );

    assert_eq!(model.kind, ComponentKind::Opamp);
    assert_eq!(model.pins.get("1").map(String::as_str), Some("ref"));
    assert_eq!(model.pins.get("4").map(String::as_str), Some("in_plus"));
    assert_eq!(model.pins.get("5").map(String::as_str), Some("in_minus"));
    assert_eq!(model.pins.get("6").map(String::as_str), Some("out"));
    assert_eq!(model.params.get_f64("gain"), Some(25.0));
    assert_eq!(model.params.get_f64("pole_hz"), Some(45_000.0));
}
