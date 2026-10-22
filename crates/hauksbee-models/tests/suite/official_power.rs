//! Official-parts batch `power`: datasheet-derived cards in `db/power_regs.toml`
//! (AMS1117 other voltages, TLV700/TLV702, LP5907, LM2596 -3.3/-12/-ADJ,
//! MP1584, and the identity-only MT3608, XL6009 and IP5306). Each part binds its
//! own card, and its near siblings (other voltage codes, other packages, other
//! families) do not.

use super::*;

/// The card a value-and-MPN query (what a layout-only board produces) binds.
fn id_of(value: &str) -> Option<String> {
    query_id(Some(value), Some(value), None)
}

fn card(value: &str) -> ModelEntry {
    resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve"))
}

fn assert_binds(value: &str, id: &str) {
    assert_eq!(id_of(value).as_deref(), Some(id), "{value}");
}

fn assert_not_in(value: &str, ids: &[&str]) {
    let got = id_of(value);
    assert!(
        got.as_deref().is_none_or(|g| !ids.contains(&g)),
        "{value} must not bind any of {ids:?}, got {got:?}"
    );
}

fn assert_unresolved(value: &str) {
    assert_eq!(id_of(value), None, "{value} must stay unresolved");
}

// ── AMS1117 ──────────────────────────────────────────────────────────────

#[test]
fn ams1117_fixed_voltages_bind_their_own_sot223_card() {
    for (value, id, vout) in [
        ("AMS1117-1.5", "ams1117_1v5", 1.5),
        ("AMS1117-1.8", "ams1117_1v8", 1.8),
        ("AMS1117-2.5", "ams1117_2v5", 2.5),
        ("AMS1117-2.85", "ams1117_2v85", 2.85),
    ] {
        let m = card(value);
        assert_pin_map(&m, id, &pins("1=gnd 2=out 3=in"));
        assert!((param(&m, "vout") - vout).abs() < 1e-9, "{value}");
        assert!(has_supply_range(
            &m,
            "in",
            (0.0, 15.0),
            Some(15.0),
            "AMS1117"
        ));
        assert!(
            !m.references.is_empty() && m.references[0].sha256.is_some(),
            "{value}"
        );
    }
}

#[test]
fn ams1117_adjustable_has_a_reference_not_an_output() {
    let m = card("AMS1117-ADJ");
    assert_pin_map(&m, "ams1117_adj", &pins("1=fb 2=out 3=in"));
    assert!((param(&m, "vref") - 1.25).abs() < 1e-9);
    assert!(m.params.get_f64("vout").is_none());
}

#[test]
fn ams1117_cs_and_cd_packages_are_their_own_cards() {
    assert_binds("AMS1117CS-3.3", "ams1117cs_3v3");
    assert_binds("AMS1117CS-5.0", "ams1117cs_5v0");
    assert_binds("AMS1117CD-3.3", "ams1117cd_3v3");
    assert_binds("AMS1117CD-5.0", "ams1117cd_5v0");
    assert_binds("AMS1117CS-1.8", "ams1117cs_1v8");
    assert_pin_map(
        &card("AMS1117CS-3.3"),
        "ams1117cs_3v3",
        &pins("1=gnd 2=out 3=out 4=in 5=nc 6=out 7=out 8=nc"),
    );
}

#[test]
fn ams1117_siblings_do_not_resolve_to_the_wrong_voltage() {
    // The existing 3.3 V and 5.0 V cards still own their codes.
    assert_binds("AMS1117-3.3", "ams1117_3v3");
    assert_binds("AMS1117-5.0", "ams1117_5v");
    // 1.2 V is not printed in the datasheet: no card, not a neighbour's.
    assert_unresolved("AMS1117-1.2");
    // 2.5 and 2.85 stay apart, as do 1.5 and 1.8.
    assert_binds("AMS1117-2.85", "ams1117_2v85");
    assert_binds("AMS1117-2.5", "ams1117_2v5");
    assert_unresolved("AMS1117-2.8");
    assert_unresolved("AMS1117-1.85");
    // The TI LM1117 and other LDOs are not AMS cards.
    for v in ["LM1117-1.8", "LM1117-ADJ", "AMS1085-1.8", "AMS1118-1.8"] {
        let got = id_of(v);
        assert!(
            got.as_deref().is_none_or(|g| !g.starts_with("ams1117")),
            "{v} bound {got:?}"
        );
    }
}

// ── TLV700 / TLV702 ──────────────────────────────────────────────────────

#[test]
fn tlv700_sot23_and_sc70_share_the_five_pin_card() {
    for v in ["TLV70033DDCR", "TLV70033DCKR", "TLV70033DDCT", "TLV70033"] {
        let m = card(v);
        assert_pin_map(&m, "tlv70033", &pins("1=in 2=gnd 3=en 4=nc 5=out"));
    }
    let m = card("TLV70033DDCR");
    assert!((param(&m, "vout") - 3.3).abs() < 1e-9);
    assert!((param(&m, "dropout_v") - 0.25).abs() < 1e-9);
    assert!(has_supply_range(&m, "in", (2.0, 5.5), Some(6.0), "TLV700"));
}

#[test]
fn tlv700_wson_is_a_separate_six_pin_card() {
    let m = card("TLV70033DSER");
    assert_pin_map(&m, "tlv70033_dse", &pins("1=in 2=gnd 3=out 4=nc 5=nc 6=en"));
    // A bare code on a WSON footprint must not borrow the five-pin map.
    let got = query_id(
        Some("TLV70033"),
        Some("TLV70033"),
        Some("Package_SON:WSON-6-1EP_1.5x1.5mm_P0.5mm"),
    );
    assert_ne!(got.as_deref(), Some("tlv70033"));
}

#[test]
fn tlv700_below_2v35_prints_no_dropout() {
    let m = card("TLV70018DDCR");
    assert_eq!(m.id, "tlv70018");
    assert!(m.params.get_f64("dropout_v").is_none());
}

#[test]
fn tlv700_family_siblings_stay_apart() {
    // Other families and the 702 do not take a 700 card.
    assert_not_in("TLV75533PDRVR", &["tlv70033", "tlv70033_dse", "tlv70233"]);
    assert_binds("TLV70233DBVR", "tlv70233");
    assert_binds("TLV70233PDBVR", "tlv70233p");
    assert_binds("TLV70233DSER", "tlv70233_dse");
    // A package the family does not ship, or a code it does not list.
    assert_unresolved("TLV70033DBVR");
    assert_unresolved("TLV70233DDCR");
    assert_unresolved("TLV70033PDDCR");
    assert_unresolved("TLV70034DDCR");
    assert_unresolved("TLV70013DCKR");
    assert_binds("TLV70013DDCR", "tlv70013");
    assert_binds("TLV70012DCKR", "tlv70012");
    // The P (output discharge) variant carries the discharge resistance, the plain one does not.
    assert!((param(&card("TLV70233PDBVR"), "en_discharge_ohms") - 120.0).abs() < 1e-9);
    assert!(card("TLV70233DBVR")
        .params
        .get_f64("en_discharge_ohms")
        .is_none());
}

#[test]
fn tlv702_has_a_higher_dropout_and_current_than_the_tlv700() {
    let a = card("TLV70233DBVR");
    let b = card("TLV70033DDCR");
    assert!(param(&a, "dropout_v") > param(&b, "dropout_v"));
    assert!((param(&a, "vout") - 3.3).abs() < 1e-9);
    assert_binds("TLV702475DBVR", "tlv702475");
}

// ── LP5907 ───────────────────────────────────────────────────────────────

#[test]
fn lp5907_sot23_and_x2son_have_their_own_pinouts() {
    let m = card("LP5907MFX-3.3/NOPB");
    assert_pin_map(&m, "lp5907mf_3v3", &pins("1=in 2=gnd 3=en 4=nc 5=out"));
    assert!((param(&m, "vout") - 3.3).abs() < 1e-9);
    assert!((param(&m, "cout_min_f") - 0.7e-6).abs() < 1e-12);
    assert!(has_supply_range(&m, "in", (2.2, 5.5), Some(6.0), "LP5907"));
    let m = card("LP5907SNX-3.3/NOPB");
    assert_pin_map(&m, "lp5907sn_3v3", &pins("1=out 2=gnd 3=en 4=in"));
    assert_binds("LP5907SNX-1.9/NOPB", "lp5907sn_1v9");
    assert_binds("LP5907MFX-2.85/NOPB", "lp5907mf_2v85");
}

#[test]
fn lp5907_voltage_codes_belong_to_their_package_only() {
    // 1.9 V and 4.0 V are X2SON-only codes, 1.5 V and 3.1 V... exist only for the SOT-23.
    assert_unresolved("LP5907MFX-1.9/NOPB");
    assert_unresolved("LP5907MFX-4.0/NOPB");
    assert_unresolved("LP5907SNX-1.5/NOPB");
    assert_unresolved("LP5907MFX-3.35/NOPB");
    // 2.8 and 2.85 stay apart.
    assert_binds("LP5907MFX-2.8/NOPB", "lp5907mf_2v8");
    assert_binds("LP5907SNX-2.85/NOPB", "lp5907sn_2v85");
    // A bare orderable on an X2SON footprint must not take the SOT-23 map.
    let got = query_id(
        Some("LP5907-3.3"),
        Some("LP5907-3.3"),
        Some("Package_SON:X2SON-4_1x1mm_P0.65mm"),
    );
    assert_ne!(got.as_deref(), Some("lp5907mf_3v3"));
    // Other TI LDOs are not LP5907 cards.
    for v in ["LP5912-3.3", "LP5900SD-3.3", "LP2985-3.3", "TLV70033DDCR"] {
        let got = id_of(v);
        assert!(
            got.as_deref().is_none_or(|g| !g.starts_with("lp5907")),
            "{v}"
        );
    }
}

// ── LM2596 ───────────────────────────────────────────────────────────────

#[test]
fn lm2596_other_voltages_bind_their_own_cards() {
    for (value, id) in [
        ("LM2596S-3.3/NOPB", "lm2596_3v3"),
        ("LM2596T-3.3", "lm2596_3v3"),
        ("LM2596SX-3.3/NOPB", "lm2596_3v3"),
        ("LM2596S-12/NOPB", "lm2596_12"),
        ("LM2596T-12", "lm2596_12"),
        ("LM2596S-ADJ", "lm2596_adj"),
        ("LM2596T-ADJ/NOPB", "lm2596_adj"),
    ] {
        let m = card(value);
        assert_pin_map(&m, id, &pins("1=in 2=sw 3=gnd 4=fb 5=on_off"));
        assert!(
            has_supply_range(&m, "in", (4.5, 40.0), Some(45.0), "LM2596"),
            "{value}"
        );
    }
}

#[test]
fn lm2596_siblings_stay_apart() {
    // The 5.0 V card is unchanged and a bare name carries no voltage.
    assert_binds("LM2596S-5.0/NOPB", "lm2596_5v0");
    assert_not_in("LM2596", &["lm2596_3v3", "lm2596_12", "lm2596_adj"]);
    assert_not_in("LM2596S", &["lm2596_3v3", "lm2596_12", "lm2596_adj"]);
    // Other SIMPLE SWITCHER families and voltages.
    for v in [
        "LM2576-3.3",
        "LM2576-ADJ",
        "LM2595S-3.3",
        "LM2596S-15",
        "LM2596S-9",
    ] {
        assert_not_in(v, &["lm2596_3v3", "lm2596_12", "lm2596_adj", "lm2596_5v0"]);
    }
    assert_unresolved("LM2596S-1.2");
}

// ── MP1584 ───────────────────────────────────────────────────────────────

#[test]
fn mp1584_binds_with_its_suffixes_and_exposed_pad() {
    for v in [
        "MP1584",
        "MP1584EN",
        "MP1584EN-LF-Z",
        "MP1584EN-LF",
        "MP1584EN-Z",
    ] {
        let m = card(v);
        assert_pin_map(
            &m,
            "mp1584en",
            &pins("1=sw 2=en 3=comp 4=fb 5=gnd 6=rt 7=in 8=boot 9=gnd_ep"),
        );
    }
    let m = card("MP1584EN");
    assert!(has_supply_range(
        &m,
        "in",
        (4.5, 28.0),
        Some(30.0),
        "MP1584"
    ));
}

#[test]
fn mp1584_siblings_stay_apart() {
    for v in ["MP1583", "MP1582EN", "MP1584X", "MP2338", "MP1584ENx"] {
        assert_not_in(v, &["mp1584en"]);
    }
}

// ── MT3608, XL6009, IP5306 (identity only) ───────────────────────────────

fn assert_identity_only(m: &ModelEntry) {
    assert_eq!(m.params.get_bool("identity_only"), Some(true), "{}", m.id);
    assert!(
        m.params
            .get_str("warning")
            .is_some_and(|w| w.contains("OPEN")),
        "{}",
        m.id
    );
    assert!(m.params.get_str("unlocked_by").is_some(), "{}", m.id);
    assert!(
        m.params.get_f64("vout").is_none(),
        "{} must state no output voltage",
        m.id
    );
}

#[test]
fn mt3608_is_identity_only_with_the_sot23_6_pinout() {
    let m = card("MT3608");
    assert_pin_map(&m, "mt3608", &pins("1=sw 2=gnd 3=fb 4=en 5=in 6=nc"));
    assert_identity_only(&m);
    assert!(has_supply_range(
        &m,
        "in",
        (2.0, 24.0),
        Some(26.0),
        "MT3608"
    ));
    assert!(has_supply_range(&m, "sw", (-0.3, 30.0), None, "MT3608"));
    for v in ["MT3540", "MT3608B", "MT3608L", "MT3607", "MT36080"] {
        assert_not_in(v, &["mt3608"]);
    }
}

#[test]
fn xl6009_is_identity_only_with_the_to263_5_pinout() {
    for v in ["XL6009", "XL6009E1", "XL6009TRE1"] {
        let m = card(v);
        assert_pin_map(&m, "xl6009", &pins("1=gnd 2=en 3=sw 4=in 5=fb"));
        assert_identity_only(&m);
    }
    assert!(has_supply_range(
        &card("XL6009E1"),
        "in",
        (5.0, 32.0),
        Some(36.0),
        "XL6009"
    ));
    for v in ["XL6019", "XL6009E2", "XL6008E1", "XL4015", "XL6009x"] {
        assert_not_in(v, &["xl6009"]);
    }
}

#[test]
fn ip5306_is_identity_only_with_the_esop8_pinout() {
    let m = card("IP5306");
    assert_pin_map(
        &m,
        "ip5306",
        &pins("1=in 2=led1 3=led2 4=led3 5=key 6=bat 7=sw 8=out 9=gnd"),
    );
    assert_identity_only(&m);
    assert!(has_supply_range(&m, "in", (4.65, 5.5), Some(6.0), "IP5306"));
    for v in ["IP5303", "IP5305", "IP5308", "IP5306x", "IP5316", "IP5310"] {
        assert_not_in(v, &["ip5306"]);
    }
}

// ── Provenance ───────────────────────────────────────────────────────────

#[test]
fn every_card_in_the_batch_pins_a_datasheet_by_sha256() {
    for value in [
        "AMS1117-1.8",
        "AMS1117-ADJ",
        "AMS1117CS-3.3",
        "TLV70033DDCR",
        "TLV70033DSER",
        "TLV70233PDBVR",
        "LP5907MFX-3.3/NOPB",
        "LP5907SNX-3.3/NOPB",
        "LM2596S-3.3/NOPB",
        "LM2596S-12/NOPB",
        "LM2596S-ADJ",
        "MP1584EN",
        "MT3608",
        "XL6009",
        "IP5306",
    ] {
        let r = resolve_query(Some(value), Some(value), None);
        assert!(!r.references.is_empty(), "{value}: no reference");
        for rf in &r.references {
            let sha = rf.sha256.as_deref().unwrap_or("");
            assert_eq!(sha.len(), 64, "{value}: reference sha256 {sha:?}");
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "{value}");
            assert!(!rf.locator.is_empty(), "{value}");
        }
    }
}
