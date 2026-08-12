//! Production-path regression for the AT28 page-close defect: a tracked board,
//! real AVR instructions, scheduler ownership, and the built-in AT28 model.

#![cfg(feature = "avr")]

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use hauksbee_server::engine::Engine;

fn fixture(path: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata")
        .join(path)
}

#[test]
fn real_avr_port_batch_programs_both_builtin_at28_page_bytes() {
    let board_path = fixture("boards/avr_at28_atomic.kicad_pcb");
    let firmware = fixture("firmware/avr_at28_atomic/atomic.hex");
    assert!(
        board_path.is_file(),
        "tracked board fixture missing: {board_path:?}"
    );
    assert!(
        firmware.is_file(),
        "tracked firmware fixture missing; build it with `make -C testdata/firmware/avr_at28_atomic`: {firmware:?}"
    );

    let board_text = std::fs::read_to_string(&board_path).expect("read tracked board");
    let board = ExtractedBoard::from_auto(&board_text).expect("parse tracked board");
    let bound = bind_board(&board, &ModelLibrary::builtin());
    assert_eq!(
        bound.report.resolved_count(),
        4,
        "{}",
        bound.report.render_table()
    );
    assert_eq!(
        bound.report.mcu_count(),
        1,
        "{}",
        bound.report.render_table()
    );
    assert!(
        bound.report.render_table().contains("eeprom_28c256"),
        "the fixture must resolve the shipped standard-grade AT28 model:\n{}",
        bound.report.render_table()
    );

    let mut engine = HauksbeeEngine::from_bound(bound, Some(&firmware), "/ci")
        .expect("build real AVR + built-in AT28 co-sim");
    let mut pass_v = 0.0;
    for _ in 0..50 {
        let frame = engine.step(1e-3);
        pass_v = frame.net_voltages.get("PASS").copied().unwrap_or(0.0);
        if pass_v > 3.0 {
            break;
        }
    }
    assert!(
        pass_v > 3.0,
        "firmware did not read back both page bytes after tWC; PASS stayed {pass_v:.3} V"
    );
    assert!(
        engine.scheduler().driver_contentions().is_empty(),
        "the real bidirectional data bus must hand off without contention: {:?}",
        engine.scheduler().driver_contentions()
    );
}
