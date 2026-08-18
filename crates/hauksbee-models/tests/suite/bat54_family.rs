use hauksbee_models::{ComponentQuery, ModelEntry, ModelLibrary};

fn resolve(value: &str, footprint: &str) -> ModelEntry {
    ModelLibrary::builtin()
        .resolve(&ComponentQuery {
            value: Some(value.to_string()),
            footprint: Some(footprint.to_string()),
            ..Default::default()
        })
        .model
        .unwrap_or_else(|| panic!("{value} / {footprint} did not resolve to any model"))
}

fn resolved_id(value: &str, footprint: &str) -> Option<String> {
    ModelLibrary::builtin()
        .resolve(&ComponentQuery {
            value: Some(value.to_string()),
            footprint: Some(footprint.to_string()),
            ..Default::default()
        })
        .model
        .map(|model| model.id)
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
    assert_eq!(model.pins.get("1").map(String::as_str), Some("cathode"));
    assert_eq!(model.pins.get("2").map(String::as_str), Some("anode"));
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
