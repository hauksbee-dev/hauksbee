//! Clock-truth oracle for the FE310's machine timer: put a KNOWN REAL-SILICON
//! PERIOD, timed by `mtime`, on a GPIO pin.
//!
//! WHY THIS EXISTS
//! ---------------------------------------------------------------------------
//! The stock Renode `sifive-fe310.repl` declares `clint frequency: 62000000`,
//! while the real FE310-G002 drives `mtime` from the 32.768 kHz always-on RTC
//! tick (rtcclk; FE310-G002 manual, the same document the descriptor cites for
//! the 16 MHz HFROSC default). That is a 1892x error, and it sat unfixed
//! because no in-tree firmware exercised `mtime` on an observable path: a
//! 1892x edit to a timer nobody can measure is how the original core-clock
//! defect was introduced. This firmware is the measuring stick that makes the
//! edit safe: `crates/hauksbee-mcu/tests/clock_truth.rs` boots it on the
//! corrected platform and asserts the measured rate, and boots it on a
//! deliberately wrong platform and asserts the gate FAILS.
//!
//! WHAT MAKES THE PERIOD A SILICON FACT
//! ---------------------------------------------------------------------------
//! `mtime` counts rtcclk ticks at 32.768 kHz on the part, independent of the
//! core clock, PLL state, or anything firmware configures. HALF_PERIOD_TICKS
//! (3277) of `mtime` is therefore 3277 / 32768 s = 100.006 ms of real time
//! between edges on a bench, for any speed grade. The 0.006% quantization
//! (3277 vs the non-integer 3276.8) is noise against the gate's 5% tolerance.
//!
//! The same aliasing rules as `tick.c` apply (see its header): 100 ms keeps
//! the gate's 1 ms poll 100x finer than the silicon half-period. A sim
//! counting mtime at the stock 62 MHz toggles every ~53 us, far below the
//! poll, and aliases into edge noise the Nth-edge measurement reads as a rate
//! tens of times too fast: loudly outside tolerance, never quietly inside it.
//!
//! BUILD (see the Makefile): rustc for riscv32imac-unknown-none-elf, no libc,
//! no vendor SDK, everything linked into the 16 KB DTIM at 0x8000_0000. The
//! entry symbol is `vinit` because the descriptor's `post_load_setup` sets the
//! PC there on every FE310 firmware (the Zephyr-demo bring-up footgun).

#![no_std]
#![no_main]

use core::ptr::{read_volatile, write_volatile};

// Entry: set up a stack at the top of DTIM and enter Rust. Named `vinit`
// because `post_load_setup` runs `{cpu} PC `sysbus GetSymbolAddress "vinit"``.
core::arch::global_asm!(
    ".section .text.vinit, \"ax\"",
    ".globl vinit",
    "vinit:",
    "    la sp, _stack_top",
    "    call rust_main",
    "1:  j 1b",
);

/// FE310 GPIO output-enable register (manual: GPIO `output_en`, base + 0x08).
const GPIO_OUTPUT_EN: *mut u32 = 0x1001_2008 as *mut u32;
/// FE310 GPIO output-value register (manual: GPIO `port`, base + 0x0C). The
/// descriptor's ODR poll reads this exact register.
const GPIO_PORT: *mut u32 = 0x1001_200C as *mut u32;
/// CLINT `mtime`, low word (base 0x0200_0000 + 0xBFF8). The low 32 bits wrap
/// after 36 hours at 32.768 kHz; the wrapping compare below is immune.
const MTIME_LO: *const u32 = 0x0200_BFF8 as *const u32;

/// GPIO 19, the HiFive1's green LED.
const LED: u32 = 1 << 19;

/// Half-period in mtime ticks: 3277 / 32768 Hz = 100.006 ms of real time.
const HALF_PERIOD_TICKS: u32 = 3277;

#[no_mangle]
pub extern "C" fn rust_main() -> ! {
    unsafe {
        write_volatile(GPIO_OUTPUT_EN, LED);
        let mut out: u32 = 0;
        let mut next = read_volatile(MTIME_LO).wrapping_add(HALF_PERIOD_TICKS);
        loop {
            // Wrapping "mtime < next": the sign bit of the mod-2^32 difference.
            while read_volatile(MTIME_LO).wrapping_sub(next) & 0x8000_0000 != 0 {}
            next = next.wrapping_add(HALF_PERIOD_TICKS);
            out ^= LED;
            write_volatile(GPIO_PORT, out);
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
