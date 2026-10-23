//! A synthetic STM32F072 board with the blinky firmware runs.
//!
//! The F072 descriptor once stubbed the RCC so the firmware's clock bring-up
//! spun forever and the scheduler reported "refused to advance". This board
//! (LQFP-48, optional 8 MHz HSE, blink on PA5, hello on USART1 TX) pins that
//! down for both clock configurations: the board with a drawn crystal and the
//! board without one (HSI), which must not be mistaken for a stalled
//! oscillator.

use hauksbee_engine::HauksbeeEngine;
use hauksbee_frontdoor_api::engine::Engine;

fn board_text(hse: bool) -> String {
    let (osc_pads, osc_parts) = if hse {
        (
            "(pad 5 smd rect (at -3 1) (net 3 \"OSC_IN\"))\n    (pad 6 smd rect (at -3 2) (net 4 \"OSC_OUT\"))",
            r#"  (module Crystal:Crystal_SMD_3225-4Pin (layer F.Cu)
    (at 105 105)
    (fp_text reference Y1 (at 0 0) (layer F.SilkS))
    (fp_text value 8MHz (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "OSC_IN"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
    (pad 3 smd rect (at 2 0) (net 4 "OSC_OUT"))
    (pad 4 smd rect (at 3 0) (net 1 "GND"))
  )"#,
        )
    } else {
        ("", "")
    };
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "OSC_IN")
  (net 4 "OSC_OUT")
  (net 5 "ALIVE")
  (net 6 "UART_TX")
  (net 7 "NRST")

  (module Package_QFP:LQFP-48_7x7mm_P0.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F072CBT6 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at -3 0) (net 2 "+3V3"))
    {osc_pads}
    (pad 7 smd rect (at -3 7) (net 7 "NRST"))
    (pad 8 smd rect (at -3 3) (net 1 "GND"))
    (pad 9 smd rect (at -3 4) (net 2 "+3V3"))
    (pad 15 smd rect (at 3 0) (net 5 "ALIVE"))
    (pad 23 smd rect (at 3 1) (net 1 "GND"))
    (pad 24 smd rect (at 3 2) (net 2 "+3V3"))
    (pad 30 smd rect (at 3 3) (net 6 "UART_TX"))
    (pad 35 smd rect (at -3 5) (net 1 "GND"))
    (pad 36 smd rect (at -3 6) (net 2 "+3V3"))
    (pad 44 smd rect (at 3 4) (net 1 "GND"))
    (pad 47 smd rect (at 3 5) (net 1 "GND"))
    (pad 48 smd rect (at 3 6) (net 2 "+3V3"))
  )
{osc_parts}
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 5 "ALIVE"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 105)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 6 "UART_TX"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 110)
    (fp_text reference R3 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 7 "NRST"))
    (pad 2 thru_hole circle (at 2 0) (net 2 "+3V3"))
  )
)"#
    )
}

struct Outcome {
    alive_high: bool,
    uart: String,
    executed: bool,
    stalled_on_external_clock: bool,
    blockers: Vec<String>,
}

fn run(hse: bool) -> Option<Outcome> {
    let fw = crate::support::testdata("firmware/stm32f072_blinky/blinky.elf")?;
    let mut engine = HauksbeeEngine::from_board_file(
        &board_text(hse),
        Some(&fw),
        "/boards/stm32f072_blinky.kicad_pcb",
    )
    .expect("build the F072 engine");
    engine.scheduler_mut().chunk_s = 50e-6;
    let mut uart = Vec::new();
    let mut alive_high = false;
    for _ in 0..400 {
        let frame = engine.step(50e-6);
        alive_high |= frame.net_voltages.get("ALIVE").copied().unwrap_or(0.0) > 2.0;
        for bytes in frame.uart.values() {
            uart.extend_from_slice(bytes);
        }
    }
    engine.scheduler_mut().refresh_execution_evidence();
    let ev = engine.scheduler().execution_evidence().first().cloned()?;
    Some(Outcome {
        alive_high,
        uart: String::from_utf8_lossy(&uart).into_owned(),
        executed: ev.executed,
        stalled_on_external_clock: ev.stalled_on_external_clock,
        blockers: ev.blockers.iter().map(|b| format!("{b:?}")).collect(),
    })
}

#[test]
fn an_f072_board_with_a_crystal_runs_its_firmware() {
    let Some(out) = run(true) else { return };
    assert!(out.executed, "blockers: {:?}", out.blockers);
    assert!(out.alive_high, "PA5 toggled");
    assert!(out.uart.contains("hello from stm32f072"), "{:?}", out.uart);
    assert!(!out.stalled_on_external_clock);
}

#[test]
fn an_f072_board_without_a_crystal_runs_on_the_internal_oscillator() {
    let Some(out) = run(false) else { return };
    assert!(out.executed, "blockers: {:?}", out.blockers);
    assert!(out.alive_high, "PA5 toggled");
    assert!(out.uart.contains("hello from stm32f072"), "{:?}", out.uart);
    assert!(
        !out.stalled_on_external_clock,
        "the F072 boots from HSI; no crystal is not a stall"
    );
}
