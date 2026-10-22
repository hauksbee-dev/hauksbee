//! The co-sim boots the way the chip would: from the board's strap levels.
//!
//! One STM32F103 board, one firmware, three wirings of BOOT0. With BOOT0 low
//! the firmware runs; with it high the part enters its system-memory
//! bootloader, the user firmware does not run, the pad the firmware would have
//! toggled stays quiet, and the ROM answers the USART handshake (AN3155).
//! These run without Renode: the bootloader path never starts the emulator.

use hauksbee_cosim::execution::ExecutionEvidence;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_ir::boot::BootOutcome;
use std::path::PathBuf;

fn firmware() -> Option<PathBuf> {
    crate::support::testdata("firmware/stm32_clock_ready/clock_ready.elf")
}

#[derive(Clone, Copy)]
enum Boot0 {
    Gnd,
    Rail,
    Floating,
}

fn board_text(boot0: Boot0, boot1_rail: bool, nrst_grounded: bool) -> String {
    let boot0_net = match boot0 {
        Boot0::Gnd => "1 \"GND\"",
        Boot0::Rail => "2 \"+3V3\"",
        Boot0::Floating => "7 \"BOOT0_NET\"",
    };
    let boot1_net = if boot1_rail { "2 \"+3V3\"" } else { "1 \"GND\"" };
    let nrst_pull = if nrst_grounded { "GND" } else { "+3V3" };
    let nrst_pull_net = if nrst_grounded { 1 } else { 2 };
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "OSC_IN")
  (net 4 "OSC_OUT")
  (net 5 "HSE_READY")
  (net 6 "PLL_READY")
  (net 7 "BOOT0_NET")
  (net 8 "NRST")

  (module Package_QFP:LQFP-48_7x7mm_P0.5mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value STM32F103C8T6 (at 0 2) (layer F.Fab))
    (pad 2 smd rect (at -3 0) (net 6 "PLL_READY"))
    (pad 5 smd rect (at -3 1) (net 3 "OSC_IN"))
    (pad 6 smd rect (at -3 2) (net 4 "OSC_OUT"))
    (pad 7 smd rect (at -3 7) (net 8 "NRST"))
    (pad 8 smd rect (at -3 3) (net 1 "GND"))
    (pad 9 smd rect (at -3 4) (net 2 "+3V3"))
    (pad 15 smd rect (at 3 0) (net 5 "HSE_READY"))
    (pad 20 smd rect (at 3 7) (net {boot1_net}))
    (pad 23 smd rect (at 3 1) (net 1 "GND"))
    (pad 24 smd rect (at 3 2) (net 2 "+3V3"))
    (pad 35 smd rect (at -3 5) (net 1 "GND"))
    (pad 36 smd rect (at -3 6) (net 2 "+3V3"))
    (pad 44 smd rect (at 3 3) (net {boot0_net}))
    (pad 48 smd rect (at 3 4) (net 2 "+3V3"))
  )
  (module Crystal:Crystal_SMD_3225-4Pin (layer F.Cu)
    (at 105 105)
    (fp_text reference Y1 (at 0 0) (layer F.SilkS))
    (fp_text value 8MHz (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 3 "OSC_IN"))
    (pad 2 smd rect (at 1 0) (net 1 "GND"))
    (pad 3 smd rect (at 2 0) (net 4 "OSC_OUT"))
    (pad 4 smd rect (at 3 0) (net 1 "GND"))
  )
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
  (module Resistor:R (layer F.Cu)
    (at 110 110)
    (fp_text reference R3 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 8 "NRST"))
    (pad 2 thru_hole circle (at 2 0) (net {nrst_pull_net} "{nrst_pull}"))
  )
)"#
    )
}

struct Run {
    evidence: ExecutionEvidence,
    pll_high: bool,
    uart: Vec<u8>,
}

fn run(boot0: Boot0, boot1_rail: bool, nrst_grounded: bool, send: &[u8]) -> Option<Run> {
    let fw = hauksbee_testkit::skip_none!(firmware() => None, "clock_ready.elf not built");
    let mut engine = HauksbeeEngine::from_board_file(
        &board_text(boot0, boot1_rail, nrst_grounded),
        Some(&fw),
        "/boards/boot_straps.kicad_pcb",
    )
    .expect("build the STM32 boot-strap engine");
    engine.scheduler_mut().chunk_s = 50e-6;
    let mut pll_high = false;
    let mut uart = Vec::new();
    engine.scheduler_mut().serial("U1", send);
    for _ in 0..80 {
        let frame = engine.step(50e-6);
        pll_high |= frame.net_voltages.get("PLL_READY").copied().unwrap_or(0.0) > 2.0;
        for bytes in frame.uart.values() {
            uart.extend_from_slice(bytes);
        }
    }
    engine.scheduler_mut().refresh_execution_evidence();
    let evidence = engine.scheduler().execution_evidence().first()?.clone();
    Some(Run {
        evidence,
        pll_high,
        uart,
    })
}

#[test]
fn boot0_high_enters_the_system_bootloader_and_the_firmware_does_not_run() {
    let Some(r) = run(Boot0::Rail, false, false, &[0x7F, 0x02, 0xFD]) else {
        return;
    };
    let boot = r.evidence.boot.as_ref().expect("a boot decision is reported");
    assert_eq!(boot.outcome, BootOutcome::RomBootloader);
    assert!(!boot.user_firmware_runs);
    assert!(
        boot.summary.contains("BOOT0 = 3.30 V") && boot.summary.contains("user firmware not run"),
        "{}",
        boot.summary
    );
    assert!(!r.evidence.executed, "{:?}", r.evidence);
    assert!(!r.evidence.firmware_pass_eligible);
    assert!(
        r.evidence.blockers.iter().any(|b| b.contains("boot straps")),
        "{:?}",
        r.evidence.blockers
    );
    assert!(!r.pll_high, "the firmware's marker pin must stay quiet");
    // AN3155: 0x7F -> ACK (0x79); Get ID -> ACK, 1, PID MSB, PID LSB, ACK.
    assert_eq!(r.uart, vec![0x79, 0x79, 0x01, 0x04, 0x10, 0x79]);
}

#[test]
fn boot0_high_with_boot1_high_boots_sram_which_runs_no_user_firmware() {
    let Some(r) = run(Boot0::Rail, true, false, &[]) else {
        return;
    };
    let boot = r.evidence.boot.as_ref().unwrap();
    assert_eq!(boot.outcome, BootOutcome::Sram);
    assert!(!r.evidence.executed);
}

#[test]
fn a_floating_boot0_is_reported_undefined_not_guessed() {
    let Some(r) = run(Boot0::Floating, false, false, &[]) else {
        return;
    };
    let boot = r.evidence.boot.as_ref().unwrap();
    assert_eq!(boot.outcome, BootOutcome::Undefined);
    assert!(
        boot.caveats.iter().any(|c| c.contains("BOOT0")),
        "{:?}",
        boot.caveats
    );
}

#[cfg(feature = "renode")]
#[test]
fn a_grounded_boot0_boots_the_user_firmware_and_says_so() {
    // Needs Renode: the user firmware really runs.
    if !hauksbee_mcu::renode::is_available() {
        eprintln!("skipping: Renode not installed");
        return;
    }
    let Some(r) = run(Boot0::Gnd, false, false, &[]) else {
        return;
    };
    let boot = r.evidence.boot.as_ref().unwrap();
    assert_eq!(boot.outcome, BootOutcome::UserFlash);
    assert!(boot.user_firmware_runs);
}

#[test]
fn reset_held_low_keeps_the_core_from_running() {
    let Some(r) = run(Boot0::Gnd, false, true, &[]) else {
        return;
    };
    let boot = r.evidence.boot.as_ref().unwrap();
    assert_eq!(boot.outcome, BootOutcome::HeldInReset);
    assert!(!r.evidence.executed);
    assert!(!r.pll_high);
}
