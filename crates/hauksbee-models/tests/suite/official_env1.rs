//! Official-parts batch env1: resolution and behaviour tests for the cards added in that batch.

use super::{assert_pin_map, expect_query, has_supply_range, param, pins, resolve_part};
use hauksbee_models::{PeripheralSpec, SensorSpec};

fn id_of(value: &str) -> Option<String> {
    resolve_part(value).map(|m| m.id)
}

fn assert_not_in(value: &str, family: &[&str]) {
    if let Some(id) = id_of(value) {
        assert!(
            !family.contains(&id.as_str()),
            "{value} must not resolve into {family:?}, got {id}"
        );
    }
}

fn register_map_address(value: &str) -> u8 {
    let m = expect_query(Some(value), Some(value), None);
    match m.peripheral {
        Some(PeripheralSpec::RegisterMap { spec_toml, .. }) => {
            let spec = SensorSpec::from_toml(&spec_toml).expect("embedded spec parses");
            spec.sensor.i2c_address.expect("I2C address")
        }
        other => panic!("{value}: no register-map peripheral: {other:?}"),
    }
}

// ---- SHT40 / SHT41 / SHT45 -------------------------------------------------------------------

const SHT4X: [&str; 3] = ["sht4x_44", "sht4x_45", "sht4x_46"];

#[test]
fn sht4x_ordering_codes_bind_the_address_variant() {
    for code in [
        "SHT40-AD1B-R2",
        "SHT40-AD1B-R3",
        "SHT40-AD1F-R2",
        "SHT40-AD1P-R2",
        "SHT41-AD1B-R2",
        "SHT41-AD1B-R3",
        "SHT41-AD1F-R2",
        "SHT41-AD1P-R2",
        "SHT45-AD1B-R2",
        "SHT45-AD1F-R2",
        "SHT45-AD1B-R3",
    ] {
        assert_eq!(id_of(code).as_deref(), Some("sht4x_44"), "{code}");
        assert_eq!(register_map_address(code), 0x44, "{code}");
    }
    for code in ["SHT40-BD1B-R2", "SHT40-BD1F-R2", "SHT40-BD1B-R3"] {
        assert_eq!(id_of(code).as_deref(), Some("sht4x_45"), "{code}");
        assert_eq!(register_map_address(code), 0x45, "{code}");
    }
    assert_eq!(id_of("SHT40-CD1B-R3").as_deref(), Some("sht4x_46"));
    assert_eq!(register_map_address("SHT40-CD1B-R3"), 0x46);
}

#[test]
fn sht4x_pins_levels_and_supply() {
    let m = expect_query(Some("SHT45-AD1B-R2"), Some("SHT45-AD1B-R2"), None);
    assert_pin_map(
        &m,
        "sht4x_44",
        &pins("1=sda 2=scl 3=vdd 4=gnd"),
    );
    assert!(has_supply_range(&m, "vdd", (1.08, 3.6), None, "Table 4"));
    assert!((param(&m, "vih") - 0.7 * 3.3).abs() < 0.01);
    assert!((param(&m, "vil") - 0.3 * 3.3).abs() < 0.01);
    assert_eq!(m.params.get_str("supply_pin"), Some("3"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("4"));
}

#[test]
fn sht4x_does_not_claim_siblings() {
    for sibling in [
        "SHT43-ADCB-R2",
        "SHT43-ADCB-R3",
        "SHT43-BDCB-R3",
        "SHT30-DIS-B",
        "SHT31-DIS-B",
        "SHT35-DIS-B",
        "SHT40-DD1B-R2",
        "SHT40-AD1B-R4",
        "SHT46-AD1B-R2",
        "SHT4",
        "SHT21",
        "SHT45X",
    ] {
        assert_not_in(sibling, &SHT4X);
    }
}
