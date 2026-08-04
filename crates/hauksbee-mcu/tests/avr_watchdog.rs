//! The AVR watchdog livelock, and the two-sided proof that it is fixed.
//!
//! # The bug
//!
//! `wdt_enable(WDTO_15MS)` with no `wdt_reset()` HUNG the whole co-simulator.
//! simavr's `avr_reset` zeroes `avr->cycle`, and `AvrMcu::run_cycles` steps
//! `while raw_cycle() < run_target` against an ABSOLUTE cumulative target, so
//! once the watchdog rewound the counter the loop was chasing a number the
//! counter could never reach again. Measured before the fix: chunks 0, 1 and 2
//! returned at cycles 80,000 / 160,001 / 240,000, and chunk 3 (the 15-20 ms
//! window where WDTO_15MS bites) never returned at all. It was killed after
//! 240 s of wall clock.
//!
//! It is a livelock, not a deadlock, which is why it presented as "the co-sim
//! got slow" rather than as a crash: the emulator was executing firmware at
//! full speed the whole time, just never satisfying its exit condition.
//!
//! # Two-sided
//!
//! `wdt.elf` starves its watchdog; `nowdt.elf` is the same firmware with the one
//! arming line removed. The control completed every chunk even while the first
//! hung, so a green result on the pair cannot come from a harness that stopped
//! looking. The starved image must also report its reboots as COUNTED, and the
//! control must report zero: a rewind detector that triggered on ordinary
//! running would report phantom reboots forever.
//!
//! # Timing
//!
//! Chunks are 5 ms and the timeout is ~16 ms, so the watchdog bites in chunk 3
//! or 4 of every reboot cycle. 40 chunks is 200 ms of virtual time, about a
//! dozen reboots: enough that a fix which survives only the FIRST rewind fails
//! here.

#![cfg(feature = "avr")]

use hauksbee_mcu::{AvrMcu, Mcu};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const CHUNK_US: u64 = 5_000;
const CHUNKS: u32 = 40;

fn firmware(name: &str) -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/avr_watchdog")
        .join(name);
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}

/// Run `elf` for `CHUNKS` chunks and return (chunks completed, PB5 edges,
/// watchdog resets reported).
///
/// "Chunks completed" is the whole point: before the fix this function did not
/// return, so the assertion that matters most is that the loop terminates.
fn run(elf: &Path) -> (u32, u32, u64) {
    let mut mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    mcu.load_firmware(elf).expect("load firmware");

    let edges: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let sink = edges.clone();
    mcu.on_pin_change(Box::new(move |pin, _high, _cycle| {
        if pin.port == 'B' && pin.bit == 5 {
            *sink.lock().unwrap() += 1;
        }
    }));

    let mut completed = 0;
    for _ in 0..CHUNKS {
        mcu.run_micros(CHUNK_US).expect("run chunk");
        completed += 1;
    }
    let n = *edges.lock().unwrap();
    (completed, n, mcu.watchdog_resets())
}

/// The regression itself: a starved watchdog reboots the core, every chunk still
/// returns, and the reboots are reported rather than swallowed.
#[test]
fn a_starved_watchdog_reboots_the_core_without_wedging_the_cosim() {
    let Some(elf) = firmware("wdt.elf") else {
        eprintln!(
            "SKIP: testdata/firmware/avr_watchdog/wdt.elf not built \
             (run make in testdata/firmware/avr_watchdog)"
        );
        return;
    };
    let (completed, edges, resets) = run(&elf);

    assert_eq!(
        completed, CHUNKS,
        "every chunk must return; a rewound cycle counter used to leave the \
         step loop chasing an absolute target it could never reach"
    );
    assert!(
        resets > 0,
        "WDTO_15MS starved over {} ms of virtual time must bite at least once; \
         got {resets} reboots",
        CHUNKS as u64 * CHUNK_US / 1000
    );
    // Several reboot cycles, not just the first: a fix that re-anchored once
    // and then drifted would clear the assertion above and fail this one.
    assert!(
        resets >= 3,
        "a ~16 ms watchdog over {} ms should reboot repeatedly; got {resets}",
        CHUNKS as u64 * CHUNK_US / 1000
    );
    // And the core is genuinely running between reboots, not merely returning.
    assert!(
        edges > 0,
        "PB5 must still toggle across the reboots; got {edges} edges"
    );
}

/// The control half. Same firmware, watchdog line removed: it completed every
/// chunk even when the starved image hung, so it is what pins the hang on the
/// watchdog reset path. It must also report ZERO reboots, which is what proves
/// the rewind detector does not fire on ordinary running.
#[test]
fn the_same_firmware_without_a_watchdog_never_reboots() {
    let Some(elf) = firmware("nowdt.elf") else {
        eprintln!(
            "SKIP: testdata/firmware/avr_watchdog/nowdt.elf not built \
             (run make in testdata/firmware/avr_watchdog)"
        );
        return;
    };
    let (completed, edges, resets) = run(&elf);

    assert_eq!(completed, CHUNKS, "the control must complete every chunk");
    assert_eq!(
        resets, 0,
        "a firmware with no watchdog must report no reboots; a detector that \
         fired on ordinary running would report phantom reboots forever"
    );
    // 5 ms of toggling per 5 ms chunk: roughly one edge per chunk, and this is
    // the rate the starved image is measured against.
    assert!(
        edges >= CHUNKS - 2,
        "the control toggles PB5 every 5 ms, so ~{CHUNKS} edges in {CHUNKS} \
         chunks; got {edges}"
    );
}

/// simavr is the one backend whose watchdog behaves, so it claims no
/// limitation. That claim is what the two tests above earn, and stating it in a
/// test keeps the claim and the evidence in the same place: the moment simavr's
/// watchdog stops rebooting, this file goes red rather than the claim going
/// quietly stale.
#[test]
fn the_avr_backend_claims_no_watchdog_limitation() {
    let mcu = AvrMcu::atmega328p_16mhz().expect("create MCU");
    assert!(
        mcu.watchdog_limitation().is_none(),
        "simavr reboots at the right virtual time and reports it, so it has no \
         watchdog limitation to declare"
    );
    assert_eq!(
        mcu.watchdog_resets(),
        0,
        "a fresh core has rebooted zero times"
    );
}
