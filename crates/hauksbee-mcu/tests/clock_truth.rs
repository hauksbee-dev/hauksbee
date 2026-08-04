//! Per-backend clock-truth gate: does a firmware delay cost the virtual time
//! the part would really take?
//!
//! # The bug this exists to stop coming back
//!
//! Four Renode platforms used to run simulated time at the EMULATOR's clock
//! rate instead of the part's. `platforms/cpus/stm32f103.repl` declares
//! `nvic systickFrequency: 72000000` while `db/mcu/stm32f103.soc.toml` declares
//! an 8 MHz part, and 72/8 is exactly the 9.00x by which a SysTick-timed
//! firmware ran fast. Every stock platform also left `cpu PerformanceInMips` at
//! Renode's 100, against roughly 8 MIPS of real F103 silicon. Nothing
//! complained, because `frequency_hz` cancels out of the engine's own
//! `cycles = seconds * frequency_hz` bookkeeping: the descriptor could disagree
//! with the platform by 9x in silence. So the two time-based assertions the
//! product sells passed at rates a real board cannot hit.
//!
//! Declaring each part's clock fixed it once. This test is what keeps it fixed:
//! the numbers rot the moment nobody measures them.
//!
//! # How the measurement avoids the trap that nearly buried the bug
//!
//! The engine sees a pin by polling its output-data register once per chunk, so
//! a half-period at or below the chunk width ALIASES. At 5 ms chunks the 9x-fast
//! F103 firmware read a perfect 100 edges in 2 s and looked exact; the same
//! firmware at 200 us chunks read 450. Two rules follow, and both are load
//! bearing:
//!
//! 1. `CHUNK_US` is 1 ms against a `HALF_PERIOD_MS` of 100 ms, so the poll is
//!    100x finer than the silicon half-period and still 10x finer than the
//!    half-period of a sim running 10x fast. A gate whose chunk is only 10x
//!    finer than the CORRECT period can be fooled by a fast sim, which is
//!    exactly how the aliased reading happened.
//! 2. The measurement is the SIM TIME AT WHICH THE Nth EDGE ARRIVES, not an
//!    edge count in a fixed window. Edge times are monotone, so a missed edge
//!    can only ever make the sim look SLOWER than it is, never faster: the
//!    failure mode is a false RED, which someone investigates, rather than the
//!    false GREEN a count gives.
//!
//! # Reading a failure
//!
//! `ratio` is sim rate over silicon rate. Above 1.0 the part runs fast (virtual
//! time is cheaper than the board's), below 1.0 it runs slow. The tolerance is
//! 5%, which is loose against the 0.2% quantization of a 1 ms chunk over a
//! 500 ms window and tight against the 6.5x-to-9x errors that were shipping.

#![cfg(feature = "renode")]

use hauksbee_mcu::renode::is_available;
use hauksbee_mcu::{Mcu, PinId, RenodeBackend, RenodeConfig};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Poll interval. 100x finer than the firmware's silicon half-period; see the
/// aliasing rules in the module docs before changing it.
const CHUNK_US: u64 = 1_000;

/// The firmware's real-silicon half-period, from `tick.c`'s `HALF_PERIOD_MS`.
const HALF_PERIOD_MS: f64 = 100.0;

/// How many edges to wait for. Five puts the deadline at 500 ms of silicon
/// time, far enough out that the 1 ms poll quantization is 0.2% of the answer.
const EDGES: u32 = 5;

/// Chunk budget. 1.8x the correct number of chunks, so a sim as slow as 0.55x
/// still produces a measured ratio to report rather than a bare timeout, while
/// a sim stuck forever gives up in bounded time.
const MAX_CHUNKS: u64 = 900;

/// Accepted deviation from the part's real rate.
const TOLERANCE: f64 = 0.05;

fn firmware(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/clock_truth")
        .join(name);
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}

/// Boot `elf` on `cfg` and return the sim rate divided by the silicon rate.
///
/// `None` means "could not measure", never "measured fine": a spawn or load
/// failure is reported by the caller as a SKIP, not swallowed into a pass.
fn measure(cfg: RenodeConfig, elf: &PathBuf, pin: PinId) -> Option<f64> {
    let mut mcu = RenodeBackend::new(cfg).ok()?;
    mcu.load_firmware(elf).ok()?;

    // Chunk index of every edge seen, so the whole edge train is available in
    // the failure message rather than only the deadline.
    let edges: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));
    let chunk = Arc::new(Mutex::new(0u64));
    let sink = edges.clone();
    let now = chunk.clone();
    mcu.on_pin_change(Box::new(move |p, _high, _cycle| {
        if p == pin {
            sink.lock().unwrap().push(*now.lock().unwrap());
        }
    }));

    for i in 1..=MAX_CHUNKS {
        mcu.run_micros(CHUNK_US).ok()?;
        *chunk.lock().unwrap() = i;
        if edges.lock().unwrap().len() >= EDGES as usize {
            break;
        }
    }

    let seen = edges.lock().unwrap().clone();
    if seen.len() < EDGES as usize {
        // Report the shortfall as a ratio anyway: the run was bounded, so the
        // most this can be is a lower bound on how slow the part is.
        println!(
            "clock_truth: only {} of {EDGES} edges in {MAX_CHUNKS} x {CHUNK_US} us",
            seen.len()
        );
        return Some(0.0);
    }
    let sim_ms = seen[EDGES as usize - 1] as f64 * CHUNK_US as f64 / 1000.0;
    let silicon_ms = EDGES as f64 * HALF_PERIOD_MS;
    println!("clock_truth: edges at chunks {seen:?}; {EDGES}th edge at {sim_ms} ms sim, {silicon_ms} ms on silicon");
    Some(silicon_ms / sim_ms)
}

fn gate(part: &str, cfg: RenodeConfig, elf_name: &str, pin: PinId) {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return;
    }
    let Some(elf) = firmware(elf_name) else {
        eprintln!(
            "SKIP: testdata/firmware/clock_truth/{elf_name} not built \
             (run make in testdata/firmware/clock_truth)"
        );
        return;
    };
    let Some(ratio) = measure(cfg, &elf, pin) else {
        eprintln!("SKIP: could not bring up {part} under Renode");
        return;
    };
    assert!(
        (ratio - 1.0).abs() <= TOLERANCE,
        "{part}: simulated time runs at {ratio:.3}x the part's real rate. \
         The platform's declared core clock disagrees with the part: check \
         `cpu PerformanceInMips` and `nvic systickFrequency` in the \
         descriptor's platform_repl against soc.frequency_hz."
    );
}

/// STM32F103 on its reset-default 8 MHz HSI. Was 9.00x fast: the stock
/// platform's `systickFrequency: 72000000` against an 8 MHz part.
#[test]
fn stm32f103_runs_at_the_parts_clock() {
    gate(
        "renode:stm32f103",
        RenodeConfig::stm32f103(),
        "stm32f103_tick.elf",
        PinId { port: 'C', bit: 13 },
    );
}

/// STM32F407 on its reset-default 16 MHz HSI. The base `stm32f4.repl` carries
/// the same `systickFrequency: 72000000` copy-paste as the F1.
#[test]
fn stm32f4_runs_at_the_parts_clock() {
    gate(
        "renode:stm32f4_discovery",
        RenodeConfig::stm32f4_discovery(),
        "stm32f4_tick.elf",
        PinId { port: 'D', bit: 12 },
    );
}

/// nRF52840 at its fixed 64 MHz HFCLK. Was 6.54x fast: the stock platform
/// declares no `systickFrequency` at all, so SysTick ran at a Renode default
/// with no relation to the part.
#[test]
fn nrf52840_runs_at_the_parts_clock() {
    gate(
        "renode:nrf52840",
        RenodeConfig::nrf52840(),
        "nrf52840_tick.elf",
        PinId { port: '0', bit: 13 },
    );
}
