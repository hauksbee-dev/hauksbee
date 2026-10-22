//! Official-parts batch mcu2: resolution and behaviour tests for the cards added in that batch.
//!
//! STM32F405/F407 (DS8626 Rev 12), ATSAMD21G18 (DS40001882L), ATmega2560 (DS40002211A).
//! The expected pads below are read from the datasheets' own pin tables and figures, not from
//! the card text.

use super::*;
use hauksbee_models::{ComponentKind, StrapInternalPull, StrapLevel};

/// The model for a part written as `value` with no footprint.
fn mpn_only(value: &str) -> Option<ModelEntry> {
    resolve_part(value)
}

fn strap<'a>(m: &'a ModelEntry, role: &str) -> &'a hauksbee_models::StrapPin {
    m.straps
        .iter()
        .find(|s| s.role == role)
        .unwrap_or_else(|| panic!("{}: no strap row for {role}", m.id))
}

// ---------------------------------------------------------------------------
// STM32F405 / STM32F407
// ---------------------------------------------------------------------------

const F4_IDS: [&str; 6] = [
    "stm32f40x_lqfp64",
    "stm32f40x_wlcsp90",
    "stm32f40x_lqfp100",
    "stm32f40x_lqfp144",
    "stm32f407_lqfp176",
    "stm32f407_ufbga176",
];

#[test]
fn f4_ordering_codes_resolve_to_their_package() {
    // DS8626 section 8 ordering information: pin letter R/O/V/Z/I = 64/90/100/144/176 pins,
    // package letter T = LQFP, H = UFBGA, Y = WLCSP; Table 1 lists the pairs that exist.
    let cases = [
        ("STM32F405RGT6", "stm32f40x_lqfp64"),
        ("STM32F405RGT7", "stm32f40x_lqfp64"),
        ("STM32F405RGTx", "stm32f40x_lqfp64"),
        ("STM32F405RGT6TR", "stm32f40x_lqfp64"),
        ("STM32F405OGY6", "stm32f40x_wlcsp90"),
        ("STM32F405OEY7", "stm32f40x_wlcsp90"),
        ("STM32F405VGT6", "stm32f40x_lqfp100"),
        ("STM32F407VGT6", "stm32f40x_lqfp100"),
        ("STM32F407VET6", "stm32f40x_lqfp100"),
        ("STM32F407VGT7", "stm32f40x_lqfp100"),
        ("STM32F407VG", "stm32f40x_lqfp100"),
        ("STM32F405ZGT6", "stm32f40x_lqfp144"),
        ("STM32F407ZGT6", "stm32f40x_lqfp144"),
        ("STM32F407ZET6", "stm32f40x_lqfp144"),
        ("STM32F407IGT6", "stm32f407_lqfp176"),
        ("STM32F407IET6", "stm32f407_lqfp176"),
        ("STM32F407IGH6", "stm32f407_ufbga176"),
        ("STM32F407IEH6", "stm32f407_ufbga176"),
        ("stm32f407vgt6", "stm32f40x_lqfp100"),
    ];
    for (part, id) in cases {
        let m = mpn_only(part).unwrap_or_else(|| panic!("{part} did not resolve"));
        assert_eq!(m.id, id, "{part}");
        assert_eq!(m.kind, ComponentKind::Mcu, "{part}");
    }
}

#[test]
fn f4_siblings_and_impossible_codes_do_not_resolve_to_these_cards() {
    // Not in Table 1: F405 has no VE/ZE/IG parts, F407 has no R/O parts, and a bare 176-pin code
    // names no package (T is LQFP176, H is UFBGA176). Other family members have other pinouts.
    for part in [
        "STM32F405VET6",
        "STM32F405ZET6",
        "STM32F405IGT6",
        "STM32F407RGT6",
        "STM32F407OGY6",
        "STM32F407IG",
        "STM32F407IE",
        "STM32F407VGY6",
        "STM32F407IGY6",
        "STM32F405RGH6",
        "STM32F415RGT6",
        "STM32F417VGT6",
        "STM32F427VGT6",
        "STM32F429ZIT6",
        "STM32F401CCU6",
        "STM32F411CEU6",
        "STM32F446RET6",
        "STM32F4",
        "STM32F407",
    ] {
        let got = mpn_only(part).map(|m| m.id);
        assert!(
            got.as_deref().is_none_or(|id| !F4_IDS.contains(&id)),
            "{part} must not resolve to an mcu2 STM32F40x card, got {got:?}"
        );
    }
}

#[test]
fn f4_lqfp100_pad_map_matches_table_7() {
    let m = mpn_only("STM32F407VGT6").expect("F407VGT6");
    assert_eq!(m.pins.len(), 100, "LQFP100 has 100 pins");
    assert_pins(
        &m,
        &pins(
            "1=pe2 5=pe6 6=vbat 7=pc13 8=pc14_osc32_in 9=pc15_osc32_out 10=vss 11=vdd \
             12=osc_in 13=osc_out 14=reset 15=pc0_adc10 19=vdd2 20=vssa 21=vref 22=vdda \
             23=pa0_adc0 27=vss2 28=vdd3 37=pb2_boot1 49=vcap1 50=vdd4 72=pa13_swdio \
             73=vcap2 74=vss3 75=vdd5 76=pa14_swclk 94=boot0 99=vss4 100=vdd6",
        ),
    );
}

#[test]
fn f4_lqfp64_pad_map_matches_figure_12() {
    let m = mpn_only("STM32F405RGT6").expect("F405RGT6");
    assert_eq!(m.pins.len(), 64);
    assert_pins(
        &m,
        &pins(
            "1=vbat 5=osc_in 6=osc_out 7=reset 12=vssa 13=vdda 14=pa0_adc0 18=vss 19=vdd \
             28=pb2_boot1 31=vcap1 32=vdd2 46=pa13_swdio 47=vcap2 48=vdd3 49=pa14_swclk \
             60=boot0 63=vss2 64=vdd4",
        ),
    );
    // LQFP64 has no VREF+ pin (it is bonded to VDDA) and no PDR_ON or BYPASS_REG.
    for role in ["vref", "pdr_on", "bypass_reg"] {
        assert!(!m.pins.values().any(|r| r == role), "LQFP64 has no {role}");
    }
}

#[test]
fn f4_lqfp144_pad_map_matches_figure_14() {
    let m = mpn_only("STM32F407ZGT6").expect("F407ZGT6");
    assert_eq!(m.pins.len(), 144);
    assert_pins(
        &m,
        &pins(
            "6=vbat 23=osc_in 24=osc_out 25=reset 31=vssa 32=vref 33=vdda 34=pa0_adc0 \
             71=vcap1 106=vcap2 138=boot0 143=pdr_on 144=vdd12",
        ),
    );
    // Table 3: LQFP144 has PDR_ON but no BYPASS_REG (regulator always on).
    assert!(!m.pins.values().any(|r| r == "bypass_reg"));
}

#[test]
fn f4_176_pin_packages_have_pdr_on_and_bypass_reg() {
    let lq = mpn_only("STM32F407IGT6").expect("F407IGT6");
    assert_eq!(lq.pins.len(), 176);
    assert_pins(
        &lq,
        &pins(
            "6=vbat 31=reset 37=vssa 38=vref 39=vdda 40=pa0_adc0 48=bypass_reg 81=vcap1 \
             125=vcap2 166=boot0 171=pdr_on 172=vdd15",
        ),
    );
    let bga = mpn_only("STM32F407IGH6").expect("F407IGH6");
    assert_eq!(bga.pins.len(), 176);
    assert_pins(
        &bga,
        &pins(
            "C1=vbat J1=reset M1=vssa N1=vref_n P1=vref R1=vdda N3=pa0_adc0 L4=bypass_reg \
             M10=vcap1 F13=vcap2 D6=boot0 C6=pdr_on C5=vdd",
        ),
    );
}

#[test]
fn f4_wlcsp90_pad_map_matches_table_7() {
    let m = mpn_only("STM32F405OGY6").expect("F405OGY6");
    assert_eq!(m.pins.len(), 90);
    assert_pins(
        &m,
        &pins(
            "A10=vbat G10=reset H10=vssa G9=vdda C10=pa0_adc0 D9=bypass_reg F4=vcap1 B1=vcap2 \
             A7=boot0 A8=pdr_on A1=vdd",
        ),
    );
}

#[test]
fn f4_adc_pads_are_the_adc_channel_pins_and_nothing_else() {
    // Table 2 channel counts: LQFP64 16, WLCSP90 13, LQFP100 16, LQFP144 24, 176-pin 24.
    for (part, channels) in [
        ("STM32F405RGT6", 16),
        ("STM32F405OGY6", 13),
        ("STM32F407VGT6", 16),
        ("STM32F407ZGT6", 24),
        ("STM32F407IGT6", 24),
        ("STM32F407IGH6", 24),
    ] {
        let m = mpn_only(part).expect(part);
        let adc = m.pin_capability.adc.as_ref().expect("adc table declared");
        assert_eq!(adc.pads.len(), channels, "{part}");
    }
    let m = mpn_only("STM32F407VGT6").unwrap();
    // LQFP100: PA0 pad 23 is ADC123_IN0, PA4 pad 29 is ADC12_IN4, PB0 pad 35 is ADC12_IN8.
    assert_eq!(m.adc_channel("23"), Some("ADC123_IN0"));
    assert_eq!(m.adc_channel("29"), Some("ADC12_IN4"));
    assert_eq!(m.adc_channel("35"), Some("ADC12_IN8"));
    assert_eq!(m.adc_channel("94"), None, "BOOT0 is not an ADC pad");
    // PF3-PF10 are ADC3 only and exist on the 144/176-pin packages.
    let z = mpn_only("STM32F407ZGT6").unwrap();
    assert!(z
        .pin_capability
        .adc
        .as_ref()
        .unwrap()
        .pads
        .values()
        .any(|c| c == "ADC3_IN9"));
}

#[test]
fn f4_boot_and_reset_straps() {
    for part in ["STM32F405RGT6", "STM32F407VGT6", "STM32F407IGH6"] {
        let m = mpn_only(part).expect(part);
        let boot0 = strap(&m, "boot0");
        assert!(boot0.boot_select, "{part}: BOOT0 selects the boot memory");
        assert_eq!(
            boot0.level,
            StrapLevel::Low,
            "{part}: BOOT0 low is a main-flash boot"
        );
        // Table 48: BOOT0 VIL max 0.1 VDD + 0.1 V, VIH min 0.17 VDD + 0.7 V.
        assert_eq!(boot0.vil, Some([0.1, 0.1]));
        assert_eq!(boot0.vih, Some([0.17, 0.7]));
        // Not stated by the datasheet or RM0090 2.4: must stay unknown, not guessed.
        assert_eq!(boot0.internal_pull, StrapInternalPull::Unknown);
        let rst = strap(&m, "reset");
        assert_eq!(
            rst.internal_pull,
            StrapInternalPull::PullUp,
            "{part}: NRST RPU"
        );
        assert_eq!(rst.level, StrapLevel::High);
        assert!(!rst.boot_select);
        assert_eq!(
            param(&m, "reset_pull_up_ohms"),
            30_000.0,
            "Table 51 RPU min 30 kOhm"
        );
    }
}

#[test]
fn f4_ratings_envelope_and_levels_are_the_datasheet_figures() {
    let m = mpn_only("STM32F407VGT6").expect("F407VGT6");
    assert_eq!(m.ratings.max_pin_current_a, Some(0.025), "Table 12 IIO");
    assert_eq!(m.ratings.max_current_a, Some(0.24), "Table 12 IVDD");
    assert_eq!(m.ratings.max_voltage_v, Some(4.0), "Table 11 VDD-VSS");
    assert_eq!(m.ratings.max_junction_temp_c, Some(125.0), "Table 13 TJ");
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(43.0), "Table 98 LQFP100");
    assert!(has_supply_range(
        &m,
        "vdd",
        (1.8, 3.6),
        Some(4.0),
        "Table 14"
    ));
    assert!(has_supply_range(
        &m,
        "vdda",
        (1.8, 3.6),
        Some(4.0),
        "Table 14"
    ));
    assert!(has_supply_range(&m, "vbat", (1.65, 3.6), None, "Table 14"));
    assert!(m.envelope.iter().any(|e| matches!(
        e,
        OperatingEnvelope::RailOrder { lower, upper, .. } if lower == "vref" && upper == "vdda"
    )));
    assert!(m.envelope.iter().any(|e| matches!(
        e,
        OperatingEnvelope::InputTolerance { pins, max_v, .. }
            if *max_v == 5.5 && pins.iter().any(|p| p == "boot0") && pins.iter().any(|p| p == "pa9")
    )));
    // Thermal resistance per package, Table 98.
    for (part, theta) in [
        ("STM32F405RGT6", 46.0),
        ("STM32F405OGY6", 38.1),
        ("STM32F407ZGT6", 40.0),
        ("STM32F407IGT6", 38.0),
        ("STM32F407IGH6", 39.0),
    ] {
        assert_eq!(
            mpn_only(part).unwrap().ratings.theta_ja_c_per_w,
            Some(theta),
            "{part}"
        );
    }
    assert_eq!(param(&m, "xtal_cap_min_f"), 5.0e-12);
    assert_eq!(param(&m, "xtal_cap_max_f"), 25.0e-12);
    assert_eq!(param(&m, "default_clock_hz"), 16_000_000.0);
    assert_eq!(m.params.get_str("backend"), Some("renode:stm32f4"));
    // VIH 0.7 VDD and VIL 0.3 VDD - 0.04 V at the 3.6 V end of the range.
    let vih = &m.levels["vih"];
    assert!((hauksbee_models::schema::level_at(vih, 3.6) - 2.52).abs() < 1e-9);
    let vil = &m.levels["vil"];
    assert!((hauksbee_models::schema::level_at(vil, 3.6) - 1.04).abs() < 1e-9);
}

#[test]
fn f4_vcap_pins_are_outputs_not_supplies() {
    let m = mpn_only("STM32F407VGT6").unwrap();
    let outs: Vec<&str> = m.supply_outputs.iter().map(|o| o.role.as_str()).collect();
    assert_eq!(outs, ["vcap1", "vcap2"]);
    assert!(!m.envelope.iter().any(
        |e| matches!(e, OperatingEnvelope::SupplyRange { pin, .. } if pin.starts_with("vcap"))
    ));
}

// ---------------------------------------------------------------------------
// ATSAMD21G18A
// ---------------------------------------------------------------------------

const SAMD21_IDS: [&str; 2] = ["atsamd21g18a_tqfp48", "atsamd21g18a_qfn48"];

#[test]
fn samd21_ordering_codes_resolve_to_their_package() {
    // DS40001882L section 3 Ordering Information: ATSAMD21G18A-AU / -AUT are the TQFP,
    // -MU / -MUT the VQFN, -40 to 85 C matte tin.
    for (part, id) in [
        ("ATSAMD21G18A-AU", "atsamd21g18a_tqfp48"),
        ("ATSAMD21G18A-AUT", "atsamd21g18a_tqfp48"),
        ("atsamd21g18a-au", "atsamd21g18a_tqfp48"),
        ("ATSAMD21G18A-MU", "atsamd21g18a_qfn48"),
        ("ATSAMD21G18A-MUT", "atsamd21g18a_qfn48"),
    ] {
        let m = mpn_only(part).unwrap_or_else(|| panic!("{part} did not resolve"));
        assert_eq!(m.id, id, "{part}");
        assert_eq!(m.kind, ComponentKind::Mcu, "{part}");
    }
}

#[test]
fn samd21_siblings_and_other_grades_do_not_resolve_to_these_cards() {
    // The bare family name names no package; the N/F/Z grades (105 and 125 C) have their own
    // electrical tables; E, J and the smaller G memory sizes have other pinouts or ratings.
    for part in [
        "ATSAMD21G18A",
        "ATSAMD21G18A-AN",
        "ATSAMD21G18A-AF",
        "ATSAMD21G18A-AZ",
        "ATSAMD21G18A-MN",
        "ATSAMD21G18A-MF",
        "ATSAMD21G18A-MZ",
        "ATSAMD21G18A-CU",
        "ATSAMD21G17A-AU",
        "ATSAMD21G16A-MU",
        "ATSAMD21E18A-AU",
        "ATSAMD21E18A-MU",
        "ATSAMD21J18A-AU",
        "ATSAMD21G18L-AU",
        "ATSAMD51G19A-AU",
        "ATSAMD21",
    ] {
        let got = mpn_only(part).map(|m| m.id);
        assert!(
            got.as_deref().is_none_or(|id| !SAMD21_IDS.contains(&id)),
            "{part} must not resolve to an mcu2 SAMD21G18A card, got {got:?}"
        );
    }
    // Each package's code stays on its own package.
    assert_eq!(
        mpn_only("ATSAMD21G18A-AU").unwrap().id,
        "atsamd21g18a_tqfp48"
    );
    assert_eq!(
        mpn_only("ATSAMD21G18A-MU").unwrap().id,
        "atsamd21g18a_qfn48"
    );
}

#[test]
fn samd21_pad_map_matches_section_5_2_1() {
    for part in ["ATSAMD21G18A-AU", "ATSAMD21G18A-MU"] {
        let m = mpn_only(part).expect(part);
        assert_eq!(
            m.pins.len(),
            48,
            "{part}: 48 pads, the exposed pad is not connected inside"
        );
        assert_pins(
            &m,
            &pins(
                "1=pa00_xin32 2=pa01_xout32 5=gnda 6=vddana 17=vddio 18=gnd 23=pa14_xin \
                 24=pa15_xout 33=pa24_usb_dm 34=pa25_usb_dp 35=gnd2 36=vddio2 40=reset_n \
                 42=gnd3 43=vddcore 44=vcc 45=pa30_swclk 46=pa31_swdio 47=pb02 48=pb03",
            ),
        );
    }
}

#[test]
fn samd21_adc_pads_are_the_ain_pins_of_table_7_1() {
    for part in ["ATSAMD21G18A-AU", "ATSAMD21G18A-MU"] {
        let m = mpn_only(part).expect(part);
        let adc = m.pin_capability.adc.as_ref().expect("adc table declared");
        assert_eq!(
            adc.pads.len(),
            14,
            "{part}: 14 ADC channels on the G48 package"
        );
        assert_eq!(m.adc_channel("3"), Some("AIN0"), "PA02");
        assert_eq!(m.adc_channel("4"), Some("AIN1"), "PA03");
        assert_eq!(m.adc_channel("7"), Some("AIN2"), "PB08");
        assert_eq!(m.adc_channel("13"), Some("AIN16"), "PA08");
        assert_eq!(m.adc_channel("16"), Some("AIN19"), "PA11");
        assert_eq!(m.adc_channel("47"), Some("AIN10"), "PB02");
        assert_eq!(m.adc_channel("48"), Some("AIN11"), "PB03");
        assert_eq!(m.adc_channel("6"), None, "VDDANA is not an ADC pad");
        assert_eq!(m.adc_channel("40"), None, "RESET is not an ADC pad");
    }
}

#[test]
fn samd21_straps_ratings_envelope_and_levels_are_the_datasheet_figures() {
    let m = mpn_only("ATSAMD21G18A-AU").expect("TQFP");
    let rst = strap(&m, "reset_n");
    assert_eq!(
        rst.internal_pull,
        StrapInternalPull::PullUp,
        "16.6.2.7 internal pull-up"
    );
    assert_eq!(rst.level, StrapLevel::High);
    assert!(!rst.boot_select, "SAM D21 has no boot-mode pin");
    // Table 37-15 prints the thresholds by VDD range, which one affine row cannot hold.
    assert_eq!((rst.vil, rst.vih), (None, None));
    assert_eq!(
        strap(&m, "pa30_swclk").internal_pull,
        StrapInternalPull::PullUp
    );
    assert_eq!(m.straps.len(), 2);
    assert_eq!(
        param(&m, "reset_pull_up_ohms"),
        20_000.0,
        "Table 37-15 RPULL min"
    );
    assert_eq!(
        param(&m, "default_clock_hz"),
        1_000_000.0,
        "OSC8M divided by 8 after reset"
    );
    assert_eq!(m.params.get_str("backend"), Some("none:samd21"));
    assert!(
        m.params.get_f64("max_adc_source_impedance_ohms").is_none(),
        "RSAMPLE is internal"
    );
    assert!(
        m.params.get_f64("xtal_cap_min_f").is_none(),
        "no capacitor range is printed"
    );

    assert_eq!(m.ratings.max_current_a, Some(0.092), "Table 37-2 IVDD");
    assert_eq!(m.ratings.max_voltage_v, Some(3.8), "Table 37-2 VDD");
    assert_eq!(m.ratings.max_junction_temp_c, Some(100.0), "Table 37-3 TJ");
    assert_eq!(
        m.ratings.theta_ja_c_per_w,
        Some(63.6),
        "Table 37-1 48-pin TQFP"
    );
    assert_eq!(
        mpn_only("ATSAMD21G18A-MU")
            .unwrap()
            .ratings
            .theta_ja_c_per_w,
        Some(32.0)
    );
    for pin in ["vcc", "vddio", "vddana"] {
        assert!(
            has_supply_range(&m, pin, (1.62, 3.63), Some(3.8), "Table 37-3"),
            "{pin}"
        );
    }
    let core = &m.supply_outputs;
    assert_eq!(core.len(), 1);
    assert_eq!(
        (core[0].role.as_str(), core[0].min, core[0].max),
        ("vddcore", 1.1, 1.3)
    );
    // VIH min 0.55 VDD and VIL max 0.3 VDD at 3.63 V; VIL max 0.25 VDD at 2.7 V and below.
    let at = |name: &str, v: f64| hauksbee_models::schema::level_at(&m.levels[name], v);
    assert!((at("vih", 3.63) - 1.9965).abs() < 1e-9);
    assert!((at("vil", 3.63) - 1.089).abs() < 1e-9);
    assert!((at("vil", 2.7) - 0.675).abs() < 1e-9);
    assert!((at("voh", 3.63) - 2.904).abs() < 1e-9, "0.8 VDD");
    assert!((at("vol", 3.63) - 0.726).abs() < 1e-9, "0.2 VDD");
}

// ---------------------------------------------------------------------------
// ATmega2560
// ---------------------------------------------------------------------------

const M2560_IDS: [&str; 4] = [
    "atmega2560_tqfp100",
    "atmega2560_cbga100",
    "atmega2560v_tqfp100",
    "atmega2560v_cbga100",
];

#[test]
fn atmega2560_ordering_codes_resolve_to_their_package_and_grade() {
    // DS40002211A 35.4: 100A is the 100-lead TQFP, 100C1 the 100-ball CBGA; R is tape and reel.
    // The -16 parts are the 4.5 V - 5.5 V grade, the V parts the 1.8 V - 5.5 V grade.
    for (part, id) in [
        ("ATmega2560-16AU", "atmega2560_tqfp100"),
        ("ATmega2560-16AUR", "atmega2560_tqfp100"),
        ("ATMEGA2560-16AU", "atmega2560_tqfp100"),
        ("ATmega2560-16CU", "atmega2560_cbga100"),
        ("ATmega2560-16CUR", "atmega2560_cbga100"),
        ("ATmega2560V-8AU", "atmega2560v_tqfp100"),
        ("ATmega2560V-8AUR", "atmega2560v_tqfp100"),
        ("ATmega2560V-8CU", "atmega2560v_cbga100"),
        ("ATmega2560V-8CUR", "atmega2560v_cbga100"),
    ] {
        let m = mpn_only(part).unwrap_or_else(|| panic!("{part} did not resolve"));
        assert_eq!(m.id, id, "{part}");
        assert_eq!(m.kind, ComponentKind::Mcu, "{part}");
    }
}

#[test]
fn atmega2560_siblings_and_unordered_codes_do_not_resolve_to_these_cards() {
    // ATmega2561 and ATmega1281 are the 64-pin parts, ATmega640 and ATmega1280 share the 100-pin
    // pinout but are other chips; the bare name and a grade that is not in 35.4 name no ordering code.
    for part in [
        "ATmega2560",
        "ATmega2560V",
        "ATmega2561-16AU",
        "ATmega2561V-8AU",
        "ATmega2561-16MU",
        "ATmega1281-16AU",
        "ATmega1280-16AU",
        "ATmega1280V-8AU",
        "ATmega640-16AU",
        "ATmega640V-8CU",
        "ATmega2560-8AU",
        "ATmega2560V-16AU",
        "ATmega2560-16MU",
        "ATmega2560-16PU",
        "ATmega256",
        "ATmega328P-AU",
    ] {
        let got = mpn_only(part).map(|m| m.id);
        assert!(
            got.as_deref().is_none_or(|id| !M2560_IDS.contains(&id)),
            "{part} must not resolve to an ATmega2560 card, got {got:?}"
        );
    }
}

#[test]
fn atmega2560_tqfp_pad_map_matches_figure_1_1() {
    for part in ["ATmega2560-16AU", "ATmega2560V-8AU"] {
        let m = mpn_only(part).expect(part);
        assert_eq!(m.pins.len(), 100, "{part}");
        assert_pins(
            &m,
            &pins(
                "1=pg5_oc0b 2=pe0_rxd0 3=pe1_txd0 10=vcc 11=gnd 12=ph0_rxd2 19=pb0_ss 22=pb3_miso \
                 25=pb6_oc1b 26=pb7_oc0a_oc1c 30=reset 31=vcc2 32=gnd2 33=xtal2 34=xtal1 \
                 35=pl0_icp4 43=pd0_scl_int0 44=pd1_sda_int1 45=pd2_rxd1_int2 50=pd7_t0 \
                 51=pg0_wr 52=pg1_rd 53=pc0_a8 60=pc7_a15 61=vcc3 62=gnd3 63=pj0_rxd3 \
                 70=pg2_ale 71=pa7_ad7 75=pa3_ad3 76=pa2_ad2 78=pa0_ad0 79=pj7 80=vcc4 \
                 81=gnd4 82=pk7_adc15 89=pk0_adc8 90=pf7_adc7_tdi 97=pf0_adc0 98=aref \
                 99=gnd5 100=avcc",
            ),
        );
    }
}

#[test]
fn atmega2560_cbga_pad_map_matches_table_1_1() {
    for part in ["ATmega2560-16CU", "ATmega2560V-8CU"] {
        let m = mpn_only(part).expect(part);
        assert_eq!(m.pins.len(), 100, "{part}");
        assert_pins(
            &m,
            &pins(
                "A1=gnd A2=aref A3=pf0_adc0 A6=pk0_adc8 A9=gnd2 A10=vcc B1=avcc B2=pg5_oc0b \
                 B9=pa0_ad0 C8=pj7 D2=pe4_oc3b_int4 F1=vcc2 F10=gnd3 G1=gnd4 G10=vcc3 \
                 H3=reset J1=ph7_t4 J5=xtal2 J10=pg1_rd K3=vcc4 K4=gnd5 K5=xtal1 K10=pg0_wr",
            ),
        );
    }
}

#[test]
fn atmega2560_cbga_and_tqfp_carry_the_same_pad_functions() {
    // The note under Table 1-1: the function of each ball is that of the 100-pin package.
    let sorted_roles = |part: &str| {
        let mut roles: Vec<String> = mpn_only(part).expect(part).pins.values().cloned().collect();
        roles.sort();
        roles
    };
    assert_eq!(
        sorted_roles("ATmega2560-16AU"),
        sorted_roles("ATmega2560-16CU")
    );
    assert_eq!(
        sorted_roles("ATmega2560V-8AU"),
        sorted_roles("ATmega2560V-8CU")
    );
    assert_eq!(
        sorted_roles("ATmega2560-16AU"),
        sorted_roles("ATmega2560V-8AU")
    );
    let m = mpn_only("ATmega2560-16AU").unwrap();
    let count = |prefix: &str| {
        m.pins
            .values()
            .filter(|r| {
                r.strip_prefix(prefix)
                    .is_some_and(|rest| rest.chars().all(|c| c.is_ascii_digit()))
            })
            .count()
    };
    assert_eq!(count("vcc"), 4, "four VCC pads");
    assert_eq!(count("gnd"), 5, "five GND pads");
}

#[test]
fn atmega2560_adc_pads_are_port_f_and_port_k() {
    for (part, first, last) in [
        ("ATmega2560-16AU", "97", "82"),
        ("ATmega2560V-8AU", "97", "82"),
        ("ATmega2560-16CU", "A3", "B8"),
        ("ATmega2560V-8CU", "A3", "B8"),
    ] {
        let m = mpn_only(part).expect(part);
        let adc = m.pin_capability.adc.as_ref().expect("adc table declared");
        assert_eq!(adc.pads.len(), 16, "{part}: ADC0-ADC15");
        assert_eq!(m.adc_channel(first), Some("ADC0"), "{part}: PF0");
        assert_eq!(m.adc_channel(last), Some("ADC15"), "{part}: PK7");
        assert_eq!(
            param(&m, "max_adc_source_impedance_ohms"),
            10_000.0,
            "26.6.1: about 10 kOhm or less"
        );
    }
    let t = mpn_only("ATmega2560-16AU").unwrap();
    assert_eq!(t.adc_channel("92"), Some("ADC5"), "PF5");
    assert_eq!(t.adc_channel("85"), Some("ADC12"), "PK4");
    assert_eq!(t.adc_channel("98"), None, "AREF is not an ADC input pad");
    assert_eq!(t.adc_channel("100"), None, "AVCC is not an ADC input pad");
}

#[test]
fn atmega2560_reset_strap_and_clock_params_are_the_datasheet_figures() {
    for part in ["ATmega2560-16AU", "ATmega2560V-8CU"] {
        let m = mpn_only(part).expect(part);
        assert_eq!(
            m.straps.len(),
            1,
            "{part}: AVR boot is fuse-selected, only RESET is a strap"
        );
        let rst = strap(&m, "reset");
        assert_eq!(rst.internal_pull, StrapInternalPull::PullUp, "RRST");
        assert_eq!(rst.level, StrapLevel::High);
        assert!(!rst.boot_select);
        // 31.1: VIL2 max 0.1 VCC and VIH2 min 0.9 VCC for the RESET pin.
        assert_eq!(rst.vil, Some([0.1, 0.0]));
        assert_eq!(rst.vih, Some([0.9, 0.0]));
        assert_eq!(
            param(&m, "reset_pull_up_ohms"),
            30_000.0,
            "RRST min 30 kOhm"
        );
        assert_eq!(
            param(&m, "xtal_cap_min_f"),
            12.0e-12,
            "Tables 10-3 and 10-5"
        );
        assert_eq!(param(&m, "xtal_cap_max_f"), 22.0e-12);
        assert_eq!(
            param(&m, "default_clock_hz"),
            1_000_000.0,
            "10.3.1: 8 MHz RC / 8"
        );
        assert_eq!(m.params.get_str("backend"), Some("none:atmega2560"));
    }
}

#[test]
fn atmega2560_ratings_envelope_and_levels_are_the_datasheet_figures() {
    let non_v = mpn_only("ATmega2560-16AU").expect("non-V");
    let v = mpn_only("ATmega2560V-8AU").expect("V");
    for m in [&non_v, &v] {
        assert_eq!(
            m.ratings.max_pin_current_a,
            Some(0.04),
            "DC current per I/O pin"
        );
        assert_eq!(
            m.ratings.max_current_a,
            Some(0.2),
            "DC current VCC and GND pins"
        );
        assert_eq!(
            m.ratings.max_voltage_v,
            Some(6.0),
            "maximum operating voltage"
        );
        assert_eq!(
            m.ratings.max_junction_temp_c, None,
            "no junction temperature is printed"
        );
        assert_eq!(
            m.ratings.theta_ja_c_per_w, None,
            "no thermal resistance table"
        );
    }
    // 35.4: the -16 grade runs 4.5 V - 5.5 V, the V grade 1.8 V - 5.5 V.
    assert!(has_supply_range(
        &non_v,
        "vcc",
        (4.5, 5.5),
        Some(6.0),
        "35.4"
    ));
    assert!(has_supply_range(&v, "vcc", (1.8, 5.5), Some(6.0), "35.4"));
    assert!(has_supply_range(&v, "avcc", (1.8, 5.5), None, "AVCC"));
    // VIH min 0.6 VCC and VIL max 0.3 VCC for VCC 2.4 V - 5.5 V.
    for m in [&non_v, &v] {
        let at = |name: &str, volts: f64| hauksbee_models::schema::level_at(&m.levels[name], volts);
        assert!((at("vih", 5.0) - 3.0).abs() < 1e-9, "{}: 0.6 x 5 V", m.id);
        assert!((at("vil", 5.0) - 1.5).abs() < 1e-9, "{}: 0.3 x 5 V", m.id);
        assert!(
            (at("voh", 5.0) - 4.2).abs() < 1e-9,
            "{}: VOH at -20 mA",
            m.id
        );
        assert!(
            (at("vol", 5.0) - 0.9).abs() < 1e-9,
            "{}: VOL at 20 mA",
            m.id
        );
    }
    // The low-voltage grade also carries the 1.8 V - 2.4 V rows: VIL max 0.2 VCC, VIH min 0.7 VCC.
    let at_v = |name: &str, volts: f64| hauksbee_models::schema::level_at(&v.levels[name], volts);
    assert!((at_v("vil", 1.8) - 0.36).abs() < 1e-9);
    assert!((at_v("vih", 1.8) - 1.26).abs() < 1e-9);
    assert!((at_v("voh", 3.0) - 2.3).abs() < 1e-9, "VOH at -10 mA, 3 V");
}
