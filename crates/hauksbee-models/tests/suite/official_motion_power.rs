//! Official-parts batch `motion_power`: resolution and behaviour tests for the cards added in that batch.
//!
//! Each card is resolved by its ordering codes; its bus model is read back out of the
//! card (peripheral kind, address and strap, identity and reset values), and the
//! neighbouring part numbers that are different dies must not resolve to it.

use hauksbee_models::schema::PeripheralSpec;
use hauksbee_models::sensor_spec::{Bus, Encoding, SensorSpec};
use hauksbee_models::ModelEntry;

use super::{resolve_part, resolve_query};

/// A card's register-map peripheral, unpacked.
struct Map {
    spec: SensorSpec,
    high_roles: Vec<String>,
    low_roles: Vec<String>,
    select_role: Option<String>,
    when_low: Option<u8>,
    when_high: Option<u8>,
}

fn card(value: &str, id: &str) -> ModelEntry {
    let model = resolve_part(value).unwrap_or_else(|| panic!("{value} did not resolve"));
    assert_eq!(model.id, id, "{value} resolved to {}", model.id);
    model
}

/// The datasheet hashes a resolution retains.
fn shas(value: &str) -> Vec<String> {
    (resolve_query(Some(value), Some(value), None)
        .references
        .iter())
    .filter_map(|reference| reference.sha256.clone())
    .collect()
}

fn map(model: &ModelEntry) -> Map {
    match model
        .peripheral
        .as_ref()
        .expect("card carries a peripheral")
    {
        PeripheralSpec::RegisterMap {
            spec_toml,
            required_high_roles,
            required_low_roles,
            address_select_role,
            address_when_low,
            address_when_high,
            ..
        } => Map {
            spec: SensorSpec::from_toml(spec_toml).expect("spec_toml is a valid sensor spec"),
            high_roles: required_high_roles.clone(),
            low_roles: required_low_roles.clone(),
            select_role: address_select_role.clone(),
            when_low: *address_when_low,
            when_high: *address_when_high,
        },
        other => panic!("{} carries {other:?}, not a register map", model.id),
    }
}

impl Map {
    /// The bytes of a constant register.
    fn constant(&self, addr: u8) -> Vec<u8> {
        let reg = (self.spec.sensor.registers.iter())
            .find(|r| r.addr == addr)
            .unwrap_or_else(|| panic!("no register 0x{addr:02x}"));
        reg.r#const
            .clone()
            .unwrap_or_else(|| panic!("register 0x{addr:02x} is not constant"))
    }

    /// The power-on value of a firmware-writable register, which must also read back.
    fn reset(&self, addr: u8) -> f64 {
        let write = (self.spec.sensor.write_registers.iter())
            .find(|w| w.addr == addr)
            .unwrap_or_else(|| panic!("no writable register 0x{addr:02x}"));
        let read = (self.spec.sensor.registers.iter())
            .find(|r| r.addr == addr)
            .unwrap_or_else(|| panic!("writable register 0x{addr:02x} cannot be read back"));
        assert_eq!(
            read.expr.as_deref(),
            Some(write.store.as_str()),
            "0x{addr:02x}"
        );
        write.default
    }

    fn encoding(&self, addr: u8) -> Option<Encoding> {
        (self.spec.sensor.registers.iter())
            .find(|r| r.addr == addr)
            .and_then(|r| r.encoding)
    }

    const fn address(&self) -> u8 {
        self.spec.sensor.i2c_address.expect("I2C map")
    }
}

/// None of `siblings` resolves to the card `id`.
fn assert_not_resolving(id: &str, siblings: &[&str]) {
    for sibling in siblings {
        assert!(
            resolve_part(sibling).is_none_or(|model| model.id != id),
            "{sibling} must not resolve to {id}"
        );
    }
}

#[test]
fn adxl345_resolves_by_every_ordering_code_to_a_strapped_i2c_map() {
    for code in [
        "ADXL345",
        "ADXL345BCCZ",
        "ADXL345BCCZ-RL",
        "ADXL345BCCZ-RL7",
        "adxl345bccz-rl7",
    ] {
        let model = card(code, "adxl345");
        assert!(model
            .coverage
            .missing
            .iter()
            .any(|gap| gap == "spi_register_map"));
    }
    let model = card("ADXL345BCCZ", "adxl345");
    let map = map(&model);
    assert_eq!(map.spec.sensor.bus, Bus::I2c);
    // CS high selects I2C; ALT ADDRESS (the SDO pad) picks 0x53 low, 0x1D high.
    assert_eq!(map.high_roles, ["cs"]);
    assert!(map.low_roles.is_empty());
    assert_eq!(map.select_role.as_deref(), Some("sdo"));
    assert_eq!((map.when_low, map.when_high), (Some(0x53), Some(0x1D)));
    assert_eq!(map.address(), 0x53);
    assert_eq!(model.pins.get("12").map(String::as_str), Some("sdo"));
    assert_eq!(model.pins.get("7").map(String::as_str), Some("cs"));
    assert_eq!(model.pins.len(), 14);
    assert!(model.pins.values().any(|role| role == "scl"));
    assert_eq!(model.params.get_str("must_not_float_roles"), Some("cs,sdo"));
    let power = model.peripheral_power.as_ref().expect("supply coupling");
    assert_eq!(power.supply_role, "vdd");
    assert!((power.power_on_threshold_v - 2.0).abs() < 1e-12);
    assert!((power.idle_a - 140e-6).abs() < 1e-12);
    assert!(shas("ADXL345")
        .iter()
        .any(|sha| sha == "87ae2212498c35a6759d8732adee0ec9b9d8d60fa95688bc5904f1f07ceb8ff6"));
}

#[test]
fn adxl345_identity_and_reset_values_come_from_table_19() {
    let map = map(&card("ADXL345", "adxl345"));
    assert_eq!(map.constant(0x00), [0xE5], "DEVID");
    assert_eq!(map.constant(0x2B), [0x00], "ACT_TAP_STATUS");
    assert_eq!(map.constant(0x39), [0x00], "FIFO_STATUS");
    assert!(
        (map.reset(0x2C) - 10.0).abs() < 1e-12,
        "BW_RATE resets to 0x0A"
    );
    for addr in [
        0x1D, 0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x2D,
        0x2E, 0x2F, 0x31, 0x38,
    ] {
        assert!(map.reset(addr).abs() < 1e-12, "0x{addr:02x} resets to 0x00");
    }
    // INT_SOURCE resets to 0x02 and sets DATA_READY once measuring.
    let int_source = (map.spec.sensor.registers.iter())
        .find(|r| r.addr == 0x30)
        .expect("INT_SOURCE");
    assert_eq!(int_source.expr.as_deref(), Some("2.0 + 128.0 * pc_measure"));
    // Data registers: little-endian signed words at DATAX0, DATAY0, DATAZ0.
    for addr in [0x32, 0x34, 0x36] {
        assert_eq!(map.encoding(addr), Some(Encoding::I16Le), "0x{addr:02x}");
    }
}

#[test]
fn adxl345_siblings_do_not_resolve_to_it() {
    assert_not_resolving(
        "adxl345",
        &[
            "ADXL343", "ADXL344", "ADXL346", "ADXL375", "ADXL345B", "ADXL3450", "ADXL355",
            "ADXL362",
        ],
    );
}

impl Map {
    /// The power-on value of a writable register, whatever its read-back expression.
    fn stored_default(&self, addr: u8) -> f64 {
        (self.spec.sensor.write_registers.iter())
            .find(|w| w.addr == addr)
            .unwrap_or_else(|| panic!("no writable register 0x{addr:02x}"))
            .default
    }

    fn read_expr(&self, addr: u8) -> String {
        (self.spec.sensor.registers.iter())
            .find(|r| r.addr == addr)
            .and_then(|r| r.expr.clone())
            .unwrap_or_else(|| panic!("no expression register 0x{addr:02x}"))
    }
}

#[test]
fn mpu6050_resolves_to_a_strapped_i2c_map_that_powers_up_asleep() {
    for code in ["MPU-6050", "MPU6050", "mpu-6050"] {
        card(code, "mpu6050");
    }
    let model = card("MPU-6050", "mpu6050");
    let map = map(&model);
    assert_eq!(map.spec.sensor.bus, Bus::I2c);
    // AD0 low is 0x68, high 0x69.
    assert!(map.high_roles.is_empty() && map.low_roles.is_empty());
    assert_eq!(map.select_role.as_deref(), Some("ad0"));
    assert_eq!((map.when_low, map.when_high), (Some(0x68), Some(0x69)));
    assert_eq!(model.pins.get("9").map(String::as_str), Some("ad0"));
    assert_eq!(model.pins.len(), 24);
    assert_eq!(map.constant(0x75), [0x68], "WHO_AM_I");
    assert!(
        (map.stored_default(0x6B) - 64.0).abs() < 1e-12,
        "PWR_MGMT_1 = 0x40"
    );
    // DEVICE_RESET must read back clear or a driver polling for it would hang.
    assert_eq!(map.read_expr(0x6B), "if(pm_reset == 1.0, 64.0, pwr_mgmt_1)");
    for addr in [0x19, 0x1A, 0x1B, 0x1C, 0x23, 0x37, 0x38, 0x6A, 0x6C] {
        assert!(map.reset(addr).abs() < 1e-12, "0x{addr:02x} resets to 0x00");
    }
    for addr in [0x3B, 0x3D, 0x3F, 0x41, 0x43, 0x45, 0x47] {
        assert_eq!(map.encoding(addr), Some(Encoding::I16Be), "0x{addr:02x}");
    }
    let power = model.peripheral_power.as_ref().expect("supply coupling");
    assert_eq!(power.supply_role, "vdd");
    let hashes = shas("MPU-6050");
    for sha in [
        "75d163b19dccec6b6a038e90b95976eab5028cabf00035d84b2c012e8d46cfb5",
        "ccaa6312b9d86a9da79e26e511101e1150dc85a48255600010a854369cf7c05d",
    ] {
        assert!(hashes.iter().any(|h| h == sha), "{sha}");
    }
}

#[test]
fn mpu6050_siblings_do_not_resolve_to_it() {
    assert_not_resolving(
        "mpu6050",
        &[
            "MPU-6000",
            "MPU6000",
            "MPU-6500",
            "MPU6500",
            "MPU-9250",
            "MPU9250",
            "MPU-9150",
            "MPU-6050A",
            "MPU-60500",
            "ICM-20602",
            "ICM-42605",
        ],
    );
}

#[test]
fn lsm6dso_and_lsm6dsox_are_two_cards_with_the_same_identity() {
    for (codes, id) in [
        (&["LSM6DSO", "LSM6DSOTR", "lsm6dsotr"][..], "lsm6dso"),
        (&["LSM6DSOX", "LSM6DSOXTR", "lsm6dsoxtr"][..], "lsm6dsox"),
    ] {
        for code in codes {
            card(code, id);
        }
        let model = card(codes[0], id);
        let map = map(&model);
        assert_eq!(map.spec.sensor.bus, Bus::I2c);
        // CS high selects I2C; SDO/SA0 low is 0x6A, high 0x6B.
        assert_eq!(map.high_roles, ["cs"]);
        assert_eq!(map.select_role.as_deref(), Some("sdo"));
        assert_eq!((map.when_low, map.when_high), (Some(0x6A), Some(0x6B)));
        assert_eq!(model.pins.len(), 14);
        assert_eq!(model.pins.get("1").map(String::as_str), Some("sdo"));
        assert_eq!(model.pins.get("12").map(String::as_str), Some("cs"));
        assert_eq!(model.params.get_str("must_not_float_roles"), Some("sdo"));
        assert_eq!(map.constant(0x0F), [0x6C], "{id} WHO_AM_I");
        assert!((map.reset(0x02) - 63.0).abs() < 1e-12, "PIN_CTRL = 0x3F");
        assert!((map.reset(0x18) - 224.0).abs() < 1e-12, "CTRL9_XL = 0xE0");
        assert!(
            (map.stored_default(0x12) - 4.0).abs() < 1e-12,
            "CTRL3_C = 0x04"
        );
        for addr in [
            0x10, 0x11, 0x13, 0x14, 0x15, 0x16, 0x17, 0x19, 0x07, 0x0D, 0x0E,
        ] {
            assert!(map.reset(addr).abs() < 1e-12, "0x{addr:02x} resets to 0x00");
        }
        for addr in [0x20, 0x22, 0x24, 0x26, 0x28, 0x2A, 0x2C] {
            assert_eq!(map.encoding(addr), Some(Encoding::I16Le), "0x{addr:02x}");
        }
    }
    assert!(shas("LSM6DSO")
        .iter()
        .any(|sha| sha == "dd899e37f2fc33b8592fbf6b5b9d9caa94b47c455d4d2edc3512ba2a5c3f0055"));
    assert!(shas("LSM6DSOX")
        .iter()
        .any(|sha| sha == "4c7b834fcc5ca298e0a7cbe796d9f7d2def3fc19682ee490d8b6f75ecdeb71f7"));
    assert!(card("LSM6DSOX", "lsm6dsox")
        .coverage
        .missing
        .iter()
        .any(|gap| gap == "machine_learning_core"));
}

#[test]
fn lsm6dso_siblings_keep_their_own_cards() {
    for sibling in [
        "LSM6DSO32",
        "LSM6DSR",
        "LSM6DSL",
        "LSM6DSM",
        "LSM6DSV",
        "LSM6DS3",
        "LSM6DS33",
        "LSM6DSOW",
    ] {
        assert_not_resolving("lsm6dso", &[sibling]);
        assert_not_resolving("lsm6dsox", &[sibling]);
    }
    // The cards that already exist still own their part numbers.
    assert_eq!(
        resolve_part("LSM6DS3").map(|m| m.id),
        Some("lsm6ds3".into())
    );
    assert_not_resolving("lsm6dso", &["LSM6DSOX"]);
    assert_not_resolving("lsm6dsox", &["LSM6DSO"]);
}

#[test]
fn lsm303agr_is_the_accelerometer_slave_at_0x19_and_says_the_magnetometer_is_missing() {
    for code in ["LSM303AGR", "LSM303AGRTR", "lsm303agrtr"] {
        card(code, "lsm303agr");
    }
    let model = card("LSM303AGR", "lsm303agr");
    let map = map(&model);
    assert_eq!(map.spec.sensor.bus, Bus::I2c);
    // Fixed address 0011001b; CS_XL high selects I2C; no strap.
    assert_eq!(map.address(), 0x19);
    assert_eq!(map.high_roles, ["cs_xl"]);
    assert!(map.select_role.is_none() && map.when_low.is_none() && map.when_high.is_none());
    assert_eq!(model.pins.len(), 12);
    assert_eq!(model.pins.get("2").map(String::as_str), Some("cs_xl"));
    assert_eq!(model.pins.get("3").map(String::as_str), Some("cs_mag"));
    assert_eq!(
        model.params.get_str("must_not_float_roles"),
        Some("cs_xl,cs_mag")
    );
    assert_eq!(map.constant(0x0F), [0x33], "WHO_AM_I_A");
    assert!(
        (map.reset(0x20) - 7.0).abs() < 1e-12,
        "CTRL_REG1_A = 0000 0111"
    );
    for addr in [0x1F, 0x21, 0x22, 0x23, 0x25, 0x26, 0x2E, 0x30, 0x38, 0x3F] {
        assert!(map.reset(addr).abs() < 1e-12, "0x{addr:02x} resets to 0x00");
    }
    for addr in [0x28, 0x2A, 0x2C] {
        assert_eq!(map.encoding(addr), Some(Encoding::I16Le), "0x{addr:02x}");
    }
    // The magnetometer slave (0x1E) and the registers 40h-6Fh are not part of this map.
    assert!((0x40..=0x6F).all(|addr| map.spec.sensor.registers.iter().all(|r| r.addr != addr)));
    for gap in ["magnetometer_slave_0x1e", "spi_register_map"] {
        assert!(model.coverage.missing.iter().any(|g| g == gap), "{gap}");
    }
    assert!(shas("LSM303AGR")
        .iter()
        .any(|sha| sha == "97635d4cc37b14b46a1ffdbda9349d2e75b9604248d03b77d4e1971171f813da"));
}

#[test]
fn lsm303agr_siblings_do_not_resolve_to_it() {
    assert_not_resolving(
        "lsm303agr",
        &[
            "LSM303DLHC",
            "LSM303DLH",
            "LSM303DLM",
            "LSM303D",
            "LSM303C",
            "LSM303AH",
            "LSM303AGR1",
            "LSM303",
        ],
    );
}

#[test]
fn icm20948_is_a_bank0_map_with_the_tight_vddio_rail() {
    for code in ["ICM-20948", "ICM20948", "icm-20948"] {
        card(code, "icm20948");
    }
    let model = card("ICM-20948", "icm20948");
    let map = map(&model);
    assert_eq!(map.spec.sensor.bus, Bus::I2c);
    // AD0 low is 1101000, high 1101001.
    assert_eq!(map.select_role.as_deref(), Some("ad0"));
    assert_eq!((map.when_low, map.when_high), (Some(0x68), Some(0x69)));
    assert_eq!(model.pins.get("9").map(String::as_str), Some("ad0"));
    assert_eq!(model.pins.len(), 24);
    assert_eq!(model.params.get_str("must_not_float_roles"), Some("ad0"));
    assert_eq!(map.constant(0x00), [0xEA], "WHO_AM_I");
    assert!(
        (map.stored_default(0x06) - 65.0).abs() < 1e-12,
        "PWR_MGMT_1 = 0x41"
    );
    assert_eq!(map.read_expr(0x06), "if(pm_reset == 1.0, 65.0, pwr_mgmt_1)");
    assert!((map.reset(0x05) - 64.0).abs() < 1e-12, "LP_CONFIG = 0x40");
    assert!(
        (map.reset(0x01) - 1.0).abs() < 1e-12,
        "GYRO_CONFIG_1 = 0x01"
    );
    assert!((map.reset(0x14) - 1.0).abs() < 1e-12, "ACCEL_CONFIG = 0x01");
    for addr in [0x03, 0x07, 0x0F, 0x10, 0x66, 0x69, 0x76, 0x7F] {
        assert!(map.reset(addr).abs() < 1e-12, "0x{addr:02x} resets to 0x00");
    }
    for addr in [0x2D, 0x2F, 0x31, 0x33, 0x35, 0x37, 0x39] {
        assert_eq!(map.encoding(addr), Some(Encoding::I16Be), "0x{addr:02x}");
    }
    // VDDIO is 1.71 V to 1.95 V with a 2.5 V absolute maximum, unlike VDD.
    let vddio = model
        .envelope
        .iter()
        .find_map(|e| match e {
            hauksbee_models::OperatingEnvelope::SupplyRange {
                pin,
                min_v,
                max_v,
                abs_max_v,
                ..
            } if pin == "vddio" => Some((*min_v, *max_v, *abs_max_v)),
            _ => None,
        })
        .expect("VDDIO envelope");
    assert!((vddio.0 - 1.71).abs() < 1e-12 && (vddio.1 - 1.95).abs() < 1e-12);
    assert!((vddio.2.expect("abs max") - 2.5).abs() < 1e-12);
    for gap in ["bank_qualified_registers", "magnetometer_ak09916"] {
        assert!(model.coverage.missing.iter().any(|g| g == gap), "{gap}");
    }
    assert!(shas("ICM-20948")
        .iter()
        .any(|sha| sha == "0588625f4f1d6d27382a4322f48073dc0ce569e537c63786b13aab3eeb76b718"));
}

#[test]
fn icm20948_siblings_do_not_resolve_to_it() {
    assert_not_resolving(
        "icm20948",
        &[
            "ICM-20649",
            "ICM-20600",
            "ICM-20602",
            "ICM-20689",
            "ICM-42605",
            "ICM-20948A",
            "MPU-9250",
            "MPU-9150",
        ],
    );
}

#[test]
fn ds3231_is_the_fixed_0x68_rtc_with_its_printed_reset_state() {
    for code in [
        "DS3231",
        "DS3231S",
        "DS3231SN",
        "DS3231SN#",
        "DS3231S#T&R",
        "ds3231sn+",
    ] {
        card(code, "ds3231");
    }
    let model = card("DS3231SN", "ds3231");
    let map = map(&model);
    assert_eq!(map.spec.sensor.bus, Bus::I2c);
    assert_eq!(map.address(), 0x68);
    assert!(map.select_role.is_none() && map.spec.sensor.address_selects.is_empty());
    assert_eq!(model.pins.len(), 16);
    assert_eq!(model.pins.get("15").map(String::as_str), Some("sda"));
    assert_eq!(model.pins.get("16").map(String::as_str), Some("scl"));
    assert_eq!(model.pins.get("2").map(String::as_str), Some("vcc"));
    assert_eq!(model.pins.get("13").map(String::as_str), Some("gnd"));
    // Figure 1: the time and date reset to 01/01/00 01 00:00:00.
    for (addr, value) in [
        (0x00, 0.0),
        (0x01, 0.0),
        (0x02, 0.0),
        (0x03, 1.0),
        (0x04, 1.0),
        (0x05, 1.0),
        (0x06, 0.0),
    ] {
        assert!((map.reset(addr) - value).abs() < 1e-12, "0x{addr:02x}");
    }
    // Control resets to 1Ch, status to OSF + EN32kHz (88h), aging to 00h.
    assert!((map.stored_default(0x0E) - 28.0).abs() < 1e-12);
    assert!((map.reset(0x0F) - 136.0).abs() < 1e-12);
    assert!(map.reset(0x10).abs() < 1e-12);
    // CONV self-clears in the read-back so a polling driver does not hang.
    assert_eq!(map.read_expr(0x0E), "control - 32.0 * conv");
    // Temperature: integer part at 11h, quarter degrees in the top bits of 12h.
    assert_eq!(map.encoding(0x11), Some(Encoding::U8));
    assert_eq!(map.encoding(0x12), Some(Encoding::U8));
    assert!(shas("DS3231")
        .iter()
        .any(|sha| sha == "dc81cd89bee82d0c57b654d6315e03dec913fb8fe753cc80b2a3f1cca71cbb34"));
    for gap in ["timekeeping_counting", "alarms", "ds3231m_variant"] {
        assert!(model.coverage.missing.iter().any(|g| g == gap), "{gap}");
    }
    // Open-drain SDA: no VOH, VOL 0.4 V.
    assert!(model.params.get_f64("voh").is_none());
    assert!((model.params.get_f64("vol").expect("vol") - 0.4).abs() < 1e-12);
}

#[test]
fn ds3231_siblings_do_not_resolve_to_it() {
    assert_not_resolving(
        "ds3231",
        &[
            "DS3231M", "DS3231MZ", "DS3232", "DS3234", "DS1307", "DS1337", "DS3231X", "DS32315",
        ],
    );
}

#[test]
fn ds1307_is_the_fixed_0x68_rtc_with_battery_ram() {
    for code in [
        "DS1307",
        "DS1307+",
        "DS1307N+",
        "DS1307Z+",
        "DS1307ZN+",
        "DS1307Z+T&R",
        "DS1307ZN+T&R",
        "ds1307z+",
    ] {
        card(code, "ds1307");
    }
    let model = card("DS1307Z+", "ds1307");
    let map = map(&model);
    assert_eq!(map.address(), 0x68);
    assert!(map.select_role.is_none() && map.spec.sensor.address_selects.is_empty());
    assert_eq!(model.pins.len(), 8);
    assert_eq!(model.pins.get("5").map(String::as_str), Some("sda"));
    assert_eq!(model.pins.get("6").map(String::as_str), Some("scl"));
    assert_eq!(model.pins.get("8").map(String::as_str), Some("vcc"));
    assert_eq!(model.pins.get("4").map(String::as_str), Some("gnd"));
    // Table 2: seconds reset with CH set (80h); the rest is 01/01/00 01.
    for (addr, value) in [
        (0x00, 128.0),
        (0x01, 0.0),
        (0x02, 0.0),
        (0x03, 1.0),
        (0x04, 1.0),
        (0x05, 1.0),
        (0x06, 0.0),
        (0x07, 3.0),
    ] {
        assert!((map.reset(addr) - value).abs() < 1e-12, "0x{addr:02x}");
    }
    // 56 bytes of RAM at 08h to 3Fh, and nothing past it.
    for addr in 0x08..=0x3F {
        assert!(map.reset(addr).abs() < 1e-12, "RAM 0x{addr:02x}");
    }
    assert!(!map
        .spec
        .sensor
        .write_registers
        .iter()
        .any(|w| w.addr > 0x3F));
    // The 5 V part's inputs are TTL-level and VCC must be 4.5 V to 5.5 V.
    assert!((model.params.get_f64("vih").expect("vih") - 2.2).abs() < 1e-12);
    assert!((model.params.get_f64("vil").expect("vil") - 0.8).abs() < 1e-12);
    let vcc = model
        .envelope
        .iter()
        .find_map(|e| match e {
            hauksbee_models::OperatingEnvelope::SupplyRange {
                pin, min_v, max_v, ..
            } if pin == "vcc" => Some((*min_v, *max_v)),
            _ => None,
        })
        .expect("VCC envelope");
    assert!((vcc.0 - 4.5).abs() < 1e-12 && (vcc.1 - 5.5).abs() < 1e-12);
    assert!(shas("DS1307")
        .iter()
        .any(|sha| sha == "c81233053aa1da1571258d117bf4ce7bcef8f6225e6895259add55809189516a"));
}

#[test]
fn ds1307_siblings_do_not_resolve_to_it() {
    assert_not_resolving(
        "ds1307",
        &[
            "DS1302", "DS1305", "DS1307M", "DS13071", "DS1337", "DS1338", "DS1340", "DS3231",
            "DS1307ZZ",
        ],
    );
}

#[test]
fn ina226_has_sixteen_strapped_addresses_and_the_printed_identity() {
    for code in ["INA226", "INA226AIDGSR", "INA226AIDGST", "ina226aidgsr"] {
        card(code, "ina226");
    }
    let model = card("INA226", "ina226");
    let map = map(&model);
    assert_eq!(map.spec.sensor.bus, Bus::I2c);
    // Table 6-2: four levels on each of A1 and A0 select 0x40 to 0x4F.
    let selects = &map.spec.sensor.address_selects;
    assert_eq!(selects.len(), 16);
    let mut addresses: Vec<u8> = selects.iter().map(|row| row.address).collect();
    addresses.sort_unstable();
    assert_eq!(addresses, (0x40..=0x4F).collect::<Vec<u8>>());
    let row = |a1: &str, a0: &str| {
        selects
            .iter()
            .find(|row| {
                format!("{:?}", row.straps.get("a1"))
                    .to_lowercase()
                    .contains(a1)
                    && format!("{:?}", row.straps.get("a0"))
                        .to_lowercase()
                        .contains(a0)
            })
            .map(|row| row.address)
    };
    assert_eq!(row("low", "low"), Some(0x40));
    assert_eq!(row("low", "high"), Some(0x41));
    assert_eq!(row("high", "low"), Some(0x44));
    assert_eq!(row("sda", "scl"), Some(0x4B));
    assert_eq!(row("scl", "sda"), Some(0x4E));
    assert_eq!(row("scl", "scl"), Some(0x4F));
    assert_eq!(map.address(), 0x40);
    assert_eq!(model.pins.len(), 10);
    assert_eq!(model.pins.get("1").map(String::as_str), Some("a1"));
    assert_eq!(model.pins.get("2").map(String::as_str), Some("a0"));
    assert_eq!(model.pins.get("6").map(String::as_str), Some("vs"));
    assert_eq!(model.params.get_str("must_not_float_roles"), Some("a0,a1"));
    // Table 7-1.
    assert_eq!(map.constant(0xFE), [0x54, 0x49], "manufacturer ID 5449h");
    assert_eq!(map.constant(0xFF), [0x22, 0x60], "die ID 2260h");
    assert!((map.stored_default(0x00) - f64::from(0x4127)).abs() < 1e-12);
    assert_eq!(map.read_expr(0x00), "if(rst == 1.0, 16679.0, config)");
    for addr in [0x05, 0x06, 0x07] {
        assert!(map.stored_default(addr).abs() < 1e-12, "0x{addr:02x}");
    }
    for addr in [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07] {
        let want = if addr == 0x01 || addr == 0x04 {
            Encoding::I16Be
        } else {
            Encoding::U16Be
        };
        assert_eq!(map.encoding(addr), Some(want), "0x{addr:02x}");
    }
    assert!((model.params.get_f64("vol").expect("vol") - 0.4).abs() < 1e-12);
    assert!(model.params.get_f64("voh").is_none());
    assert!((model.ratings.theta_ja_c_per_w.expect("theta ja") - 144.6).abs() < 1e-12);
    assert!(shas("INA226")
        .iter()
        .any(|sha| sha == "c9b67f886d4a5241a5e070723f7b61867409eeb27eed768b9cdd9cb17e03ca2d"));
}

#[test]
fn ina226_siblings_do_not_resolve_to_it() {
    assert_not_resolving(
        "ina226",
        &[
            "INA219", "INA220", "INA225", "INA228", "INA229", "INA230", "INA231", "INA233",
            "INA2260", "INA226Q1",
        ],
    );
    // The existing INA228 card is untouched.
    card("INA228", "ina228");
}

fn acs712_id(code: &str) -> &'static str {
    if code.contains("05B") || code.contains("05b") {
        "acs712_05b"
    } else if code.contains("20A") || code.contains("20a") {
        "acs712_20a"
    } else {
        "acs712_30a"
    }
}

#[test]
fn acs712_variants_are_three_analog_cards_with_their_own_sensitivity() {
    let variants: [(&[&str], f64); 3] = [
        (
            &[
                "ACS712ELCTR-05B-T",
                "ACS712-05B",
                "ACS712ELC-05B-T",
                "acs712elctr-05b-t",
            ],
            185.0,
        ),
        (
            &[
                "ACS712ELCTR-20A-T",
                "ACS712ELCTR-20A-S",
                "ACS712-20A",
                "ACS712ELC-20A",
            ],
            100.0,
        ),
        (
            &[
                "ACS712ELCTR-30A-T",
                "ACS712ELCTR-30A-S",
                "ACS712-30A",
                "ACS712ELC-30A",
            ],
            66.0,
        ),
    ];
    for (codes, sensitivity) in variants {
        let id = acs712_id(codes[0]);
        for code in codes {
            let model = card(code, id);
            // No bus model: the part is an analog output.
            assert!(model.peripheral.is_none(), "{id}");
            assert_eq!(model.pins.len(), 8);
            assert_eq!(model.pins.get("7").map(String::as_str), Some("viout"));
            assert_eq!(model.pins.get("8").map(String::as_str), Some("vcc"));
            assert_eq!(model.pins.get("5").map(String::as_str), Some("gnd"));
            assert_eq!(model.pins.get("1").map(String::as_str), Some("ip_p1"));
            assert_eq!(model.pins.get("4").map(String::as_str), Some("ip_n2"));
            assert!(
                (model.params.get_f64("supply_static_ua").expect("icc") - 10000.0).abs() < 1e-9
            );
            assert!((model.ratings.max_voltage_v.expect("8 V") - 8.0).abs() < 1e-12);
            let vcc = model
                .envelope
                .iter()
                .find_map(|e| match e {
                    hauksbee_models::OperatingEnvelope::SupplyRange {
                        pin, min_v, max_v, ..
                    } if pin == "vcc" => Some((*min_v, *max_v)),
                    _ => None,
                })
                .expect("VCC envelope");
            assert!((vcc.0 - 4.5).abs() < 1e-12 && (vcc.1 - 5.5).abs() < 1e-12);
            assert!(model
                .coverage
                .missing
                .iter()
                .any(|g| g == "analog_output_vout_vs_current"));
            // The printed sensitivity of this variant, and not a sibling's.
            assert!(
                model
                    .description
                    .contains(&format!("{sensitivity:.0} mV/A")),
                "{code}: {}",
                model.description
            );
        }
        assert!(shas(codes[0])
            .iter()
            .any(|sha| sha == "21b345f7a98d36947d5562218b0bc730b80609b614e9fc4374f56993011466b9"));
    }
}

#[test]
fn acs712_siblings_do_not_resolve_to_any_variant() {
    for sibling in [
        "ACS712",
        "ACS712ELCTR",
        "ACS712-10A",
        "ACS712ELCTR-05B-S",
        "ACS711",
        "ACS714",
        "ACS723",
        "ACS724",
        "ACS758",
        "ACS712-05B-X",
    ] {
        assert!(
            resolve_part(sibling).is_none_or(|model| !model.id.starts_with("acs712")),
            "{sibling} must not resolve to an ACS712 card"
        );
    }
}
