//! The TI LM1117 cards against the TI sheet (SNOS412Q), and their separation
//! from the AMS1117 cards.
//!
//! The two families share a pinout and a name but not an output-capacitor rule:
//! TI p.1 "A minimum of 10-µF tantalum capacitor is required at the output to
//! improve the transient response and stability" is a MINIMUM, while the AMS1117
//! sheet's 22 µF is a recommendation. A value string "LM1117-3.3" therefore must
//! not bind the AMS card, and an "AMS1117-3.3" must not bind the TI one.

use super::*;

/// (value, card id, vout) for the SOT-223 / TO-252 / TO-263 / TO-220 order codes.
/// Table 6-1 p.3: ADJ/GND 1, VOUT 2 (and the tab), VIN 3.
#[test]
fn lm1117_values_bind_the_ti_card_with_its_output_capacitor_minimum() {
    let rows = [
        ("LM1117-1.8", "lm1117_1v8", 1.8),
        ("LM1117-2.5", "lm1117_2v5", 2.5),
        ("LM1117-3.3", "lm1117_3v3", 3.3),
        ("LM1117MP-3.3", "lm1117_3v3", 3.3),
        ("LM1117MPX-3.3_NOPB", "lm1117_3v3", 3.3),
        ("LM1117MPX-3.3/NOPB", "lm1117_3v3", 3.3),
        ("LM1117IMP-3.3", "lm1117_3v3", 3.3),
        ("LM1117DT-3.3", "lm1117_3v3", 3.3),
        ("LM1117T-3.3", "lm1117_3v3", 3.3),
        ("LM1117-3V3", "lm1117_3v3", 3.3),
        ("LM1117-5.0", "lm1117_5v", 5.0),
        ("LM1117S-5.0", "lm1117_5v", 5.0),
    ];
    for (value, id, vout) in rows {
        let m = expect_model(value);
        assert_eq!(m.id, id, "{value} bound the wrong card");
        assert!((param(&m, "vout") - vout).abs() < 1e-9, "{value}: vout");
        assert!(
            (param(&m, "cout_min_f") - 10.0e-6).abs() < 1e-12,
            "{value}: the 10 uF tantalum minimum (p.1, 9.2.2.1.3) must be a gating minimum"
        );
        assert!(
            m.params.get_f64("cout_recommended_f").is_none(),
            "{value}: the TI sheet states a minimum, not a recommendation"
        );
        assert_pins(&m, &pins("1=gnd 2=out 3=in 4=out"));
    }
    // The adjustable part carries the same minimum and maps pin 1 as ADJ.
    for value in ["LM1117", "LM1117-ADJ", "LM1117MPX-ADJ/NOPB"] {
        let m = expect_model(value);
        assert_eq!(m.id, "lm1117_adj", "{value}");
        assert!((param(&m, "cout_min_f") - 10.0e-6).abs() < 1e-12, "{value}");
        assert_pins(&m, &pins("1=fb 2=out 3=in"));
        assert!(m.params.get_f64("vout").is_none(), "{value}: divider-set");
    }
}

/// The AMS1117 sheet is a different part: its cards keep the 22 µF
/// recommendation and no longer answer to an LM1117 value.
#[test]
fn ams1117_values_keep_their_own_cards() {
    for (value, id) in [
        ("AMS1117-3.3", "ams1117_3v3"),
        ("AMS1117-5.0", "ams1117_5v"),
    ] {
        let m = expect_model(value);
        assert_eq!(m.id, id, "{value}");
        assert!(m.params.get_f64("cout_min_f").is_none(), "{value}");
    }
    // The 3.3 V card records the sheet's 22 µF tantalum recommendation (p.4).
    let m = expect_model("AMS1117-3.3");
    assert!((param(&m, "cout_recommended_f") - 22.0e-6).abs() < 1e-12);
    for value in ["LM1117-3.3", "LM1117-5.0", "LM1117MPX-3.3_NOPB"] {
        let m = expect_model(value);
        assert!(
            !m.id.starts_with("ams1117"),
            "{value} must not bind an AMS1117 card, got {}",
            m.id
        );
    }
}

/// The WSON order codes (LM1117LD) number their pins ADJ/GND 1, VIN 2-4,
/// VOUT 5-7: reading them with the three-lead map would be wrong, so no
/// LM1117 or AMS1117 card may bind them.
#[test]
fn the_wson_order_codes_are_not_read_with_the_three_lead_map() {
    for value in ["LM1117LD-3.3", "LM1117LDX-ADJ", "LM1117LD"] {
        if let Some(m) = resolve_value(value).model {
            assert!(
                !m.id.starts_with("lm1117") && !m.id.starts_with("ams1117"),
                "{value} bound {} with the TO/SOT pin map",
                m.id
            );
        }
    }
}

/// The ratings are the TI figures: 20 V absolute maximum, 15 V operating, and
/// a per-package `RθJA` so a TO-252 footprint is not judged on the SOT-223 figure.
#[test]
fn lm1117_ratings_and_packages_are_the_ti_figures() {
    let m = expect_model("LM1117-3.3");
    assert_eq!(m.ratings.max_current_a, Some(0.8));
    assert_eq!(m.ratings.max_voltage_v, Some(20.0));
    assert!(has_supply_range(
        &m,
        "in",
        (0.0, 15.0),
        Some(20.0),
        "7.3 Recommended Operating Conditions"
    ));
    // The same envelope row carries the Absolute Maximum Ratings basis too.
    assert!(m.envelope.iter().any(|e| matches!(
        e,
        OperatingEnvelope::SupplyRange { abs_max_basis, .. }
            if abs_max_basis.contains("7.1 Absolute Maximum Ratings")
    )));
    // A resolved card drops its cited rows, so read them from the card as written.
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db/vreg.toml");
    let text = std::fs::read_to_string(path).expect("vreg.toml readable");
    let db: hauksbee_models::schema::DbFile = toml::from_str(&text).expect("vreg.toml parses");
    let written = db
        .models
        .into_iter()
        .find(|e| e.id == m.id)
        .expect("card in vreg.toml");
    let theta: Vec<(String, f64)> = written
        .source
        .as_ref()
        .expect("source block")
        .characteristic
        .iter()
        .filter(|c| c.rating.as_deref() == Some("theta_ja_c_per_w"))
        .map(|c| (c.package.clone().unwrap_or_default(), c.max.unwrap_or(0.0)))
        .collect();
    for (pkg, v) in [
        ("SOT-223", 61.6),
        ("TO-252", 45.1),
        ("TO-263", 41.3),
        ("TO-220", 23.8),
    ] {
        assert!(
            theta.iter().any(|(p, t)| p == pkg && (*t - v).abs() < 1e-9),
            "{pkg}: RθJA {v} °C/W (7.4) missing from {theta:?}"
        );
    }
}
