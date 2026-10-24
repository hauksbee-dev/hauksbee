//! Pad roles and voltage codes of the ME6211C, NCP1117(LP), XC6206 and
//! single-gate logic cards, against the manufacturer pin tables.

use super::*;
use hauksbee_models::behavioral::Topology;

const SOT23_5: &str = "Package_TO_SOT_SMD:SOT-23-5";

/// (value, footprint, id, `pad=role` map, vout). Pin maps: Microne ME6211 V14 p.4
/// (SOT23-5 column); onsemi NCP1117LP/D Rev.6 Table 1 and NCP1117/D Rev.31 (SOT-223,
/// tab on pin 2); Torex `ETR0305_003a` Pin Assignment (SOT-23 column).
#[test]
fn ldo_pad_roles_and_voltage_codes_follow_the_datasheets() {
    const ME: &str = "1=in 2=gnd 3=en 4=nc 5=out";
    const SOT223: &str = "1=gnd 2=out 3=in";
    const XC: &str = "1=gnd 2=out 3=in";
    let rows = [
        ("ME6211C12M5G-N", Some(SOT23_5), "me6211c12", ME, 1.2),
        ("ME6211C18M5G-N", Some(SOT23_5), "me6211c18", ME, 1.8),
        ("ME6211C25M5G-N", Some(SOT23_5), "me6211c25", ME, 2.5),
        ("ME6211C28M5G-N", Some(SOT23_5), "me6211c28", ME, 2.8),
        ("ME6211C30M5G-N", Some(SOT23_5), "me6211c30", ME, 3.0),
        ("ME6211C33M5G-N", Some(SOT23_5), "me6211c33", ME, 3.3),
        ("ME6211C33", Some(SOT23_5), "me6211c33", ME, 3.3),
        ("ME6211C50M5G-N", Some(SOT23_5), "me6211c50", ME, 5.0),
        ("NCP1117LPST15T3G", None, "ncp1117lp_15", SOT223, 1.5),
        ("NCP1117LPST18T3G", None, "ncp1117lp_18", SOT223, 1.8),
        ("NCP1117LPST25T3G", None, "ncp1117lp_25", SOT223, 2.5),
        ("NCP1117LPST33T3G", None, "ncp1117lp_33", SOT223, 3.3),
        ("NCP1117LPST50T3G", None, "ncp1117lp_50", SOT223, 5.0),
        ("NCP1117ST18T3G", None, "ncp1117_18", SOT223, 1.8),
        ("NCP1117ST25T3G", None, "ncp1117_25", SOT223, 2.5),
        ("NCP1117ST33T3G", None, "ncp1117_3v3_sot223", SOT223, 3.3),
        ("NCV1117ST33T3G", None, "ncp1117_3v3_sot223", SOT223, 3.3),
        ("XC6206P182MR-G", None, "xc6206_1v8", XC, 1.8),
        ("XC6206P252MR", None, "xc6206_2v5", XC, 2.5),
        ("XC6206P281MR", None, "xc6206_2v8", XC, 2.8),
        ("XC6206P302MR-G", None, "xc6206_3v0", XC, 3.0),
        ("XC6206P502MR", None, "xc6206_5v0", XC, 5.0),
    ];
    for (value, footprint, id, map, vout) in rows {
        let m = expect_query(Some(value), Some(value), footprint);
        assert_eq!(m.id, id, "{value} bound the wrong card");
        let expected = pins(map);
        assert_pin_map(&m, id, &expected);
        assert!((param(&m, "vout") - vout).abs() < 1e-9, "{value}: vout");
    }
}

/// A package or family the datasheet pin table does not cover must not bind: the
/// DFN2x2-6L and SOT-89/USP-6B maps differ, ME6211A/H are different parts, and the
/// ADJ members carry a 1.25 V reference, never an output voltage.
#[test]
fn ldo_cards_do_not_bind_other_packages_or_adjustable_members() {
    let none = [
        (
            "ME6211C33M5G-N",
            Some("Package_DFN_QFN:DFN-6_2x2mm_P0.65mm"),
        ),
        ("ME6211C33", None),
        ("ME6211A33M3G", Some("Package_TO_SOT_SMD:SOT-23")),
        ("ME6211H33M5G-N", Some(SOT23_5)),
        ("XC6206P332PR", None),
        ("XC6206P502DR", None),
        ("XC6206P122MR", None),
        ("NCP1117LPSTADJT3G", None),
        ("NCP1117STAT3G", None),
        ("NCP1117LPDT33RKG", None),
    ];
    for (value, footprint) in none {
        assert_eq!(
            query_id(Some(value), Some(value), footprint),
            None,
            "{value}"
        );
    }
}

/// (value, id, `pad=role` map). TI SCES214AF / SCES217AA / SCES218AA / SCES351Y /
/// SCLS318V / SCLS314R / SCES571K and the matching Nexperia Table 3 five-lead
/// column: inverting and buffer parts leave pad 1 NC, the AND gates put A and B on
/// pads 1 and 2 (Nexperia prints them swapped, which a symmetric AND cannot see).
#[test]
fn single_gate_logic_pad_roles_follow_the_five_lead_tables() {
    const NOT: &str = "1=nc 2=a 3=gnd 4=y 5=vcc";
    const AND: &str = "1=a 2=b 3=gnd 4=y 5=vcc";
    let rows = [
        ("SN74LVC1G04DBVR", "74lvc1g04", NOT),
        ("74LVC1G04GW,125", "74lvc1g04", NOT),
        ("74LVC1G08DCKR", "74lvc1g08", AND),
        ("SN74LVC1G14DBVR", "74lvc1g14", NOT),
        ("74LVC1G14GV", "74lvc1g14", NOT),
        ("SN74LVC1G17DCKR", "74lvc1g17", NOT),
        ("SN74AHC1G04DBVR", "74ahc1g04", NOT),
        ("SN74AHC1G08DBVR", "74ahc1g08", AND),
        ("SN74AUP1G04DRLR", "74aup1g04", NOT),
    ];
    for (value, id, map) in rows {
        let m = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve"));
        assert_pin_map(&m, id, &pins(map));
        assert!(
            param(&m, "vih") > param(&m, "vil"),
            "{value}: vih must exceed vil"
        );
    }
    // Values reach the regex with commas as points, so reel suffixes like ",125" must match.
    for (value, id) in [
        ("74LVC1G125GW,125", "74lvc1g125"),
        ("74LVC125AD,118", "74lvc125a"),
    ] {
        let m = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve"));
        assert_eq!(m.id, id, "{value}");
    }
    // Nexperia 74HC_HCT1G125 Rev. 8 Table 3: OE on pad 1; TTL thresholds (Table 8).
    // TI SCLS378P Table 4-1 gives the DBV (SOT-23-5) package the same five pads.
    for value in [
        "74HCT1G125GW,125",
        "74HCT1G125GV",
        "74HCT1G125DBV",
        "74HCT1G125DBVR",
    ] {
        let m = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve"));
        assert_pin_map(&m, "74hct1g125", &pins("1=oe_n 2=a 3=gnd 4=y 5=vcc"));
        assert!((param(&m, "vih") - 2.0).abs() < 1e-9, "{value}: vih");
        assert!((param(&m, "vil") - 0.8).abs() < 1e-9, "{value}: vil");
    }
}

/// TI SCLS069G (SN74HCT125, pin figure p.3), SCLS264R (SN74AHCT125, Table 4-1 p.4)
/// and SCLS378P (SN74AHCT1G125, Table 4-1 p.3): the buffer pad maps and the TTL
/// thresholds (5.2 / 5.3: VIH 2 V, VIL 0.8 V), whichever package the value names.
#[test]
fn hct_and_ahct_bus_buffers_resolve_with_their_pad_maps() {
    const QUAD: &str = "1=oe_n_1 2=a1 3=y1 4=oe_n_2 5=a2 6=y2 7=gnd 8=y3 9=a3 10=oe_n_3 11=y4 12=a4 13=oe_n_4 14=vcc";
    const SINGLE: &str = "1=oe_n 2=a 3=gnd 4=y 5=vcc";
    let rows = [
        ("SN74HCT125DR", "74hct125", QUAD),
        ("SN74HCT125N", "74hct125", QUAD),
        ("74HCT125PW,118", "74hct125", QUAD),
        ("SN74AHCT125PWR", "74ahct125", QUAD),
        ("SN74AHCT125RGYR", "74ahct125", QUAD),
        ("74AHCT125D", "74ahct125", QUAD),
        ("SN74AHCT1G125DBVR", "74ahct1g125", SINGLE),
        ("SN74AHCT1G125DCKR", "74ahct1g125", SINGLE),
        ("74AHCT1G125GW,125", "74ahct1g125", SINGLE),
    ];
    for (value, id, map) in rows {
        let m = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve"));
        assert_pin_map(&m, id, &pins(map));
        assert!((param(&m, "vih") - 2.0).abs() < 1e-9, "{value}: vih");
        assert!((param(&m, "vil") - 0.8).abs() < 1e-9, "{value}: vil");
        let supply = m.envelope.iter().find_map(|e| match e {
            hauksbee_models::OperatingEnvelope::SupplyRange { min_v, max_v, .. } => {
                Some((*min_v, *max_v))
            }
            _ => None,
        });
        assert_eq!(supply, Some((4.5, 5.5)), "{value}: supply window");
    }
}

/// The HCT input has no rating above its supply; the AHCT input is rated to 5.5 V
/// at any supply (TI 5.2 / 5.3 VI), which is the difference the 3.3 V case turns on.
#[test]
fn only_the_ahct_buffers_rate_their_inputs_above_the_supply() {
    let ceiling = |value: &str, role: &str| {
        let m = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve"));
        hauksbee_models::declared_pin_ceiling_v(&m.envelope, role)
    };
    assert_eq!(ceiling("SN74HCT125D", "a1"), None);
    assert_eq!(ceiling("74HCT1G125GW", "a"), None);
    assert_eq!(ceiling("SN74AHCT125D", "a1"), Some(7.0));
    assert_eq!(ceiling("SN74AHCT1G125DBVR", "oe_n"), Some(7.0));
}

/// Six-pad XSON/SON and four-pad X2SON parts move VCC off pad 5, so they must not
/// bind the five-lead map; neither may a neighbouring family or function.
#[test]
fn single_gate_logic_does_not_bind_other_packages_or_families() {
    let none = [
        "74LVC1G04GM",
        "74LVC1G04GX4",
        "SN74LVC1G04DRYR",
        "SN74LVC1G08DSFR",
        "74LVC1G14GN",
        "SN74AHC1G04DRYR",
        "74AHCT1G04GW",
        "SN74AUP1G04DSFR",
        "74LVC1G18GW",
        "74HC1G125GW",
        "74HC1G125DBV",
        "74HCT1G125GM",
        "74HCT1G125DCK",
        "74LVC1G00GW",
    ];
    for value in none {
        assert_eq!(query_id(Some(value), Some(value), None), None, "{value}");
    }
}

/// (value, footprint, id, `pad=role` map). TI SCES515N Table 4-1 and Nexperia
/// 74AXP1T45 Rev. 2.1 Table 3 (six-lead); TI SCES205N Table 5 (2G126, 1Y on pad 6);
/// TI SNOSDB1E Table 5-1 (TLV3601), SBOS779D Pin Functions (TLV6001 DBV and DCK
/// columns differ); Micrel M9999-072908-A p.2; TI SBVS034P Table 4-1; TI SNVS124G
/// Table 6-1; TI SLUSEA4D Table 7-1; TI SLVSGK6B Table 4-1.
#[test]
fn translator_gate_opamp_and_regulator_pad_roles_follow_the_datasheets() {
    const T45: &str = "1=vcca 2=gnd 3=a1 4=b1 5=dir 6=vccb";
    const OPAMP_DBV: &str = "1=out 2=vss 3=in_plus 4=in_minus 5=vcc";
    const OPAMP_DCK: &str = "1=in_plus 2=vss 3=in_minus 4=out 5=vcc";
    const MIC: &str = "1=in 2=gnd 3=en 4=nc 5=out";
    let sc70 = Some("Package_TO_SOT_SMD:SC-70-5");
    let rows = [
        ("SN74LVC1T45DBVR", None, "sn74lvc1t45", T45),
        ("SN74LVC1T45DCKR", None, "sn74lvc1t45", T45),
        ("74AXP1T45GW,125", None, "74axp1t45", T45),
        (
            "SN74LVC2G126DCUR",
            None,
            "sn74lvc2g126",
            "1=oe_1 2=a1 3=y2 4=gnd 5=a2 6=y1 7=oe_2 8=vcc",
        ),
        ("TLV3601IDBVR", None, "tlv3601", OPAMP_DBV),
        ("TLV6001IDBVR", Some(SOT23_5), "tlv6001_dbv", OPAMP_DBV),
        ("TLV6001", Some(SOT23_5), "tlv6001_dbv", OPAMP_DBV),
        ("TLV6001IDCKR", sc70, "tlv6001_dck", OPAMP_DCK),
        ("MIC5225-3.3YM5", None, "mic5225_3v3", MIC),
        ("MIC5225-5.0YM5-TR", None, "mic5225_5v0", MIC),
        (
            "TPS73101DBVR",
            None,
            "tps73101_dbv",
            "1=in 2=gnd 3=en 4=fb 5=out",
        ),
        (
            "LM2596S-5.0",
            None,
            "lm2596_5v0",
            "1=in 2=sw 3=gnd 4=fb 5=on_off",
        ),
        (
            "TPS62933DRLR",
            None,
            "tps62933_drl",
            "1=rt 2=en 3=in 4=gnd 5=sw 6=bst 7=ss 8=fb",
        ),
        (
            "TLV61070ADBVR",
            None,
            "tlv61070a",
            "1=sw 2=gnd 3=en 4=fb 5=out 6=in",
        ),
    ];
    for (value, footprint, id, map) in rows {
        let m = expect_query(Some(value), Some(value), footprint);
        assert_pin_map(&m, id, &pins(map));
    }
    // The family card that already covers AP2112K-3.3 still resolves on the five-lead map.
    let ap = expect_query(Some("AP2112K-3.3TRG1"), None, None);
    assert_pin_map(&ap, "ap2112k_3v3", &pins(MIC));
}

/// Translator levels follow the datasheet bands, the fixed LDOs carry their voltage
/// code, and adjustable or switching parts carry the reference and loop the datasheet
/// states instead of an output voltage.
#[test]
fn new_cards_carry_the_datasheet_electricals() {
    let tr = expect_query(Some("SN74LVC1T45DBVR"), None, None);
    assert!(!tr.levels.is_empty() && !tr.level_rails.is_empty());
    for (value, vout) in [("MIC5225-3.3YM5", 3.3), ("MIC5225-5.0YM5", 5.0)] {
        assert!((param(&expect_query(Some(value), None, None), "vout") - vout).abs() < 1e-9);
    }
    let adj = expect_query(Some("TPS73101DBVR"), None, None);
    assert!((param(&adj, "vref") - 1.204).abs() < 1e-9);
    assert!(
        adj.params.get_f64("vout").is_none(),
        "an adjustable LDO has no vout"
    );
    for (value, topology, vref, enable) in [
        ("TPS62933DRLR", Topology::Buck, 0.8, Some(1.28)),
        ("LM2596S-5.0", Topology::Buck, 5.0, None),
        ("TLV61070ADBVR", Topology::Boost, 0.5, Some(1.2)),
    ] {
        let c = expect_query(Some(value), None, None)
            .behavioral
            .converter
            .unwrap_or_else(|| panic!("{value} has no converter block"));
        assert_eq!(c.topology, topology, "{value}");
        assert!(
            (c.feedback.expect("feedback").vref_v - vref).abs() < 1e-9,
            "{value}"
        );
        assert_eq!(c.enable.map(|e| e.high_threshold_v), enable, "{value}");
    }
}

/// Other packages, other family members and other voltage codes must not bind:
/// the DSBGA translators and Nexperia's Schmitt-input 2G126, the TLV6001R and
/// TLV6001U reversed or single-sourced variants, the adjustable MIC5225, the
/// fixed `TPS731xx`, the 2 A and PG variants of the TPS62933.
#[test]
fn new_cards_do_not_bind_other_packages_or_family_members() {
    let none = [
        ("SN74LVC1T45YZPR", None),
        ("SN74LVC2T45DCUR", None),
        ("74LVC2G126GT", None),
        ("SN74LVC2G126YZPR", None),
        ("TLV6001RIDBVR", None),
        ("TLV6001UIDCKR", None),
        ("TLV6001IDBVR", Some("Package_TO_SOT_SMD:SC-70-5")),
        ("TLV6001IDCKR", Some(SOT23_5)),
        ("MIC5225YM5", None),
        ("MIC5225-2.8YM5", None),
        ("TPS73125DBVR", None),
        ("TPS62932DRLR", None),
        ("TPS62933PDRLR", None),
        ("TPS62933FDRLR", None),
        ("TLV61070DBVR", None),
    ];
    for (value, footprint) in none {
        assert_eq!(
            query_id(Some(value), Some(value), footprint),
            None,
            "{value}"
        );
    }
}

/// Buck and SPI NOR pad maps against the pin tables: TI SLVSD89B (TLV62568 SOT23-5),
/// Silergy `AN_SY8089` (SOT23-5), TI SNVSA24/SNVSAC1 (LMR16006 SOT-23-6), TI SNVS077J
/// (LM2677 TO-263-7), Winbond `W25Q32JV` 3.3 / `W25Q80DV` 3.4, `GigaDevice` `GD25Q16C`.
/// An adjustable buck regulates FB at its reference; a fixed option senses VOUT on FB.
#[test]
fn buck_and_spi_nor_cards_follow_the_datasheets() {
    const LMR: &str = "1=cb 2=gnd 3=fb 4=shdn 5=in 6=sw";
    const LM2677: &str = "1=sw 2=in 3=cb 4=gnd 5=sync 6=fb 7=on_off";
    const NOR: &str = "1=cs 2=miso 3=wp_n 4=gnd 5=mosi 6=sck 7=hold_n 8=vcc";
    let rows = [
        (
            "TLV62568DBVR",
            "tlv62568_dbv",
            "1=en 2=gnd 3=sw 4=in 5=fb",
            0.6,
            Some(1.2),
        ),
        (
            "SY8089AAAC",
            "sy8089_sot23_5",
            "1=en 2=gnd 3=sw 4=in 5=fb",
            0.6,
            Some(1.5),
        ),
        ("LMR16006XDDCR", "lmr16006x_adj", LMR, 0.765, None),
        ("LMR16006YDDCR", "lmr16006y_adj", LMR, 0.765, None),
        ("LMR16006YQDDCRQ1", "lmr16006y_adj", LMR, 0.765, None),
        ("LM2677SX-3.3/NOPB", "lm2677_3v3_to263", LM2677, 3.3, None),
        ("LM2677S-5.0", "lm2677_5v0_to263", LM2677, 5.0, None),
        ("LM2677SX-5", "lm2677_5v0_to263", LM2677, 5.0, None),
    ];
    for (value, id, map, vref, enable) in rows {
        let m = expect_query(Some(value), Some(value), None);
        assert_pin_map(&m, id, &pins(map));
        let c = m.behavioral.converter.expect("converter block");
        assert_eq!(c.topology, Topology::Buck, "{value}");
        assert!(
            (c.feedback.expect("feedback").vref_v - vref).abs() < 1e-9,
            "{value}"
        );
        assert_eq!(c.enable.map(|e| e.high_threshold_v), enable, "{value}");
    }
    for (value, id) in [
        ("W25Q32JVSSIQ", "w25q32jv_soic8"),
        ("W25Q32JVSS", "w25q32jv_soic8"),
        ("W25Q80DVSSIG", "w25q80dv_soic8"),
        ("W25Q80DVS", "w25q80dv_soic8"),
        ("GD25Q16CSIG", "gd25q16_soic8"),
        ("GD25Q16", "gd25q16_soic8"),
    ] {
        assert_pin_map(
            &expect_query(Some(value), Some(value), None),
            id,
            &pins(NOR),
        );
    }
    // Other packages, grades and output options stay unbound.
    for value in [
        "TLV62568DRLR",
        "TLV62568",
        "LM2677SD-3.3",
        "LM2677S-ADJ",
        // The fixed LMR16006 options are left unresolved, not caught by the
        // adjustable card (vreg.toml says why).
        "LMR16006YQ3DDCRQ1",
        "LMR16006YQ5",
        "W25Q32JVSFIQ",
        "W25Q32JVSSIM",
        "GD25LQ16",
    ] {
        assert_eq!(query_id(Some(value), Some(value), None), None, "{value}");
    }
}
