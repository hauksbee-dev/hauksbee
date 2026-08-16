//! Reference-board models must describe the executable slice that coverage,
//! the browser, MCP and authoring tools present to a user. Merely resolving a
//! model kind is not an assertion that the entire datasheet is simulated.

use hauksbee_models::{ComponentQuery, ModelLibrary};

fn resolve(value: &str) -> hauksbee_models::Resolution {
    ModelLibrary::builtin().resolve(&ComponentQuery {
        value: Some(value.into()),
        // Layout-only inputs promote the value to the MPN slot when no BOM
        // property exists. Mirror the production query shape: match rules are
        // conjunctive, so leaving this empty would make exact value+MPN cards
        // look unresolved only in the test.
        mpn: Some(value.into()),
        ..Default::default()
    })
}

#[test]
fn multi_board_reference_models_declare_runs_and_omissions() {
    for (value, id, implemented, missing) in [
        (
            "ATmega328P",
            "atmega328p",
            "avr_firmware_execution",
            "board_crystal_frequency_binding",
        ),
        (
            "STM32F103C8T6",
            "stm32f103c8",
            "i2c1_controller_bridge",
            "adc_injection",
        ),
        (
            "ESP32-WROOM-32",
            "esp32_wroom",
            "xtensa_lx6_firmware_execution",
            "native_adc_peripheral",
        ),
        (
            "ESP32-S3",
            "esp32s3",
            "xtensa_lx7_machine_boot",
            "gpio_32_to_48_observation",
        ),
        (
            "TP4054",
            "tp4054",
            "program_resistor_static_charge_current",
            "cc_cv_state_machine",
        ),
        (
            "SR2HARU",
            "sr2",
            "open_drain_output_without_fabricated_high_drive",
            "smart_reset_timer",
        ),
        (
            "RT908033",
            "rt9080_3v3",
            "nominal_dc_regulation",
            "enable_gating",
        ),
        (
            "USBLC6-2P6",
            "usblc6_2",
            "positive_io_steering_clamps",
            "io_capacitance",
        ),
        (
            "BMA423",
            "bma423",
            "performance_mode_supply_current",
            "full_register_map",
        ),
    ] {
        let resolution = resolve(value);
        let model = resolution
            .model
            .as_ref()
            .unwrap_or_else(|| panic!("{value} did not resolve"));
        assert_eq!(model.id, id, "{value} resolved to a different model");
        assert!(
            model
                .coverage
                .implements
                .iter()
                .any(|item| item == implemented),
            "{id} did not declare implemented capability {implemented}"
        );
        assert!(
            model.coverage.missing.iter().any(|item| item == missing),
            "{id} hid missing capability {missing}"
        );
    }
}
