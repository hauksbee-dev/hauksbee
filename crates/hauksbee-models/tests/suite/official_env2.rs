//! Official-parts batch env2: resolution and behaviour tests for the cards added in that batch.
//!
//! Every card resolves by a real ordering code, binds the datasheet pad map, carries the
//! datasheet supply range and absolute maximum, and declares a register-map bus model whose
//! identity and reset values are the ones printed in the datasheet. Sibling part numbers
//! (other members of the same vendor family) must NOT resolve to these cards.

use super::{assert_pin_map, has_supply_range, param, pins, query_id, resolve_part, resolve_query};
use hauksbee_models::sensor_spec::SensorSpec;
use hauksbee_models::{ModelEntry, PeripheralSpec};

/// The register-map spec a card carries, parsed and validated.
fn spec_of(m: &ModelEntry) -> SensorSpec {
    let Some(PeripheralSpec::RegisterMap { spec_toml, .. }) = &m.peripheral else {
        panic!("{}: no register_map peripheral", m.id);
    };
    SensorSpec::from_toml(spec_toml).unwrap_or_else(|e| panic!("{}: spec invalid: {e}", m.id))
}

/// The constant bytes a read of `addr` returns.
fn const_at(spec: &SensorSpec, addr: u8) -> Vec<u8> {
    spec.sensor
        .registers
        .iter()
        .find(|r| r.addr == addr)
        .unwrap_or_else(|| panic!("{}: register {addr:#04x} not declared", spec.sensor.name))
        .r#const
        .clone()
        .unwrap_or_else(|| {
            panic!(
                "{}: register {addr:#04x} is not a constant",
                spec.sensor.name
            )
        })
}

/// The write-register default at `addr`.
fn write_default(spec: &SensorSpec, addr: u8) -> f64 {
    spec.sensor
        .write_registers
        .iter()
        .find(|r| r.addr == addr)
        .unwrap_or_else(|| panic!("{}: write register {addr:#04x} missing", spec.sensor.name))
        .default
}

/// The I2C address strap contract of a card: (required high, required low, role, low, high).
fn strap(
    m: &ModelEntry,
) -> (
    Vec<String>,
    Vec<String>,
    Option<String>,
    Option<u8>,
    Option<u8>,
) {
    let Some(PeripheralSpec::RegisterMap {
        required_high_roles,
        required_low_roles,
        address_select_role,
        address_when_low,
        address_when_high,
        ..
    }) = &m.peripheral
    else {
        panic!("{}: no register_map peripheral", m.id);
    };
    (
        required_high_roles.clone(),
        required_low_roles.clone(),
        address_select_role.clone(),
        *address_when_low,
        *address_when_high,
    )
}

/// Each value must not bind any of the `ours` cards.
fn assert_not_ours(values: &[&str], ours: &[&str]) {
    for v in values {
        let got = resolve_part(v).map(|m| m.id);
        assert!(
            got.as_deref().is_none_or(|id| !ours.contains(&id)),
            "{v} must not resolve to one of {ours:?}, got {got:?}"
        );
    }
}

/// The bus model draws the datasheet supply current and gates on the minimum supply.
fn assert_power(m: &ModelEntry, role: &str, threshold: f64) {
    let p = m
        .peripheral_power
        .as_ref()
        .unwrap_or_else(|| panic!("{}: no peripheral_power", m.id));
    assert_eq!(p.supply_role, role, "{}", m.id);
    assert_eq!(p.return_role, "gnd", "{}", m.id);
    assert!(
        (p.power_on_threshold_v - threshold).abs() < 1e-9,
        "{}",
        m.id
    );
    assert!(
        p.idle_a > 0.0 && p.read_a >= p.idle_a && p.write_a >= p.idle_a,
        "{}",
        m.id
    );
}

// ── BMP388 / BMP390 ──────────────────────────────────────────────────────────

const BMP3_PADS: &str = "1=vddio 2=scl 3=gnd 4=sda 5=sdo 6=csb 7=int 8=gnd2 9=gnd3 10=vdd";

#[test]
fn bmp388_card() {
    let m = resolve_part("BMP388").expect("BMP388 resolves");
    assert_pin_map(&m, "bmp388", &pins(BMP3_PADS));
    assert!(has_supply_range(
        &m,
        "vdd",
        (1.65, 3.6),
        Some(3.8),
        "Table 2"
    ));
    assert!(has_supply_range(
        &m,
        "vddio",
        (1.2, 3.6),
        Some(3.8),
        "Table 2"
    ));
    assert!((param(&m, "supply_static_ua") - 46.4).abs() < 1e-9);
    assert_power(&m, "vdd", 1.65);
    let (high, low, role, lo, hi) = strap(&m);
    assert_eq!((high, low), (vec!["csb".to_string()], Vec::new()));
    assert_eq!(
        (role.as_deref(), lo, hi),
        (Some("sdo"), Some(0x76), Some(0x77))
    );
    let spec = spec_of(&m);
    assert_eq!(spec.sensor.i2c_address, Some(0x76));
    assert_eq!(const_at(&spec, 0x00), [0x50], "CHIP_ID");
}

#[test]
fn bmp390_card() {
    let m = resolve_part("BMP390").expect("BMP390 resolves");
    assert_pin_map(&m, "bmp390", &pins(BMP3_PADS));
    assert!(has_supply_range(
        &m,
        "vdd",
        (1.65, 3.6),
        Some(3.6),
        "Table 2"
    ));
    assert!(has_supply_range(
        &m,
        "vddio",
        (1.2, 3.6),
        Some(3.6),
        "Table 2"
    ));
    assert_power(&m, "vdd", 1.65);
    let (_, _, role, lo, hi) = strap(&m);
    assert_eq!(
        (role.as_deref(), lo, hi),
        (Some("sdo"), Some(0x76), Some(0x77))
    );
    let spec = spec_of(&m);
    assert_eq!(const_at(&spec, 0x00), [0x60], "CHIP_ID");
    assert_eq!(const_at(&spec, 0x01), [0x01], "REV_ID");
}

#[test]
fn bmp3_siblings_do_not_bind() {
    assert_not_ours(
        &["BMP380", "BMP3880", "BMP38", "BMP398", "BMP3900"],
        &["bmp388", "bmp390"],
    );
    assert_eq!(
        query_id(Some("BMP388"), Some("BMP388"), None).as_deref(),
        Some("bmp388")
    );
    assert_ne!(
        query_id(Some("BMP390"), None, None).as_deref(),
        Some("bmp388")
    );
    assert_ne!(
        resolve_part("BMP280").map(|m| m.id).as_deref(),
        Some("bmp388")
    );
    assert_ne!(
        resolve_part("BMP581").map(|m| m.id).as_deref(),
        Some("bmp390")
    );
}

// ── BME680 / BME688 ──────────────────────────────────────────────────────────

const BME68_PADS: &str = "1=gnd 2=csb 3=sda 4=scl 5=sdo 6=vddio 7=gnd2 8=vdd";

#[test]
fn bme680_card() {
    let m = resolve_part("BME680").expect("BME680 resolves");
    assert_pin_map(&m, "bme680", &pins(BME68_PADS));
    assert!(has_supply_range(
        &m,
        "vdd",
        (1.71, 3.6),
        Some(4.25),
        "Table 1"
    ));
    assert!(has_supply_range(
        &m,
        "vddio",
        (1.2, 3.6),
        Some(4.25),
        "Table 1"
    ));
    assert_power(&m, "vdd", 1.71);
    let (high, _, role, lo, hi) = strap(&m);
    assert_eq!(high, vec!["csb".to_string()]);
    assert_eq!(
        (role.as_deref(), lo, hi),
        (Some("sdo"), Some(0x76), Some(0x77))
    );
    let spec = spec_of(&m);
    assert_eq!(const_at(&spec, 0xD0), [0x61], "Id");
    assert_eq!(const_at(&spec, 0x73), [0x00], "status reset");
    assert!(
        (write_default(&spec, 0x71) - 0.0).abs() < 1e-9,
        "ctrl_gas_1 reset"
    );
    assert_eq!(
        resolve_part("BME680-xyz").map(|m| m.id).as_deref(),
        Some("bme680")
    );
}

#[test]
fn bme688_card() {
    let m = resolve_part("BME688").expect("BME688 resolves");
    assert_pin_map(&m, "bme688", &pins(BME68_PADS));
    assert!(has_supply_range(
        &m,
        "vdd",
        (1.71, 3.6),
        Some(4.25),
        "Table 1"
    ));
    assert_power(&m, "vdd", 1.71);
    let spec = spec_of(&m);
    assert_eq!(const_at(&spec, 0xD0), [0x61], "Chip_Id");
    assert_eq!(const_at(&spec, 0xF0), [0x01], "Variant_Id");
    assert_eq!(const_at(&spec, 0x73), [0x01], "status reset");
}

#[test]
fn bme68_siblings_do_not_bind() {
    assert_not_ours(
        &["BME6800", "BME68", "BME690", "BME280", "BME681"],
        &["bme680", "bme688"],
    );
    assert_ne!(
        resolve_part("BME688").map(|m| m.id).as_deref(),
        Some("bme680")
    );
    assert_ne!(
        resolve_part("BME680").map(|m| m.id).as_deref(),
        Some("bme688")
    );
    // The bare resolver agrees for value-only queries.
    assert_eq!(
        resolve_query(Some("BME680"), None, None)
            .model
            .map(|m| m.id)
            .as_deref(),
        Some("bme680")
    );
}
