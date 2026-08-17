//! The mito badge's open-part noise: nine "open parts", of which only the two
//! TXB0101 level translators were real modelling gaps. The other seven were
//! match-rule misses on parts with no device physics to miss: three panel
//! mouse-bites whose footprint spells the name with a hyphen, two fuses on a
//! copper-only board with no lib_id for the `Device:Fuse` rule to see, and two
//! chip antennas whose generic value cannot say whether the part is a DC open
//! or a DC short. Each case pins the classification that clears (or honestly
//! declines to clear) it.

use hauksbee_models::{ComponentKind, ComponentQuery, ModelLibrary};

#[test]
fn hyphenated_mouse_bite_footprint_is_ignored() {
    let lib = ModelLibrary::builtin();
    let resolved = lib.resolve(&ComponentQuery {
        value: Some("break off".into()),
        footprint: Some("panelization:mouse-bite-2mm-slot".into()),
        ..Default::default()
    });
    let model = resolved
        .model
        .expect("a hyphen-spelled mouse-bite tab must classify, not fall through");
    assert_eq!(model.id, "breakaway_tab");
    assert_eq!(model.kind, ComponentKind::Ignore);
}

#[test]
fn underscore_and_space_mouse_bite_spellings_still_match() {
    let lib = ModelLibrary::builtin();
    for fp in ["Panel:mouse_bite_5mm", "house:Mouse Bite v2", "tabs:MouseBite"] {
        let resolved = lib.resolve(&ComponentQuery {
            footprint: Some(fp.into()),
            ..Default::default()
        });
        let model = resolved
            .model
            .unwrap_or_else(|| panic!("{fp} must still classify as a breakaway tab"));
        assert_eq!(model.id, "breakaway_tab", "{fp}");
    }
}

#[test]
fn footprint_only_fuse_binds_the_low_ohm_wire() {
    // Copper-only extraction offers no lib_id, and "500mA" is a rating, not a
    // resistance. The footprint rule must still classify the part as a fuse
    // and carry the sourced DC value the binder's fuse path requires.
    let lib = ModelLibrary::builtin();
    let resolved = lib.resolve(&ComponentQuery {
        value: Some("500mA".into()),
        footprint: Some("Fuse:Fuse_0603_1608Metric".into()),
        ..Default::default()
    });
    let model = resolved
        .model
        .expect("a Fuse:-library footprint must resolve without a lib_id");
    assert_eq!(model.id, "fuse_footprint");
    assert_eq!(
        model.passive_class,
        Some(hauksbee_models::schema::PassiveClass::Fuse)
    );
    assert_eq!(model.params.get_f64("ohms"), Some(0.01));
}

#[test]
fn schematic_fuse_lib_id_still_wins_its_original_rule() {
    let lib = ModelLibrary::builtin();
    let resolved = lib.resolve(&ComponentQuery {
        value: Some("6V 0.5A".into()),
        lib_id: Some("Device:Fuse".into()),
        ..Default::default()
    });
    let model = resolved.model.expect("Device:Fuse still resolves");
    assert_eq!(model.passive_class, Some(hauksbee_models::schema::PassiveClass::Fuse));
}

#[test]
fn generic_chip_antenna_gets_a_named_abstention_not_a_guess() {
    let lib = ModelLibrary::builtin();
    // No model may bind: open vs short is construction-dependent and unknown.
    let resolved = lib.resolve(&ComponentQuery {
        value: Some("Antenna_Chip".into()),
        footprint: Some("2450AT42A100E:ANTC5020X110N".into()),
        ..Default::default()
    });
    assert!(
        resolved.model.is_none(),
        "a generic antenna label must not bind a DC guess"
    );
    let note = lib
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
    let lib = ModelLibrary::builtin();
    assert!(lib.unmodelled().note_for("Antenna_Chip", "").is_some());
    assert!(lib.unmodelled().note_for("antenna chip", "").is_some());
    assert!(lib.unmodelled().note_for("AntennaTuner_XYZ99", "").is_none());
}
