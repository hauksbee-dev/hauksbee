//! The bench view against live emulators: what a person looking at the board
//! would see, taken from a real co-sim run.
//!
//! Three firmware fixtures prove the three kinds of row the view carries:
//!
//!   * `stm32_blinky` toggles PC13: the pin row must say so, at a rate that is
//!     loosely right (the clock-accuracy work on the emulator backends is
//!     separate, so the bound is wide on purpose).
//!   * `rp2040_blink_uart` prints a banner over UART0: the serial row must
//!     carry its first line.
//!   * `stm32_i2c_thermostat` polls an LM75: the bus log must list the sensor
//!     transactions with their acknowledgements.
//!
//! Each skips gracefully when Renode or the firmware is missing, like every
//! `renode_*` test.

#![cfg(feature = "renode")]

use hauksbee_engine::HauksbeeEngine;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_ir::observations::{BenchReport, BusKind, PinState};
use hauksbee_mcu::renode::is_available;
use std::path::Path;

/// Boot `fw` on `board` with 10 ms chunks (each one is an emulator round trip)
/// for `frames` frames, and take the evidence the way a finished run does.
fn run(board: &str, fw: &Path, frames: usize) -> HauksbeeEngine {
    let mut engine =
        HauksbeeEngine::from_board_file(board, Some(fw), "/ci").expect("build the engine");
    engine.scheduler_mut().chunk_s = 10e-3;
    for _ in 0..frames {
        engine.step(10e-3);
    }
    engine.scheduler_mut().refresh_execution_evidence();
    engine
}

/// With `HAUKSBEE_DUMP_BENCH=<dir>` set, keep the run's bench JSON there, so the
/// frontend fixtures are real runs rather than hand-written ones.
fn dump(name: &str, bench: &BenchReport) {
    if let Ok(dir) = std::env::var("HAUKSBEE_DUMP_BENCH") {
        let path = Path::new(&dir).join(format!("{name}.json"));
        let json = serde_json::to_string_pretty(bench).expect("bench serialises");
        std::fs::write(&path, json).expect("write the bench fixture");
    }
}

#[test]
fn bench_stm32_blinky_reports_pc13_toggling() {
    hauksbee_testkit::skip_if!(!is_available(), "Renode not installed");
    let fw = hauksbee_testkit::skip_none!(
        crate::support::testdata("firmware/stm32_blinky/blinky.elf"),
        "blinky.elf not built"
    );
    let board = std::fs::read_to_string(crate::support::board(
        "../../testdata/boards/stm32_bluepill_demo.kicad_pcb",
    ))
    .expect("read the bluepill demo board");

    let engine = run(&board, &fw, 150);
    let bench = engine.scheduler().bench_report();
    dump("bench-stm32-blinky", &bench);

    let mcu = bench.mcus.first().expect("the MCU has a bench row");
    let pc13 = mcu
        .pins
        .iter()
        .find(|p| p.pin == "PC13")
        .unwrap_or_else(|| panic!("PC13 row missing: {:?}", mcu.pins));
    assert_eq!(pc13.state, PinState::Toggling, "{pc13:?}");
    // The firmware blinks at 5 Hz. The emulator's clock accuracy is being fixed
    // separately, so only "a plausible blink rate" is asserted here.
    let hz = pc13.freq_hz.expect("a toggling pin states its rate");
    assert!((0.5..200.0).contains(&hz), "PC13 rate {hz} Hz");
    let duty = pc13.duty.expect("a toggling pin states its duty");
    assert!((0.05..0.95).contains(&duty), "PC13 duty {duty}");
    assert!(pc13.net.is_some(), "the row names the board net it drives");

    // The same view as the CLI prints it, and as the waveform draws it.
    let text = hauksbee_cosim::observe::bench::render_text(&bench);
    assert!(text.contains("PC13") && text.contains("toggling"), "{text}");
    let vcd = engine.scheduler().bench_vcd();
    assert!(
        vcd.contains("$enddefinitions") && vcd.contains("PC13"),
        "{vcd}"
    );
}

/// The RP2040 on a bare QFN-56 board: UART0 TX on GP0 (pad 2), the on-board LED
/// on GP25 (pad 37), every supply on one 3.3 V rail.
const RP2040_BOARD: &str = r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "UART_TX")
  (net 4 "LED")

  (module Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value RP2040 (at 0 2) (layer F.Fab))
    (pad 1  smd rect (at -3 0) (net 2 "+3V3"))
    (pad 2  smd rect (at -3 1) (net 3 "UART_TX"))
    (pad 10 smd rect (at -3 2) (net 2 "+3V3"))
    (pad 19 smd rect (at -3 3) (net 1 "GND"))
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
)
"#;

#[test]
fn bench_rp2040_blink_uart_reports_its_first_uart_line() {
    hauksbee_testkit::skip_if!(!is_available(), "Renode not installed");
    let fw = hauksbee_testkit::skip_none!(
        crate::support::testdata("firmware/rp2040_blink_uart/blink_uart.elf"),
        "blink_uart.elf not built"
    );

    // 1.5 s: the boot ROM, the SDK runtime and the first banner.
    let engine = run(RP2040_BOARD, &fw, 150);
    let bench = engine.scheduler().bench_report();
    dump("bench-rp2040-blink-uart", &bench);

    let mcu = bench.mcus.first().expect("the MCU has a bench row");
    let uart = mcu.uarts.first().expect("the serial port has a row");
    assert_eq!(
        uart.first_line.as_deref(),
        Some("hauksbee rp2040: main reached"),
        "{uart:?}"
    );
    assert!(uart.bytes > 0 && uart.printable_ratio > 0.9, "{uart:?}");
    // The RP2040 card's pin map names pads `gpioN` with no peripheral function,
    // so the serial row is not yet linked to a board net on this part (the STM32
    // and AVR maps carry `_tx` roles and are). A linked net, when present, must
    // be the one the board wires to pad 2.
    if let Some(net) = &uart.net {
        assert_eq!(net, "UART_TX", "{uart:?}");
    }

    // The banner is also a timeline marker, and a bus-log entry for the serial
    // line, so it can be found in time.
    assert!(
        bench
            .events
            .iter()
            .any(|e| e.detail.contains("hauksbee rp2040: main reached")
                || e.detail.to_lowercase().contains("uart")),
        "{:?}",
        bench.events
    );
    assert!(
        bench
            .bus_log
            .iter()
            .any(|t| t.bus == BusKind::Uart && t.summary.contains("main reached")),
        "{:?}",
        bench.bus_log
    );
}

#[test]
fn bench_stm32_i2c_thermostat_bus_log_lists_the_sensor_transactions() {
    hauksbee_testkit::skip_if!(!is_available(), "Renode not installed");
    let fw = hauksbee_testkit::skip_none!(
        crate::support::testdata("firmware/stm32_i2c_thermostat/thermostat.elf"),
        "thermostat.elf not built"
    );
    let board = std::fs::read_to_string(crate::support::board(
        "../../testdata/boards/stm32_i2c_thermostat.kicad_pcb",
    ))
    .expect("read the thermostat board");

    let (mut engine, _bus) = crate::support::lm75_engine(&board, &fw, 25.0);
    for _ in 0..100 {
        engine.step(5e-3);
    }
    engine.scheduler_mut().refresh_execution_evidence();
    let bench = engine.scheduler().bench_report();
    dump("bench-i2c-thermostat", &bench);

    let i2c: Vec<_> = bench
        .bus_log
        .iter()
        .filter(|t| t.bus == BusKind::I2c)
        .collect();
    assert!(
        i2c.len() >= 2,
        "the firmware polls the LM75 every few milliseconds: {:?}",
        bench.bus_log
    );
    assert!(
        i2c.iter().all(|t| t.addr == Some(0x48)),
        "every transaction is to the LM75 at 0x48: {i2c:?}"
    );
    assert!(
        i2c.iter().all(|t| t.acked),
        "the LM75 is present, so each address byte is acknowledged: {i2c:?}"
    );
    assert!(
        i2c.iter()
            .any(|t| t.summary.contains("ACK") && t.summary.contains("0x48")),
        "{i2c:?}"
    );
    // Times are in order, so the log reads as a sequence.
    assert!(
        bench.bus_log.windows(2).all(|w| w[0].t_s <= w[1].t_s),
        "{:?}",
        bench.bus_log
    );

    let log = hauksbee_cosim::observe::bench::render_bus_log(&bench);
    assert!(log.contains("0x48") && log.contains("ACK"), "{log}");
}
