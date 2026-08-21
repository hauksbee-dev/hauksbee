//! Board-only live paths must not construct an optional MCU backend.
//!
//! These tests intentionally run in the GPL-free `--no-default-features` build:
//! an AVR-labelled board is still a valid static/live board when no firmware is
//! supplied, while an actual firmware launch must keep the explicit fail-closed
//! refusal from the missing `avr` feature.

use std::path::PathBuf;
use std::process::Command;

fn blinky_board() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../hauksbee-ci/examples/boards/blinky.kicad_pcb")
}

#[test]
fn headless_board_only_run_does_not_require_avr() {
    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
        .args([
            "run",
            blinky_board().to_str().expect("board path is utf-8"),
            "--headless",
            "--seconds",
            "0.01",
        ])
        .output()
        .expect("hauksbee binary runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "board-only headless run must complete without an AVR backend: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("compiled without the `avr` feature"),
        "no-firmware path must not instantiate simavr: {stderr}"
    );
}

#[test]
fn live_launch_board_only_does_not_require_avr() {
    let launch = hauksbee_engine::commands::common::schematic_live_launcher();
    let board = include_bytes!("../../hauksbee-ci/examples/boards/blinky.kicad_pcb");
    let mut live = launch("blinky.kicad_pcb", board, None, None)
        .expect("/api/live/launch board-only path must build without AVR");

    // The wire metadata still reports the bound MCU; only its executable core
    // is absent from the board-only scheduler.
    let info = live.engine.board_info();
    assert!(
        info.mcus
            .iter()
            .any(|(_, backend)| backend.starts_with("simavr:")),
        "the board's AVR mapping remains disclosed in live metadata: {:?}",
        info.mcus
    );
    let frame = live.engine.step(0.0);
    assert!(
        !frame.net_voltages.is_empty(),
        "board-only live launch must step without an MCU backend"
    );
}

#[test]
fn engine_board_only_has_no_live_mcu_core() {
    let board = include_bytes!("../../hauksbee-ci/examples/boards/blinky.kicad_pcb");
    let board_text = std::str::from_utf8(board).expect("fixture board is UTF-8");
    let engine = hauksbee_engine::HauksbeeEngine::from_board_file(
        board_text,
        None,
        "/boards/blinky.kicad_pcb",
    )
    .expect("board-only engine must build without AVR");
    assert_eq!(
        engine.scheduler().mcu_count(),
        0,
        "firmware-less operation must not construct any MCU backend"
    );
}

#[cfg(not(feature = "avr"))]
#[test]
fn headless_firmware_run_refuses_without_avr() {
    let firmware =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hauksbee-ci/assets/firmware/demo.hex");
    let out = Command::new(env!("CARGO_BIN_EXE_hauksbee"))
        .args([
            "run",
            blinky_board().to_str().expect("board path is utf-8"),
            "--firmware",
            firmware.to_str().expect("firmware path is utf-8"),
            "--headless",
            "--seconds",
            "0.01",
        ])
        .output()
        .expect("hauksbee binary runs");
    assert_eq!(
        out.status.code(),
        Some(1),
        "firmware-backed AVR run must fail closed without AVR: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("compiled without the `avr` feature"),
        "CLI refusal names the missing backend feature: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(not(feature = "avr"))]
#[test]
fn live_launch_with_firmware_refuses_without_avr() {
    let launch = hauksbee_engine::commands::common::schematic_live_launcher();
    let board = include_bytes!("../../hauksbee-ci/examples/boards/blinky.kicad_pcb");
    let firmware = include_bytes!("../../hauksbee-ci/assets/firmware/demo.hex");
    let error = match launch(
        "blinky.kicad_pcb",
        board,
        Some(("demo.hex", firmware)),
        None,
    ) {
        Ok(_) => panic!("firmware must remain fail-closed without the AVR feature"),
        Err(error) => error,
    };
    assert!(
        error.contains("compiled without the `avr` feature"),
        "refusal names the missing backend feature: {error}"
    );
}
