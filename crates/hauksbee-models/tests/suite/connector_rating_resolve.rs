//! Footprint-bound connector ratings used by the TPS25982 budget decoder.

use super::{expect_query, resolve_query};
use hauksbee_models::ComponentKind;

#[test]
fn exact_jst_vh_and_ph_footprints_resolve_their_datasheet_current_ratings() {
    let cases = [
        (
            "JST_B2P-VH",
            "Connector_JST:JST_VH_B2P-VH_1x02_P3.96mm_Vertical",
            "jst_vh_b2p",
            10.0,
        ),
        (
            "JST_B2B-PH-SM4-TB",
            "Footprints:JST_B2B-PH-SM4-TB",
            "jst_ph_b2b_sm4_tb",
            2.0,
        ),
    ];

    for (value, footprint, id, current_a) in cases {
        let model = expect_query(Some(value), None, Some(footprint));
        assert_eq!(model.id, id);
        assert_eq!(model.kind, ComponentKind::Connector);
        assert_eq!(model.ratings.max_current_a, Some(current_a));
    }
}

#[test]
fn family_name_without_the_matching_footprint_does_not_claim_a_rating() {
    let resolution = resolve_query(
        Some("JST_B2P-VH"),
        None,
        Some("Connector_Generic:Conn_01x02"),
    );
    assert!(
        resolution
            .model
            .as_ref()
            .and_then(|model| model.ratings.max_current_a)
            .is_none(),
        "a value string alone must not become a 10 A witness"
    );
}
