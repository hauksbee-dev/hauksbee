//! Clock-truth measurement for the QEMU ESP32 backend: does a firmware delay
//! cost the credited virtual time it would cost on the part?
//!
//! The Renode parts answer this with `clock_truth.rs`. The QEMU backend's
//! version of the defect was different: virtual time is wall-paced (no
//! icount; it breaks esp32 boot, measured in `src/qemu/mod.rs`), and the old
//! `run_seconds` credited only the SLEPT window while the guest also ran
//! through the QMP `cont`/`stop` round trips on each side of the sleep. That
//! uncredited slack is a systematic bias — docs/cosim/MCU.md carried it as
//! "1.35x to 1.45x slow" — and it is exactly what crediting the measured
//! RESUME→STOP event window removes.
//!
//! The measurement: `esp32_blinky` toggles GPIO4 every `vTaskDelay(100 ms)`
//! of guest time. Edges are collected with their credited-cycle stamps, and
//! the span between the first and last edge is compared in two currencies
//! from ONE run:
//!
//!   - `ratio_measured`: guest time over CREDITED time (the shipped path);
//!   - `ratio_requested`: guest time over the sum of REQUESTED windows, which
//!     is precisely what the pre-fix backend credited post-boot.
//!
//! So the old bias and the new one come from the same edges and the same
//! host conditions, and "the fix tightens the factor toward 1.0" is asserted
//! directly rather than against a remembered number.
//!
//! Tolerances are deliberately loose (the residual TCG-pace wobble is
//! host-load dependent, which is why the backend also carries
//! `TIMING_LIMITATION` onto every report surface): the shipped ratio must be
//! within 25% of 1.0 and must not sit FURTHER from 1.0 than the old
//! crediting's ratio does. The 100 ms half-period is 20x the 5 ms chunk, so
//! the aliasing rules from `clock_truth.rs` hold.

#![cfg(feature = "qemu")]

use hauksbee_mcu::qemu::{is_available, QemuArch};
use hauksbee_mcu::{Mcu, PinId, QemuBackend};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Requested post-boot chunk width (what the pre-fix backend credited).
const CHUNK_US: u64 = 5_000;

/// The firmware's toggle half-period: `vTaskDelay(pdMS_TO_TICKS(100))`.
const HALF_PERIOD_MS: f64 = 100.0;

/// Edges to span. Eleven edges = ten 100 ms half-periods = 1 s of guest time.
const EDGES: usize = 11;

fn flash_image() -> Option<PathBuf> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testdata/firmware/esp32_blinky/flash.bin");
    p.exists().then(|| p.canonicalize().unwrap_or(p))
}

#[test]
fn esp32_credited_time_tracks_the_guests_delays() {
    if !is_available(QemuArch::Xtensa) {
        eprintln!("SKIP: Espressif QEMU (qemu-system-xtensa) not installed");
        return;
    }
    let Some(fw) = flash_image() else {
        eprintln!("SKIP: flash.bin not built; run ./build.sh in testdata/firmware/esp32_blinky");
        return;
    };
    let mut mcu = QemuBackend::esp32(&fw).expect("boot Espressif QEMU esp32");
    let freq = mcu.frequency() as f64;

    // Edge log: (chunk index when observed, credited cycle stamp). The chunk
    // index times CHUNK_US reconstructs what the pre-fix backend credited.
    let edges: Arc<Mutex<Vec<(u64, u64)>>> = Arc::new(Mutex::new(Vec::new()));
    let chunk = Arc::new(Mutex::new(0u64));
    let sink = edges.clone();
    let now = chunk.clone();
    mcu.on_pin_change(Box::new(move |p, _high, cycle| {
        if (p == PinId { port: '0', bit: 4 }) {
            sink.lock().unwrap().push((*now.lock().unwrap(), cycle));
        }
    }));

    // Boot (floored chunks until the mailbox MAGIC), then measure. 800 boot
    // chunks is the same generous ceiling qemu_run_window.rs uses; the blink
    // task's first edges arrive within the measurement loop itself.
    for _ in 0..800 {
        mcu.run_micros(CHUNK_US).expect("run boot chunk");
        if mcu.boot_complete() {
            break;
        }
    }
    assert!(
        mcu.boot_complete(),
        "firmware never raised the mailbox MAGIC"
    );

    // Discard edges seen during boot: boot chunks are floored/capped, so they
    // measure the boot regime, not the steady-state crediting under test.
    edges.lock().unwrap().clear();
    *chunk.lock().unwrap() = 0;

    // 900 chunks at ~5 ms is ~4.5 s of guest time against the ~1 s needed:
    // room for a heavily loaded host without an unbounded loop.
    for i in 1..=900u64 {
        mcu.run_micros(CHUNK_US).expect("run steady-state chunk");
        *chunk.lock().unwrap() = i;
        if edges.lock().unwrap().len() >= EDGES {
            break;
        }
    }

    let seen = edges.lock().unwrap().clone();
    assert!(
        seen.len() >= EDGES,
        "only {} of {EDGES} GPIO4 edges observed; the blink task never ran \
         steadily ({seen:?})",
        seen.len()
    );

    let (first, last) = (seen[0], seen[EDGES - 1]);
    let guest_ms = (EDGES - 1) as f64 * HALF_PERIOD_MS;
    let credited_ms = (last.1 - first.1) as f64 / freq * 1e3;
    let requested_ms = (last.0 - first.0) as f64 * CHUNK_US as f64 / 1e3;
    let ratio_measured = guest_ms / credited_ms;
    let ratio_requested = guest_ms / requested_ms;
    println!(
        "qemu clock_truth: {guest_ms} ms of guest delays cost {credited_ms:.1} ms credited \
         (ratio {ratio_measured:.3}) vs {requested_ms:.1} ms requested (pre-fix crediting, \
         ratio {ratio_requested:.3})"
    );

    assert!(
        (ratio_measured - 1.0).abs() <= 0.25,
        "credited time is {ratio_measured:.3}x the guest's own delay rate; even for a \
         wall-paced backend that is outside the honest band (TIMING_LIMITATION covers \
         wobble, not a systematic credit error)"
    );
    assert!(
        (ratio_measured - 1.0).abs() <= (ratio_requested - 1.0).abs() + 0.05,
        "measured-window crediting ({ratio_measured:.3}) sits further from 1.0 than the \
         pre-fix requested-window crediting ({ratio_requested:.3}): the RESUME/STOP \
         measurement is making the clock WORSE"
    );
}
