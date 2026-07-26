//! Regressions for the in-process AVR core's run loop timekeeping and
//! terminal-state reporting.
//!
//! 1. Forward cycle drift: `avr_run` steps whole instructions (1-5 cycles),
//!    so each `run_cycles` chunk stops up to 4 cycles past its endpoint. The
//!    old loop targeted `start + n` per chunk and DISCARDED that overshoot,
//!    re-incurring it every chunk, over many chunks the emulated clock ran
//!    unboundedly ahead of simulated time (a 16 MHz core did more than
//!    16e6·t cycles). The fix anchors the loop to an absolute cumulative
//!    target, so the total error stays bounded by one instruction.
//!
//! 2. Swallowed terminal states: the loop broke on `cpu_Done`/`cpu_Crashed`
//!    but still returned a clean `Ok`, and `McuState` carried no flag, a
//!    crashed MCU was indistinguishable from a healthy chunk. The fix
//!    surfaces both through `state().crashed` / `state().done`.
//!
//! Firmware is hand-assembled inline (a few opcodes each) and written as
//! Intel HEX, so the tests need no external toolchain or fixture repo.

// Drives the in-process simavr core: GPL-gated `avr` feature only.
#![cfg(feature = "avr")]

use hauksbee_mcu::{AvrMcu, Mcu};
use std::fmt::Write as _;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Inline Intel-HEX fixtures
// ---------------------------------------------------------------------------

/// Emit one Intel-HEX record with a correct checksum.
fn ihex_record(record_type: u8, addr: u16, data: &[u8]) -> String {
    let mut sum = data.len() as u8;
    sum = sum
        .wrapping_add((addr >> 8) as u8)
        .wrapping_add(addr as u8)
        .wrapping_add(record_type);
    let mut line = format!(":{:02X}{:04X}{:02X}", data.len(), addr, record_type);
    for &b in data {
        sum = sum.wrapping_add(b);
        write!(line, "{b:02X}").unwrap();
    }
    write!(line, "{:02X}", (!sum).wrapping_add(1)).unwrap();
    line
}

/// Write `program` (raw flash bytes, little-endian words, loaded at 0) to a
/// temp .hex and return its path.
fn write_program_hex(name: &str, program: &[u8]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hauksbee-run-clock-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let mut text = String::new();
    for (i, chunk) in program.chunks(16).enumerate() {
        text.push_str(&ihex_record(0x00, (i * 16) as u16, chunk));
        text.push('\n');
    }
    text.push_str(":00000001FF\n"); // EOF record
    std::fs::write(&path, text).unwrap();
    path
}

/// `jmp 0` at address 0: an infinite self-jump of 3-CYCLE instructions, so a
/// 16-cycle (1 µs @ 16 MHz) budget never lands on an instruction boundary and
/// every chunk genuinely overshoots. JMP k = 0x940C + k16, little-endian.
const JMP_LOOP: &[u8] = &[0x0C, 0x94, 0x00, 0x00];

/// `rjmp .-2`: the tiniest healthy firmware (2-cycle self-loop).
const RJMP_LOOP: &[u8] = &[0xFF, 0xCF];

/// `jmp 0x4000` (word address) at address 0: lands the PC at byte address
/// 0x8000, past the ATmega328P's `flashend` (0x7FFF). simavr's `avr_run_one`
/// treats a PC beyond flash as a firmware crash (`avr_sadly_crashed` →
/// `cpu_Crashed`); the "jumped into the weeds" failure mode.
const JMP_INTO_WEEDS: &[u8] = &[
    0x0C, 0x94, 0x00, 0x40, // jmp 0x4000 (words) = byte pc 0x8000
    0xFF, 0xCF, // rjmp .-2 (never reached)
];

/// `cli; sleep; rjmp .-2`: sleeping with interrupts disabled is simavr's
/// clean-exit path (`cpu_Done`).
const CLI_SLEEP: &[u8] = &[
    0xF8, 0x94, // cli
    0x88, 0x95, // sleep
    0xFF, 0xCF, // rjmp .-2 (never reached)
];

fn mcu_with(program: &[u8], name: &str, freq: u64) -> AvrMcu {
    let hex = write_program_hex(name, program);
    let mut mcu = AvrMcu::new("atmega328p", freq).expect("create MCU");
    mcu.load_firmware(&hex).expect("load inline hex");
    mcu
}

// ---------------------------------------------------------------------------
// F1: no accumulating forward drift across chunks
// ---------------------------------------------------------------------------

/// Many small chunks of a 3-cycle-instruction loop: total elapsed cycles must
/// track freq·t to within ONE instruction, not accumulate the per-chunk
/// overshoot. 2000 × 1 µs @ 16 MHz = 32 000 cycles; each 16-cycle chunk ends
/// on a multiple of 3, so the old `start + n` loop ran 18 cycles per chunk,
/// 36 000 total, 12.5 % ahead. The absolute target keeps it ≤ 32 000 + 3.
#[test]
fn chunked_run_micros_does_not_drift_ahead() {
    let mut mcu = mcu_with(JMP_LOOP, "jmp_loop.hex", 16_000_000);

    let start = mcu.state().cycles;
    let chunks: u64 = 2000;
    for _ in 0..chunks {
        mcu.run_micros(1).expect("run_micros");
    }
    let elapsed = mcu.state().cycles - start;
    let expected = chunks * 16; // 16 cycles per µs at 16 MHz

    assert!(
        elapsed >= expected,
        "core must reach the cumulative target: {elapsed} < {expected}"
    );
    assert!(
        elapsed <= expected + 3,
        "per-chunk overshoot must not accumulate: {elapsed} cycles for \
         {expected} budgeted (old per-chunk-target loop gave ~{})",
        chunks * 18
    );
}

/// The earlier fractional-carry fix must survive: a clock that is NOT an
/// integer number of cycles per chunk (3.6864 MHz → 368.64 cycles / 100 µs)
/// still tracks total time exactly, with the carry folded into the absolute
/// target. 100 × 100 µs = 10 ms → exactly 36 864 cycles.
#[test]
fn fractional_clock_still_tracks_over_chunks() {
    let mut mcu = mcu_with(JMP_LOOP, "jmp_loop_frac.hex", 3_686_400);

    let start = mcu.state().cycles;
    for _ in 0..100 {
        mcu.run_micros(100).expect("run_micros");
    }
    let elapsed = mcu.state().cycles - start;
    let expected = 36_864u64; // carried exactly; truncation would lose 64

    assert!(
        elapsed >= expected && elapsed <= expected + 3,
        "fractional clock must track time within one instruction: \
         got {elapsed}, want {expected}..={}",
        expected + 3
    );
}

// ---------------------------------------------------------------------------
// F2: terminal CPU states are observable
// ---------------------------------------------------------------------------

/// A firmware that jumps past the end of flash crashes the core; the crash
/// must be visible through `McuState::crashed`, not swallowed as a clean
/// chunk (the old run loop broke on `cpu_Crashed` but returned Ok with no
/// flag anywhere, so co-sim/coverage saw a healthy MCU).
#[test]
fn crashed_core_is_observable_via_state() {
    let mut mcu = mcu_with(JMP_INTO_WEEDS, "jmp_weeds.hex", 16_000_000);

    assert!(!mcu.state().crashed, "core must not be born crashed");

    // The run itself still returns Ok (partial cycles ran before the crash);
    // the terminal state is carried by the state snapshot.
    mcu.run_millis(1).expect("run over the crash");
    let st = mcu.state();
    assert!(
        st.crashed,
        "out-of-RAM store must surface as crashed=true (pc=0x{:04X})",
        st.pc
    );

    // A crashed core makes no further progress, and the run loop must not
    // spin toward the now-unreachable absolute target.
    let cycles_at_crash = st.cycles;
    mcu.run_millis(5).expect("post-crash run returns promptly");
    let st2 = mcu.state();
    assert!(st2.crashed, "crash state must be sticky");
    assert_eq!(
        st2.cycles, cycles_at_crash,
        "a crashed core must not advance its cycle counter"
    );
}

/// `sleep` with interrupts disabled is simavr's clean termination
/// (`cpu_Done`); it must surface as `done`, distinct from `crashed`.
#[test]
fn done_core_is_observable_via_state() {
    let mut mcu = mcu_with(CLI_SLEEP, "cli_sleep.hex", 16_000_000);

    mcu.run_millis(1).expect("run to completion");
    let st = mcu.state();
    assert!(st.done, "sleep-with-interrupts-off must surface as done=true");
    assert!(!st.crashed, "a clean exit is not a crash");
}

/// A healthy spinning firmware reports neither terminal flag.
#[test]
fn clean_run_reports_no_terminal_state() {
    let mut mcu = mcu_with(RJMP_LOOP, "rjmp_loop.hex", 16_000_000);

    mcu.run_millis(1).expect("run");
    let st = mcu.state();
    assert!(st.cycles > 0, "the loop must actually execute");
    assert!(!st.crashed, "a healthy loop must report crashed=false");
    assert!(!st.done, "a healthy loop must report done=false");
}
