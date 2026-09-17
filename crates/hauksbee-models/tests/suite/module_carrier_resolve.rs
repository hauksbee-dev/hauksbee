//! Carrier-module bind coverage: boards built on a Raspberry Pi Pico or an
//! Electrosmith Daisy Seed used to bind UNRESOLVED even though their bare
//! chips (RP2040, STM32H750) would have bound, costing identification and
//! lint coverage. These tests pin the module entries: the common KiCad/BOM
//! value spellings resolve, the entry names the MCU it wraps, and the co-sim
//! story stays honest: the Pico carries the real `renode:rp2040` backend it can
//! now back, the Daisy Seed carries a "none:<family>" token because no STM32H7
//! descriptor exists. Either way, never a silent fall-through to the wrong-ISA
//! simavr default and never an invented emulator string.

use hauksbee_models::{ComponentKind, ComponentQuery, ModelLibrary};

fn resolve_value(value: &str) -> Option<hauksbee_models::ModelEntry> {
    let lib = ModelLibrary::builtin();
    lib.resolve(&ComponentQuery {
        value: Some(value.to_string()),
        ..Default::default()
    })
    .model
}

// ── RP2040 package map ───────────────────────────────────────────────────────

#[test]
fn rp2040_package_map_preserves_identity_fallback_and_scope() {
    let lib = ModelLibrary::builtin();
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
        let model = lib
            .resolve(&ComponentQuery {
                value: Some("RP2040".into()),
                footprint: footprint.map(str::to_owned),
                ..Default::default()
            })
            .model
            .expect("package uncertainty must not discard chip identity");
        assert_eq!(
            model.id,
            if full { "rp2040_qfn56" } else { "rp2040" },
            "{footprint:?}"
        );
        assert_eq!(model.pins.len(), if full { 57 } else { 1 });
        assert_eq!(
            model.params.get_str("backend"),
            identity.params.get_str("backend")
        );
        assert_eq!(model.coverage, identity.coverage);
        assert_eq!(model.straps, identity.straps);
    }
    for value in ["RP2350", "RP2040_PLATFORM", "Pico", "RP2040 clone"] {
        let resolved = lib.resolve(&ComponentQuery {
            value: Some(value.into()),
            footprint: Some("QFN-56".into()),
            ..Default::default()
        });
        assert!(
            resolved.model.is_none_or(|m| m.id != "rp2040_qfn56"),
            "{value}"
        );
    }
}

#[test]
fn rp2040_qfn_map_covers_sio_adc_and_non_gpio_pads() {
    let model = ModelLibrary::builtin()
        .resolve(&ComponentQuery {
            value: Some("RP2040".into()),
            footprint: Some("QFN-56".into()),
            ..Default::default()
        })
        .model
        .expect("known chip and package");
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
    assert_eq!(model.pins.get("1").map(String::as_str), Some("pb12"));
    assert_eq!(
        model.pins.get("14").map(String::as_str),
        Some("pb6_usart1_tx")
    );
    assert_eq!(model.pins.get("39").map(String::as_str), Some("vin"));
    assert_eq!(model.pins.get("40").map(String::as_str), Some("gnd"));
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
