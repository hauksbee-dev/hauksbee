//! 1N4148 package variants bind to cards carrying their own package ratings, and
//! an unlisted W-package suffix assumes the smallest listed package's ratings
//! rather than inheriting the DO-35 card's.

fn card(value: &str) -> hauksbee_models::ModelEntry {
    super::expect_model(value)
}

/// Every entry of the shipped `diodes.toml`, as written. A resolved card drops
/// its cited rows, so the rows a rating comes from are read here.
fn diodes_db() -> Vec<hauksbee_models::ModelEntry> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("db/diodes.toml");
    let text = std::fs::read_to_string(path).expect("diodes.toml readable");
    let db: hauksbee_models::schema::DbFile = toml::from_str(&text).expect("diodes.toml parses");
    db.models
}

/// The entry `value` resolves to, with its cited rows (see [`diodes_db`]).
fn card_with_rows(value: &str) -> hauksbee_models::ModelEntry {
    let id = card(value).id;
    diodes_db()
        .into_iter()
        .find(|entry| entry.id == id)
        .unwrap_or_else(|| panic!("{id} is not in diodes.toml"))
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

/// The false-safe case: a steady load is held to the printed AVERAGE forward
/// current IF(AV), never to the continuous-IF (300 mA) or repetitive-peak IFRM
/// (500 mA) rows, which sit above it. The DO-35 card is Vishay 81857 Rev. 1.6
/// p.1 (IF(AV) 150 mA, IF 300 mA, IFRM 500 mA, VR 75 V, VRRM 100 V).
#[test]
fn do35_card_rates_steady_current_and_voltage_by_the_average_and_dc_rows() {
    let model = card_with_rows("1N4148");
    assert_eq!(model.ratings.max_current_a, Some(0.15));
    assert_eq!(model.ratings.max_voltage_v, Some(75.0));
    // The higher rows stay on the card as data, tagged with no rating key.
    let rows = &model
        .source
        .as_ref()
        .expect("the card cites its rows")
        .characteristic;
    let untagged_max = |parameter: &str| {
        rows.iter()
            .find(|c| c.parameter.starts_with(parameter))
            .map(|c| (c.rating.clone(), c.max))
    };
    assert_eq!(
        untagged_max("IF forward continuous"),
        Some((None, Some(0.3))),
        "{rows:#?}"
    );
    assert_eq!(untagged_max("IFRM"), Some((None, Some(0.5))), "{rows:#?}");
}

/// No card may rate a steady current above the average-current row its own
/// source prints: the shared guard against the peak/continuous-as-average slip.
#[test]
fn small_signal_cards_rate_by_their_printed_average_current_row() {
    for value in [
        "1N4148", "1N4148W", "1N4148WS", "1N4148WT", "LL4148", "1N4148WX",
    ] {
        let model = card_with_rows(value);
        let rated = model.ratings.max_current_a.expect(value);
        let rows = &model.source.as_ref().expect(value).characteristic;
        let average = rows
            .iter()
            .find(|c| {
                c.rating.as_deref() == Some("max_current_a")
                    && (c.parameter.contains("IF(AV)") || c.parameter.starts_with("IO "))
            })
            .unwrap_or_else(|| panic!("{value}: no average-current row tagged max_current_a"));
        assert_eq!(average.max, Some(rated), "{value}");
        for other in rows.iter().filter(|c| c.rating.is_none()) {
            if other.parameter.contains("forward") && other.parameter.contains("current") {
                assert!(
                    other.max.is_some_and(|m| m >= rated),
                    "{value}: untagged row '{}' is below the rating",
                    other.parameter
                );
            }
        }
    }
}

/// The unlisted-package fallback's breakdown is the SOD-523 sheet's own V(BR)R
/// row (min 75 V at 1 uA), carried as a row so `models lint` derives it rather
/// than reading 75 as the VR rating. The SOD-123 card keeps its 100 V.
#[test]
fn unlisted_w_package_breakdown_comes_from_its_own_row() {
    let fallback = card_with_rows("1N4148WX");
    assert_eq!(fallback.id, "1n4148w_unlisted_package");
    assert_eq!(fallback.params.get_f64("bv"), Some(75.0));
    let rows = &fallback.source.as_ref().unwrap().characteristic;
    assert!(
        rows.iter()
            .any(|c| c.parameter.starts_with("V(BR)R") && c.min == Some(75.0)),
        "the fallback must cite the V(BR)R row its bv comes from"
    );
    assert_eq!(card("1N4148W").params.get_f64("bv"), Some(100.0));
}

/// Every diode-kind card in the shipped diodes db passes the kind rules (the
/// required is/n/rs), which `models lint` reports as an ERROR. The LM4040
/// reference shipped without them.
#[test]
fn every_builtin_diode_card_has_its_required_forward_params() {
    let models = diodes_db();
    let mut bad = Vec::new();
    for entry in &models {
        if let Err(errors) = hauksbee_models::validation::validate(entry) {
            bad.push(format!("{}: {errors:?}", entry.id));
        }
    }
    assert!(models.len() > 40, "expected the whole diode db");
    assert!(bad.is_empty(), "diode cards failing validation: {bad:#?}");
    let lm4040 = models
        .iter()
        .find(|entry| entry.id == "lm4040_30_dbz")
        .expect("the LM4040 card is in diodes.toml");
    for key in ["is", "n", "rs", "bv"] {
        assert!(lm4040.params.get_f64(key).is_some(), "lm4040 lacks {key}");
    }
}

/// The false-safe case: no W-family value may be judged against the DO-35
/// card's rating; the SOD-523 part, and any package suffix not listed, stay at
/// or under its 125 mA.
#[test]
fn small_packages_never_inherit_the_do35_current_rating() {
    assert_eq!(card("1N4148").ratings.max_current_a, Some(0.15));
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
