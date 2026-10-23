//! Official-parts batch `serial_logic`: resolution and behaviour tests for the cards added in that batch.
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

// ---------------------------------------------------------------------------------------------
// Section 2: serial interface cards in db/interface_serial.toml
// ---------------------------------------------------------------------------------------------

// --- CH340T / CH340K / CH340E (WCH CH340 datasheet V3D) ---------------------------------------

#[test]
fn ch340_variants_pin_maps_and_params() {
    let t = check_map(
        "CH340T",
        "ch340t",
        "1=cko 2=act_n 3=txd 4=rxd 5=v3 6=ud_p 7=ud_n 8=gnd 9=xi 10=xo 11=cts_n 12=dsr_n 13=ri_n \
         14=dcd_n 15=dtr_n 16=rts_n 17=tnow 18=r232 19=vcc 20=nos_n",
    );
    assert_param(&t, "vih", 1.9);
    assert_param(&t, "vil", 0.8);
    assert_param(&t, "vol", 0.5);
    assert!(has_supply_range(&t, "vcc", (2.9, 5.3), Some(6.0), "V3D"));

    // The ESSOP-10 K and the MSOP-10 E swap DTR#/RTS# and differ in the exposed pad.
    let k = check_map(
        "CH340K",
        "ch340k",
        "0=ep 1=ud_p 2=ud_n 3=gnd 4=dtr_n 5=cts_n 6=rts_n 7=vcc 8=txd 9=rxd 10=v3",
    );
    assert!(has_supply_range(&k, "vcc", (3.1, 5.3), Some(6.0), "V3D"));
    let e = check_map(
        "CH340E",
        "ch340e",
        "1=ud_p 2=ud_n 3=gnd 4=rts_n 5=cts_n 6=tnow 7=vcc 8=txd 9=rxd 10=v3",
    );
    assert!(has_supply_range(&e, "vcc", (3.1, 5.3), Some(6.0), "V3D"));
    for code in ["CH340T-ND", "ch340t", "CH340K", "CH340E"] {
        assert!(resolve_value(code).model.is_some(), "{code} must resolve");
    }
}

#[test]
fn ch340_siblings_do_not_cross_resolve() {
    assert_not_card(
        "ch340t",
        &[
            "CH340G", "CH340C", "CH340X", "CH340N", "CH340K", "CH340E", "CH340B", "CH340R",
            "CH341A", "CH330N", "CH9102F", "CP2102",
        ],
    );
    assert_not_card(
        "ch340k",
        &[
            "CH340G", "CH340C", "CH340X", "CH340N", "CH340T", "CH340E", "CH341A",
        ],
    );
    assert_not_card(
        "ch340e",
        &[
            "CH340G", "CH340C", "CH340X", "CH340N", "CH340T", "CH340K", "CH341A",
        ],
    );
    // The pre-existing cards keep their own ids.
    for (code, id) in [("CH340G", "ch340g"), ("CH340C", "ch340c")] {
        assert_eq!(query_id(Some(code), None, None).as_deref(), Some(id));
    }
}

// --- CH9102F / CH9102X (WCH CH9102 datasheet V1C) ---------------------------------------------

#[test]
fn ch9102_pin_maps_and_envelope() {
    let f = check_map(
        "CH9102F",
        "ch9102f",
        "0=ep 1=ri_n 2=gnd 3=ud_p 4=ud_n 5=vio 6=v3 7=vdd5 8=vbus 9=rst_n 10=act_n 11=wakeup_gpio3 \
         12=tnow_gpio2 13=rxs_gpio1 14=txs_gpio0 15=suspend_n 16=gpio4 17=suspend 18=cts_n 19=rts_n \
         20=rxd 21=txd 22=dsr_n 23=dtr_n 24=dcd_n",
    );
    assert!(has_supply_range(&f, "vdd5", (3.0, 5.5), Some(6.0), "V1C"));
    assert!(has_supply_range(&f, "vio", (1.7, 5.5), Some(6.0), "V1C"));
    assert_eq!(f.params.get_str("supply_pin"), Some("5"));
    let x = check_map(
        "CH9102X",
        "ch9102x",
        "0=ep 1=dcd_n 2=ri_n 3=gnd 4=ud_p 5=ud_n 6=v3 7=vdd5 8=vbus 9=rst_n 10=act_n 11=suspend_n \
         12=suspend 13=nc 14=nc 15=nc 16=wakeup_gpio3 17=tnow_gpio2 18=rxs_gpio1 19=txs_gpio0 \
         20=gpio6 21=gpio5 22=v3 23=cts_n 24=rts_n 25=rxd 26=txd 27=dsr_n 28=dtr_n",
    );
    assert!(has_supply_range(&x, "vdd5", (3.0, 5.5), Some(6.0), "V1C"));
}

#[test]
fn ch9102_f_and_x_do_not_cross_resolve() {
    assert_not_card(
        "ch9102f",
        &["CH9102X", "CH9102", "CH9101F", "CH9103", "CH340T"],
    );
    assert_not_card("ch9102x", &["CH9102F", "CH9102", "CH9101", "CH340X"]);
}

// --- MAX485 / MAX3485 (Maxim/ADI) ---------------------------------------------------------------

#[test]
fn max485_pin_maps_and_params() {
    for code in [
        "MAX485",
        "MAX485CSA",
        "MAX485ESA+",
        "MAX485ESA+T",
        "MAX485CPA",
        "MAX485EPA",
        "MAX485MJA",
    ] {
        let m = check_map(code, "max485", "1=ro 2=re_n 3=de 4=di 5=gnd 6=a 7=b 8=vcc");
        assert_param(&m, "vih", 2.0);
        assert_param(&m, "vil", 0.8);
        assert_param(&m, "voh", 3.5);
        assert_param(&m, "vol", 0.4);
        assert!(has_supply_range(
            &m,
            "vcc",
            (4.75, 5.25),
            Some(12.0),
            "Rev 10"
        ));
    }
    // The uMAX has a different pin order.
    let u = check_map(
        "MAX485CUA",
        "max485_umax",
        "1=b 2=vcc 3=ro 4=re_n 5=de 6=di 7=gnd 8=a",
    );
    assert!(has_supply_range(
        &u,
        "vcc",
        (4.75, 5.25),
        Some(12.0),
        "Rev 10"
    ));
}

#[test]
fn max3485_pin_map_and_params() {
    for code in [
        "MAX3485",
        "MAX3485CSA",
        "MAX3485ESA+",
        "MAX3485EPA",
        "MAX3485CPA+T",
    ] {
        let m = check_map(code, "max3485", "1=ro 2=re_n 3=de 4=di 5=gnd 6=a 7=b 8=vcc");
        assert_param(&m, "vih", 2.0);
        assert_param(&m, "vil", 0.8);
        assert_param(&m, "vol", 0.4);
        assert!(has_supply_range(&m, "vcc", (3.0, 3.6), Some(7.0), "Rev 1"));
    }
}

#[test]
fn max485_family_siblings_do_not_cross_resolve() {
    assert_not_card(
        "max485",
        &[
            "MAX3485",
            "MAX3485CSA",
            "SP3485",
            "SP3485EN",
            "MAX481CSA",
            "MAX483CSA",
            "MAX487CSA",
            "MAX488CSA",
            "MAX490CSA",
            "MAX1487CSA",
            "MAX485E",
            "MAX485ECSA",
            "MAX485EESA",
            "MAX485CUA",
            "MAX4850",
            "MAX3232",
        ],
    );
    assert_not_card(
        "max485_umax",
        &[
            "MAX485",
            "MAX485CSA",
            "MAX481CUA",
            "MAX483CUA",
            "MAX3485",
            "SP3485",
        ],
    );
    assert_not_card(
        "max3485",
        &[
            "MAX485",
            "MAX485CSA",
            "SP3485",
            "MAX3483CSA",
            "MAX3486CSA",
            "MAX3488CSA",
            "MAX3490CSA",
            "MAX3491CSD",
            "MAX3485E",
            "MAX3485ECSA",
            "MAX3485EESA",
            "MAX3485CUA",
        ],
    );
}

// ---------------------------------------------------------------------------------------------
// USB bridges: FT232RL / FT232RQ (FT_000053 v2.16) and MCP2221A (DS20005565B), db/interface_serial.toml
// ---------------------------------------------------------------------------------------------

/// Resolve `value` by ordering code, assert the complete pad->role map of card `id`, and check that
/// the code binds the same card when it is the only metadata a layout-only board carries.
fn usb_check_map(value: &str, id: &str, spec: &str) -> ModelEntry {
    let m = expect_model(value);
    assert_pin_map(&m, id, &pins(spec));
    let again = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve by mpn"));
    assert_eq!(again.id, id, "{value}: value and mpn resolution disagree");
    m
}

/// None of `values` may bind to card `id` (they may bind another card, or none).
fn usb_assert_not_card(id: &str, values: &[&str]) {
    for v in values {
        assert_ne!(
            query_id(Some(v), None, None).as_deref(),
            Some(id),
            "{v} is a different part or package and must not resolve to {id}"
        );
        assert_ne!(
            resolve_part(v).map(|m| m.id).as_deref(),
            Some(id),
            "{v} (by mpn) is a different part or package and must not resolve to {id}"
        );
    }
}

fn usb_assert_param(m: &ModelEntry, k: &str, want: f64) {
    let got = param(m, k);
    assert!(
        (got - want).abs() <= 1e-9 * want.abs().max(1.0),
        "{}: {k} = {got}, datasheet value {want}",
        m.id
    );
}

fn usb_assert_levels(m: &ModelEntry, k: &str, want: &[[f64; 2]]) {
    let got = m
        .levels
        .get(k)
        .unwrap_or_else(|| panic!("{}: no levels table for {k}", m.id));
    assert_eq!(got.len(), want.len(), "{}: levels {k} point count", m.id);
    for (g, w) in got.iter().zip(want) {
        assert!(
            (g[0] - w[0]).abs() < 1e-9 && (g[1] - w[1]).abs() < 1e-9,
            "{}: levels {k} point {g:?}, expected {w:?}",
            m.id
        );
    }
}

const FT232RL_MAP: &str =
    "1=txd 2=dtr_n 3=rts_n 4=vccio 5=rxd 6=ri_n 7=gnd 8=nc 9=dsr_n 10=dcd_n 11=cts_n \
     12=cbus4 13=cbus2 14=cbus3 15=usbdp 16=usbdm 17=v3v3out 18=gnd2 19=reset_n 20=vcc 21=gnd3 \
     22=cbus1 23=cbus0 24=nc2 25=agnd 26=test 27=osci 28=osco";

const FT232RQ_MAP: &str = "1=vccio 2=rxd 3=ri_n 4=gnd 5=nc 6=dsr_n 7=dcd_n 8=cts_n 9=cbus4 10=cbus2 11=cbus3 \
     12=nc2 13=nc3 14=usbdp 15=usbdm 16=v3v3out 17=gnd2 18=reset_n 19=vcc 20=gnd3 21=cbus1 22=cbus0 \
     23=nc4 24=agnd 25=nc5 26=test 27=osci 28=osco 29=nc6 30=txd 31=dtr_n 32=rts_n 33=ep EP=ep";

const MCP2221A_14_MAP: &str =
    "1=vdd 2=gp0 3=gp1 4=rst_n 5=rxd 6=txd 7=gp2 8=gp3 9=sda 10=scl 11=vusb 12=usbdm 13=usbdp 14=gnd";

const MCP2221A_QFN_MAP: &str =
    "1=gp0 2=gp1 3=rst_n 4=rxd 5=txd 6=gp2 7=gp3 8=sda 9=scl 10=vusb 11=usbdm \
     12=usbdp 13=gnd 14=nc 15=nc2 16=vdd 17=ep EP=ep";

// --- FT232RL ----------------------------------------------------------------------------------

#[test]
fn usb_ft232rl_codes_resolve_with_the_ssop28_map() {
    for code in [
        "FT232RL",
        "ft232rl",
        "FT232RL-REEL",
        "FT232RL-TUBE",
        "FT232RL_REEL",
    ] {
        usb_check_map(code, "ft232rl", FT232RL_MAP);
    }
}

#[test]
fn usb_ft232rl_params_ratings_and_envelope() {
    let m = expect_model("FT232RL");
    // Table 5.4 (p.16), VCCIO = 3.3 V standard drive: Voh min 2.2 V, Vol max 0.5 V.
    usb_assert_param(&m, "voh", 2.2);
    usb_assert_param(&m, "vol", 0.5);
    // Vin switching threshold 1.0 V to 1.5 V (Tables 5.3 to 5.10).
    usb_assert_param(&m, "vih", 1.5);
    usb_assert_param(&m, "vil", 1.0);
    // Table 5.2 Icc1 typical, 15 mA.
    usb_assert_param(&m, "supply_static_ua", 15000.0);
    assert_eq!(m.params.get_str("supply_pin"), Some("20"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("7"));
    // TEST has no internal pull and must be tied to GND.
    assert_eq!(m.params.get_str("must_not_float_roles"), Some("test"));
    assert!(has_supply_range(
        &m,
        "vcc",
        (4.0, 5.25),
        Some(6.0),
        "Absolute Maximum Ratings"
    ));
    assert!(has_supply_range(&m, "vccio", (1.8, 5.25), None, "VCC2"));
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(55.82));
    assert_eq!(m.ratings.theta_jc_c_per_w, Some(24.04));
    assert_eq!(m.ratings.max_power_w, Some(0.5));
    // Internal 200 kOhm pull-ups to VCCIO on the input pins, and the 3V3OUT LDO.
    for role in ["rxd", "cts_n", "dsr_n", "dcd_n", "ri_n"] {
        let pin = m
            .behavioral
            .pins
            .get(role)
            .unwrap_or_else(|| panic!("ft232rl: no behavioral pin {role}"));
        assert_eq!(pin.pull_to.as_deref(), Some("vccio"), "{role}");
        assert_eq!(pin.pull_ohms, Some(200_000.0), "{role}");
    }
    assert_eq!(m.behavioral.laws.len(), 1);
    assert_eq!(m.behavioral.laws[0].name, "ldo_3v3out");
    assert_eq!(m.behavioral.laws[0].a, "v3v3out");
}

#[test]
fn usb_ft232rl_siblings_do_not_resolve_to_it() {
    usb_assert_not_card(
        "ft232rl",
        &[
            "FT232R",
            "FT232RQ",
            "FT232RQ-REEL",
            "FT232RLX",
            "FT232RL-TRAY",
            "FT232H",
            "FT232HL",
            "FT231X",
            "FT231XQ",
            "FT231XS",
            "FT2232H",
            "FT232BM",
            "FT232BL",
            "CH340C",
            "CP2102",
            "MCP2221A-I/SL",
        ],
    );
}

// --- FT232RQ ----------------------------------------------------------------------------------

#[test]
fn usb_ft232rq_codes_resolve_with_the_qfn32_map() {
    for code in ["FT232RQ", "ft232rq", "FT232RQ-REEL", "FT232RQ-TRAY"] {
        usb_check_map(code, "ft232rq", FT232RQ_MAP);
    }
}

#[test]
fn usb_ft232rq_params_ratings_and_envelope() {
    let m = expect_model("FT232RQ");
    usb_assert_param(&m, "voh", 2.2);
    usb_assert_param(&m, "vol", 0.5);
    usb_assert_param(&m, "vih", 1.5);
    usb_assert_param(&m, "vil", 1.0);
    usb_assert_param(&m, "supply_static_ua", 15000.0);
    assert_eq!(m.params.get_str("supply_pin"), Some("19"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("4"));
    assert_eq!(m.params.get_str("must_not_float_roles"), Some("test"));
    assert!(has_supply_range(
        &m,
        "vcc",
        (4.0, 5.25),
        Some(6.0),
        "Absolute Maximum Ratings"
    ));
    assert!(has_supply_range(&m, "vccio", (1.8, 5.25), None, "VCC2"));
    // Table 5.17: the less favourable row (centre pad unsoldered) is the scalar; theta-JC is blank.
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(62.31));
    assert_eq!(m.ratings.theta_jc_c_per_w, None);
    let pin = m.behavioral.pins.get("rxd").expect("ft232rq rxd pull-up");
    assert_eq!(pin.pull_to.as_deref(), Some("vccio"));
    assert_eq!(pin.pull_ohms, Some(200_000.0));
    assert_eq!(m.behavioral.laws.len(), 1);
}

#[test]
fn usb_ft232rq_siblings_do_not_resolve_to_it() {
    usb_assert_not_card(
        "ft232rq",
        &[
            "FT232R",
            "FT232RL",
            "FT232RL-REEL",
            "FT232RQX",
            "FT232RQ-TUBE",
            "FT232H",
            "FT231XQ",
            "FT231X-Q",
            "FT231XS",
            "FT2232H",
            "CH340C",
            "MCP2221A-I/ML",
        ],
    );
}

#[test]
fn usb_ft232r_packages_have_different_pin_orders() {
    // The SSOP-28 and QFN-32 number the same signals differently; borrowing one map for the other
    // would put VCC on the wrong pad.
    let rl = expect_model("FT232RL");
    let rq = expect_model("FT232RQ");
    assert_ne!(rl.id, rq.id);
    assert_eq!(rl.pins.get("20").map(String::as_str), Some("vcc"));
    assert_eq!(rq.pins.get("19").map(String::as_str), Some("vcc"));
    assert_eq!(rl.pins.get("1").map(String::as_str), Some("txd"));
    assert_eq!(rq.pins.get("1").map(String::as_str), Some("vccio"));
}

// --- MCP2221A, 14-pin PDIP / SOIC / TSSOP -------------------------------------------------------

#[test]
fn usb_mcp2221a_14pin_codes_resolve_with_the_shared_map() {
    for code in [
        "MCP2221A-I/P",
        "MCP2221A-I/SL",
        "MCP2221A-I/ST",
        "MCP2221AT-I/SL",
        "MCP2221AT-I/ST",
        "mcp2221a-i/sl",
    ] {
        usb_check_map(code, "mcp2221a", MCP2221A_14_MAP);
    }
}

#[test]
fn usb_mcp2221a_14pin_params_ratings_and_envelope() {
    let m = expect_model("MCP2221A-I/SL");
    // Derived at VDD = 3.3 V from D041 (0.8 VDD), D031 (0.2 VDD) and D090 (VDD - 0.7 V); VOL is D080.
    usb_assert_param(&m, "vih", 2.64);
    usb_assert_param(&m, "vil", 0.66);
    usb_assert_param(&m, "voh", 2.6);
    usb_assert_param(&m, "vol", 0.6);
    usb_assert_param(&m, "supply_static_ua", 13000.0);
    assert_eq!(m.params.get_str("supply_pin"), Some("1"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("14"));
    // The internal RST pull-up has no printed value, so the card does not opt into the float rule.
    assert_eq!(m.params.get_str("must_not_float_roles"), None);
    assert!(m.behavioral.pins.is_empty());
    usb_assert_levels(&m, "vih", &[[3.0, 2.4], [5.5, 4.4]]);
    usb_assert_levels(&m, "vil", &[[3.0, 0.6], [5.5, 1.1]]);
    usb_assert_levels(&m, "voh", &[[3.0, 2.3], [5.5, 4.8]]);
    assert!(has_supply_range(
        &m,
        "vdd",
        (3.0, 5.5),
        Some(6.0),
        "Absolute Maximum Ratings"
    ));
    assert!(has_supply_range(
        &m,
        "vusb",
        (3.0, 3.6),
        Some(4.0),
        "Absolute Maximum Ratings"
    ));
    assert_eq!(m.ratings.max_junction_temp_c, Some(150.0));
    assert_eq!(m.ratings.max_pin_current_a, Some(0.025));
    assert_eq!(m.ratings.max_current_a, Some(0.095));
    // Table 4-4: PDIP 70, SOIC 95.3, TSSOP 100; the scalar is the least favourable.
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(100.0));
}

#[test]
fn usb_mcp2221a_14pin_siblings_do_not_resolve_to_it() {
    usb_assert_not_card(
        "mcp2221a",
        &[
            "MCP2221A",
            "MCP2221",
            "MCP2221-I/SL",
            "MCP2221-I/P",
            "MCP2221-I/ST",
            "MCP2221A-E/SL",
            "MCP2221A-I/ML",
            "MCP2221AT-I/ML",
            "MCP2221AX-I/SL",
            "FT232RL",
            "FT232RQ",
            "FT231X",
            "CH340C",
            "CP2102",
        ],
    );
}

// --- MCP2221A, 16-pin QFN -----------------------------------------------------------------------

#[test]
fn usb_mcp2221a_qfn16_codes_resolve_with_the_qfn_map() {
    for code in ["MCP2221A-I/ML", "MCP2221AT-I/ML", "mcp2221a-i/ml"] {
        usb_check_map(code, "mcp2221a_qfn16", MCP2221A_QFN_MAP);
    }
}

#[test]
fn usb_mcp2221a_qfn16_params_ratings_and_envelope() {
    let m = expect_model("MCP2221A-I/ML");
    usb_assert_param(&m, "vih", 2.64);
    usb_assert_param(&m, "vil", 0.66);
    usb_assert_param(&m, "voh", 2.6);
    usb_assert_param(&m, "vol", 0.6);
    usb_assert_param(&m, "supply_static_ua", 13000.0);
    assert_eq!(m.params.get_str("supply_pin"), Some("16"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("13"));
    usb_assert_levels(&m, "vih", &[[3.0, 2.4], [5.5, 4.4]]);
    usb_assert_levels(&m, "vil", &[[3.0, 0.6], [5.5, 1.1]]);
    usb_assert_levels(&m, "voh", &[[3.0, 2.3], [5.5, 4.8]]);
    assert!(has_supply_range(
        &m,
        "vdd",
        (3.0, 5.5),
        Some(6.0),
        "Absolute Maximum Ratings"
    ));
    assert!(has_supply_range(
        &m,
        "vusb",
        (3.0, 3.6),
        Some(4.0),
        "Absolute Maximum Ratings"
    ));
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(45.7));
}

#[test]
fn usb_mcp2221a_qfn16_siblings_do_not_resolve_to_it() {
    usb_assert_not_card(
        "mcp2221a_qfn16",
        &[
            "MCP2221A",
            "MCP2221",
            "MCP2221-I/ML",
            "MCP2221A-E/ML",
            "MCP2221A-I/SL",
            "MCP2221A-I/ST",
            "MCP2221A-I/P",
            "MCP2221AT-I/SL",
            "FT232RQ",
            "CH340C",
        ],
    );
}

#[test]
fn usb_mcp2221a_packages_have_different_pin_orders() {
    // The 14-pin packages put VDD on pad 1 and the QFN puts it on pad 16; a borrowed map would put
    // the supply on GP0.
    let dip = expect_model("MCP2221A-I/P");
    let qfn = expect_model("MCP2221A-I/ML");
    assert_ne!(dip.id, qfn.id);
    assert_eq!(dip.pins.get("1").map(String::as_str), Some("vdd"));
    assert_eq!(qfn.pins.get("16").map(String::as_str), Some("vdd"));
    assert_eq!(qfn.pins.get("1").map(String::as_str), Some("gp0"));
}

// --- the existing USB bridge cards keep their own codes -----------------------------------------

#[test]
fn usb_existing_bridge_cards_are_not_taken_over() {
    for (code, id) in [
        ("FT231X-Q", "ft231x"),
        ("FT231XS", "ft231xs"),
        ("CH340C", "ch340c"),
        ("CH340G", "ch340g"),
    ] {
        let got = query_id(Some(code), None, None);
        assert_eq!(
            got.as_deref(),
            Some(id),
            "{code} must keep resolving to {id}"
        );
    }
    for code in ["FT232RL", "FT232RQ", "MCP2221A-I/SL", "MCP2221A-I/ML"] {
        let got = query_id(Some(code), None, None).unwrap_or_default();
        for other in ["ft231x", "ft231xs", "ch340c", "ch340g", "ch340x_usb_uart"] {
            assert_ne!(got, other, "{code} must not resolve to {other}");
        }
        assert!(
            !got.starts_with("cp2102"),
            "{code} must not resolve to a cp2102 card"
        );
    }
}

// --- MAX3232 / MAX232 (TI SLLS410O / SLLS047N) -------------------------------------------------

const RS232_MAP: &str =
    "1=c1p 2=vpos 3=c1n 4=c2p 5=c2n 6=vneg 7=dout2 8=rin2 9=rout2 10=din2 11=din1 \
     12=rout1 13=rin1 14=dout1 15=gnd 16=vcc";

#[test]
fn max3232_pin_map_params_and_envelope() {
    for code in [
        "MAX3232",
        "MAX3232CDR",
        "MAX3232IDR",
        "MAX3232CPWR",
        "MAX3232IDWR",
        "MAX3232CDBR",
    ] {
        let m = check_map(code, "max3232", RS232_MAP);
        assert_param(&m, "vil", 0.8);
        assert_param(&m, "vih", 2.0);
        assert!(
            param(&m, "vol") <= 0.4,
            "unloaded VOL must not exceed the printed guarantee"
        );
        assert!(has_supply_range(
            &m,
            "vcc",
            (3.0, 5.5),
            Some(6.0),
            "SLLS410O"
        ));
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(108.2));
    }
}

#[test]
fn max232_pin_map_params_and_envelope() {
    for code in [
        "MAX232",
        "MAX232DR",
        "MAX232IDR",
        "MAX232N",
        "MAX232DWR",
        "MAX232NSR",
    ] {
        let m = check_map(code, "max232", RS232_MAP);
        assert!(
            param(&m, "voh") >= 3.5,
            "unloaded VOH must be at least the printed guarantee"
        );
        assert!(
            param(&m, "vol") <= 0.4,
            "unloaded VOL must not exceed the printed guarantee"
        );
        assert!(has_supply_range(
            &m,
            "vcc",
            (4.5, 5.5),
            Some(6.0),
            "SLLS047N"
        ));
    }
}

#[test]
fn max232_family_siblings_do_not_cross_resolve() {
    assert_not_card(
        "max3232",
        &[
            "MAX232",
            "MAX232DR",
            "MAX3232E",
            "MAX3232ECPE",
            "MAX3232ESE",
            "SN65C3232",
            "MAX3237E",
            "MAX3222",
            "MAX32320",
            "MAX485",
        ],
    );
    assert_not_card(
        "max232",
        &[
            "MAX3232",
            "MAX3232CDR",
            "MAX232A",
            "MAX232E",
            "MAX232CPE",
            "MAX232ACSE",
            "MAX2320",
            "MAX202",
        ],
    );
    // The pre-existing MAX3237E card keeps its own id.
    assert_eq!(
        query_id(Some("MAX3237E"), None, None).as_deref(),
        Some("max3237e")
    );
}

// --- SP3485 (MaxLinear 216DSR00 Rev 2.0.2) ------------------------------------------------------

#[test]
fn sp3485_pin_map_and_params() {
    for code in ["SP3485", "SP3485CN-L", "SP3485EN-L", "SP3485EN-L/TR"] {
        let m = check_map(
            code,
            "sp3485_soic8",
            "1=ro 2=re_n 3=de 4=di 5=gnd 6=a 7=b 8=vcc",
        );
        assert_param(&m, "vih", 2.0);
        assert_param(&m, "vil", 0.8);
        assert!(
            param(&m, "vol") <= 0.4,
            "unloaded VOL must not exceed the printed guarantee"
        );
        assert!(has_supply_range(
            &m,
            "vcc",
            (3.135, 3.465),
            Some(6.0),
            "Rev. 2.0.2"
        ));
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(128.4));
    }
    assert_not_card(
        "sp3485_soic8",
        &[
            "SP3485EEN-L",
            "SP3481",
            "SP3483",
            "SP485",
            "MAX3485",
            "MAX485",
            "SN65HVD3485",
            "SP3485ECN",
        ],
    );
}

// --- MCP2562 / MCP2562FD (DS20005167C / DS20005284A) ----------------------------------------------

#[test]
fn mcp2562_pin_maps_and_envelope() {
    for code in [
        "MCP2562",
        "MCP2562-E/SN",
        "MCP2562-E/P",
        "MCP2562-H/SN",
        "MCP2562T-E/MF",
        "MCP2562-E/MF",
    ] {
        let m = check_map(
            code,
            "mcp2562_8pin",
            "1=txd 2=gnd 3=vdd 4=rxd 5=vio 6=canl 7=canh 8=stby 9=ep EP=ep",
        );
        assert!(has_supply_range(
            &m,
            "vdd",
            (4.5, 5.5),
            Some(7.0),
            "DS20005167C"
        ));
        assert!(has_supply_range(
            &m,
            "vio",
            (1.8, 5.5),
            Some(7.0),
            "DS20005167C"
        ));
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(149.5));
    }
    for code in [
        "MCP2562FD",
        "MCP2562FD-E/SN",
        "MCP2562FD-H/SN",
        "MCP2562FDT-E/MF",
        "MCP2562FD-E/P",
    ] {
        let m = check_map(
            code,
            "mcp2562fd_8pin",
            "1=txd 2=gnd 3=vdd 4=rxd 5=vio 6=canl 7=canh 8=stby 9=ep EP=ep",
        );
        assert!(has_supply_range(
            &m,
            "vdd",
            (4.5, 5.5),
            Some(7.0),
            "DS20005284A"
        ));
    }
}

#[test]
fn mcp2562_fd_and_non_fd_do_not_cross_resolve() {
    assert_not_card(
        "mcp2562_8pin",
        &[
            "MCP2562FD",
            "MCP2562FD-E/SN",
            "MCP2561",
            "MCP2561-E/SN",
            "MCP2561FD",
            "MCP25625",
            "MCP2551",
        ],
    );
    assert_not_card(
        "mcp2562fd_8pin",
        &[
            "MCP2562",
            "MCP2562-E/SN",
            "MCP2561FD",
            "MCP2561FD-E/SN",
            "MCP25625",
            "MCP2551",
        ],
    );
}
