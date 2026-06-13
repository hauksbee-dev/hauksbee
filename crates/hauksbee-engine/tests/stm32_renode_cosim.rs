//! Full board co-simulation of the STM32 blue pill demo through Renode.
//!
//! This is the headline non-AVR proof: hauksbee extracts the STM32 demo
//! `.kicad_pcb`, binds U1 to the `renode:stm32f103` backend, spawns a headless
//! Renode STM32F103 running the bundled blinky+UART firmware, and co-simulates
//! it against the solved analog circuit. It asserts:
//!
//!   1. UART: the firmware's "hello from stm32" banner arrives through the
//!      Renode UART socket bridge.
//!   2. Analog: when the firmware drives PA5 HIGH at boot, real current flows
//!      through the 330 Ohm resistor R1 into the LED, computed by the solver
//!      from the node voltages (V across R1 / 330).
//!   3. GPIO: the PC13 LED net toggles through the solved circuit at the
//!      firmware's blink rate.
//!
//! It skips gracefully when Renode is not installed, but runs for real wherever
//! Renode is present.

#![cfg(feature = "renode")]

use hauksbee_engine::HauksbeeEngine;
use hauksbee_mcu::renode::is_available;
use hauksbee_server::engine::Engine;
use std::path::PathBuf;

fn board_text() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/boards/stm32_bluepill_demo.kicad_pcb");
    std::fs::read_to_string(p).expect("read stm32 board")
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
fn stm32_full_cosim_through_solved_circuit() {
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
        "/boards/stm32_bluepill_demo.kicad_pcb",
    )
    .expect("build STM32 engine");

    // Each Renode RunFor and GPIO read is a Monitor TCP round-trip, so use a
    // coarse co-sim chunk (5 ms): plenty to oversample a ~5 Hz blink while
    // keeping the round-trip count modest. The LED RC settling is sub-us, so a
    // 5 ms analog chunk resolves the DC operating point fine.
    engine.scheduler_mut().chunk_s = 5e-3;

    // Step 5 ms frames for ~1.0 s of virtual time (~200 chunks).
    let frame_dt = 5e-3_f64;
    let total = 1.0_f64;
    let n = (total / frame_dt).round() as usize;

    let mut uart: Vec<u8> = Vec::new();
    let mut max_r1_current_ma = 0.0_f64;
    let mut led_high_samples: Vec<f64> = Vec::new();
    let mut pc13_transitions = 0u32;
    let mut prev_pc13: Option<bool> = None;

    for _ in 0..n {
        let frame = engine.step(frame_dt);
        for b in frame.uart.values() {
            uart.extend_from_slice(b);
        }

        // Analog proof: real current through R1 = (V_PA5_OUT - V_LED_A)/330.
        let pa5 = frame.net_voltages.get("PA5_OUT").copied().unwrap_or(0.0);
        let led_a = frame.net_voltages.get("LED_A").copied().unwrap_or(0.0);
        let i_r1_ma = (pa5 - led_a).abs() / 330.0 * 1000.0;
        max_r1_current_ma = max_r1_current_ma.max(i_r1_ma);
        if led_a > 1.0 {
            led_high_samples.push(led_a);
        }

        // GPIO proof: PC13 LED net toggles through the solved circuit.
        let pc13 = frame.net_voltages.get("PC13_LED").copied().unwrap_or(0.0);
        let logic = if pc13 > 2.0 {
            Some(true)
        } else if pc13 < 1.0 {
            Some(false)
        } else {
            prev_pc13
        };
        if let (Some(p), Some(c)) = (prev_pc13, logic) {
            if p != c {
                pc13_transitions += 1;
            }
        }
        prev_pc13 = logic;
    }

    let text = String::from_utf8_lossy(&uart).to_string();
    eprintln!("STM32 co-sim results:");
    eprintln!("  UART: {text:?}");
    eprintln!("  max R1 current: {max_r1_current_ma:.3} mA");
    eprintln!("  LED_A high samples: {}", led_high_samples.len());
    eprintln!("  PC13 transitions in {total}s: {pc13_transitions}");

    // 1. UART bridge: the boot banner came through.
    assert!(
        text.contains("hello from stm32"),
        "expected boot banner over UART, got: {text:?}"
    );

    // 2. Analog: PA5 driven HIGH pushes real current through the 330 Ohm R1
    //    into the LED. A 3.3 V rail across 330 Ohm + a ~1.8 V red LED clamp
    //    gives roughly (3.3 - 1.8)/330 ~= 4.5 mA. Assert a clearly nonzero,
    //    physically sane current.
    assert!(
        max_r1_current_ma > 1.0,
        "expected real current through R1 (PA5 HIGH); got {max_r1_current_ma:.3} mA"
    );
    assert!(
        max_r1_current_ma < 20.0,
        "R1 current implausibly high: {max_r1_current_ma:.3} mA"
    );
    assert!(
        !led_high_samples.is_empty(),
        "LED net never energised through the solved circuit"
    );

    // 3. GPIO: PC13 toggled through the solved circuit several times. The
    //    firmware toggles PC13 at ~5 Hz, so ~10 logic transitions/sec; assert a
    //    generous floor that still proves it is blinking, not stuck.
    assert!(
        pc13_transitions >= 3,
        "PC13 LED net should toggle through the circuit; got {pc13_transitions}"
    );
}
