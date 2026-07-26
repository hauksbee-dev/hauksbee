//! Test 2: bind the stormduino board (KiCad-5 layout, ATmega328 Uno clone).
//!
//! Asserts that more than 60% of non-ignored components resolve better than
//! Unresolved, and prints the report. As a stretch goal, since the ATmega328P
//! resolves and binds with a fully-connected /D13 net, we run the demo
//! firmware and assert the D13/SCK net toggles.

mod common;

use hauksbee_engine::binder::bind_board;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_models::ModelLibrary;
use hauksbee_server::engine::Engine;

/// The stormduino board, if this machine has the corpus. It is a private
/// board, so it is absent from corpus.toml and from any public checkout.
fn board_path(what: &str) -> Option<std::path::PathBuf> {
    hauksbee_testkit::corpus_or_skip(
        env!("CARGO_MANIFEST_DIR"),
        "stormduino/stormduino Rev2.kicad_pcb",
        what,
    )
}

#[test]
fn stormduino_resolves_over_60pct() {
    let Some(path) = board_path("stormduino_resolves_over_60pct") else {
        return;
    };
    let text = std::fs::read_to_string(&path).unwrap();
    let board = ExtractedBoard::from_auto(&text).expect("parse KiCad-5 PCB");
    let lib = ModelLibrary::builtin();
    let bound = bind_board(&board, &lib);

    // Print the report for the record.
    print!("{}", bound.report.render_table());

    let frac = bound.report.resolved_fraction();
    println!(
        "stormduino: {}/{} non-ignored resolved = {:.1}%, {} mcu(s)",
        bound.report.resolved_count(),
        bound.report.non_ignored_count(),
        frac * 100.0,
        bound.report.mcu_count(),
    );
    assert!(
        frac > 0.60,
        "stormduino resolution {:.1}% should be > 60%",
        frac * 100.0
    );
    assert_eq!(bound.report.mcu_count(), 1, "the ATmega328P binds");
}

/// Stretch goal: the ATmega328P-PU resolves and binds with a connected /D13
/// net, so the demo firmware should make D13 (the SCK / Arduino-D13 net) toggle.
#[test]
fn stormduino_d13_toggles_under_firmware() {
    let Some(path) = board_path("stormduino_d13_toggles_under_firmware") else {
        return;
    };
    let text = std::fs::read_to_string(&path).unwrap();
    let mut engine =
        HauksbeeEngine::from_board_file(&text, Some(&common::demo_firmware()), "/boards/storm")
            .expect("build engine");

    let frame_dt = 1e-3_f64;
    let mut transitions = 0u32;
    let mut prev: Option<bool> = None;
    let mut vmax = f64::NEG_INFINITY;
    let mut vmin = f64::INFINITY;
    for _ in 0..1200 {
        let frame = engine.step(frame_dt);
        let v = *frame
            .net_voltages
            .get("/D13")
            .expect("/D13 net present in frame");
        vmax = vmax.max(v);
        vmin = vmin.min(v);
        let logic = if v > 2.5 {
            Some(true)
        } else if v < 1.5 {
            Some(false)
        } else {
            prev
        };
        if let (Some(p), Some(c)) = (prev, logic) {
            if p != c {
                transitions += 1;
            }
        }
        prev = logic;
    }
    println!("stormduino /D13: {transitions} transitions, range {vmin:.2}..{vmax:.2} V");
    assert!(
        transitions >= 4,
        "D13/SCK net should toggle under demo firmware (got {transitions})"
    );
}
