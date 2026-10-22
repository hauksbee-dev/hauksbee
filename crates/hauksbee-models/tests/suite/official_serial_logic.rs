//! Official-parts batch serial_logic: resolution and behaviour tests for the cards added in that batch.
//!
//! Section 1 covers the 74-series logic cards in db/logic.toml (74LVC1G02/07/32, 74HC138/139/240/244/373/573).
//! Every card is resolved by its ordering code, its full pin map is pinned against the datasheet, key params
//! and the supply envelope are asserted, and sibling part numbers are asserted NOT to bind to it.

use super::{
    assert_pin_map, expect_model, has_supply_range, param, pins, query_id, resolve_part,
    resolve_value,
};
use hauksbee_models::ModelEntry;

/// Resolve `value` by ordering code and assert the complete pad->role map of card `id`.
fn check_map(value: &str, id: &str, spec: &str) -> ModelEntry {
    let m = expect_model(value);
    assert_pin_map(&m, id, &pins(spec));
    // The same code must bind the same card when it is the only metadata a layout-only board carries.
    let again = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve by mpn"));
    assert_eq!(again.id, id, "{value}: value and mpn resolution disagree");
    m
}

/// None of `values` may bind to card `id` (they may bind another card, or none).
fn assert_not_card(id: &str, values: &[&str]) {
    for v in values {
        assert_ne!(
            query_id(Some(v), None, None).as_deref(),
            Some(id),
            "{v} is a different part and must not resolve to {id}"
        );
    }
}

fn assert_param(m: &ModelEntry, k: &str, want: f64) {
    let got = param(m, k);
    assert!(
        (got - want).abs() <= 1e-9 * want.abs().max(1.0),
        "{}: {k} = {got}, datasheet value {want}",
        m.id
    );
}

// ---------------------------------------------------------------------------------------------
// 74LVC1G02 / 74LVC1G07 / 74LVC1G32
// ---------------------------------------------------------------------------------------------

const LVC_SUPPLY: (f64, f64) = (1.65, 5.5);

#[test]
fn lvc1g02_pin_maps_per_package() {
    for code in [
        "74LVC1G02",
        "SN74LVC1G02DBVR",
        "SN74LVC1G02DCKR",
        "SN74LVC1G02DRLR",
        "SN74LVC1G02YZPR",
    ] {
        check_map(code, "74lvc1g02", "1=a 2=b 3=gnd 4=y 5=vcc");
    }
    for code in ["SN74LVC1G02DRYR", "SN74LVC1G02DSFR"] {
        check_map(code, "74lvc1g02_dry_dsf", "1=a 2=b 3=gnd 4=y 5=nc 6=vcc");
    }
    // DPW numbers the inputs the other way round: B on pad 1, A on pad 2.
    check_map(
        "SN74LVC1G02DPWR",
        "74lvc1g02_dpw",
        "1=b 2=a 3=gnd 4=y 5=vcc",
    );
}

#[test]
fn lvc1g02_params_and_envelope() {
    let m = expect_model("SN74LVC1G02DBVR");
    assert_param(&m, "tpd_s", 4.5e-9);
    assert_param(&m, "supply_cpd_pf", 23.0);
    assert_param(&m, "vil", 0.8);
    assert_param(&m, "vih", 2.0);
    assert_param(&m, "supply_static_ua", 10.0);
    assert!(has_supply_range(
        &m,
        "vcc",
        LVC_SUPPLY,
        Some(6.5),
        "SCES213Y"
    ));
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(371.0));
}

#[test]
fn lvc1g32_pin_maps_per_package() {
    for code in [
        "74LVC1G32",
        "SN74LVC1G32DBVR",
        "SN74LVC1G32DCKR",
        "SN74LVC1G32DRLR",
        "SN74LVC1G32DPWR",
        "SN74LVC1G32YZPR",
    ] {
        check_map(code, "74lvc1g32", "1=a 2=b 3=gnd 4=y 5=vcc");
    }
    for code in ["SN74LVC1G32DRYR", "SN74LVC1G32DSFR"] {
        check_map(code, "74lvc1g32_dry_dsf", "1=a 2=b 3=gnd 4=y 5=nc 6=vcc");
    }
}

#[test]
fn lvc1g32_params_and_envelope() {
    let m = expect_model("SN74LVC1G32DBVR");
    assert_param(&m, "tpd_s", 4.5e-9);
    assert_param(&m, "supply_cpd_pf", 21.0);
    assert!(has_supply_range(
        &m,
        "vcc",
        LVC_SUPPLY,
        Some(6.5),
        "SCES219W"
    ));
}

#[test]
fn lvc1g07_open_drain_pin_maps_per_package() {
    for code in [
        "74LVC1G07",
        "SN74LVC1G07DBVR",
        "SN74LVC1G07DCKR",
        "SN74LVC1G07DRLR",
        "SN74LVC1G07DPWR",
    ] {
        check_map(code, "74lvc1g07", "1=nc 2=a 3=gnd 4=y 5=vcc");
    }
    for code in ["SN74LVC1G07DRYR", "SN74LVC1G07DSFR"] {
        check_map(code, "74lvc1g07_dry_dsf", "1=nc 2=a 3=gnd 4=y 5=nc 6=vcc");
    }
}

#[test]
fn lvc1g07_params_and_envelope() {
    let m = expect_model("SN74LVC1G07DBVR");
    // Open drain: no high-level output voltage is carried, only the sink side.
    assert!(
        m.params.get_f64("voh").is_none(),
        "an open-drain output has no VOH"
    );
    assert_param(&m, "vol", 0.1);
    assert_param(&m, "tpd_s", 4.2e-9);
    assert_param(&m, "supply_cpd_pf", 4.0);
    assert!(has_supply_range(
        &m,
        "vcc",
        LVC_SUPPLY,
        Some(6.5),
        "SCES296AG"
    ));
}

#[test]
fn lvc1g_siblings_do_not_cross_resolve() {
    assert_not_card(
        "74lvc1g02",
        &[
            "74LVC1G04",
            "74LVC1G08",
            "74LVC1G32",
            "74LVC1G07",
            "SN74LVC1G04DBVR",
            "74AHC1G02",
            "74LVC2G02",
            "74HC02",
            "SN74LVC1G02DRYR",
        ],
    );
    assert_not_card(
        "74lvc1g02_dry_dsf",
        &["SN74LVC1G02DBVR", "SN74LVC1G02DPWR", "SN74LVC1G32DRYR"],
    );
    assert_not_card(
        "74lvc1g02_dpw",
        &["SN74LVC1G02DBVR", "SN74LVC1G02DRYR", "SN74LVC1G32DPWR"],
    );
    assert_not_card(
        "74lvc1g32",
        &[
            "74LVC1G02",
            "74LVC1G08",
            "74AHC1G32",
            "74LVC2G32",
            "74HC32",
            "SN74LVC1G32DRYR",
            "SN74LVC1G32DSFR",
        ],
    );
    assert_not_card("74lvc1g32_dry_dsf", &["SN74LVC1G32DBVR", "SN74LVC1G07DRYR"]);
    assert_not_card(
        "74lvc1g07",
        &[
            "74LVC1G04",
            "74LVC1G17",
            "74LVC1G06",
            "74AHC1G07",
            "74LVC2G07",
            "SN74LVC1G07DRYR",
        ],
    );
    assert_not_card("74lvc1g07_dry_dsf", &["SN74LVC1G07DBVR", "SN74LVC1G02DRYR"]);
    // The existing single-gate cards still bind their own parts.
    for (code, id) in [
        ("74LVC1G04", "74lvc1g04"),
        ("74LVC1G08", "74lvc1g08"),
        ("74LVC1G14", "74lvc1g14"),
    ] {
        assert_eq!(
            query_id(Some(code), None, None).as_deref(),
            Some(id),
            "{code}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// 74HC138 / 74HC139
// ---------------------------------------------------------------------------------------------

const HC_SUPPLY: (f64, f64) = (2.0, 6.0);

#[test]
fn hc138_pin_map_and_params() {
    let spec = "1=a 2=b 3=c 4=g2a_n 5=g2b_n 6=g1 7=y7_n 8=gnd 9=y6_n 10=y5_n 11=y4_n 12=y3_n 13=y2_n 14=y1_n 15=y0_n 16=vcc";
    for code in [
        "74HC138",
        "SN74HC138D",
        "SN74HC138DR",
        "SN74HC138N",
        "SN74HC138NSR",
        "SN74HC138PWR",
        "SN74HC138DBR",
    ] {
        check_map(code, "74hc138", spec);
    }
    let m = expect_model("SN74HC138PWR");
    assert_param(&m, "vih", 3.15);
    assert_param(&m, "vil", 1.35);
    assert_param(&m, "supply_cpd_pf", 85.0);
    assert_param(&m, "tpd_s", 45e-9);
    assert_param(&m, "supply_static_ua", 80.0);
    assert!(has_supply_range(
        &m,
        "vcc",
        HC_SUPPLY,
        Some(7.0),
        "SCLS107G"
    ));
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(141.6));
}

#[test]
fn hc139_pin_map_and_params() {
    let spec = "1=1g_n 2=1a 3=1b 4=1y0_n 5=1y1_n 6=1y2_n 7=1y3_n 8=gnd 9=2y3_n 10=2y2_n 11=2y1_n 12=2y0_n 13=2b 14=2a 15=2g_n 16=vcc";
    for code in [
        "74HC139",
        "SN74HC139D",
        "SN74HC139DR",
        "SN74HC139N",
        "SN74HC139NSR",
        "SN74HC139PWR",
    ] {
        check_map(code, "74hc139", spec);
    }
    let m = expect_model("SN74HC139PWR");
    assert_param(&m, "supply_cpd_pf", 25.0);
    assert_param(&m, "tpd_s", 44e-9);
    assert!(has_supply_range(
        &m,
        "vcc",
        HC_SUPPLY,
        Some(7.0),
        "SCLS108E"
    ));
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(108.0));
}

#[test]
fn hc138_139_siblings_do_not_cross_resolve() {
    assert_not_card(
        "74hc138",
        &[
            "74HCT138", "74LS138", "74AHC138", "74HC139", "74HC238", "74HC1380",
        ],
    );
    assert_not_card(
        "74hc139",
        &["74HCT139", "74LS139", "74AHC139", "74HC138", "74HC239"],
    );
}

// ---------------------------------------------------------------------------------------------
// 74HC244 / 74HC240 / 74HC373 / 74HC573
// ---------------------------------------------------------------------------------------------

const BUFFER_20: &str = "1=oe_n_1 2=1a1 3=2y4 4=1a2 5=2y3 6=1a3 7=2y2 8=1a4 9=2y1 10=gnd 11=2a1 12=1y4 13=2a2 14=1y3 15=2a3 16=1y2 17=2a4 18=1y1 19=oe_n_2 20=vcc";

#[test]
fn hc244_pin_map_and_params() {
    for code in [
        "74HC244",
        "SN74HC244DWR",
        "SN74HC244DBR",
        "SN74HC244N",
        "SN74HC244NSR",
        "SN74HC244PWR",
    ] {
        check_map(code, "74hc244", BUFFER_20);
    }
    let m = expect_model("SN74HC244DWR");
    assert_param(&m, "supply_cpd_pf", 35.0);
    assert!(has_supply_range(
        &m,
        "vcc",
        HC_SUPPLY,
        Some(7.0),
        "SCLS130I"
    ));
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(131.8));
    assert_eq!(m.ratings.max_pin_current_a, Some(0.035));
}

#[test]
fn hc240_pin_map_and_params() {
    // Same pad numbering as the '244, but the outputs invert.
    for code in [
        "74HC240",
        "SN74HC240DWR",
        "SN74HC240DBR",
        "SN74HC240DGSR",
        "SN74HC240N",
        "SN74HC240PWR",
    ] {
        check_map(code, "74hc240", BUFFER_20);
    }
    let m = expect_model("SN74HC240DWR");
    assert_param(&m, "tpd_s", 25e-9);
    assert!(has_supply_range(
        &m,
        "vcc",
        HC_SUPPLY,
        Some(7.0),
        "SCLS128H"
    ));
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(131.8));
}

#[test]
fn hc373_pin_map_and_params() {
    let spec = "1=oe_n 2=1q 3=1d 4=2d 5=2q 6=3q 7=3d 8=4d 9=4q 10=gnd 11=le 12=5q 13=5d 14=6d 15=6q 16=7q 17=7d 18=8d 19=8q 20=vcc";
    for code in [
        "74HC373",
        "SN74HC373DWR",
        "SN74HC373DBR",
        "SN74HC373N",
        "SN74HC373NSR",
        "SN74HC373PWR",
    ] {
        check_map(code, "74hc373", spec);
    }
    let m = expect_model("SN74HC373DWR");
    assert_param(&m, "supply_cpd_pf", 100.0);
    assert_param(&m, "tpd_s", 45e-9);
    assert!(has_supply_range(
        &m,
        "vcc",
        HC_SUPPLY,
        Some(7.0),
        "SCLS140G"
    ));
}

#[test]
fn hc573_pin_map_and_params() {
    // The '573 puts all D inputs on one side and all Q outputs on the other.
    let spec = "1=oe_n 2=1d 3=2d 4=3d 5=4d 6=5d 7=6d 8=7d 9=8d 10=gnd 11=le 12=8q 13=7q 14=6q 15=5q 16=4q 17=3q 18=2q 19=1q 20=vcc";
    for code in [
        "74HC573",
        "74HC573A",
        "SN74HC573ADWR",
        "SN74HC573ADBR",
        "SN74HC573AN",
        "SN74HC573APWR",
    ] {
        check_map(code, "74hc573", spec);
    }
    let m = expect_model("SN74HC573ADWR");
    assert_param(&m, "supply_cpd_pf", 50.0);
    assert_param(&m, "tpd_s", 44e-9);
    assert!(has_supply_range(
        &m,
        "vcc",
        HC_SUPPLY,
        Some(7.0),
        "SCLS147G"
    ));
}

#[test]
fn hc_octal_siblings_do_not_cross_resolve() {
    assert_not_card(
        "74hc244",
        &[
            "74HCT244", "74LS244", "74AHC244", "74LVC244", "74HC240", "74HC245", "74HC2440",
        ],
    );
    assert_not_card(
        "74hc240",
        &["74HCT240", "74LS240", "74AHC240", "74HC244", "74HC241"],
    );
    assert_not_card(
        "74hc373",
        &[
            "74HCT373", "74LS373", "74AHC373", "74LVC373", "74HC573", "74HC374", "74HC3730",
        ],
    );
    assert_not_card(
        "74hc573",
        &[
            "74HCT573", "74LS573", "74AHC573", "74LVC573", "74HC373", "74HC574",
        ],
    );
}

#[test]
fn every_batch_logic_card_resolves_to_itself_not_a_neighbour() {
    // One code per card: a regex widened by mistake would make two of these bind the same id.
    for (code, id) in [
        ("SN74HC138PWR", "74hc138"),
        ("SN74HC139PWR", "74hc139"),
        ("SN74HC244PWR", "74hc244"),
        ("SN74HC240PWR", "74hc240"),
        ("SN74HC373PWR", "74hc373"),
        ("SN74HC573APWR", "74hc573"),
        ("SN74LVC1G02DBVR", "74lvc1g02"),
        ("SN74LVC1G07DBVR", "74lvc1g07"),
        ("SN74LVC1G32DBVR", "74lvc1g32"),
    ] {
        assert_eq!(
            resolve_value(code).model.map(|m| m.id).as_deref(),
            Some(id),
            "{code}"
        );
    }
}
