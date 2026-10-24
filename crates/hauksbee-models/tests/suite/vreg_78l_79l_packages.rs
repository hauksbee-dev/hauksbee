//! The 100 mA 78Lxx / 79Lxx regulators are not small 78xx / 79xx parts.
//!
//! A 78L05 in TO-92 is pad 1 = OUT, 2 = GND, 3 = IN (TI UA78L SLVS010X p.3),
//! the reverse of a TO-220 7805 (1 = IN, 2 = GND, 3 = OUT). The 78xx cards used
//! to bind 78L05 too, so a TO-92 78L05 came out with input and output swapped.

use super::{assert_pins, expect_query, pins, query_id};

const TO92: &str = "Package_TO_SOT_THT:TO-92_Inline";
const SOT89: &str = "Package_TO_SOT_SMD:SOT-89-3";
const SOIC8: &str = "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm";
const TO220: &str = "Package_TO_SOT_THT:TO-220-3_Vertical";

#[test]
fn a_78l05_in_to92_has_its_output_on_pad_1() {
    let m = expect_query(Some("78L05"), None, Some(TO92));
    assert_eq!(m.id, "ua78l05_3pin");
    assert_pins(&m, &pins("1=out 2=gnd 3=in"));
    assert_eq!(m.params.get_f64("vout"), Some(5.0));
    assert_eq!(m.ratings.max_current_a, Some(0.1));
}

#[test]
fn a_7805_in_to220_keeps_the_78xx_map() {
    let m = expect_query(Some("7805"), None, Some(TO220));
    assert_eq!(m.id, "7805");
    assert_pins(&m, &pins("1=in 2=gnd 3=out"));
    assert_eq!(m.ratings.max_current_a, Some(1.5));
}

#[test]
fn the_78l_pad_order_holds_in_sot89_and_soic8() {
    let sot = expect_query(Some("UA78L05ACPK"), None, Some(SOT89));
    assert_eq!(sot.id, "ua78l05_3pin");
    assert_pins(&sot, &pins("1=out 2=gnd 3=in"));

    let soic = expect_query(Some("UA78L05ACD"), None, Some(SOIC8));
    assert_eq!(soic.id, "ua78l05_soic8");
    assert_pins(&soic, &pins("1=out 2=gnd 3=gnd 4=nc 5=nc 6=gnd 7=gnd 8=in"));
}

#[test]
fn every_78l_code_resolves_to_its_own_voltage_in_each_package() {
    for (value, vout) in [
        ("78L05", 5.0),
        ("LM78L05", 5.0),
        ("L78L05ACZ", 5.0),
        ("MC78L06A", 6.2),
        ("78L08", 8.0),
        ("UA78L09", 9.0),
        ("78L10", 10.0),
        ("MC78L12ACP", 12.0),
        ("78L15", 15.0),
    ] {
        for footprint in [TO92, SOT89, SOIC8] {
            let m = expect_query(Some(value), None, Some(footprint));
            assert!(
                m.id.starts_with("ua78l"),
                "{value} on {footprint} resolved to {}",
                m.id
            );
            assert_eq!(m.params.get_f64("vout"), Some(vout), "{value} {footprint}");
            assert_eq!(m.ratings.max_current_a, Some(0.1), "{value} {footprint}");
            assert_eq!(m.pins.values().filter(|r| *r == "in").count(), 1);
            assert_eq!(m.pins.values().filter(|r| *r == "out").count(), 1);
        }
    }
}

#[test]
fn a_78l_with_no_known_package_keeps_the_electrical_card_and_no_pad_map() {
    // No footprint, or one the sheet does not print: the part is still the
    // 100 mA 78L05, but no pad order is guessed for it.
    for footprint in [None, Some("Package_TO_SOT_SMD:SOT-23")] {
        let m = expect_query(Some("78L05"), None, footprint);
        assert_eq!(m.id, "ua78l05", "footprint {footprint:?}");
        assert!(m.pins.is_empty(), "footprint {footprint:?} got a pad map");
        assert_eq!(m.ratings.max_current_a, Some(0.1));
    }
}

#[test]
fn the_78xx_cards_no_longer_claim_the_l_class_but_keep_m_and_s() {
    assert_eq!(
        query_id(Some("78L05"), None, None).as_deref(),
        Some("ua78l05")
    );
    for (value, id) in [("78M05", "7805"), ("UA78M05", "7805"), ("78M12", "7812")] {
        assert_eq!(
            query_id(Some(value), None, Some(TO220)).as_deref(),
            Some(id)
        );
    }
}

#[test]
fn a_79l_has_its_own_100ma_cards_and_is_not_the_79xx() {
    let to92 = expect_query(Some("79L05"), None, Some(TO92));
    assert_eq!(to92.id, "lm79l05_to92");
    assert_pins(&to92, &pins("1=gnd 2=in 3=out"));
    assert_eq!(to92.params.get_f64("vout"), Some(-5.0));
    assert_eq!(to92.ratings.max_current_a, Some(0.1));

    let soic = expect_query(Some("LM79L12ACM"), None, Some(SOIC8));
    assert_eq!(soic.id, "lm79l12_soic8");
    assert_pins(&soic, &pins("1=out 2=in 3=in 4=nc 5=gnd 6=in 7=in 8=nc"));

    // The 1.5 A 79xx in TO-220 is unchanged.
    let neg = expect_query(Some("7905"), None, Some(TO220));
    assert_eq!(neg.id, "7905");
    assert_pins(&neg, &pins("1=gnd 2=in 3=out"));
}
