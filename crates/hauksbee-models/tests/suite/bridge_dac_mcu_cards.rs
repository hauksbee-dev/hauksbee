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
