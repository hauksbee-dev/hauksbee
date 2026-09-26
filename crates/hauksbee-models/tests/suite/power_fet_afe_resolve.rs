//! Focused resolution tests for the power-FET / AFE model coverage added in
//! item 6 of the tooling-gap fix.
//!
//! Each assertion checks:
//!  - the part resolves (model is Some),
//!  - it resolves to the expected model id,
//!  - it has the expected `ComponentKind`,
//!  - the key ratings (`max_voltage_v`, `max_current_a` where applicable) are
//!    present and within a sane range derived from the datasheet.
//!
//! The generic power-FET fallback is also checked: an unknown FET value in a
//! DPAK footprint should bind by footprint when no value match exists.

use super::{assert_pins, expect_model, expect_query};
use hauksbee_models::ComponentKind::{self, Digital, Nmos, Opamp, Pmos};

/// A datasheet-derived sanity bound on one rating, `None` when unchecked.
type Bound = Option<fn(f64) -> bool>;

/// (value spellings, entry id, kind, max voltage, max current, max junction temp)
type Row = (
    &'static [&'static str],
    &'static str,
    ComponentKind,
    Bound,
    Bound,
    Bound,
);

// ── 6b/6c: Specific datasheet-sourced FETs, AFE, gate drivers, CSAs ───────────

/// Every value spelling resolves to its entry, with its kind, and each checked
/// rating (`max_voltage_v`, `max_current_a`, `max_junction_temp_c`) is present and
/// inside the bound its datasheet figure (in the row comment) implies.
#[test]
fn specific_parts_resolve_with_their_kind_and_datasheet_ratings() {
    let rows: [Row; 8] = [
        // 100 V, 100 A
        (
            &["IPA045N10N3G"],
            "ipa045n10n3g",
            Nmos,
            Some(|v| (90.0..=110.0).contains(&v)),
            Some(|i| i > 50.0),
            None,
        ),
        // 30 V, 9.2 A
        (
            &["IRF9358"],
            "irf9358",
            Pmos,
            Some(|v| (25.0..=35.0).contains(&v)),
            Some(|i| (5.0..=15.0).contains(&i)),
            None,
        ),
        // 100 V, 21 A
        (
            &["SIR182DP"],
            "sir182dp",
            Nmos,
            Some(|v| (90.0..=110.0).contains(&v)),
            Some(|i| (15.0..=30.0).contains(&i)),
            None,
        ),
        // 85 V stack, Tj >= 100 C
        (
            &["bq76952"],
            "bq76952",
            Digital,
            Some(|v| v >= 80.0),
            None,
            Some(|t| t >= 100.0),
        ),
        // VDD abs max 18 V; the mppt-1210 board uses the LM5109BMA variant
        (
            &["LM5109", "LM5109BMA", "LM5109BSD"],
            "lm5109",
            Digital,
            Some(|v| (12.0..=18.0).contains(&v)),
            None,
            None,
        ),
        (
            &["LM5107", "LM5107SD", "LM5107SDX"],
            "lm5107",
            Digital,
            None,
            None,
            None,
        ),
        // VS abs max 6 V (26 V is the IN+/IN- common-mode row), SBOS793H 6.1
        (
            &["INA181", "INA181A1", "INA181A3", "INA181A4IDBVR"],
            "ina181",
            Opamp,
            Some(|v| (5.5..=6.5).contains(&v)),
            None,
            None,
        ),
        // dual CSA, 6 V supply abs max (SBOS793H 6.1)
        (
            &["INA2181", "INA2181A1", "INA2181A3IDGSR"],
            "ina2181",
            Opamp,
            Some(|v| (5.5..=6.5).contains(&v)),
            None,
            None,
        ),
    ];
    for (values, id, kind, vmax, imax, tj) in rows {
        for &val in values {
            let m = expect_model(val);
            assert_eq!(m.id, id, "{val} must resolve to {id}");
            assert_eq!(m.kind, kind, "{val} must be {kind:?}");
            let r = &m.ratings;
            for (name, bound, got) in [
                ("max_voltage_v", vmax, r.max_voltage_v),
                ("max_current_a", imax, r.max_current_a),
                ("max_junction_temp_c", tj, r.max_junction_temp_c),
            ] {
                if let Some(ok) = bound {
                    let got = got.unwrap_or_else(|| panic!("{val}: {name} must be present"));
                    assert!(
                        ok(got),
                        "{val}: {name} {got} is outside its datasheet bound"
                    );
                }
            }
        }
    }
}

#[test]
fn fds4435bz_resolves_exact_grouped_so8_and_reproduces_datasheet_dc_points() {
    let m = expect_query(Some("FDS4435BZ"), None, Some("Pedalboard Library:SO08-E3"));
    assert_eq!(m.id, "fds4435bz");
    assert_eq!(m.kind, Pmos);
    assert_pins(&m, &[("1-3", "source"), ("4", "gate"), ("5-8", "drain")]);

    // The card's recorded derivation uses the level-1 channel resistance plus
    // the sourced ohmic residue. Pin the two fit rows and the independent gFS
    // cross-check so a later cleanup cannot silently turn this into a generic
    // SO-8 estimate.
    let vto = m.params.get_f64("vto").unwrap().abs();
    let kp = m.params.get_f64("kp").unwrap();
    let residue = m.params.get_f64("rd").unwrap() + m.params.get_f64("rs").unwrap();
    let rds = |vgs: f64| 1.0 / (kp * (vgs - vto)) + residue;
    assert!((rds(10.0) - 0.016).abs() < 2e-4);
    assert!((rds(4.5) - 0.026).abs() < 2e-4);
    let gfs = (2.0 * kp * 8.8).sqrt();
    assert!((gfs - 24.0).abs() / 24.0 < 0.07);
    assert!(m
        .coverage
        .implements
        .contains(&"dc_channel_conduction".into()));
    assert!(m.coverage.missing.contains(&"switching_loss".into()));
}

// ── 6a: Generic power-FET fallback ────────────────────────────────────────────

#[test]
fn generic_nmos_fallback_binds_unknown_fet_in_dpak_footprint() {
    // An unknown N-ch FET with a DPAK footprint: no value match exists, but
    // the generic_nmos_power_pkg catch-all should bind by footprint.
    let m = expect_query(
        Some("UNKNOWN_FET_XYZ_N"),
        None,
        Some("Package_TO_SOT_SMD:TO-252-2"),
    );
    assert_eq!(
        m.id, "generic_nmos_power_pkg",
        "unknown N-ch FET in DPAK should bind to generic_nmos_power_pkg, got '{}'",
        m.id
    );
    assert_eq!(m.kind, Nmos);

    // Ratings must be present (the whole point of the fallback is to enable Tj computation)
    assert!(
        m.ratings.max_voltage_v.is_some(),
        "generic fallback must have max_voltage_v for thermal modeling"
    );
    assert!(
        m.ratings.theta_ja_c_per_w.is_some(),
        "generic fallback must have theta_ja_c_per_w for Tj estimation"
    );
}

#[test]
fn specific_fet_beats_generic_fallback() {
    // When both the generic fallback footprint and a specific value match fire,
    // the specific value entry should win (higher specificity score).
    let m = expect_query(
        Some("SIR182DP"),
        None,
        Some("Package_TO_SOT_SMD:PowerPAK_SO-8"),
    );
    assert_eq!(
        m.id, "sir182dp",
        "specific SIR182DP entry must beat the generic fallback when both match"
    );
}
