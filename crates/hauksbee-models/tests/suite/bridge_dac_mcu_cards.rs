//! Order codes and pad maps of the CH340C, DAC081C081, DAC43204 and
//! CY7C68013A cards, against the manufacturer pin tables, with the
//! neighbouring packages and family members that must not bind.

use super::*;

/// WCH CH340 datasheet V3D Section 4, SOP16 column: the CH340C has NC and
/// OUT# where the CH340G has its crystal pins.
#[test]
fn ch340c_takes_the_sop16_map_with_nc_and_out() {
    let m = resolve_part("CH340C").expect("CH340C resolves");
    assert_pin_map(
        &m,
        "ch340c",
        &pins(
            "1=gnd 2=txd 3=rxd 4=v3 5=ud_p 6=ud_n 7=nc 8=out_n 9=cts_n 10=dsr_n \
             11=ri_n 12=dcd_n 13=dtr_n 14=rts_n 15=r232 16=vcc",
        ),
    );
    assert_eq!(
        query_id(Some("CH340G"), Some("CH340G"), None).as_deref(),
        Some("ch340g")
    );
    for other in ["CH340N", "CH340K", "CH340E", "CH340B", "CH340X"] {
        assert_eq!(query_id(Some(other), Some(other), None), None, "{other}");
    }
}

/// TI SNAS449F pin table, SOT (DDC) column. The WSON order code and the
/// DAC081C085 VSSOP order their pins differently and must not bind.
#[test]
fn dac081c081_binds_only_the_sot_order_code() {
    for value in [
        "DAC081C081CIMK",
        "DAC081C081CIMK/NOPB",
        "DAC081C081CIMKX/NOPB",
    ] {
        let m = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve"));
        assert_pin_map(
            &m,
            "dac081c081cimk",
            &pins("1=vout 2=va 3=gnd 4=sda 5=scl 6=adr0"),
        );
    }
    for other in [
        "DAC081C081CISD",
        "DAC081C081",
        "DAC081C085CIMM",
        "DAC101C081CIMK",
    ] {
        assert_eq!(query_id(Some(other), Some(other), None), None, "{other}");
    }
}

/// TI SLASEX3A Table 5-1, RTE WQFN-16; the 10- and 12-bit siblings are other
/// parts and must not take this card.
#[test]
fn dac43204_takes_the_rte_map() {
    for value in ["DAC43204", "DAC43204RTER", "DAC43204RTET"] {
        let m = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve"));
        assert_pin_map(
            &m,
            "dac43204",
            &pins(
                "1=fb3 2=out3 3=out2 4=fb2 5=gpio 6=scl 7=a0 8=sda 9=fb1 10=out1 11=out0 \
                 12=fb0 13=cap 14=agnd 15=vdd 16=vref 17=thermal_pad",
            ),
        );
    }
    for other in ["DAC53204", "DAC63204RTER", "DAC43204W"] {
        assert_eq!(query_id(Some(other), Some(other), None), None, "{other}");
    }
}
