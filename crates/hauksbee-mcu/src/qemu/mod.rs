//! Espressif-QEMU-backed MCU emulation for the ESP32 family.
//!
//! [`QemuBackend`] drives a headless Espressif QEMU process (the fork with full
//! ESP32 SoC peripheral models) over two TCP control channels plus a UART
//! socket, exposing the same generic [`Mcu`] trait the
//! simavr and Renode backends implement. The engine's lockstep contract is
//! unchanged: it calls `run_micros`, exchanges GPIO/UART state, and this backend
//! translates that into QEMU control operations.
//!
//! # Why QEMU and not Renode for the ESP32
//!
//! Renode (as of 1.16.1) ships no `esp32.repl` / `esp32c3.repl`: neither the
//! Xtensa ESP32 nor the RISC-V ESP32-C3 has a turnkey platform. Espressif
//! maintains a QEMU fork with the ESP32 GPIO matrix, UART, SPI-flash controller
//! and timers modelled, and publishes native macOS-arm64 / Linux binaries. So
//! the ESP32 path is a separate, backend-pluggable QEMU backend rather than a
//! Renode config. (The RISC-V ESP32-C3 also has a Renode-less story here; see
//! docs/cosim/MCU.md for the per-part status.)
//!
//! # Lockstep mechanism (chosen empirically)
//!
//! The contract is: advance a bounded amount of guest virtual time, then block
//! until done so the analog solver can run the matching chunk. The mechanism is
//! **QMP `cont` to run, a wall-time-bounded window, then QMP `stop` to pause**,
//! followed by the GPIO/UART exchange. This is the QMP analogue of Renode's
//! `RunFor`: it advances the guest a bounded amount and pauses.
//!
//! ## Why not `-icount` (measured, not assumed)
//!
//! The theoretically ideal primitive is `-icount shift=N`, which makes virtual
//! time a deterministic function of executed instructions and gives bit-exact
//! reproducibility (this is how the Renode-vs-QEMU note in docs/cosim/MCU.md framed
//! it). We TESTED it against the Espressif fork's `esp32` machine and it does not
//! work: with `-icount` at any shift (4/6/8/auto), with or without `sleep=off`,
//! the Xtensa esp32 machine produces ZERO UART output in a 15 s wall window,
//! versus ~1 s to the "hello from esp32" banner with no icount. icount on these
//! Xtensa machines is undocumented by Espressif and, empirically, breaks boot.
//! So icount is off, and determinism comes from the guest's own timers rather
//! than an instruction-counted clock.
//!
//! ## Determinism without icount
//!
//! Without icount the esp32 machine runs its virtual clock roughly at wall rate
//! (like Renode's `RunFor` blocking for the interval). Run-to-run timing is not
//! bit-exact, but the firmware's *logic behaviour* is reproducible because it is
//! driven by the guest's deterministic peripheral timers (the FreeRTOS tick, the
//! UART baud generator), which we sample only at chunk boundaries. The
//! integration test asserts this directly: the boot banner is identical and the
//! GPIO toggle count is stable (within a couple of chunks) across repeated runs.
//! That is the same standard a logic-analyser sampling a real board at the chunk
//! rate would meet.
//!
//! Alternatives considered and rejected:
//!   - **`-icount` + QMP stepping**: the ideal, but breaks esp32 boot (measured
//!     above). Rejected.
//!   - **qtest `clock_step`**: gives exact virtual-time advance and clean
//!     `readl`/`writel`, but qtest replaces the accelerator and gates all guest
//!     execution on test-driven clock steps; it cannot boot a real flash image
//!     through the normal TCG path. Rejected: it cannot boot the app the way a
//!     product does.
//!   - **gdbstub single-step budget**: exact, but stepping millions of
//!     instructions per chunk over RSP is far too slow. We DO use the gdbstub,
//!     but only for word-granular memory writes (GPIO input mailbox), never for
//!     stepping.
//!
//! # Coupling model (transplanted ODR-poll, via a RAM mailbox)
//!
//! Like Renode, the backend is poll-based for GPIO output: after each chunk it
//! reads an output word over QMP (`xp /1wx`), diffs against the previous
//! snapshot, and synthesises per-bit edge callbacks for the wired pins only.
//! GPIO input is push-based: it writes an input word over the gdbstub `M`
//! packet. UART is the serial socket.
//!
//! The one substantive difference from Renode: the Espressif QEMU `esp32.gpio`
//! model does NOT implement read-back of `GPIO_OUT_REG` (a host read returns 0
//! regardless of the driven level; writes to `GPIO_IN_REG` are dropped). RAM, in
//! contrast, round-trips exactly. So the output/input words are a fixed RAM
//! [`mailbox`] the demo firmware maintains (mirroring its GPIO output, reading
//! injected inputs), with the identical bit layout as `GPIO_OUT_REG`. The edge
//! synthesis is otherwise byte-for-byte the Renode ODR-poll. The real GPIO
//! `gpio_set_level` writes still happen in the firmware; the mailbox is only the
//! observation path the emulator's gpio model lacks. See the limitations section
//! in docs/cosim/MCU.md.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/qemu.md.

mod gdb;
pub mod install;
mod process;
mod qmp;
mod uart;

pub use process::{find_qemu, is_available, QemuArch};

use crate::traits::{I2cEvent, Mcu, McuState, PinId, SpiEvent};
use anyhow::{bail, ensure, Context, Result};
use gdb::GdbStub;
use process::QemuProcess;
use qmp::Qmp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use uart::UartSocket;

type I2cCb = Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>;
type SpiCb = Box<dyn FnMut(SpiEvent) -> u8 + Send>;

/// GPIO observation/injection mailbox in ESP32 RTC slow memory.
///
/// The Espressif QEMU `esp32.gpio` peripheral model does NOT implement read-back
/// of `GPIO_OUT_REG` (a host read of 0x3FF44004 over QMP `xp` or the gdbstub
/// returns 0 regardless of the firmware-driven level; writes to `GPIO_IN_REG`
/// are likewise dropped). This was verified empirically against the fork's
/// `esp32`/`esp32s3`/`esp32c3` machines. RAM, by contrast, reads and writes back
/// exactly over the control channel.
///
/// So the GPIO exchange goes through a fixed RAM mailbox the demo firmware
/// maintains: it mirrors its GPIO output word to `GPIO_OUT` after every change
/// and reads injected inputs from `GPIO_IN`. The bit layout is identical to
/// `GPIO_OUT_REG`, so the backend's edge-synthesis logic is unchanged: it polls
/// a word and diffs it, exactly the Renode ODR-poll pattern, only at a RAM
/// address instead of a peripheral register. `MAGIC` lets the backend confirm
/// the firmware is mailbox-aware before trusting the channel.
///
/// RTC slow memory (0x5000_0000, 8 KiB, uncached, fixed) is chosen because a
/// minimal app never touches it, the address is stable, and it survives across
/// the run. Firmware side: testdata/firmware/esp32_blinky/main/main.c.
pub mod mailbox {
    /// Base of the mailbox in RTC slow memory (Xtensa ESP32 / ESP32-S3).
    pub const BASE: u32 = 0x5000_0000;
    /// Firmware -> host: mirror of GPIO_OUT_REG.
    pub const GPIO_OUT: u32 = BASE;
    /// Host -> firmware: injected GPIO input word.
    pub const GPIO_IN: u32 = BASE + 0x04;
    /// Firmware -> host: 0x6A6C6E69 once the firmware has set up the mailbox.
    pub const MAGIC: u32 = BASE + 0x08;
    /// Magic tag value ("inlj" little-endian of 0x6A6C6E69).
    pub const MAGIC_VALUE: u32 = 0x6A6C_6E69;

    /// ESP32-C3 (RISC-V) RTC slow memory base differs from the Xtensa parts.
    pub const C3_BASE: u32 = 0x5000_0000;

    // ── Mailbox v2: the ADC + bus extension ─────────────────────────────────
    //
    // Espressif QEMU models neither the SAR ADC nor a host hook for I2C/SPI
    // byte traffic, so these functions ride the same RAM mailbox as GPIO.
    // Like the GPIO words, this is a FIRMWARE CONTRACT, not general firmware
    // support (05 §5.3): unmodified vendor firmware does not read these slots.
    // Each function is stated here precisely so a firmware author (or the
    // repo's demo firmware) can opt in; §5.3 retires each slot the day the
    // QEMU fork grows the corresponding peripheral hook.
    //
    // ADC (host → firmware, no handshake): `Mcu::set_analog_in(ch, volts)` is
    // converted to a count against `ADC_FULL_SCALE_VOLTS`/`ADC_MAX_COUNT` and
    // written into `adc_channel_word(ch)` each chunk, with bit `ch` of
    // ADC_MASK set. Firmware reads the count from the slot where it would
    // read the SAR ADC result register; the mask distinguishes "channel never
    // injected" from an honest zero count.
    //
    // I2C/SPI byte transfers (firmware ⇄ host, sequence handshake): the
    // firmware is the bus MASTER, so transfers originate guest-side. The
    // firmware describes one transaction-level request in the request cell
    // (op / addr / len / payload), then bumps REQ_SEQ (monotonic, nonzero)
    // and spin-waits for RSP_SEQ == REQ_SEQ (its timeout must exceed one
    // chunk). The backend services the cell once per `run_micros` chunk while
    // the guest is paused: it surfaces the bytes through the SAME
    // `on_i2c`/`on_spi` trait callbacks the simavr/Renode backends use (so
    // the engine's I2C slave models and SPI framing apply uniformly), writes
    // any reply bytes into RSP_DATA, and echoes the sequence into RSP_SEQ.
    // One transaction per bus per chunk: a register read (write-reg, read,
    // stop) costs three chunks. Coarse, and called out honestly as such.
    //
    // The whole v2 block is gated on the firmware writing BUS_MAGIC_VALUE
    // into BUS_MAGIC: without it the backend never touches the cells (and
    // with no callback registered it never even reads BUS_MAGIC), so the
    // pre-v2 behaviour is bit-identical.

    /// Firmware -> host: `BUS_MAGIC_VALUE` once the v2 bus cells are valid.
    pub const BUS_MAGIC: u32 = BASE + 0x0C;
    /// Magic tag for the v2 extension (MAGIC_VALUE + 1, "jnlj").
    pub const BUS_MAGIC_VALUE: u32 = 0x6A6C_6E6A;

    /// Host -> firmware: bitmask of ADC channels that carry an injected count.
    pub const ADC_MASK: u32 = BASE + 0x10;
    /// Host -> firmware: first ADC count word; channel N is at `+ 4*N`.
    pub const ADC_CH0: u32 = BASE + 0x14;
    /// Number of injectable ADC channel slots.
    pub const ADC_CHANNELS: u8 = 8;
    /// Count written at full scale (the ESP32 SAR ADC is a 12-bit converter).
    pub const ADC_MAX_COUNT: u32 = 4095;
    /// Voltage mapping to `ADC_MAX_COUNT` (11 dB attenuation full scale,
    /// approximated at the 3V3 rail the demo boards reference).
    pub const ADC_FULL_SCALE_VOLTS: f64 = 3.3;

    /// The mailbox word carrying channel `ch`'s injected count.
    pub const fn adc_channel_word(ch: u8) -> u32 {
        ADC_CH0 + 4 * ch as u32
    }

    /// Maximum payload/reply bytes per bus request cell.
    pub const BUS_DATA_MAX: u32 = 64;

    /// I2C request cell (firmware -> host) and response cell (host -> firmware).
    pub const I2C_REQ_SEQ: u32 = BASE + 0x40;
    pub const I2C_REQ_OP: u32 = BASE + 0x44;
    pub const I2C_REQ_ADDR: u32 = BASE + 0x48;
    pub const I2C_REQ_LEN: u32 = BASE + 0x4C;
    pub const I2C_REQ_DATA: u32 = BASE + 0x50;
    pub const I2C_RSP_SEQ: u32 = BASE + 0x90;
    pub const I2C_RSP_DATA: u32 = BASE + 0x94;

    /// I2C ops (mirroring the Renode bridge protocol op codes).
    pub const I2C_OP_WRITE: u32 = 1;
    pub const I2C_OP_READ: u32 = 2;
    pub const I2C_OP_STOP: u32 = 3;

    /// SPI request cell (firmware -> host) and response cell (host -> firmware).
    pub const SPI_REQ_SEQ: u32 = BASE + 0xE0;
    pub const SPI_REQ_OP: u32 = BASE + 0xE4;
    pub const SPI_REQ_LEN: u32 = BASE + 0xE8;
    pub const SPI_REQ_DATA: u32 = BASE + 0xEC;
    pub const SPI_RSP_SEQ: u32 = BASE + 0x12C;
    pub const SPI_RSP_DATA: u32 = BASE + 0x130;

    /// SPI ops: a byte transfer burst, or a chip-select deassert.
    pub const SPI_OP_TRANSFER: u32 = 1;
    pub const SPI_OP_DESELECT: u32 = 2;
}

/// One GPIO bank's observation addresses, generic over ESP32 family member.
///
/// The addresses are the RAM mailbox words (see [`mailbox`]), not the GPIO
/// peripheral registers, because the QEMU fork's gpio model does not expose
/// register read-back. The bit layout still matches GPIO_OUT_REG.
///
/// Plain-data register-offset carrier, so a new part declares its mailbox
/// addresses instead of adding branches to the backend; serde-derivable so it
/// is a W5 file-load target with no loader landing now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpioBank {
    /// Logical port letter the engine uses in [`PinId`]. The ESP32 GPIO matrix
    /// is a single flat 0..39 space; the demo uses the low bank (port `'0'`,
    /// bits 0..31).
    pub letter: char,
    /// Address of this bank's output-mirror word (firmware -> host).
    pub out_reg: u32,
    /// Address of this bank's input-injection word (host -> firmware).
    pub in_reg: u32,
    /// Number of valid bits in this bank.
    pub width: u8,
}

/// Per-part QEMU configuration: enough to boot a machine and bridge it.
///
/// Plain-data per-part surface, carrying every part difference as data rather
/// than as backend logic: the machine name, GPIO
/// banks with their mailbox offsets, icount/frequency, expected ISA, and I2C bus
/// paths are struct fields a constructor fills. `Serialize`/`Deserialize` make it
/// the file-load target for W5's data-driven MCU descriptor; no loader now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QemuConfig {
    /// Which QEMU system binary to use.
    pub arch: QemuArch,
    /// QEMU `-machine` name (e.g. `"esp32"`, `"esp32s3"`, `"esp32c3"`).
    pub machine: String,
    /// GPIO banks to bridge, in engine-facing order.
    pub banks: Vec<GpioBank>,
    /// icount shift: each guest instruction advances virtual time by `1<<shift`
    /// ns. 2 => 4 ns/instr (~250 MIPS of virtual time), a good match for the
    /// ESP32's 160-240 MHz so loop delays land near real durations.
    pub icount_shift: u8,
    /// Clock frequency in Hz reported by [`Mcu::frequency`] (advisory: QEMU
    /// models the SoC clocking; this is for the engine's bookkeeping).
    pub frequency_hz: u64,
    /// ELF `e_machine` this SoC's core executes: `EM_XTENSA` for ESP32 / S3,
    /// `EM_RISCV` for the ESP32-C3. Used to gate the firmware ELF (or its
    /// sibling app ELF beside the merged flash image) against the board's ISA,
    /// so an Xtensa image on a RISC-V board is refused rather than booted into
    /// 136 MB of UART garbage. See [`crate::elf`].
    pub expected_e_machine: u16,
    /// Human-readable MCU/board name for arch-mismatch error messages.
    pub mcu_label: String,
    /// QOM paths of the machine's I2C buses, searched for an emulated I2C device
    /// matching a modeled sensor's address so the engine can push readings into
    /// it (e.g. the ESP32 machine's built-in `tmp105` on `i2c0`). Empty disables
    /// the I2C-device bridge.
    pub i2c_buses: Vec<String>,
}

impl QemuConfig {
    // ── Built-in parts (06 §2) ──────────────────────────────────────────────
    //
    // Named accessors over the shipped `db/mcu/*.soc.toml` descriptors (embedded
    // via `include_str!`): the mailbox layout, arch, and clocking all live in
    // the TOML. A fresh part is addable purely as
    // data via [`crate::SocConfig::resolve`]. `.expect` is correct, a shipped
    // descriptor failing to load is a build bug caught by tests/soc_descriptors.rs.

    /// Classic ESP32 (Xtensa LX6). See `db/mcu/esp32.soc.toml`.
    pub fn esp32() -> Self {
        Self::from_soc_toml(include_str!("../../db/mcu/esp32.soc.toml"))
            .expect("built-in esp32.soc.toml is valid")
    }

    /// ESP32-S3 (Xtensa LX7). See `db/mcu/esp32s3.soc.toml`.
    pub fn esp32s3() -> Self {
        Self::from_soc_toml(include_str!("../../db/mcu/esp32s3.soc.toml"))
            .expect("built-in esp32s3.soc.toml is valid")
    }

    /// ESP32-C3 (RISC-V RV32IMC). See `db/mcu/esp32c3.soc.toml`.
    pub fn esp32c3() -> Self {
        Self::from_soc_toml(include_str!("../../db/mcu/esp32c3.soc.toml"))
            .expect("built-in esp32c3.soc.toml is valid")
    }
}

/// Validate the architecture of a QEMU flash image against the SoC's ISA.
///
/// QEMU boots from a merged `.bin` flash image, which is raw and carries no
/// ELF header. So:
///   1. If `flash_image` is itself an ELF (rare, but possible), check it.
///   2. Otherwise (raw `.bin`), look for the esp-idf-emitted app ELF in the same
///      directory; the sibling `*.elf` files, and check the arch against those.
///      This is the build layout in `testdata/firmware/esp32*/` (e.g. `flash.bin`
///      beside `esp32_blinky.elf`), and it is what makes the persona's
///      Xtensa-on-RISC-V mistake catchable even though the bin itself is opaque.
///   3. If neither yields an ELF, the image is left unchecked (no false error).
///
/// # Sibling resolution (conservative: zero false positives)
///
/// A firmware directory can legitimately hold sibling ELFs of *different* ISAs
/// (e.g. `testdata/firmware/esp32_blinky/` ships both the Xtensa
/// `esp32_blinky.elf` and the RISC-V `esp32c3_blinky.elf` next to the raw
/// `flash.bin`). We must not false-error just because *some* sibling disagrees
/// with the board. The rule is therefore "any sibling matches → pass":
///   - collect every sibling that parses as an ELF and read its `e_machine`;
///   - if ANY of them matches `expected`, the image is arch-consistent → `Ok`;
///   - only if there are sibling ELFs and NONE matches do we raise the clear
///     two-sided mismatch error (the genuine Xtensa-on-RISC-V case the gate was
///     built for, where the *only* sibling disagrees, is still caught);
///   - no parseable sibling ELF at all → unchecked (`Ok`, never a false error).
///
/// As a tie-break, when nothing matches we report the mismatch against a sibling
/// whose filename stem matches the `.bin`'s stem if one exists, else the first
/// mismatching sibling, so the message is the most relevant one.
///
/// Returns `Err` only on a genuine architecture mismatch.
fn validate_flash_image_arch(flash_image: &Path, expected: u16, mcu_label: &str) -> Result<()> {
    // Case 1: the image is itself an ELF.
    if let Some(found) = crate::elf::read_e_machine(flash_image)? {
        if found != expected {
            return crate::elf::validate_arch(flash_image, expected, mcu_label);
        }
        return Ok(());
    }

    // Case 2: raw .bin, probe sibling ELFs in the same directory.
    let Some(dir) = flash_image.parent() else {
        return Ok(());
    };
    let bin_stem = flash_image.file_stem().and_then(|s| s.to_str());

    // Collect every sibling that actually parses as an ELF, with its e_machine.
    let mut sibling_elfs: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            let is_elf = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("elf"))
                .unwrap_or(false);
            if !is_elf {
                continue;
            }
            // A `.elf` that does not actually parse as an ELF (None) is skipped;
            // a real I/O error is propagated.
            match crate::elf::read_e_machine(&p)? {
                Some(found) => {
                    if found == expected {
                        // Any matching sibling → the image is arch-consistent.
                        return Ok(());
                    }
                    sibling_elfs.push(p);
                }
                None => continue,
            }
        }
    }

    // No sibling ELF at all → unchecked (never a false error).
    if sibling_elfs.is_empty() {
        return Ok(());
    }

    // Sibling ELFs exist but NONE matched the board ISA → genuine mismatch.
    // Prefer the sibling whose filename stem relates to the .bin for the message;
    // otherwise the first mismatching sibling.
    let report = bin_stem
        .and_then(|bs| {
            sibling_elfs.iter().find(|p| {
                p.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|es| es == bs || es.contains(bs) || bs.contains(es))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(&sibling_elfs[0]);
    crate::elf::validate_arch(report, expected, mcu_label)
}

/// Allocate three distinct free TCP ports (QMP, gdbstub, UART), holding all
/// listeners until every number is read so the OS cannot reissue one to
/// another. QEMU binds each shortly after we release them.
fn free_port_triple() -> Result<(u16, u16, u16)> {
    let a = std::net::TcpListener::bind(("127.0.0.1", 0)).context("alloc QMP port")?;
    let b = std::net::TcpListener::bind(("127.0.0.1", 0)).context("alloc gdb port")?;
    let c = std::net::TcpListener::bind(("127.0.0.1", 0)).context("alloc uart port")?;
    let pa = a.local_addr()?.port();
    let pb = b.local_addr()?.port();
    let pc = c.local_addr()?.port();
    anyhow::ensure!(
        pa != pb && pb != pc && pa != pc,
        "port allocator returned a collision"
    );
    Ok((pa, pb, pc))
}

/// Wall-time floor applied to every run window while the guest is BOOTING.
///
/// The esp32 boot ROM + 2nd-stage bootloader take ~1-2 s of virtual time to
/// reach `app_main`. The engine's default co-sim chunk is 100 µs; honoring it
/// during boot would need ~15,000 cont/stop round-trips (plus per-chunk QMP
/// overhead) before the firmware even starts. Flooring boot chunks at 8 ms
/// clears boot in a few hundred chunks instead. Boot-ONLY: in steady state
/// this floor would over-advance a 100 µs chunk ~80x and desync the guest
/// from the analog solve and the lockstep peers (R8 #5).
const BOOT_WINDOW_FLOOR_S: f64 = 8e-3;

/// Wall-time cap on boot-phase run windows, so one chunk of a misconfigured
/// huge `dt` cannot stall the whole co-sim behind a single blocking sleep
/// while nothing observable is happening yet. Boot-only, like the floor:
/// post-boot a legitimately long requested chunk must run its full length,
/// or the guest falls BEHIND sim time (the mirror image of the floor bug).
const BOOT_WINDOW_CAP_S: f64 = 50e-3;

/// Wall-clock run window for a requested virtual interval of `seconds`.
///
/// The guest's virtual clock runs at roughly wall rate (no icount; it breaks
/// esp32 boot, measured; see the module notes), so the window IS the intended
/// virtual-time advance. Two regimes:
///
/// - **Booting** (`boot_complete == false`): clamp to
///   `[BOOT_WINDOW_FLOOR_S, BOOT_WINDOW_CAP_S]` so the ROM/bootloader makes
///   real progress per chunk without any one chunk blocking the co-sim.
/// - **Booted**: honor the request exactly. The lockstep contract is that
///   `run_micros(us)` advances ~`us` so all domains level at each chunk
///   boundary; any floor here would run the guest ahead of the analog solve
///   (pin edges, UART timing, everything time-correlated desyncs, R8 #5).
///
/// Pure function of its inputs so the regression tests can pin the mapping
/// without a QEMU process.
fn run_window(seconds: f64, boot_complete: bool) -> Duration {
    let s = if boot_complete {
        seconds.max(0.0)
    } else {
        seconds.clamp(BOOT_WINDOW_FLOOR_S, BOOT_WINDOW_CAP_S)
    };
    Duration::from_secs_f64(s)
}

/// Espressif-QEMU-backed [`Mcu`].
pub struct QemuBackend {
    config: QemuConfig,
    // Drop order: control channels and uart close before the process is killed.
    qmp: Qmp,
    /// gdbstub, used only for GPIO-input memory writes. Optional: if QEMU's
    /// gdbserver could not be attached, input injection is disabled but the rest
    /// of the backend (UART, GPIO output, stepping) still works.
    gdb: Option<GdbStub>,
    uart: UartSocket,
    _process: QemuProcess,

    /// Last-read OUT register per bank letter, for edge synthesis.
    last_out: HashMap<char, u32>,
    /// Current driven IN register per bank letter (what we poke for inputs).
    in_shadow: HashMap<char, u32>,
    /// If set, only these bank letters are polled each chunk.
    active_ports: Option<Vec<char>>,
    on_pin_change: Option<Box<dyn FnMut(PinId, bool, u64) + Send>>,
    on_uart: Option<Box<dyn FnMut(u8) + Send>>,
    firmware_loaded: bool,
    /// True once the firmware has raised the mailbox [`mailbox::MAGIC`] word,
    /// i.e. `app_main` is running. Gates the boot-only run-window floor (see
    /// [`run_window`]): before this, every chunk is floored so the ROM +
    /// 2nd-stage bootloader make real progress; after it, the requested chunk
    /// time is honored exactly so the guest stays leveled with the analog
    /// solve and the other MCUs (R8 #5). A firmware that never raises MAGIC
    /// (unmodified vendor firmware, not mailbox-aware) keeps the floor
    /// forever, identical to the pre-fix behaviour, and the only honest
    /// option: without the handshake there is no boot signal to gate on.
    boot_complete: bool,
    /// Virtual time advanced so far, in cycles-equivalent.
    cycles: u64,
    /// Resolved QOM path of the emulated I2C device at each modeled sensor
    /// address (`None` once searched and not found, so we do not re-walk QMP
    /// every frame). Populated lazily by [`Mcu::set_i2c_device_temperature`].
    i2c_temp_paths: HashMap<u8, Option<String>>,

    // ── Mailbox v2 (05 §5.1/§5.2) state ─────────────────────────────────────
    /// I2C byte-event handler, serviced from the mailbox I2C request cell.
    on_i2c: Option<I2cCb>,
    /// SPI byte-event handler, serviced from the mailbox SPI request cell.
    on_spi: Option<SpiCb>,
    /// True once BUS_MAGIC_VALUE has been observed; the bus cells are never
    /// read before the firmware declares them valid.
    bus_magic_seen: bool,
    /// Last serviced I2C / SPI request sequence numbers.
    i2c_serviced_seq: u32,
    spi_serviced_seq: u32,
    /// Open I2C transaction state for Start/Stop synthesis: `(addr, read)`.
    /// Mirrors the Renode bridge's `I2cBridgeState::ensure_mode` semantics.
    i2c_ring_active: Option<(u8, bool)>,
    /// Shadow of the ADC_MASK word (channels injected so far).
    adc_mask_shadow: u32,
    /// One-time warning flag for ADC injection without a gdbstub.
    adc_no_gdb_warned: bool,
    /// One-time warning flag for digital-input injection without a gdbstub.
    digital_no_gdb_warned: bool,
    /// One-time warning flag for a GPIO port that has no bank in this SoC
    /// descriptor (e.g. ESP32 GPIO>=32 maps to port '1', but the ESP32 SoC has a
    /// single 32-bit bank '0'). Such pins cannot be observed or injected, so the
    /// backend must SAY so rather than silently report the pin as never-driven.
    unbridged_gpio_warned: bool,
}

impl QemuBackend {
    /// Boot an Espressif QEMU machine from `config` running the merged flash
    /// image at `flash_image`, and connect the QMP, gdbstub, and UART channels.
    ///
    /// Unlike the Renode backend (which loads firmware via a Monitor command
    /// after the machine is up), QEMU boots from the flash image given on the
    /// command line, so the image path is needed at spawn time. The image is the
    /// merged 2nd-stage bootloader + partition table + app produced by
    /// `esptool merge_bin` (see testdata/firmware/esp32_blinky/build.sh).
    pub fn new(config: QemuConfig, flash_image: &Path) -> Result<Self> {
        if !flash_image.exists() {
            bail!(
                "ESP32 flash image not found: {}. Build it with esp-idf and \
                 esptool merge_bin (see testdata/firmware/esp32_blinky/build.sh).",
                flash_image.display()
            );
        }

        // Arch gate (BEFORE spawning QEMU): the persona-esp32-iot hunt loaded an
        // Xtensa image onto a RISC-V ESP32-C3 and got 136 MB of UART garbage with
        // no error. QEMU boots from the merged `.bin`, which is raw and carries no
        // ISA, so we cannot check the flash image directly, but the esp-idf build
        // emits the app ELF (which DOES carry e_machine) right beside it. If the
        // image itself is an ELF, check it; otherwise check a sibling app ELF if
        // one is present. A raw `.bin` with no sibling ELF is left unchecked
        // (we never false-error), but the common build layout (flash.bin next to
        // <name>.elf) is now caught.
        validate_flash_image_arch(flash_image, config.expected_e_machine, &config.mcu_label)?;

        let (qmp_port, gdb_port, uart_port) = free_port_triple()?;

        let mut process = QemuProcess::spawn(
            config.arch,
            &config.machine,
            flash_image,
            config.icount_shift,
            qmp_port,
            uart_port,
        )?;
        // Tell QEMU to also expose a gdbstub. We pass it as part of spawn? The
        // process builder set QMP/serial; the gdbstub is added via QMP after
        // connect using `human-monitor-command gdbserver`, which attaches a stub
        // without restarting. That keeps the spawn argument list stable.

        // Give QEMU a moment; if it died immediately the image/args were bad.
        std::thread::sleep(Duration::from_millis(150));
        if process.has_exited() {
            bail!(
                "Espressif QEMU exited immediately booting {}. The image or \
                 machine '{}' was rejected.",
                flash_image.display(),
                config.machine
            );
        }

        let mut qmp = Qmp::connect(("127.0.0.1", qmp_port), QemuProcess::startup_timeout())?;
        qmp.set_timeout(Duration::from_secs(20));
        // The guest is running at boot; pause it so the first chunk starts from a
        // known stopped state (the lockstep is cont -> window -> stop).
        let _ = qmp.stop();

        // Attach a gdbstub for GPIO-input memory writes. Best-effort: if it fails
        // we keep going with input injection disabled (the common demo path
        // drives no inputs). `gdbserver tcp::<port>` is the HMP form.
        let gdb = match qmp.hmp(&format!("gdbserver tcp::{gdb_port}")) {
            Ok(resp)
                if !resp.to_lowercase().contains("could not")
                    && !resp.to_lowercase().contains("error") =>
            {
                GdbStub::connect(gdb_port, Duration::from_secs(5)).ok()
            }
            _ => None,
        };

        let uart = UartSocket::connect(uart_port, Duration::from_secs(10))?;

        let last_out = config.banks.iter().map(|b| (b.letter, 0u32)).collect();
        let in_shadow = config.banks.iter().map(|b| (b.letter, 0u32)).collect();

        Ok(QemuBackend {
            config,
            qmp,
            gdb,
            uart,
            _process: process,
            last_out,
            in_shadow,
            active_ports: None,
            on_pin_change: None,
            on_uart: None,
            firmware_loaded: true, // booted from the image
            boot_complete: false,
            cycles: 0,
            i2c_temp_paths: HashMap::new(),
            on_i2c: None,
            on_spi: None,
            bus_magic_seen: false,
            i2c_serviced_seq: 0,
            spi_serviced_seq: 0,
            i2c_ring_active: None,
            adc_mask_shadow: 0,
            adc_no_gdb_warned: false,
            digital_no_gdb_warned: false,
            unbridged_gpio_warned: false,
        })
    }

    /// Convenience constructor for the classic ESP32.
    pub fn esp32(flash_image: &Path) -> Result<Self> {
        Self::new(QemuConfig::esp32(), flash_image)
    }

    /// Read one bank's output-mirror word from the RAM mailbox, preferring QMP
    /// `xp`, falling back to the gdbstub on a parse failure.
    fn read_out(&mut self, bank: &GpioBank) -> u32 {
        match self.qmp.read_u32(bank.out_reg) {
            Ok(v) => v,
            Err(_) => self
                .gdb
                .as_mut()
                .and_then(|g| g.read_u32(bank.out_reg).ok())
                .unwrap_or_else(|| *self.last_out.get(&bank.letter).unwrap_or(&0)),
        }
    }

    /// Poll the relevant banks' OUT registers, diff against the snapshot, fire
    /// per-bit edges for the wired pins.
    fn poll_gpio_edges(&mut self) {
        let banks: Vec<GpioBank> = match &self.active_ports {
            Some(active) => self
                .config
                .banks
                .iter()
                .filter(|b| active.contains(&b.letter))
                .cloned()
                .collect(),
            None => self.config.banks.clone(),
        };
        // Wall-clock-derived poll boundary time, in cycles-equivalent. QEMU has
        // no icount here, so this is coarse and every edge this poll shares it;
        // `cycle_exact()` is false (05 §1.1). Snapshot before the callback borrow.
        let cyc = self.cycles;
        for bank in &banks {
            let new = self.read_out(bank);
            let prev = *self.last_out.get(&bank.letter).unwrap_or(&0);
            if new != prev {
                let changed = new ^ prev;
                if let Some(cb) = &mut self.on_pin_change {
                    for bit in 0..bank.width {
                        if (changed >> bit) & 1 != 0 {
                            let high = (new >> bit) & 1 != 0;
                            cb(
                                PinId {
                                    port: bank.letter,
                                    bit,
                                },
                                high,
                                cyc,
                            );
                        }
                    }
                }
                self.last_out.insert(bank.letter, new);
            }
        }
    }

    /// Drain UART bytes the firmware emitted and dispatch them.
    fn pump_uart_out(&mut self) {
        let bytes = self.uart.drain();
        if let Some(cb) = &mut self.on_uart {
            for b in bytes {
                cb(b);
            }
        }
    }

    /// Advance the guest by ~`seconds` of virtual time, then exchange state.
    ///
    /// Mechanism (no icount; see the module lockstep notes): resume the guest
    /// (`cont`), hold a wall-time window during which the esp32 machine
    /// advances its virtual clock at roughly wall rate, pause (`stop`), then read
    /// the GPIO mailbox and drain UART. This mirrors Renode's `RunFor`, which
    /// also blocks for the interval, with the difference that the pace is
    /// wall-bounded rather than instruction-counted.
    ///
    /// The window is [`run_window`]-shaped: floored/capped only while BOOTING
    /// (until the firmware raises the mailbox MAGIC), then the requested
    /// interval exactly, so steady-state chunks stay leveled with the analog
    /// solve and the other MCUs (R8 #5).
    fn run_seconds(&mut self, seconds: f64) -> Result<()> {
        if !self.firmware_loaded {
            bail!("no firmware image booted in the QEMU machine");
        }
        self.qmp.cont().context("qmp cont")?;
        let window = run_window(seconds, self.boot_complete);
        std::thread::sleep(window);
        self.qmp.stop().context("qmp stop")?;

        // Credit cycles from the window we ACTUALLY ran, not the requested
        // `seconds`: during boot the floor/cap reshapes the real run, so
        // crediting `seconds` would systematically skew the guest cycle bracket
        // the chunk's GPIO-edge timestamps are measured against. Post-boot the
        // window IS the requested interval, so credit and request agree.
        // (Honest caveat: the guest also runs during the cont/stop QMP round
        // trips, so the true advance is `window` plus some control latency;
        // without icount that slack is unmeasurable and left uncredited.)
        self.cycles += (window.as_secs_f64() * self.config.frequency_hz as f64).round() as u64;

        // Boot-complete detection (one word read per chunk, only until seen):
        // the demo firmware writes MAGIC_VALUE into the mailbox as the first
        // thing app_main does, so its appearance is the earliest reliable
        // "past the ROM + 2nd-stage bootloader" signal the backend can see.
        // A failed read leaves the flag down (floor stays; the safe side).
        if !self.boot_complete {
            if let Ok(magic) = self.read_guest_u32(mailbox::MAGIC) {
                if magic == mailbox::MAGIC_VALUE {
                    self.boot_complete = true;
                }
            }
        }

        self.poll_gpio_edges();
        // Service the mailbox bus cells while the guest is paused (05 §5.2),
        // so a firmware spin-waiting on RSP_SEQ proceeds next chunk.
        self.service_bus_mailbox()?;
        self.pump_uart_out();
        Ok(())
    }

    /// Read one guest word over the control channels: QMP `xp` first, gdbstub
    /// fallback, erroring only when both paths fail (the mailbox protocol must
    /// not mistake a dead control channel for a zero word).
    fn read_guest_u32(&mut self, addr: u32) -> Result<u32> {
        match self.qmp.read_u32(addr) {
            Ok(v) => Ok(v),
            Err(qmp_err) => match self.gdb.as_mut() {
                Some(g) => g.read_u32(addr),
                None => Err(qmp_err).context("QMP word read failed and no gdbstub fallback"),
            },
        }
    }

    /// Read `len` packed bytes from the mailbox at `addr`.
    fn read_guest_bytes(&mut self, addr: u32, len: usize) -> Result<Vec<u8>> {
        // Prefer the gdbstub's native byte read; fall back to word-granular QMP.
        if let Some(g) = self.gdb.as_mut() {
            if let Ok(b) = g.read_mem(addr, len) {
                if b.len() >= len {
                    return Ok(b[..len].to_vec());
                }
            }
        }
        let mut out = Vec::with_capacity(len);
        let mut a = addr;
        while out.len() < len {
            let w = self.qmp.read_u32(a)?;
            out.extend_from_slice(&w.to_le_bytes());
            a += 4;
        }
        out.truncate(len);
        Ok(out)
    }

    /// Write packed bytes into the mailbox (gdbstub `M` packet). The bus
    /// mailbox cannot answer without a gdbstub; that is a loud error, not a
    /// silently dropped reply the firmware would read as bus data.
    fn write_guest_bytes(&mut self, addr: u32, bytes: &[u8]) -> Result<()> {
        let g = self
            .gdb
            .as_mut()
            .context("the QEMU bus mailbox needs the gdbstub for reply writes")?;
        g.write_mem(addr, bytes)
    }

    fn write_guest_u32(&mut self, addr: u32, val: u32) -> Result<()> {
        self.write_guest_bytes(addr, &val.to_le_bytes())
    }

    /// Service the mailbox v2 bus cells once per chunk (05 §5.2), while the
    /// guest is paused. Zero-cost when no callback is registered; one word
    /// read per chunk until the firmware raises BUS_MAGIC.
    fn service_bus_mailbox(&mut self) -> Result<()> {
        if self.on_i2c.is_none() && self.on_spi.is_none() {
            return Ok(());
        }
        if !self.bus_magic_seen {
            // A pre-v2 firmware never raises the magic, un-primed RAM reads
            // as an honest 0 through a WORKING channel. A read that FAILS is a
            // control-channel fault and must surface, not masquerade as "magic
            // not raised" (a v2 firmware would spin on RSP_SEQ forever).
            let magic = self
                .read_guest_u32(mailbox::BUS_MAGIC)
                .context("reading the QEMU bus-mailbox magic word")?;
            if magic != mailbox::BUS_MAGIC_VALUE {
                return Ok(());
            }
            self.bus_magic_seen = true;
        }
        if self.on_i2c.is_some() {
            self.service_i2c_cell()
                .context("servicing the QEMU I2C mailbox cell")?;
        }
        if self.on_spi.is_some() {
            self.service_spi_cell()
                .context("servicing the QEMU SPI mailbox cell")?;
        }
        Ok(())
    }

    /// Service one pending I2C transaction request, surfacing it as the same
    /// Start/Write/Read/Stop [`I2cEvent`]s the simavr TWI decode and the
    /// Renode bridge produce (the state machine mirrors the Renode bridge's
    /// `I2cBridgeState::ensure_mode`).
    fn service_i2c_cell(&mut self) -> Result<()> {
        let seq = self.read_guest_u32(mailbox::I2C_REQ_SEQ)?;
        if seq == 0 || seq == self.i2c_serviced_seq {
            return Ok(());
        }
        let op = self.read_guest_u32(mailbox::I2C_REQ_OP)?;
        let addr = self.read_guest_u32(mailbox::I2C_REQ_ADDR)? as u8;
        let len = self.read_guest_u32(mailbox::I2C_REQ_LEN)?;
        ensure!(
            len <= mailbox::BUS_DATA_MAX,
            "QEMU I2C mailbox request too large: {len} bytes (max {})",
            mailbox::BUS_DATA_MAX
        );
        let len = len as usize;
        let payload = if op == mailbox::I2C_OP_WRITE && len != 0 {
            self.read_guest_bytes(mailbox::I2C_REQ_DATA, len)?
        } else {
            Vec::new()
        };

        // Dispatch with the callback taken out of `self` so the borrow does
        // not pin the control channels.
        let mut cb = self.on_i2c.take().expect("checked by caller");
        let mut active = self.i2c_ring_active.take();
        // Mirrors the Renode bridge's `I2cBridgeState::ensure_mode` exactly:
        // switching TO write stops any open transaction; switching to read on
        // the SAME address is a repeated START (no Stop, a register-read
        // slave must not see its transaction boundary mid-read); a read on a
        // DIFFERENT address stops the old transaction first.
        let ensure_mode = |active: &mut Option<(u8, bool)>, cb: &mut I2cCb, a: u8, read: bool| {
            if *active != Some((a, read)) {
                if !read {
                    if let Some((prev, _)) = active.take() {
                        let _ = cb(I2cEvent::Stop { addr: prev });
                    }
                } else if let Some((prev, _)) = *active {
                    if prev != a {
                        let _ = cb(I2cEvent::Stop { addr: prev });
                        *active = None;
                    }
                }
                let _ = cb(I2cEvent::Start { addr: a, read });
                *active = Some((a, read));
            }
        };
        let mut reply: Vec<u8> = Vec::new();
        let dispatch = (|| -> Result<()> {
            match op {
                mailbox::I2C_OP_WRITE => {
                    ensure_mode(&mut active, &mut cb, addr, false);
                    for data in payload {
                        let _ = cb(I2cEvent::Write { addr, data });
                    }
                }
                mailbox::I2C_OP_READ => {
                    ensure_mode(&mut active, &mut cb, addr, true);
                    for _ in 0..len {
                        // `None` is the model layer's "no slave / NACK"; 0xFF
                        // is the level an open-drain bus floats to (the same
                        // convention as the Renode bridge).
                        reply.push(cb(I2cEvent::Read { addr }).unwrap_or(0xFF));
                    }
                }
                mailbox::I2C_OP_STOP => {
                    if let Some((prev, _)) = active.take() {
                        let _ = cb(I2cEvent::Stop { addr: prev });
                    }
                }
                other => bail!("QEMU I2C mailbox: unknown op {other}"),
            }
            Ok(())
        })();
        self.on_i2c = Some(cb);
        self.i2c_ring_active = active;
        // The request is SERVICED once dispatched, acknowledged or not: mark
        // it before the response writes so a caller that retries after a
        // failed write cannot replay the byte events into a stateful slave
        // model. A failed write still errors out loudly below (the firmware's
        // spin-wait then times out rather than reading a half-written reply).
        self.i2c_serviced_seq = seq;
        dispatch?;

        if !reply.is_empty() {
            self.write_guest_bytes(mailbox::I2C_RSP_DATA, &reply)?;
        }
        self.write_guest_u32(mailbox::I2C_RSP_SEQ, seq)?;
        Ok(())
    }

    /// Service one pending SPI request: a byte-transfer burst (one
    /// [`SpiEvent`] per byte, MISO bytes returned in the response cell) or a
    /// chip-select deassert.
    fn service_spi_cell(&mut self) -> Result<()> {
        let seq = self.read_guest_u32(mailbox::SPI_REQ_SEQ)?;
        if seq == 0 || seq == self.spi_serviced_seq {
            return Ok(());
        }
        let op = self.read_guest_u32(mailbox::SPI_REQ_OP)?;
        let len = self.read_guest_u32(mailbox::SPI_REQ_LEN)?;
        ensure!(
            len <= mailbox::BUS_DATA_MAX,
            "QEMU SPI mailbox request too large: {len} bytes (max {})",
            mailbox::BUS_DATA_MAX
        );
        let len = len as usize;
        let mosi = if op == mailbox::SPI_OP_TRANSFER && len != 0 {
            self.read_guest_bytes(mailbox::SPI_REQ_DATA, len)?
        } else {
            Vec::new()
        };

        // Coarse poll-boundary stamp, the same tier the pin edges carry
        // (`cycle_exact()` is false on this backend).
        let cyc = self.cycles;
        let mut cb = self.on_spi.take().expect("checked by caller");
        let mut miso: Vec<u8> = Vec::new();
        let dispatch = (|| -> Result<()> {
            match op {
                mailbox::SPI_OP_TRANSFER => {
                    for b in mosi {
                        miso.push(cb(SpiEvent {
                            mosi: b,
                            deselect: false,
                            cycle: cyc,
                        }));
                    }
                }
                mailbox::SPI_OP_DESELECT => {
                    let _ = cb(SpiEvent {
                        mosi: 0,
                        deselect: true,
                        cycle: cyc,
                    });
                }
                other => bail!("QEMU SPI mailbox: unknown op {other}"),
            }
            Ok(())
        })();
        self.on_spi = Some(cb);
        // Serviced once dispatched (see the I2C twin): never replay byte
        // events into a stateful slave model on a retry after a failed write.
        self.spi_serviced_seq = seq;
        dispatch?;

        if !miso.is_empty() {
            self.write_guest_bytes(mailbox::SPI_RSP_DATA, &miso)?;
        }
        self.write_guest_u32(mailbox::SPI_RSP_SEQ, seq)?;
        Ok(())
    }

    /// Read a guest physical word over the control channels (QMP `xp`, gdbstub
    /// fallback).
    ///
    /// Diagnostic / test-support accessor: the mailbox integration tests use
    /// it to play the firmware's half of the RAM-mailbox contract (no
    /// mailbox-v2-aware firmware image ships in the repo yet, and this machine
    /// carries no Xtensa cross-toolchain to build one).
    pub fn debug_read_u32(&mut self, addr: u32) -> Result<u32> {
        self.read_guest_u32(addr)
    }

    /// Whether the firmware has raised the mailbox MAGIC handshake, i.e. the
    /// backend has switched from boot-floored run windows to exact ones.
    /// Diagnostic / test-support accessor.
    pub fn boot_complete(&self) -> bool {
        self.boot_complete
    }

    /// Write a guest physical word (gdbstub `M` packet). See [`Self::debug_read_u32`].
    pub fn debug_write_u32(&mut self, addr: u32, val: u32) -> Result<()> {
        self.write_guest_u32(addr, val)
    }

    /// Write guest physical bytes (gdbstub `M` packet). See [`Self::debug_read_u32`].
    pub fn debug_write_bytes(&mut self, addr: u32, bytes: &[u8]) -> Result<()> {
        self.write_guest_bytes(addr, bytes)
    }

    /// Walk the configured I2C buses for an emulated device whose `address`
    /// property matches `addr`, returning its QOM path. One-time discovery
    /// (cached by the caller). Children without an `address` property, or buses
    /// the machine does not expose, simply yield QMP errors that are skipped.
    fn resolve_i2c_device(&mut self, addr: u8) -> Option<String> {
        let buses = self.config.i2c_buses.clone();
        for bus in &buses {
            for i in 0..8u32 {
                let child = format!("{bus}/child[{i}]");
                if let Ok(v) = self.qmp.qom_get(&child, "address") {
                    if parse_leading_int(&v) == Some(i64::from(addr)) {
                        return Some(child);
                    }
                }
            }
        }
        None
    }
}

/// Parse the first signed integer out of a QMP `return` scalar string (e.g.
/// `"72"` for an `address` property), tolerating surrounding quotes/whitespace.
fn parse_leading_int(s: &str) -> Option<i64> {
    let t = s.trim().trim_matches('"').trim();
    if let Ok(v) = t.parse::<i64>() {
        return Some(v);
    }
    let digits: String = t
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '-')
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

impl Mcu for QemuBackend {
    fn load_firmware(&mut self, _path: &Path) -> Result<()> {
        // QEMU boots from the flash image given at spawn; there is no separate
        // load step. The image was validated in `new`. This is a no-op so the
        // scheduler's "load firmware after instantiate" flow stays uniform.
        Ok(())
    }

    fn run_cycles(&mut self, n: u64) -> Result<u64> {
        let seconds = n as f64 / self.config.frequency_hz as f64;
        self.run_seconds(seconds)?;
        Ok(n)
    }

    fn run_micros(&mut self, us: u64) -> Result<()> {
        self.run_seconds(us as f64 / 1_000_000.0)
    }

    fn frequency(&self) -> u64 {
        self.config.frequency_hz
    }

    fn set_digital_in(&mut self, pin: PinId, high: bool) {
        // Drive the firmware-visible input by poking GPIO_IN_REG. Maintain a
        // shadow so we set/clear exactly the addressed bit.
        if let Some(bank) = self
            .config
            .banks
            .iter()
            .find(|b| b.letter == pin.port)
            .cloned()
        {
            let cur = *self.in_shadow.get(&bank.letter).unwrap_or(&0);
            let next = if high {
                cur | (1 << pin.bit)
            } else {
                cur & !(1 << pin.bit)
            };
            if next != cur {
                let Some(g) = self.gdb.as_mut() else {
                    if !self.digital_no_gdb_warned {
                        self.digital_no_gdb_warned = true;
                        eprintln!(
                            "qemu: DROPPING digital-input injection: no gdbstub attached, \
                             so guest memory cannot be written (matching set_analog_in)"
                        );
                    }
                    return;
                };
                match g.write_u32(bank.in_reg, next) {
                    Ok(()) => {
                        self.in_shadow.insert(bank.letter, next);
                    }
                    // Loud-drop discipline: a failed guest write means the
                    // injection never landed. Silently swallowing the Err left
                    // the shadow un-updated AND printed nothing, so the drop was
                    // invisible, matching set_analog_in's "ADC injection write
                    // failed" and the no-gdbstub branch above.
                    Err(e) => eprintln!(
                        "qemu: digital-input injection write failed \
                         (port {}, reg {:#x}): {e:#}",
                        bank.letter, bank.in_reg
                    ),
                }
            }
        } else if !self.unbridged_gpio_warned {
            // The addressed port has no bank in this SoC; the injection cannot
            // land and the firmware read of this pin will never see the board
            // level. Say so once rather than returning silently (ESP32 GPIO>=32
            // maps to port '1', which the single-bank ESP32 SoC does not expose).
            self.unbridged_gpio_warned = true;
            eprintln!(
                "qemu: DROPPING digital-input injection for port '{}' bit {}: \
                 no such bank in the '{}' SoC descriptor; the firmware read of \
                 this pin will see nothing (e.g. ESP32 GPIO>=32).",
                pin.port, pin.bit, self.config.machine
            );
        }
    }

    fn set_analog_in(&mut self, channel: u8, volts: f64) {
        // The ESP32 SAR ADC is not modelled by the QEMU fork's peripheral set,
        // so injection rides the RAM mailbox (05 §5.1): the modeled voltage is
        // converted to a 12-bit count and written into the channel's mailbox
        // slot each chunk, with the channel's ADC_MASK bit set. Firmware reads
        // the count from the slot rather than a real peripheral, a FIRMWARE
        // CONTRACT, stated as such in [`mailbox`], retired per function the
        // day the fork models the peripheral (05 §5.3).
        if channel >= mailbox::ADC_CHANNELS {
            eprintln!(
                "qemu: DROPPING ADC injection for channel {channel}: the mailbox \
                 carries {} slots",
                mailbox::ADC_CHANNELS
            );
            return;
        }
        let count = adc_count(volts, mailbox::ADC_FULL_SCALE_VOLTS, mailbox::ADC_MAX_COUNT);
        let Some(g) = self.gdb.as_mut() else {
            if !self.adc_no_gdb_warned {
                self.adc_no_gdb_warned = true;
                eprintln!(
                    "qemu: DROPPING ADC injection: no gdbstub attached, so guest \
                     memory cannot be written (matching set_digital_in)"
                );
            }
            return;
        };
        let mask = self.adc_mask_shadow | (1 << channel);
        let write = g
            .write_u32(mailbox::adc_channel_word(channel), count)
            .and_then(|()| g.write_u32(mailbox::ADC_MASK, mask));
        match write {
            Ok(()) => self.adc_mask_shadow = mask,
            // LOUD drop: a failed injection must not read as a valid count.
            Err(e) => eprintln!("qemu: ADC injection write failed: {e:#}"),
        }
    }

    fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool, u64) + Send>) {
        self.on_pin_change = Some(cb);
    }

    fn current_cycle(&self) -> u64 {
        self.cycles
    }

    fn cycle_exact(&self) -> bool {
        // Wall-clock-derived virtual time, no icount: GPIO is observed by diffing
        // a RAM-mailbox output word per chunk, so edge ordering is coarse (05 §1.1).
        false
    }

    fn uart_write(&mut self, bytes: &[u8]) {
        let _ = self.uart.write_bytes(bytes);
    }

    fn on_uart(&mut self, cb: Box<dyn FnMut(u8) + Send>) {
        self.on_uart = Some(cb);
    }

    fn on_i2c(&mut self, cb: Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>) {
        // The Espressif QEMU controller exposes no host-byte hook for its RX
        // FIFO, so byte events cannot be intercepted from the emulated I2C
        // controller the way simavr/Renode do. Two paths coexist instead:
        //
        //   1. This callback services the RAM-mailbox I2C cell (05 §5.2): a
        //      firmware participating in the mailbox v2 contract gets its
        //      transactions surfaced as the same Start/Write/Read/Stop events,
        //      so the engine's I2C slave models answer uniformly across
        //      backends. Coarse (one transaction per chunk) and gated on
        //      BUS_MAGIC; a non-participating firmware costs nothing.
        //   2. Temperature sensors are ALSO pushed into QEMU's own emulated
        //      I2C device (the machine ships a tmp105 at 0x48 on i2c0) via
        //      `set_i2c_device_temperature`, which unmodified vendor firmware
        //      reads through its real I2C controller. Where such a device
        //      exists it is preferred (05 §5.3: retire the mailbox function
        //      when a real peripheral emulation exists).
        self.on_i2c = Some(cb);
    }

    fn set_i2c_device_temperature(&mut self, addr: u8, milli_c: i32) {
        // Resolve (once, then cache) the QOM path of the emulated I2C device at
        // this address, then set its `temperature`. The ESP32 machine's built-in
        // tmp105 takes milli-degrees C; the firmware reads it over its own I2C0.
        if !self.i2c_temp_paths.contains_key(&addr) {
            let resolved = self.resolve_i2c_device(addr);
            self.i2c_temp_paths.insert(addr, resolved);
        }
        if let Some(Some(path)) = self.i2c_temp_paths.get(&addr).cloned() {
            let _ = self.qmp.qom_set(&path, "temperature", &milli_c.to_string());
        }
    }

    fn on_spi(&mut self, cb: Box<dyn FnMut(SpiEvent) -> u8 + Send>) {
        // Espressif QEMU exposes no host hook for GPSPI transfers, so SPI byte
        // events ride the RAM-mailbox SPI cell (05 §5.2): a firmware
        // participating in the mailbox v2 contract submits transaction-level
        // bursts and each byte is surfaced through this callback (MISO bytes
        // return via the response cell). Gated on BUS_MAGIC; unmodified vendor
        // firmware driving the real (unmodeled) SPI controller is untouched,
        // and, honestly, unserved until the fork grows a peripheral hook.
        self.on_spi = Some(cb);
    }

    fn state(&self) -> McuState {
        McuState {
            pc: 0,
            cycles: self.cycles,
            sleeping: false,
            // QEMU's QMP poll path carries no terminal-CPU signal here;
            // conservatively report "still running" rather than guessing.
            done: false,
            crashed: false,
        }
    }

    fn set_active_ports(&mut self, ports: &[char]) {
        let known: Vec<char> = self
            .config
            .banks
            .iter()
            .filter(|b| ports.contains(&b.letter))
            .map(|b| b.letter)
            .collect();
        // Loud-drop: any requested port with no matching bank cannot be observed
        // or injected (poll_gpio_edges / set_digital_in both key on config.banks),
        // so a firmware drive/read of its pins is invisible and the net would be
        // silently reported as never-driven. Say so once instead of failing quiet.
        // (ESP32/-S3 GPIO>=32 maps to port '1', but the ESP32 SoC has one 32-bit
        // bank '0'; those high pins need a second OUT1 mailbox bank to co-sim.)
        if !self.unbridged_gpio_warned {
            let unbridged: Vec<char> = ports
                .iter()
                .copied()
                .filter(|p| !self.config.banks.iter().any(|b| b.letter == *p))
                .collect();
            if !unbridged.is_empty() {
                self.unbridged_gpio_warned = true;
                eprintln!(
                    "qemu: GPIO port(s) {:?} have no bank in the '{}' SoC descriptor \
                     (banks present: {:?}); pins on them are NOT co-simulated; firmware \
                     drives/reads of those pins are invisible (e.g. ESP32 GPIO>=32).",
                    unbridged, self.config.machine, known
                );
            }
        }
        self.active_ports = Some(known);
    }
}

/// Quantize a voltage to an n-bit ADC code. An n-bit converter's transfer
/// function is round(frac * 2^n) saturated at 2^n - 1: multiply by
/// (`max_count` + 1) then clamp to `max_count`. Multiplying by `max_count`
/// itself (2^n - 1) systematically under-reads sub-full-scale voltages by up to
/// ~1 LSB and only reaches the top code at exactly full scale. Kept identical to
/// renode's `adc_count` and the engine SPI ADC path so every backend quantizes
/// a given voltage to the same code.
fn adc_count(volts: f64, full_scale_volts: f64, max_count: u32) -> u32 {
    if !(full_scale_volts > 0.0) {
        return 0;
    }
    let frac = (volts / full_scale_volts).clamp(0.0, 1.0);
    ((frac * (f64::from(max_count) + 1.0)).round() as u32).min(max_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R14: the QEMU ADC injection must use the 2^n transfer function (like
    /// renode and the SPI path), not 2^n-1 which under-reads by up to ~1 LSB.
    #[test]
    fn adc_count_uses_2n_scaling() {
        let max = mailbox::ADC_MAX_COUNT; // 4095 (12-bit)
        let fs = mailbox::ADC_FULL_SCALE_VOLTS;
        assert_eq!(adc_count(0.0, fs, max), 0);
        assert_eq!(adc_count(fs, fs, max), max, "full scale reads the top code");
        assert_eq!(
            adc_count(2.0 * fs, fs, max),
            max,
            "over-range clamps to the top code"
        );
        // Near-full-scale: 2^n scaling rounds up to the top code where a
        // 2^n-1 scaling (round(0.99976*4095)) sticks one code low at 4094.
        let near_full = fs * (f64::from(max) - 0.5) / f64::from(max);
        assert_eq!(
            adc_count(near_full, fs, max),
            max,
            "the top LSB band reaches 4095"
        );
        // A guard against a zero/negative reference.
        assert_eq!(adc_count(1.0, 0.0, max), 0);
    }

    #[test]
    fn esp32_config_shape() {
        let c = QemuConfig::esp32();
        assert_eq!(c.machine, "esp32");
        assert_eq!(c.arch, QemuArch::Xtensa);
        assert_eq!(c.banks.len(), 1);
        // GPIO is observed through the RAM mailbox (the gpio model has no
        // register read-back).
        assert_eq!(c.banks[0].out_reg, mailbox::GPIO_OUT);
        assert_eq!(c.banks[0].in_reg, mailbox::GPIO_IN);
    }

    #[test]
    fn esp32c3_uses_riscv() {
        let c = QemuConfig::esp32c3();
        assert_eq!(c.arch, QemuArch::Riscv32);
        assert_eq!(c.machine, "esp32c3");
        assert_eq!(c.banks[0].out_reg, mailbox::GPIO_OUT);
    }

    /// Bit-identity proof for the data-driven config bridge (05 §5.5): every
    /// stock QEMU config round-trips through serde equal to the constructor's
    /// output, so it is a lossless plain-data carrier for W5's future file load,
    /// and the refactor (inert `#[derive]`s only) left the values unchanged.
    #[test]
    fn config_bridge_roundtrips_bit_identically() {
        for c in [
            QemuConfig::esp32(),
            QemuConfig::esp32s3(),
            QemuConfig::esp32c3(),
        ] {
            let json = serde_json::to_string(&c).expect("serialize QemuConfig");
            let back: QemuConfig = serde_json::from_str(&json).expect("deserialize QemuConfig");
            assert_eq!(c, back, "QemuConfig must round-trip bit-identically");
        }
    }

    #[test]
    fn mailbox_layout() {
        assert_eq!(mailbox::GPIO_OUT, 0x5000_0000);
        assert_eq!(mailbox::GPIO_IN, 0x5000_0004);
        assert_eq!(mailbox::MAGIC, 0x5000_0008);
        assert_eq!(mailbox::MAGIC_VALUE, 0x6A6C_6E69);
    }

    #[test]
    fn mailbox_v2_layout_does_not_overlap() {
        use mailbox::*;
        // v2 marker sits directly after the v1 words.
        assert_eq!(BUS_MAGIC, 0x5000_000C);
        assert_ne!(BUS_MAGIC_VALUE, MAGIC_VALUE);
        // ADC block: mask + 8 channel words, ending before the I2C cell.
        assert_eq!(ADC_MASK, 0x5000_0010);
        assert_eq!(adc_channel_word(0), 0x5000_0014);
        assert_eq!(adc_channel_word(7), 0x5000_0030);
        assert!(adc_channel_word(ADC_CHANNELS - 1) + 4 <= I2C_REQ_SEQ);
        // I2C cell: request data ends exactly at the response seq; response
        // data ends before the SPI cell.
        assert_eq!(I2C_REQ_DATA + BUS_DATA_MAX, I2C_RSP_SEQ);
        assert!(I2C_RSP_DATA + BUS_DATA_MAX <= SPI_REQ_SEQ);
        // SPI cell: request data ends exactly at the response seq; the whole
        // mailbox stays comfortably inside the 8 KiB RTC slow memory.
        assert_eq!(SPI_REQ_DATA + BUS_DATA_MAX, SPI_RSP_SEQ);
        assert!(SPI_RSP_DATA + BUS_DATA_MAX <= BASE + 0x2000);
    }

    // ── run_window: the boot-only floor/cap (R8 #5) ──────────────────────────

    /// THE BUG (R8 #5): the 8 ms floor was unconditional, so a steady-state
    /// scheduler chunk of 100 µs advanced the guest ~8 ms, an ~80x
    /// over-advance that desynced the QEMU MCU from the analog solve and the
    /// lockstep peers. Post-boot, the requested interval must be honored
    /// exactly.
    #[test]
    fn run_window_post_boot_honors_the_requested_chunk() {
        assert_eq!(
            run_window(100e-6, true),
            Duration::from_secs_f64(100e-6),
            "a booted guest must advance the requested 100 µs, not the boot floor"
        );
        // A 50 ms cap would also truncate legitimately long post-boot chunks.
        assert_eq!(
            run_window(0.2, true),
            Duration::from_secs_f64(0.2),
            "a long post-boot chunk must run its full length (no cap)"
        );
    }

    /// The floor exists so the ROM + 2nd-stage bootloader clear boot in a few
    /// hundred chunks instead of ~15,000; it must survive for the boot phase.
    #[test]
    fn run_window_boot_phase_keeps_floor_and_cap() {
        assert_eq!(
            run_window(100e-6, false),
            Duration::from_secs_f64(BOOT_WINDOW_FLOOR_S),
            "boot chunks are floored so the bootloader makes real progress"
        );
        assert_eq!(
            run_window(0.2, false),
            Duration::from_secs_f64(BOOT_WINDOW_CAP_S),
            "boot chunks are capped so one chunk cannot stall the co-sim"
        );
        // In-range boot request passes through unchanged.
        assert_eq!(run_window(20e-3, false), Duration::from_secs_f64(20e-3));
    }

    /// Degenerate inputs stay sane: a zero/negative request never panics and
    /// never sleeps (post-boot) / still gets the boot floor (booting).
    #[test]
    fn run_window_degenerate_inputs() {
        assert_eq!(run_window(0.0, true), Duration::ZERO);
        assert_eq!(run_window(-1.0, true), Duration::ZERO);
        assert_eq!(
            run_window(0.0, false),
            Duration::from_secs_f64(BOOT_WINDOW_FLOOR_S)
        );
    }

    #[test]
    fn ports_triple_distinct() {
        let (a, b, c) = free_port_triple().unwrap();
        assert!(a != b && b != c && a != c);
    }

    // ── validate_flash_image_arch: sibling-ELF resolution ────────────────────

    /// Minimal fake ELF carrying `e_machine` (little-endian) at the given path.
    /// `e_machine` lives at file offset 0x12 for both ELF32 and ELF64.
    fn write_fake_elf(path: &Path, e_machine: u16) {
        use std::io::Write;
        const E_MACHINE_OFFSET: usize = 0x12;
        let mut buf = vec![0u8; E_MACHINE_OFFSET + 2];
        buf[..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
        buf[5] = 1; // EI_DATA little-endian
        buf[E_MACHINE_OFFSET..E_MACHINE_OFFSET + 2].copy_from_slice(&e_machine.to_le_bytes());
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&buf).unwrap();
    }

    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hauksbee-archgate-unit-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// THE BUG: a raw `.bin` (Xtensa flash image) sits in a directory that holds
    /// TWO sibling ELFs of DIFFERENT arch; the matching Xtensa `esp32_blinky.elf`
    /// AND the non-matching RISC-V `esp32c3_blinky.elf`. The gate must PASS for an
    /// Xtensa board because a correct-arch sibling exists; it must NOT false-error
    /// just because the RISC-V sibling disagrees.
    #[test]
    fn raw_bin_with_one_matching_and_one_mismatching_sibling_passes() {
        let dir = unique_dir("mixed");
        let bin = dir.join("flash.bin");
        std::fs::write(&bin, [0xff, 0xff, 0xff, 0xff, 0x00, 0x01]).unwrap();
        write_fake_elf(&dir.join("esp32_blinky.elf"), crate::elf::EM_XTENSA);
        write_fake_elf(&dir.join("esp32c3_blinky.elf"), crate::elf::EM_RISCV);

        let res = validate_flash_image_arch(&bin, crate::elf::EM_XTENSA, "ESP32 (Xtensa LX6)");
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            res.is_ok(),
            "an Xtensa flash.bin with a matching Xtensa sibling must PASS even \
             though a RISC-V sibling is also present: {res:?}"
        );
    }

    /// Regression guard for the genuine mismatch: a raw `.bin` whose ONLY sibling
    /// ELFs are ALL the wrong arch (here two distinct wrong-arch siblings) must
    /// still be REFUSED with the clear two-sided message. The fix must not let an
    /// all-wrong image slip through.
    #[test]
    fn raw_bin_with_all_mismatching_siblings_is_refused() {
        let dir = unique_dir("allwrong");
        let bin = dir.join("flash.bin");
        std::fs::write(&bin, [0xff, 0xff, 0xff, 0xff, 0x00, 0x01]).unwrap();
        // Two siblings, both Xtensa, on a RISC-V board: none match.
        write_fake_elf(&dir.join("app_a.elf"), crate::elf::EM_XTENSA);
        write_fake_elf(&dir.join("app_b.elf"), crate::elf::EM_XTENSA);

        let res = validate_flash_image_arch(&bin, crate::elf::EM_RISCV, "ESP32-C3 (RISC-V)");
        std::fs::remove_dir_all(&dir).ok();
        let err = res.expect_err("an all-wrong-arch image must be refused");
        let msg = format!("{err}");
        assert!(msg.contains("Xtensa"), "msg: {msg}");
        assert!(msg.contains("RISC-V"), "msg: {msg}");
        assert!(msg.contains("ESP32-C3"), "msg: {msg}");
    }

    /// A raw `.bin` with no sibling ELF at all is left unchecked (never errors).
    #[test]
    fn raw_bin_with_no_sibling_elf_is_unchecked() {
        let dir = unique_dir("nosib");
        let bin = dir.join("flash.bin");
        std::fs::write(&bin, [0xff, 0xff, 0xff, 0xff, 0x00, 0x01]).unwrap();
        let res = validate_flash_image_arch(&bin, crate::elf::EM_RISCV, "ESP32-C3");
        std::fs::remove_dir_all(&dir).ok();
        assert!(res.is_ok(), "no sibling ELF → unchecked, got: {res:?}");
    }
}
