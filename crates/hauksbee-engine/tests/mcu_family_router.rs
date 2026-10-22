use hauksbee_engine::binder::bind_board;
use hauksbee_engine::reports::bind::BindOutcome;
use hauksbee_extract::{Component, ExtractedBoard, Net, Pin};
use hauksbee_models::ModelLibrary;

fn net(id: i64, name: &str) -> Net {
    crate::support::net(id, name.to_string())
}

fn pin(number: &str, net: Option<i64>, function: &str) -> Pin {
    crate::support::pin(number, net, function, "")
}

fn component(reference: &str, value: &str, lib_id: &str, pins: Vec<Pin>) -> Component {
    crate::support::component(
        reference.to_string(),
        value.to_string(),
        lib_id.to_string(),
        String::new(),
        pins,
    )
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
fn shipped_stm32f072_model_binds_the_exact_backend_and_adc_roles() {
    let board = board_with(component(
        "U5",
        "STM32F072CB",
        "MCU_ST_STM32F0:STM32F072CBTx",
        vec![
            pin("10", Some(1), "PA0"),
            pin("13", Some(2), "PA3"),
            pin("18", Some(3), "PB0"),
            pin("44", Some(4), "BOOT0"),
            pin("24", Some(5), "VDD"),
            pin("23", Some(6), "VSS"),
        ],
    ));

    let bound = bind_board(&board, &ModelLibrary::builtin());

    assert_eq!(bound.mcus.len(), 1);
    let mcu = &bound.mcus[0];
    assert_eq!(mcu.backend, "renode:stm32f072");
    assert_eq!(
        mcu.pad_roles.get("10").map(String::as_str),
        Some("pa0_adc0")
    );
    assert_eq!(
        mcu.pad_roles.get("13").map(String::as_str),
        Some("pa3_adc3")
    );
    assert_eq!(
        mcu.pad_roles.get("18").map(String::as_str),
        Some("pb0_adc8")
    );
    assert_eq!(mcu.adc_nets.len(), 3);
    assert!(mcu.adc_nets.contains_key(&0));
    assert!(mcu.adc_nets.contains_key(&3));
    assert!(mcu.adc_nets.contains_key(&8));
    assert!(matches!(
        bound.report.rows[0].outcome,
        BindOutcome::Mcu { ref backend } if backend == "renode:stm32f072"
    ));
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
        "ESP32-S2-SOLO-2",
        "RF_Module:ESP32-S2-SOLO-2",
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

// Regression: the builtin esp32s3 DB entry matches value "ESP32-S3" (Watchy
// v3's MCU), so the family-router fallback never runs for it. The bind must
// still land on the QEMU Xtensa backend; the old silent
// `.unwrap_or("simavr:atmega328p")` default sent an ESP32-S3 into the AVR
// path (wrong ISA, and the GPL-gated `avr` feature the GPL-free build
// excludes).
#[test]
fn esp32s3_db_model_binds_qemu_backend_not_avr() {
    let board = board_with(component(
        "U1",
        "ESP32-S3",
        "RF_Module:ESP32-S3-WROOM-1",
        vec![
            pin("27", Some(4), "GPIO0"),
            pin("2", Some(5), "3V3"),
            pin("1", Some(6), "GND"),
        ],
    ));

    let bound = bind_board(&board, &ModelLibrary::builtin());

    assert_eq!(bound.mcus.len(), 1);
    let mcu = &bound.mcus[0];
    assert_eq!(
        mcu.backend, "qemu:esp32s3",
        "ESP32-S3 must co-sim through the Espressif-QEMU Xtensa backend, \
         never fall back to a simavr AVR core"
    );
    assert!(matches!(
        bound.report.rows[0].outcome,
        BindOutcome::Mcu { ref backend } if backend == "qemu:esp32s3"
    ));
}

// A processor on a MODULE footprint binds through the module card's pad map:
// the ESP32-S3-MINI-1 numbers its pads (3V3 on 3, IO0 on 4, IO45 on 41, EN on
// 45) differently from the bare chip and from the WROOM-1 (IO0 on 27), so a
// board that carries the module must get the roles from the card, with the
// module's pad names as the board prints them.
#[test]
fn a_module_footprint_binds_its_processor_through_the_module_cards_pad_map() {
    let board = board_with(component(
        "U1",
        "ESP32-S3-MINI-1",
        "RF_Module:ESP32-S3-MINI-1",
        vec![
            pin("4", Some(4), "IO0"),
            pin("3", Some(5), "3V3"),
            pin("1", Some(6), "GND"),
            pin("45", Some(5), "EN"),
            pin("41", Some(1), "IO45"),
        ],
    ));

    let bound = bind_board(&board, &ModelLibrary::builtin());

    assert_eq!(bound.mcus.len(), 1, "{:#?}", bound.report.rows);
    let mcu = &bound.mcus[0];
    assert_eq!(mcu.backend, "qemu:esp32s3");
    for (pad, role) in [
        ("4", "gpio0"),
        ("3", "vdd"),
        ("1", "gnd"),
        ("45", "en"),
        ("41", "gpio45"),
    ] {
        assert_eq!(
            mcu.pad_roles.get(pad).map(String::as_str),
            Some(role),
            "pad {pad}"
        );
    }
}

// Regression: a DB-resolved MCU entry with no `backend` param for a family
// the router knows has NO co-sim platform (ESP32-S2) must not silently
// default to `simavr:atmega328p`. The binding stays (straps lint etc. still
// work) but the backend must be an explicit no-platform token the scheduler
// refuses loudly, not a wrong-ISA AVR core.
#[test]
fn esp32s2_db_model_does_not_default_to_avr_backend() {
    // "ESP32-S2-WROOM" matches the builtin esp32s2 DB entry's value_re, so
    // (unlike the -SOLO-2 variant in the unresolved test above) the model DB
    // resolves it and the family-router fallback never runs.
    let board = board_with(component(
        "U2",
        "ESP32-S2-WROOM",
        "RF_Module:ESP32-S2-WROOM",
        vec![pin("27", Some(4), "GPIO0")],
    ));

    let bound = bind_board(&board, &ModelLibrary::builtin());

    assert_eq!(bound.mcus.len(), 1);
    assert!(
        !bound.mcus[0].backend.starts_with("simavr"),
        "ESP32-S2 (no co-sim platform) must never inherit the AVR backend; got {:?}",
        bound.mcus[0].backend
    );
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

/// A card with no pin map binds an MCU only through the board's own pin
/// names. When those name no supply pin, nothing can power the part, and
/// accepting the bind would hand the solver an unpowered MCU (and report the
/// part as covered): it must abstain, saying why. The same card on a board
/// that names its supply pins still binds.
#[test]
fn pinless_card_with_no_named_supply_abstains_with_a_reason() {
    let dir = std::env::temp_dir().join(format!("hauksbee_pinless_mcu_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let toml = r#"
[[models]]
id = "pinless_module"
kind = "mcu"
description = "module card without a pin map"
[models.match]
value_re = "(?i)^PINLESS-MODULE$"
[models.params]
backend = "renode:stm32f103"
module = true
"#;
    std::fs::write(dir.join("mcu.toml"), toml).unwrap();
    let lib = ModelLibrary::builtin_with_user_dirs(&[dir.as_path()]);

    let unnamed = board_with(component(
        "M1",
        "PINLESS-MODULE",
        "",
        vec![
            pin("1", Some(5), ""),
            pin("2", Some(6), ""),
            pin("3", Some(1), ""),
        ],
    ));
    let named = board_with(component(
        "M1",
        "PINLESS-MODULE",
        "",
        vec![
            pin("1", Some(5), "VDD"),
            pin("2", Some(6), "GND"),
            pin("3", Some(1), "PA0"),
        ],
    ));
    let abstained = bind_board(&unnamed, &lib);
    let powered = bind_board(&named, &lib);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(abstained.mcus.is_empty());
    match &abstained.report.rows[0].outcome {
        BindOutcome::Unresolved { reason } => {
            assert!(reason.contains("no pin map"), "{reason}");
            assert!(reason.contains("no supply pin"), "{reason}");
        }
        other => panic!("expected an abstention, got {other:?}"),
    }
    assert_eq!(powered.mcus.len(), 1, "named supply pins still bind");

    // A bare chip (not a module) keeps binding through its own pin names.
    let chip = board_with(component(
        "U5",
        "STM32F411CEU6",
        "MCU_ST_STM32F4:STM32F411CEUx",
        vec![pin("10", Some(1), "PA0")],
    ));
    assert_eq!(bind_board(&chip, &ModelLibrary::builtin()).mcus.len(), 1);
}
