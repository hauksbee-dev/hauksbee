//! Each 78xx voltage code must resolve to its own model with the correct output
//! voltage — the family must not collapse onto the +5V model (which silently
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
        let q = ComponentQuery { value: Some(val.into()), ..Default::default() };
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
