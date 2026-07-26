//! Live-Renode proof for the ADC-coverage honesty surfaces (U3 finding 1).
//!
//! The board (`stm32_adc_divider_demo.kicad_pcb`) is the blue pill demo plus a
//! 10k/10k divider on PA0; the binder maps LQFP-48 pin 10 (`pa0_adc0`) to
//! engine ADC channel 0, so the scheduler pushes the solved TEMP_SENSE voltage
//! into the core every chunk. The stock Renode 1.16.1 STM32F103 platform
//! models NO ADC and the shipped descriptor honestly carries no `[[soc.adc]]`
//! map, so every one of those injections is DROPPED by the backend.
//!
//! Before this round, the only trace was an `eprintln!` that never reached
//! `--json`, `--plain`, or CI: the run reported healthy while `analogRead` fed
//! the firmware nothing. This test proves, against the LIVE Renode:
//!
//!   1. the backend records the drop and the scheduler resolves it to the MCU,
//!      channel, and board net (`adc_dropped()`), and
//!   2. `build_cosim_json` carries it into the machine-readable CosimJson (the
//!      exact struct `hauksbee run --json` serializes), so no machine consumer
//!      can miss it.
//!
//! Skips gracefully when Renode or the STM32 firmware is missing, like every
//! renode_* test.

#![cfg(feature = "renode")]

use hauksbee_engine::HauksbeeEngine;
use hauksbee_mcu::renode::is_available;
use hauksbee_server::engine::Engine;
use std::path::PathBuf;

fn board_text() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/boards/stm32_adc_divider_demo.kicad_pcb");
    std::fs::read_to_string(p).expect("read stm32 adc divider board")
}

fn firmware() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/stm32_blinky/blinky.elf");
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

#[test]
fn dropped_adc_injection_reaches_cosim_json() {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let Some(fw) = firmware() else {
        eprintln!("SKIP: blinky.elf not built");
        return;
    };

    let mut engine = HauksbeeEngine::from_board_file(
        &board_text(),
        Some(&fw),
        "/boards/stm32_adc_divider_demo.kicad_pcb",
    )
    .expect("build STM32 engine");

    // Coarse chunks (external emulator); a short run suffices; the very first
    // chunk pushes the ADC injection and the backend records the drop.
    engine.scheduler_mut().chunk_s = 5e-3;
    for _ in 0..20 {
        engine.step(5e-3);
    }

    // 1. The scheduler resolved the drop to the MCU, channel, and board net.
    let drops = engine.scheduler().adc_dropped();
    assert_eq!(
        drops.len(),
        1,
        "exactly the divider channel must be dropped: {drops:?}"
    );
    assert_eq!(drops[0].mcu_ref, "U1");
    assert_eq!(drops[0].channel, 0);
    assert_eq!(drops[0].net, "TEMP_SENSE");
    let msg = drops[0].message();
    assert!(
        msg.contains("TEMP_SENSE") && msg.contains("no ADC injection map"),
        "the canonical warning must name the net and the cause: {msg}"
    );

    // 2. The machine surface: build_cosim_json (what `run --json` serializes)
    //    carries the drop, so a JSON consumer cannot read this run as healthy
    //    ADC coverage.
    let cosim = hauksbee_engine::reports::cosim::build_cosim_json(&engine, false)
        .expect("an MCU ran, so the co-sim summary exists");
    let v = serde_json::to_value(&cosim).expect("CosimJson serializes");
    let dropped = v
        .get("adc_dropped")
        .and_then(|d| d.as_array())
        .expect("adc_dropped must be present in the co-sim JSON");
    assert_eq!(dropped.len(), 1, "{v}");
    assert_eq!(dropped[0]["net"], "TEMP_SENSE");
    assert_eq!(dropped[0]["channel"], 0);
    assert_eq!(dropped[0]["mcu_ref"], "U1");

    // And the divider parts are named (best-effort context for the user).
    let parts = dropped[0]["parts"].as_array().cloned().unwrap_or_default();
    assert!(
        !parts.is_empty(),
        "the divider resistors on TEMP_SENSE should be named: {v}"
    );
}
