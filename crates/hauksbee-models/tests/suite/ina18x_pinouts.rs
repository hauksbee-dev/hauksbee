//! The `INA18x` current-sense amplifiers: one pin map per real package, and the
//! input pins carry their datasheet voltage rating.
//!
//! The family's pin map moves with the package (INA181 and INA185 put OUT on pin
//! 1 in the SOT-23/SOT-563 and on pin 6 in the SC70; INA180 has two SOT-23-5
//! pinouts chosen by the order code's first letter; the INA186 has no 5-pin
//! package at all). A map read for one package and bound to another binds
//! cleanly and simulates a different circuit, so each row below is the pin
//! table of ONE package, read from the TI datasheet named beside it.
//!
//! Each resolved card must also declare the package its map is for (so the
//! binder can hold it against the footprint), map exactly that package's pins,
//! and rate IN+ and IN- itself.

use hauksbee_models::package::pin_count;
use hauksbee_models::OperatingEnvelope;

use super::{assert_pin_map, expect_query, pins, query_id};

const SC70: &str = "Package_TO_SOT_SMD:SOT-363_SC-70-6";
const SC70_IPC: &str = "SOT65P210X110-6N:SOT65P210X110-6N";
const SOT23_6: &str = "Package_TO_SOT_SMD:SOT-23-6";
const SOT563: &str = "Package_TO_SOT_SMD:SOT-563";
const SOT23_8: &str = "Package_TO_SOT_SMD:SOT-23-8";
const SOT23_5: &str = "Package_TO_SOT_SMD:SOT-23-5";

// Datasheet pin tables.
// INA180 SBOS741H Table 6-1: pinout A (INA180A*) and pinout B (INA180B*).
const INA180_A: &str = "1=out 2=gnd 3=in_plus 4=in_minus 5=vs";
const INA180_B: &str = "1=in_plus 2=gnd 3=in_minus 4=out 5=vs";
// INA181 SBOS793H Table 5-1 (SOT-23 column); INA185 SBOS378A Table 4-1 (SOT-563 column).
const SOT23_6_MAP: &str = "1=out 2=gnd 3=in_plus 4=in_minus 5=ref 6=vs";
// INA181 Table 5-1 and INA185 Table 4-1 (SC70 column).
const SC70_MAP: &str = "1=ref 2=gnd 3=vs 4=in_plus 5=in_minus 6=out";
// INA186 SBOS318B Table 5-1: DCK (SC70-6) and DDF (SOT-23-8, with ENABLE).
const INA186_DCK: &str = "1=ref 2=vss 3=vcc 4=in_plus 5=in_minus 6=out";
const INA186_DDF: &str = "1=vs 2=en 3=ref 4=gnd 5=out 6=nc 7=in_plus 8=in_minus";

/// `(value, footprint, card id, pad=role map, input pin abs max V)`: one row per
/// (order code, footprint) a board can carry.
type Row = (
    &'static str,
    Option<&'static str>,
    &'static str,
    &'static str,
    f64,
);

fn rows() -> Vec<Row> {
    vec![
        ("INA180A1IDBVR", None, "ina180", INA180_A, 26.0),
        ("INA180A3", Some(SOT23_5), "ina180", INA180_A, 26.0),
        ("INA180B2IDBVR", None, "ina180b", INA180_B, 26.0),
        ("INA181A3IDBVR", None, "ina181_dbv", SOT23_6_MAP, 26.0),
        (
            "INA181A1",
            Some(SOT23_6),
            "ina181_dbv_by_footprint",
            SOT23_6_MAP,
            26.0,
        ),
        ("INA181A3IDCKR", None, "ina181_dck", SC70_MAP, 26.0),
        (
            "INA181",
            Some(SC70),
            "ina181_dck_by_footprint",
            SC70_MAP,
            26.0,
        ),
        (
            "INA181A2",
            Some(SC70_IPC),
            "ina181_dck_by_footprint",
            SC70_MAP,
            26.0,
        ),
        ("INA185A2IDRLR", None, "ina185_drl", SOT23_6_MAP, 26.0),
        (
            "INA185A4",
            Some(SOT563),
            "ina185_drl_by_footprint",
            SOT23_6_MAP,
            26.0,
        ),
        ("INA185A1IDCKR", None, "ina185_dck", SC70_MAP, 26.0),
        (
            "INA185A3",
            Some(SC70),
            "ina185_dck_by_footprint",
            SC70_MAP,
            26.0,
        ),
        ("INA186A3IDCKR", None, "ina186_dck_a3", INA186_DCK, 42.0),
        ("INA186A5", Some(SC70), "ina186_dck_a5", INA186_DCK, 42.0),
        (
            "INA186",
            Some(SC70),
            "ina186_dck_a1_family",
            INA186_DCK,
            42.0,
        ),
        (
            "INA186",
            Some(SOT23_8),
            "ina186_ddf_by_footprint",
            INA186_DDF,
            42.0,
        ),
        ("INA186A2IDDFR", None, "ina186_ddf", INA186_DDF, 42.0),
    ]
}

/// The abs-max (or, lacking one, max) of the `supply_range` declared on `role`.
fn pin_ceiling(m: &hauksbee_models::ModelEntry, role: &str) -> Option<(f64, String)> {
    m.envelope.iter().find_map(|e| match e {
        OperatingEnvelope::SupplyRange {
            pin,
            max_v,
            abs_max_v,
            basis,
            ..
        } if pin == role => Some((abs_max_v.unwrap_or(*max_v), basis.clone())),
        _ => None,
    })
}

#[test]
fn every_row_resolves_to_its_packages_pin_table() {
    for (value, footprint, id, map, _) in rows() {
        let model = expect_query(Some(value), None, footprint);
        assert_pin_map(&model, id, &pins(map));
    }
}

#[test]
fn every_card_declares_its_package_and_rates_both_inputs() {
    for (value, footprint, id, _, ceiling) in rows() {
        let model = expect_query(Some(value), None, footprint);
        let package = model
            .pin_package
            .as_deref()
            .unwrap_or_else(|| panic!("{id}: the card declares no package for its pin map"));
        // A map for a package with fewer or more pins than it maps was read for another package.
        assert_eq!(
            pin_count(package).map(|n| n as usize),
            Some(model.pins.len()),
            "{id}: {package} does not have the {} pins the card maps",
            model.pins.len()
        );
        // The input pins are rated against GND by the datasheet, so each carries its own
        // cited ceiling (the family's, not the 6 V supply's).
        for role in ["in_plus", "in_minus"] {
            let (got, basis) = pin_ceiling(&model, role)
                .unwrap_or_else(|| panic!("{id}: {role} declares no input pin rating"));
            assert!(
                (got - ceiling).abs() < f64::EPSILON,
                "{id}: {role} ceiling {got} V, datasheet says {ceiling} V"
            );
            assert!(
                basis.contains("SBOS"),
                "{id}: {role} rating cites no datasheet: {basis}"
            );
        }
    }
}

#[test]
fn the_dual_rates_both_channels_inputs() {
    // INA2181 SBOS793H Table 5-2 (VSSOP-10 DGS / WSON-10 DSQ).
    let model = expect_query(Some("INA2181A1IDGSR"), None, None);
    assert_pin_map(
        &model,
        "ina2181",
        &pins("1=out_a 2=in_minus_a 3=in_plus_a 4=gnd 5=ref_a 6=ref_b 7=in_plus_b 8=in_minus_b 9=out_b 10=vs"),
    );
    let package = model.pin_package.as_deref().expect("package");
    assert_eq!(pin_count(package), Some(10), "{package}");
    for role in ["in_plus_a", "in_minus_a", "in_plus_b", "in_minus_b"] {
        let (got, basis) = pin_ceiling(&model, role).unwrap_or_else(|| panic!("{role} unrated"));
        assert!((got - 26.0).abs() < f64::EPSILON, "{role}: {got}");
        assert!(basis.contains("SBOS793H"), "{role}: {basis}");
    }
}

#[test]
fn a_value_that_does_not_fix_the_pinout_binds_no_card() {
    // Guessing is the failure this file exists to stop. Each of these names a part
    // whose pin map the value and footprint together leave open, or a package the
    // part does not come in.
    for (value, footprint) in [
        // INA180 has pinouts A and B and the bare value names neither.
        ("INA180", None),
        ("INA180", Some(SOT23_5)),
        // INA181/INA185 bare with no footprint: the SOT-23/SOT-563 and SC70 maps differ.
        ("INA181", None),
        ("INA185A1", None),
        // The INA181 and INA186 have no 5-pin package.
        ("INA181A1", Some(SOT23_5)),
        ("INA186", Some(SOT23_5)),
    ] {
        assert_eq!(
            query_id(Some(value), None, footprint),
            None,
            "{value} on {footprint:?} must not bind a card"
        );
    }
}

#[test]
fn an_order_code_never_binds_another_packages_map() {
    // The order code names the package. A DDF code on an SC70 footprint still gets the
    // 8-pin DDF map (never the 6-pin DCK map), so the binder's package-fit check sees
    // the 8-pin card against the 6-pin footprint and reports the misfit.
    let id = |value, footprint| query_id(Some(value), None, Some(footprint));
    assert_eq!(id("INA186A1IDDFR", SC70).as_deref(), Some("ina186_ddf"));
    assert_eq!(
        id("INA186A1IDCKR", SOT23_8).as_deref(),
        Some("ina186_dck_a1")
    );
    assert_eq!(id("INA181A1IDBVR", SC70).as_deref(), Some("ina181_dbv"));
    assert_eq!(id("INA181A1IDCKR", SOT23_6).as_deref(), Some("ina181_dck"));
}
