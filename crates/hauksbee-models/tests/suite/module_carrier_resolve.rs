//! Carrier-module bind coverage: boards built on a Raspberry Pi Pico or an
//! Electrosmith Daisy Seed used to bind UNRESOLVED even though their bare
//! chips (RP2040, STM32H750) would have bound, costing identification and
//! lint coverage. These tests pin the module entries: the common KiCad/BOM
//! value spellings resolve, the entry names the MCU it wraps, and the co-sim
//! story stays honest, a "none:<family>" backend token (no emulator models
//! either part in this tool) instead of a silent fall-through to the
//! wrong-ISA simavr default or an invented emulator string.

use hauksbee_models::{ComponentKind, ComponentQuery, ModelLibrary};

fn resolve_value(value: &str) -> Option<hauksbee_models::ModelEntry> {
    let lib = ModelLibrary::builtin();
    lib.resolve(&ComponentQuery {
        value: Some(value.to_string()),
        ..Default::default()
    })
    .model
}

// ── Raspberry Pi Pico ────────────────────────────────────────────────────────

/// The Raspberry Pi product code (SC0918 = Pico W) is what distributor BOMs
/// carry as the value; it must bind, not go UNRESOLVED.
#[test]
fn pico_product_code_sc0918_binds_with_honest_no_cosim_backend() {
    let model = resolve_value("SC0918").expect("SC0918 (Pico W) should resolve");
    assert_eq!(model.id, "rpi_pico");
    assert_eq!(model.kind, ComponentKind::Mcu);
    // Honesty: nothing in this tool emulates the RP2040, so the entry must say
    // so with the explicit none: token (the scheduler refuses it loudly),
    // never a real backend string it cannot back.
    assert_eq!(model.params.get_str("backend"), Some("none:rp2040"));
    // And never the Arduino-header role mapping, which d-name modules opt into.
    assert_ne!(model.params.get_bool("module"), Some(true));
}

/// The common schematic value spellings all land on the same entry.
#[test]
fn pico_value_spellings_bind() {
    for value in ["Pico", "RPi_Pico", "RPi_Pico_WH", "Raspberry Pi Pico", "SC0915"] {
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
