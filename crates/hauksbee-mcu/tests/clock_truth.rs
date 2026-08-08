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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Poll interval. 100x finer than the firmware's silicon half-period; see the
/// aliasing rules in the module docs before changing it.
const CHUNK_US: u64 = 1_000;

/// The SysTick firmwares' real-silicon half-period, from `tick.c`'s
/// `HALF_PERIOD_MS`.
const HALF_PERIOD_MS: f64 = 100.0;

/// The FE310 mtime oracle's real-silicon half-period: `fe310_tick.rs` toggles
/// every 3277 mtime ticks and the part counts mtime at 32.768 kHz, so this is
/// 3277 / 32.768 ms. Kept as the exact expression so the tick constant is
/// auditable against the firmware source.
const FE310_HALF_PERIOD_MS: f64 = 3277.0 / 32.768;

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

/// Boot `elf` on `cfg` and return the sim rate divided by the silicon rate,
/// for a firmware whose real-silicon half-period is `half_period_ms`.
///
/// `None` means "could not measure", never "measured fine": a spawn or load
/// failure is reported by the caller as a SKIP, not swallowed into a pass.
fn measure(cfg: RenodeConfig, elf: &Path, pin: PinId, half_period_ms: f64) -> Option<f64> {
    let mut mcu = RenodeBackend::new(cfg).ok()?;
    mcu.load_firmware(elf).ok()?;
    // Poll only the port the firmware drives. Every chunk costs one Monitor
    // round trip per polled register, and the F103 descriptor carries seven
    // ports with a CRL/CRH pair each: left unhinted, a 500-chunk measurement
    // spends 21 round trips per chunk reading ports no firmware here touches.
    // This is also what the engine does with a bound board, so the gate
    // measures the path the product uses.
    mcu.set_active_ports(&[pin.port]);

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
    let silicon_ms = EDGES as f64 * half_period_ms;
    println!("clock_truth: edges at chunks {seen:?}; {EDGES}th edge at {sim_ms} ms sim, {silicon_ms} ms on silicon");
    Some(silicon_ms / sim_ms)
}

/// Measure `elf` on `cfg`, or explain (as a SKIP) why it could not be
/// measured. The two gates below share this so PASS and FAIL are judged from
/// the identical measurement path.
fn measured_ratio(
    part: &str,
    cfg: RenodeConfig,
    elf_name: &str,
    pin: PinId,
    half_period_ms: f64,
) -> Option<f64> {
    if !is_available() {
        eprintln!("SKIP: Renode not installed");
        return None;
    }
    let Some(elf) = firmware(elf_name) else {
        eprintln!(
            "SKIP: testdata/firmware/clock_truth/{elf_name} not built \
             (run make in testdata/firmware/clock_truth)"
        );
        return None;
    };
    let ratio = measure(cfg, &elf, pin, half_period_ms);
    if ratio.is_none() {
        eprintln!("SKIP: could not bring up {part} under Renode");
    }
    ratio
}

fn gate(part: &str, cfg: RenodeConfig, elf_name: &str, pin: PinId) {
    let Some(ratio) = measured_ratio(part, cfg, elf_name, pin, HALF_PERIOD_MS) else {
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

/// The FE310 oracle drives GPIO 19 (the HiFive1 green LED) on port '0'.
const FE310_PIN: PinId = PinId { port: '0', bit: 19 };

/// FE310 `mtime` at the part's 32.768 kHz RTC tick. Was declared 62 MHz by
/// the stock platform, a 1892x error nothing could measure until the
/// `fe310_tick.rs` oracle existed; the descriptor now overrides
/// `clint frequency` to 32768 and this gate holds it there. The core-clock
/// gate (`PerformanceInMips`) does not police the CLINT because mtime is a
/// separate clock domain on the part, which is exactly why it needs its own
/// oracle.
#[test]
fn fe310_mtime_counts_at_the_parts_rtc_rate() {
    let Some(ratio) = measured_ratio(
        "renode:sifive_fe310",
        RenodeConfig::sifive_fe310(),
        "fe310_tick.elf",
        FE310_PIN,
        FE310_HALF_PERIOD_MS,
    ) else {
        return;
    };
    assert!(
        (ratio - 1.0).abs() <= TOLERANCE,
        "renode:sifive_fe310: mtime-timed simulated time runs at {ratio:.3}x \
         the part's real rate. The platform's `clint frequency` disagrees with \
         the FE310's 32.768 kHz RTC tick: check the clint override in \
         db/mcu/sifive_fe310.soc.toml's platform_repl."
    );
}

/// The other side of the same gate: a deliberately WRONG declared mtime rate
/// must FAIL the measurement, or the PASS above proves only that the gate
/// cannot tell right from wrong. The stock platform's 62 MHz is used as the
/// wrong value because it is the exact defect the descriptor's override
/// corrects: if this measurement ever reads within tolerance, the gate has
/// gone blind (aliasing, a dead pin, a stuck measurement) and the green
/// `fe310_mtime_counts_at_the_parts_rtc_rate` means nothing.
#[test]
fn fe310_gate_fails_a_deliberately_wrong_mtime_rate() {
    let canonical = include_str!("../db/mcu/sifive_fe310.soc.toml");
    let wrong = canonical.replace("frequency: 32768", "frequency: 62000000");
    assert_ne!(
        canonical, wrong,
        "the descriptor no longer declares `clint frequency: 32768`; \
         update this test's substitution alongside it"
    );
    let cfg = RenodeConfig::from_soc_toml(&wrong).expect("wrong-clint descriptor still parses");
    let Some(ratio) = measured_ratio(
        "renode:sifive_fe310 (deliberately wrong CLINT)",
        cfg,
        "fe310_tick.elf",
        FE310_PIN,
        FE310_HALF_PERIOD_MS,
    ) else {
        return;
    };
    // Strictly FAST, not merely out of tolerance: a dead pin or a stuck
    // measurement reads 0.0 from `measure`'s shortfall path, and 0.0 is
    // "out of tolerance" too, which would let a broken gate pass this test
    // without ever demonstrating it can see the 1892x error. A 62 MHz mtime
    // makes virtual time cheap, so the failure this test guards must present
    // as fast; anything else is the gate malfunctioning, which is exactly
    // what this test exists to catch.
    assert!(
        ratio > 1.0 + TOLERANCE,
        "a 62 MHz mtime measured {ratio:.3}x, not clearly FAST: either the gate \
         can no longer distinguish a 1892x-wrong timer from a correct one \
         (ratio near 1.0, its passing sibling is vacuous), or the measurement \
         itself is broken (ratio at or below 1.0, e.g. a dead pin reading 0.0)"
    );
}
