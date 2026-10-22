//! ADC pad tables and strap rows of the STM32 and RP2040 cards, each read from
//! the vendor datasheet (page and table in the card's TOML comments) and
//! cross-checked pad for pad against ST's `STM32_open_pin_data`.

use super::*;
use hauksbee_models::{StrapInternalPull, StrapLevel};

/// The pads of `m` that the card lists as ADC inputs, in numeric order.
fn adc_pads(m: &ModelEntry) -> Vec<u32> {
    let table = m
        .pin_capability
        .adc
        .as_ref()
        .unwrap_or_else(|| panic!("{} declares no ADC table", m.id));
    let mut pads: Vec<u32> = table
        .pads
        .keys()
        .map(|p| p.parse().expect("numeric pad"))
        .collect();
    pads.sort_unstable();
    pads
}

/// A complete table can only describe pads the card maps.
fn assert_adc_pads_are_mapped(m: &ModelEntry) {
    for pad in m
        .pin_capability
        .adc
        .as_ref()
        .expect("ADC table")
        .pads
        .keys()
    {
        assert!(
            m.pins.contains_key(pad),
            "{}: ADC pad {pad} is not in [models.pins]",
            m.id
        );
    }
}

#[test]
fn stm32f072_adc_pads_are_pa0_to_pa7_pb0_pb1() {
    let m = resolve_part("STM32F072CBT6").expect("resolves");
    assert_eq!(m.id, "stm32f072c");
    assert_eq!(adc_pads(&m), (10..=19).collect::<Vec<_>>());
    assert_eq!(m.adc_channel("10"), Some("ADC_IN0"));
    assert_eq!(m.adc_channel("19"), Some("ADC_IN9"));
    // PA8 (pad 29) and the supply pads are not ADC inputs.
    assert_eq!(m.adc_channel("29"), None);
    assert_adc_pads_are_mapped(&m);
}

#[test]
fn stm32f401_maps_its_adc_pads_with_plain_roles() {
    let m = resolve_part("STM32F401CCU6").expect("resolves");
    assert_eq!(m.id, "stm32f401c");
    assert_eq!(adc_pads(&m), (10..=19).collect::<Vec<_>>());
    assert_pins(&m, &pins("10=pa0 17=pa7 18=pb0 19=pb1 44=boot0"));
    assert_eq!(m.adc_channel("12"), Some("ADC1_IN2"));
    assert_adc_pads_are_mapped(&m);
}

#[test]
fn stm32g474_adc_pads_follow_the_ufqfpn48_column() {
    let m = resolve_part("STM32G474CEU6").expect("resolves");
    assert_eq!(m.id, "stm32g474c");
    assert_eq!(
        adc_pads(&m),
        vec![5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 24, 25, 26, 27, 28, 30, 31]
    );
    // PA2 is ADC1_IN3 only; PA3 (pad 11) is ADC1_IN4 only; PC4 (pad 16) is ADC2_IN5.
    assert_eq!(m.adc_channel("10"), Some("ADC1_IN3"));
    assert_eq!(m.adc_channel("16"), Some("ADC2_IN5"));
    assert_adc_pads_are_mapped(&m);
}

#[test]
fn stm32f77x_adc_pads_are_on_lqfp100() {
    let m = resolve_part("STM32F777VIT6").expect("resolves");
    assert_eq!(m.id, "stm32f77xv");
    assert_eq!(
        adc_pads(&m),
        vec![15, 16, 17, 18, 22, 23, 24, 25, 28, 29, 30, 31, 32, 33, 34, 35]
    );
    // PA0..PA3 reach all three converters, PA4..PA7 only ADC1 and ADC2.
    assert_eq!(m.adc_channel("22"), Some("ADC1_IN0/ADC2_IN0/ADC3_IN0"));
    assert_eq!(m.adc_channel("28"), Some("ADC1_IN4/ADC2_IN4"));
    assert_adc_pads_are_mapped(&m);
}

#[test]
fn stm32l0x1_has_adc_pads_and_a_boot0_strap_on_pb9() {
    let m = resolve_part("STM32L011K4T6").expect("resolves");
    assert_eq!(m.id, "stm32l0x1k_lqfp32");
    assert_eq!(adc_pads(&m), (6..=15).collect::<Vec<_>>());
    assert_adc_pads_are_mapped(&m);

    // RM0377 Rev 10 Table 4: with nBOOT_SEL = 0 (default) the BOOT0 pin picks
    // the boot area; the pad is a dedicated input with no internal pull.
    assert_pins(&m, &pins("31=pb9_boot0"));
    let boot0 = m
        .straps
        .iter()
        .find(|s| s.role == "pb9_boot0")
        .expect("BOOT0 strap row");
    assert_eq!(boot0.level, StrapLevel::Low);
    assert!(boot0.boot_select);
    assert_eq!(boot0.internal_pull, StrapInternalPull::None);
    // Table 50: VIL max 0.14 VDD, VIH min 0.15 VDD + 0.56 V.
    assert_eq!(boot0.vil, Some([0.14, 0.0]));
    assert_eq!(boot0.vih, Some([0.15, 0.56]));
    // The NRST row is unchanged.
    assert!(m.straps.iter().any(|s| s.role == "reset"));
}

#[test]
fn rp2040_run_pad_is_an_active_low_pulled_up_reset_not_a_boot_select() {
    for query in [(Some("RP2040"), None), (Some("RP2040"), Some("QFN-56"))] {
        let m = expect_query(query.0, None, query.1);
        assert_pins(&m, &pins("26=run 56=qspi_ss"));
        let run = m
            .straps
            .iter()
            .find(|s| s.role == "run")
            .unwrap_or_else(|| panic!("{}: no RUN strap row", m.id));
        // RP2040 datasheet Table 619: RUN, pad 26, reset state pull-up.
        assert_eq!(run.level, StrapLevel::High);
        assert_eq!(run.internal_pull, StrapInternalPull::PullUp);
        assert!(
            !run.boot_select,
            "RUN resets the chip; it selects no boot source"
        );
    }
}
