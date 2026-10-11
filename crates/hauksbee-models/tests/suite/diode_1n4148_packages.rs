//! 1N4148 package variants bind to cards carrying their own package ratings, and
//! an unlisted W-package suffix assumes the smallest listed package's ratings
//! rather than inheriting the DO-35 card's.

fn card(value: &str) -> hauksbee_models::ModelEntry {
    super::expect_model(value)
}

#[test]
fn package_variants_bind_to_their_own_cards() {
    for (value, want) in [
        ("1N4148", "1n4148"),
        ("1N4148TR", "1n4148"),
        ("1N4148-T", "1n4148"),
        ("1N4148W", "1n4148w"),
        ("1N4148W-7-F", "1n4148w"),
        ("1N4148WQ-13-F", "1n4148w"),
        ("1N4148W-HE3_A-08", "1n4148w"),
        ("1N4148WS", "1n4148ws"),
        ("1N4148WS-E3-08", "1n4148ws"),
        ("1N4148WT", "1n4148wt"),
        ("1N4148WT-7", "1n4148wt"),
        ("1N4148WTQ-7", "1n4148wt"),
        ("LL4148", "ll4148"),
        ("LL4148-GS08", "ll4148"),
    ] {
        assert_eq!(card(value).id, want, "{value}");
    }
}

/// The false-safe case: no W-family value may be judged against the DO-35
/// card's 300 mA; the SOD-523 part, and any package suffix not listed, stay at
/// or under its 125 mA.
#[test]
fn small_packages_never_inherit_the_do35_current_rating() {
    assert_eq!(card("1N4148").ratings.max_current_a, Some(0.3));
    for (value, amps) in [
        ("1N4148W", 0.25),
        ("1N4148WS", 0.15),
        ("1N4148WT", 0.125),
        ("LL4148", 0.15),
    ] {
        assert_eq!(card(value).ratings.max_current_a, Some(amps), "{value}");
    }
    for value in ["1N4148WX", "1N4148WTX-7", "1N4148WSXYZ", "1N4148W7"] {
        let model = card(value);
        assert_eq!(model.id, "1n4148w_unlisted_package", "{value}");
        assert_eq!(model.ratings.max_current_a, Some(0.125), "{value}");
        assert!(
            model
                .params
                .get_str("warning")
                .is_some_and(|w| w.contains("ASSUMED")),
            "{value} must say its ratings are assumed"
        );
    }
}
