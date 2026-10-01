//! Live checks for the STM32G4/F7/L0 descriptors: execution evidence (main
//! reached, instructions retired, faults seen) and internal-pull modelling.
//!
//! Each check has a legal control and a faulty or unsupported input: a
//! firmware that runs cleanly against one that traps after `main`, and a pull
//! applied by the backend against the same firmware on a port map without the
//! pull register and against an external drive that must win.

#![cfg(feature = "renode")]

use hauksbee_mcu::{Mcu, PinId, RenodeBackend, RenodeConfig};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn fixture(name: &str) -> Option<PathBuf> {
    let found = crate::support::firmware(&format!("clock_truth/{name}"));
    if found.is_none() {
        eprintln!("SKIP: testdata/firmware/clock_truth/{name} not built (run make there)");
    }
    found
}

/// Boot `elf` on `cfg`, run `chunks` x 1 ms, and return the backend.
fn boot(cfg: RenodeConfig, elf: &std::path::Path, chunks: u32) -> RenodeBackend {
    let mut mcu = RenodeBackend::new(cfg).expect("Renode machine comes up");
    mcu.load_firmware(elf).expect("firmware loads");
    for _ in 0..chunks {
        mcu.run_micros(1_000).expect("chunk runs");
    }
    mcu
}

#[test]
fn clean_firmware_reports_main_reached_and_no_fault() {
    if !crate::support::renode_or_skip() {
        return;
    }
    let Some(elf) = fixture("stm32g4_tick.elf") else {
        return;
    };
    let mut mcu = boot(RenodeConfig::stm32g4(), &elf, 5);
    let probe = mcu.execution_probe();
    assert_eq!(probe.reached_entry, Some(true), "{probe:?}");
    assert_eq!(probe.entry_symbol.as_deref(), Some("main"));
    assert!(probe.instructions.unwrap_or(0) > 1_000, "{probe:?}");
    assert!(probe.pc_samples >= 5, "{probe:?}");
    assert!(probe.faults.is_empty(), "{probe:?}");
}

#[test]
fn trapping_firmware_reports_the_fault_handler() {
    if !crate::support::renode_or_skip() {
        return;
    }
    let Some(elf) = fixture("stm32g4_fault.elf") else {
        return;
    };
    let mut mcu = boot(RenodeConfig::stm32g4(), &elf, 5);
    let probe = mcu.execution_probe();
    assert_eq!(probe.reached_entry, Some(true), "{probe:?}");
    assert!(
        probe.faults.iter().any(|f| f.contains("HardFault")),
        "an undefined instruction must enter the fault handler: {probe:?}"
    );
    // The core is parked in the handler's `b .`: every sample after the trap
    // is the same PC.
    assert!(probe.distinct_pcs <= 2, "{probe:?}");
}

/// Run the pull-mirror firmware and report PA5's last level.
fn mirrored_level(cfg: RenodeConfig, drive_pb11_low: bool) -> Option<bool> {
    let elf = fixture("stm32g4_pull_mirror.elf")?;
    let mut mcu = RenodeBackend::new(cfg).expect("Renode machine comes up");
    mcu.load_firmware(&elf).expect("firmware loads");
    let pa5 = PinId { port: 'A', bit: 5 };
    let level: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let sink = level.clone();
    mcu.on_pin_change(Box::new(move |pin, high, _| {
        if pin == pa5 {
            *sink.lock().unwrap() = high;
        }
    }));
    if drive_pb11_low {
        mcu.set_digital_in(PinId { port: 'B', bit: 11 }, false);
    }
    for _ in 0..5 {
        mcu.run_micros(1_000).expect("chunk runs");
    }
    let high = *level.lock().unwrap();
    Some(high)
}

#[test]
fn internal_pull_up_reads_high_on_an_undriven_input() {
    if !crate::support::renode_or_skip() {
        return;
    }
    let Some(high) = mirrored_level(RenodeConfig::stm32g4(), false) else {
        return;
    };
    assert!(high, "PB11's pull-up must read high, so PA5 mirrors high");
}

#[test]
fn external_drive_wins_over_the_internal_pull() {
    if !crate::support::renode_or_skip() {
        return;
    }
    let Some(high) = mirrored_level(RenodeConfig::stm32g4(), true) else {
        return;
    };
    assert!(!high, "an external low on PB11 must beat its pull-up");
}

#[test]
fn without_a_pull_register_renode_reads_the_pull_up_as_low() {
    if !crate::support::renode_or_skip() {
        return;
    }
    let mut cfg = RenodeConfig::stm32g4();
    for port in &mut cfg.ports {
        port.pull = None;
    }
    let Some(high) = mirrored_level(cfg, false) else {
        return;
    };
    assert!(
        !high,
        "Renode's GPIO model ignores PUPDR; if this now reads high the backend's \
         pull application may be redundant"
    );
}
