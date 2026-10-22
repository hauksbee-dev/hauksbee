//! Official-parts batch converters_analog: resolution and behaviour tests for the cards added in that batch.

use hauksbee_models::{Confidence, ModelEntry, PeripheralSpec};

use super::{assert_pin_map, query_id, resolve_query};

/// The embedded register-map spec of a card, parsed.
fn spec(m: &ModelEntry) -> toml::Value {
    match m
        .peripheral
        .as_ref()
        .unwrap_or_else(|| panic!("{} has no peripheral", m.id))
    {
        PeripheralSpec::RegisterMap { spec_toml, .. } => {
            toml::from_str(spec_toml).unwrap_or_else(|e| panic!("{}: spec_toml: {e}", m.id))
        }
        other => panic!("{}: expected a register map, got {other:?}", m.id),
    }
}

/// The address-select table of a spec: `(address, [(strap role, level)])` rows.
fn address_table(spec: &toml::Value) -> Vec<(i64, Vec<(String, String)>)> {
    spec["sensor"]["address_select"]
        .as_array()
        .expect("address_select")
        .iter()
        .map(|row| {
            let straps = row["straps"]
                .as_table()
                .expect("straps")
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().expect("strap level").to_string()))
                .collect();
            (row["address"].as_integer().expect("address"), straps)
        })
        .collect()
}

/// Resolve by an ordering code given as both value and MPN.
fn ordering(code: &str) -> ModelEntry {
    resolve_query(Some(code), Some(code), None)
        .model
        .unwrap_or_else(|| panic!("{code} did not resolve"))
}

fn assert_close(a: f64, b: f64, what: &str) {
    assert!((a - b).abs() < 1e-12, "{what}: {a} != {b}");
}

fn assert_ads_power(m: &ModelEntry) {
    let p = m.peripheral_power.as_ref().expect("peripheral_power");
    assert_eq!(p.supply_role, "vdd");
    assert_eq!(p.return_role, "gnd");
    assert_close(p.power_on_threshold_v, 2.0, "power-on threshold");
    assert_close(p.idle_a, 150e-6, "operating current");
    assert_close(
        p.low_power_a.expect("power-down current"),
        0.5e-6,
        "power-down",
    );
}

fn assert_ads_registers(m: &ModelEntry) {
    let s = spec(m);
    assert_eq!(s["sensor"]["i2c_address"].as_integer(), Some(0x48));
    let table = address_table(&s);
    assert_eq!(
        table.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
        vec![0x48, 0x49, 0x4A, 0x4B]
    );
    assert!(table
        .iter()
        .all(|(_, st)| st.len() == 1 && st[0].0 == "addr"));
    let levels: Vec<&str> = table.iter().map(|(_, st)| st[0].1.as_str()).collect();
    assert_eq!(levels, ["low", "high", "sda", "scl"]);
    let writes = s["sensor"]["write_register"]
        .as_array()
        .expect("write_register");
    let find = |addr: i64| {
        writes
            .iter()
            .find(|w| w["addr"].as_integer() == Some(addr))
            .unwrap_or_else(|| panic!("no write register {addr:#x}"))
    };
    assert_eq!(find(1)["default"].as_float(), Some(34179.0), "config 8583h");
    assert_eq!(
        find(2)["default"].as_float(),
        Some(-32768.0),
        "Lo_thresh 8000h"
    );
    assert_eq!(
        find(3)["default"].as_float(),
        Some(32767.0),
        "Hi_thresh 7FFFh"
    );
    let regs = s["sensor"]["register"].as_array().expect("register");
    assert!(
        regs.iter().any(|r| r["addr"].as_integer() == Some(0)),
        "conversion register 0"
    );
}

#[test]
fn ads1115_resolves_per_package_with_the_register_map() {
    let cases = [
        ("ADS1115IDGSR", "ads1115_dgs", 182.7),
        ("ADS1115IDGST", "ads1115_dgs", 182.7),
        ("ADS1115IDYNR", "ads1115_dyn", 147.1),
        ("ADS1115IRUGR", "ads1115_rug", 245.2),
        ("ADS1115IRUGT", "ads1115_rug", 245.2),
    ];
    for (code, id, theta) in cases {
        let m = ordering(code);
        assert_pin_map(
            &m,
            id,
            &[
                ("1", "addr"),
                ("2", "alert_rdy"),
                ("3", "gnd"),
                ("4", "ain0"),
                ("5", "ain1"),
                ("6", "ain2"),
                ("7", "ain3"),
                ("8", "vdd"),
                ("9", "sda"),
                ("10", "scl"),
            ],
        );
        assert_ads_power(&m);
        assert_ads_registers(&m);
        assert_eq!(m.ratings.max_voltage_v, Some(7.0), "{code}");
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
    }
}

#[test]
fn ads1015_resolves_per_package_with_the_register_map() {
    for (code, id) in [
        ("ADS1015IDGSR", "ads1015_dgs"),
        ("ADS1015IDGST", "ads1015_dgs"),
        ("ADS1015IRUGR", "ads1015_rug"),
        ("ADS1015IRUGT", "ads1015_rug"),
    ] {
        let m = ordering(code);
        assert_eq!(m.id, id, "{code}");
        assert_ads_power(&m);
        assert_ads_registers(&m);
        assert_eq!(m.pins.len(), 10, "{code}");
    }
}

#[test]
fn ads_siblings_do_not_resolve_to_the_ads1115_or_ads1015_cards() {
    for code in [
        "ADS1113IDGSR",
        "ADS1114IDGSR",
        "ADS1013IDGSR",
        "ADS1014IDGSR",
    ] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("ads1115_") || i.starts_with("ads1015_")),
            "{code} must not resolve to an ADS1115/ADS1015 card, got {id:?}"
        );
    }
    // The 12-bit and 16-bit parts are distinct cards.
    assert_ne!(ordering("ADS1115IDGSR").id, ordering("ADS1015IDGSR").id);
}

#[test]
fn mcp4725_options_resolve_to_their_hard_wired_address_pair() {
    for (code, id, lo, hi) in [
        ("MCP4725A0T-E/CH", "mcp4725_a0", 0x60, 0x61),
        ("MCP4725A1T-E/CH", "mcp4725_a1", 0x62, 0x63),
        ("MCP4725A2T-E/CH", "mcp4725_a2", 0x64, 0x65),
        ("MCP4725A3T-E/CH", "mcp4725_a3", 0x66, 0x67),
    ] {
        let m = ordering(code);
        assert_pin_map(
            &m,
            id,
            &[
                ("1", "vout"),
                ("2", "vss"),
                ("3", "vdd"),
                ("4", "sda"),
                ("5", "scl"),
                ("6", "a0"),
            ],
        );
        let s = spec(&m);
        assert_eq!(s["sensor"]["i2c_address"].as_integer(), Some(lo), "{code}");
        let table = address_table(&s);
        assert_eq!(
            table.iter().map(|(a, _)| *a).collect::<Vec<_>>(),
            vec![lo, hi]
        );
        let levels: Vec<&str> = table.iter().map(|(_, st)| st[0].1.as_str()).collect();
        assert_eq!(levels, ["low", "high"]);
        assert!(table.iter().all(|(_, st)| st.len() == 1 && st[0].0 == "a0"));
        // Factory EEPROM default 800h (mid-scale, normal mode) is the power-up DAC state.
        let states = s["sensor"]["state"].as_array().expect("state");
        let default_of = |name: &str| {
            states
                .iter()
                .find(|st| st["name"].as_str() == Some(name))
                .and_then(|st| st["default"].as_float())
                .unwrap_or_else(|| panic!("{code}: state {name}"))
        };
        assert_eq!(default_of("code"), 2048.0);
        assert_eq!(default_of("ecode"), 2048.0);
        assert_eq!(default_of("pd"), 0.0);
        let p = m.peripheral_power.as_ref().expect("peripheral_power");
        assert_eq!(p.supply_role, "vdd");
        assert_eq!(p.return_role, "vss");
        assert_close(p.power_on_threshold_v, 2.0, "VPOR");
        assert_close(p.idle_a, 210e-6, "IDD");
        assert_close(p.low_power_a.expect("IDDP"), 0.06e-6, "IDDP");
        assert_eq!(m.ratings.max_voltage_v, Some(6.5), "{code}");
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(190.5), "{code}");
    }
}

#[test]
fn mcp3208_and_mcp3204_resolve_per_package_with_the_pad_map() {
    let pins_3208 = [
        ("1", "ch0"),
        ("2", "ch1"),
        ("3", "ch2"),
        ("4", "ch3"),
        ("5", "ch4"),
        ("6", "ch5"),
        ("7", "ch6"),
        ("8", "ch7"),
        ("9", "dgnd"),
        ("10", "cs"),
        ("11", "mosi"),
        ("12", "miso"),
        ("13", "sck"),
        ("14", "agnd"),
        ("15", "vref"),
        ("16", "vdd"),
    ];
    let pins_3204 = [
        ("1", "ch0"),
        ("2", "ch1"),
        ("3", "ch2"),
        ("4", "ch3"),
        ("5", "nc"),
        ("6", "nc"),
        ("7", "dgnd"),
        ("8", "cs"),
        ("9", "mosi"),
        ("10", "miso"),
        ("11", "sck"),
        ("12", "agnd"),
        ("13", "vref"),
        ("14", "vdd"),
    ];
    for (code, id, theta) in [
        ("MCP3208-BI/P", "mcp3208_pdip16", 70.0),
        ("MCP3208-CI/SL", "mcp3208_soic16", 86.1),
        ("MCP3208T-CI/SL", "mcp3208_soic16", 86.1),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &pins_3208);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert!(
            m.peripheral.is_none(),
            "{code}: the SPI framing is not modelled"
        );
    }
    for (code, id, theta) in [
        ("MCP3204-BI/P", "mcp3204_pdip14", 70.0),
        ("MCP3204-BI/SL", "mcp3204_soic14", 95.3),
        ("MCP3204-CI/ST", "mcp3204_tssop14", 100.0),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &pins_3204);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(7.0), "{code}");
    }
    // No TSSOP-16 member exists for the MCP3208: the code binds a variant stand-in, never an exact match.
    let st = resolve_query(Some("MCP3208-CI/ST"), Some("MCP3208-CI/ST"), None);
    assert_ne!(
        st.confidence,
        Confidence::Exact,
        "no card lists MCP3208-CI/ST"
    );
    // The 10-bit parts and other numbers keep their own cards.
    // An unlisted suffix on the same number binds a flagged stand-in, never an exact member.
    let unlisted = resolve_query(Some("MCP3208A-BI/P"), Some("MCP3208A-BI/P"), None);
    assert_ne!(
        unlisted.confidence,
        Confidence::Exact,
        "no card lists MCP3208A-BI/P"
    );
    for code in ["MCP3201-BI/P", "MCP3004-I/P"] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("mcp3208_") || i.starts_with("mcp3204_")),
            "{code} must not resolve to an MCP3204/3208 card, got {id:?}"
        );
    }
    assert_eq!(
        query_id(
            Some("MCP3008-I/SL"),
            Some("MCP3008-I/SL"),
            Some("Package_SO:SOIC-16_3.9x9.9mm_P1.27mm")
        )
        .as_deref(),
        Some("mcp3008")
    );
}

#[test]
fn mcp4725_siblings_do_not_resolve_to_its_cards() {
    for code in ["MCP4728-E/UN", "MCP4726A0T-E/CH", "MCP4706A0T-E/CH"] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("mcp4725_")),
            "{code} must not resolve to an MCP4725 card, got {id:?}"
        );
    }
}

#[test]
fn pcm5102a_resolves_with_the_i2s_pad_map_and_no_bus_model() {
    for code in [
        "PCM5102APW",
        "PCM5102APWR",
        "PCM5102APWG4",
        "PCM5102AQPWRQ1",
    ] {
        let m = ordering(code);
        assert_pin_map(
            &m,
            "pcm5102a",
            &[
                ("1", "cpvdd"),
                ("2", "capp"),
                ("3", "cpgnd"),
                ("4", "capm"),
                ("5", "vneg"),
                ("6", "outl"),
                ("7", "outr"),
                ("8", "avdd"),
                ("9", "agnd"),
                ("10", "demp"),
                ("11", "flt"),
                ("12", "sysclk"),
                ("13", "bck"),
                ("14", "din"),
                ("15", "lrck"),
                ("16", "fmt"),
                ("17", "xsmt"),
                ("18", "ldoo"),
                ("19", "dgnd"),
                ("20", "dvdd"),
            ],
        );
        assert!(m.peripheral.is_none(), "{code}: hardware-strapped, no bus");
        assert_eq!(m.ratings.max_voltage_v, Some(3.9), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(130.0), "{code}");
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(91.2), "{code}");
        assert_eq!(m.params.get_str("i2s_clock_role"), Some("slave"), "{code}");
        assert_eq!(m.params.get_bool("mclk_required"), Some(false), "{code}");
    }
    // The lower-grade siblings and the no-suffix PCM5102 are different silicon.
    for code in ["PCM5101APW", "PCM5100APWR", "PCM5102PW", "PCM5122PW"] {
        let id = query_id(Some(code), Some(code), None);
        assert_ne!(id.as_deref(), Some("pcm5102a"), "{code}");
    }
}

#[test]
fn mcp4921_resolves_per_package_with_the_pad_map() {
    let eight = [
        ("1", "vdd"),
        ("2", "cs"),
        ("3", "sck"),
        ("4", "mosi"),
        ("5", "ldac"),
        ("6", "vref"),
        ("7", "vss"),
        ("8", "vout"),
    ];
    let mut dfn = eight.to_vec();
    dfn.push(("9", "thermal_pad"));
    for (code, id, theta) in [
        ("MCP4921-E/P", "mcp4921_pdip8", 90.0),
        ("MCP4921-E/SN", "mcp4921_soic8", 150.0),
        ("MCP4921-E/SL", "mcp4921_soic8", 150.0),
        ("MCP4921T-E/SL", "mcp4921_soic8", 150.0),
        ("MCP4921-E/MS", "mcp4921_msop8", 211.0),
        ("MCP4921T-E/MS", "mcp4921_msop8", 211.0),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &eight);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert!(
            m.peripheral.is_none(),
            "{code}: write-only SPI is not modelled"
        );
    }
    for code in ["MCP4921-E/MC", "MCP4921T-E/MC"] {
        let m = ordering(code);
        assert_pin_map(&m, "mcp4921_dfn8", &dfn);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(68.0), "{code}");
    }
    assert_eq!(ordering("MCP4921-E/P").ratings.max_voltage_v, Some(6.5));
}

#[test]
fn mcp4922_resolves_per_package_with_the_dual_pad_map() {
    let pins = [
        ("1", "vdd"),
        ("2", "nc"),
        ("3", "cs"),
        ("4", "sck"),
        ("5", "mosi"),
        ("6", "nc"),
        ("7", "nc"),
        ("8", "ldac"),
        ("9", "shdn"),
        ("10", "vout_b"),
        ("11", "vref_b"),
        ("12", "vss"),
        ("13", "vref_a"),
        ("14", "vout_a"),
    ];
    for (code, id, theta) in [
        ("MCP4922-E/P", "mcp4922_pdip14", 70.0),
        ("MCP4922-E/SL", "mcp4922_soic14", 120.0),
        ("MCP4922T-E/SL", "mcp4922_soic14", 120.0),
        ("MCP4922-E/ST", "mcp4922_tssop14", 100.0),
        ("MCP4922T-E/ST", "mcp4922_tssop14", 100.0),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &pins);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(6.5), "{code}");
    }
}

#[test]
fn mcp492x_siblings_do_not_resolve_to_each_others_cards() {
    // The 8-bit and 10-bit parts, the dual/single twins and the 4800 family are other silicon.
    for code in [
        "MCP4901-E/P",
        "MCP4911-E/SN",
        "MCP4902-E/P",
        "MCP4912-E/ST",
        "MCP4821-E/P",
    ] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("mcp4921_") || i.starts_with("mcp4922_")),
            "{code} must not resolve to an MCP4921/4922 card, got {id:?}"
        );
    }
    // MCP4921 has no 14-pin member and MCP4922 has no 8-pin member.
    // Such a code binds a flagged stand-in of its own number, never an exact member.
    for code in ["MCP4921-E/ST", "MCP4922-E/MS", "MCP4922-E/MC"] {
        let r = resolve_query(Some(code), Some(code), None);
        assert_ne!(r.confidence, Confidence::Exact, "no card lists {code}");
    }
}

/// The one 8-pin pad map of every LM2904-family package (TI SLOS068AB Table 4-1).
fn lm2904_pins() -> [(&'static str, &'static str); 8] {
    [
        ("1", "out_a"),
        ("2", "in_minus_a"),
        ("3", "in_plus_a"),
        ("4", "vss"),
        ("5", "in_plus_b"),
        ("6", "in_minus_b"),
        ("7", "out_b"),
        ("8", "vcc"),
    ]
}

/// The two per-channel input common-mode rows: both inputs of a channel, 1.5 V under V+.
fn assert_lm2904_input_common_mode(m: &ModelEntry) {
    let rows: Vec<(&[String], f64)> = m
        .envelope
        .iter()
        .filter_map(|e| match e {
            hauksbee_models::OperatingEnvelope::InputCommonMode {
                supply,
                inputs,
                headroom_v,
                ..
            } if supply == "vcc" => Some((inputs.as_slice(), *headroom_v)),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "{}: one input_common_mode row per channel",
        m.id
    );
    for ((inputs, headroom), ch) in rows.iter().zip(["a", "b"]) {
        assert_close(*headroom, 1.5, "VICR headroom");
        assert!(inputs.contains(&format!("in_plus_{ch}")), "{}", m.id);
        assert!(inputs.contains(&format!("in_minus_{ch}")), "{}", m.id);
    }
}

#[test]
fn lm2904b_resolves_per_package_with_the_pad_map() {
    for (code, id, theta) in [
        ("LM2904BIDR", "lm2904b_d", 124.7),
        ("LM2904BIDRG4", "lm2904b_d", 124.7),
        ("LM2904BAIDR", "lm2904b_d", 124.7),
        ("LM2904BIDR.A", "lm2904b_d", 124.7),
        ("LM2904BIPWR", "lm2904b_pw", 171.7),
        ("LM2904BAIPWR", "lm2904b_pw", 171.7),
        ("LM2904BIDGKR", "lm2904b_dgk", 181.4),
        ("LM2904BAIDGKR", "lm2904b_dgk", 181.4),
        ("LM2904BIDDFR", "lm2904b_ddf", 164.3),
        ("LM2904BAIDDFR", "lm2904b_ddf", 164.3),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &lm2904_pins());
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(40.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_close(
            m.params.get_f64("gain").expect("gain"),
            140_000.0,
            "AOL 140 V/mV",
        );
        assert_close(m.params.get_f64("slew").expect("slew"), 0.5, "SR");
        assert_close(
            m.params.get_f64("swing_hi").expect("swing_hi"),
            1.48,
            "VOH headroom",
        );
        assert_close(
            m.params.get_f64("swing_lo").expect("swing_lo"),
            0.020,
            "VOL headroom",
        );
        assert!(m.params.get_str("warning").is_some(), "{code}");
        assert_lm2904_input_common_mode(&m);
    }
}

#[test]
fn lm2904_resolves_per_package_with_the_pad_map() {
    for (code, id, theta) in [
        ("LM2904DR", "lm2904_d", 124.7),
        ("LM2904DRG3", "lm2904_d", 124.7),
        ("LM2904DRG4", "lm2904_d", 124.7),
        ("LM2904DR.A", "lm2904_d", 124.7),
        ("LM2904PWR", "lm2904_pw", 171.7),
        ("LM2904PW", "lm2904_pw", 171.7),
        ("LM2904PWRG3", "lm2904_pw", 171.7),
        ("LM2904PWRG4-JF", "lm2904_pw", 171.7),
        ("LM2904DGKR", "lm2904_dgk", 181.4),
        ("LM2904P", "lm2904_p", 80.9),
        ("LM2904PE4", "lm2904_p", 80.9),
        ("LM2904PSR", "lm2904_ps", 116.9),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &lm2904_pins());
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(26.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_close(
            m.params.get_f64("gain").expect("gain"),
            100_000.0,
            "AOL 100 V/mV",
        );
        assert_close(m.params.get_f64("slew").expect("slew"), 0.3, "SR");
        assert_close(
            m.params.get_f64("swing_hi").expect("swing_hi"),
            1.5,
            "VOH headroom",
        );
        assert_close(
            m.params.get_f64("swing_lo").expect("swing_lo"),
            0.020,
            "VOL headroom",
        );
        assert!(m.params.get_str("warning").is_some(), "{code}");
        assert_lm2904_input_common_mode(&m);
    }
}

#[test]
fn lm2904v_resolves_per_package_with_the_pad_map() {
    for (code, id, theta) in [
        ("LM2904VQDR", "lm2904v_d", 124.7),
        ("LM2904VQDRG4", "lm2904v_d", 124.7),
        ("LM2904AVQDR", "lm2904v_d", 124.7),
        ("LM2904AVQDRG4", "lm2904v_d", 124.7),
        ("LM2904VQPWR", "lm2904v_pw", 171.7),
        ("LM2904VQPWRG4", "lm2904v_pw", 171.7),
        ("LM2904AVQPWR", "lm2904v_pw", 171.7),
        ("LM2904AVQPWRG4", "lm2904v_pw", 171.7),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &lm2904_pins());
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(32.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_close(
            m.params.get_f64("gain").expect("gain"),
            100_000.0,
            "AOL 100 V/mV",
        );
        assert_close(m.params.get_f64("slew").expect("slew"), 0.3, "SR");
        assert!(m.params.get_str("warning").is_some(), "{code}");
        assert_lm2904_input_common_mode(&m);
    }
}

#[test]
fn lm2904_classes_never_resolve_into_each_others_cards() {
    // The three electrical classes of the datasheet bind their own families only.
    for (code, prefix) in [
        ("LM2904BIDR", "lm2904b_"),
        ("LM2904BAIDDFR", "lm2904b_"),
        ("LM2904DR", "lm2904_"),
        ("LM2904PSR", "lm2904_"),
        ("LM2904VQDR", "lm2904v_"),
        ("LM2904AVQPWR", "lm2904v_"),
    ] {
        let id = query_id(Some(code), Some(code), None).expect(code);
        assert!(
            id.starts_with(prefix),
            "{code} resolved to {id}, want a {prefix} card"
        );
        // "lm2904_" is a prefix of no B or V card id: those read "lm2904b_" and "lm2904v_".
    }
}

#[test]
fn bare_lm2904_names_bind_a_flagged_stand_in_of_their_own_class() {
    // One pad map across every package, so a name pinning no package binds a stand-in of the
    // class its spelling names; no member is determined.
    for (value, prefix) in [
        ("LM2904", "lm2904_"),
        ("LM2904B", "lm2904b_"),
        ("LM2904BA", "lm2904b_"),
        ("LM2904V", "lm2904v_"),
        ("LM2904AV", "lm2904v_"),
    ] {
        let res = resolve_query(Some(value), None, None);
        let id = res.model.as_ref().map(|m| m.id.clone()).expect(value);
        assert!(
            id.starts_with(prefix),
            "{value} resolved to {id}, want a {prefix} card"
        );
        let selection = res.selection.as_ref().expect(value);
        assert!(
            selection.determined.is_none(),
            "{value}: no member may be determined"
        );
    }
}

#[test]
fn lm2904_codes_the_datasheet_does_not_list_do_not_bind_a_member() {
    // The Q-suffixed codes are printed in the addendum but their electrical class is not (they sit
    // in no Electrical Characteristics table), so no family claims them. The A-only name and the
    // typo in the package table are not parts either.
    for code in ["LM2904QDR", "LM2904QDRG4", "LM2904AIDR", "LM2490V"] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("lm2904")),
            "{code} must not resolve to an LM2904 card, got {id:?}"
        );
    }
    // Other members of the SLOS068AB family are other cards or none, never an LM2904 card.
    for code in [
        "LM358BIDR",
        "LM358ADR",
        "LM258DR",
        "LM158JG",
        "LM358BAIDDFR",
    ] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("lm2904")),
            "{code} must not resolve to an LM2904 card, got {id:?}"
        );
    }
    // A package a class is not offered in binds a stand-in of that class, never an exact member:
    // the B class has no PDIP or SO member and the V class no VSSOP member.
    for (code, prefix) in [
        ("LM2904BIP", "lm2904b_"),
        ("LM2904BIPSR", "lm2904b_"),
        ("LM2904VQDGKR", "lm2904v_"),
    ] {
        let res = resolve_query(Some(code), Some(code), None);
        let id = res.model.as_ref().map(|m| m.id.clone()).expect(code);
        assert!(id.starts_with(prefix), "{code} resolved to {id}");
        let selection = res.selection.as_ref().expect(code);
        assert!(
            selection.determined.is_none(),
            "{code}: no member lists this code"
        );
    }
}

/// The pad map of the 14-pin LM2902 packages (D, DB, N, NS, PW), Table 5-1 p.3.
const LM2902_14: [(&str, &str); 14] = [
    ("1", "out_a"),
    ("2", "in_minus_a"),
    ("3", "in_plus_a"),
    ("4", "vcc"),
    ("5", "in_plus_b"),
    ("6", "in_minus_b"),
    ("7", "out_b"),
    ("8", "out_c"),
    ("9", "in_minus_c"),
    ("10", "in_plus_c"),
    ("11", "vss"),
    ("12", "in_plus_d"),
    ("13", "in_minus_d"),
    ("14", "out_d"),
];

/// The pad map of the 16-pin WQFN (RTE) LM2902B, Table 5-2 p.4, plus the thermal pad.
const LM2902_RTE: [(&str, &str); 17] = [
    ("1", "in_plus_a"),
    ("2", "vcc"),
    ("3", "nc"),
    ("4", "in_plus_b"),
    ("5", "in_minus_b"),
    ("6", "out_b"),
    ("7", "out_c"),
    ("8", "in_minus_c"),
    ("9", "in_plus_c"),
    ("10", "nc"),
    ("11", "vss"),
    ("12", "in_plus_d"),
    ("13", "in_minus_d"),
    ("14", "out_d"),
    ("15", "out_a"),
    ("16", "in_minus_a"),
    ("17", "thermal_pad"),
];

#[test]
fn lm2902b_resolves_per_package_with_the_pad_map() {
    for (code, id, theta, pins) in [
        ("LM2902BIDR", "lm2902b_d", 99.3, &LM2902_14[..]),
        ("LM2902BIDR.A", "lm2902b_d", 99.3, &LM2902_14[..]),
        ("LM2902BIPWR", "lm2902b_pw", 124.7, &LM2902_14[..]),
        ("LM2902BIPWR.A", "lm2902b_pw", 124.7, &LM2902_14[..]),
        ("LM2902BIRTER", "lm2902b_rte", 64.9, &LM2902_RTE[..]),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, pins);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(40.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_close(m.params.get_f64("swing_hi").expect("swing_hi"), 1.5, code);
        assert_close(m.params.get_f64("swing_lo").expect("swing_lo"), 0.020, code);
        assert_close(m.params.get_f64("gain").expect("gain"), 100_000.0, code);
        assert!(
            m.params.get_str("warning").is_some(),
            "{code}: honest warning"
        );
    }
}

#[test]
fn lm2902ba_resolves_per_package_with_the_pad_map() {
    for (code, id, theta, pins) in [
        ("LM2902BAIDR", "lm2902ba_d", 99.3, &LM2902_14[..]),
        ("LM2902BAIDR.A", "lm2902ba_d", 99.3, &LM2902_14[..]),
        ("LM2902BAIPWR", "lm2902ba_pw", 124.7, &LM2902_14[..]),
        ("LM2902BAIPWR.A", "lm2902ba_pw", 124.7, &LM2902_14[..]),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, pins);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(40.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_close(m.params.get_f64("swing_hi").expect("swing_hi"), 1.5, code);
        assert_close(m.params.get_f64("swing_lo").expect("swing_lo"), 0.020, code);
        assert_close(m.params.get_f64("gain").expect("gain"), 100_000.0, code);
        assert!(
            m.params.get_str("warning").is_some(),
            "{code}: honest warning"
        );
    }
}

#[test]
fn lm2902_resolves_per_package_with_the_pad_map() {
    for (code, id, theta, pins) in [
        ("LM2902D", "lm2902_d", 99.3, &LM2902_14[..]),
        ("LM2902DR", "lm2902_d", 99.3, &LM2902_14[..]),
        ("LM2902DR.A", "lm2902_d", 99.3, &LM2902_14[..]),
        ("LM2902DRE4", "lm2902_d", 99.3, &LM2902_14[..]),
        ("LM2902DRG3", "lm2902_d", 99.3, &LM2902_14[..]),
        ("LM2902DRG4", "lm2902_d", 99.3, &LM2902_14[..]),
        ("LM2902DRG4.A", "lm2902_d", 99.3, &LM2902_14[..]),
        ("LM2902N", "lm2902_n", 83.5, &LM2902_14[..]),
        ("LM2902N.A", "lm2902_n", 83.5, &LM2902_14[..]),
        ("LM2902NE4", "lm2902_n", 83.5, &LM2902_14[..]),
        ("LM2902NSR", "lm2902_ns", 90.4, &LM2902_14[..]),
        ("LM2902NSR.A", "lm2902_ns", 90.4, &LM2902_14[..]),
        ("LM2902PW", "lm2902_pw", 124.7, &LM2902_14[..]),
        ("LM2902PWR", "lm2902_pw", 124.7, &LM2902_14[..]),
        ("LM2902PWR.A", "lm2902_pw", 124.7, &LM2902_14[..]),
        ("LM2902PWRE4", "lm2902_pw", 124.7, &LM2902_14[..]),
        ("LM2902PWRG3", "lm2902_pw", 124.7, &LM2902_14[..]),
        ("LM2902PWRG4", "lm2902_pw", 124.7, &LM2902_14[..]),
        ("LM2902PWRG4.A", "lm2902_pw", 124.7, &LM2902_14[..]),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, pins);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(26.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_close(m.params.get_f64("swing_hi").expect("swing_hi"), 1.5, code);
        assert_close(m.params.get_f64("swing_lo").expect("swing_lo"), 0.020, code);
        assert_close(m.params.get_f64("gain").expect("gain"), 100_000.0, code);
        assert!(
            m.params.get_str("warning").is_some(),
            "{code}: honest warning"
        );
    }
}

#[test]
fn lm2902k_resolves_per_package_with_the_pad_map() {
    for (code, id, theta, pins) in [
        ("LM2902KD", "lm2902k_d", 99.3, &LM2902_14[..]),
        ("LM2902KDR", "lm2902k_d", 99.3, &LM2902_14[..]),
        ("LM2902KDR.A", "lm2902k_d", 99.3, &LM2902_14[..]),
        ("LM2902KDB", "lm2902k_db", 106.5, &LM2902_14[..]),
        ("LM2902KDB.A", "lm2902k_db", 106.5, &LM2902_14[..]),
        ("LM2902KN", "lm2902k_n", 83.5, &LM2902_14[..]),
        ("LM2902KN.A", "lm2902k_n", 83.5, &LM2902_14[..]),
        ("LM2902KNSR", "lm2902k_ns", 90.4, &LM2902_14[..]),
        ("LM2902KNSR.A", "lm2902k_ns", 90.4, &LM2902_14[..]),
        ("LM2902KNSRG4", "lm2902k_ns", 90.4, &LM2902_14[..]),
        ("LM2902KPW", "lm2902k_pw", 124.7, &LM2902_14[..]),
        ("LM2902KPWR", "lm2902k_pw", 124.7, &LM2902_14[..]),
        ("LM2902KPWR.A", "lm2902k_pw", 124.7, &LM2902_14[..]),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, pins);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(26.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_close(m.params.get_f64("swing_hi").expect("swing_hi"), 1.5, code);
        assert_close(m.params.get_f64("swing_lo").expect("swing_lo"), 0.020, code);
        assert_close(m.params.get_f64("gain").expect("gain"), 100_000.0, code);
        assert!(
            m.params.get_str("warning").is_some(),
            "{code}: honest warning"
        );
    }
}

#[test]
fn lm2902kv_resolves_per_package_with_the_pad_map() {
    for (code, id, theta, pins) in [
        ("LM2902KVQDR", "lm2902kv_d", 99.3, &LM2902_14[..]),
        ("LM2902KVQDR.A", "lm2902kv_d", 99.3, &LM2902_14[..]),
        ("LM2902KVQDRG4", "lm2902kv_d", 99.3, &LM2902_14[..]),
        ("LM2902KVQPWR", "lm2902kv_pw", 124.7, &LM2902_14[..]),
        ("LM2902KVQPWR.A", "lm2902kv_pw", 124.7, &LM2902_14[..]),
        ("LM2902KVQPWRG4", "lm2902kv_pw", 124.7, &LM2902_14[..]),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, pins);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(32.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_close(m.params.get_f64("swing_hi").expect("swing_hi"), 1.5, code);
        assert_close(m.params.get_f64("swing_lo").expect("swing_lo"), 0.020, code);
        assert_close(m.params.get_f64("gain").expect("gain"), 100_000.0, code);
        assert!(
            m.params.get_str("warning").is_some(),
            "{code}: honest warning"
        );
    }
}

#[test]
fn lm2902kav_resolves_per_package_with_the_pad_map() {
    for (code, id, theta, pins) in [
        ("LM2902KAVQDR", "lm2902kav_d", 99.3, &LM2902_14[..]),
        ("LM2902KAVQDR.A", "lm2902kav_d", 99.3, &LM2902_14[..]),
        ("LM2902KAVQDRG4", "lm2902kav_d", 99.3, &LM2902_14[..]),
        ("LM2902KAVQPWR", "lm2902kav_pw", 124.7, &LM2902_14[..]),
        ("LM2902KAVQPWR.A", "lm2902kav_pw", 124.7, &LM2902_14[..]),
        ("LM2902KAVQPWRG4", "lm2902kav_pw", 124.7, &LM2902_14[..]),
        ("LM2902KAVQPWRG4.A", "lm2902kav_pw", 124.7, &LM2902_14[..]),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, pins);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(32.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_close(m.params.get_f64("swing_hi").expect("swing_hi"), 1.5, code);
        assert_close(m.params.get_f64("swing_lo").expect("swing_lo"), 0.020, code);
        assert_close(m.params.get_f64("gain").expect("gain"), 100_000.0, code);
        assert!(
            m.params.get_str("warning").is_some(),
            "{code}: honest warning"
        );
    }
}

#[test]
fn lm2902_siblings_do_not_resolve_to_the_lm2902_cards() {
    // The LM324 and LM2904 families, the LM124/LM224 parts and the automotive Q1 grades are other
    // silicon or other sheets; none may bind an LM2902 card.
    for code in [
        "LM324BIDR",
        "LM324DR",
        "LM324N",
        "LM224N",
        "LM124J",
        "LM2904DR",
        "LM2904BIDR",
        "LM2904KPWR",
        "LM2902-Q1",
        "LM2902BQDR",
        "LM2902BAQDR",
        "LM2902KQDR",
        "LM2902BIDBR",
        "LM2902ZDR",
    ] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("lm2902")),
            "{code} must not resolve to an LM2902 card, got {id:?}"
        );
    }
    // The B class is offered in D, PW and RTE only: an NS-package code pins no member of the set,
    // and the RTE pad map differs, so it stays unbound rather than taking a stand-in.
    let id = query_id(Some("LM2902BINSR"), Some("LM2902BINSR"), None);
    assert!(
        id.is_none(),
        "LM2902BINSR is not an orderable code, got {id:?}"
    );
}

#[test]
fn lm2902_classes_resolve_to_their_own_family_not_a_neighbour() {
    // Prefix classes: a code resolves to the longest-named class it spells.
    for (code, id) in [
        ("LM2902BIDR", "lm2902b_d"),
        ("LM2902BAIDR", "lm2902ba_d"),
        ("LM2902DR", "lm2902_d"),
        ("LM2902KDR", "lm2902k_d"),
        ("LM2902KVQDR", "lm2902kv_d"),
        ("LM2902KAVQDR", "lm2902kav_d"),
    ] {
        assert_eq!(
            query_id(Some(code), Some(code), None).as_deref(),
            Some(id),
            "{code}"
        );
    }
}

#[test]
fn lm386_resolves_per_package_and_grade_with_the_pad_map() {
    use hauksbee_models::OperatingEnvelope::InputTolerance;
    let pins = [
        ("1", "gain1"),
        ("2", "in_minus"),
        ("3", "in_plus"),
        ("4", "gnd"),
        ("5", "out"),
        ("6", "vcc"),
        ("7", "bypass"),
        ("8", "gain8"),
    ];
    // (ordering code, card id, theta_ja, abs-max supply, recommended supply range)
    for (code, id, theta, vabs, range) in [
        ("LM386M-1", "lm386_soic8", 115.7, Some(15.0), (4.0, 12.0)),
        ("LM386MX-1", "lm386_soic8", 115.7, Some(15.0), (4.0, 12.0)),
        (
            "LM386M-1/NOPB",
            "lm386_soic8",
            115.7,
            Some(15.0),
            (4.0, 12.0),
        ),
        (
            "LM386M-1/NOPB.B",
            "lm386_soic8",
            115.7,
            Some(15.0),
            (4.0, 12.0),
        ),
        (
            "LM386MX-1/NOPB",
            "lm386_soic8",
            115.7,
            Some(15.0),
            (4.0, 12.0),
        ),
        (
            "LM386MX-1/NOPB.B",
            "lm386_soic8",
            115.7,
            Some(15.0),
            (4.0, 12.0),
        ),
        ("LM386MMX-1", "lm386_vssop8", 169.3, None, (4.0, 12.0)),
        ("LM386MMX-1/NOPB", "lm386_vssop8", 169.3, None, (4.0, 12.0)),
        (
            "LM386MMX-1/NOPB.B",
            "lm386_vssop8",
            169.3,
            None,
            (4.0, 12.0),
        ),
        ("LM386N-1", "lm386_pdip8_n1", 53.4, Some(15.0), (4.0, 12.0)),
        (
            "LM386N-1/NOPB",
            "lm386_pdip8_n1",
            53.4,
            Some(15.0),
            (4.0, 12.0),
        ),
        (
            "LM386N-1/NOPB.B",
            "lm386_pdip8_n1",
            53.4,
            Some(15.0),
            (4.0, 12.0),
        ),
        ("LM386N-3", "lm386_pdip8_n3", 53.4, Some(15.0), (4.0, 12.0)),
        (
            "LM386N-3/NOPB",
            "lm386_pdip8_n3",
            53.4,
            Some(15.0),
            (4.0, 12.0),
        ),
        (
            "LM386N-3/NOPB.B",
            "lm386_pdip8_n3",
            53.4,
            Some(15.0),
            (4.0, 12.0),
        ),
        (
            "LM386N-3/NOPBG4",
            "lm386_pdip8_n3",
            53.4,
            Some(15.0),
            (4.0, 12.0),
        ),
        (
            "LM386N-3/NOPBG4.B",
            "lm386_pdip8_n3",
            53.4,
            Some(15.0),
            (4.0, 12.0),
        ),
        ("LM386N-4", "lm386_pdip8_n4", 53.4, Some(22.0), (5.0, 18.0)),
        (
            "LM386N-4/NOPB",
            "lm386_pdip8_n4",
            53.4,
            Some(22.0),
            (5.0, 18.0),
        ),
        (
            "LM386N-4/NOPB.B",
            "lm386_pdip8_n4",
            53.4,
            Some(22.0),
            (5.0, 18.0),
        ),
        (
            "LM386N-4/NOPBG4",
            "lm386_pdip8_n4",
            53.4,
            Some(22.0),
            (5.0, 18.0),
        ),
        (
            "LM386N-4/NOPBG4.B",
            "lm386_pdip8_n4",
            53.4,
            Some(22.0),
            (5.0, 18.0),
        ),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &pins);
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_voltage_v, vabs, "{code}");
        assert!(
            super::has_supply_range(&m, "vcc", range, vabs, "SNAS545D"),
            "{code}: supply range {range:?} on vcc"
        );
        // A fixed-gain, self-biased power amplifier is not an ideal op-amp: the card is
        // identity-only and never stamps an op-amp or any output.
        assert_eq!(m.kind, hauksbee_models::ComponentKind::Digital, "{code}");
        assert_eq!(m.params.get_bool("identity_only"), Some(true), "{code}");
        assert!(m.params.get_f64("gain").is_none(), "{code}: no gain param");
        assert!(m.peripheral.is_none(), "{code}");
        // The input ceiling of 0.4 V is on both inputs.
        assert!(
            m.envelope.iter().any(|e| matches!(
                e,
                InputTolerance { pins, max_v, .. }
                    if pins.iter().any(|p| p == "in_plus")
                        && pins.iter().any(|p| p == "in_minus")
                        && (*max_v - 0.4).abs() < 1e-12
            )),
            "{code}: input_tolerance on in_plus/in_minus"
        );
    }
}

#[test]
fn lm386_siblings_and_other_audio_amps_do_not_resolve_to_its_cards() {
    // Other audio power amplifiers are different silicon, and so are look-alike numbers.
    for code in [
        "LM3886TF", "LM380N", "LM384N", "LM387N", "LM3860", "LM1875T",
    ] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("lm386_")),
            "{code} must not resolve to an LM386 card, got {id:?}"
        );
    }
}

#[test]
fn max4466_resolves_per_package_as_an_opamp_with_the_pad_map() {
    use hauksbee_models::OperatingEnvelope::InputCommonMode;
    let pins = [
        ("1", "in_plus"),
        ("2", "vss"),
        ("3", "in_minus"),
        ("4", "out"),
        ("5", "vcc"),
    ];
    for (code, id, theta, power) in [
        ("MAX4466EXK-T", "max4466_sc70", 400.0, 0.2),
        ("MAX4466EUK-T", "max4466_sot23", 140.8, 0.571),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &pins);
        assert_eq!(m.kind, hauksbee_models::ComponentKind::Opamp, "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(6.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
        assert_eq!(m.ratings.max_power_w, Some(power), "{code}");
        assert!(
            super::has_supply_range(&m, "vcc", (2.4, 5.5), Some(6.0), "19-1950"),
            "{code}: supply range on vcc"
        );
        assert_close(
            m.params.get_f64("gain").expect("gain"),
            1_778_279.41,
            "gain",
        );
        assert_close(
            m.params.get_f64("pole_hz").expect("pole_hz"),
            0.3374,
            "pole_hz",
        );
        assert_close(m.params.get_f64("slew").expect("slew"), 0.3, "slew");
        assert_close(
            m.params.get_f64("swing_hi").expect("swing_hi"),
            0.05,
            "swing_hi",
        );
        assert_close(
            m.params.get_f64("swing_lo").expect("swing_lo"),
            0.05,
            "swing_lo",
        );
        assert!(
            m.envelope.iter().any(|e| matches!(
                e,
                InputCommonMode { supply, inputs, headroom_v, .. }
                    if supply == "vcc"
                        && inputs.iter().any(|p| p == "in_plus")
                        && inputs.iter().any(|p| p == "in_minus")
                        && (*headroom_v - 0.1).abs() < 1e-12
            )),
            "{code}: input_common_mode headroom 0.1 V"
        );
    }
}

#[test]
fn max4466_siblings_do_not_resolve_to_its_cards() {
    // The unity-gain-stable MAX4465/4467/4469 and the shutdown MAX4468 are other parts of the same
    // datasheet (different gain stability, shutdown pin, dual die, 8-pin maps).
    for code in [
        "MAX4465EXK-T",
        "MAX4465EUK-T",
        "MAX4467EKA-T",
        "MAX4467ESA",
        "MAX4468EKA-T",
        "MAX4468ESA",
        "MAX4469EKA-T",
        "MAX4469ESA",
        "MAX44660",
    ] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("max4466_")),
            "{code} must not resolve to a MAX4466 card, got {id:?}"
        );
    }
}

/// TLV9061 5-pin SOT-23 (DBV) pad map (TI SBOS839N Table 5-1).
fn tlv9061_pins_sot23_5() -> [(&'static str, &'static str); 5] {
    [
        ("1", "out"),
        ("2", "vss"),
        ("3", "in_plus"),
        ("4", "in_minus"),
        ("5", "vcc"),
    ]
}

/// TLV9061 5-pin SC70 (DCK) pad map (TI SBOS839N Table 5-1).
fn tlv9061_pins_sc70_5() -> [(&'static str, &'static str); 5] {
    [
        ("1", "in_plus"),
        ("2", "vss"),
        ("3", "in_minus"),
        ("4", "out"),
        ("5", "vcc"),
    ]
}

/// TLV9061 5-pin X2SON (DPW) pad map (TI SBOS839N Table 5-1).
fn tlv9061_pins_x2son_5() -> [(&'static str, &'static str); 5] {
    [
        ("1", "out"),
        ("2", "in_minus"),
        ("3", "vss"),
        ("4", "in_plus"),
        ("5", "vcc"),
    ]
}

/// The one 8-pin pad map of the TLV9062 D, DGK, PW and DDF packages (TI SBOS839N Table 5-3).
fn tlv9062_pins_8() -> [(&'static str, &'static str); 8] {
    [
        ("1", "out_a"),
        ("2", "in_minus_a"),
        ("3", "in_plus_a"),
        ("4", "vss"),
        ("5", "in_plus_b"),
        ("6", "in_minus_b"),
        ("7", "out_b"),
        ("8", "vcc"),
    ]
}

/// The TLV9062 DSG WSON pad map: the 8-pin map plus the exposed pad, tied to V- (Figure 5-7).
fn tlv9062_pins_dsg() -> [(&'static str, &'static str); 9] {
    [
        ("1", "out_a"),
        ("2", "in_minus_a"),
        ("3", "in_plus_a"),
        ("4", "vss"),
        ("5", "in_plus_b"),
        ("6", "in_minus_b"),
        ("7", "out_b"),
        ("8", "vcc"),
        ("9", "vss"),
    ]
}

/// The TLV9064 D and PW 14-pin pad map (TI SBOS839N Table 5-5, SOIC and TSSOP column).
fn tlv9064_pins_14() -> [(&'static str, &'static str); 14] {
    [
        ("1", "out_a"),
        ("2", "in_minus_a"),
        ("3", "in_plus_a"),
        ("4", "vcc"),
        ("5", "in_plus_b"),
        ("6", "in_minus_b"),
        ("7", "out_b"),
        ("8", "out_c"),
        ("9", "in_minus_c"),
        ("10", "in_plus_c"),
        ("11", "vss"),
        ("12", "in_plus_d"),
        ("13", "in_minus_d"),
        ("14", "out_d"),
    ]
}

/// The TLV9064 RUC X2QFN-14 pad map (TI SBOS839N Table 5-5, X2QFN column).
fn tlv9064_pins_ruc() -> [(&'static str, &'static str); 14] {
    [
        ("1", "in_minus_a"),
        ("2", "in_plus_a"),
        ("3", "vcc"),
        ("4", "in_plus_b"),
        ("5", "in_minus_b"),
        ("6", "out_b"),
        ("7", "out_c"),
        ("8", "in_minus_c"),
        ("9", "in_plus_c"),
        ("10", "vss"),
        ("11", "in_plus_d"),
        ("12", "in_minus_d"),
        ("13", "out_d"),
        ("14", "out_a"),
    ]
}

/// The TLV9064 RTE WQFN-16 pad map: NC pads 6 and 7 and the exposed pad tied to V-
/// (TI SBOS839N Table 5-5 WQFN column, Figure 5-12).
fn tlv9064_pins_rte() -> [(&'static str, &'static str); 17] {
    [
        ("1", "in_plus_a"),
        ("2", "vcc"),
        ("3", "in_plus_b"),
        ("4", "in_minus_b"),
        ("5", "out_b"),
        ("6", "nc"),
        ("7", "nc"),
        ("8", "out_c"),
        ("9", "in_minus_c"),
        ("10", "in_plus_c"),
        ("11", "vss"),
        ("12", "in_plus_d"),
        ("13", "in_minus_d"),
        ("14", "out_d"),
        ("15", "out_a"),
        ("16", "in_minus_a"),
        ("17", "vss"),
    ]
}

/// The electrical class shared by every TLV906x card: rail-to-rail in and out, 1.8 to 5.5 V.
fn assert_tlv906x_class(m: &ModelEntry, theta: f64) {
    let code = &m.id;
    assert_eq!(m.ratings.theta_ja_c_per_w, Some(theta), "{code}");
    assert_eq!(m.ratings.max_voltage_v, Some(6.0), "{code}");
    assert_eq!(m.ratings.max_pin_current_a, Some(0.01), "{code}");
    assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
    assert_close(
        m.params.get_f64("gain").expect("gain"),
        3_162_278.0,
        "AOL 130 dB",
    );
    assert_close(m.params.get_f64("slew").expect("slew"), 6.5, "SR");
    assert_close(
        m.params.get_f64("swing_hi").expect("swing_hi"),
        0.02,
        "VOH headroom",
    );
    assert_close(
        m.params.get_f64("swing_lo").expect("swing_lo"),
        0.02,
        "VOL headroom",
    );
    assert!(m.params.get_str("warning").is_some(), "{code}");
    // Rail-to-rail input: no supply-relative input ceiling to check, only the supply window.
    assert!(
        !m.envelope.iter().any(|e| matches!(
            e,
            hauksbee_models::OperatingEnvelope::InputCommonMode { .. }
        )),
        "{code}: a rail-to-rail input has no input_common_mode row"
    );
    let supply: Vec<(f64, f64, Option<f64>)> = m
        .envelope
        .iter()
        .filter_map(|e| match e {
            hauksbee_models::OperatingEnvelope::SupplyRange {
                pin,
                min_v,
                max_v,
                abs_max_v,
                ..
            } if pin == "vcc" => Some((*min_v, *max_v, *abs_max_v)),
            _ => None,
        })
        .collect();
    assert_eq!(supply, vec![(1.8, 5.5, Some(6.0))], "{code}");
}

#[test]
fn tlv9061_resolves_per_package_with_the_pad_map() {
    for (code, id, theta) in [
        ("TLV9061IDBVR", "tlv9061_dbv", 221.7),
        ("TLV9061IDBVRG4", "tlv9061_dbv", 221.7),
        ("TLV9061IDBVR.A", "tlv9061_dbv", 221.7),
        ("TLV9061IDBVRG4.A", "tlv9061_dbv", 221.7),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &tlv9061_pins_sot23_5());
        assert_tlv906x_class(&m, theta);
    }
    let m = ordering("TLV9061IDCKR");
    assert_pin_map(&m, "tlv9061_dck", &tlv9061_pins_sc70_5());
    assert_tlv906x_class(&m, 263.3);
    let m = ordering("TLV9061IDCKR.A");
    assert_pin_map(&m, "tlv9061_dck", &tlv9061_pins_sc70_5());
    let m = ordering("TLV9061IDPWR");
    assert_pin_map(&m, "tlv9061_dpw", &tlv9061_pins_x2son_5());
    assert_tlv906x_class(&m, 467.0);
    let m = ordering("TLV9061IDPWR.A");
    assert_pin_map(&m, "tlv9061_dpw", &tlv9061_pins_x2son_5());
}

#[test]
fn tlv9062_resolves_per_package_with_the_pad_map() {
    for (code, id, theta) in [
        ("TLV9062IDR", "tlv9062_d", 157.6),
        ("TLV9062IDRG4", "tlv9062_d", 157.6),
        ("TLV9062IDR.A", "tlv9062_d", 157.6),
        ("TLV9062IDGKR", "tlv9062_dgk", 201.2),
        ("TLV9062IDGKRG4", "tlv9062_dgk", 201.2),
        ("TLV9062IDGKT", "tlv9062_dgk", 201.2),
        ("TLV9062IDGKT.A", "tlv9062_dgk", 201.2),
        ("TLV9062IPWR", "tlv9062_pw", 205.1),
        ("TLV9062IPWR.A", "tlv9062_pw", 205.1),
        ("TLV9062IDDFR", "tlv9062_ddf", 184.4),
        ("TLV9062IDDFR.A", "tlv9062_ddf", 184.4),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &tlv9062_pins_8());
        assert_tlv906x_class(&m, theta);
    }
    for code in [
        "TLV9062IDSGR",
        "TLV9062IDSGRG4",
        "TLV9062IDSGT",
        "TLV9062IDSGRG4.A",
    ] {
        let m = ordering(code);
        assert_pin_map(&m, "tlv9062_dsg", &tlv9062_pins_dsg());
        assert_tlv906x_class(&m, 94.4);
    }
}

#[test]
fn tlv9064_resolves_per_package_with_the_pad_map() {
    for (code, id, theta) in [
        ("TLV9064IDR", "tlv9064_d", 106.9),
        ("TLV9064IDR.A", "tlv9064_d", 106.9),
        ("TLV9064IPWR", "tlv9064_pw", 135.8),
        ("TLV9064IPWT", "tlv9064_pw", 135.8),
        ("TLV9064IPWT.A", "tlv9064_pw", 135.8),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &tlv9064_pins_14());
        assert_tlv906x_class(&m, theta);
    }
    for code in ["TLV9064IRUCR", "TLV9064IRUCRG4", "TLV9064IRUCRG4.A"] {
        let m = ordering(code);
        assert_pin_map(&m, "tlv9064_ruc", &tlv9064_pins_ruc());
        assert_tlv906x_class(&m, 205.5);
    }
    for code in ["TLV9064IRTER", "TLV9064IRTER.A"] {
        let m = ordering(code);
        assert_pin_map(&m, "tlv9064_rte", &tlv9064_pins_rte());
        assert_tlv906x_class(&m, 65.1);
    }
}

#[test]
fn tlv906x_families_never_resolve_into_each_others_cards() {
    // The family match is anchored on the digit, so 9061, 9062 and 9064 each bind their own cards.
    for (code, prefix) in [
        ("TLV9061IDBVR", "tlv9061_"),
        ("TLV9061IDPWR", "tlv9061_"),
        ("TLV9062IDR", "tlv9062_"),
        ("TLV9062IDSGT", "tlv9062_"),
        ("TLV9064IPWT", "tlv9064_"),
        ("TLV9064IRTER", "tlv9064_"),
    ] {
        let id = query_id(Some(code), Some(code), None).expect(code);
        assert!(
            id.starts_with(prefix),
            "{code} resolved to {id}, want a {prefix} card"
        );
    }
}

#[test]
fn bare_tlv906x_names_never_determine_a_package_member() {
    // A name pinning no package must not determine a member: the packages carry different pad
    // maps, so the resolver may bind nothing, but never an exact member or another device's card.
    for (value, prefix) in [
        ("TLV9061", "tlv9061_"),
        ("TLV9062", "tlv9062_"),
        ("TLV9064", "tlv9064_"),
    ] {
        let res = resolve_query(Some(value), None, None);
        if let Some(m) = res.model.as_ref() {
            assert!(
                m.id.starts_with(prefix),
                "{value} resolved to {}, want a {prefix} card",
                m.id
            );
            assert_ne!(
                res.confidence,
                Confidence::Exact,
                "{value}: a bare name is never an exact match"
            );
        }
        if let Some(selection) = res.selection.as_ref() {
            assert!(
                selection.determined.is_none(),
                "{value}: no member may be determined"
            );
        }
    }
}

#[test]
fn tlv906x_codes_the_datasheet_does_not_list_do_not_bind_a_member() {
    // The shutdown parts (TLV906xS, different pins), the automotive -Q1 parts and the other TLV
    // families are other parts and never bind a TLV906x card.
    for code in [
        "TLV9061SIDBVR",
        "TLV9062SIDGSR",
        "TLV9062SIRUGR",
        "TLV9062SIYCKR",
        "TLV9064SIRTER",
        "TLV9061S",
        "TLV9062S",
        "TLV9064S",
        "TLV9061-Q1",
        "TLV9062-Q1",
        "TLV9064-Q1",
        "TLV9041",
        "TLV9102",
        "TLV9002IDR",
        "TLV9060",
        "TLV9065",
    ] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(id.as_deref(), Some(i) if i.starts_with("tlv906")),
            "{code} must not resolve to a TLV906x card, got {id:?}"
        );
    }
    // A package a device is not offered in (the DRL SOT-553 is preview-only and prints no
    // orderable code, TLV9064 has no VSSOP) binds a stand-in, never an exact member.
    for (code, prefix) in [("TLV9061IDRLR", "tlv9061_"), ("TLV9064IDGKR", "tlv9064_")] {
        let res = resolve_query(Some(code), Some(code), None);
        if let Some(m) = res.model.as_ref() {
            assert!(m.id.starts_with(prefix), "{code} resolved to {}", m.id);
            assert_ne!(res.confidence, Confidence::Exact, "{code}: not exact");
        }
        if let Some(selection) = res.selection.as_ref() {
            assert!(
                selection.determined.is_none(),
                "{code}: no member lists this code"
            );
        }
    }
}

/// The 8-channel 4051 pad map (HC: Table 4-1 p.3; CD4051B: Table 4-1 p.3). Pad 8 is GND (HC) or VSS (CD).
fn mux4051_pins(
    neg_rail: &'static str,
    pos_rail: &'static str,
    ground: &'static str,
    en: &'static str,
) -> Vec<(&'static str, &'static str)> {
    vec![
        ("1", "ch4"),
        ("2", "ch6"),
        ("3", "com"),
        ("4", "ch7"),
        ("5", "ch5"),
        ("6", en),
        ("7", neg_rail),
        ("8", ground),
        ("9", "sel2"),
        ("10", "sel1"),
        ("11", "sel0"),
        ("12", "ch3"),
        ("13", "ch0"),
        ("14", "ch1"),
        ("15", "ch2"),
        ("16", pos_rail),
    ]
}

/// The dual 4-channel 4052 pad map. `x`/`y` name the two sections (HC: a/b with `ch_a0`; CD: x/y).
fn mux4052_pins(
    first: &'static str,
    second: &'static str,
    ground: &'static str,
    pos_rail: &'static str,
    en: &'static str,
) -> Vec<(String, String)> {
    let ch = |sec: &str, n: u8| format!("ch_{sec}{n}");
    vec![
        ("1".to_string(), ch(second, 0)),
        ("2".to_string(), ch(second, 2)),
        ("3".to_string(), format!("com_{second}")),
        ("4".to_string(), ch(second, 3)),
        ("5".to_string(), ch(second, 1)),
        ("6".to_string(), en.to_string()),
        ("7".to_string(), "vee".to_string()),
        ("8".to_string(), ground.to_string()),
        ("9".to_string(), "sel1".to_string()),
        ("10".to_string(), "sel0".to_string()),
        ("11".to_string(), ch(first, 3)),
        ("12".to_string(), ch(first, 0)),
        ("13".to_string(), format!("com_{first}")),
        ("14".to_string(), ch(first, 1)),
        ("15".to_string(), ch(first, 2)),
        ("16".to_string(), pos_rail.to_string()),
    ]
}

fn assert_mux_pin_map(m: &ModelEntry, id: &str, pins: &[(String, String)]) {
    let refs: Vec<(&str, &str)> = pins.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    assert_pin_map(m, id, &refs);
}

#[test]
fn cd74hc4051_resolves_per_package_with_the_pad_map() {
    let pins = mux4051_pins("vee", "vcc", "gnd", "en_n");
    for (code, id, theta) in [
        ("CD74HC4051E", "cd74hc4051_pdip16", Some(77.3)),
        ("CD74HC4051EE4", "cd74hc4051_pdip16", Some(77.3)),
        ("CD74HC4051M96", "cd74hc4051_soic16", None),
        ("CD74HC4051M96G4", "cd74hc4051_soic16", None),
        ("CD74HC4051NSR", "cd74hc4051_so16", Some(99.3)),
        ("CD74HC4051NSRE4", "cd74hc4051_so16", Some(99.3)),
        ("CD74HC4051PWR", "cd74hc4051_tssop16", Some(116.5)),
        ("CD74HC4051PWT", "cd74hc4051_tssop16", Some(116.5)),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &pins);
        assert_eq!(m.kind, hauksbee_models::ComponentKind::Digital, "{code}");
        assert_eq!(m.params.get_bool("identity_only"), Some(true), "{code}");
        assert_eq!(m.ratings.theta_ja_c_per_w, theta, "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(7.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(150.0), "{code}");
        assert_eq!(m.ratings.max_pin_current_a, Some(0.025), "{code}");
        assert!(
            m.params.get_str("warning").is_some(),
            "{code}: honest warning"
        );
        assert!(
            super::has_supply_range(&m, "vcc", (2.0, 6.0), Some(7.0), "SCHS122O"),
            "{code}: vcc 2-6 V, abs max 7 V"
        );
    }
}

#[test]
fn cd74hc4052_resolves_per_package_with_the_pad_map() {
    let pins = mux4052_pins("a", "b", "gnd", "vcc", "en_n");
    for (code, id) in [
        ("CD74HC4052E", "cd74hc4052_pdip16"),
        ("CD74HC4052M96", "cd74hc4052_soic16"),
        ("CD74HC4052MT", "cd74hc4052_soic16"),
        ("CD74HC4052NSR", "cd74hc4052_so16"),
        ("CD74HC4052PW", "cd74hc4052_tssop16"),
        ("CD74HC4052PWR", "cd74hc4052_tssop16"),
    ] {
        let m = ordering(code);
        assert_mux_pin_map(&m, id, &pins);
        assert_eq!(m.ratings.max_voltage_v, Some(7.0), "{code}");
        // The p.7 thermal table is headed CD74HC4051 only, so no CD74HC4052 value is printed.
        assert_eq!(m.ratings.theta_ja_c_per_w, None, "{code}");
    }
}

#[test]
fn cd4051b_resolves_per_package_with_the_pad_map() {
    let pins = mux4051_pins("vee", "vdd", "vss", "inh");
    for (code, id, theta, tj) in [
        ("CD4051BE", "cd4051b_pdip16", Some(67.0), 150.0),
        ("CD4051BEE4", "cd4051b_pdip16", Some(67.0), 150.0),
        ("CD4051BF3A", "cd4051b_cdip16", None, 175.0),
        ("CD4051BM96", "cd4051b_soic16", Some(73.0), 150.0),
        ("CD4051BMT", "cd4051b_soic16", Some(73.0), 150.0),
        ("CD4051BNSR", "cd4051b_so16", Some(64.0), 150.0),
        ("CD4051BPWR", "cd4051b_tssop16", Some(116.5), 150.0),
        ("CD4051BPW", "cd4051b_tssop16", Some(116.5), 150.0),
    ] {
        let m = ordering(code);
        assert_pin_map(&m, id, &pins);
        assert_eq!(m.kind, hauksbee_models::ComponentKind::Digital, "{code}");
        assert_eq!(m.params.get_bool("identity_only"), Some(true), "{code}");
        assert_eq!(m.ratings.theta_ja_c_per_w, theta, "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(20.0), "{code}");
        assert_eq!(m.ratings.max_junction_temp_c, Some(tj), "{code}");
        assert!(
            super::has_supply_range(&m, "vdd", (3.0, 20.0), Some(20.0), "SCHS047O"),
            "{code}: vdd 3-20 V"
        );
    }
}

#[test]
fn cd4052b_resolves_per_package_with_the_pad_map() {
    let pins = mux4052_pins("x", "y", "vss", "vdd", "inh");
    for (code, id, theta) in [
        ("CD4052BE", "cd4052b_pdip16", Some(67.0)),
        ("CD4052BF3A", "cd4052b_cdip16", None),
        ("CD4052BM96", "cd4052b_soic16", Some(73.0)),
        ("CD4052BM961G4", "cd4052b_soic16", Some(73.0)),
        ("CD4052BNSR", "cd4052b_so16", Some(64.0)),
        ("CD4052BPWR", "cd4052b_tssop16", Some(116.5)),
    ] {
        let m = ordering(code);
        assert_mux_pin_map(&m, id, &pins);
        assert_eq!(m.ratings.theta_ja_c_per_w, theta, "{code}");
        assert_eq!(m.ratings.max_voltage_v, Some(20.0), "{code}");
    }
}

#[test]
fn mux_siblings_and_other_vendors_do_not_resolve_to_the_ti_mux_cards() {
    // HCT types, the 4053, the 4066/4067 and the unbuffered legacy series are other silicon or other
    // sheets; the 74HC405x of other vendors has other supply limits.
    for code in [
        "CD74HCT4051E",
        "CD74HCT4052M96",
        "CD74HC4053E",
        "CD74HC4051-Q1",
        "CD74HC40511",
        "74HC4051D",
        "74HC4052D",
        "CD4053BE",
        "CD4051AE",
        "CD4067BE",
        "CD74HC4066E",
    ] {
        let id = query_id(Some(code), Some(code), None);
        assert!(
            !matches!(
                id.as_deref(),
                Some(i) if i.starts_with("cd74hc4051_")
                    || i.starts_with("cd74hc4052_")
                    || i.starts_with("cd4051b_")
                    || i.starts_with("cd4052b_")
            ),
            "{code} must not resolve to a 4051/4052 card, got {id:?}"
        );
    }
    // The 4051 and 4052 never swap cards.
    for (code, prefix) in [
        ("CD74HC4051E", "cd74hc4051_"),
        ("CD74HC4052E", "cd74hc4052_"),
        ("CD4051BE", "cd4051b_"),
        ("CD4052BE", "cd4052b_"),
    ] {
        let id = query_id(Some(code), Some(code), None).expect(code);
        assert!(id.starts_with(prefix), "{code} resolved to {id}");
    }
}
