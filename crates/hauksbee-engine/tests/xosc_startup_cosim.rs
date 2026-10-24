//! A board whose crystal starts slower than the firmware waits for it.
//!
//! The RP2040 on its minimal QFN-56 board with the hardware design guide's
//! 12 MHz crystal circuit, the crystal carrying a card that states a 10 ms
//! start-up (`startup_s`). The firmware programs `XOSC_STARTUP.DELAY`, waits
//! for `XOSC_STATUS.STABLE`, switches `clk_ref` onto the crystal and drives the
//! LED on GP25 high (`testdata/firmware/rp2040_xosc_startup`):
//!
//!   * a 1 ms delay switches onto a crystal that has not started: the core is
//!     halted there, the LED never lights, and the run's evidence names the
//!     crystal, its stated start-up and the firmware's wait;
//!   * the 64 ms control lights the LED and raises nothing.

#![cfg(feature = "renode")]

use crate::support::rp2040_crystal;
use hauksbee_engine::binder::bind_board;
use hauksbee_engine::execution::CLOCK_NOT_READY_PREFIX;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_extract::ExtractedBoard;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_mcu::renode::is_available;
use hauksbee_models::ModelLibrary;
use std::path::Path;

/// A 12 MHz crystal whose start-up is stated as 10 ms. Synthetic: the value is
/// chosen to sit between the two fixtures' waits, not read from a datasheet.
const CARD: &str = r#"
[[models]]
id = "test_xtal_12mhz_10ms_startup"
kind = "passive"
passive_class = "crystal"
description = "synthetic 12 MHz crystal with a 10 ms start-up"
[models.match]
mpn_re = '^HKB-TEST-XTAL-10MS$'
[models.params]
cl_f = 10.0e-12
c0_f = 3.0e-12
startup_s = 10.0e-3
[models.pins]
"1" = "a"
"2" = "gnd"
"3" = "b"
"4" = "gnd"
"#;

fn board_text() -> String {
    let (xtal_pads, crystal_parts) = rp2040_crystal(1, 5, 6, 7);
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "UART_TX")
  (net 4 "LED")
  (net 5 "XIN")
  (net 6 "XOUT")
  (net 7 "XTAL_B")

  (module Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value RP2040 (at 0 2) (layer F.Fab))
    (pad 1  smd rect (at -3 0) (net 2 "+3V3"))
    (pad 2  smd rect (at -3 1) (net 3 "UART_TX"))
    (pad 10 smd rect (at -3 2) (net 2 "+3V3"))
    (pad 19 smd rect (at -3 3) (net 1 "GND"))
    {xtal_pads}
    (pad 22 smd rect (at -3 4) (net 2 "+3V3"))
    (pad 23 smd rect (at -3 5) (net 2 "+3V3"))
    (pad 26 smd rect (at -3 6) (net 2 "+3V3"))
    (pad 33 smd rect (at 3 0) (net 2 "+3V3"))
    (pad 37 smd rect (at 3 1) (net 4 "LED"))
    (pad 42 smd rect (at 3 2) (net 2 "+3V3"))
    (pad 43 smd rect (at 3 3) (net 2 "+3V3"))
    (pad 44 smd rect (at 3 4) (net 2 "+3V3"))
    (pad 45 smd rect (at 3 5) (net 2 "+3V3"))
    (pad 48 smd rect (at 3 6) (net 2 "+3V3"))
    (pad 49 smd rect (at 3 7) (net 2 "+3V3"))
    (pad 50 smd rect (at 3 8) (net 2 "+3V3"))
    (pad 56 smd rect (at 0 3) (net 2 "+3V3"))
    (pad 57 smd rect (at 0 0) (net 1 "GND"))
  )

  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 330 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 4 "LED"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )
{crystal_parts}
)
"#
    )
}

/// Run `fw` for 200 ms on the board with the 10 ms crystal; return the engine.
fn run(fw: &Path) -> HauksbeeEngine {
    let mut board = ExtractedBoard::from_auto(&board_text()).expect("extract the board");
    let y1 = board
        .components
        .iter_mut()
        .find(|c| c.reference == "Y1")
        .expect("the crystal");
    y1.properties
        .push(("MPN".into(), "HKB-TEST-XTAL-10MS".into()));
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("xtal.toml"), CARD).expect("write the card");
    let mut lib = ModelLibrary::builtin();
    let errors = lib.load_models_dir(dir.path());
    assert!(errors.is_empty(), "the card loads: {errors:?}");
    let bound = bind_board(&board, &lib);
    let mut engine = HauksbeeEngine::from_board_and_bound(&board, bound, Some(fw), "/ci")
        .expect("build the engine");
    engine.scheduler_mut().chunk_s = 10e-3;
    for _ in 0..20 {
        engine.step(10e-3);
    }
    engine.scheduler_mut().refresh_execution_evidence();
    engine
}

fn fixture(elf: &str) -> Option<std::path::PathBuf> {
    crate::support::testdata(&format!("firmware/rp2040_xosc_startup/{elf}"))
}

#[test]
fn a_1ms_wait_on_a_10ms_crystal_halts_the_core_and_names_the_cause() {
    hauksbee_testkit::skip_if!(!is_available(), "Renode not installed");
    let fw = hauksbee_testkit::skip_none!(fixture("xosc_wait_1ms.elf"), "fixture not built");
    let engine = run(&fw);
    let sched = engine.scheduler();
    let ev = sched
        .execution_evidence()
        .iter()
        .find(|e| e.mcu_ref == "U1")
        .expect("U1 evidence");
    let blocker = ev
        .blockers
        .iter()
        .find(|b| b.starts_with(CLOCK_NOT_READY_PREFIX))
        .unwrap_or_else(|| panic!("no clock-not-ready blocker: {:?}", ev.blockers));
    for needle in [
        "crystal Y1",
        "10 ms",
        "card test_xtal_12mhz_10ms_startup",
        "xosc",
        "1.0",
    ] {
        assert!(blocker.contains(needle), "{needle}: {blocker}");
    }
    assert!(!ev.firmware_pass_eligible);
    // The symptom: the LED the firmware lights after the switch stays dark.
    let led = sched.net_voltage("LED").expect("LED net");
    assert!(led < 0.5, "the halted core never lit the LED: {led} V");
    let bench = sched.bench_report();
    let mcu = bench.mcus.first().expect("bench row");
    assert!(
        mcu.headline.starts_with(CLOCK_NOT_READY_PREFIX),
        "{}",
        mcu.headline
    );
}

#[test]
fn a_64ms_wait_on_the_same_crystal_runs_and_raises_nothing() {
    hauksbee_testkit::skip_if!(!is_available(), "Renode not installed");
    let fw = hauksbee_testkit::skip_none!(fixture("xosc_wait_64ms.elf"), "fixture not built");
    let engine = run(&fw);
    let sched = engine.scheduler();
    let ev = sched
        .execution_evidence()
        .iter()
        .find(|e| e.mcu_ref == "U1")
        .expect("U1 evidence");
    assert!(
        !ev.blockers
            .iter()
            .any(|b| b.starts_with(CLOCK_NOT_READY_PREFIX)),
        "{:?}",
        ev.blockers
    );
    let led = sched.net_voltage("LED").expect("LED net");
    // The pad's drive resistance into the 330 Ohm load puts the lit LED net
    // near 2.2 V, well clear of the dark case's 0 V.
    assert!(led > 1.5, "the firmware lit the LED after 64 ms: {led} V");
}
