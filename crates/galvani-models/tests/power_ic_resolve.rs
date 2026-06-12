//! The behavioural power-IC models resolve and carry their behavioural blocks.

use galvani_models::{ComponentQuery, ModelLibrary};

#[test]
fn power_ics_resolve_and_carry_behavioral() {
    let lib = ModelLibrary::builtin();
    for (val, id) in [
        ("LTC4020EUHFPBF", "ltc4020"),
        ("LTC6803-4", "ltc6803_4"),
        ("nPM1300-QEXX", "npm1300"),
    ] {
        let q = ComponentQuery {
            value: Some(val.into()),
            ..Default::default()
        };
        let r = lib.resolve(&q);
        let m = r.model.unwrap_or_else(|| panic!("{val} did not resolve"));
        assert_eq!(m.id, id, "{val} resolved to wrong id");
        assert!(!m.behavioral.is_empty(), "{val} must carry a behavioural block");
    }

    // LTC4020 converter reads R8 (ILIMIT) and R49 (input shunt) off the board.
    let q = ComponentQuery {
        value: Some("LTC4020EUHFPBF".into()),
        ..Default::default()
    };
    let m = lib.resolve(&q).model.unwrap();
    let conv = m.behavioral.converter.as_ref().expect("converter");
    let sp = conv.iin_program.as_ref().expect("iin_program");
    assert_eq!(sp.prog_ref.as_deref(), Some("R8"));
    assert_eq!(sp.rsense_ref.as_deref(), Some("R49"));
    assert_eq!(m.behavioral.fsm.as_ref().unwrap().states.len(), 2);

    // nPM1300 SHPHLD pulls to vsys.
    let q = ComponentQuery {
        value: Some("nPM1300-QEXX".into()),
        ..Default::default()
    };
    let m = lib.resolve(&q).model.unwrap();
    let pin = m.behavioral.pins.get("shphld").expect("shphld pin");
    assert_eq!(pin.pull_to.as_deref(), Some("vsys"));

    // LTC6803 leak law reads the tie resistor by ref.
    let q = ComponentQuery {
        value: Some("LTC6803-4".into()),
        ..Default::default()
    };
    let m = lib.resolve(&q).model.unwrap();
    assert_eq!(m.behavioral.laws.len(), 1);
    assert_eq!(m.behavioral.laws[0].name, "absent_cell_leak");
    assert_eq!(m.params.get_str("tie_ohms_from_ref"), Some("R52"));
}
