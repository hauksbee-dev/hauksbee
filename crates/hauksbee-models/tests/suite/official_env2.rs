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

// ── Every card: write registers read back ────────────────────────────────────

/// Every pointer-framed write register has a read register at its address, so firmware that
/// writes a configuration value and reads it back sees what it wrote.
#[test]
fn every_env2_write_register_reads_back() {
    for code in [
        "BMP388",
        "BMP390",
        "BME680",
        "BME688",
        "TMP117AIDRVR",
        "TMP117AIYBGR",
        "TSL2591",
        "TSL2561CS",
        "TSL2561CL",
        "MLX90614ESF-BCC-000-TU",
        "MLX90614ESF-ACC-000-TU",
    ] {
        let m = resolve_part(code).unwrap_or_else(|| panic!("{code} resolves"));
        let spec = spec_of(&m);
        for w in &spec.sensor.write_registers {
            let r = spec
                .sensor
                .registers
                .iter()
                .find(|r| r.addr == w.addr)
                .unwrap_or_else(|| panic!("{}: write {:#04x} has no readback", m.id, w.addr));
            assert!(
                r.expr
                    .as_deref()
                    .is_some_and(|e| e.contains(w.store.as_str())),
                "{}: readback of {:#04x} must be built from the store {}",
                m.id,
                w.addr,
                w.store
            );
        }
    }
}

// ── TMP117 ───────────────────────────────────────────────────────────────────

const TMP117_DRV_PADS: &str = "1=scl 2=gnd 3=alert 4=add0 5=vdd 6=sda";
const TMP117_YBG_PADS: &str = "A1=sda A2=scl B1=vdd B2=gnd C1=add0 C2=alert";

#[test]
fn tmp117_cards_follow_the_package() {
    for code in [
        "TMP117AIDRVR",
        "TMP117AIDRVRG4",
        "TMP117MAIDRVR",
        "TMP117NAIDRVR",
    ] {
        let m = resolve_part(code).unwrap_or_else(|| panic!("{code} resolves"));
        assert_pin_map(&m, "tmp117_drv", &pins(TMP117_DRV_PADS));
        assert_eq!(m.pins.get("5").map(String::as_str), Some("vdd"));
        assert_eq!(param_str(&m, "supply_pin"), "5");
    }
    for code in [
        "TMP117AIYBGR",
        "TMP117AIYBGT",
        "TMP117MAIYBGT",
        "TMP117NAIYBGR",
    ] {
        let m = resolve_part(code).unwrap_or_else(|| panic!("{code} resolves"));
        assert_pin_map(&m, "tmp117_ybg", &pins(TMP117_YBG_PADS));
        assert_eq!(param_str(&m, "supply_pin"), "B1");
    }
}

#[test]
fn tmp117_card_values() {
    let m = resolve_part("TMP117AIDRVR").expect("TMP117AIDRVR resolves");
    assert!(has_supply_range(
        &m,
        "vdd",
        (1.8, 5.5),
        Some(6.0),
        "Section 6.3"
    ));
    assert_power(&m, "vdd", 1.6);
    assert!((param(&m, "supply_static_ua") - 16.0).abs() < 1e-9);
    let spec = spec_of(&m);
    assert_eq!(spec.sensor.i2c_address, Some(0x48));
    let rows: Vec<u8> = spec
        .sensor
        .address_selects
        .iter()
        .map(|a| a.address)
        .collect();
    assert_eq!(rows, [0x48, 0x49, 0x4A, 0x4B], "four ADD0 straps");
    assert_eq!(const_at(&spec, 0x0F), [0x01, 0x17], "device ID 0x0117");
    assert!((write_default(&spec, 0x01) - f64::from(0x0220)).abs() < 1e-9);
    assert!((write_default(&spec, 0x02) - f64::from(0x6000)).abs() < 1e-9);
    assert!((write_default(&spec, 0x03) - f64::from(-32768)).abs() < 1e-9);
    let ybg = resolve_part("TMP117AIYBGR").expect("YBG resolves");
    assert_power(&ybg, "vdd", 1.6);
    assert!(
        (ybg.ratings.theta_ja_c_per_w.expect("YBG theta_ja") - 133.2).abs() < 1e-9,
        "DSBGA junction-to-ambient"
    );
    assert!(
        (m.ratings.theta_ja_c_per_w.expect("DRV theta_ja") - 70.7).abs() < 1e-9,
        "WSON junction-to-ambient"
    );
}

#[test]
fn tmp117_bare_name_asks_for_the_package() {
    let r = resolve_query(Some("TMP117"), Some("TMP117"), None);
    assert!(r.model.is_none(), "two pinouts: no guessed stand-in");
    assert!(r.selection.is_some(), "the selection is offered instead");
}

#[test]
fn tmp117_siblings_do_not_bind() {
    assert_not_ours(
        &["TMP116", "TMP1075", "TMP11", "TMP1170", "TMP119", "TMP117X"],
        &["tmp117_drv", "tmp117_ybg"],
    );
}

// ── TSL2591 ──────────────────────────────────────────────────────────────────

#[test]
fn tsl2591_card() {
    for code in ["TSL2591", "TSL25911", "TSL25911FN"] {
        let m = resolve_part(code).unwrap_or_else(|| panic!("{code} resolves"));
        assert_pin_map(&m, "tsl2591", &pins("1=scl 2=int 3=gnd 4=nc 5=vdd 6=sda"));
    }
    let m = resolve_part("TSL25911FN").expect("resolves");
    assert!(has_supply_range(
        &m,
        "vdd",
        (2.7, 3.6),
        Some(3.8),
        "Figure TSL2591-7"
    ));
    assert_power(&m, "vdd", 2.7);
    let spec = spec_of(&m);
    assert_eq!(spec.sensor.i2c_address, Some(0x29));
    assert_eq!(const_at(&spec, 0xB2), [0x50], "ID");
    assert_eq!(
        strap(&m),
        (Vec::new(), Vec::new(), None, None, None),
        "the address is fixed"
    );
    assert_not_ours(
        &["TSL2590", "TSL2571", "TSL25913FN", "TSL25911X", "TSL2592"],
        &["tsl2591"],
    );
}

// ── TSL2561 ──────────────────────────────────────────────────────────────────

#[test]
fn tsl2561_cards_follow_the_package() {
    for code in ["TSL2561CS", "TSL2561T", "TSL2561FN"] {
        let m = resolve_part(code).unwrap_or_else(|| panic!("{code} resolves"));
        assert_pin_map(
            &m,
            "tsl2561_cs_t_fn",
            &pins("1=vdd 2=addr_sel 3=gnd 4=scl 5=int 6=sda"),
        );
        assert_eq!(param_str(&m, "supply_pin"), "1");
        assert_eq!(param_str(&m, "gnd_pin"), "3");
    }
    let m = resolve_part("TSL2561CL").expect("TSL2561CL resolves");
    assert_pin_map(
        &m,
        "tsl2561_cl",
        &pins("1=vdd 2=gnd 3=addr_sel 4=scl 5=sda 6=int"),
    );
    assert_eq!(param_str(&m, "gnd_pin"), "2");
}

#[test]
fn tsl2561_card_values() {
    let m = resolve_part("TSL2561FN").expect("resolves");
    assert!(has_supply_range(
        &m,
        "vdd",
        (2.7, 3.6),
        Some(3.8),
        "Recommended Operating"
    ));
    assert_power(&m, "vdd", 2.7);
    assert!((param(&m, "supply_static_ua") - 240.0).abs() < 1e-9);
    let (high, low, role, lo, hi) = strap(&m);
    assert!(high.is_empty() && low.is_empty());
    assert_eq!(
        (role.as_deref(), lo, hi),
        (Some("addr_sel"), Some(0x29), Some(0x49))
    );
    let spec = spec_of(&m);
    assert_eq!(spec.sensor.i2c_address, Some(0x39), "floating ADDR SEL");
    assert!((write_default(&spec, 0x81) - 2.0).abs() < 1e-9, "TIMING");
    assert!((write_default(&spec, 0x80) - 0.0).abs() < 1e-9, "CONTROL");
}

#[test]
fn tsl2561_bare_name_asks_for_the_package() {
    let r = resolve_query(Some("TSL2561"), Some("TSL2561"), None);
    assert!(r.model.is_none(), "CS/T/FN and CL pinouts differ");
    assert!(r.selection.is_some());
    assert_not_ours(
        &["TSL2560", "TSL2562", "TSL25611", "TSL2560CS", "TSL2571"],
        &["tsl2561_cs_t_fn", "tsl2561_cl"],
    );
}

// ── BH1750 ───────────────────────────────────────────────────────────────────

#[test]
fn bh1750_card() {
    for code in ["BH1750", "BH1750FVI", "BH1750FVI-TR"] {
        let m = resolve_part(code).unwrap_or_else(|| panic!("{code} resolves"));
        assert_pin_map(
            &m,
            "bh1750fvi",
            &pins("1=vcc 2=addr 3=gnd 4=sda 5=dvi 6=scl"),
        );
    }
    let m = resolve_part("BH1750FVI-TR").expect("resolves");
    assert!(has_supply_range(
        &m,
        "vcc",
        (2.4, 3.6),
        Some(4.5),
        "Operating Conditions"
    ));
    assert!(has_supply_range(
        &m,
        "dvi",
        (1.65, 3.6),
        None,
        "I2C Reference"
    ));
    assert!(
        m.envelope.iter().any(|e| matches!(
            e,
            hauksbee_models::OperatingEnvelope::RailOrder { lower, upper, .. }
                if lower == "dvi" && upper == "vcc"
        )),
        "DVI may not exceed VCC"
    );
    assert_power(&m, "vcc", 2.4);
    let (high, low, role, lo, hi) = strap(&m);
    assert_eq!((high, low), (vec!["dvi".to_string()], Vec::new()));
    assert_eq!(
        (role.as_deref(), lo, hi),
        (Some("addr"), Some(0x23), Some(0x5C))
    );
    let spec = spec_of(&m);
    assert_eq!(spec.sensor.i2c_address, Some(0x23));
    assert_eq!(spec.sensor.write_commands.len(), 11, "opcode table");
    assert!(spec.sensor.read_frame.is_some(), "two-byte result frame");
    assert_not_ours(
        &["BH1721", "BH1730", "BH17500", "BH1745", "BH1750X"],
        &["bh1750fvi"],
    );
}

// ── MLX90614 ─────────────────────────────────────────────────────────────────

#[test]
fn mlx90614_cards_follow_the_supply_option() {
    for code in [
        "MLX90614ESF-BCC-000-TU",
        "MLX90614ESF-DCC-000-TU",
        "MLX90614ESF-BAA",
    ] {
        let m = resolve_part(code).unwrap_or_else(|| panic!("{code} resolves"));
        assert_pin_map(&m, "mlx90614_3v", &pins("1=scl 2=sda 3=vdd 4=gnd"));
    }
    for code in [
        "MLX90614ESF-ACC-000-TU",
        "MLX90614KSF-ACC-000-TU",
        "MLX90614ESF-AAA",
    ] {
        let m = resolve_part(code).unwrap_or_else(|| panic!("{code} resolves"));
        assert_pin_map(&m, "mlx90614_5v", &pins("1=scl 2=sda 3=vdd 4=gnd"));
    }
    let three = resolve_part("MLX90614ESF-BCC-000-TU").expect("3 V resolves");
    assert!(has_supply_range(
        &three,
        "vdd",
        (2.6, 3.6),
        Some(5.0),
        "Table 6"
    ));
    assert_power(&three, "vdd", 1.75);
    assert!(
        three
            .peripheral_power
            .as_ref()
            .and_then(|p| p.low_power_a)
            .is_some_and(|a| (a - 2e-6).abs() < 1e-12),
        "the 3 V parts sleep at 2 uA"
    );
    let five = resolve_part("MLX90614ESF-ACC-000-TU").expect("5 V resolves");
    assert!(has_supply_range(
        &five,
        "vdd",
        (4.5, 5.5),
        Some(7.0),
        "Table 4"
    ));
    assert!(
        five.peripheral_power
            .as_ref()
            .is_some_and(|p| p.low_power_a.is_none()),
        "the 5 V parts have no sleep mode"
    );
    assert!((param(&five, "vih") - 3.0).abs() < 1e-9);
    assert!((param(&three, "vih") - 2.9).abs() < 1e-9);
    for m in [&three, &five] {
        let spec = spec_of(m);
        assert_eq!(spec.sensor.i2c_address, Some(0x5A));
        assert_eq!(const_at(&spec, 0xF0), [0x10, 0x00], "flags");
        assert!((write_default(&spec, 0x24) - 65535.0).abs() < 1e-9);
    }
}

#[test]
fn mlx90614_bare_name_binds_a_flagged_stand_in() {
    let r = resolve_query(Some("MLX90614"), Some("MLX90614"), None);
    let m = r.model.expect("same pinout binds a stand-in");
    assert_eq!(m.id, "mlx90614_3v", "first by id");
    let s = r.selection.expect("the stand-in is flagged");
    assert!(s.same_pinout);
    assert_not_ours(
        &["MLX90615", "MLX90640", "MLX906140", "MLX90614X", "MLX90632"],
        &["mlx90614_3v", "mlx90614_5v"],
    );
}

/// The text of a string param the entry must carry.
fn param_str(m: &ModelEntry, k: &str) -> String {
    m.params
        .get_str(k)
        .unwrap_or_else(|| panic!("model {:?} has no string param {k}", m.id))
        .to_string()
}
