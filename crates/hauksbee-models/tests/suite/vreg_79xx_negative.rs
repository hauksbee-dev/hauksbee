//! The 79xx negative regulators, and the pin map that is the reason they exist
//! in this library.
//!
//! A 7805 in TO-220 is pin 1 = IN, 2 = GND, 3 = OUT. A 7905 is pin 1 = GND,
//! 2 = IN, 3 = OUT. Dropping a negative regulator into a footprint drawn for
//! the positive one is one of the most common ways a dual-rail supply gets
//! built wrong, and it is invisible in a schematic that names both parts "78xx
//! family". If this map ever drifts to match the 78xx, hauksbee stops being
//! able to see that mistake and starts endorsing it.

use hauksbee_models::{ComponentQuery, ModelLibrary};

fn resolve(value: &str) -> hauksbee_models::ModelEntry {
    let lib = ModelLibrary::builtin();
    let q = ComponentQuery {
        value: Some(value.into()),
        ..Default::default()
    };
    lib.resolve(&q)
        .model
        .unwrap_or_else(|| panic!("{value} did not resolve to any model"))
}

#[test]
fn each_negative_code_resolves_to_its_own_voltage() {
    for (val, id, vout) in [
        ("7905", "7905", -5.0),
        ("LM7905CT", "7905", -5.0),
        ("79L05", "7905", -5.0),
        ("L7905CV", "7905", -5.0),
        ("7912", "7912", -12.0),
        ("LM7912", "7912", -12.0),
        ("79M12", "7912", -12.0),
        ("7915", "7915", -15.0),
        ("L7915CV", "7915", -15.0),
    ] {
        let m = resolve(val);
        assert_eq!(m.id, id, "{val} resolved to the wrong model id");
        assert_eq!(
            m.params.get_f64("vout"),
            Some(vout),
            "{val} resolved with the wrong output voltage"
        );
    }
}

#[test]
fn the_rail_is_actually_below_ground() {
    // The whole point of a negative regulator. A positive `vout` here would
    // stamp a rail above ground and quietly turn every op-amp's negative supply
    // into a second positive one.
    for id in ["7905", "7912", "7915"] {
        let m = resolve(id);
        let v = m.params.get_f64("vout").expect("vout");
        assert!(v < 0.0, "{id} must regulate below ground, got {v} V");
    }
}

#[test]
fn the_pinout_is_not_the_78xx_pinout() {
    // TI SNOSBQ7C Figure 1, un-rotated: 1 = GROUND, 2 = INPUT, 3 = OUTPUT.
    for id in ["7905", "7912", "7915"] {
        let m = resolve(id);
        assert_eq!(
            m.pins.get("1").map(String::as_str),
            Some("gnd"),
            "{id} pin 1"
        );
        assert_eq!(
            m.pins.get("2").map(String::as_str),
            Some("in"),
            "{id} pin 2"
        );
        assert_eq!(
            m.pins.get("3").map(String::as_str),
            Some("out"),
            "{id} pin 3"
        );
    }

    // And the positive part it is confused with really does differ, so this is
    // a live distinction rather than two copies of the same map.
    let pos = resolve("7805");
    assert_eq!(pos.pins.get("1").map(String::as_str), Some("in"));
    assert_eq!(pos.pins.get("2").map(String::as_str), Some("gnd"));
    assert_eq!(pos.pins.get("3").map(String::as_str), Some("out"));
}

#[test]
fn a_negative_code_never_resolves_to_the_positive_part() {
    // The regexes live next to each other and both end in the voltage code, so
    // a loose one would silently regulate a -12 V rail to +12 V.
    for val in ["7905", "7912", "7915", "LM7905", "L7912CV"] {
        let m = resolve(val);
        assert!(
            m.id.starts_with("79"),
            "{val} resolved to {}, a positive regulator",
            m.id
        );
    }
    for val in ["7805", "7812", "7815"] {
        let m = resolve(val);
        assert!(
            m.id.starts_with("78"),
            "{val} resolved to {}, a negative regulator",
            m.id
        );
    }
}
