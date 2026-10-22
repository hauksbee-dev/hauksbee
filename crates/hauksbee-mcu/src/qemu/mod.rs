//! Espressif-QEMU-backed MCU emulation for the ESP32 family.
//!
//! [`QemuBackend`] drives a headless Espressif QEMU process (the fork with full
//! ESP32 `SoC` peripheral models) over two TCP control channels plus a UART
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
//! ## Why not `-icount`
//!
//! `-icount shift=N` would make virtual time a deterministic function of
//! executed instructions, but on the Espressif fork's Xtensa `esp32` machine it
//! slows boot about 2.6x (3 s to 8-10 s to the "hello from esp32" banner; an
//! earlier 15 s probe saw no UART output at all), and QMP exposes only the raw
//! instruction count, not the virtual-clock offset the guest timers run on, so
//! crediting from it is wrong. icount is undocumented by Espressif; it stays off.
//!
//! Two other exact primitives are also unusable here. qtest `clock_step` gives
//! exact virtual-time advance and clean `readl`/`writel`, but it replaces the
//! accelerator and gates all guest execution on test-driven clock steps, so it
//! cannot boot a real flash image through the normal TCG path. A gdbstub
//! single-step budget is exact but far too slow for millions of instructions
//! per chunk over RSP; the gdbstub is used only for word-granular memory
//! writes (the GPIO input mailbox), never for stepping.
//!
//! ## Determinism without icount
//!
//! Without icount the esp32 machine runs its virtual clock at roughly wall rate
//! (like Renode's `RunFor` blocking for the interval). Run-to-run timing is not
//! bit-exact, but the firmware's *logic behaviour* is reproducible because it is
//! driven by the guest's deterministic peripheral timers (the `FreeRTOS` tick, the
//! UART baud generator), sampled only at chunk boundaries: the same standard a
//! logic analyser sampling a real board at the chunk rate would meet. The
//! integration test pins it -- identical boot banner, GPIO toggle count stable
//! within a couple of chunks across repeated runs.
//!
//! # Coupling model (transplanted ODR-poll, register-first)
//!
//! Like Renode, the backend is poll-based for GPIO output: after each chunk it
//! reads an output word over QMP (`xp /1wx`), diffs against the previous
//! snapshot, and synthesises per-bit edge callbacks for the wired pins only.
//! GPIO input is push-based: it writes an input word over the gdbstub `M`
//! packet. UART is the serial socket.
//!
//! Espressif's pinned release discards `GPIO_OUT_REG` writes. Hauksbee carries a
//! small exact-source QEMU patch that retains OUT/ENABLE state and exposes
//! paired read-only `gpio-out`/`gpio-enable` QOM capabilities. When both live
//! properties exist, the backend polls the real MMIO registers, so ordinary third-party ESP-IDF
//! firmware needs no cooperation. An unpatched binary fails closed to the old
//! output [`mailbox`] and prints the unlocking condition. GPIO input, SAR ADC,
//! and byte-level I2C/SPI interception still use their named mailbox slots
//! because those emulator models/hooks do not exist. See docs/cosim/MCU.md.
//!
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/qemu.md.

mod console;
mod flashimage;
mod gdb;
pub mod install;
mod process;
mod qmp;

// QemuProcess is exported (not just used internally) for the child-reaping
// integration tests and any host that needs the raw spawned-emulator handle
// rather than the backend.
pub use process::{find_qemu, is_available, QemuArch, QemuProcess};

use crate::external::{
    adc_count, builtin_configs, free_ports, nonempty_or, poll_state_mcu_methods, I2cTxn, PollState,
    UartSocket,
};
use crate::traits::{I2cCb, I2cEvent, Mcu, McuState, PinId, SpiCb, SpiEvent};
use anyhow::{bail, ensure, Context, Result};
use gdb::GdbStub;
use qmp::Qmp;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// GPIO observation/injection mailbox in ESP32 RTC slow memory.
///
/// The unpatched Espressif release does not retain `GPIO_OUT_REG`, and no family
/// member accepts host-driven `GPIO_IN_REG`. Hauksbee's reviewed source patch
/// retires this mailbox for GPIO output only; the backend capability-probes it
/// and otherwise falls back here.
///
/// The demo firmware keeps the compatibility exchange: it mirrors output into
/// `GPIO_OUT`, reads injected inputs from `GPIO_IN`, and raises `MAGIC`. This
/// proves old binaries and mailbox-aware firmware remain usable while ordinary
/// firmware uses the real peripheral register on a patched build.
///
/// RTC slow memory (`0x5000_0000`, 8 KiB, uncached, fixed) is chosen because a
/// minimal app never touches it, the address is stable, and it survives across
/// the run. Firmware side: `testdata/firmware/esp32_blinky/main/main.c`.
pub mod mailbox {
    /// Base of the mailbox in RTC slow memory (Xtensa ESP32 / ESP32-S3).
    pub const BASE: u32 = 0x5000_0000;
    /// Firmware -> host: mirror of `GPIO_OUT_REG`.
    pub const GPIO_OUT: u32 = BASE;
    /// Host -> firmware: injected GPIO input word.
    pub const GPIO_IN: u32 = BASE + 0x04;
    /// Firmware -> host: 0x6A6C6E69 once the firmware has set up the mailbox.
    pub const MAGIC: u32 = BASE + 0x08;
    /// Magic tag value ("inlj" little-endian of 0x6A6C6E69).
    pub const MAGIC_VALUE: u32 = 0x6A6C_6E69;

    // ── Mailbox v2: the ADC + bus extension ─────────────────────────────────
    //
    // Espressif QEMU models neither the SAR ADC nor a host hook for I2C/SPI
    // byte traffic, so these functions ride the same RAM mailbox as GPIO.
    // Like the GPIO words, this is a FIRMWARE CONTRACT, not general firmware
    // support: unmodified vendor firmware does not read these slots.
    // Each function is stated here precisely so a firmware author (or the
    // repo's demo firmware) can opt in; a slot is retired the day the
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
    /// Magic tag for the v2 extension (`MAGIC_VALUE` + 1, "jnlj").
    pub const BUS_MAGIC_VALUE: u32 = 0x6A6C_6E6A;

    /// Host -> firmware: bitmask of ADC channels that carry an injected count.
    pub const ADC_MASK: u32 = BASE + 0x10;
    /// Host -> firmware: first ADC count word; channel N is at `+ 4*N`.
    pub const ADC_CH0: u32 = BASE + 0x14;
    /// Number of injectable ADC channel slots.
    pub const ADC_CHANNELS: u8 = 8;
    /// Count written at full scale (the ESP32 SAR ADC is a 12-bit converter).
    pub const ADC_MAX_COUNT: u32 = 4095;
    /// Voltage mapping to `ADC_MAX_COUNT`, shared by the ESP32 family
    /// backends this module drives.
    ///
    /// This must be what the converter SATURATES at, not the board's rail, or
    /// every injected voltage scales wrong as a count. 3.1 V is the datasheet
    /// full-scale input at the highest attenuation for the ESP32-S2/S3/C3
    /// (ESP32-C3 datasheet, "ADC Characteristics": `ATTEN_DB_11` measurable range
    /// 0 ~ 3100 mV). The classic ESP32 is messier -- characterized-accurate to
    /// 2450 mV at 11 dB, theoretical full scale 1.1 V x 3.548 = 3.9 V clipped
    /// by the rail -- but real parts saturate near 3.1 V too. Injections
    /// between 3.1 V and the rail clamp to the top code, as the silicon does.
    pub const ADC_FULL_SCALE_VOLTS: f64 = 3.1;

    /// The mailbox word carrying channel `ch`'s injected count.
    #[must_use]
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
/// `out_reg` remains the RAM-mailbox fallback (see [`mailbox`]);
/// `peripheral_out_reg` is the real GPIO register used only after the backend
/// proves the running QEMU carries Hauksbee's stateful GPIO patch. Both words
/// use the `GPIO_OUT` bit layout, so edge synthesis is identical on either path.
///
/// Plain-data register-offset carrier, so a new part declares its mailbox
/// addresses instead of adding branches to the backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpioBank {
    /// Logical port letter the engine uses in [`PinId`]. The ESP32 GPIO matrix
    /// is a single flat 0..39 space; the demo uses the low bank (port `'0'`,
    /// bits 0..31).
    pub letter: char,
    /// Address of this bank's output-mirror word (firmware -> host).
    pub out_reg: u32,
    /// Real GPIO peripheral OUT register. Read only when the QOM capability
    /// probe succeeds; an old/unpatched fork continues to use `out_reg`.
    pub peripheral_out_reg: u32,
    /// Real GPIO peripheral ENABLE register, paired with
    /// `peripheral_out_reg`. It makes drive direction authoritative on the
    /// patched model instead of treating an input pin's output latch as driven.
    pub peripheral_enable_reg: u32,
    /// Address of this bank's `GPIO_FUNC0_OUT_SEL_CFG` (the first of one
    /// output-select word per pad, 4 bytes apart), or `None` when the part's
    /// matrix block is not described. It names the GPIO-matrix registers that
    /// route a peripheral output (LEDC PWM, timers, RMT, ...) to a pad, which
    /// the OUT/ENABLE words alone never show. Read only when the live QOM
    /// capability probe finds the stateful matrix model (see
    /// [`QemuBackend::pins_routed_to_peripheral`]).
    #[serde(default)]
    pub peripheral_func_out_sel_reg: Option<u32>,
    /// The `IO_MUX` and RTC IO blocks that can take this bank's pads off the
    /// GPIO matrix, or `None` when not described (see [`PadMuxBlocks`]).
    #[serde(default)]
    pub pad_mux: Option<PadMuxBlocks>,
    /// Address of this bank's input-injection word (host -> firmware).
    pub in_reg: u32,
    /// Number of valid bits in this bank.
    pub width: u8,
}

/// A classic ESP32 bank's `IO_MUX` and RTC IO blocks, and their live QOM objects.
///
/// `IO_MUX` holds `MCU_SEL` per pad; RTC IO the RTC mux, DAC and touch per RTC
/// pad. Each object's `word0` property proves the model retains writes; the
/// upstream esp32 machine maps both as unimplemented devices that read zero.
/// The per-pad layout decoded from them (`foreign_pads`) is the classic
/// ESP32's only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PadMuxBlocks {
    /// `IO_MUX` base (`PIN_CTRL`), 0x3FF49000.
    pub io_mux_reg: u32,
    /// RTC IO base (`RTCIO_RTC_GPIO_OUT_REG`), 0x3FF48400.
    pub rtc_io_reg: u32,
    /// QOM path of the `IO_MUX` model.
    pub io_mux_qom_path: String,
    /// QOM path of the RTC IO model.
    pub rtc_io_qom_path: String,
}

/// Per-part QEMU configuration: enough to boot a machine and bridge it.
///
/// Plain-data per-part surface, carrying every part difference as data rather
/// than as backend logic: the machine name, GPIO banks with their mailbox
/// offsets, icount/frequency, expected ISA and I2C bus paths are struct fields,
/// loadable from a `db/mcu/*.soc.toml` descriptor.
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
    /// models the `SoC` clocking; this is for the engine's bookkeeping).
    pub frequency_hz: u64,
    /// ELF `e_machine` this `SoC`'s core executes: `EM_XTENSA` for ESP32 / S3,
    /// `EM_RISCV` for the ESP32-C3. Used to gate the firmware ELF (or its
    /// sibling app ELF beside the merged flash image) against the board's ISA,
    /// so an Xtensa image on a RISC-V board is refused rather than booted into
    /// 136 MB of UART garbage. See [`crate::elf`].
    pub expected_e_machine: u16,
    /// Human-readable MCU/board name for arch-mismatch error messages.
    pub mcu_label: String,
    /// QOM path of the GPIO device. The Hauksbee QEMU patch exposes paired
    /// read-only `gpio-out` and `gpio-enable` properties here; both must be
    /// present for real peripheral level and drive-direction observation.
    pub gpio_qom_path: String,
    /// QOM paths of the machine's I2C buses, searched for an emulated I2C device
    /// matching a modeled sensor's address so the engine can push readings into
    /// it (e.g. the ESP32 machine's built-in `tmp105` on `i2c0`). Empty disables
    /// the I2C-device bridge.
    pub i2c_buses: Vec<String>,
    /// Pads with no output stage in silicon, per port letter: no firmware can
    /// drive them, so they are never counted as possibly driven by a
    /// peripheral the backend cannot see. From `[[soc.input_only]]`.
    #[serde(default)]
    pub input_only: Vec<InputOnlyPins>,
}

/// One port's input-only pads (`[[soc.input_only]]` in a `*.soc.toml`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputOnlyPins {
    /// The [`PinId`] port letter.
    pub letter: char,
    /// Bit indices within that port.
    pub bits: Vec<u8>,
}

/// The source from which GPIO output edges are observed in this QEMU process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpioOutputObservation {
    /// Real ESP32 `GPIO_OUT` MMIO state from a capability-probed patched model.
    PeripheralRegisters,
    /// Legacy RTC-slow-RAM word maintained by cooperating firmware.
    FirmwareMailbox,
}

impl QemuConfig {
    builtin_configs! {
        /// Classic ESP32 (Xtensa LX6). See `db/mcu/esp32.soc.toml`.
        esp32 => "qemu:esp32";
        /// ESP32-S3 (Xtensa LX7). See `db/mcu/esp32s3.soc.toml`.
        esp32s3 => "qemu:esp32s3";
        /// ESP32-C3 (RISC-V RV32IMC). See `db/mcu/esp32c3.soc.toml`.
        esp32c3 => "qemu:esp32c3";
    }
}

/// Validate the architecture of a QEMU flash image against the `SoC`'s ISA.
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
/// # Sibling resolution
///
/// A firmware directory can legitimately hold sibling ELFs of *different* ISAs
/// (`testdata/firmware/esp32_blinky/` ships both the Xtensa `esp32_blinky.elf`
/// and the RISC-V `esp32c3_blinky.elf` next to the raw `flash.bin`), so the
/// rule is "any sibling matches -> pass": read `e_machine` from every sibling
/// that parses as an ELF, and error only when there are such siblings and NONE
/// matches. No parseable sibling means unchecked, never a false error. When
/// nothing matches, the reported mismatch prefers a sibling whose filename stem
/// matches the `.bin`'s.
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
                .is_some_and(|e| e.eq_ignore_ascii_case("elf"));
            if !is_elf {
                continue;
            }
            // A `.elf` that does not actually parse as an ELF (None) is skipped;
            // a real I/O error is propagated.
            if let Some(found) = crate::elf::read_e_machine(&p)? {
                if found == expected {
                    // Any matching sibling → the image is arch-consistent.
                    return Ok(());
                }
                sibling_elfs.push(p);
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
                    .is_some_and(|es| es == bs || es.contains(bs) || bs.contains(es))
            })
        })
        .unwrap_or(&sibling_elfs[0]);
    crate::elf::validate_arch(report, expected, mcu_label)
}

/// Refuse a merged flash image whose 2nd-stage bootloader is not at the offset
/// this machine's ROM reads (0x1000 on esp32/esp32s2, 0x0 on esp32s3/esp32c3).
///
/// Why: booting such an image does not error, the ROM just loops printing
/// "invalid header: 0xffffffff" on UART forever, and the co-sim's failure then
/// surfaces minutes later as an opaque control-channel timeout. The classic way
/// to hit it is an image merged for one ESP32 family member run on a board that
/// binds another (e.g. a classic-esp32 image, bootloader at 0x1000, on an
/// ESP32-S3 board whose ROM reads 0x0); when the bootloader IS present at the
/// other family's offset, the error says exactly that.
fn validate_bootloader_magic(image: &Path, machine: &str) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    /// First byte of every ESP image (bootloader or app): the esptool magic.
    const ESP_IMAGE_MAGIC: u8 = 0xE9;

    let offset = flashimage::bootloader_offset(machine);
    let mut f = std::fs::File::open(image)
        .with_context(|| format!("opening flash image {}", image.display()))?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut byte_at = |off: u64| -> Option<u8> {
        if off >= len {
            return None;
        }
        let mut b = [0u8; 1];
        f.seek(SeekFrom::Start(off)).ok()?;
        f.read_exact(&mut b).ok()?;
        Some(b[0])
    };
    if byte_at(offset) == Some(ESP_IMAGE_MAGIC) {
        return Ok(());
    }
    let other_offset = if offset == 0x1000 { 0x0 } else { 0x1000 };
    let hint = if byte_at(other_offset) == Some(ESP_IMAGE_MAGIC) {
        format!(
            "It DOES have one at {other_offset:#x}, so it looks like an image \
             merged for a different ESP32 family member; rebuild it for this \
             chip (idf.py set-target, then merge with the bootloader at \
             {offset:#x})."
        )
    } else {
        "It does not look like an esptool merged flash image at all; pass the \
         app .elf your build produced instead (hauksbee builds the bootable \
         image from it), or produce a merged image with esptool merge_bin."
            .to_string()
    };
    bail!(
        "flash image {} has no bootloader at offset {offset:#x}, where the \
         '{machine}' ROM boots from, so it would boot-loop on an invalid \
         header instead of running. {hint}",
        image.display()
    );
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
/// The guest's virtual clock runs at roughly wall rate (no icount; see the
/// module notes), so the window IS the intended
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

/// `GPIO_FUNCn_OUT_SEL_CFG` bits [8:0] (`OUT_SEL`) selecting the plain
/// `GPIO_OUT`/`GPIO_ENABLE` bit as the pad's output. Any other value routes
/// peripheral output signal `s` (0..=255) to the pad through the GPIO matrix.
///
/// Source: ESP32 Technical Reference Manual v5.8, Register 6.32
/// `GPIO_FUNCn_OUT_SEL_CFG_REG` (address 0x3FF44530 + 4 * n): "A value of s
/// (0<=s<256) connects peripheral output s to GPIO output n. A value of 256
/// selects bit n of `GPIO_OUT_REG`/`GPIO_OUT1_REG` and `GPIO_ENABLE_REG`/
/// `GPIO_ENABLE1_REG` as the output value and output enable." Matches ESP-IDF
/// v5.4 `components/soc/esp32/register/soc/gpio_reg.h`
/// (`GPIO_FUNC0_OUT_SEL_CFG_REG = DR_REG_GPIO_BASE + 0x0530`,
/// `GPIO_FUNC0_OUT_SEL_V = 0x1FF`) and `soc/gpio_sig_map.h`
/// (`SIG_GPIO_OUT_IDX = 256`).
const FUNC_OUT_SEL_MASK: u32 = 0x1FF;
/// The "plain GPIO" `OUT_SEL` value (see [`FUNC_OUT_SEL_MASK`]).
const FUNC_OUT_SEL_GPIO: u32 = 0x100;
/// `GPIO_FUNCn_OUT_SEL_CFG` bit 10 (`OEN_SEL`): 1 forces the output enable from
/// `GPIO_ENABLE` bit n, 0 takes it from the peripheral (same TRM register).
const FUNC_OEN_SEL: u32 = 1 << 10;

/// ESP32 output signals of the SPI0/SPI1 memory bus: `SPICLK`..`SPICS2`
/// (0-7) and `SPID4`..`SPID7` (128-131), ESP-IDF v5.4
/// `components/soc/esp32/include/soc/gpio_sig_map.h`. The boot ROM and cache
/// own them, not the application, and QEMU's esp32 machine attaches its flash
/// on the module's default pads whatever the board's package (an ESP32-PICO-D4
/// keeps its flash elsewhere and leaves GPIO9/GPIO10 free), so a pad carrying
/// one says nothing about what the firmware drives on this board.
const fn is_memory_bus_signal(out_sel: u32) -> bool {
    matches!(out_sel, 0..=7 | 128..=131)
}

/// The pads (bit n set for pad n) whose `OUT_SEL` carries a memory-bus signal
/// (see [`is_memory_bus_signal`]): QEMU's flash attach, not the firmware.
fn memory_bus_pads(words: &[u32]) -> u32 {
    words
        .iter()
        .take(32)
        .enumerate()
        .filter(|&(_, &word)| is_memory_bus_signal(word & FUNC_OUT_SEL_MASK))
        .fold(0, |mask, (pad, _)| mask | 1 << pad)
}

/// The pads (bit n set for pad n) whose output is a peripheral's waveform
/// rather than the plain `GPIO_OUT` latch: `OUT_SEL` is neither 256 nor a
/// memory-bus signal (see [`is_memory_bus_signal`]), and the pad's output is
/// not held off. The output enable comes from `GPIO_ENABLE`
/// when `OEN_SEL` is set, so a pad whose enable bit is clear is high
/// impedance whatever is routed to it; with `OEN_SEL` clear the peripheral
/// owns the enable and the pad counts as driven (the safe direction: the
/// caller treats a routed pad as firmware-driven, never as passive).
///
/// `words` is one `GPIO_FUNCn_OUT_SEL_CFG` word per pad starting at pad 0;
/// only the first 32 pads fit the result and `enable` word.
fn peripheral_routed_pads(words: &[u32], enable: u32) -> u32 {
    words
        .iter()
        .take(32)
        .enumerate()
        .filter(|&(pad, &word)| {
            let out_sel = word & FUNC_OUT_SEL_MASK;
            let routed = out_sel != FUNC_OUT_SEL_GPIO && !is_memory_bus_signal(out_sel);
            let held_off = word & FUNC_OEN_SEL != 0 && enable & (1 << pad) == 0;
            routed && !held_off
        })
        .fold(0, |mask, (pad, _)| mask | 1 << pad)
}

/// The pads (bit n for pad n) whose output is the plain `GPIO_OUT` latch,
/// `OUT_SEL` 256; pads past the words read stay set, unseen rather than
/// excluded.
fn plain_gpio_pads(words: &[u32]) -> u32 {
    words
        .iter()
        .take(32)
        .enumerate()
        .filter(|&(_, &word)| word & FUNC_OUT_SEL_MASK != FUNC_OUT_SEL_GPIO)
        .fold(u32::MAX, |mask, (pad, _)| mask & !(1 << pad))
}

/// `IO_MUX` word offset of pads 0..=27 from the block base (ESP32 TRM v5.8
/// Table 6.12-2; ESP-IDF v5.5 `soc/esp32/register/soc/io_mux_reg.h`
/// `PERIPHS_IO_MUX_*_U`, which adds GPIO24 at 0x90). Pads 28-31 do not exist.
const IO_MUX_PAD_OFFSET: [u8; 28] = [
    0x44, 0x88, 0x40, 0x84, 0x48, 0x6c, 0x60, 0x64, 0x68, 0x54, 0x58, 0x5c, 0x34, 0x38, 0x30, 0x3c,
    0x4c, 0x50, 0x70, 0x74, 0x78, 0x7c, 0x80, 0x8c, 0x90, 0x24, 0x28, 0x2c,
];
/// Words read from the `IO_MUX` base: `PIN_CTRL` through GPIO24 (0x90).
const IO_MUX_WORDS: usize = 0x94 / 4;
/// Pads whose Function 0 is `GPIOn` as well as Function 2 (TRM Table 6.10-1;
/// ESP-IDF `FUNC_GPIOn_GPIOn_0`): 0, 2, 4, 5 and 16-27. On the rest, U0TXD,
/// U0RXD, the SD and the JTAG pads, `MCU_SEL` 0 (the reset value, Register
/// 6.34) is a direct function.
const F0_IS_GPIO: u32 = 0x0FFF_0035;
/// RTC IO words read from `RTCIO_PAD_DAC1_REG` (base + 0x84) through
/// `RTCIO_TOUCH_PAD7_REG` (+ 0xB0), TRM Table 6.12-3.
const RTC_IO_FIRST: u32 = 0x84;
const RTC_IO_WORDS: usize = 12;
/// Per RTC pad of the low bank: its word index from [`RTC_IO_FIRST`], the bits
/// that hand it to the RTC side, and the pad. `PAD_DAC1/2` (GPIO25/26): bit 17
/// `MUX_SEL`, bit 18 `XPD_DAC`, bit 10 `DAC_XPD_FORCE`; `TOUCH_PADn` (TOUCH0-7
/// on GPIO4, 0, 2, 15, 13, 12, 14, 27, Table 6.11-1): bit 19 `MUX_SEL`, bit 20
/// `XPD` (TRM Registers 6.49-6.53; ESP-IDF `rtc_io_reg.h`). `MUX_SEL` routes
/// the pad to the RTC block (RTC GPIO, ULP, RTC I2C); a powered DAC or touch
/// channel drives it whatever the mux says.
const RTC_PAD_OWNERS: [(usize, u32, u8); 10] = [
    (0, 0x0006_0400, 25),
    (1, 0x0006_0400, 26),
    (4, 0x0018_0000, 4),
    (5, 0x0018_0000, 0),
    (6, 0x0018_0000, 2),
    (7, 0x0018_0000, 15),
    (8, 0x0018_0000, 13),
    (9, 0x0018_0000, 12),
    (10, 0x0018_0000, 14),
    (11, 0x0018_0000, 27),
];

/// The pads (bit n = pad n) that something other than the GPIO matrix owns:
/// an `IO_MUX` direct function (`MCU_SEL`, bits [14:12], other than 2, or 0
/// where Function 0 is not GPIO; TRM §6.2.2, Register 6.34) or the RTC side
/// (see [`RTC_PAD_OWNERS`]). Such a pad's `GPIO_OUT` bit never reaches it.
/// `io_mux` starts at the `IO_MUX` base, `rtc_io` at [`RTC_IO_FIRST`]; a word
/// past either slice counts the pad as owned elsewhere.
fn foreign_pads(io_mux: &[u32], rtc_io: &[u32]) -> u32 {
    let mux = IO_MUX_PAD_OFFSET.iter().enumerate().filter(|&(pad, &off)| {
        io_mux.get(usize::from(off) / 4).is_none_or(|w| {
            let sel = (w >> 12) & 7;
            sel != 2 && (sel != 0 || F0_IS_GPIO & (1 << pad) == 0)
        })
    });
    let rtc = RTC_PAD_OWNERS
        .iter()
        .filter(|&&(i, bits, _)| rtc_io.get(i).is_none_or(|w| w & bits != 0))
        .map(|&(_, _, pad)| usize::from(pad));
    mux.map(|(pad, _)| pad)
        .chain(rtc)
        .fold(0, |m, pad| m | 1 << pad)
}

/// Why a drive on `pin` could go unreported, completing "the backend could not
/// observe peripheral routing: ...", or `None` when any drive on it would be
/// seen, or it is QEMU's own flash attach (`memory_bus`, bit n = pad n).
///
/// A pad is driven three ways besides `GPIO_OUT` (ESP32 TRM v5.8 ch. 6): a
/// peripheral through the GPIO matrix (§6.3, seen when `matrix_seen`); an
/// `IO_MUX` direct function, `MCU_SEL` other than GPIO (§6.4, Table 6.10-1:
/// U0TXD, `CLK_OUT1..3`, HSPI/VSPI, SD, JTAG, EMAC); and the RTC IO mux (§6.5,
/// Table 6.11-1: `DAC_1`/`DAC_2` on GPIO25/26, touch, RTC GPIO). The upstream
/// esp32 machine maps `IO_MUX` and RTC IO as unimplemented devices that read
/// zero; Hauksbee's patch retains them, which the live probe reports as
/// `pads_seen`. With both seen, a pad is unobserved only while `foreign`
/// (latched [`foreign_pads`]) says a direct function or the RTC side owns it.
fn routing_blind_spot(
    config: &QemuConfig,
    (matrix_seen, pads_seen): (bool, bool),
    memory_bus: u32,
    foreign: u32,
    pin: PinId,
) -> Option<&'static str> {
    let Some(bank) = config
        .banks
        .iter()
        .find(|b| b.letter == pin.port && pin.bit < b.width.min(32))
    else {
        return Some(
            "the pad is outside the GPIO banks this backend polls, so neither its GPIO_OUT \
             level nor a matrix, IO_MUX or RTC IO drive on it is observed",
        );
    };
    if memory_bus & (1 << pin.bit) != 0 {
        return None;
    }
    let matrix = matrix_seen && bank.peripheral_func_out_sel_reg.is_some();
    if matrix && pads_seen && bank.pad_mux.is_some() {
        return (foreign & (1 << pin.bit) != 0).then_some(
            "the pad's IO_MUX selects a direct function (UART, SPI, SD, JTAG, EMAC or \
             CLK_OUT), or the RTC IO side owns it (RTC GPIO, ULP, RTC I2C, touch or a DAC), \
             and this backend does not model that drive",
        );
    }
    Some(if matrix {
        "this QEMU build reports GPIO-matrix routing but does not retain the IO_MUX or RTC IO \
         selection a pad's own UART, SPI, SD, EMAC or CLK_OUT signal, a DAC, a touch pad or \
         an RTC GPIO output uses (both read as zero); rebuild it with \
         scripts/install-sims.sh --qemu-patched-source"
    } else {
        "this QEMU build retains neither the GPIO-matrix output selection (a PWM, timer or \
         RMT output) nor the IO_MUX or RTC IO selection (a pad's own UART, SPI or EMAC \
         signal, a DAC, touch or RTC GPIO output); a build that reports them comes from \
         scripts/install-sims.sh --qemu-patched-source"
    })
}

const fn observable_gpio_changes(prev: u32, new: u32, prev_enable: u32, enable: u32) -> u32 {
    ((new ^ prev) | (enable & !prev_enable)) & enable
}

/// Espressif-QEMU-backed [`Mcu`].
#[allow(clippy::struct_excessive_bools)] // independent handshake and one-shot warning latches
pub struct QemuBackend {
    config: QemuConfig,
    // Drop order: control channels and uart close before the process is killed.
    qmp: Qmp,
    /// gdbstub, used only for GPIO-input memory writes. Optional: if QEMU's
    /// gdbserver could not be attached, input injection is disabled but the rest
    /// of the backend (UART, GPIO output, stepping) still works.
    gdb: Option<GdbStub>,
    /// UART bridge, callbacks, wired-bank hint and cycle counter shared with
    /// the other poll-based backend.
    core: PollState,
    process: QemuProcess,
    /// When the caller handed us a bare app ELF, this is the merged flash
    /// image built from it (see [`flashimage`]); QEMU boots from this file, so
    /// it must live exactly as long as the process does.
    _flash_temp: Option<tempfile::NamedTempFile>,

    /// Last-read OUT register per bank letter, for edge synthesis.
    last_out: HashMap<char, u32>,
    /// Last-read real GPIO ENABLE register per bank. Populated only on the
    /// patched peripheral path; mailbox fallback keeps direction unobservable.
    last_enable: HashMap<char, u32>,
    /// Capability-probed GPIO observation source. Never inferred from a QEMU
    /// version string: only the live QOM property unlocks peripheral reads.
    gpio_output_observation: GpioOutputObservation,
    /// Whether the live QOM object carries the GPIO-matrix output-select
    /// capability (`gpio-func-out-sel`), probed like the OUT/ENABLE pair.
    /// Without it the matrix registers read back as zero whatever the firmware
    /// wrote, so they are never consulted.
    func_out_sel_observable: bool,
    /// Whether every described `IO_MUX`/RTC IO block's live QOM object
    /// carries `word0`, the patch's proof that it retains writes.
    pad_mux_observable: bool,
    /// Pads each bank has ever had owned by an `IO_MUX` direct function or
    /// the RTC side (bit n = pad n; see [`foreign_pads`]). Latched like
    /// `routed_pads`.
    foreign_pads: HashMap<char, u32>,
    /// Pads each bank has ever had a peripheral output routed onto while
    /// enabled (bit n = pad n). Latched: a PWM waveform that ran during the run
    /// stays a firmware-driven net after the firmware stops it.
    routed_pads: HashMap<char, u32>,
    /// Pads carrying a memory-bus signal as of the last poll (bit n = pad n):
    /// QEMU's own flash attach, so neither drive evidence nor an unobserved
    /// peripheral the firmware chose.
    memory_bus_now: HashMap<char, u32>,
    /// Current driven IN register per bank letter (what we poke for inputs).
    in_shadow: HashMap<char, u32>,
    /// True once mailbox-aware firmware raises [`mailbox::MAGIC`], or ordinary
    /// firmware produces an enabled real-GPIO transition on the capability-
    /// probed patched model. Gates the boot-only run-window floor (see
    /// [`run_window`]); without either signal the conservative floor remains.
    boot_complete: bool,
    /// Resolved QOM path of the emulated I2C device at each modeled sensor
    /// address (`None` once searched and not found, so we do not re-walk QMP
    /// every frame). Populated lazily by [`Mcu::set_i2c_device_temperature`].
    i2c_temp_paths: HashMap<u8, Option<String>>,

    // ── Mailbox v2 state ─────────────────────────────────────
    /// I2C byte-event handler, serviced from the mailbox I2C request cell.
    on_i2c: Option<I2cCb>,
    /// SPI byte-event handler, serviced from the mailbox SPI request cell.
    on_spi: Option<SpiCb>,
    /// True once `BUS_MAGIC_VALUE` has been observed; the bus cells are never
    /// read before the firmware declares them valid.
    bus_magic_seen: bool,
    /// Last serviced I2C / SPI request sequence numbers.
    i2c_serviced_seq: u32,
    spi_serviced_seq: u32,
    /// Open I2C transaction for Start/Stop synthesis (the same state machine
    /// the Renode bridge runs).
    i2c_txn: I2cTxn,
    /// Shadow of the `ADC_MASK` word (channels injected so far).
    adc_mask_shadow: u32,
    /// One-time warning flag for ADC injection without a gdbstub.
    adc_no_gdb_warned: bool,
    /// One-time warning flag for digital-input injection without a gdbstub.
    digital_no_gdb_warned: bool,
    /// One-time warning flag for a GPIO port that has no bank in this `SoC`
    /// descriptor (e.g. ESP32 GPIO>=32 maps to port '1', but the ESP32 `SoC` has a
    /// single 32-bit bank '0'). Such pins cannot be observed or injected, so the
    /// backend must SAY so rather than silently report the pin as never-driven.
    unbridged_gpio_warned: bool,
    /// Panics and reboots read off UART0, and PC samples outside the mask ROM.
    console: console::BootConsole,
    pcs: crate::traits::PcSampler,
}

impl QemuBackend {
    /// Boot an Espressif QEMU machine from `config` running the firmware at
    /// `flash_image`, and connect the QMP, gdbstub, and UART channels.
    ///
    /// Unlike the Renode backend (which loads firmware via a Monitor command
    /// after the machine is up), QEMU boots from a merged flash image
    /// (2nd-stage bootloader + partition table + app, the `esptool merge_bin`
    /// layout) given on the command line, so the image path is needed at spawn
    /// time. Two input shapes are accepted:
    ///
    /// - a merged flash image (`flash.bin`): booted as-is, after a check that
    ///   its bootloader sits where THIS chip's ROM reads it (a wrong-chip
    ///   image otherwise boot-loops forever with no diagnostic);
    /// - a bare app ELF, the artifact a user's build actually produces: the
    ///   merged image is built from it in-process (see `flashimage`) with
    ///   the default bootloader + partition table, exactly what
    ///   `esptool merge_bin` would have produced on the build machine.
    ///
    /// # Errors
    ///
    /// When the image is missing, fails the arch or bootloader check, cannot be
    /// converted from ELF, or QEMU dies or does not answer on its QMP or UART
    /// socket. A gdbstub that cannot attach is not an error.
    pub fn new(config: QemuConfig, flash_image: &Path) -> Result<Self> {
        Self::new_with_strap(config, flash_image, None)
    }

    /// [`Self::new`] with the chip's boot-strap pins set: the machine latches
    /// `strap` as the levels its strapping pins held at reset, so a board that
    /// holds GPIO0 low boots the ROM downloader exactly as the silicon would.
    /// `None` leaves QEMU's own default (a normal flash boot).
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    #[allow(clippy::too_many_lines)] // one linear spawn-and-handshake sequence
    pub fn new_with_strap(
        config: QemuConfig,
        flash_image: &Path,
        strap: Option<&crate::boot_mode::QemuStrapMode>,
    ) -> Result<Self> {
        if !flash_image.exists() {
            bail!(
                "ESP32 firmware not found: {}. Pass the app .elf your build \
                 produced, or a merged flash image (esptool merge_bin; see \
                 testdata/firmware/esp32_blinky/build.sh).",
                flash_image.display()
            );
        }

        // Arch gate (BEFORE spawning QEMU): the persona-esp32-iot hunt loaded an
        // Xtensa image onto a RISC-V ESP32-C3 and got 136 MB of UART garbage with
        // no error. An ELF carries its ISA; a merged `.bin` is raw, so for those
        // we check a sibling app ELF when one exists (see
        // `validate_flash_image_arch`), and never false-error otherwise.
        let is_elf = crate::elf::read_e_machine(flash_image)?.is_some();
        let (boot_image, flash_temp) = if is_elf {
            crate::elf::validate_arch(flash_image, config.expected_e_machine, &config.mcu_label)?;
            // A bare ELF is NOT bootable by QEMU (its flash model takes only
            // 2/4/8/16 MB raw images, and the ROM wants an image header, not
            // an ELF header). Build the real merged image from it.
            let tmp = flashimage::merged_image_from_elf(flash_image, &config.machine)
                .with_context(|| {
                    format!(
                        "building a bootable flash image from {}",
                        flash_image.display()
                    )
                })?;
            (tmp.path().to_path_buf(), Some(tmp))
        } else {
            validate_flash_image_arch(flash_image, config.expected_e_machine, &config.mcu_label)?;
            // An app-only .bin is merged with its build's bootloader and
            // partition table, and a short merged image is padded to the flash
            // size QEMU boots from (see `flashimage::prepare_raw_image`).
            let staged = flashimage::prepare_raw_image(flash_image, &config.machine)?;
            let image = staged
                .as_ref()
                .map_or(flash_image, tempfile::NamedTempFile::path);
            // Refuse an image whose bootloader is not where this chip's ROM
            // will look, BEFORE spawning: booted anyway, the ROM loops on
            // "invalid header: 0xffffffff" forever and the failure would
            // surface much later as an opaque control-channel error.
            validate_bootloader_magic(image, &config.machine)?;
            (image.to_path_buf(), staged)
        };

        let [qmp_port, gdb_port, uart_port] = free_ports()?;

        let mut process = QemuProcess::spawn_with_strap(
            config.arch,
            &config.machine,
            &boot_image,
            config.icount_shift,
            qmp_port,
            uart_port,
            strap,
        )?;
        // Tell QEMU to also expose a gdbstub. We pass it as part of spawn? The
        // process builder set QMP/serial; the gdbstub is added via QMP after
        // connect using `human-monitor-command gdbserver`, which attaches a stub
        // without restarting. That keeps the spawn argument list stable.

        let died = |process: &QemuProcess, phase: &str| {
            anyhow::anyhow!(
                "Espressif QEMU exited {phase} booting {} on machine '{}'. QEMU said: {}",
                flash_image.display(),
                config.machine,
                nonempty_or(&process.stderr_output(), "(nothing on stderr)"),
            )
        };
        // Give QEMU a moment; if it died immediately the image/args were bad,
        // and its own stderr says why (bad drive size, unknown machine, ...).
        std::thread::sleep(Duration::from_millis(150));
        if process.has_exited() {
            return Err(died(&process, "immediately"));
        }

        let mut qmp = match Qmp::connect(qmp_port, QemuProcess::startup_timeout()) {
            Ok(q) => q,
            // Losing the control socket almost always means QEMU died between
            // the spawn check and the handshake; report the death and QEMU's
            // stderr rather than the raw socket error.
            Err(_) if process.has_exited() => return Err(died(&process, "while")),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!(
                        "connecting to the QEMU control (QMP) socket for machine \
                         '{}'; the emulator is running but did not answer",
                        config.machine
                    )
                });
            }
        };
        qmp.set_timeout(Duration::from_secs(20));
        // The guest starts stopped (`-S`); stopping again is harmless and keeps
        // the first chunk's known state explicit (lockstep: cont -> window -> stop).
        checked_control(&mut process, "initial stop", qmp.stop())?;

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

        let uart = UartSocket::connect("QEMU", uart_port, Duration::from_secs(10))?;

        let gpio_output_observation = match (
            qmp.qom_get(&config.gpio_qom_path, "gpio-out"),
            qmp.qom_get(&config.gpio_qom_path, "gpio-enable"),
        ) {
            (Ok(out), Ok(enable))
                if out.parse::<u32>().is_ok() && enable.parse::<u32>().is_ok() =>
            {
                GpioOutputObservation::PeripheralRegisters
            }
            _ => {
                eprintln!(
                    "QEMU GPIO output limitation: '{}' has no stateful GPIO register \
                     read-back. Output observation requires firmware to maintain the \
                     Hauksbee mailbox at {:#010x}. GPIO input and SAR ADC also remain \
                     explicit mailbox contracts; I2C/SPI byte interception does too.",
                    config.mcu_label,
                    mailbox::BASE,
                );
                GpioOutputObservation::FirmwareMailbox
            }
        };

        // The matrix output-select words are trusted only when the live QOM
        // object proves the model retains them (an older patched build keeps
        // OUT/ENABLE but discards these writes, so they would read as zero, a
        // signal-0 routing on every pad).
        let wants_matrix = config
            .banks
            .iter()
            .any(|b| b.peripheral_func_out_sel_reg.is_some());
        let func_out_sel_observable = wants_matrix
            && gpio_output_observation == GpioOutputObservation::PeripheralRegisters
            && qmp
                .qom_get(&config.gpio_qom_path, "gpio-func-out-sel")
                .is_ok_and(|v| v.parse::<u32>().is_ok());
        if wants_matrix
            && gpio_output_observation == GpioOutputObservation::PeripheralRegisters
            && !func_out_sel_observable
        {
            eprintln!(
                "QEMU GPIO matrix limitation: '{}' retains GPIO OUT/ENABLE but not the \
                 GPIO_FUNCn_OUT_SEL_CFG output-select words, so a PWM, timer or RMT output \
                 routed to a pad through the GPIO matrix is not observed unless the \
                 firmware also sets GPIO_ENABLE. Rebuild the patched QEMU with \
                 scripts/install-sims.sh --qemu-patched-source.",
                config.mcu_label,
            );
        }

        let pad_mux_observable = gpio_output_observation
            == GpioOutputObservation::PeripheralRegisters
            && config.banks.iter().any(|b| b.pad_mux.is_some())
            && config
                .banks
                .iter()
                .filter_map(|b| b.pad_mux.as_ref())
                .all(|p| {
                    [&p.io_mux_qom_path, &p.rtc_io_qom_path].iter().all(|path| {
                        qmp.qom_get(path, "word0")
                            .is_ok_and(|v| v.parse::<u32>().is_ok())
                    })
                });

        let last_out = config.banks.iter().map(|b| (b.letter, 0u32)).collect();
        let initial_enable =
            if gpio_output_observation == GpioOutputObservation::PeripheralRegisters {
                0
            } else {
                // The legacy mailbox has levels but no direction. Treat every bit
                // as eligible for level-edge synthesis without fabricating a
                // first-poll "newly enabled" edge for every pin.
                u32::MAX
            };
        let last_enable = config
            .banks
            .iter()
            .map(|b| (b.letter, initial_enable))
            .collect();
        let in_shadow = config.banks.iter().map(|b| (b.letter, 0u32)).collect();

        let mut core = PollState::new("QEMU", Some(uart));
        // The machine was started FROM the flash image, so the guest is already
        // booted by the time this returns; nothing loads firmware later.
        core.firmware_loaded = true;

        Ok(Self {
            config,
            qmp,
            gdb,
            core,
            process,
            _flash_temp: flash_temp,
            last_out,
            last_enable,
            gpio_output_observation,
            func_out_sel_observable,
            pad_mux_observable,
            foreign_pads: HashMap::new(),
            routed_pads: HashMap::new(),
            memory_bus_now: HashMap::new(),
            in_shadow,
            boot_complete: false,
            i2c_temp_paths: HashMap::new(),
            on_i2c: None,
            on_spi: None,
            bus_magic_seen: false,
            i2c_serviced_seq: 0,
            spi_serviced_seq: 0,
            i2c_txn: I2cTxn::default(),
            adc_mask_shadow: 0,
            adc_no_gdb_warned: false,
            digital_no_gdb_warned: false,
            unbridged_gpio_warned: false,
            console: console::BootConsole::default(),
            pcs: crate::traits::PcSampler::default(),
        })
    }

    /// Convenience constructor for the classic ESP32.
    ///
    /// # Errors
    ///
    /// As [`Self::new`].
    pub fn esp32(flash_image: &Path) -> Result<Self> {
        Self::new(QemuConfig::esp32(), flash_image)
    }

    /// Read one GPIO register word, preferring QMP `xp` and falling back to
    /// the gdbstub, with a dead child reported as such rather than as a
    /// socket error.
    fn read_gpio_word(&mut self, what: &str, addr: u32) -> Result<u32> {
        let qmp_err = match self.qmp.read_u32(addr) {
            Ok(v) => return Ok(v),
            Err(e) => e,
        };
        self.process
            .ensure_running(&format!("reading a GPIO {what} word over QMP"))
            .with_context(|| format!("QMP GPIO {what} read at {addr:#x} failed: {qmp_err:#}"))?;
        match self.gdb.as_mut() {
            Some(gdb) => gdb.read_u32(addr).with_context(|| {
                format!("QMP GPIO {what} read at {addr:#x} failed ({qmp_err:#}); gdb fallback failed too")
            }),
            None => Err(qmp_err).context(format!(
                "QMP GPIO {what} read at {addr:#x} failed and no gdbstub fallback is attached"
            )),
        }
    }

    /// Read one bank's selected output word. The live QOM capability probe,
    /// not the configured address alone, decides between peripheral and
    /// mailbox so an unpatched QEMU never yields a false zero register trace.
    fn read_out(&mut self, bank: &GpioBank) -> Result<u32> {
        let addr = match self.gpio_output_observation {
            GpioOutputObservation::PeripheralRegisters => bank.peripheral_out_reg,
            GpioOutputObservation::FirmwareMailbox => bank.out_reg,
        };
        self.read_gpio_word("output", addr)
    }

    /// Poll the relevant banks' OUT/ENABLE registers, diff against the
    /// snapshot, and fire per-bit edges for pins actually configured as output.
    fn poll_gpio_edges(&mut self) -> Result<bool> {
        let banks: Vec<GpioBank> = self
            .config
            .banks
            .iter()
            .filter(|b| self.core.polls(b.letter))
            .cloned()
            .collect();
        let mut observed_change = false;
        for bank in &banks {
            let new = self.read_out(bank)?;
            let enable = match self.gpio_output_observation {
                GpioOutputObservation::PeripheralRegisters => {
                    let raw = self.read_gpio_word("enable", bank.peripheral_enable_reg)?;
                    // GPIO_OUT drives only the pads whose matrix selects it;
                    // the rest belong to a peripheral (the boot ROM leaves the
                    // flash pads enabled on SPIQ..SPICS0, holding GPIO_OUT 0).
                    raw & self.poll_matrix_routing(bank, raw)? & !self.poll_pad_mux(bank)?
                }
                // Mailbox firmware publishes only levels. Preserve the old
                // behavior while reporting direction as unobservable.
                GpioOutputObservation::FirmwareMailbox => u32::MAX,
            };
            let prev = self.last_out.get(&bank.letter).copied().unwrap_or(0);
            let prev_enable = self.last_enable.get(&bank.letter).copied().unwrap_or(0);
            if new != prev || enable != prev_enable {
                observed_change = true;
                let changed = observable_gpio_changes(prev, new, prev_enable, enable);
                self.core
                    .publish_edges(bank.letter, bank.width, changed, new);
                self.last_out.insert(bank.letter, new);
                self.last_enable.insert(bank.letter, enable);
            }
        }
        Ok(observed_change)
    }

    /// Read the bank's GPIO-matrix output-select words (one `xp` round trip)
    /// and latch the pads a peripheral output is routed onto. A PWM, timer or
    /// RMT waveform reaches its pad through `GPIO_FUNCn_OUT_SEL_CFG` and never
    /// touches `GPIO_OUT`, so without this a pad driven by LEDC alone looks
    /// like an untouched input. Returns the pads whose output is the plain
    /// `GPIO_OUT` latch (`OUT_SEL` 256); every pad until the capability probe
    /// succeeded, since the matrix is then unseen.
    fn poll_matrix_routing(&mut self, bank: &GpioBank, enable: u32) -> Result<u32> {
        let Some(reg) = bank.peripheral_func_out_sel_reg else {
            return Ok(u32::MAX);
        };
        if !self.func_out_sel_observable {
            return Ok(u32::MAX);
        }
        let pads = usize::from(bank.width.min(32));
        let words = self.qmp.read_words(reg, pads).with_context(|| {
            format!("reading the GPIO matrix output-select block at {reg:#x} over QMP")
        })?;
        let routed = peripheral_routed_pads(&words, enable);
        self.memory_bus_now
            .insert(bank.letter, memory_bus_pads(&words));
        *self.routed_pads.entry(bank.letter).or_insert(0) |= routed;
        Ok(plain_gpio_pads(&words))
    }

    /// Read the bank's `IO_MUX` and RTC IO words (two `xp` round trips) and
    /// latch the pads a direct function or the RTC side owns; returns those
    /// owned now, whose `GPIO_OUT` bit does not reach the pad. None (0) until
    /// the capability probe succeeded, since the blocks then read zero.
    fn poll_pad_mux(&mut self, bank: &GpioBank) -> Result<u32> {
        let Some(blocks) = bank.pad_mux.as_ref().filter(|_| self.pad_mux_observable) else {
            return Ok(0);
        };
        let (mux, rtc) = (blocks.io_mux_reg, blocks.rtc_io_reg + RTC_IO_FIRST);
        let io_mux = self.qmp.read_words(mux, IO_MUX_WORDS);
        let rtc_io = self.qmp.read_words(rtc, RTC_IO_WORDS);
        let foreign = foreign_pads(
            &io_mux.with_context(|| format!("reading IO_MUX at {mux:#x} over QMP"))?,
            &rtc_io.with_context(|| format!("reading RTC IO at {rtc:#x} over QMP"))?,
        );
        *self.foreign_pads.entry(bank.letter).or_insert(0) |= foreign;
        Ok(foreign)
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
        if !self.core.firmware_loaded {
            bail!("no firmware image booted in the QEMU machine");
        }
        // Credit the span the virtual clock actually ran (see `Qmp::run_for`):
        // not the requested `seconds` or the slept `window` (the guest keeps
        // running through the control round trips, a ~1.4x-slow bias), and not
        // QEMU's RESUME→STOP event span, which also counts the post-freeze
        // vCPU-parking wait and inflates without bound under host load. The
        // boot floor/cap reshapes the window; the same span brackets it. What
        // no wall measurement can see is TCG pace itself, which is why
        // TIMING_LIMITATION below still reaches every report surface.
        let window = run_window(seconds, self.boot_complete);
        let credited = checked_control(&mut self.process, "cont/stop", self.qmp.run_for(window))?;
        self.core
            .credit(credited.as_secs_f64(), self.config.frequency_hz);

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

        let gpio_activity = self.poll_gpio_edges()?;
        // Service the mailbox bus cells while the guest is paused,
        // so a firmware spin-waiting on RSP_SEQ proceeds next chunk.
        self.service_bus_mailbox()?;
        let console = self.core.pump_uart_out(false)?;
        self.console.feed(&console);
        // A PC sample while paused: one HMP round trip, and only on the
        // sampler's schedule. A failed read is a missing sample, not an error.
        if self.pcs.due() {
            let pc = self.qmp.hmp("info registers").ok();
            if let Some(pc) = pc.as_deref().and_then(console::parse_pc) {
                self.pcs.record(pc);
                self.console.sample_pc(pc);
            }
        }
        // Unmodified firmware deliberately has no mailbox MAGIC. With the
        // patched GPIO model, a real enabled peripheral transition is direct
        // guest activity and is therefore a valid end-of-boot signal;
        // this retires the boot-floor dependency on cooperating firmware too.
        if !self.boot_complete
            && self.gpio_output_observation == GpioOutputObservation::PeripheralRegisters
            && gpio_activity
        {
            self.boot_complete = true;
        }
        // A child that died after replying to `stop` must make this chunk fail,
        // even if a buffered/fallback read happened to return stale state.
        self.process
            .ensure_running("completing the QEMU co-simulation chunk")?;
        self.core.uart_rx_inflight = 0;
        Ok(())
    }

    /// Read one guest word over the control channels: QMP `xp` first, gdbstub
    /// fallback, erroring only when both paths fail (the mailbox protocol must
    /// not mistake a dead control channel for a zero word).
    fn read_guest_u32(&mut self, addr: u32) -> Result<u32> {
        match self.qmp.read_u32(addr) {
            Ok(v) => Ok(v),
            Err(qmp_err) => self.gdb.as_mut().map_or_else(
                || Err(qmp_err).context("QMP word read failed and no gdbstub fallback"),
                |g| g.read_u32(addr),
            ),
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

    /// Service the mailbox v2 bus cells once per chunk, while the
    /// guest is paused. Zero-cost when no callback is registered; one word
    /// read per chunk until the firmware raises `BUS_MAGIC`.
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
    #[allow(clippy::cast_possible_truncation)] // the 7-bit address sits in the word's low byte
    fn service_i2c_cell(&mut self) -> Result<()> {
        let Some((seq, op, len)) = self.pending_request(
            "I2C",
            [
                mailbox::I2C_REQ_SEQ,
                mailbox::I2C_REQ_OP,
                mailbox::I2C_REQ_LEN,
            ],
            self.i2c_serviced_seq,
        )?
        else {
            return Ok(());
        };
        let addr = self.read_guest_u32(mailbox::I2C_REQ_ADDR)? as u8;
        let payload = if op == mailbox::I2C_OP_WRITE && len != 0 {
            self.read_guest_bytes(mailbox::I2C_REQ_DATA, len)?
        } else {
            Vec::new()
        };

        // Dispatch with the callback taken out of `self` so the borrow does
        // not pin the control channels.
        let mut cb = self.on_i2c.take().expect("checked by caller");
        let txn = &mut self.i2c_txn;
        let mut reply: Vec<u8> = Vec::new();
        let dispatch = (|| -> Result<()> {
            match op {
                mailbox::I2C_OP_WRITE => {
                    txn.ensure(addr, false, &mut cb);
                    for data in payload {
                        let _ = cb(I2cEvent::Write { addr, data });
                    }
                }
                mailbox::I2C_OP_READ => {
                    txn.ensure(addr, true, &mut cb);
                    for _ in 0..len {
                        // `None` is the model layer's "no slave / NACK"; 0xFF
                        // is the level an open-drain bus floats to (the same
                        // convention as the Renode bridge).
                        reply.push(cb(I2cEvent::Read { addr }).unwrap_or(0xFF));
                    }
                }
                mailbox::I2C_OP_STOP => txn.stop(&mut cb),
                other => bail!("QEMU I2C mailbox: unknown op {other}"),
            }
            Ok(())
        })();
        self.on_i2c = Some(cb);
        // The request is SERVICED once dispatched, acknowledged or not: mark
        // it before the response writes so a caller that retries after a
        // failed write cannot replay the byte events into a stateful slave
        // model. A failed write still errors out loudly below (the firmware's
        // spin-wait then times out rather than reading a half-written reply).
        self.i2c_serviced_seq = seq;
        dispatch?;
        self.reply(mailbox::I2C_RSP_DATA, mailbox::I2C_RSP_SEQ, &reply, seq)
    }

    /// Service one pending SPI request: a byte-transfer burst (one
    /// [`SpiEvent`] per byte, MISO bytes returned in the response cell) or a
    /// chip-select deassert.
    fn service_spi_cell(&mut self) -> Result<()> {
        let Some((seq, op, len)) = self.pending_request(
            "SPI",
            [
                mailbox::SPI_REQ_SEQ,
                mailbox::SPI_REQ_OP,
                mailbox::SPI_REQ_LEN,
            ],
            self.spi_serviced_seq,
        )?
        else {
            return Ok(());
        };
        let mosi = if op == mailbox::SPI_OP_TRANSFER && len != 0 {
            self.read_guest_bytes(mailbox::SPI_REQ_DATA, len)?
        } else {
            Vec::new()
        };

        // Coarse poll-boundary stamp, the same tier the pin edges carry
        // (`cycle_exact()` is false on this backend).
        let cyc = self.core.cycles;
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
        self.reply(mailbox::SPI_RSP_DATA, mailbox::SPI_RSP_SEQ, &miso, seq)
    }

    /// The pending request in a mailbox cell as `(seq, op, len)`, or `None`
    /// when the firmware has raised nothing since `serviced`. `cell` is the
    /// cell's `[REQ_SEQ, REQ_OP, REQ_LEN]` words.
    fn pending_request(
        &mut self,
        bus: &str,
        cell: [u32; 3],
        serviced: u32,
    ) -> Result<Option<(u32, u32, usize)>> {
        let seq = self.read_guest_u32(cell[0])?;
        if seq == 0 || seq == serviced {
            return Ok(None);
        }
        let op = self.read_guest_u32(cell[1])?;
        let len = self.read_guest_u32(cell[2])?;
        ensure!(
            len <= mailbox::BUS_DATA_MAX,
            "QEMU {bus} mailbox request too large: {len} bytes (max {})",
            mailbox::BUS_DATA_MAX
        );
        Ok(Some((seq, op, len as usize)))
    }

    /// Write the reply bytes (if any) into a response cell and acknowledge
    /// `seq`, which releases the firmware's spin-wait.
    fn reply(&mut self, data_addr: u32, seq_addr: u32, reply: &[u8], seq: u32) -> Result<()> {
        if !reply.is_empty() {
            self.write_guest_bytes(data_addr, reply)?;
        }
        self.write_guest_u32(seq_addr, seq)
    }

    /// Read a guest physical word over the control channels (QMP `xp`, gdbstub
    /// fallback).
    ///
    /// Diagnostic / test-support accessor: the mailbox integration tests use
    /// it to play the firmware's half of the RAM-mailbox contract (no
    /// mailbox-v2-aware firmware image ships in the repo yet, and this machine
    /// carries no Xtensa cross-toolchain to build one).
    ///
    /// # Errors
    ///
    /// When neither QMP nor the gdbstub can read the word.
    pub fn debug_read_u32(&mut self, addr: u32) -> Result<u32> {
        self.read_guest_u32(addr)
    }

    /// Whether the firmware has raised the mailbox MAGIC handshake, i.e. the
    /// backend has switched from boot-floored run windows to exact ones.
    /// Diagnostic / test-support accessor.
    #[must_use]
    pub const fn boot_complete(&self) -> bool {
        self.boot_complete
    }

    /// Which live GPIO output observation path this backend selected.
    #[must_use]
    pub const fn gpio_output_observation(&self) -> GpioOutputObservation {
        self.gpio_output_observation
    }

    /// Whether the running QEMU retains the GPIO-matrix output-select words,
    /// so [`Mcu::pins_routed_to_peripheral`] is authoritative (an empty list
    /// then means no peripheral output is routed, not "cannot tell").
    #[must_use]
    pub const fn func_out_sel_observable(&self) -> bool {
        self.func_out_sel_observable
    }

    /// Whether the running QEMU retains the `IO_MUX` and RTC IO blocks, so a
    /// pad's direct-function or RTC ownership is decoded, not assumed.
    #[must_use]
    pub const fn pad_mux_observable(&self) -> bool {
        self.pad_mux_observable
    }

    /// Write a guest physical word (gdbstub `M` packet). See [`Self::debug_read_u32`].
    ///
    /// # Errors
    ///
    /// When no gdbstub is attached or the write is refused.
    pub fn debug_write_u32(&mut self, addr: u32, val: u32) -> Result<()> {
        self.write_guest_u32(addr, val)
    }

    /// Write guest physical bytes (gdbstub `M` packet). See [`Self::debug_read_u32`].
    ///
    /// # Errors
    ///
    /// When no gdbstub is attached or the write is refused.
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

/// A QMP run-control result with a dead child reported as the cause: the
/// control socket failing almost always means QEMU exited, and its exit
/// status plus stderr explain far more than a bare socket error.
fn checked_control<T>(process: &mut QemuProcess, what: &str, result: Result<T>) -> Result<T> {
    let e = match result {
        Ok(value) => return Ok(value),
        Err(e) => e,
    };
    match process.ensure_running(&format!("servicing QMP {what}")) {
        Err(death) => Err(death).context(format!("QMP {what} failed: {e:#}")),
        Ok(()) => Err(e).context(format!("qmp {what}")),
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

/// Why an ESP32 watchdog cannot bite here, stated once.
///
/// Backend-wide rather than per-descriptor: the reason is how this co-simulator
/// drives QEMU, not anything a part declares. A `const` and not a string built
/// inside [`Mcu::watchdog_limitation`] so that a static reader, `hauksbee models
/// lint` in particular, can quote the same sentence a run reports rather than
/// printing a second wording of it or claiming a fidelity this family does not
/// have.
pub const WATCHDOG_LIMITATION: &str =
    "The ESP32 timer-group watchdogs are disabled in this co-simulator, because \
     pausing the guest at every chunk boundary would otherwise trip them. An \
     unserviced task or interrupt watchdog will NOT reboot the firmware here, \
     however long it is starved, so watchdog recovery is untested on this run.";

/// Why ESP32 virtual time is approximate here, stated once.
///
/// Backend-wide for the same reason as [`WATCHDOG_LIMITATION`]: the cause is
/// how this co-simulator drives QEMU (no icount; see the module notes),
/// not anything a descriptor declares. Each chunk credits the host span from
/// sending `cont` to sending `stop` (`Qmp::run_for`), so the control-channel
/// slack is credited and the post-freeze vCPU-parking wait is not; what remains systematic is TCG pace
/// itself: instruction-bound work (a boot) gets as far per credited
/// millisecond as the host's TCG speed allows. Measured on the same firmware
/// and board (the Watchy v1.5 RES drive), 384-496 ms on an M-series Mac and
/// 704-760 ms on an x86-64 CI runner, so about 2x across hosts, not the "few
/// percent" an earlier wording claimed. A `const` so `hauksbee models lint`
/// quotes the same sentence a run reports.
pub const TIMING_LIMITATION: &str =
    "ESP32 virtual time is paced by the host wall clock in this co-simulator \
     (QEMU runs without icount), so how much firmware runs in a simulated \
     millisecond depends on the host's speed and load: the same boot has \
     measured about 2x apart on two machines (384 ms vs 760 ms). Times on \
     this core are approximate, so a deadline or time bound judged against \
     them is reported as conditional, never as a hard PASS or FAIL.";

impl Mcu for QemuBackend {
    fn load_firmware(&mut self, _path: &Path) -> Result<()> {
        // QEMU boots from the flash image given at spawn; there is no separate
        // load step. The image was validated in `new`. This is a no-op so the
        // scheduler's "load firmware after instantiate" flow stays uniform.
        Ok(())
    }

    #[allow(clippy::cast_precision_loss)] // cycle counts and clock rates are far below 2^53
    fn run_cycles(&mut self, n: u64) -> Result<u64> {
        let seconds = n as f64 / self.config.frequency_hz as f64;
        // Return what was CREDITED, not what was requested: with the
        // measured cont→stop crediting the two differ by the control
        // slack (and by the boot floor/cap while booting), and the trait
        // promises "cycles actually executed". The delta is what keeps this
        // return consistent with `current_cycle()` after the call.
        let before = self.core.cycles;
        self.run_seconds(seconds)?;
        Ok(self.core.cycles - before)
    }

    poll_state_mcu_methods!(core);

    /// Backend-wide, not per-part: the ESP32 family's timer-group watchdogs are
    /// disabled by a `-global` on the QEMU command line
    /// (`process.rs`, `wdt_disable=true`), so it is a property of how this
    /// backend launches QEMU rather than of the descriptor.
    ///
    /// Disabling them is deliberate: co-simulation pauses the guest at every
    /// chunk boundary while the analog side solves, and a running timer-group
    /// watchdog would read those pauses as a hung firmware and reset a core
    /// that is doing nothing wrong. Surfaced as data so a user reading a green
    /// report sees the trade.
    fn watchdog_limitation(&self) -> Option<String> {
        Some(WATCHDOG_LIMITATION.to_string())
    }

    /// Backend-wide like the watchdog statement: wall-clock pacing is how this
    /// backend drives QEMU, not a per-part property. See [`TIMING_LIMITATION`].
    fn timing_limitation(&self) -> Option<String> {
        Some(TIMING_LIMITATION.to_string())
    }

    /// Always: each chunk credits the host span from QMP `cont` to `stop`
    /// (see `run_seconds`), so instruction-bound firmware
    /// progress per simulated millisecond is the host's TCG speed.
    fn clock_host_paced(&self) -> bool {
        true
    }

    /// No symbol table reaches QEMU (it boots a flash image), so "entry" here
    /// is any code outside the mask ROM: a PC sample in the image's
    /// bootloader or app. The ROM prints its own boot log on UART0, so the
    /// probe marks the console as shared and UART bytes alone prove nothing.
    fn execution_probe(&mut self) -> crate::traits::ExecutionProbe {
        let mut probe = crate::traits::ExecutionProbe {
            reached_entry: (self.console.firmware_pcs > 0).then_some(true),
            entry_symbol: Some("code outside the boot ROM".to_string()),
            faults: self.console.faults(),
            boot_rom_console: true,
            ..crate::traits::ExecutionProbe::default()
        };
        self.pcs.fill(&mut probe);
        probe
    }

    fn pins_configured_output(&self) -> Vec<PinId> {
        if self.gpio_output_observation != GpioOutputObservation::PeripheralRegisters {
            return Vec::new();
        }
        let mut pins = Vec::new();
        for bank in &self.config.banks {
            let enabled = *self.last_enable.get(&bank.letter).unwrap_or(&0);
            for bit in 0..bank.width {
                if enabled & (1u32 << bit) != 0 {
                    pins.push(PinId {
                        port: bank.letter,
                        bit,
                    });
                }
            }
        }
        pins
    }

    fn drive_direction_observable(&self) -> bool {
        self.gpio_output_observation == GpioOutputObservation::PeripheralRegisters
    }

    /// Pads with a GPIO-matrix peripheral output routed to them and enabled,
    /// from the polled `GPIO_FUNCn_OUT_SEL_CFG` words. Empty on a QEMU build
    /// that does not retain those words (see `func_out_sel_observable`).
    fn pins_routed_to_peripheral(&self) -> Vec<PinId> {
        let mut pins = Vec::new();
        for bank in &self.config.banks {
            let routed = self.routed_pads.get(&bank.letter).copied().unwrap_or(0);
            pins.extend(
                (0..bank.width.min(32))
                    .filter(|bit| routed & (1 << bit) != 0)
                    .map(|bit| PinId {
                        port: bank.letter,
                        bit,
                    }),
            );
        }
        pins
    }

    /// See `routing_blind_spot`.
    fn peripheral_routing_unobserved(&self, pin: PinId) -> Option<String> {
        let memory_bus = self.memory_bus_now.get(&pin.port).copied().unwrap_or(0);
        // A bank never polled has shown nothing, so every pad stays unruled-out.
        let foreign = self
            .foreign_pads
            .get(&pin.port)
            .copied()
            .unwrap_or(u32::MAX);
        let seen = (self.func_out_sel_observable, self.pad_mux_observable);
        routing_blind_spot(&self.config, seen, memory_bus, foreign, pin).map(str::to_string)
    }

    fn pin_can_drive(&self, pin: PinId) -> bool {
        !self
            .config
            .input_only
            .iter()
            .any(|p| p.letter == pin.port && p.bits.contains(&pin.bit))
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
        // so injection rides the RAM mailbox: the modeled voltage is
        // converted to a 12-bit count and written into the channel's mailbox
        // slot each chunk, with the channel's ADC_MASK bit set. Firmware reads
        // the count from the slot rather than a real peripheral, a FIRMWARE
        // CONTRACT, stated as such in [`mailbox`], retired per function the
        // day the fork models the peripheral.
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

    fn on_i2c(&mut self, cb: I2cCb) {
        // The Espressif QEMU controller exposes no host-byte hook for its RX
        // FIFO, so byte events cannot be intercepted from the emulated I2C
        // controller the way simavr/Renode do. Two paths coexist instead:
        //
        //   1. This callback services the RAM-mailbox I2C cell, surfacing a
        //      participating firmware's transactions as the same
        //      Start/Write/Read/Stop events so the engine's I2C slave models
        //      answer uniformly across backends. Coarse (one transaction per
        //      chunk) and gated on BUS_MAGIC, so a non-participating firmware
        //      costs nothing.
        //   2. Temperature sensors are ALSO pushed into QEMU's own emulated I2C
        //      device (the machine ships a tmp105 at 0x48 on i2c0) via
        //      `set_i2c_device_temperature`, which unmodified vendor firmware
        //      reads through its real I2C controller. Preferred wherever such a
        //      device exists: a real peripheral emulation retires a mailbox slot.
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

    fn on_spi(&mut self, cb: SpiCb) {
        // Espressif QEMU exposes no host hook for GPSPI transfers, so SPI byte
        // events ride the RAM-mailbox SPI cell: a firmware
        // participating in the mailbox v2 contract submits transaction-level
        // bursts and each byte is surfaced through this callback (MISO bytes
        // return via the response cell). Gated on BUS_MAGIC; unmodified vendor
        // firmware driving the real (unmodeled) SPI controller is untouched,
        // and, honestly, unserved until the fork grows a peripheral hook.
        self.on_spi = Some(cb);
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
        self.core.active_ports = Some(known);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpio_change_mask_respects_real_drive_enable() {
        assert_eq!(
            observable_gpio_changes(0, 0, 0, 1 << 4),
            1 << 4,
            "newly enabling a LOW output must publish its driven level"
        );
        assert_eq!(
            observable_gpio_changes(0, 1 << 2, 0, 1 << 2),
            1 << 2,
            "newly enabling a HIGH output must publish its driven level"
        );
        assert_eq!(
            observable_gpio_changes(0, 1 << 7, 0, 0),
            0,
            "an input pin's OUT latch is not a driven edge"
        );
    }

    #[test]
    fn mailbox_level_diff_does_not_fabricate_enable_edges() {
        assert_eq!(observable_gpio_changes(0, 0, u32::MAX, u32::MAX), 0);
        assert_eq!(
            observable_gpio_changes(0, 1 << 4, u32::MAX, u32::MAX),
            1 << 4
        );
    }

    #[test]
    fn esp32_config_shape() {
        let c = QemuConfig::esp32();
        assert_eq!(c.machine, "esp32");
        assert_eq!(c.arch, QemuArch::Xtensa);
        assert_eq!(c.banks.len(), 1);
        assert_eq!(c.gpio_qom_path, "/machine/soc/gpio");
        assert_eq!(c.banks[0].peripheral_out_reg, 0x3ff4_4004);
        assert_eq!(c.banks[0].peripheral_enable_reg, 0x3ff4_4020);
        // GPIO_FUNC0_OUT_SEL_CFG_REG: ESP32 TRM v5.8 table 6.12-1, ESP-IDF
        // gpio_reg.h (DR_REG_GPIO_BASE 0x3FF44000 + 0x530).
        assert_eq!(c.banks[0].peripheral_func_out_sel_reg, Some(0x3ff4_4530));
        // The old mailbox remains an explicit fallback for unpatched QEMU.
        assert_eq!(c.banks[0].out_reg, mailbox::GPIO_OUT);
        assert_eq!(c.banks[0].in_reg, mailbox::GPIO_IN);
    }

    /// The ESP32-S3 and ESP32-C3 matrix blocks start at 0x554, not 0x530, and
    /// the patched model only retains the classic layout, so those parts name
    /// no block and are never asked.
    #[test]
    fn only_the_classic_esp32_names_a_matrix_block() {
        assert_eq!(
            QemuConfig::esp32s3().banks[0].peripheral_func_out_sel_reg,
            None
        );
        assert_eq!(
            QemuConfig::esp32c3().banks[0].peripheral_func_out_sel_reg,
            None
        );
    }

    /// An LEDC-style routing: pad 5 carries peripheral signal 71 (the ESP-IDF
    /// `LEDC_HS_SIG_OUT0_IDX`) with its enable taken from the peripheral
    /// (`OEN_SEL` clear), and `GPIO_ENABLE` is never set. The pad is routed.
    #[test]
    fn a_peripheral_signal_routed_to_a_pad_is_reported_without_gpio_enable() {
        let mut words = [FUNC_OUT_SEL_GPIO; 32];
        words[5] = 71;
        assert_eq!(peripheral_routed_pads(&words, 0), 1 << 5);
    }

    /// The boot ROM's flash attach leaves SPIQ/SPID/SPIHD/SPIWP/SPICS0 (1-5)
    /// on pads 7-11; on a package with its flash elsewhere those pads are
    /// free GPIOs the firmware may never touch, so they must not read as
    /// firmware-driven.
    #[test]
    fn the_boot_roms_flash_bus_routing_is_not_firmware_drive() {
        let mut words = [FUNC_OUT_SEL_GPIO; 32];
        for (pad, sig) in [(7, 1), (8, 2), (9, 3), (10, 4), (11, 5), (16, 6), (17, 130)] {
            words[pad] = sig;
        }
        assert_eq!(peripheral_routed_pads(&words, 0), 0);
        words[9] = 71;
        assert_eq!(peripheral_routed_pads(&words, 0), 1 << 9);
    }

    /// The boot ROM's flash attach state on the classic ESP32: pads 7-11 on
    /// SPIQ..SPICS0 with `GPIO_ENABLE` set and `GPIO_OUT` 0. Only the
    /// fixture's own pads (2, 4) may count as plain GPIO outputs.
    #[test]
    fn only_pads_selecting_gpio_out_are_plain_outputs() {
        let mut words = [FUNC_OUT_SEL_GPIO; 32];
        for (pad, sig) in [(7, 1), (8, 2), (9, 3), (10, 4), (11, 5)] {
            words[pad] = sig;
        }
        let enable = 0xF94; // pads 2, 4 and 7-11, read from the live fixture
        assert_eq!(enable & plain_gpio_pads(&words), 1 << 2 | 1 << 4);
        assert_eq!(plain_gpio_pads(&words[..4]), u32::MAX);
    }

    #[test]
    fn memory_bus_pads_are_the_flash_attach() {
        let mut words = [FUNC_OUT_SEL_GPIO; 32];
        for (pad, sig) in [(6, 0), (9, 3), (17, 130), (5, 71)] {
            words[pad] = sig;
        }
        assert_eq!(memory_bus_pads(&words), 1 << 6 | 1 << 9 | 1 << 17);
    }

    /// `IO_MUX` words with every pad's `MCU_SEL` at `sel`, and the RTC IO
    /// words at their reset values (ESP-IDF `rtc_io_reg.h` defaults: DRV 2 on
    /// the DAC pads, the touch DAC/DRV fields; nothing routed to RTC).
    fn pad_blocks(sel: u32) -> (Vec<u32>, Vec<u32>) {
        let rtc = [
            0x8000_0000,
            0x8000_0000,
            0x8410_0010,
            0x6600_0000,
            0x5200_0000,
        ];
        let mut rtc_io = rtc.to_vec();
        rtc_io.extend([0x4a00_0000; 7]);
        (vec![sel << 12 | 0xa00; IO_MUX_WORDS], rtc_io)
    }

    /// `MCU_SEL` 2 is the GPIO matrix everywhere; 0 is GPIO only where Function
    /// 0 is (TRM Table 6.10-1), so the reset state leaves U0TXD (1), U0RXD (3),
    /// the SD pads (6-11) and JTAG (12-15) on their direct functions.
    #[test]
    fn mcu_sel_decides_whether_the_gpio_matrix_owns_a_pad() {
        let (mut mux, rtc) = pad_blocks(2);
        assert_eq!(foreign_pads(&mux, &rtc), 0);
        let (reset, _) = pad_blocks(0);
        assert_eq!(foreign_pads(&reset, &rtc), 1 << 1 | 1 << 3 | 0xFFC0);
        mux[0x48 / 4] = 1 << 12 | 0xa00; // GPIO4 on HSPIHD (Function 1)
        mux[0x88 / 4] = 0; // U0TXD back on its reset UART function
        assert_eq!(foreign_pads(&mux, &rtc), 1 << 4 | 1 << 1);
        // A short read never decodes as GPIO-owned.
        assert_eq!(foreign_pads(&mux[..0x48 / 4], &rtc) & 1 << 4, 1 << 4);
    }

    /// The RTC side takes a pad by `MUX_SEL` (RTC GPIO, ULP, RTC I2C) or by a
    /// powered DAC (`XPD_DAC`, `DAC_XPD_FORCE`) or touch channel (`XPD`).
    #[test]
    fn rtc_mux_dac_and_touch_take_a_pad_off_the_gpio_matrix() {
        let (mux, reset) = pad_blocks(2);
        for (word, bit, pad) in [
            (0, 18, 25),
            (0, 10, 25),
            (1, 17, 26),
            (4, 19, 4),
            (11, 20, 27),
        ] {
            let mut rtc = reset.clone();
            rtc[word] |= 1 << bit;
            assert_eq!(foreign_pads(&mux, &rtc), 1 << pad, "word {word} bit {bit}");
        }
        assert_eq!(foreign_pads(&mux, &reset[..4]), 0x0800_F015); // touch words unread
    }

    /// With the matrix and both pad blocks seen, only a pad a direct function
    /// or the RTC side owned stays "may be driven"; a build without the pad
    /// blocks still names them; GPIO32 (`RTC_GPIO9`, `TOUCH9`) sits outside the
    /// polled bank; the flash attach is always clear.
    #[test]
    fn io_mux_and_rtc_io_drives_are_ruled_out_only_when_seen() {
        let c = QemuConfig::esp32();
        let pin = |port, bit| PinId { port, bit };
        let foreign = 1 << 1 | 1 << 25;
        assert_eq!(
            routing_blind_spot(&c, (true, true), 0, foreign, pin('0', 4)),
            None
        );
        for bit in [1, 25] {
            let why = routing_blind_spot(&c, (true, true), 0, foreign, pin('0', bit)).unwrap();
            assert!(why.contains("IO_MUX selects a direct function"), "{why}");
        }
        for bit in [1, 4, 25] {
            let why = routing_blind_spot(&c, (true, false), 0, 0, pin('0', bit)).unwrap();
            assert!(why.contains("IO_MUX") && why.contains("DAC"), "{why}");
            let unpatched = routing_blind_spot(&c, (false, true), 0, 0, pin('0', bit)).unwrap();
            assert!(unpatched.contains("retains neither"), "{unpatched}");
        }
        assert_eq!(
            routing_blind_spot(&c, (true, true), 1 << 9, u32::MAX, pin('0', 9)),
            None
        );
        let outside = routing_blind_spot(&c, (true, true), u32::MAX, 0, pin('1', 0)).unwrap();
        assert!(outside.contains("outside the GPIO banks"), "{outside}");
        // The S3 names no matrix or pad blocks, so even a capable build is blind.
        let s3 = routing_blind_spot(&QemuConfig::esp32s3(), (true, true), 0, 0, pin('0', 4));
        assert!(s3.unwrap().contains("retains neither"));
    }

    #[test]
    fn plain_gpio_selections_are_not_routed() {
        // Reset state and an IDF-configured plain output: OUT_SEL = 256, with
        // and without the OEN_SEL / inversion bits.
        let mut words = [FUNC_OUT_SEL_GPIO; 32];
        words[2] = FUNC_OUT_SEL_GPIO | FUNC_OEN_SEL;
        words[4] = FUNC_OUT_SEL_GPIO | (1 << 9) | (1 << 11);
        assert_eq!(peripheral_routed_pads(&words, 1 << 2), 0);
    }

    #[test]
    fn a_routed_pad_forced_to_the_gpio_enable_and_left_disabled_is_high_impedance() {
        let mut words = [FUNC_OUT_SEL_GPIO; 32];
        // Signal 14 routed, enable forced from GPIO_ENABLE (OEN_SEL set).
        words[7] = 14 | FUNC_OEN_SEL;
        assert_eq!(peripheral_routed_pads(&words, 0), 0, "enable bit clear");
        assert_eq!(peripheral_routed_pads(&words, 1 << 7), 1 << 7, "enabled");
        // Signal 14 routed with a peripheral-owned enable: driven regardless.
        words[7] = 14;
        assert_eq!(peripheral_routed_pads(&words, 0), 1 << 7);
    }

    #[test]
    fn a_zero_out_sel_is_the_flash_clock_not_firmware_drive() {
        // OUT_SEL 0 is peripheral signal 0 (SPICLK_out), a memory-bus signal,
        // so even a build that reads back zeros marks no pad; the capability
        // probe still gates the whole block, since zeros prove nothing.
        let words = [0u32; 32];
        assert_eq!(peripheral_routed_pads(&words, 0), 0);
    }

    /// A pad the firmware moved from `GPIO_OUT` to a peripheral output keeps
    /// its stale `GPIO_OUT` bit; no edge may stamp that latch as its level.
    /// Handed back to `GPIO_OUT`, an enabled pad republishes its latch.
    #[test]
    fn a_routed_pad_publishes_no_edge_from_its_stale_latch() {
        let both = 1 << 5 | 1 << 2;
        let mut words = [FUNC_OUT_SEL_GPIO; 32];
        words[5] = 71; // LEDC on pad 5, GPIO_OUT bit 5 left set
        let enable = both & plain_gpio_pads(&words);
        assert_eq!(observable_gpio_changes(0, both, 0, enable), 1 << 2);
        // Pad 5 handed back to GPIO_OUT with nothing else changing: its
        // enable rises in the masked word, so its latch is republished.
        let back = both & plain_gpio_pads(&[FUNC_OUT_SEL_GPIO; 32]);
        assert_eq!(observable_gpio_changes(both, both, enable, back), 1 << 5);
    }

    /// GPIO34..39 have no output driver (ESP-IDF `soc_caps.h`: "GPIO >= 34
    /// are input only"), carried as data in the descriptor at port '1' bits
    /// 2..7; the S3 and C3 descriptors mark none.
    #[test]
    fn the_descriptor_marks_input_only_pads() {
        let only = &QemuConfig::esp32().input_only;
        assert_eq!(
            only,
            &[InputOnlyPins {
                letter: '1',
                bits: vec![2, 3, 4, 5, 6, 7]
            }]
        );
        assert!(QemuConfig::esp32s3().input_only.is_empty());
        assert!(QemuConfig::esp32c3().input_only.is_empty());
    }

    #[test]
    fn matrix_words_beyond_the_enable_word_are_ignored() {
        let mut words = [FUNC_OUT_SEL_GPIO; 40];
        words[33] = 71;
        assert_eq!(peripheral_routed_pads(&words, 0), 0);
    }

    #[test]
    fn esp32c3_uses_riscv() {
        let c = QemuConfig::esp32c3();
        assert_eq!(c.arch, QemuArch::Riscv32);
        assert_eq!(c.machine, "esp32c3");
        assert_eq!(c.gpio_qom_path, "/machine/gpio");
        assert_eq!(c.banks[0].peripheral_out_reg, 0x6000_4004);
        assert_eq!(c.banks[0].peripheral_enable_reg, 0x6000_4020);
        assert_eq!(c.banks[0].out_reg, mailbox::GPIO_OUT);
    }

    /// Bit-identity proof for the data-driven config bridge: every
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
        const { assert!(I2C_RSP_DATA + BUS_DATA_MAX <= SPI_REQ_SEQ) };
        // SPI cell: request data ends exactly at the response seq; the whole
        // mailbox stays comfortably inside the 8 KiB RTC slow memory.
        assert_eq!(SPI_REQ_DATA + BUS_DATA_MAX, SPI_RSP_SEQ);
        const { assert!(SPI_RSP_DATA + BUS_DATA_MAX <= BASE + 0x2000) };
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
