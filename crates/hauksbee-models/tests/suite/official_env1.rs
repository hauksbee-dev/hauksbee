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
    assert_pin_map(&m, "sht4x_44", &pins("1=sda 2=scl 3=vdd 4=gnd"));
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

// ---- HTU21D / SHT21 ---------------------------------------------------------------------------

const HTU_SHT21: [&str; 2] = ["htu21d", "sht21"];

#[test]
fn htu21d_codes_resolve_to_its_card() {
    for code in [
        "HTU21D",
        "htu21d",
        "HTU21DF",
        "HTU21D-F",
        "HPP845E031R4",
        "HPP845E031R1",
        "HPP845E131R5",
    ] {
        assert_eq!(id_of(code).as_deref(), Some("htu21d"), "{code}");
        assert_eq!(register_map_address(code), 0x40, "{code}");
    }
    let m = expect_query(Some("HTU21D"), Some("HTU21D"), None);
    assert_pin_map(&m, "htu21d", &pins("1=sda 2=gnd 3=nc 4=nc 5=vdd 6=scl"));
    assert!(has_supply_range(&m, "vdd", (1.5, 3.6), Some(3.6), "p.3"));
    assert!((param(&m, "vih") - 0.7 * 3.0).abs() < 0.01);
    assert!((param(&m, "vil") - 0.3 * 3.0).abs() < 0.01);
    assert_eq!(m.params.get_str("supply_pin"), Some("5"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("2"));
}

#[test]
fn sht21_codes_resolve_to_its_card() {
    for code in [
        "SHT21",
        "sht21",
        "1-100707-01",
        "1-100645-01",
        "1-100694-01",
    ] {
        assert_eq!(id_of(code).as_deref(), Some("sht21"), "{code}");
        assert_eq!(register_map_address(code), 0x40, "{code}");
    }
    let m = expect_query(Some("SHT21"), Some("SHT21"), None);
    assert_pin_map(&m, "sht21", &pins("1=sda 2=gnd 3=nc 4=nc 5=vdd 6=scl"));
    assert!(has_supply_range(
        &m,
        "vdd",
        (2.1, 3.6),
        Some(5.0),
        "Table 1"
    ));
    assert!((param(&m, "vih") - 0.7 * 3.0).abs() < 0.01);
    assert_eq!(m.params.get_str("supply_pin"), Some("5"));
}

#[test]
fn htu21d_and_sht21_do_not_claim_siblings() {
    for sibling in [
        "HTU20D",
        "HTU21P",
        "HTU21",
        "HTU21DX",
        "HPP845E331R4",
        "SHT20",
        "SHT25",
        "SHT21D",
        "SHT2",
        "SHT2x",
        "SHT31-DIS-B",
        "SHT30-DIS-B",
        "SHT40-AD1B-R2",
        "Si7021-A20",
        "Si7020-A20",
        "HDC1080",
        "AHT21",
    ] {
        assert_not_in(sibling, &HTU_SHT21);
    }
}

// ---- AHT10 / AHT20 / AHT21 --------------------------------------------------------------------

const AHT: [&str; 3] = ["aht10", "aht20", "aht21"];

#[test]
fn aht_codes_resolve_to_their_own_card() {
    for (code, id) in [
        ("AHT10", "aht10"),
        ("aht10", "aht10"),
        ("AHT20", "aht20"),
        ("aht20", "aht20"),
        ("AHT21", "aht21"),
        ("Aht21", "aht21"),
    ] {
        assert_eq!(id_of(code).as_deref(), Some(id), "{code}");
        assert_eq!(register_map_address(code), 0x38, "{code}");
    }
}

#[test]
fn aht_pins_levels_and_supply() {
    let m = expect_query(Some("AHT10"), Some("AHT10"), None);
    assert_pin_map(&m, "aht10", &pins("1=adr 2=sda 3=scl 4=vdd 5=gnd 6=nc"));
    assert!(has_supply_range(
        &m,
        "vdd",
        (1.8, 3.6),
        Some(3.6),
        "Table 2"
    ));
    assert_eq!(m.params.get_str("supply_pin"), Some("4"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("5"));
    assert!((param(&m, "vih") - 0.7 * 3.3).abs() < 0.01);

    let m = expect_query(Some("AHT20"), Some("AHT20"), None);
    assert_pin_map(&m, "aht20", &pins("1=nc 2=vdd 3=scl 4=sda 5=gnd 6=nc"));
    assert!(has_supply_range(
        &m,
        "vdd",
        (2.0, 5.5),
        Some(5.5),
        "Table 2"
    ));
    assert_eq!(m.params.get_str("supply_pin"), Some("2"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("5"));
    assert!((param(&m, "vil") - 0.3 * 3.3).abs() < 0.01);

    let m = expect_query(Some("AHT21"), Some("AHT21"), None);
    assert_pin_map(&m, "aht21", &pins("1=nc 2=vdd 3=scl 4=sda 5=gnd 6=nc"));
    assert!(has_supply_range(
        &m,
        "vdd",
        (2.2, 5.5),
        Some(5.5),
        "Table 3"
    ));
    assert_eq!(m.params.get_str("supply_pin"), Some("2"));
}

#[test]
fn aht_does_not_claim_siblings() {
    for sibling in [
        "AHT1",
        "AHT2",
        "AHT20B",
        "AHT21B",
        "AHT25",
        "AHT30",
        "AHT10A",
        "AHT20F",
        "AHT2x",
        "AHT",
        "DHT20",
        "DHT21",
        "SHT21",
        "HTU21D",
        "Si7021-A20",
        "SHT40-AD1B-R2",
        "SHT31-DIS-B",
    ] {
        assert_not_in(sibling, &AHT);
    }
}

// ---- Si7021 -----------------------------------------------------------------------------------

#[test]
fn si7021_ordering_codes_resolve_to_its_card() {
    for code in [
        "Si7021",
        "SI7021",
        "Si7021-A20",
        "Si7021-A20-GM",
        "Si7021-A20-GMR",
        "Si7021-A20-GM1",
        "Si7021-A20-GM1R",
        "Si7021-A20-IM",
        "Si7021-A20-IMR",
        "Si7021-A20-IM1",
        "Si7021-A20-IM1R",
        "Si7021-A20-YM0",
        "Si7021-A20-YM0R",
        "Si7021-A20-YM1",
        "Si7021-A20-YM1R",
    ] {
        assert_eq!(id_of(code).as_deref(), Some("si7021"), "{code}");
        assert_eq!(register_map_address(code), 0x40, "{code}");
    }
}

#[test]
fn si7021_pins_levels_supply_and_thermal() {
    let m = expect_query(Some("Si7021-A20-GM"), Some("Si7021-A20-GM"), None);
    assert_pin_map(&m, "si7021", &pins("1=sda 2=gnd 3=dnc 4=dnc 5=vdd 6=scl"));
    assert!(has_supply_range(
        &m,
        "vdd",
        (1.9, 3.6),
        Some(4.2),
        "Table 1"
    ));
    assert!((param(&m, "vih") - 0.7 * 3.3).abs() < 0.01);
    assert!((param(&m, "vil") - 0.3 * 3.3).abs() < 0.01);
    assert_eq!(m.params.get_str("supply_pin"), Some("5"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("2"));
    assert!((param(&m, "supply_static_ua") - 0.06).abs() < 1e-9);
}

#[test]
fn si7021_does_not_claim_siblings() {
    for sibling in [
        "Si7020",
        "Si7020-A20",
        "Si7013",
        "Si7006",
        "Si7021-A10",
        "Si7021-A20-GM2",
        "Si7021-A20-XM",
        "Si7021-A20-GMX",
        "Si7021-B20",
        "Si70210",
        "SI7021X",
        "SHT21",
        "HTU21D",
        "AHT20",
        "SHT40-AD1B-R2",
    ] {
        if let Some(id) = id_of(sibling) {
            assert_ne!(
                id, "si7021",
                "{sibling} must not resolve to the Si7021 card"
            );
        }
    }
}

// ---- VL53L0X ----------------------------------------------------------------------------------

#[test]
fn vl53l0x_codes_resolve_and_the_card_is_at_0x29() {
    for code in ["VL53L0X", "vl53l0x", "VL53L0CXV0DH/1", "VL53L0CXV9DH/1"] {
        assert_eq!(id_of(code).as_deref(), Some("vl53l0x"), "{code}");
        assert_eq!(register_map_address(code), 0x29, "{code}");
    }
}

#[test]
fn vl53l0x_pins_levels_and_supply() {
    let m = expect_query(Some("VL53L0X"), Some("VL53L0X"), None);
    assert_pin_map(
        &m,
        "vl53l0x",
        &pins("1=avddvcsel 2=avssvcsel 3=gnd 4=gnd 5=xshut 6=gnd 7=gpio1 8=dnc 9=sda 10=scl 11=vdd 12=gnd"),
    );
    assert!(has_supply_range(
        &m,
        "vdd",
        (2.6, 3.5),
        Some(3.6),
        "Table 8"
    ));
    assert!((param(&m, "vih") - 1.12).abs() < 1e-9);
    assert!((param(&m, "vil") - 0.6).abs() < 1e-9);
    assert_eq!(m.params.get_str("supply_pin"), Some("11"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("3"));
}

#[test]
fn vl53l0x_does_not_claim_siblings() {
    for sibling in [
        "VL53L1X",
        "VL53L0",
        "VL53L0CXV0DH",
        "VL53L0CXV8DH/1",
        "VL53L0X-V2",
        "VL53L1CXV0FY/1",
        "VL6180X",
        "VL53L4CD",
    ] {
        if let Some(id) = id_of(sibling) {
            assert_ne!(
                id, "vl53l0x",
                "{sibling} must not resolve to the VL53L0X card"
            );
        }
    }
}

// ---- DS18B20 ----------------------------------------------------------------------------------

#[test]
fn ds18b20_ordering_codes_bind_the_package_card() {
    for code in [
        "DS18B20",
        "DS18B20+",
        "DS18B20/T&R",
        "DS18B20+T&R",
        "DS18B20-SL/T&R",
        "DS18B20-SL+T&R",
        "ds18b20",
    ] {
        assert_eq!(id_of(code).as_deref(), Some("ds18b20_to92"), "{code}");
    }
    for code in ["DS18B20Z", "DS18B20Z+", "DS18B20Z/T&R", "DS18B20Z+T&R"] {
        assert_eq!(id_of(code).as_deref(), Some("ds18b20_so8"), "{code}");
    }
    for code in ["DS18B20U", "DS18B20U+", "DS18B20U/T&R", "DS18B20U+T&R"] {
        assert_eq!(id_of(code).as_deref(), Some("ds18b20_usop8"), "{code}");
    }
}

#[test]
fn ds18b20_pin_maps_follow_each_package() {
    let m = expect_query(Some("DS18B20"), Some("DS18B20"), None);
    assert_pin_map(&m, "ds18b20_to92", &pins("1=gnd 2=dq 3=vdd"));
    assert!(has_supply_range(
        &m,
        "vdd",
        (3.0, 5.5),
        Some(6.0),
        "DC Electrical"
    ));
    assert_eq!(m.params.get_str("supply_pin"), Some("3"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("1"));
    assert!((param(&m, "vih") - 2.2).abs() < 1e-9);
    assert!((param(&m, "vil") - 0.8).abs() < 1e-9);

    let m = expect_query(Some("DS18B20Z+"), Some("DS18B20Z+"), None);
    assert_pin_map(
        &m,
        "ds18b20_so8",
        &pins("1=nc 2=nc 3=vdd 4=dq 5=gnd 6=nc 7=nc 8=nc"),
    );
    assert_eq!(m.params.get_str("supply_pin"), Some("3"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("5"));

    let m = expect_query(Some("DS18B20U+T&R"), Some("DS18B20U+T&R"), None);
    assert_pin_map(
        &m,
        "ds18b20_usop8",
        &pins("1=dq 2=nc 3=nc 4=gnd 5=nc 6=nc 7=nc 8=vdd"),
    );
    assert_eq!(m.params.get_str("supply_pin"), Some("8"));
    assert_eq!(m.params.get_str("gnd_pin"), Some("4"));
}

#[test]
fn ds18b20_does_not_claim_siblings() {
    const FAMILY: [&str; 3] = ["ds18b20_to92", "ds18b20_so8", "ds18b20_usop8"];
    for sibling in [
        "DS18B20-PAR",
        "DS18S20",
        "DS1822",
        "DS18B20X",
        "DS18B20Z-SL/T&R",
        "DS18B20U-SL+T&R",
        "DS18B20Z/T",
        "DS18B2",
        "DS28EA00",
        "MAX31820",
    ] {
        assert_not_in(sibling, &FAMILY);
    }
}
