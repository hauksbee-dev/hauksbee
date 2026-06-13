//! Bind-only check for the STM32 blue pill demo board (no Renode needed).
use hauksbee_engine::binder::bind_board;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use std::path::PathBuf;

fn board_text() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/boards/stm32_bluepill_demo.kicad_pcb");
    std::fs::read_to_string(p).expect("read stm32 board")
}

#[test]
fn stm32_board_binds_with_renode_backend() {
    let board = ExtractedBoard::from_auto(&board_text()).expect("parse stm32 board");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    assert_eq!(bound.mcus.len(), 1, "one STM32 MCU");
    let mcu = &bound.mcus[0];
    assert!(
        mcu.backend.starts_with("renode:stm32f103"),
        "backend is renode:stm32f103, got {:?}",
        mcu.backend
    );
    assert!(
        mcu.gpio_drivers.contains_key(&('A', 5)),
        "PA5 driver present: {:?}",
        mcu.gpio_drivers.keys().collect::<Vec<_>>()
    );
    assert!(
        mcu.gpio_drivers.contains_key(&('C', 13)),
        "PC13 driver present: {:?}",
        mcu.gpio_drivers.keys().collect::<Vec<_>>()
    );
    let has_diode = bound
        .circuit
        .devices
        .iter()
        .any(|d| matches!(d, hauksbee_ir::Device::Diode { .. }));
    assert!(has_diode, "LED stamped as a diode");
}
