use super::assert_pins;
use hauksbee_models::ModelEntry;

fn resolve(value: &str, footprint: &str) -> ModelEntry {
    super::expect_query(Some(value), None, Some(footprint))
}

fn resolved_id(value: &str, footprint: &str) -> Option<String> {
    super::query_id(Some(value), None, Some(footprint))
}

#[test]
fn single_diode_order_codes_select_the_body_card_from_the_footprint() {
    for (value, footprint, expected) in [
        ("BAT54XV2T1G", "Diode_SMD:D_SOD-523", "bat54_2pin"),
        ("BAT54", "Diode_SMD:D_SOT-23", "bat54"),
        ("BAT54W", "Diode_SMD:D_SOT-323", "bat54"),
        ("BAT54W", "Diode_SMD:D_SOD-123", "bat54_2pin"),
        ("BAT54WS-7-F", "Diode_SMD:D_SOD-323", "bat54_2pin"),
        ("BAT54-7-F", "Diode_SMD:D_SOT-23", "bat54"),
        ("BAT54HT1G", "Diode_SMD:D_SOD-323", "bat54_2pin"),
        ("BAT54LT1G", "Diode_SMD:D_SOT-23", "bat54"),
    ] {
        assert_eq!(
            resolve(value, footprint).id,
            expected,
            "{value} / {footprint}"
        );
    }
}

#[test]
fn dual_variants_are_excluded_from_bat54_cards() {
    for value in [
        "BAT54A",
        "BAT54C",
        "BAT54S",
        "BAT54AWT1G",
        "BAT54CT1G",
        "BAT54SW",
    ] {
        for footprint in ["Diode_SMD:D_SOT-23", "Diode_SMD:D_SOD-323"] {
            let id = resolved_id(value, footprint);
            assert_ne!(id.as_deref(), Some("bat54"), "{value} / {footprint}");
            assert_ne!(id.as_deref(), Some("bat54_2pin"), "{value} / {footprint}");
        }
    }
}

#[test]
fn two_pin_card_reverses_the_sod_body_pad_roles() {
    let model = resolve("BAT54XV2T1G", "Diode_SMD:D_SOD-523");
    assert_pins(&model, &[("1", "cathode"), ("2", "anode")]);
}

#[test]
fn both_cards_share_the_die_parameters_but_two_pin_has_lower_tj_max() {
    let three_pin = resolve("BAT54", "Diode_SMD:D_SOT-23");
    let two_pin = resolve("BAT54XV2T1G", "Diode_SMD:D_SOD-523");

    for key in ["is", "rs", "bv"] {
        assert_eq!(
            three_pin.params.get_f64(key),
            two_pin.params.get_f64(key),
            "{key}"
        );
    }
    assert_eq!(three_pin.ratings.max_junction_temp_c, Some(150.0));
    assert_eq!(two_pin.ratings.max_junction_temp_c, Some(125.0));
}
