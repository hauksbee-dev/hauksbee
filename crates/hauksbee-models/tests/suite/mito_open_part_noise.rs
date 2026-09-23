//! The mito badge's open-part noise: nine "open parts", of which only the two
//! TXB0101 level translators were real modelling gaps. The other seven were
//! match-rule misses on parts with no device physics to miss: three panel
//! mouse-bites whose footprint spells the name with a hyphen, two fuses on a
//! copper-only board with no lib_id for the `Device:Fuse` rule to see, and two
//! chip antennas whose generic value cannot say whether the part is a DC open
//! or a DC short. Each case pins the classification that clears (or honestly
//! declines to clear) it.

use super::{assert_pins, expect_model, expect_query, has_supply_range, resolve_query};
use hauksbee_models::{schema::PassiveClass, ComponentKind, ModelLibrary, OperatingEnvelope};

#[test]
fn txb0101_card_has_sourced_pin_roles_and_operating_envelopes() {
    for value in ["TXB0101", "TXB0101DCK"] {
        let model = expect_model(value);
        assert_eq!(model.id, "txb0101_identity", "{value}");
        assert_eq!(model.kind, ComponentKind::Digital, "{value}");
        assert_eq!(model.params.get_bool("identity_only"), Some(true));
        assert_pins(&model, &[("1", "vcca"), ("6", "vccb")]);
        assert_eq!(model.envelope.len(), 3);

        let rec = "Section 5.3 Recommended Operating Conditions";
        assert!(has_supply_range(&model, "vcca", (1.2, 3.6), None, rec));
        assert!(has_supply_range(&model, "vccb", (1.65, 5.5), None, rec));
        assert!(model.envelope.iter().any(|entry| matches!(
            entry,
            OperatingEnvelope::RailOrder {
                lower,
                upper,
                basis,
                ..
            } if lower == "vcca"
                && upper == "vccb"
                && basis.contains("Section 5.3 Recommended Operating Conditions, note (2)")
        )));
    }
}

#[test]
fn hyphenated_mouse_bite_footprint_is_ignored() {
    // A hyphen-spelled mouse-bite tab must classify, not fall through.
    let model = expect_query(
        Some("break off"),
        None,
        Some("panelization:mouse-bite-2mm-slot"),
    );
    assert_eq!(model.id, "breakaway_tab");
    assert_eq!(model.kind, ComponentKind::Ignore);
}

#[test]
fn underscore_and_space_mouse_bite_spellings_still_match() {
    for fp in [
        "Panel:mouse_bite_5mm",
        "house:Mouse Bite v2",
        "tabs:MouseBite",
    ] {
        let model = expect_query(None, None, Some(fp));
        assert_eq!(model.id, "breakaway_tab", "{fp}");
    }
}

#[test]
fn footprint_only_fuse_binds_the_low_ohm_wire() {
    // Copper-only extraction offers no lib_id, and "500mA" is a rating, not a
    // resistance. The footprint rule must still classify the part as a fuse
    // and carry the sourced DC value the binder's fuse path requires.
    for (value, id) in [("", "fuse_footprint"), ("500mA", "fuse_rated")] {
        let model = expect_query(Some(value), None, Some("Fuse:Fuse_0603_1608Metric"));
        assert_eq!(model.id, id);
        assert_eq!(model.passive_class, Some(PassiveClass::Fuse));
        assert_eq!(model.params.get_f64("ohms"), Some(0.01));
    }
}

#[test]
fn schematic_fuse_lib_id_still_wins_its_original_rule() {
    let resolved = ModelLibrary::builtin_shared().resolve(&hauksbee_models::ComponentQuery {
        value: Some("6V 0.5A".into()),
        lib_id: Some("Device:Fuse".into()),
        ..Default::default()
    });
    let model = resolved.model.expect("Device:Fuse still resolves");
    assert_eq!(model.passive_class, Some(PassiveClass::Fuse));
}

#[test]
fn generic_chip_antenna_gets_a_named_abstention_not_a_guess() {
    // No model may bind: open vs short is construction-dependent and unknown.
    let resolved = resolve_query(
        Some("Antenna_Chip"),
        None,
        Some("2450AT42A100E:ANTC5020X110N"),
    );
    assert!(
        resolved.model.is_none(),
        "a generic antenna label must not bind a DC guess"
    );
    let note = ModelLibrary::builtin_shared()
        .unmodelled()
        .note_for("Antenna_Chip", "")
        .expect("the abstention must be named, not generic");
    assert!(note.because.contains("DC open"));
    assert!(note.unlocked_by.contains("part number"));
}

#[test]
fn antenna_prefixed_real_part_numbers_are_not_captured_by_the_abstention() {
    // The regex is anchored to the generic label; a real MPN starting with
    // "Antenna" in some house library must stay out of its blast radius.
    let lib = ModelLibrary::builtin_shared();
    assert!(lib.unmodelled().note_for("Antenna_Chip", "").is_some());
    assert!(lib.unmodelled().note_for("antenna chip", "").is_some());
    assert!(lib
        .unmodelled()
        .note_for("AntennaTuner_XYZ99", "")
        .is_none());
}
