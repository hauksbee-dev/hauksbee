//! Board-to-firmware proof for STM32F103 external-clock readiness.
//!
//! The same clock-polling ELF runs on two otherwise identical boards. Only the
//! board with a populated crystal across OSC_IN/OSC_OUT may reach its HSERDY and
//! PLLRDY marker pins, and those markers must respect the descriptor delays.

#![cfg(feature = "renode")]

use hauksbee_engine::HauksbeeEngine;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_mcu::renode::is_available;
use std::path::PathBuf;

fn firmware() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/stm32_clock_ready/clock_ready.elf");
    if p.exists() {
        Some(p.canonicalize().unwrap_or(p))
    } else {
        None
    }
}

fn board_text(with_crystal: bool) -> String {
    let crystal = if with_crystal {
        r#"
  (module Crystal:Crystal_SMD_3225-4Pin (layer F.Cu)
    (at 105 105)
    (fp_text reference Y1 (at 0 0) (layer F.SilkS))
    (fp_text value 8MHz (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "OSC_IN"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
    (pad 3 smd rect (at 2 0) (net 4 "OSC_OUT"))
    (pad 4 smd rect (at 3 0) (net 1 "GND"))
  )
"#
    } else {
        ""
    };

    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "OSC_IN")
  (net 4 "OSC_OUT")
  (net 5 "HSE_READY")
  (net 6 "PLL_READY")

  (module Package_QFP:LQFP-48_7x7mm_P0.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F103C8T6 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at -3 0) (net 6 "PLL_READY"))
    (pad 5 smd rect (at -3 1) (net 3 "OSC_IN"))
    (pad 6 smd rect (at -3 2) (net 4 "OSC_OUT"))
    (pad 8 smd rect (at -3 3) (net 1 "GND"))
    (pad 9 smd rect (at -3 4) (net 2 "+3V3"))
    (pad 15 smd rect (at 3 0) (net 5 "HSE_READY"))
    (pad 23 smd rect (at 3 1) (net 1 "GND"))
    (pad 24 smd rect (at 3 2) (net 2 "+3V3"))
    (pad 35 smd rect (at -3 5) (net 1 "GND"))
    (pad 36 smd rect (at -3 6) (net 2 "+3V3"))
    (pad 44 smd rect (at 3 3) (net 1 "GND"))
    (pad 48 smd rect (at 3 4) (net 2 "+3V3"))
  )
{crystal}
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 5 "HSE_READY"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 105)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 6 "PLL_READY"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
)"#
    )
}

fn marker_times(with_crystal: bool) -> Option<(Option<u64>, Option<u64>)> {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return None;
    }
    let Some(fw) = firmware() else {
        eprintln!("SKIP: clock_ready.elf not built");
        return None;
    };
    let mut engine = HauksbeeEngine::from_board_file(
        &board_text(with_crystal),
        Some(&fw),
        "/boards/stm32_clock_readiness.kicad_pcb",
    )
    .expect("build STM32 clock-readiness engine");

    const CHUNK_US: u64 = 50;
    engine.scheduler_mut().chunk_s = CHUNK_US as f64 / 1_000_000.0;
    let mut hse_us = None;
    let mut pll_us = None;
    for now_us in (CHUNK_US..=3_000).step_by(CHUNK_US as usize) {
        let frame = engine.step(CHUNK_US as f64 / 1_000_000.0);
        if frame.net_voltages.get("HSE_READY").copied().unwrap_or(0.0) > 2.0 {
            hse_us.get_or_insert(now_us);
        }
        if frame.net_voltages.get("PLL_READY").copied().unwrap_or(0.0) > 2.0 {
            pll_us.get_or_insert(now_us);
        }
    }
    Some((hse_us, pll_us))
}

#[test]
fn assembled_crystal_reaches_ready_markers_after_startup_delays() {
    let Some((hse_us, pll_us)) = marker_times(true) else {
        return;
    };
    let hse_us = hse_us.expect("HSERDY marker must rise with an assembled crystal");
    let pll_us = pll_us.expect("PLLRDY marker must rise after HSE");
    assert!((2_000..=2_150).contains(&hse_us), "HSERDY at {hse_us} us");
    assert!((2_200..=2_350).contains(&pll_us), "PLLRDY at {pll_us} us");
    assert!(pll_us > hse_us);
}

#[test]
fn missing_crystal_keeps_firmware_blocked_in_hse_wait() {
    let Some((hse_us, pll_us)) = marker_times(false) else {
        return;
    };
    assert_eq!(hse_us, None, "missing crystal must not produce HSERDY");
    assert_eq!(pll_us, None, "PLL cannot lock without its HSE source");
}
