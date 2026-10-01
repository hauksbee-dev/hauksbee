//! Pad roles and voltage codes of the ME6211C, NCP1117(LP), XC6206 and
//! single-gate logic cards, against the manufacturer pin tables.

use super::*;

const SOT23_5: &str = "Package_TO_SOT_SMD:SOT-23-5";

/// (value, footprint, id, `pad=role` map, vout). Pin maps: Microne ME6211 V14 p.4
/// (SOT23-5 column); onsemi NCP1117LP/D Rev.6 Table 1 and NCP1117/D Rev.31 (SOT-223,
/// tab on pin 2); Torex ETR0305_003a Pin Assignment (SOT-23 column).
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
