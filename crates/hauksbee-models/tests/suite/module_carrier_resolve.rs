//! Carrier-module bind coverage: boards built on a Raspberry Pi Pico or an
//! Electrosmith Daisy Seed used to bind UNRESOLVED even though their bare
//! chips (RP2040, STM32H750) would have bound, costing identification and
//! lint coverage. These tests pin the module entries: the common KiCad/BOM
//! value spellings resolve, the entry names the MCU it wraps, and the co-sim
//! story stays honest: the Pico carries the real `renode:rp2040` backend it can
//! now back, the Daisy Seed carries a "none:<family>" token because no STM32H7
//! descriptor exists. Either way, never a silent fall-through to the wrong-ISA
//! simavr default and never an invented emulator string.

use super::{assert_pins, expect_query, pins, resolve_query};
use hauksbee_models::ComponentKind;

fn resolve_value(value: &str) -> Option<hauksbee_models::ModelEntry> {
    super::resolve_value(value).model
}

// ── RP2040 package map ───────────────────────────────────────────────────────

#[test]
fn rp2040_package_map_preserves_identity_fallback_and_scope() {
    let identity = resolve_value("RP2040").expect("bare chip identity");
    assert!(identity
        .coverage
        .missing
        .iter()
        .any(|cap| cap == "missing_external_crystal_startup_validation"));
    for (footprint, full) in [
        (None, false),
        (Some(""), false),
        (Some("unknown"), false),
        (Some("QFN-48"), false),
        (Some("QFN-560"), false),
        (Some("TQFP-56"), false),
        (Some("WQFN-56"), false),
        (Some("QFN-56-2EP"), false),
        (Some("QFN-56_8x8mm_P0.5mm"), false),
        (Some("Module:RaspberryPi_Pico"), false),
        (Some("QFN-56"), true),
        (Some("qfn56"), true),
        (Some("QFN_56_1EP"), true),
        (Some("QFN-56-1EP_7x7mm_P0.4mm"), true),
        (
            Some("Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm"),
            true,
        ),
    ] {
        // Package uncertainty must not discard chip identity.
        let model = expect_query(Some("RP2040"), None, footprint);
        assert_eq!(
            model.id,
            if full { "rp2040_qfn56" } else { "rp2040" },
            "{footprint:?}"
        );
        // The identity entry maps QSPI_SS and RUN, the two pads its straps name, and DVDD
        // (pad 23), the supply its brown-out threshold watches.
        assert_eq!(model.pins.len(), if full { 57 } else { 3 });
        assert_eq!(
            model.params.get_str("backend"),
            identity.params.get_str("backend")
        );
        assert_eq!(model.coverage, identity.coverage);
        assert_eq!(model.straps, identity.straps);
    }
    for value in ["RP2350", "RP2040_PLATFORM", "Pico", "RP2040 clone"] {
        let resolved = resolve_query(Some(value), None, Some("QFN-56"));
        assert!(
            resolved.model.is_none_or(|m| m.id != "rp2040_qfn56"),
            "{value}"
        );
    }
}

#[test]
fn rp2040_qfn_map_covers_sio_adc_and_non_gpio_pads() {
    let model = expect_query(Some("RP2040"), None, Some("QFN-56"));
    assert_eq!(model.pins.len(), 57);
    assert!((1..=57).all(|pad| model.pins.contains_key(&pad.to_string())));
    assert_eq!(
        model
            .pins
            .values()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        57
    );
    // Table 615: pad33 is IOVDD, not GPIO22; the final GPIOs start on pad34.
    for (gpio, pad) in (2..=9)
        .chain(11..=18)
        .chain(27..=32)
        .chain(34..=41)
        .enumerate()
    {
        let role = if gpio < 26 {
            format!("gpio{gpio}")
        } else {
            format!("gpio{gpio}_adc{}", gpio - 26)
        };
        assert_eq!(model.pins[&pad.to_string()], role);
    }
    assert_eq!(
        model
            .pins
            .values()
            .filter(|role| role.starts_with("gpio"))
            .count(),
        30
    );
    for (pad, role) in [
        ("19", "testen"),
        ("20", "osc_in"),
        ("21", "osc_out"),
        ("24", "swclk"),
        ("25", "swd"),
        ("26", "run"),
        ("33", "iovdd4"),
        ("43", "adc_avdd"),
        ("44", "vreg_vin"),
        ("45", "vreg_vout"),
        ("46", "usb_dm"),
        ("47", "usb_dp"),
        ("48", "usb_vdd"),
        ("51", "qspi_sd3"),
        ("52", "qspi_sclk"),
        ("53", "qspi_sd0"),
        ("54", "qspi_sd2"),
        ("55", "qspi_sd1"),
        ("56", "qspi_ss"),
        ("57", "gnd"),
    ] {
        assert_eq!(model.pins[pad], role, "pad {pad}");
    }
}

// ── Raspberry Pi Pico ────────────────────────────────────────────────────────

/// The Raspberry Pi product code (SC0918 = Pico W) is what distributor BOMs
/// carry as the value; it must bind, not go UNRESOLVED.
#[test]
fn pico_product_code_sc0918_binds_with_rp2040_cosim_backend() {
    let model = resolve_value("SC0918").expect("SC0918 (Pico W) should resolve");
    assert_eq!(model.id, "rpi_pico");
    assert_eq!(model.kind, ComponentKind::Mcu);
    // The module routes to the same descriptor as the bare chip: RP2040 co-sim
    // is real (support-bundle platform, proven GPIO/UART/ADC/I2C), so the entry
    // names the backend rather than the honest-refusal none: token it carried
    // while no platform existed.
    assert_eq!(model.params.get_str("backend"), Some("renode:rp2040"));
    // And never the Arduino-header role mapping, which d-name modules opt into.
    assert_ne!(model.params.get_bool("module"), Some(true));
}

/// The common schematic value spellings all land on the same entry.
#[test]
fn pico_value_spellings_bind() {
    for value in [
        "Pico",
        "RPi_Pico",
        "RPi_Pico_WH",
        "Raspberry Pi Pico",
        "SC0915",
    ] {
        let model = resolve_value(value)
            .unwrap_or_else(|| panic!("{value:?} should resolve, not UNRESOLVED"));
        assert_eq!(model.id, "rpi_pico", "{value:?} bound the wrong entry");
    }
}

/// Anchoring: near-miss values must NOT false-bind to the Pico module. The
/// ESP32-PICO-D4 has its own entry, and the RP2350-based "Pico 2" is a
/// different chip this db does not model, binding it here would claim a
/// pinout and an identity that are both wrong.
#[test]
fn pico_match_does_not_false_bind_lookalikes() {
    let esp = resolve_value("ESP32-PICO-D4").expect("ESP32-PICO-D4 should resolve");
    assert_eq!(esp.id, "esp32_pico");
    for value in ["Pico 2", "Pico2", "RPi_Pico_2"] {
        if let Some(model) = resolve_value(value) {
            assert_ne!(
                model.id, "rpi_pico",
                "{value:?} (RP2350) must not bind the RP2040 Pico entry"
            );
        }
    }
}

/// The header map is at module level: GP0/GP1 (UART0 defaults) on pins 1/2,
/// power on 39/40, and no strap table (BOOTSEL never reaches the header).
#[test]
fn pico_header_pin_map_and_strapless_module() {
    let model = resolve_value("SC0918").expect("Pico W should resolve");
    assert_eq!(model.pins.get("1").map(String::as_str), Some("gpio0"));
    assert_eq!(model.pins.get("2").map(String::as_str), Some("gpio1"));
    assert_eq!(model.pins.get("26").map(String::as_str), Some("gpio20"));
    assert_eq!(model.pins.get("30").map(String::as_str), Some("run"));
    assert_eq!(model.pins.get("39").map(String::as_str), Some("vsys"));
    assert_eq!(model.pins.get("40").map(String::as_str), Some("vbus"));
    assert!(
        model.straps.is_empty(),
        "the QSPI_SS/BOOTSEL strap lives on the module, not the header"
    );
}

// ── Electrosmith Daisy Seed ──────────────────────────────────────────────────

/// Both value spellings bind, name the wrapped MCU family honestly, and carry
/// the none: token (no STM32H7 descriptor exists in hauksbee-mcu).
#[test]
fn daisy_seed_binds_with_honest_no_cosim_backend() {
    for value in ["Daisy Seed", "Daisy_Seed", "DaisySeed"] {
        let model = resolve_value(value)
            .unwrap_or_else(|| panic!("{value:?} should resolve, not UNRESOLVED"));
        assert_eq!(model.id, "daisy_seed", "{value:?} bound the wrong entry");
        assert_eq!(model.kind, ComponentKind::Mcu);
        assert_eq!(model.params.get_str("backend"), Some("none:stm32h7"));
        assert_ne!(model.params.get_bool("module"), Some(true));
    }
}

/// Header map spot checks against the Daisy Seed datasheet v1.0.5 Table 2:
/// pin 1 = PB12 (D0), pin 14 = PB6/USART1 TX (D13), pins 39/40 = VIN/DGND.
#[test]
fn daisy_seed_header_pin_map_matches_datasheet() {
    let model = resolve_value("Daisy_Seed").expect("Daisy_Seed should resolve");
    let header = pins("1=pb12 14=pb6_usart1_tx 39=vin 40=gnd");
    assert_pins(&model, &header);
    assert!(
        model.straps.is_empty(),
        "the H750 BOOT pin stays on-module (BOOT button); no header strap"
    );
}

/// The Seed2 DFM is a different module with a different pinout; it must not
/// inherit this entry's map.
#[test]
fn daisy_seed2_dfm_does_not_bind_the_seed_entry() {
    if let Some(model) = resolve_value("Daisy_Seed2_DFM") {
        assert_ne!(model.id, "daisy_seed");
    }
}

/// ESP pad maps are per package (db/mcu.toml family note): the footprint picks
/// the module or bare-chip map (spot checks from the cited datasheet tables),
/// and an unrecognised package keeps the family identity and backend but no
/// pad map or strap table, so no strap is examined on a wrong pad.
#[test]
fn esp_strap_maps_follow_the_package() {
    for row in [
        "ESP32-S3|QFN-56-1EP_7x7mm_P0.4mm_EP5.6x5.6mm|esp32s3_qfn56|5=gpio0 51=gpio45 52=gpio46 20=vdd",
        "ESP32-S3|RF_Module:ESP32-S3-WROOM-1|esp32s3_wroom|27=gpio0 26=gpio45 16=gpio46 2=vdd",
        "ESP32-S2-WROOM|RF_Module:ESP32-S2-WROVER|esp32s2_wroom|3=gpio0 39=gpio45 40=gpio46 2=vdd",
        "ESP32-S2|QFN-56-1EP_7x7mm_P0.4mm_EP4x4mm|esp32s2_qfn56|5=gpio0 50=gpio45 55=gpio46 27=vdd",
        "ESP32-C3-MINI-1|RF_Module:ESP32-C3-MINI-1|esp32c3_mini1|5=gpio2 22=gpio8 23=gpio9 3=vdd",
        "ESP32-C3|RF_Module:ESP32-C3-WROOM-02|esp32c3_wroom02|16=gpio2 7=gpio8 8=gpio9 1=vdd",
        "ESP32-C3|QFN-32-1EP_5x5mm_P0.5mm_EP3.7x3.7mm|esp32c3_qfn32|6=gpio2 14=gpio8 15=gpio9 17=vdd",
        // Ordering-code value and the EasyEDA/LCSC footprint spelling.
        "ESP32-C3FH4|lcsc:QFN-32_L5.0-W5.0-P0.50-BL-EP3.7|esp32c3_qfn32|6=gpio2 14=gpio8 15=gpio9 17=vdd",
        "ESP32-D0WD-V3|QFN-48-1EP_5x5mm_P0.35mm_EP3.7x3.7mm|esp32_qfn48|23=gpio0 22=gpio2 19=vdd",
        "ESP32-WROOM-32E||esp32_wroom|25=gpio0 24=gpio2 35=p01_txd 34=p03_rxd",
        "ESP32-PICO-D4||esp32_pico|23=gpio0 22=gpio2 19=vdd",
    ] {
        let [value, footprint, id, pins] = row.split('|').collect::<Vec<_>>()[..] else {
            panic!("{row}")
        };
        let model = expect_query(Some(value), None, Some(footprint));
        assert_eq!(model.id, id, "{row}");
        assert_pins(&model, &super::pins(pins));
        assert!(!model.straps.is_empty(), "{row}");
    }
    for (value, backend) in [
        ("ESP32-S3", "qemu:esp32s3"),
        ("ESP32-S2", ""),
        ("ESP32-C3", "qemu:esp32c3"),
        ("ESP32", "qemu:esp32"),
    ] {
        for footprint in [None, Some("Package_DFN_QFN:QFN-64-1EP_9x9mm_P0.5mm")] {
            let model = expect_query(Some(value), None, footprint);
            assert_eq!(model.params.get_str("backend").unwrap_or(""), backend);
            assert!(
                model.pins.is_empty() && model.straps.is_empty(),
                "{}",
                model.id
            );
        }
    }
}
