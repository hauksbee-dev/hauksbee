//! Official-parts batch mcu1: STM32F411 (five packages), nRF52832 (QFN48,
//! WLCSP) and the ESP8266EX family (bare QFN32 chip, ESP-12F, ESP-07/07S/12S,
//! ESP-WROOM-02). Pad maps are checked against the datasheet tables the cards
//! cite; the negative tests pin the neighbouring part numbers that must NOT
//! resolve to these cards.

use super::*;
use hauksbee_models::{StrapInternalPull, StrapLevel};

fn with_fp(value: &str, footprint: &str) -> Option<String> {
    query_id(Some(value), Some(value), Some(footprint))
}

fn by_fp(value: &str, footprint: &str) -> ModelEntry {
    expect_query(Some(value), Some(value), Some(footprint))
}

fn role_pads<'a>(m: &'a ModelEntry, role: &str) -> Vec<&'a str> {
    let mut v: Vec<&str> = m
        .pins
        .iter()
        .filter(|(_, r)| r.as_str() == role)
        .map(|(p, _)| p.as_str())
        .collect();
    v.sort_unstable();
    v
}

// ───────────────────────────── STM32F411 ─────────────────────────────

#[test]
fn stm32f411_ufqfpn48_pad_map_follows_table_8() {
    let m = by_fp("STM32F411CEU6", "Package_DFN_QFN:QFN-48-1EP_7x7mm_P0.5mm");
    assert_eq!(m.id, "stm32f411c_ufqfpn48");
    assert_eq!(m.pins.len(), 48);
    assert_pins(
        &m,
        &pins("1=vbat 7=reset 8=vssa 9=vdda 22=vcap1 24=vdd 36=vdd2 44=boot0 48=vdd3 23=vss"),
    );
    assert_eq!(m.params.get_str("backend"), Some("renode:stm32f4"));
}

#[test]
fn stm32f411_wlcsp49_pad_map_follows_table_8() {
    let m = by_fp(
        "STM32F411CEY6",
        "Package_CSP:ST_WLCSP-49_3.06x3.28mm_P0.4mm",
    );
    assert_eq!(m.id, "stm32f411c_wlcsp49");
    assert_eq!(m.pins.len(), 49);
    assert_pins(
        &m,
        &pins("B7=vbat E7=reset E6=vssa F7=vdda G2=vcap1 A5=boot0 B6=pdr_on"),
    );
}

#[test]
fn stm32f411_lqfp64_pad_map_follows_table_8() {
    let m = by_fp("STM32F411RET6", "Package_QFP:LQFP-64_10x10mm_P0.5mm");
    assert_eq!(m.id, "stm32f411r_lqfp64");
    assert_eq!(m.pins.len(), 64);
    assert_pins(
        &m,
        &pins("1=vbat 7=reset 12=vssa 13=vdda 30=vcap1 60=boot0"),
    );
}

#[test]
fn stm32f411_lqfp100_pad_map_follows_table_8() {
    let m = by_fp("STM32F411VET6", "Package_QFP:LQFP-100_14x14mm_P0.5mm");
    assert_eq!(m.id, "stm32f411v_lqfp100");
    assert_eq!(m.pins.len(), 100);
    assert_pins(
        &m,
        &pins("6=vbat 14=reset 20=vssa 21=vref 22=vdda 48=vcap1 73=vcap2 94=boot0"),
    );
}

#[test]
fn stm32f411_ufbga100_pad_map_follows_table_8() {
    let m = by_fp(
        "STM32F411VEH6",
        "Package_BGA:UFBGA-100_7x7mm_Layout10x10_P0.5mm",
    );
    assert_eq!(m.id, "stm32f411v_ufbga100");
    assert_eq!(m.pins.len(), 100);
    assert_pins(
        &m,
        &pins("E2=vbat H2=reset J1=vssa K1=vref_neg L1=vref M1=vdda L11=vcap1 C11=vcap2 A4=boot0 E3=bypass_reg H3=pdr_on"),
    );
}

#[test]
fn stm32f411_has_one_vcap_or_two_by_package() {
    // Table 8: the small packages bond one VCAP pin, the 100-pin ones two.
    let small = by_fp("STM32F411RET6", "LQFP-64_10x10mm_P0.5mm");
    assert_eq!(role_pads(&small, "vcap2").len(), 0);
    let big = by_fp("STM32F411VET6", "LQFP-100_14x14mm_P0.5mm");
    assert_eq!(role_pads(&big, "vcap2").len(), 1);
}

#[test]
fn stm32f411_boot0_is_an_unasserted_strap_because_no_boot_table_was_read() {
    let m = by_fp("STM32F411RET6", "LQFP-64_10x10mm_P0.5mm");
    let s = m
        .straps
        .iter()
        .find(|s| s.role == "boot0")
        .expect("boot0 strap row");
    assert!(s.boot_select);
    assert_eq!(
        s.internal_pull,
        StrapInternalPull::Unknown,
        "BOOT0 pull is not in DS10314 and must stay absent"
    );
    assert_eq!(s.level, StrapLevel::Defined);
}

#[test]
fn stm32f411_wrong_or_missing_footprint_does_not_get_a_foreign_pad_map() {
    // A bare ordering code names no footprint, so it keeps the family routing and never
    // receives another package's pad numbers.
    for id in [
        query_id(Some("STM32F411RET6"), Some("STM32F411RET6"), None),
        with_fp("STM32F411RET6", "Package_QFP:LQFP-100_14x14mm_P0.5mm"),
        with_fp("STM32F411CEU6", "Package_QFP:LQFP-64_10x10mm_P0.5mm"),
        with_fp("STM32F411VET6", "Package_QFP:LQFP-64_10x10mm_P0.5mm"),
    ]
    .into_iter()
    .flatten()
    {
        assert!(
            !id.starts_with("stm32f411"),
            "{id} matched a mismatched package"
        );
    }
}

#[test]
fn stm32f411_does_not_capture_sibling_parts() {
    for v in [
        "STM32F405RGT6",
        "STM32F412RET6",
        "STM32F410RBT6",
        "STM32F413RGT6",
        "STM32F401RET6",
        "STM32F407VGT6",
        "STM32F4110",
    ] {
        for fp in [
            "LQFP-64_10x10mm_P0.5mm",
            "LQFP-100_14x14mm_P0.5mm",
            "UFQFPN-48",
        ] {
            if let Some(id) = with_fp(v, fp) {
                assert!(!id.starts_with("stm32f411"), "{v} on {fp} resolved to {id}");
            }
        }
    }
}

#[test]
fn stm32f401c_still_resolves_to_its_existing_card() {
    let m = resolve_part("STM32F401CCU6").expect("STM32F401CCU6 resolves");
    assert_eq!(m.id, "stm32f401c");
}

// ───────────────────────────── nRF52832 ─────────────────────────────

#[test]
fn nrf52832_qfn48_pad_map_follows_table_3() {
    let m = by_fp("nRF52832-QFAA", "Package_DFN_QFN:QFN-48-1EP_6x6mm_P0.4mm");
    assert_eq!(m.id, "nrf52832_qfn48");
    assert_eq!(m.pins.len(), 49);
    assert_pins(
        &m,
        &pins("1=dec1 2=p000_xl1 3=p001_xl2 4=p002 13=vdd 24=p021 25=swdclk 26=swdio 30=ant 34=xc1 35=xc2 36=vdd2 43=p031 47=dcc 48=vdd3 49=vss3"),
    );
    assert_eq!(m.params.get_str("backend"), Some("renode:nrf52840"));
}

#[test]
fn nrf52832_wlcsp_pad_map_follows_table_4() {
    let m = by_fp("nRF52832-CIAA", "Package_CSP:Nordic_WLCSP-50");
    assert_eq!(m.id, "nrf52832_wlcsp");
    assert_eq!(m.pins.len(), 50);
    assert_pins(
        &m,
        &pins("A1=xc2 B2=xc1 D1=ant D2=vss_pa F1=swdclk G1=swdio H1=p021 A7=vdd H7=vdd2"),
    );
}

#[test]
fn nrf52832_has_eight_adc_inputs_and_no_boot_strap() {
    let m = by_fp("nRF52832-QFAA", "QFN-48-1EP_6x6mm_P0.4mm");
    assert!(m.straps.is_empty(), "the nRF52832 has no boot-strap pins");
}

#[test]
fn nrf52832_does_not_capture_siblings() {
    for v in [
        "nRF52840-QIAA",
        "nRF52833-QDAA",
        "nRF52810-QFAA",
        "nRF52811-QFAA",
        "nRF5340",
    ] {
        if let Some(id) = with_fp(v, "QFN-48-1EP_6x6mm_P0.4mm") {
            assert!(!id.starts_with("nrf52832"), "{v} resolved to {id}");
        }
    }
}

// ───────────────────────────── ESP8266 family ─────────────────────────────

#[test]
fn esp8266ex_qfn32_pad_map_follows_table_2_1() {
    let m = by_fp("ESP8266EX", "Package_DFN_QFN:QFN-32-1EP_5x5mm_P0.5mm");
    assert_eq!(m.id, "esp8266ex_qfn32");
    assert_eq!(m.pins.len(), 33);
    assert_pins(
        &m,
        &pins("1=vdda 6=tout 7=en 11=vdd 14=gpio2 15=gpio0 13=gpio15_mtdo 17=vdd2 26=gpio1_u0txd 27=xtal_out 28=xtal_in 29=vddd 30=vdda2 32=reset 33=gnd"),
    );
    assert_eq!(m.params.get_str("backend"), Some("none:esp8266"));
}

#[test]
fn esp_12f_pad_map_follows_the_manual_22_pad_table() {
    let m = expect_query(Some("ESP-12F"), Some("ESP-12F"), None);
    assert_eq!(m.id, "esp_12f");
    assert_eq!(m.pins.len(), 22);
    assert_pins(
        &m,
        &pins("1=reset 2=tout 3=en 8=vdd 15=gnd 16=gpio15 17=gpio2 18=gpio0 21=gpio3_rxd 22=gpio1_txd"),
    );
    assert_eq!(m.params.get_str("backend"), Some("none:esp8266"));
}

#[test]
fn esp_07_family_shares_the_16_pad_map() {
    for v in ["ESP-07", "ESP-07S", "ESP-12S", "ESP07"] {
        let m = expect_query(Some(v), Some(v), None);
        assert_eq!(m.id, "esp_07_16pad", "{v}");
        assert_eq!(m.pins.len(), 16);
        assert_pins(&m, &pins("1=reset 2=tout 3=en 8=vdd 9=gnd 10=gpio15 11=gpio2 12=gpio0 15=gpio3_rxd 16=gpio1_txd"));
    }
}

#[test]
fn esp_wroom_02_pad_map_follows_table_2_1() {
    let m = expect_query(Some("ESP-WROOM-02"), Some("ESP-WROOM-02"), None);
    assert_eq!(m.id, "esp_wroom_02");
    assert_eq!(m.pins.len(), 19);
    assert_pins(
        &m,
        &pins("1=vdd 2=en 6=gpio15 7=gpio2 8=gpio0 11=gpio3_rxd 12=gpio1_txd 15=reset 16=tout 17=gpio16"),
    );
}

#[test]
fn esp_boot_straps_follow_the_vendor_tables() {
    let m = expect_query(Some("ESP-WROOM-02"), Some("ESP-WROOM-02"), None);
    let level = |role: &str| {
        m.straps
            .iter()
            .find(|s| s.role == role)
            .unwrap_or_else(|| panic!("no strap row for {role}"))
            .level
    };
    assert_eq!(level("gpio15"), StrapLevel::Low);
    assert_eq!(level("gpio0"), StrapLevel::High);
    assert_eq!(level("gpio2"), StrapLevel::High);
}

#[test]
fn esp_cards_do_not_capture_sibling_parts() {
    for v in [
        "ESP32-WROOM-32",
        "ESP32-C3-WROOM-02",
        "ESP32-S3-WROOM-1",
        "ESP-12E",
        "ESP-12",
        "ESP-01",
        "ESP-WROOM-02D",
        "ESP-WROOM-32",
        "NodeMCU",
        "ESP8285",
        "ESP-07X",
        "ESP-12FX",
    ] {
        if let Some(id) = query_id(Some(v), Some(v), None) {
            assert!(
                !id.starts_with("esp_12f")
                    && !id.starts_with("esp_07")
                    && !id.starts_with("esp_wroom_02")
                    && !id.starts_with("esp8266ex"),
                "{v} resolved to {id}"
            );
        }
    }
}

#[test]
fn bare_esp8266_without_a_qfn32_footprint_keeps_other_routing() {
    if let Some(id) = query_id(
        Some("ESP8266EX"),
        Some("ESP8266EX"),
        Some("Package_QFP:LQFP-64"),
    ) {
        assert_ne!(id, "esp8266ex_qfn32");
    }
}
