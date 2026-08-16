//! Exact memory/hub cards combine firmware-visible protocol behavior with
//! analogue power behavior and retain machine-readable datasheet citations.

use hauksbee_models::{ComponentQuery, ModelLibrary};

fn resolve(value: &str, footprint: Option<&str>) -> hauksbee_models::Resolution {
    ModelLibrary::builtin().resolve(&ComponentQuery {
        value: Some(value.into()),
        footprint: footprint.map(str::to_string),
        ..Default::default()
    })
}

#[test]
fn at24cs01_combines_i2c_array_and_bus_coupled_source_bound_power() {
    let resolution = resolve("AT24CS01-STUM", Some("Package_TO_SOT_SMD:SOT-23-5"));
    let model = resolution.model.as_ref().expect("exact AT24CS01 resolves");
    assert_eq!(model.id, "at24cs01_sot23_5");
    assert!(model.peripheral.is_some(), "I2C array behavior is present");
    assert!(
        model.behavioral.profiled_loads.is_empty(),
        "standby must not be double-counted by a constant load"
    );
    let power = model
        .peripheral_power
        .as_ref()
        .expect("protocol work is coupled to the supply rail");
    assert_eq!(power.supply_role, "vcc");
    assert_eq!(power.return_role, "gnd");
    assert!((power.power_on_threshold_v - 1.7).abs() < 1e-15);
    assert!((power.idle_a - 6e-6).abs() < 1e-15);
    assert!((power.read_a - 1e-3).abs() < 1e-15);
    assert!((power.write_a - 3e-3).abs() < 1e-15);
    assert!(
        model
            .coverage
            .implements
            .iter()
            .any(|cap| cap == "bus_coupled_active_current"),
        "typed protocol activity now controls the electrical load"
    );
    assert!(resolution.references.iter().any(|reference| {
        reference.sha256.as_deref()
            == Some("2acc10a565652d40103acdd631bf2837433cee652c1b7170e7c39f92c9b3a6f0")
            && reference.locator.contains("4.3")
    }));
}

#[test]
fn w25q128_and_usb2514_retain_exact_sources_and_narrow_behavior() {
    let flash = resolve(
        "W25Q128JVS",
        Some("Pedalboard Library:SOIC-8_5.23x5.23mm_P1.27mm"),
    );
    let flash_model = flash.model.as_ref().expect("exact W25Q128 resolves");
    assert_eq!(flash_model.id, "w25q128jv_soic8");
    assert!(flash_model.peripheral.is_some());
    let flash_power = flash_model
        .peripheral_power
        .as_ref()
        .expect("SPI activity is coupled to flash supply current");
    assert!((flash_power.power_on_threshold_v - 2.7).abs() < 1e-15);
    assert!((flash_power.read_a - 20e-3).abs() < 1e-15);
    assert!((flash_power.write_a - 25e-3).abs() < 1e-15);
    assert_eq!(flash_power.low_power_a, Some(20e-6));
    assert!(flash_model
        .coverage
        .implements
        .iter()
        .any(|capability| capability == "deep_power_down_current_max"));
    assert!(flash.references.iter().any(|reference| {
        reference.sha256.as_deref()
            == Some("809f066e62bcde10b12c2202daf05f4776929ad7dc5f9d3b5131cdcc84502bc1")
    }));

    let hub = resolve(
        "USB2514B_Bi",
        Some("Package_DFN_QFN:QFN-36-1EP_6x6mm_P0.5mm_EP3.7x3.7mm"),
    );
    let hub_model = hub.model.as_ref().expect("exact USB2514B_Bi resolves");
    assert_eq!(hub_model.id, "usb2514b_qfn36_startup");
    for law in [
        "reset_supply_draw",
        "starting_supply_draw",
        "port1_powered_supply_draw",
        "port1_overcurrent_supply_draw",
    ] {
        assert!(
            hub_model
                .behavioral
                .laws
                .iter()
                .any(|entry| entry.name == law),
            "missing state-coupled source-bound law {law}"
        );
    }
    assert!(
        hub_model
            .coverage
            .missing
            .iter()
            .any(|cap| cap == "distributed_supply_current"),
        "lumped all-supplies current must retain its topology caveat"
    );
    assert!(hub.references.iter().any(|reference| {
        reference.sha256.as_deref()
            == Some("4ebdca503ca18f8a7a5d787148fc9f488ccd0b1a75743714aa6920c75e9a4ead")
    }));
}

#[test]
fn near_names_do_not_gain_exact_behavior() {
    for (value, exact_id) in [
        ("AT24CS01-NOT-FITTED", "at24cs01_sot23_5"),
        ("W25Q128-NOT-JV", "w25q128jv_soic8"),
        ("USB2514-NOT-B", "usb2514b_qfn36_startup"),
    ] {
        let resolution = resolve(value, None);
        assert!(
            resolution
                .model
                .as_ref()
                .is_none_or(|model| model.id != exact_id),
            "hostile near-name must not gain exact executable behavior: {value}"
        );
    }
}
