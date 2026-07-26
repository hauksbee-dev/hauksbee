//! Focused resolution tests for the power-FET / AFE model coverage added in
//! item 6 of the tooling-gap fix.
//!
//! Each assertion checks:
//!  - the part resolves (model is Some),
//!  - it resolves to the expected model id,
//!  - it has the expected ComponentKind,
//!  - the key ratings (max_voltage_v, max_current_a where applicable) are
//!    present and within a sane range derived from the datasheet.
//!
//! The generic power-FET fallback is also checked: an unknown FET value in a
//! DPAK footprint should bind by footprint when no value match exists.

use hauksbee_models::{ComponentKind, ComponentQuery, ModelLibrary};

fn lib() -> ModelLibrary {
    ModelLibrary::builtin()
}

// ── 6b: Specific datasheet-sourced FET entries ────────────────────────────────

#[test]
fn ipa045n10n3g_resolves_nmos_with_correct_ratings() {
    let lib = lib();
    let q = ComponentQuery {
        value: Some("IPA045N10N3G".into()),
        ..Default::default()
    };
    let r = lib.resolve(&q);
    let m = r.model.expect("IPA045N10N3G must resolve");
    assert_eq!(m.id, "ipa045n10n3g");
    assert_eq!(m.kind, ComponentKind::Nmos, "IPA045N10N3G must be nmos");

    let vmax = m
        .ratings
        .max_voltage_v
        .expect("IPA045N10N3G: max_voltage_v must be present");
    assert!(
        (90.0..=110.0).contains(&vmax),
        "IPA045N10N3G: max_voltage_v should be ~100V, got {vmax}"
    );

    let imax = m
        .ratings
        .max_current_a
        .expect("IPA045N10N3G: max_current_a must be present");
    assert!(
        imax > 50.0,
        "IPA045N10N3G: max_current_a should be >50A (datasheet 100A), got {imax}"
    );
}

#[test]
fn irf9358_resolves_pmos_with_correct_ratings() {
    let lib = lib();
    let q = ComponentQuery {
        value: Some("IRF9358".into()),
        ..Default::default()
    };
    let r = lib.resolve(&q);
    let m = r.model.expect("IRF9358 must resolve");
    assert_eq!(m.id, "irf9358");
    assert_eq!(m.kind, ComponentKind::Pmos, "IRF9358 must be pmos");

    let vmax = m
        .ratings
        .max_voltage_v
        .expect("IRF9358: max_voltage_v must be present");
    assert!(
        (25.0..=35.0).contains(&vmax),
        "IRF9358: max_voltage_v should be ~30V, got {vmax}"
    );

    let imax = m
        .ratings
        .max_current_a
        .expect("IRF9358: max_current_a must be present");
    assert!(
        (5.0..=15.0).contains(&imax),
        "IRF9358: max_current_a should be ~9.2A, got {imax}"
    );
}

#[test]
fn sir182dp_resolves_nmos_with_correct_ratings() {
    let lib = lib();
    let q = ComponentQuery {
        value: Some("SIR182DP".into()),
        ..Default::default()
    };
    let r = lib.resolve(&q);
    let m = r.model.expect("SIR182DP must resolve");
    assert_eq!(m.id, "sir182dp");
    assert_eq!(m.kind, ComponentKind::Nmos, "SIR182DP must be nmos");

    let vmax = m
        .ratings
        .max_voltage_v
        .expect("SIR182DP: max_voltage_v must be present");
    assert!(
        (90.0..=110.0).contains(&vmax),
        "SIR182DP: max_voltage_v should be ~100V, got {vmax}"
    );

    let imax = m
        .ratings
        .max_current_a
        .expect("SIR182DP: max_current_a must be present");
    assert!(
        (15.0..=30.0).contains(&imax),
        "SIR182DP: max_current_a should be ~21A, got {imax}"
    );
}

// ── 6c: AFE + gate drivers + current-sense amps ───────────────────────────────

#[test]
fn bq76952_resolves_with_stack_voltage_rating() {
    let lib = lib();
    let q = ComponentQuery {
        value: Some("bq76952".into()),
        ..Default::default()
    };
    let r = lib.resolve(&q);
    let m = r.model.expect("bq76952 must resolve");
    assert_eq!(m.id, "bq76952");
    assert_eq!(
        m.kind,
        ComponentKind::Digital,
        "bq76952 must be digital kind"
    );

    let vmax = m
        .ratings
        .max_voltage_v
        .expect("bq76952: max_voltage_v must be present");
    assert!(
        vmax >= 80.0,
        "bq76952: max_voltage_v should be >=80V (datasheet 85V), got {vmax}"
    );

    let tj = m
        .ratings
        .max_junction_temp_c
        .expect("bq76952: max_junction_temp_c must be present");
    assert!(
        tj >= 100.0,
        "bq76952: max_junction_temp_c should be >=100C, got {tj}"
    );
}

#[test]
fn lm5109_resolves_as_digital() {
    let lib = lib();
    // The mppt-1210 board uses LM5109BMA variant
    for val in ["LM5109", "LM5109BMA", "LM5109BSD"] {
        let q = ComponentQuery {
            value: Some(val.into()),
            ..Default::default()
        };
        let r = lib.resolve(&q);
        let m = r.model.unwrap_or_else(|| panic!("{val} must resolve"));
        assert_eq!(m.id, "lm5109", "{val} must resolve to lm5109");
        assert_eq!(m.kind, ComponentKind::Digital, "{val} must be digital");

        let vmax = m
            .ratings
            .max_voltage_v
            .expect("{val}: max_voltage_v required");
        assert!(
            (12.0..=18.0).contains(&vmax),
            "{val}: VDD max_voltage_v should be ~15V, got {vmax}"
        );
    }
}

#[test]
fn lm5107_resolves_as_digital() {
    let lib = lib();
    for val in ["LM5107", "LM5107SD", "LM5107SDX"] {
        let q = ComponentQuery {
            value: Some(val.into()),
            ..Default::default()
        };
        let r = lib.resolve(&q);
        let m = r.model.unwrap_or_else(|| panic!("{val} must resolve"));
        assert_eq!(m.id, "lm5107", "{val} must resolve to lm5107");
        assert_eq!(m.kind, ComponentKind::Digital, "{val} must be digital");
    }
}

#[test]
fn ina181_resolves_as_opamp_with_supply_rating() {
    let lib = lib();
    for val in ["INA181", "INA181A1", "INA181A3", "INA181A4IDBVR"] {
        let q = ComponentQuery {
            value: Some(val.into()),
            ..Default::default()
        };
        let r = lib.resolve(&q);
        let m = r.model.unwrap_or_else(|| panic!("{val} must resolve"));
        assert_eq!(m.id, "ina181", "{val} must resolve to ina181");
        assert_eq!(m.kind, ComponentKind::Opamp, "{val} must be opamp");

        let vmax = m
            .ratings
            .max_voltage_v
            .expect("{val}: max_voltage_v required");
        assert!(
            (24.0..=28.0).contains(&vmax),
            "{val}: max_voltage_v should be ~26V, got {vmax}"
        );
    }
}

#[test]
fn ina2181_resolves_dual_opamp_with_supply_rating() {
    let lib = lib();
    for val in ["INA2181", "INA2181A1", "INA2181A3IDGSR"] {
        let q = ComponentQuery {
            value: Some(val.into()),
            ..Default::default()
        };
        let r = lib.resolve(&q);
        let m = r.model.unwrap_or_else(|| panic!("{val} must resolve"));
        assert_eq!(m.id, "ina2181", "{val} must resolve to ina2181");
        assert_eq!(
            m.kind,
            ComponentKind::Opamp,
            "{val} must be opamp (dual CSA)"
        );

        let vmax = m
            .ratings
            .max_voltage_v
            .expect("{val}: max_voltage_v required");
        assert!(
            (24.0..=28.0).contains(&vmax),
            "{val}: max_voltage_v should be ~26V, got {vmax}"
        );
    }
}

// ── 6a: Generic power-FET fallback ────────────────────────────────────────────

#[test]
fn generic_nmos_fallback_binds_unknown_fet_in_dpak_footprint() {
    let lib = lib();
    // An unknown N-ch FET with a DPAK footprint: no value match exists, but
    // the generic_nmos_power_pkg catch-all should bind by footprint.
    let q = ComponentQuery {
        value: Some("UNKNOWN_FET_XYZ_N".into()),
        footprint: Some("Package_TO_SOT_SMD:TO-252-2".into()),
        ..Default::default()
    };
    let r = lib.resolve(&q);
    let m = r
        .model
        .expect("unknown FET in DPAK footprint should bind to generic fallback");
    assert_eq!(
        m.id, "generic_nmos_power_pkg",
        "unknown N-ch FET in DPAK should bind to generic_nmos_power_pkg, got '{}'",
        m.id
    );
    assert_eq!(m.kind, ComponentKind::Nmos);

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
    let lib = lib();
    let q = ComponentQuery {
        value: Some("SIR182DP".into()),
        footprint: Some("Package_TO_SOT_SMD:PowerPAK_SO-8".into()),
        ..Default::default()
    };
    let r = lib.resolve(&q);
    let m = r.model.expect("SIR182DP with DPAK footprint must resolve");
    assert_eq!(
        m.id, "sir182dp",
        "specific SIR182DP entry must beat the generic fallback when both match"
    );
}
