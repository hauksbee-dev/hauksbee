//! Official-parts batch io_drivers: resolution and behaviour tests for the cards added in that batch.

use super::resolve_query;

fn resolve(value: &str, footprint: Option<&str>) -> hauksbee_models::Resolution {
    resolve_query(Some(value), None, footprint)
}

const TXS0102_SHA: &str = "75397ef71b07af6e47b81441a502a224c59b6ac359083d71516465f0c885eaf7";
const TXS0104E_SHA: &str = "88295316d8d520f1b967af2ce9c7dc4b7491c1e647968728b9690c6514fb7b41";
const LVC245A_SHA: &str = "ad3e705aef834214404d6e1e38cb00f95fdb6f0d9807a784a6f58c4d060f13a3";

fn io_model_id(value: &str) -> Option<String> {
    resolve(value, None).model.map(|m| m.id)
}

fn io_has_sha(value: &str, sha: &str) -> bool {
    resolve(value, None)
        .references
        .iter()
        .any(|reference| reference.sha256.as_deref() == Some(sha))
}

#[test]
fn io_txs0102_resolves_per_package_with_its_own_pin_map() {
    let cases = [
        ("TXS0102DCTR", "txs0102_dct", "1", "b2"),
        ("TXS0102DCUR", "txs0102_dcu", "5", "a1"),
        ("TXS0102DTTR", "txs0102_dtt", "7", "vccb"),
        ("TXS0102DQER", "txs0102_dqe", "1", "vcca"),
        ("TXS0102DQMR", "txs0102_dqm", "4", "gnd"),
        ("TXS0102YZPR", "txs0102_yzp", "D2", "a1"),
    ];
    for (value, id, pad, role) in cases {
        let resolution = resolve(value, None);
        let model = resolution.model.as_ref().expect(value);
        assert_eq!(model.id, id, "{value}");
        assert_eq!(
            model.pins.get(pad).map(String::as_str),
            Some(role),
            "{value}"
        );
        assert_eq!(model.pins.len(), 8, "{value}");
        assert!(io_has_sha(value, TXS0102_SHA), "{value}");
        assert!(
            !model.logic.outputs.is_empty(),
            "{value}: auto-direction logic"
        );
    }
    for sibling in [
        "TXS0102",
        "TXS0108E",
        "TXS0104EPWR",
        "TXS01022",
        "TXB0102DCUR",
    ] {
        let id = io_model_id(sibling);
        assert!(
            id.as_deref().is_none_or(|id| !id.starts_with("txs0102_")),
            "{sibling} must not resolve to a TXS0102 card"
        );
    }
}

#[test]
fn io_txs0104e_resolves_per_package_with_its_own_pin_map() {
    let cases = [
        ("TXS0104EDR", "txs0104e_d", "1", "vcca"),
        ("TXS0104EPWR", "txs0104e_pw", "14", "vccb"),
        ("TXS0104ERGYR", "txs0104e_rgy", "8", "oe"),
        ("TXS0104EBQAR", "txs0104e_bqa", "13", "b1"),
        ("TXS0104ERUTR", "txs0104e_rut", "12", "oe"),
        ("TXS0104EYZTR", "txs0104e_yzt", "A3", "a1"),
        ("TXS0104EZXUR", "txs0104e_zxu", "B4", "gnd"),
        ("TXS0104ENMNR", "txs0104e_nmn", "B3", "oe"),
    ];
    for (value, id, pad, role) in cases {
        let resolution = resolve(value, None);
        let model = resolution.model.as_ref().expect(value);
        assert_eq!(model.id, id, "{value}");
        assert_eq!(
            model.pins.get(pad).map(String::as_str),
            Some(role),
            "{value}"
        );
        assert!(io_has_sha(value, TXS0104E_SHA), "{value}");
        assert!(
            !model.logic.outputs.is_empty(),
            "{value}: auto-direction logic"
        );
    }
    for sibling in ["TXS0104", "TXS0108EPWR", "TXS0102DCUR"] {
        let id = io_model_id(sibling);
        assert!(
            id.as_deref().is_none_or(|id| !id.starts_with("txs0104e_")),
            "{sibling} must not resolve to a TXS0104E card"
        );
    }
}

#[test]
fn io_sn74lvc245a_resolves_per_package_and_family_name_has_no_theta() {
    let cases = [
        ("SN74LVC245ADBR", "sn74lvc245a_db"),
        ("SN74LVC245ADGVR", "sn74lvc245a_dgv"),
        ("SN74LVC245ADWR", "sn74lvc245a_dw"),
        ("SN74LVC245ADWRG4", "sn74lvc245a_dw"),
        ("SN74LVC245AN", "sn74lvc245a_n"),
        ("SN74LVC245ANE4", "sn74lvc245a_n"),
        ("SN74LVC245ANSR", "sn74lvc245a_ns"),
        ("SN74LVC245APWR", "sn74lvc245a_pw"),
        ("SN74LVC245ARGYR", "sn74lvc245a_rgy"),
        ("SN74LVC245AZQNR", "sn74lvc245a_gqn_zqn"),
        ("SN74LVC245A", "74lvc245a_family"),
        ("74LVC245", "74lvc245a_family"),
        ("74LVC245A", "74lvc245a_family"),
    ];
    for (value, id) in cases {
        let resolution = resolve(value, None);
        let model = resolution.model.as_ref().expect(value);
        assert_eq!(model.id, id, "{value}");
        assert!(io_has_sha(value, LVC245A_SHA), "{value}");
        assert_eq!(model.pins.len(), 20, "{value}");
        assert!(!model.logic.outputs.is_empty(), "{value}");
        assert!(!model.envelope.is_empty(), "{value}");
    }
    let family = resolve("74LVC245A", None).model.expect("family card");
    assert!(
        family.ratings.theta_ja_c_per_w.is_none(),
        "family has no package theta"
    );
    let pw = resolve("SN74LVC245APWR", None).model.expect("PW card");
    assert_eq!(pw.pins.get("2").map(String::as_str), Some("a1"));
    assert_eq!(pw.pins.get("18").map(String::as_str), Some("b1"));
    assert_eq!(pw.pins.get("19").map(String::as_str), Some("oe_n"));
    for sibling in [
        "SN74LVC2245A",
        "74LVC2G245",
        "SN74LVCC245A",
        "SN74LVC244APWR",
        "74HCT245",
    ] {
        let id = io_model_id(sibling);
        assert!(
            id.as_deref().is_none_or(|id| !id.contains("lvc245a")),
            "{sibling} must not resolve to an LVC245A card"
        );
    }
}

const A4988_SHA: &str = "d1dfba3bb7be2cbf2b689f39424569fc78b1f4fd1f427648bbd8f291ebb933ea";
const DRV8825_SHA: &str = "5b4dccc2497334e10574ffa5a55518c8d59b0ed35758f5e8f099b84bbfbb407b";
const DRV8833_SHA: &str = "3e400e333f08f1b93ec99a2d52a6af2a864e88462cc892282208548d9e9f0de4";
const TB6612_SHA: &str = "928865fade9c7c18bd11ae5c4d3ac68a56ead028239669d5e5e9d6b087f716de";

fn io_assert_card(value: &str, id: &str, sha: &str, pads: &[(&str, &str)]) {
    let resolution = resolve(value, None);
    let model = resolution.model.as_ref().expect(value);
    assert_eq!(model.id, id, "{value}");
    for (pad, role) in pads {
        assert_eq!(
            model.pins.get(*pad).map(String::as_str),
            Some(*role),
            "{value} pad {pad}"
        );
    }
    assert!(
        resolution
            .references
            .iter()
            .any(|reference| reference.sha256.as_deref() == Some(sha)),
        "{value}: datasheet sha256"
    );
    assert!(!model.envelope.is_empty(), "{value}: operating envelope");
}

#[test]
fn io_motors_a4988_resolves_its_qfn_card() {
    for value in ["A4988", "A4988SETTR-T", "A4988SET-T"] {
        io_assert_card(
            value,
            "a4988_et_qfn28",
            A4988_SHA,
            &[
                ("22", "vbb1"),
                ("28", "vbb2"),
                ("15", "vdd"),
                ("16", "step"),
                ("19", "dir"),
                ("9", "ms1"),
                ("29", "gnd_ep"),
            ],
        );
    }
    let model = resolve("A4988", None).model.expect("a4988");
    assert_eq!(model.ratings.theta_ja_c_per_w, Some(32.0));
    assert_eq!(model.ratings.max_voltage_v, Some(35.0));
    for sibling in ["A4989", "A4950", "A49880", "DRV8825"] {
        let id = resolve(sibling, None).model.map(|m| m.id);
        assert!(
            id.as_deref() != Some("a4988_et_qfn28"),
            "{sibling} is not an A4988"
        );
    }
}

#[test]
fn io_motors_drv8825_resolves_its_htssop_card() {
    for value in ["DRV8825", "DRV8825PWP", "DRV8825PWPR", "DRV8825PWPRG4"] {
        io_assert_card(
            value,
            "drv8825_pwp28",
            DRV8825_SHA,
            &[
                ("4", "vma"),
                ("11", "vmb"),
                ("14", "gnd"),
                ("28", "gnd"),
                ("22", "step"),
                ("20", "dir"),
                ("21", "nenbl"),
            ],
        );
    }
    let model = resolve("DRV8825", None).model.expect("drv8825");
    assert_eq!(model.ratings.theta_ja_c_per_w, Some(31.6));
    assert_eq!(model.ratings.max_voltage_v, Some(47.0));
    for sibling in ["DRV8824", "DRV8826", "DRV88250"] {
        let id = resolve(sibling, None).model.map(|m| m.id);
        assert!(
            id.as_deref() != Some("drv8825_pwp28"),
            "{sibling} is not a DRV8825"
        );
    }
}

#[test]
fn io_motors_drv8833_resolves_one_card_per_package() {
    io_assert_card(
        "DRV8833PWPR",
        "drv8833_pwp16",
        DRV8833_SHA,
        &[
            ("12", "vm"),
            ("13", "gnd"),
            ("17", "gnd_ep"),
            ("1", "nsleep"),
            ("16", "ain1"),
        ],
    );
    io_assert_card(
        "DRV8833RTYR",
        "drv8833_rty16",
        DRV8833_SHA,
        &[
            ("10", "vm"),
            ("11", "gnd"),
            ("17", "gnd_ep"),
            ("15", "nsleep"),
            ("14", "ain1"),
        ],
    );
    io_assert_card(
        "DRV8833PWR",
        "drv8833_pw16",
        DRV8833_SHA,
        &[("12", "vm"), ("13", "gnd"), ("1", "nsleep"), ("16", "ain1")],
    );
    let thetas = [
        ("DRV8833PWPR", 40.5),
        ("DRV8833RTYR", 37.2),
        ("DRV8833PWR", 103.1),
    ];
    for (value, theta) in thetas {
        let model = resolve(value, None).model.expect(value);
        assert_eq!(model.ratings.theta_ja_c_per_w, Some(theta), "{value}");
        assert_eq!(model.ratings.max_voltage_v, Some(11.8), "{value}");
    }
    for sibling in ["DRV8833", "DRV8832", "DRV8837", "DRV8833PWPX"] {
        let id = resolve(sibling, None).model.map(|m| m.id);
        assert!(
            id.as_deref().is_none_or(|id| !id.starts_with("drv8833_")),
            "{sibling} names no DRV8833 package"
        );
    }
}

#[test]
fn io_motors_tb6612fng_resolves_its_ssop_card() {
    for value in ["TB6612FNG", "TB6612FNG(O,C,8,EL)"] {
        io_assert_card(
            value,
            "tb6612fng_ssop24",
            TB6612_SHA,
            &[
                ("20", "vcc"),
                ("18", "gnd"),
                ("19", "stby"),
                ("24", "vm1"),
                ("13", "vm2"),
                ("14", "vm3"),
                ("23", "pwma"),
                ("1", "ao1"),
            ],
        );
    }
    let model = resolve("TB6612FNG", None).model.expect("tb6612fng");
    assert_eq!(model.ratings.max_voltage_v, Some(15.0));
    for sibling in ["TB6612", "TB67H450", "TB6612FNG2"] {
        let id = resolve(sibling, None).model.map(|m| m.id);
        assert!(
            id.as_deref() != Some("tb6612fng_ssop24"),
            "{sibling} is not a TB6612FNG"
        );
    }
}

// Fragment: tests for the Microchip I/O expander cards (MCP23017, MCP23S17, MCP23008).
// Written without running cargo; the lead compiles and runs them. Needs `resolve(value, footprint)` in scope.

const IOD_EXP_SHA_17: &str = "63cb5f2bec44434cdeeada1790d0316c9dc06b33febb489ad87bb0e2d540496a";
const IOD_EXP_SHA_08: &str = "3516fba5df6da78c2e7a58defaf90e4bdb9fb71f904e56b74b502fab0b4d112c";

const IOD_EXP_FP_28_LEADED: [&str; 4] = [
    "Package_SO:SOIC-28W_7.5x17.9mm_P1.27mm",
    "Package_DIP:DIP-28_W7.62mm",
    "Package_SSOP:SSOP-28_5.3x10.2mm_P0.65mm",
    "SOIC127P1030X265-28N",
];
const IOD_EXP_FP_28_QFN: &str = "Package_DFN_QFN:QFN-28-1EP_6x6mm_P0.65mm_EP4.25x4.25mm";
const IOD_EXP_FP_18: [&str; 2] = [
    "Package_SO:SOIC-18W_7.5x11.6mm_P1.27mm",
    "Package_DIP:DIP-18_W7.62mm",
];
const IOD_EXP_FP_SSOP20: &str = "Package_SSOP:SSOP-20_5.3x7.2mm_P0.65mm";
const IOD_EXP_FP_QFN20: &str = "Package_DFN_QFN:QFN-20-1EP_4x4mm_P0.5mm_EP2.5x2.5mm";

fn iod_exp_model(value: &str, footprint: &str) -> hauksbee_models::ModelEntry {
    resolve(value, Some(footprint))
        .model
        .unwrap_or_else(|| panic!("{value} on {footprint} should resolve"))
}

fn iod_exp_has_sha(value: &str, footprint: &str, sha: &str) -> bool {
    resolve(value, Some(footprint))
        .references
        .iter()
        .any(|reference| reference.sha256.as_deref() == Some(sha))
}

fn iod_exp_spec(model: &hauksbee_models::ModelEntry) -> hauksbee_models::sensor_spec::SensorSpec {
    match model.peripheral.as_ref().expect("register-map peripheral") {
        hauksbee_models::PeripheralSpec::RegisterMap { spec_toml, .. } => {
            hauksbee_models::sensor_spec::SensorSpec::from_toml(spec_toml)
                .expect("spec_toml parses and validates")
        }
        other => panic!("expected a register map, got {other:?}"),
    }
}

fn iod_exp_pin<'a>(model: &'a hauksbee_models::ModelEntry, pad: &str) -> &'a str {
    model
        .pins
        .get(pad)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("{} has no pad {pad}", model.id))
}

/// Shared register-file assertions: I2C address 0x20 with the eight A2:A0 straps, the reset values
/// of the write registers (IODIR = 0xFF, everything else 0), the declared read addresses, and the
/// IOCON bit fields kept as stored fields.
fn iod_exp_check_register_file(
    model: &hauksbee_models::ModelEntry,
    iodir_addrs: &[u8],
    read_addrs: &[u8],
    gpio_inputs: &[&str],
    iocon_addr: u8,
    iocon_fields: usize,
) {
    let spec = iod_exp_spec(model).sensor;
    assert_eq!(spec.i2c_address, Some(0x20), "{}", model.id);
    assert_eq!(spec.address_selects.len(), 8, "{}", model.id);
    let mut addresses: Vec<u8> = spec.address_selects.iter().map(|row| row.address).collect();
    addresses.sort_unstable();
    assert_eq!(
        addresses,
        (0x20..=0x27).collect::<Vec<u8>>(),
        "{}",
        model.id
    );
    for row in &spec.address_selects {
        assert_eq!(row.straps.len(), 3, "{}: a2, a1 and a0", model.id);
    }
    let input_names: Vec<&str> = spec.inputs.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(input_names, gpio_inputs, "{}", model.id);
    assert!(spec.inputs.iter().all(|i| i.default == 0.0), "{}", model.id);
    for write in &spec.write_registers {
        let want = if iodir_addrs.contains(&write.addr) {
            255.0
        } else {
            0.0
        };
        assert_eq!(
            write.default, want,
            "{} write register 0x{:02X}",
            model.id, write.addr
        );
    }
    let iocon = spec
        .write_registers
        .iter()
        .find(|w| w.addr == iocon_addr)
        .expect("IOCON write register");
    assert_eq!(iocon.fields.len(), iocon_fields, "{}", model.id);
    let mut got: Vec<u8> = spec.registers.iter().map(|r| r.addr).collect();
    got.sort_unstable();
    assert_eq!(got, read_addrs, "{} read registers", model.id);
    // INTF and INTCAP are read-only const zeros; GPIO is an input-driven read.
    assert!(spec
        .registers
        .iter()
        .any(|r| r.r#const.as_deref() == Some(&[0u8][..])));
    assert!(spec
        .registers
        .iter()
        .any(|r| r.expr.as_deref().is_some_and(|e| gpio_inputs.contains(&e))));
}

fn iod_exp_check_common(model: &hauksbee_models::ModelEntry, with_peripheral: bool) {
    assert_eq!(model.kind, hauksbee_models::ComponentKind::Digital);
    assert_eq!(model.params.get_f64("vol"), Some(0.6), "{}", model.id);
    assert_eq!(
        model.params.get_str("must_not_float_roles"),
        Some("reset_n,a0,a1,a2"),
        "{}",
        model.id
    );
    assert!(model.levels.contains_key("vih") && model.levels.contains_key("vil"));
    assert!(model.levels.contains_key("voh"), "{}", model.id);
    assert!(!model.envelope.is_empty(), "{}", model.id);
    assert!(model
        .coverage
        .missing
        .iter()
        .any(|m| m == "gpio_pin_outputs"));
    assert_eq!(model.peripheral.is_some(), with_peripheral, "{}", model.id);
    assert_eq!(
        model.peripheral_power.is_some(),
        with_peripheral,
        "{}",
        model.id
    );
    if let Some(power) = &model.peripheral_power {
        assert_eq!(power.supply_role, "vdd");
        assert_eq!(power.return_role, "gnd");
        assert_eq!(power.power_on_threshold_v, 1.8);
        assert_eq!(power.read_a, 0.001);
        assert_eq!(power.idle_a, 0.000001);
    }
}

const IOD_EXP_16BIT_READS: [u8; 22] = [
    0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15,
];

#[test]
fn iod_exp_mcp23017_leaded_28_resolves_with_its_pin_map_and_register_file() {
    for value in [
        "MCP23017",
        "MCP23017-E/SO",
        "MCP23017T-E/SO",
        "MCP23017-E/SP",
        "MCP23017-E/SS",
        "MCP23017T-E/SS",
        "MCP23017-E/SSVAO",
        "MCP23017T-E/SSVAO",
    ] {
        for footprint in IOD_EXP_FP_28_LEADED {
            let model = iod_exp_model(value, footprint);
            assert_eq!(
                model.id, "mcp23017_spdip_soic_ssop28",
                "{value} {footprint}"
            );
            assert!(iod_exp_has_sha(value, footprint, IOD_EXP_SHA_17), "{value}");
        }
    }
    let model = iod_exp_model("MCP23017-E/SO", IOD_EXP_FP_28_LEADED[0]);
    assert_eq!(model.pins.len(), 28);
    for (pad, role) in [
        ("1", "gpb0"),
        ("8", "gpb7"),
        ("9", "vdd"),
        ("10", "gnd"),
        ("11", "nc"),
        ("12", "scl"),
        ("13", "sda"),
        ("14", "nc"),
        ("15", "a0"),
        ("16", "a1"),
        ("17", "a2"),
        ("18", "reset_n"),
        ("19", "intb"),
        ("20", "inta"),
        ("21", "gpa0"),
        ("28", "gpa7"),
    ] {
        assert_eq!(iod_exp_pin(&model, pad), role, "pad {pad}");
    }
    assert_eq!(model.params.get_str("supply_pin"), Some("9"));
    assert_eq!(model.params.get_str("gnd_pin"), Some("10"));
    iod_exp_check_common(&model, true);
    iod_exp_check_register_file(
        &model,
        &[0x00, 0x01],
        &IOD_EXP_16BIT_READS,
        &["gpioa", "gpiob"],
        0x0A,
        7,
    );
}

#[test]
fn iod_exp_mcp23017_qfn_28_has_its_own_pin_map() {
    for value in ["MCP23017", "MCP23017-E/ML", "MCP23017T-E/ML"] {
        let model = iod_exp_model(value, IOD_EXP_FP_28_QFN);
        assert_eq!(model.id, "mcp23017_qfn28", "{value}");
        assert!(iod_exp_has_sha(value, IOD_EXP_FP_28_QFN, IOD_EXP_SHA_17));
    }
    let model = iod_exp_model("MCP23017-E/ML", IOD_EXP_FP_28_QFN);
    assert_eq!(model.pins.len(), 29);
    for (pad, role) in [
        ("1", "gpb4"),
        ("4", "gpb7"),
        ("5", "vdd"),
        ("6", "gnd"),
        ("7", "nc"),
        ("8", "scl"),
        ("9", "sda"),
        ("10", "nc"),
        ("11", "a0"),
        ("13", "a2"),
        ("14", "reset_n"),
        ("15", "intb"),
        ("16", "inta"),
        ("17", "gpa0"),
        ("24", "gpa7"),
        ("25", "gpb0"),
        ("28", "gpb3"),
        ("29", "gnd_ep"),
    ] {
        assert_eq!(iod_exp_pin(&model, pad), role, "pad {pad}");
    }
    assert_eq!(model.params.get_str("supply_pin"), Some("5"));
    assert_eq!(model.params.get_str("gnd_pin"), Some("6"));
    iod_exp_check_common(&model, true);
    iod_exp_check_register_file(
        &model,
        &[0x00, 0x01],
        &IOD_EXP_16BIT_READS,
        &["gpioa", "gpiob"],
        0x0A,
        7,
    );
    // The two MCP23017 packages must not share a pin map.
    let leaded = iod_exp_model("MCP23017-E/SO", IOD_EXP_FP_28_LEADED[0]);
    assert_ne!(leaded.pins.get("1"), model.pins.get("1"));
}

#[test]
fn iod_exp_mcp23017_register_values_match_the_reset_table() {
    let model = iod_exp_model("MCP23017-E/SO", IOD_EXP_FP_28_LEADED[0]);
    let spec = iod_exp_spec(&model).sensor;
    let by_addr = |addr: u8| {
        spec.write_registers
            .iter()
            .find(|w| w.addr == addr)
            .unwrap_or_else(|| panic!("write register 0x{addr:02X}"))
    };
    assert_eq!(by_addr(0x00).store, "iodira");
    assert_eq!(by_addr(0x01).store, "iodirb");
    assert_eq!(by_addr(0x0C).store, "gppua");
    assert_eq!(by_addr(0x14).store, "olata");
    assert_eq!(by_addr(0x15).store, "olatb");
    // IOCON.BANK = 0 means IOCON is the single register printed at 0A and 0B; GPIO is not a stored register.
    assert!(spec
        .write_registers
        .iter()
        .all(|w| w.addr != 0x12 && w.addr != 0x13));
    let read_expr = |addr: u8| {
        spec.registers
            .iter()
            .find(|r| r.addr == addr)
            .and_then(|r| r.expr.clone())
    };
    assert_eq!(read_expr(0x12).as_deref(), Some("gpioa"));
    assert_eq!(read_expr(0x13).as_deref(), Some("gpiob"));
    assert_eq!(read_expr(0x0A), read_expr(0x0B));
}

#[test]
fn iod_exp_mcp23s17_has_pin_maps_and_no_register_file() {
    for value in [
        "MCP23S17",
        "MCP23S17-E/SO",
        "MCP23S17T-E/SO",
        "MCP23S17-E/SP",
        "MCP23S17-E/SS",
        "MCP23S17-E/SSVAO",
    ] {
        for footprint in IOD_EXP_FP_28_LEADED {
            let model = iod_exp_model(value, footprint);
            assert_eq!(
                model.id, "mcp23s17_spdip_soic_ssop28",
                "{value} {footprint}"
            );
            assert!(iod_exp_has_sha(value, footprint, IOD_EXP_SHA_17));
        }
    }
    let leaded = iod_exp_model("MCP23S17-E/SO", IOD_EXP_FP_28_LEADED[0]);
    for (pad, role) in [
        ("11", "cs"),
        ("12", "sck"),
        ("13", "mosi"),
        ("14", "miso"),
        ("9", "vdd"),
        ("10", "gnd"),
        ("18", "reset_n"),
    ] {
        assert_eq!(iod_exp_pin(&leaded, pad), role, "pad {pad}");
    }
    assert_eq!(leaded.pins.len(), 28);
    iod_exp_check_common(&leaded, false);
    assert!(leaded
        .coverage
        .missing
        .iter()
        .any(|m| m == "spi_two_byte_header"));

    for value in ["MCP23S17", "MCP23S17-E/ML", "MCP23S17T-E/ML"] {
        let model = iod_exp_model(value, IOD_EXP_FP_28_QFN);
        assert_eq!(model.id, "mcp23s17_qfn28", "{value}");
    }
    let qfn = iod_exp_model("MCP23S17-E/ML", IOD_EXP_FP_28_QFN);
    for (pad, role) in [
        ("7", "cs"),
        ("8", "sck"),
        ("9", "mosi"),
        ("10", "miso"),
        ("5", "vdd"),
        ("6", "gnd"),
        ("29", "gnd_ep"),
    ] {
        assert_eq!(iod_exp_pin(&qfn, pad), role, "pad {pad}");
    }
    assert_eq!(qfn.pins.len(), 29);
    iod_exp_check_common(&qfn, false);
}

#[test]
fn iod_exp_mcp23008_resolves_per_package_with_its_own_pin_map() {
    let cases: [(&str, &str, &str, &[(&str, &str)]); 3] = [
        (
            "MCP23008-E/SO",
            IOD_EXP_FP_18[0],
            "mcp23008_pdip_soic18",
            &[
                ("1", "scl"),
                ("2", "sda"),
                ("3", "a2"),
                ("4", "a1"),
                ("5", "a0"),
                ("6", "reset_n"),
                ("7", "nc"),
                ("8", "int"),
                ("9", "gnd"),
                ("10", "gp0"),
                ("17", "gp7"),
                ("18", "vdd"),
            ],
        ),
        (
            "MCP23008T-E/SS",
            IOD_EXP_FP_SSOP20,
            "mcp23008_ssop20",
            &[
                ("1", "scl"),
                ("2", "sda"),
                ("8", "int"),
                ("9", "gnd"),
                ("10", "nc"),
                ("11", "nc"),
                ("12", "gp0"),
                ("19", "gp7"),
                ("20", "vdd"),
            ],
        ),
        (
            "MCP23008-E/ML",
            IOD_EXP_FP_QFN20,
            "mcp23008_qfn20",
            &[
                ("1", "a2"),
                ("3", "a0"),
                ("4", "reset_n"),
                ("5", "nc"),
                ("6", "nc"),
                ("7", "int"),
                ("8", "nc"),
                ("9", "gp0"),
                ("16", "gp7"),
                ("17", "gnd"),
                ("18", "vdd"),
                ("19", "scl"),
                ("20", "sda"),
            ],
        ),
    ];
    for (value, footprint, id, pads) in cases {
        let model = iod_exp_model(value, footprint);
        assert_eq!(model.id, id, "{value}");
        assert!(iod_exp_has_sha(value, footprint, IOD_EXP_SHA_08), "{value}");
        for (pad, role) in pads {
            assert_eq!(iod_exp_pin(&model, pad), *role, "{id} pad {pad}");
        }
        assert_eq!(
            model.pins.len(),
            if id == "mcp23008_pdip_soic18" { 18 } else { 20 }
        );
        iod_exp_check_common(&model, true);
        // 8-bit register file: eleven registers 00h to 0Ah, IODIR = 0xFF, IOCON bits 5:1 stored.
        iod_exp_check_register_file(
            &model,
            &[0x00],
            &[
                0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A,
            ],
            &["gpio"],
            0x05,
            5,
        );
    }
    // The other orderable spellings of the 18-pin package resolve to the same card.
    for (value, footprint) in [
        ("MCP23008-E/P", IOD_EXP_FP_18[1]),
        ("MCP23008T-E/SO", IOD_EXP_FP_18[0]),
        ("MCP23008", IOD_EXP_FP_18[0]),
    ] {
        assert_eq!(iod_exp_model(value, footprint).id, "mcp23008_pdip_soic18");
    }
    assert_eq!(
        iod_exp_model("MCP23008", IOD_EXP_FP_SSOP20).id,
        "mcp23008_ssop20"
    );
    assert_eq!(
        iod_exp_model("MCP23008", IOD_EXP_FP_QFN20).id,
        "mcp23008_qfn20"
    );
}

#[test]
fn iod_exp_sibling_part_numbers_do_not_resolve_to_these_cards() {
    let ours = ["mcp23017_", "mcp23s17_", "mcp23008_"];
    let mut footprints: Vec<&str> = IOD_EXP_FP_28_LEADED.to_vec();
    footprints.extend([IOD_EXP_FP_28_QFN, IOD_EXP_FP_SSOP20, IOD_EXP_FP_QFN20]);
    footprints.extend(IOD_EXP_FP_18);
    for sibling in [
        "MCP23S08",
        "MCP23S08-E/SO",
        "MCP23018-E/SO",
        "MCP23A17",
        "MCP230171",
        "MCP23009",
        "PCF8574",
        "PCF8575",
        "TCA9535",
    ] {
        for footprint in &footprints {
            let id = resolve(sibling, Some(footprint)).model.map(|m| m.id);
            assert!(
                id.as_deref()
                    .is_none_or(|id| !ours.iter().any(|prefix| id.starts_with(prefix))),
                "{sibling} on {footprint} must not resolve to an expander card, got {id:?}"
            );
        }
    }
    // Each of the three base parts stays on its own card family.
    let want = [
        ("MCP23017-E/SO", "mcp23017_"),
        ("MCP23S17-E/SO", "mcp23s17_"),
        ("MCP23008-E/SO", "mcp23008_"),
    ];
    for (value, prefix) in want {
        for footprint in &footprints {
            let id = resolve(value, Some(footprint)).model.map(|m| m.id);
            for other in ours.iter().filter(|p| **p != prefix) {
                assert!(
                    id.as_deref().is_none_or(|id| !id.starts_with(other)),
                    "{value} on {footprint} resolved to {id:?}"
                );
            }
        }
    }
}

#[test]
fn iod_exp_package_and_footprint_must_agree() {
    // A suffix naming one package on a footprint of another resolves to no expander card.
    assert!(resolve("MCP23017-E/ML", Some(IOD_EXP_FP_28_LEADED[0]))
        .model
        .is_none_or(|m| !m.id.starts_with("mcp23017_")));
    assert!(resolve("MCP23017-E/SO", Some(IOD_EXP_FP_28_QFN))
        .model
        .is_none_or(|m| !m.id.starts_with("mcp23017_")));
    assert!(resolve("MCP23008-E/SS", Some(IOD_EXP_FP_18[0]))
        .model
        .is_none_or(|m| !m.id.starts_with("mcp23008_")));
    // A bare part number with no footprint is ambiguous across packages, so it stays unresolved.
    for value in ["MCP23017", "MCP23S17", "MCP23008"] {
        assert!(
            resolve(value, None)
                .model
                .is_none_or(|m| !m.id.starts_with("mcp23")),
            "{value} without a footprint must not pick a package"
        );
    }
}

#[test]
fn iod_exp_levels_ratings_and_envelope_follow_the_datasheet() {
    for (value, footprint) in [
        ("MCP23017-E/SO", IOD_EXP_FP_28_LEADED[0]),
        ("MCP23S17-E/ML", IOD_EXP_FP_28_QFN),
        ("MCP23008-E/SO", IOD_EXP_FP_18[0]),
    ] {
        let model = iod_exp_model(value, footprint);
        // 0.8 VDD and 0.2 VDD at the printed VDD range ends (derived), VDD - 0.7 V for VOH.
        assert_eq!(
            model.levels["vih"],
            vec![[1.8, 1.44], [5.5, 4.4]],
            "{value}"
        );
        assert_eq!(
            model.levels["vil"],
            vec![[1.8, 0.36], [5.5, 1.1]],
            "{value}"
        );
        assert_eq!(model.levels["voh"], vec![[1.8, 1.1], [5.5, 4.8]], "{value}");
        assert_eq!(model.params.get_f64("iih_a"), Some(0.000001), "{value}");
    }
    let model = iod_exp_model("MCP23017-E/SO", IOD_EXP_FP_28_LEADED[0]);
    let hauksbee_models::OperatingEnvelope::SupplyRange {
        pin,
        min_v,
        max_v,
        abs_max_v,
        ..
    } = &model.envelope[0]
    else {
        panic!("first envelope row is the VDD supply range");
    };
    assert_eq!(pin, "vdd");
    assert_eq!((*min_v, *max_v, *abs_max_v), (1.8, 5.5, Some(5.5)));
}

// Fragment: tests for the PCF8574/PCF8574A, PCA9685 and TCA9548A/PCA9548A digital cards.
// Written without running cargo; the lead compiles and runs them. Needs `resolve(value, footprint)` in scope.

const IO_MUXPWM_SHA_PCF8574: &str =
    "745da5489509a9ed802977a30bf5bf03aab1aa06ae53c717c49cc98053cf8cfc";
const IO_MUXPWM_SHA_PCF8574A: &str =
    "67a11a49db26f2e415196694707ac06598d81b764ed7ce6de138a465b59c04d5";
const IO_MUXPWM_SHA_PCA9685: &str =
    "237d47f339cac4c3a0d56a5f0b4d3c93df71e3eb43f36ac57ea4ff38e6b2e585";
const IO_MUXPWM_SHA_TCA9548A: &str =
    "2137822fd7128945ea44e83f4e100a932b30964d763715eeffe217ee6080a2dc";
const IO_MUXPWM_SHA_PCA9548A: &str =
    "b2238acf0fecd58cbbdf8a9f09e8c354a2ae0414a10cbc4dcc5530516bac6145";

const IO_MUXPWM_FP_DW16: &str = "Package_SO:SOIC-16W_7.5x10.3mm_P1.27mm";
const IO_MUXPWM_FP_N16: &str = "Package_DIP:DIP-16_W7.62mm";
const IO_MUXPWM_FP_TVSOP20: &str = "Package_SO:TVSOP-20_4.4x6.5mm_P0.65mm";
const IO_MUXPWM_FP_TSSOP20: &str = "Package_SO:TSSOP-20_4.4x6.5mm_P0.65mm";
const IO_MUXPWM_FP_RGY20: &str = "Package_DFN_QFN:VQFN-20-1EP_3.5x4.5mm_P0.5mm_EP2.05x3.05mm";
const IO_MUXPWM_FP_RGT16: &str = "Package_DFN_QFN:VQFN-16-1EP_3x3mm_P0.5mm_EP1.68x1.68mm";
const IO_MUXPWM_FP_TSSOP28: &str = "Package_SO:TSSOP-28_4.4x9.7mm_P0.65mm";
const IO_MUXPWM_FP_HVQFN28: &str = "Package_DFN_QFN:HVQFN-28-1EP_6x6mm_P0.65mm_EP4.25x4.25mm";
const IO_MUXPWM_FP_TSSOP24: &str = "Package_SO:TSSOP-24_4.4x7.8mm_P0.65mm";
const IO_MUXPWM_FP_VSSOP24: &str = "Package_SO:VSSOP-24_3x6mm_P0.5mm";
const IO_MUXPWM_FP_SSOP24: &str = "Package_SO:SSOP-24_5.3x8.2mm_P0.65mm";
const IO_MUXPWM_FP_TVSOP24: &str = "Package_SO:TVSOP-24_4.4x5mm_P0.4mm";
const IO_MUXPWM_FP_SOIC24: &str = "Package_SO:SOIC-24W_7.5x15.4mm_P1.27mm";
const IO_MUXPWM_FP_RGE24: &str = "Package_DFN_QFN:VQFN-24-1EP_4x4mm_P0.5mm_EP2.45x2.45mm";
const IO_MUXPWM_FP_SOIC16N: &str = "Package_SO:SOIC-16_3.9x9.9mm_P1.27mm";
const IO_MUXPWM_FP_TSSOP16: &str = "Package_SO:TSSOP-16_4.4x5mm_P0.65mm";

struct IoMuxpwmCard {
    id: &'static str,
    footprint: &'static str,
    values: &'static [&'static str],
    sha: &'static str,
    pins: usize,
    supply_pad: &'static str,
    gnd_pad: &'static str,
    supply_role: &'static str,
    address: u8,
    selects: usize,
    vol: f64,
    must_not_float: &'static str,
    threshold_v: f64,
    idle_a: f64,
    active_a: f64,
}

fn io_muxpwm_model(value: &str, footprint: &str) -> hauksbee_models::ModelEntry {
    resolve(value, Some(footprint))
        .model
        .unwrap_or_else(|| panic!("{value} on {footprint} should resolve"))
}

fn io_muxpwm_has_sha(value: &str, footprint: &str, sha: &str) -> bool {
    resolve(value, Some(footprint))
        .references
        .iter()
        .any(|reference| reference.sha256.as_deref() == Some(sha))
}

fn io_muxpwm_spec(model: &hauksbee_models::ModelEntry) -> hauksbee_models::sensor_spec::SensorSpec {
    match model.peripheral.as_ref().expect("register-map peripheral") {
        hauksbee_models::PeripheralSpec::RegisterMap { spec_toml, .. } => {
            hauksbee_models::sensor_spec::SensorSpec::from_toml(spec_toml)
                .expect("spec_toml parses and validates")
        }
        other => panic!("expected a register map, got {other:?}"),
    }
}

fn io_muxpwm_pin<'a>(model: &'a hauksbee_models::ModelEntry, pad: &str) -> &'a str {
    model
        .pins
        .get(pad)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("{} has no pad {pad}", model.id))
}

/// The value on that footprint must not resolve to the named card (it may resolve to another card
/// or to nothing).
fn io_muxpwm_not(card: &str, value: &str, footprint: &str) {
    let got = resolve(value, Some(footprint)).model.map(|m| m.id);
    assert_ne!(
        got.as_deref(),
        Some(card),
        "{value} on {footprint} must not resolve to {card}"
    );
}

/// Assertions shared by every card: every ordering code resolves to this card, the pin map and the
/// peripheral spec agree, supply and ground pads carry the right roles, the datasheet is cited by
/// hash, and the executable I2C part is wired to its address straps.
fn io_muxpwm_check_card(card: &IoMuxpwmCard) {
    for value in card.values {
        let model = io_muxpwm_model(value, card.footprint);
        assert_eq!(model.id, card.id, "{value} on {}", card.footprint);
        assert!(
            io_muxpwm_has_sha(value, card.footprint, card.sha),
            "{value} on {} cites the datasheet by hash",
            card.footprint
        );
    }
    let model = io_muxpwm_model(card.values[0], card.footprint);
    assert_eq!(model.kind, hauksbee_models::ComponentKind::Digital);
    assert_eq!(model.pins.len(), card.pins, "{}", card.id);
    assert_eq!(
        io_muxpwm_pin(&model, card.supply_pad),
        card.supply_role,
        "{}",
        card.id
    );
    assert_eq!(io_muxpwm_pin(&model, card.gnd_pad), "gnd", "{}", card.id);
    assert_eq!(
        model.params.get_str("supply_pin"),
        Some(card.supply_pad),
        "{}",
        card.id
    );
    assert_eq!(
        model.params.get_str("gnd_pin"),
        Some(card.gnd_pad),
        "{}",
        card.id
    );
    assert_eq!(model.params.get_f64("vol"), Some(card.vol), "{}", card.id);
    assert_eq!(
        model.params.get_str("must_not_float_roles"),
        Some(card.must_not_float),
        "{}",
        card.id
    );
    assert!(
        model.levels.contains_key("vih") && model.levels.contains_key("vil"),
        "{}",
        card.id
    );
    assert!(!model.envelope.is_empty(), "{}", card.id);
    let roles: Vec<&str> = model.pins.values().map(String::as_str).collect();
    assert!(
        roles.contains(&"scl") && roles.contains(&"sda"),
        "{}",
        card.id
    );

    let power = model.peripheral_power.as_ref().expect("peripheral_power");
    assert_eq!(power.supply_role, card.supply_role, "{}", card.id);
    assert_eq!(power.return_role, "gnd", "{}", card.id);
    assert_eq!(power.power_on_threshold_v, card.threshold_v, "{}", card.id);
    assert_eq!(power.idle_a, card.idle_a, "{}", card.id);
    assert_eq!(power.read_a, card.active_a, "{}", card.id);
    assert_eq!(power.write_a, card.active_a, "{}", card.id);

    let spec = io_muxpwm_spec(&model).sensor;
    assert_eq!(spec.i2c_address, Some(card.address), "{}", card.id);
    assert_eq!(spec.address_selects.len(), card.selects, "{}", card.id);
    assert!(
        spec.address_selects
            .iter()
            .any(|row| row.address == card.address),
        "{}: the default address has a strap row",
        card.id
    );
    assert_eq!(spec.pins.len(), model.pins.len(), "{}", card.id);
    for (pad, role) in &spec.pins {
        assert_eq!(model.pins.get(pad), Some(role), "{} pad {pad}", card.id);
    }
    for row in &spec.address_selects {
        for strap in row.straps.keys() {
            assert!(
                roles.contains(&strap.as_str()),
                "{}: strap {strap} is a pin role",
                card.id
            );
        }
    }
}

fn io_muxpwm_pcf8574_cards() -> Vec<IoMuxpwmCard> {
    vec![
        IoMuxpwmCard {
            id: "pcf8574_dw",
            footprint: IO_MUXPWM_FP_DW16,
            values: &[
                "PCF8574",
                "PCF8574DW",
                "PCF8574DWR",
                "PCF8574DWR.A",
                "PCF8574DWR1G4",
                "PCF8574DWRG4",
            ],
            sha: IO_MUXPWM_SHA_PCF8574,
            pins: 16,
            supply_pad: "16",
            gnd_pad: "8",
            supply_role: "vcc",
            address: 0x20,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
        IoMuxpwmCard {
            id: "pcf8574_n",
            footprint: IO_MUXPWM_FP_N16,
            values: &["PCF8574", "PCF8574N", "PCF8574N.A", "PCF8574NE4"],
            sha: IO_MUXPWM_SHA_PCF8574,
            pins: 16,
            supply_pad: "16",
            gnd_pad: "8",
            supply_role: "vcc",
            address: 0x20,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
        IoMuxpwmCard {
            id: "pcf8574_dgv",
            footprint: IO_MUXPWM_FP_TVSOP20,
            values: &["PCF8574", "PCF8574DGVR", "PCF8574DGVR.A"],
            sha: IO_MUXPWM_SHA_PCF8574,
            pins: 20,
            supply_pad: "5",
            gnd_pad: "15",
            supply_role: "vcc",
            address: 0x20,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
        IoMuxpwmCard {
            id: "pcf8574_pw",
            footprint: IO_MUXPWM_FP_TSSOP20,
            values: &["PCF8574", "PCF8574PWR", "PCF8574PWR.A", "PCF8574PWRG4"],
            sha: IO_MUXPWM_SHA_PCF8574,
            pins: 20,
            supply_pad: "5",
            gnd_pad: "15",
            supply_role: "vcc",
            address: 0x20,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
        IoMuxpwmCard {
            id: "pcf8574_rgy",
            footprint: IO_MUXPWM_FP_RGY20,
            values: &["PCF8574", "PCF8574RGYR", "PCF8574RGYR.A", "PCF8574RGYRG4"],
            sha: IO_MUXPWM_SHA_PCF8574,
            pins: 20,
            supply_pad: "5",
            gnd_pad: "15",
            supply_role: "vcc",
            address: 0x20,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
        IoMuxpwmCard {
            id: "pcf8574_rgt",
            footprint: IO_MUXPWM_FP_RGT16,
            values: &["PCF8574", "PCF8574RGTR", "PCF8574RGTR.A", "PCF8574RGTRG4"],
            sha: IO_MUXPWM_SHA_PCF8574,
            pins: 16,
            supply_pad: "1",
            gnd_pad: "9",
            supply_role: "vcc",
            address: 0x20,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
    ]
}

fn io_muxpwm_pcf8574a_cards() -> Vec<IoMuxpwmCard> {
    vec![
        IoMuxpwmCard {
            id: "pcf8574a_dw",
            footprint: IO_MUXPWM_FP_DW16,
            values: &[
                "PCF8574A",
                "PCF8574ADW",
                "PCF8574ADWR",
                "PCF8574ADWR.A",
                "PCF8574ADWR1G4",
            ],
            sha: IO_MUXPWM_SHA_PCF8574A,
            pins: 16,
            supply_pad: "16",
            gnd_pad: "8",
            supply_role: "vcc",
            address: 0x38,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
        IoMuxpwmCard {
            id: "pcf8574a_n",
            footprint: IO_MUXPWM_FP_N16,
            values: &["PCF8574A", "PCF8574AN", "PCF8574AN.A", "PCF8574ANE4"],
            sha: IO_MUXPWM_SHA_PCF8574A,
            pins: 16,
            supply_pad: "16",
            gnd_pad: "8",
            supply_role: "vcc",
            address: 0x38,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
        IoMuxpwmCard {
            id: "pcf8574a_dgv",
            footprint: IO_MUXPWM_FP_TVSOP20,
            values: &["PCF8574A", "PCF8574ADGVR", "PCF8574ADGVR.A"],
            sha: IO_MUXPWM_SHA_PCF8574A,
            pins: 20,
            supply_pad: "5",
            gnd_pad: "15",
            supply_role: "vcc",
            address: 0x38,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
        IoMuxpwmCard {
            id: "pcf8574a_pw",
            footprint: IO_MUXPWM_FP_TSSOP20,
            values: &[
                "PCF8574A",
                "PCF8574APW",
                "PCF8574APWR",
                "PCF8574APWR.A",
                "PCF8574APWRG4",
            ],
            sha: IO_MUXPWM_SHA_PCF8574A,
            pins: 20,
            supply_pad: "5",
            gnd_pad: "15",
            supply_role: "vcc",
            address: 0x38,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
        IoMuxpwmCard {
            id: "pcf8574a_rgy",
            footprint: IO_MUXPWM_FP_RGY20,
            values: &[
                "PCF8574A",
                "PCF8574ARGYR",
                "PCF8574ARGYR.A",
                "PCF8574ARGYRG4",
            ],
            sha: IO_MUXPWM_SHA_PCF8574A,
            pins: 20,
            supply_pad: "5",
            gnd_pad: "15",
            supply_role: "vcc",
            address: 0x38,
            selects: 8,
            vol: 1.0,
            must_not_float: "a0,a1,a2",
            threshold_v: 2.5,
            idle_a: 1e-05,
            active_a: 0.0001,
        },
    ]
}

fn io_muxpwm_pca9685_cards() -> Vec<IoMuxpwmCard> {
    vec![
        IoMuxpwmCard {
            id: "pca9685_tssop28",
            footprint: IO_MUXPWM_FP_TSSOP28,
            values: &["PCA9685", "PCA9685PW", "PCA9685PW/Q900", "PCA9685PW,118"],
            sha: IO_MUXPWM_SHA_PCA9685,
            pins: 28,
            supply_pad: "28",
            gnd_pad: "14",
            supply_role: "vdd",
            address: 0x40,
            selects: 63,
            vol: 0.5,
            must_not_float: "a0,a1,a2,a3,a4,a5,extclk",
            threshold_v: 2.3,
            idle_a: 1.55e-05,
            active_a: 0.01,
        },
        IoMuxpwmCard {
            id: "pca9685_hvqfn28",
            footprint: IO_MUXPWM_FP_HVQFN28,
            values: &["PCA9685", "PCA9685BS", "PCA9685BS,118"],
            sha: IO_MUXPWM_SHA_PCA9685,
            pins: 28,
            supply_pad: "25",
            gnd_pad: "11",
            supply_role: "vdd",
            address: 0x40,
            selects: 63,
            vol: 0.5,
            must_not_float: "a0,a1,a2,a3,a4,a5,extclk",
            threshold_v: 2.3,
            idle_a: 1.55e-05,
            active_a: 0.01,
        },
    ]
}

fn io_muxpwm_tca9548a_cards() -> Vec<IoMuxpwmCard> {
    vec![
        IoMuxpwmCard {
            id: "tca9548a_pw",
            footprint: IO_MUXPWM_FP_TSSOP24,
            values: &["TCA9548A", "TCA9548APWR", "TCA9548APWR.B", "TCA9548APWRG4"],
            sha: IO_MUXPWM_SHA_TCA9548A,
            pins: 24,
            supply_pad: "24",
            gnd_pad: "12",
            supply_role: "vcc",
            address: 0x70,
            selects: 8,
            vol: 0.4,
            must_not_float: "a0,a1,a2,reset_n",
            threshold_v: 1.65,
            idle_a: 2e-06,
            active_a: 8e-05,
        },
        IoMuxpwmCard {
            id: "tca9548a_dgs",
            footprint: IO_MUXPWM_FP_VSSOP24,
            values: &["TCA9548A", "TCA9548ADGSR", "TCA9548ADGSR.B"],
            sha: IO_MUXPWM_SHA_TCA9548A,
            pins: 24,
            supply_pad: "24",
            gnd_pad: "12",
            supply_role: "vcc",
            address: 0x70,
            selects: 8,
            vol: 0.4,
            must_not_float: "a0,a1,a2,reset_n",
            threshold_v: 1.65,
            idle_a: 2e-06,
            active_a: 8e-05,
        },
        IoMuxpwmCard {
            id: "tca9548a_rge",
            footprint: IO_MUXPWM_FP_RGE24,
            values: &[
                "TCA9548A",
                "TCA9548AMRGER",
                "TCA9548ARGER",
                "TCA9548ARGERG4",
                "TCA9548ARGER.B",
            ],
            sha: IO_MUXPWM_SHA_TCA9548A,
            pins: 24,
            supply_pad: "21",
            gnd_pad: "9",
            supply_role: "vcc",
            address: 0x70,
            selects: 8,
            vol: 0.4,
            must_not_float: "a0,a1,a2,reset_n",
            threshold_v: 1.65,
            idle_a: 2e-06,
            active_a: 8e-05,
        },
    ]
}

fn io_muxpwm_pca9548a_cards() -> Vec<IoMuxpwmCard> {
    vec![
        IoMuxpwmCard {
            id: "pca9548a_db",
            footprint: IO_MUXPWM_FP_SSOP24,
            values: &["PCA9548A", "PCA9548ADB", "PCA9548ADBR", "PCA9548ADBR.A"],
            sha: IO_MUXPWM_SHA_PCA9548A,
            pins: 24,
            supply_pad: "24",
            gnd_pad: "12",
            supply_role: "vcc",
            address: 0x70,
            selects: 8,
            vol: 0.4,
            must_not_float: "a0,a1,a2,reset_n",
            threshold_v: 2.3,
            idle_a: 2e-06,
            active_a: 8e-05,
        },
        IoMuxpwmCard {
            id: "pca9548a_dgv",
            footprint: IO_MUXPWM_FP_TVSOP24,
            values: &["PCA9548A", "PCA9548ADGV", "PCA9548ADGVR", "PCA9548ADGVR.A"],
            sha: IO_MUXPWM_SHA_PCA9548A,
            pins: 24,
            supply_pad: "24",
            gnd_pad: "12",
            supply_role: "vcc",
            address: 0x70,
            selects: 8,
            vol: 0.4,
            must_not_float: "a0,a1,a2,reset_n",
            threshold_v: 2.3,
            idle_a: 2e-06,
            active_a: 8e-05,
        },
        IoMuxpwmCard {
            id: "pca9548a_dw",
            footprint: IO_MUXPWM_FP_SOIC24,
            values: &["PCA9548A", "PCA9548ADW", "PCA9548ADWR", "PCA9548ADWR.A"],
            sha: IO_MUXPWM_SHA_PCA9548A,
            pins: 24,
            supply_pad: "24",
            gnd_pad: "12",
            supply_role: "vcc",
            address: 0x70,
            selects: 8,
            vol: 0.4,
            must_not_float: "a0,a1,a2,reset_n",
            threshold_v: 2.3,
            idle_a: 2e-06,
            active_a: 8e-05,
        },
        IoMuxpwmCard {
            id: "pca9548a_pw",
            footprint: IO_MUXPWM_FP_TSSOP24,
            values: &["PCA9548A", "PCA9548APWR", "PCA9548APWR.B", "PCA9548APWRG4"],
            sha: IO_MUXPWM_SHA_PCA9548A,
            pins: 24,
            supply_pad: "24",
            gnd_pad: "12",
            supply_role: "vcc",
            address: 0x70,
            selects: 8,
            vol: 0.4,
            must_not_float: "a0,a1,a2,reset_n",
            threshold_v: 2.3,
            idle_a: 2e-06,
            active_a: 8e-05,
        },
        IoMuxpwmCard {
            id: "pca9548a_rge",
            footprint: IO_MUXPWM_FP_RGE24,
            values: &["PCA9548A", "PCA9548ARGER", "PCA9548ARGER.B"],
            sha: IO_MUXPWM_SHA_PCA9548A,
            pins: 24,
            supply_pad: "21",
            gnd_pad: "9",
            supply_role: "vcc",
            address: 0x70,
            selects: 8,
            vol: 0.4,
            must_not_float: "a0,a1,a2,reset_n",
            threshold_v: 2.3,
            idle_a: 2e-06,
            active_a: 8e-05,
        },
    ]
}

fn io_muxpwm_all_cards() -> Vec<IoMuxpwmCard> {
    let mut all = io_muxpwm_pcf8574_cards();
    all.extend(io_muxpwm_pcf8574a_cards());
    all.extend(io_muxpwm_pca9685_cards());
    all.extend(io_muxpwm_tca9548a_cards());
    all.extend(io_muxpwm_pca9548a_cards());
    all
}

fn io_muxpwm_check_missing(model: &hauksbee_models::ModelEntry, gaps: &[&str]) {
    for gap in gaps {
        assert!(
            model.coverage.missing.iter().any(|m| m == gap),
            "{} lists {gap} as missing",
            model.id
        );
    }
}

/// PCF8574 and PCF8574A share one pointerless protocol: every byte written sets the output latch
/// P0..P7 (all high at power on, eight states) and a read returns the latch gated by the external
/// level (eight inputs, default high).
fn io_muxpwm_check_pcf_spec(card: &IoMuxpwmCard) {
    let model = io_muxpwm_model(card.values[0], card.footprint);
    let spec = io_muxpwm_spec(&model).sensor;
    let mut addresses: Vec<u8> = spec.address_selects.iter().map(|row| row.address).collect();
    addresses.sort_unstable();
    assert_eq!(
        addresses,
        (card.address..card.address + 8).collect::<Vec<u8>>(),
        "{}",
        card.id
    );
    for row in &spec.address_selects {
        assert_eq!(row.straps.len(), 3, "{}: a2, a1 and a0", card.id);
    }
    let inputs: Vec<&str> = spec.inputs.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        inputs,
        ["ext0", "ext1", "ext2", "ext3", "ext4", "ext5", "ext6", "ext7"],
        "{}",
        card.id
    );
    assert!(spec.inputs.iter().all(|i| i.default == 1.0), "{}", card.id);
    let states: Vec<&str> = spec.states.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        states,
        ["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7"],
        "{}",
        card.id
    );
    assert!(spec.states.iter().all(|s| s.default == 1.0), "{}", card.id);
    assert!(
        spec.write_registers.is_empty() && spec.registers.is_empty(),
        "{}",
        card.id
    );
    assert_eq!(spec.write_commands.len(), 1, "{}", card.id);
    let command = &spec.write_commands[0];
    assert_eq!(command.name, "port_write", "{}", card.id);
    assert_eq!(
        (command.match_mask, command.match_value),
        (0, 0),
        "{}",
        card.id
    );
    assert!(!command.prefix, "{}", card.id);
    assert_eq!(command.group_bytes, 1, "{}", card.id);
    assert_eq!(command.fields.len(), 8, "{}", card.id);
    assert_eq!(command.update.len(), 8, "{}", card.id);
    let frame = spec.read_frame.as_ref().expect("read_frame");
    assert_eq!(frame.bytes.len(), 1, "{}", card.id);
    for bit in 0..8 {
        assert!(
            frame.bytes[0].contains(&format!("ext{bit}")),
            "{} bit {bit}",
            card.id
        );
        assert!(
            frame.bytes[0].contains(&format!("p{bit} ")),
            "{} bit {bit}",
            card.id
        );
    }
    assert!(
        model.params.get_f64("voh").is_none(),
        "{}: VOH is not in the datasheet",
        card.id
    );
    assert!(model.levels.get("voh").is_none(), "{}", card.id);
    io_muxpwm_check_missing(
        &model,
        &[
            "p_port_pin_drive",
            "quasi_bidirectional_pull_up",
            "int_pin_output",
            "voh",
        ],
    );
}

#[test]
fn io_muxpwm_pcf8574_cards_resolve_with_executable_port_latch() {
    let cards = io_muxpwm_pcf8574_cards();
    assert_eq!(cards.len(), 6, "N, DW, DGV, PW, RGY and RGT");
    for card in &cards {
        io_muxpwm_check_card(card);
        io_muxpwm_check_pcf_spec(card);
        assert_eq!(card.address, 0x20);
    }
}

#[test]
fn io_muxpwm_pcf8574a_cards_resolve_at_the_0x38_base() {
    let cards = io_muxpwm_pcf8574a_cards();
    assert_eq!(cards.len(), 5, "N, DW, DGV, PW and RGY (no RGT variant)");
    for card in &cards {
        io_muxpwm_check_card(card);
        io_muxpwm_check_pcf_spec(card);
        assert_eq!(card.address, 0x38);
    }
}

#[test]
fn io_muxpwm_pcf8574_and_pcf8574a_do_not_cross_resolve() {
    for card in io_muxpwm_pcf8574_cards() {
        for bad in ["PCF8574A", "PCF8574ADW", "PCF8574APWR", "PCF8574AN"] {
            io_muxpwm_not(card.id, bad, card.footprint);
        }
    }
    for card in io_muxpwm_pcf8574a_cards() {
        for bad in ["PCF8574", "PCF8574DW", "PCF8574PWR", "PCF8574N"] {
            io_muxpwm_not(card.id, bad, card.footprint);
        }
    }
    // The 0x20 and 0x38 bases come from different cards on the same footprint.
    let plain = io_muxpwm_model("PCF8574", IO_MUXPWM_FP_DW16);
    let a_part = io_muxpwm_model("PCF8574A", IO_MUXPWM_FP_DW16);
    assert_eq!(plain.id, "pcf8574_dw");
    assert_eq!(a_part.id, "pcf8574a_dw");
    assert_eq!(io_muxpwm_spec(&plain).sensor.i2c_address, Some(0x20));
    assert_eq!(io_muxpwm_spec(&a_part).sensor.i2c_address, Some(0x38));
}

#[test]
fn io_muxpwm_pcf_variants_and_siblings_are_rejected() {
    for card in io_muxpwm_pcf8574_cards()
        .iter()
        .chain(io_muxpwm_pcf8574a_cards().iter())
    {
        for bad in [
            "PCF8574T",
            "PCF8574AT",
            "PCF8574P",
            "PCF8574AP",
            "PCF8575",
            "PCF8574ATS",
        ] {
            io_muxpwm_not(card.id, bad, card.footprint);
        }
        io_muxpwm_not(card.id, "PCF8574", IO_MUXPWM_FP_TSSOP16);
        io_muxpwm_not(card.id, "PCF8574A", IO_MUXPWM_FP_TSSOP16);
    }
    io_muxpwm_not("pcf8574_dw", "PCF8574", IO_MUXPWM_FP_SOIC16N);
    io_muxpwm_not("pcf8574a_dw", "PCF8574A", IO_MUXPWM_FP_SOIC16N);
}

/// A card matches only its own package: the same ordering value on another package's footprint
/// resolves elsewhere or not at all.
#[test]
fn io_muxpwm_cards_are_gated_by_package_footprint() {
    let all = io_muxpwm_all_cards();
    for card in &all {
        for other in &all {
            if other.footprint != card.footprint {
                for value in card.values {
                    io_muxpwm_not(card.id, value, other.footprint);
                }
            }
        }
    }
}

#[test]
fn io_muxpwm_pca9685_cards_expose_the_reset_register_file() {
    let cards = io_muxpwm_pca9685_cards();
    assert_eq!(cards.len(), 2, "TSSOP-28 and HVQFN-28");
    for card in &cards {
        io_muxpwm_check_card(card);
        assert_eq!(card.address, 0x40);
        let model = io_muxpwm_model(card.values[0], card.footprint);
        let spec = io_muxpwm_spec(&model).sensor;
        let mut addresses: Vec<u8> = spec.address_selects.iter().map(|row| row.address).collect();
        addresses.sort_unstable();
        let want: Vec<u8> = (0x40..=0x7F).filter(|a| *a != 0x70).collect();
        assert_eq!(addresses.len(), 63, "{}", card.id);
        assert_eq!(
            addresses, want,
            "{}: 0x70 is the reserved Software Reset address",
            card.id
        );
        for row in &spec.address_selects {
            assert_eq!(row.straps.len(), 6, "{}: a5..a0", card.id);
        }
        assert!(
            spec.write_commands.is_empty() && spec.read_frame.is_none(),
            "{}",
            card.id
        );
        assert_eq!(spec.write_registers.len(), 71, "{}", card.id);
        assert_eq!(spec.registers.len(), 75, "{}", card.id);
        let write = |addr: u8| {
            spec.write_registers
                .iter()
                .find(|w| w.addr == addr)
                .unwrap_or_else(|| panic!("{} write register 0x{addr:02X}", card.id))
        };
        assert_eq!(
            write(0x00).default,
            17.0,
            "{} MODE1 resets to 0x11",
            card.id
        );
        assert_eq!(write(0x00).fields.len(), 7, "{}", card.id);
        assert_eq!(write(0x01).default, 4.0, "{} MODE2 resets to 0x04", card.id);
        assert_eq!(write(0x01).fields.len(), 4, "{}", card.id);
        assert_eq!(write(0x02).default, 226.0, "{} SUBADR1", card.id);
        assert_eq!(write(0x03).default, 228.0, "{} SUBADR2", card.id);
        assert_eq!(write(0x04).default, 232.0, "{} SUBADR3", card.id);
        assert_eq!(write(0x05).default, 224.0, "{} ALLCALLADR", card.id);
        assert_eq!(
            write(0xFE).default,
            30.0,
            "{} PRE_SCALE resets to 0x1E",
            card.id
        );
        for led in 0..16u8 {
            let base = 0x06 + 4 * led;
            assert_eq!(write(base).default, 0.0, "{} LED{led}_ON_L", card.id);
            assert_eq!(write(base + 1).default, 0.0, "{} LED{led}_ON_H", card.id);
            assert_eq!(write(base + 2).default, 0.0, "{} LED{led}_OFF_L", card.id);
            assert_eq!(
                write(base + 3).default,
                16.0,
                "{} LED{led}_OFF_H: full-off set at reset",
                card.id
            );
            for high in [base + 1, base + 3] {
                let bits: Vec<[u8; 2]> = write(high).fields.iter().map(|f| f.bits).collect();
                assert_eq!(
                    bits,
                    [[4, 4], [3, 0]],
                    "{} 0x{high:02X}: full flag and count",
                    card.id
                );
            }
        }
        // ALL_LED registers are write-only: they read back as zero. 0xFF is not a register.
        for addr in 0xFAu8..=0xFD {
            let reg = spec
                .registers
                .iter()
                .find(|r| r.addr == addr)
                .expect("ALL_LED read");
            assert_eq!(
                reg.r#const.as_deref(),
                Some(&[0u8][..]),
                "{} 0x{addr:02X}",
                card.id
            );
        }
        assert!(spec.registers.iter().all(|r| r.addr != 0xFF), "{}", card.id);
        assert!(
            spec.write_registers.iter().all(|w| w.addr != 0xFF),
            "{}",
            card.id
        );
        assert!(
            model.params.get_f64("voh").is_some(),
            "{}: VOH is printed for the PCA9685",
            card.id
        );
        assert!(model.levels.contains_key("voh"), "{}", card.id);
        io_muxpwm_check_missing(
            &model,
            &[
                "led_pwm_outputs",
                "oe_pin_gating",
                "sub_call_and_all_call_addresses",
                "software_reset_command",
                "thermal_resistance",
            ],
        );
    }
}

#[test]
fn io_muxpwm_pca9685_siblings_and_other_package_are_rejected() {
    for card in io_muxpwm_pca9685_cards() {
        for bad in ["PCA9635", "PCA9685X", "PCA9686", "PCA9685BS,", "TCA9548A"] {
            io_muxpwm_not(card.id, bad, card.footprint);
        }
    }
    io_muxpwm_not("pca9685_tssop28", "PCA9685", IO_MUXPWM_FP_HVQFN28);
    io_muxpwm_not("pca9685_hvqfn28", "PCA9685", IO_MUXPWM_FP_TSSOP28);
    io_muxpwm_not("pca9685_tssop28", "PCA9685BS", IO_MUXPWM_FP_TSSOP28);
    io_muxpwm_not("pca9685_hvqfn28", "PCA9685PW", IO_MUXPWM_FP_HVQFN28);
}

/// TCA9548A and PCA9548A share one control register: a single written byte selects the enabled
/// downstream channels and a read returns it.
fn io_muxpwm_check_mux_spec(card: &IoMuxpwmCard) {
    let model = io_muxpwm_model(card.values[0], card.footprint);
    let spec = io_muxpwm_spec(&model).sensor;
    let mut addresses: Vec<u8> = spec.address_selects.iter().map(|row| row.address).collect();
    addresses.sort_unstable();
    assert_eq!(addresses, (0x70..=0x77).collect::<Vec<u8>>(), "{}", card.id);
    for row in &spec.address_selects {
        assert_eq!(row.straps.len(), 3, "{}: a2, a1 and a0", card.id);
    }
    assert_eq!(spec.states.len(), 1, "{}", card.id);
    assert_eq!(spec.states[0].name, "ctrl", "{}", card.id);
    assert_eq!(
        spec.states[0].default, 0.0,
        "{}: all channels disabled at reset",
        card.id
    );
    assert!(
        spec.write_registers.is_empty() && spec.registers.is_empty(),
        "{}",
        card.id
    );
    assert_eq!(spec.write_commands.len(), 1, "{}", card.id);
    let command = &spec.write_commands[0];
    assert_eq!(command.name, "control_write", "{}", card.id);
    assert_eq!(
        (command.match_mask, command.match_value),
        (0, 0),
        "{}",
        card.id
    );
    assert!(!command.prefix, "{}", card.id);
    assert_eq!(command.group_bytes, 1, "{}", card.id);
    assert_eq!(command.fields.len(), 1, "{}", card.id);
    assert_eq!(command.fields[0].bits, [7, 0], "{}", card.id);
    assert_eq!(
        spec.read_frame.as_ref().expect("read_frame").bytes,
        ["ctrl"],
        "{}",
        card.id
    );
    io_muxpwm_check_missing(
        &model,
        &[
            "downstream_channel_connectivity",
            "reset_pin_behaviour",
            "switch_on_resistance_and_pass_voltage",
        ],
    );
}

#[test]
fn io_muxpwm_tca9548a_cards_resolve_with_a_control_register() {
    let cards = io_muxpwm_tca9548a_cards();
    assert_eq!(cards.len(), 3, "PW, DGS and RGE");
    for card in &cards {
        io_muxpwm_check_card(card);
        io_muxpwm_check_mux_spec(card);
        assert_eq!(card.address, 0x70);
        assert_eq!(card.threshold_v, 1.65, "TCA9548A runs from 1.65 V");
    }
}

#[test]
fn io_muxpwm_pca9548a_cards_resolve_with_a_control_register() {
    let cards = io_muxpwm_pca9548a_cards();
    assert_eq!(cards.len(), 5, "DB, DGV, DW, PW and RGE");
    for card in &cards {
        io_muxpwm_check_card(card);
        io_muxpwm_check_mux_spec(card);
        assert_eq!(card.address, 0x70);
        assert_eq!(card.threshold_v, 2.3, "PCA9548A runs from 2.3 V");
    }
}

#[test]
fn io_muxpwm_tca9548a_and_pca9548a_do_not_cross_resolve() {
    for card in io_muxpwm_tca9548a_cards() {
        for bad in [
            "PCA9548A",
            "PCA9548APWR",
            "TCA9548",
            "TCA9548AX",
            "TCA9546A",
            "TCA9548APW",
        ] {
            io_muxpwm_not(card.id, bad, card.footprint);
        }
    }
    for card in io_muxpwm_pca9548a_cards() {
        for bad in [
            "TCA9548A",
            "TCA9548APWR",
            "PCA9548",
            "PCA9546A",
            "PCA9548AXX",
        ] {
            io_muxpwm_not(card.id, bad, card.footprint);
        }
    }
    // The shared TSSOP-24 and VQFN-24 footprints are told apart by the part name alone.
    assert_eq!(
        io_muxpwm_model("TCA9548A", IO_MUXPWM_FP_TSSOP24).id,
        "tca9548a_pw"
    );
    assert_eq!(
        io_muxpwm_model("PCA9548A", IO_MUXPWM_FP_TSSOP24).id,
        "pca9548a_pw"
    );
    assert_eq!(
        io_muxpwm_model("TCA9548A", IO_MUXPWM_FP_RGE24).id,
        "tca9548a_rge"
    );
    assert_eq!(
        io_muxpwm_model("PCA9548A", IO_MUXPWM_FP_RGE24).id,
        "pca9548a_rge"
    );
}

#[test]
fn io_muxpwm_every_card_cites_a_datasheet_hash_and_a_register_map() {
    let all = io_muxpwm_all_cards();
    assert_eq!(all.len(), 21);
    for card in &all {
        let model = io_muxpwm_model(card.values[0], card.footprint);
        assert!(
            model.peripheral.is_some() && model.peripheral_power.is_some(),
            "{}",
            card.id
        );
        assert!(
            model
                .coverage
                .implements
                .iter()
                .any(|i| i == "i2c_address_strap"),
            "{}",
            card.id
        );
        assert!(
            model
                .coverage
                .missing
                .iter()
                .any(|m| m == "i2c_timing_100khz"
                    || m == "i2c_timing_fm_plus"
                    || m == "i2c_timing_modes"),
            "{} states that I2C timing is not modelled",
            card.id
        );
    }
}
