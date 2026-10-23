//! RP2040 boot strap: `QSPI_SS` shared with a QSPI flash chip's chip select.
//!
//! The RP2040 reads `QSPI_SS` at reset release: high boots the flash, low enters
//! the USB bootloader (RP2040 datasheet 2.8.1). On a real board `QSPI_SS` is
//! also the flash's chip-select input, so the net always has a chip attached.
//! A chip input is a receiver: it must not count as something pulling the net
//! low, and the RP2040's own pad pull-up (`PADS_QSPI` PUE, datasheet Tables
//! 344-348, as the rp2040 card cites) holds the net high. Only a real low (a tie to ground, a held
//! BOOTSEL button) selects USB boot.
//!
//! The resistors the Pico reference design hangs on `QSPI_SS` (1 kohm to the
//! BOOTSEL switch) are the interesting case: with the switch open the 1 kohm
//! leads to a node nothing else touches, so it must not read as a path to
//! ground, and a 1 kohm into a dead end is the same.

use hauksbee_cosim::execution::ExecutionEvidence;
use hauksbee_engine::HauksbeeEngine;
use hauksbee_frontdoor_api::engine::Engine;
use hauksbee_ir::boot::BootOutcome;

#[derive(Clone, Copy, Debug)]
enum Ss {
    /// Flash CS only: nothing else on the net.
    FlashOnly,
    /// Flash CS plus a 10k pull-up to 3V3.
    PullUp,
    /// Flash CS plus a 1k to GND (BOOTSEL held) and no pull-up.
    HeldLow1k,
    /// Flash CS tied to GND.
    Gnd,
    /// The Pico reference: 1k to a BOOTSEL switch to GND, switch open.
    ButtonOpen,
    /// The same, with the switch closed (a 0 ohm link to ground).
    ButtonClosed,
    /// 1k to a node nothing else touches.
    DeadEnd,
}

fn resistor(value: &str, other_net: &str, other_id: u32) -> String {
    format!(
        r#"  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 115 100)
    (fp_text reference R4 (at 0 0) (layer F.SilkS))
    (fp_text value {value} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 9 "QSPI_SS"))
    (pad 2 smd rect (at 2 0) (net {other_id} "{other_net}"))
  )"#
    )
}

/// What hangs on `QSPI_SS` besides the RP2040 pad and the flash chip.
fn extra_parts(ss: Ss) -> String {
    let button = |value: &str| {
        format!(
            r#"  (module Button_Switch_SMD:SW_SPST_TL3342 (layer F.Cu)
    (at 118 100)
    (fp_text reference SW1 (at 0 0) (layer F.SilkS))
    (fp_text value {value} (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 15 "BTN"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )"#
        )
    };
    match ss {
        Ss::FlashOnly => String::new(),
        Ss::PullUp => resistor("10k", "+3V3", 2),
        Ss::HeldLow1k => resistor("1k", "GND", 1),
        Ss::Gnd => resistor("0", "GND", 1),
        Ss::ButtonOpen => format!("{}\n{}", resistor("1k", "BTN", 15), button("BOOTSEL")),
        // A closed switch is modelled as the button part's own contact being
        // shorted by a 0 ohm link in parallel.
        Ss::ButtonClosed => format!(
            "{}\n{}\n{}",
            resistor("1k", "BTN", 15),
            button("BOOTSEL"),
            r#"  (module Resistor_SMD:R_0603_1608Metric (layer F.Cu)
    (at 119 100)
    (fp_text reference R9 (at 0 0) (layer F.SilkS))
    (fp_text value 0 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 15 "BTN"))
    (pad 2 smd rect (at 2 0) (net 1 "GND"))
  )"#
        ),
        Ss::DeadEnd => resistor("1k", "STUB", 16),
    }
}

fn board_text(ss: Ss) -> String {
    let extra = extra_parts(ss);
    let (xtal_pads, xtal_parts) = crate::support::rp2040_crystal(1, 6, 7, 8);
    format!(
        r#"(kicad_pcb (version 20171130) (host pcbnew 5.1.0)
  (net 0 "")
  (net 1 "GND")
  (net 2 "+3V3")
  (net 3 "LED")
  (net 4 "UART_TX")
  (net 5 "RUN")
  (net 6 "XIN")
  (net 7 "XOUT")
  (net 8 "XTAL_B")
  (net 9 "QSPI_SS")
  (net 10 "QSPI_SD0")
  (net 11 "QSPI_SD1")
  (net 12 "QSPI_SD2")
  (net 13 "QSPI_SD3")
  (net 14 "QSPI_SCLK")
  (net 15 "BTN")
  (net 16 "STUB")

  (module Package_DFN_QFN:QFN-56-1EP_7x7mm_P0.4mm_EP3.2x3.2mm (layer F.Cu)
    (at 100 100)
    (fp_text reference U1 (at 0 0) (layer F.SilkS))
    (fp_text value RP2040 (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at -3 0) (net 2 "+3V3"))
    (pad 2 smd rect (at -3 1) (net 4 "UART_TX"))
    (pad 10 smd rect (at -3 2) (net 2 "+3V3"))
    {xtal_pads}
    (pad 22 smd rect (at -3 3) (net 2 "+3V3"))
    (pad 26 smd rect (at -3 4) (net 5 "RUN"))
    (pad 33 smd rect (at -3 5) (net 2 "+3V3"))
    (pad 37 smd rect (at -3 6) (net 3 "LED"))
    (pad 42 smd rect (at -3 7) (net 2 "+3V3"))
    (pad 49 smd rect (at -3 8) (net 2 "+3V3"))
    (pad 51 smd rect (at -3 9) (net 13 "QSPI_SD3"))
    (pad 52 smd rect (at -3 10) (net 14 "QSPI_SCLK"))
    (pad 53 smd rect (at -3 11) (net 10 "QSPI_SD0"))
    (pad 54 smd rect (at -3 12) (net 12 "QSPI_SD2"))
    (pad 55 smd rect (at -3 13) (net 11 "QSPI_SD1"))
    (pad 56 smd rect (at -3 14) (net 9 "QSPI_SS"))
    (pad 57 smd rect (at 3 0) (net 1 "GND"))
  )
  (module Package_SO:SOIC-8_5.23x5.23mm_P1.27mm (layer F.Cu)
    (at 120 100)
    (fp_text reference U2 (at 0 0) (layer F.SilkS))
    (fp_text value W25Q128JVSIQ (at 0 2) (layer F.Fab))
    (pad 1 smd rect (at 0 0) (net 9 "QSPI_SS"))
    (pad 2 smd rect (at 0 1) (net 11 "QSPI_SD1"))
    (pad 3 smd rect (at 0 2) (net 12 "QSPI_SD2"))
    (pad 4 smd rect (at 0 3) (net 1 "GND"))
    (pad 5 smd rect (at 5 3) (net 10 "QSPI_SD0"))
    (pad 6 smd rect (at 5 2) (net 14 "QSPI_SCLK"))
    (pad 7 smd rect (at 5 1) (net 13 "QSPI_SD3"))
    (pad 8 smd rect (at 5 0) (net 2 "+3V3"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 100)
    (fp_text reference R1 (at 0 0) (layer F.SilkS))
    (fp_text value 1k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 3 "LED"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 105)
    (fp_text reference R2 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 4 "UART_TX"))
    (pad 2 thru_hole circle (at 2 0) (net 1 "GND"))
  )
  (module Resistor:R (layer F.Cu)
    (at 110 110)
    (fp_text reference R3 (at 0 0) (layer F.SilkS))
    (fp_text value 10k (at 0 2) (layer F.Fab))
    (pad 1 thru_hole circle (at 0 0) (net 5 "RUN"))
    (pad 2 thru_hole circle (at 2 0) (net 2 "+3V3"))
  )
{xtal_parts}
{extra}
)"#
    )
}

fn run(ss: Ss) -> Option<ExecutionEvidence> {
    let fw = crate::support::testdata("firmware/rp2040_blink_uart/blink_uart.elf")?;
    let mut engine = HauksbeeEngine::from_board_file(
        &board_text(ss),
        Some(&fw),
        "/boards/rp2040_qspi_ss.kicad_pcb",
    )
    .expect("build the RP2040 engine");
    engine.scheduler_mut().chunk_s = 1e-3;
    // Run until the firmware has reached main (or the strap held it in USB
    // boot): how much start-up fits in a step varies with machine load.
    for i in 0..250 {
        let _ = engine.step(1e-3);
        if i % 25 == 24 {
            engine.scheduler_mut().refresh_execution_evidence();
            if engine
                .scheduler()
                .execution_evidence()
                .first()
                .is_some_and(|e| e.executed)
            {
                break;
            }
        }
    }
    engine.scheduler_mut().refresh_execution_evidence();
    engine.scheduler().execution_evidence().first().cloned()
}

fn outcome(ss: Ss) -> Option<(BootOutcome, bool, String)> {
    let ev = run(ss)?;
    let boot = ev
        .boot
        .as_ref()
        .expect("an RP2040 board has a boot decision");
    Some((boot.outcome, ev.executed, boot.summary.clone()))
}

/// A flash chip on `QSPI_SS`, with or without a pull-up, and with the BOOTSEL
/// reference circuit left open, boots the flash and runs the firmware.
#[test]
fn a_flash_chip_on_qspi_ss_boots_the_flash() {
    for ss in [Ss::FlashOnly, Ss::PullUp, Ss::ButtonOpen, Ss::DeadEnd] {
        let Some((out, executed, summary)) = outcome(ss) else {
            return;
        };
        assert_eq!(out, BootOutcome::UserFlash, "{ss:?}: {summary}");
        assert!(executed, "{ss:?}: the firmware runs: {summary}");
    }
}

/// A real low (a tie to ground, a held BOOTSEL) is the USB bootloader, and the
/// summary says it is the strap, not a driver.
#[test]
fn a_real_low_on_qspi_ss_selects_usb_boot() {
    for ss in [Ss::HeldLow1k, Ss::Gnd, Ss::ButtonClosed] {
        let Some((out, executed, summary)) = outcome(ss) else {
            return;
        };
        assert_eq!(out, BootOutcome::RomBootloader, "{ss:?}: {summary}");
        assert!(!executed, "{ss:?}: user firmware must not run: {summary}");
        assert!(summary.contains("BOOTSEL"), "{ss:?}: {summary}");
    }
}
