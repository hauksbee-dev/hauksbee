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
    let boot1_net = if boot1_rail {
        "2 \"+3V3\""
    } else {
        "1 \"GND\""
    };
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
    /// What every surface asks: did the firmware do nothing?
    quiet: bool,
    stops: Vec<String>,
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
    let quiet = engine.scheduler().firmware_quiet(0, !uart.is_empty());
    let stops = engine.scheduler().boot_stops();
    Some(Run {
        evidence,
        pll_high,
        uart,
        quiet,
        stops,
    })
}

#[test]
fn boot0_high_enters_the_system_bootloader_and_the_firmware_does_not_run() {
    let Some(r) = run(Boot0::Rail, false, false, &[0x7F, 0x02, 0xFD]) else {
        return;
    };
    let boot = r
        .evidence
        .boot
        .as_ref()
        .expect("a boot decision is reported");
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
        r.evidence
            .blockers
            .iter()
            .any(|b| b.contains("boot straps")),
        "{:?}",
        r.evidence.blockers
    );
    assert!(!r.pll_high, "the firmware's marker pin must stay quiet");
    // AN3155: 0x7F -> ACK (0x79); Get ID -> ACK, 1, PID MSB, PID LSB, ACK.
    assert_eq!(r.uart, vec![0x79, 0x79, 0x01, 0x04, 0x10, 0x79]);
    // The ROM's reply is UART traffic, but it is not the firmware running: every
    // surface still reads the run as one that exercised no firmware, and says why.
    assert!(
        r.quiet,
        "the bootloader's reply must not count as firmware activity"
    );
    assert_eq!(r.stops.len(), 1, "{:?}", r.stops);
    assert!(r.stops[0].starts_with("U1: BOOT0"), "{:?}", r.stops);
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

// --- ESP32 (Espressif QEMU): the machine's own ROM takes the strap path ---

#[cfg(feature = "qemu")]
mod esp32 {
    use super::*;
    use hauksbee_mcu::qemu::{is_available, QemuArch};

    fn qemu_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    const BOARD: &str = include_str!("../../../testdata/boards/esp32_devkit_demo.kicad_pcb");

    /// The devkit demo with GPIO0 (module pad 25) left open, or tied to ground
    /// the way a held BOOT button ties it.
    fn board(gpio0_grounded: bool) -> String {
        if !gpio0_grounded {
            return BOARD.to_string();
        }
        let anchor = "(pad 34 smd rect (at 3 2) (net 0 \"\"))";
        assert!(BOARD.contains(anchor), "demo board changed");
        BOARD.replace(
            anchor,
            &format!("(pad 25 smd rect (at 3 4) (net 1 \"GND\"))\n    {anchor}"),
        )
    }

    struct Run {
        evidence: ExecutionEvidence,
        uart: String,
        blink_edges: u32,
    }

    fn run(gpio0_grounded: bool) -> Option<Run> {
        hauksbee_testkit::skip_if!(
            !is_available(QemuArch::Xtensa) => None,
            "Espressif QEMU (qemu-system-xtensa) not installed"
        );
        let fw = hauksbee_testkit::skip_none!(
            crate::support::testdata("firmware/esp32_blinky/flash.bin") => None,
            "esp32_blinky flash.bin not built"
        );
        let mut engine = HauksbeeEngine::from_board_file(
            &board(gpio0_grounded),
            Some(&fw),
            "/boards/esp32_boot_straps.kicad_pcb",
        )
        .expect("build the ESP32 boot-strap engine");
        engine.scheduler_mut().chunk_s = 5e-3;
        let mut uart = Vec::new();
        let mut blink_edges = 0;
        let mut prev = false;
        for _ in 0..300 {
            let frame = engine.step(5e-3);
            for bytes in frame.uart.values() {
                uart.extend_from_slice(bytes);
            }
            let high = frame
                .net_voltages
                .get("GPIO4_BLINK")
                .copied()
                .unwrap_or(0.0)
                > 2.0;
            if high != prev {
                blink_edges += 1;
            }
            prev = high;
        }
        engine.scheduler_mut().refresh_execution_evidence();
        let evidence = engine.scheduler().execution_evidence().first()?.clone();
        Some(Run {
            evidence,
            uart: String::from_utf8_lossy(&uart).to_string(),
            blink_edges,
        })
    }

    #[test]
    fn an_open_gpio0_boots_the_user_firmware() {
        let _guard = qemu_test_lock();
        let Some(r) = run(false) else { return };
        let boot = r
            .evidence
            .boot
            .as_ref()
            .expect("a boot decision is reported");
        assert_eq!(boot.outcome, BootOutcome::UserFlash, "{}", boot.summary);
        assert!(boot.user_firmware_runs);
        assert!(r.uart.contains("hello from esp32"), "{:?}", r.uart);
        assert!(r.blink_edges > 0);
    }

    #[test]
    fn gpio0_held_low_enters_the_rom_download_mode_and_the_firmware_does_not_run() {
        let _guard = qemu_test_lock();
        let Some(r) = run(true) else { return };
        let boot = r
            .evidence
            .boot
            .as_ref()
            .expect("a boot decision is reported");
        assert_eq!(boot.outcome, BootOutcome::RomBootloader, "{}", boot.summary);
        assert!(!boot.user_firmware_runs);
        assert!(boot.summary.contains("GPIO0 = 0.00 V"), "{}", boot.summary);
        assert!(!r.evidence.executed, "{:?}", r.evidence);
        assert!(!r.evidence.firmware_pass_eligible);
        assert!(
            !r.uart.contains("hello from esp32"),
            "the user firmware's banner must not appear: {:?}",
            r.uart
        );
        assert_eq!(r.blink_edges, 0, "the firmware's blink must not run");
    }
}

// --- RP2040: QSPI_SS low at boot is BOOTSEL; RUN low holds the chip in reset ---

mod rp2040 {
    use super::*;

    #[derive(Clone, Copy, PartialEq)]
    enum QspiSs {
        Open,
        Gnd,
    }

    fn board_text(qspi_ss: QspiSs, run_grounded: bool) -> String {
        let ss_pad = match qspi_ss {
            QspiSs::Open => "(pad 56 smd rect (at -3 9) (net 0 \"\"))",
            QspiSs::Gnd => "(pad 56 smd rect (at -3 9) (net 1 \"GND\"))",
        };
        // RUN reaches a rail through 10k, the way a pull-up or a reset switch
        // wires it (a pad tied straight to ground is refused outright).
        let (run_pull_net, run_pull_name) = if run_grounded {
            (1, "GND")
        } else {
            (2, "+3V3")
        };
        // The 12 MHz crystal the SDK's start-up waits for: without it the
        // firmware stalls before `main`, as on silicon.
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
    {ss_pad}
    (pad 57 smd rect (at 3 0) (net 1 "GND"))
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
    (pad 2 thru_hole circle (at 2 0) (net {run_pull_net} "{run_pull_name}"))
  )
{xtal_parts}
)"#
        )
    }

    struct Run {
        evidence: ExecutionEvidence,
        led_high: bool,
        uart: Vec<u8>,
    }

    fn run(qspi_ss: QspiSs, run_grounded: bool) -> Option<Run> {
        let fw = hauksbee_testkit::skip_none!(
            crate::support::testdata("firmware/rp2040_blink_uart/blink_uart.elf") => None,
            "blink_uart.elf not built"
        );
        let mut engine = HauksbeeEngine::from_board_file(
            &board_text(qspi_ss, run_grounded),
            Some(&fw),
            "/boards/rp2040_boot_straps.kicad_pcb",
        )
        .expect("build the RP2040 boot-strap engine");
        engine.scheduler_mut().chunk_s = 1e-3;
        let mut led_high = false;
        let mut uart = Vec::new();
        for _ in 0..200 {
            let frame = engine.step(1e-3);
            led_high |= frame.net_voltages.get("LED").copied().unwrap_or(0.0) > 2.0;
            for bytes in frame.uart.values() {
                uart.extend_from_slice(bytes);
            }
        }
        engine.scheduler_mut().refresh_execution_evidence();
        let evidence = engine.scheduler().execution_evidence().first()?.clone();
        Some(Run {
            evidence,
            led_high,
            uart,
        })
    }

    #[test]
    fn qspi_ss_held_low_boots_the_usb_bootloader_and_the_flash_firmware_does_not_run() {
        let Some(r) = run(QspiSs::Gnd, false) else {
            return;
        };
        let boot = r
            .evidence
            .boot
            .as_ref()
            .expect("a boot decision is reported");
        assert_eq!(boot.outcome, BootOutcome::RomBootloader, "{}", boot.summary);
        assert!(!boot.user_firmware_runs);
        assert!(boot.summary.contains("QSPI_SS"), "{}", boot.summary);
        assert!(!r.evidence.executed);
        assert!(!r.evidence.firmware_pass_eligible);
        assert!(!r.led_high, "the firmware's LED pin must stay quiet");
        assert!(r.uart.is_empty(), "{:?}", r.uart);
    }

    #[test]
    fn run_held_low_keeps_the_chip_in_reset() {
        let Some(r) = run(QspiSs::Open, true) else {
            return;
        };
        let boot = r.evidence.boot.as_ref().unwrap();
        assert_eq!(boot.outcome, BootOutcome::HeldInReset, "{}", boot.summary);
        assert!(!r.evidence.executed);
        assert!(!r.led_high);
    }

    #[cfg(feature = "renode")]
    #[test]
    fn an_open_qspi_ss_takes_its_pull_up_and_boots_the_flash_firmware() {
        if !hauksbee_mcu::renode::is_available() {
            eprintln!("skipping: Renode not installed");
            return;
        }
        let Some(r) = run(QspiSs::Open, false) else {
            return;
        };
        let boot = r.evidence.boot.as_ref().unwrap();
        assert_eq!(boot.outcome, BootOutcome::UserFlash, "{}", boot.summary);
        assert!(boot.user_firmware_runs);
        // The board carries the reference-design crystal, so the SDK's wait for
        // the oscillator ends and the firmware really runs.
        assert!(r.evidence.executed, "{:?}", r.evidence);
    }
}
