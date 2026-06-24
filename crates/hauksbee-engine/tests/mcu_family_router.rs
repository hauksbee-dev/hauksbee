use hauksbee_engine::binder::bind_board;
use hauksbee_engine::report::BindOutcome;
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_models::ModelLibrary;

fn net(id: i64, name: &str) -> Net {
    Net {
        id,
        name: name.to_string(),
    }
}

fn pin(number: &str, net: Option<i64>, function: &str) -> Pin {
    Pin {
        number: number.to_string(),
        net,
        function: function.to_string(),
        kind: String::new(),
        position: None,
    }
}

fn component(reference: &str, value: &str, lib_id: &str, pins: Vec<Pin>) -> Component {
    Component {
        reference: reference.to_string(),
        value: value.to_string(),
        lib_id: lib_id.to_string(),
        footprint: String::new(),
        position: None,
        layer: String::new(),
        properties: Vec::new(),
        dnp: false,
        pins,
    }
}

fn board_with(component: Component) -> ExtractedBoard {
    ExtractedBoard {
        name: "router-test".to_string(),
        nets: vec![
            net(1, "GPIO_A0"),
            net(2, "GPIO_A1"),
            net(3, "GPIO_B3"),
            net(4, "BOOT"),
            net(5, "+3V3"),
            net(6, "GND"),
        ],
        components: vec![component],
    }
}

#[test]
fn stm32f4_without_db_model_routes_to_renode_and_derives_gpio_roles() {
    let board = board_with(component(
        "U5",
        "STM32F411CEU6",
        "MCU_ST_STM32F4:STM32F411CEUx",
        vec![
            pin("10", Some(1), "PA0"),
            pin("11", Some(2), "PA1"),
            pin("12", Some(3), "PB3"),
            pin("44", Some(4), "BOOT0"),
            pin("24", Some(5), "VDD"),
            pin("23", Some(6), "VSS"),
        ],
    ));

    let bound = bind_board(&board, &ModelLibrary::builtin());

    assert_eq!(bound.mcus.len(), 1);
    let mcu = &bound.mcus[0];
    assert_eq!(mcu.backend, "renode:stm32f4");
    assert_eq!(mcu.pad_roles.get("10").map(String::as_str), Some("pa0"));
    assert_eq!(mcu.pad_roles.get("11").map(String::as_str), Some("pa1"));
    assert_eq!(mcu.pad_roles.get("12").map(String::as_str), Some("pb3"));
    assert_eq!(mcu.pad_roles.get("44").map(String::as_str), Some("boot0"));
    assert_eq!(mcu.pad_roles.get("24").map(String::as_str), Some("vdd"));
    assert_eq!(mcu.pad_roles.get("23").map(String::as_str), Some("vss"));
    assert!(mcu.gpio_drivers.contains_key(&('A', 0)));
    assert!(mcu.gpio_drivers.contains_key(&('A', 1)));
    assert!(mcu.gpio_drivers.contains_key(&('B', 3)));
}

#[test]
fn explicit_models_dir_model_wins_over_family_router_backend_and_pins() {
    let dir = std::env::temp_dir().join(format!("hauksbee_router_override_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let toml = r#"
[[models]]
id = "override_stm32f411"
kind = "mcu"
description = "manual override"
[models.match]
value_re = "(?i)^STM32F411CEU6$"
[models.params]
backend = "renode:stm32f103"
[models.pins]
"10" = "pc13"
"11" = "vdd"
"12" = "vss"
"#;
    std::fs::write(dir.join("mcu.toml"), toml).unwrap();

    let board = board_with(component(
        "U5",
        "STM32F411CEU6",
        "MCU_ST_STM32F4:STM32F411CEUx",
        vec![
            pin("10", Some(1), "PA0"),
            pin("11", Some(5), "VDD"),
            pin("12", Some(6), "VSS"),
        ],
    ));
    let lib = ModelLibrary::builtin_with_user_dirs(&[dir.as_path()]);
    let bound = bind_board(&board, &lib);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(bound.mcus.len(), 1);
    let mcu = &bound.mcus[0];
    assert_eq!(mcu.backend, "renode:stm32f103");
    assert_eq!(mcu.pad_roles.get("10").map(String::as_str), Some("pc13"));
    assert!(!mcu.pad_roles.values().any(|role| role == "pa0"));
}

#[test]
fn recognized_family_without_platform_and_non_mcu_parts_stay_unresolved() {
    let s2_board = board_with(component(
        "U2",
        "ESP32-S2-MINI-1",
        "RF_Module:ESP32-S2-MINI-1",
        vec![pin("1", Some(1), "GPIO1")],
    ));
    let s2 = bind_board(&s2_board, &ModelLibrary::builtin());
    assert!(s2.mcus.is_empty());
    assert!(matches!(
        s2.report.rows[0].outcome,
        BindOutcome::Unresolved { .. }
    ));

    let resistor_board = board_with(component(
        "R1",
        "STM32F411CEU6",
        "Device:R",
        vec![pin("1", Some(1), "PA0"), pin("2", Some(6), "VSS")],
    ));
    let resistor = bind_board(&resistor_board, &ModelLibrary::builtin());
    assert!(resistor.mcus.is_empty());
    assert!(matches!(
        resistor.report.rows[0].outcome,
        BindOutcome::Unresolved { .. }
    ));
}

#[test]
fn bare_pcb_routes_backend_but_does_not_guess_pin_numbers_as_gpio_roles() {
    let board = board_with(component(
        "U5",
        "STM32F411CEU6",
        "MCU_ST_STM32F4:STM32F411CEUx",
        vec![
            pin("10", Some(1), ""),
            pin("11", Some(2), ""),
            pin("12", Some(3), ""),
        ],
    ));

    let bound = bind_board(&board, &ModelLibrary::builtin());

    assert_eq!(bound.mcus.len(), 1);
    let mcu = &bound.mcus[0];
    assert_eq!(mcu.backend, "renode:stm32f4");
    assert!(mcu.pad_roles.is_empty());
    assert!(mcu.gpio_drivers.is_empty());
    let warning = bound.report.rows[0].warning.as_deref().unwrap_or("");
    assert!(
        warning.contains("GPIO map cannot be derived")
            && warning.contains("no schematic pin names"),
        "warning was {warning:?}"
    );
}
