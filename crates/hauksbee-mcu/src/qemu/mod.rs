//! Espressif-QEMU-backed MCU emulation for the ESP32 family.
//!
//! [`QemuBackend`] drives a headless Espressif QEMU process (the fork with full
//! ESP32 SoC peripheral models) over two TCP control channels plus a UART
//! socket, exposing the same generic [`Mcu`](crate::traits::Mcu) trait the
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
//! docs/MCU.md for the per-part status.)
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
//! reproducibility (this is how the Renode-vs-QEMU note in docs/MCU.md framed
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
//! in docs/MCU.md.

mod gdb;
mod process;
mod qmp;
mod uart;

pub use process::{is_available, QemuArch};

use crate::traits::{I2cEvent, Mcu, McuState, PinId, SpiEvent};
use anyhow::{bail, Context, Result};
use gdb::GdbStub;
use process::QemuProcess;
use qmp::Qmp;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;
use uart::UartSocket;

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
}

/// One GPIO bank's observation addresses, generic over ESP32 family member.
///
/// The addresses are the RAM mailbox words (see [`mailbox`]), not the GPIO
/// peripheral registers, because the QEMU fork's gpio model does not expose
/// register read-back. The bit layout still matches GPIO_OUT_REG.
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
}

impl QemuConfig {
    /// Classic ESP32 (Xtensa LX6 dual-core), 240 MHz, single 32-bit GPIO bank
    /// for pins 0..31 (the demo and most products use only low pins). GPIO is
    /// observed through the RAM mailbox (the gpio model has no register
    /// read-back).
    pub fn esp32() -> Self {
        QemuConfig {
            arch: QemuArch::Xtensa,
            machine: "esp32".to_string(),
            banks: vec![GpioBank {
                letter: '0',
                out_reg: mailbox::GPIO_OUT,
                in_reg: mailbox::GPIO_IN,
                width: 32,
            }],
            icount_shift: 2,
            frequency_hz: 240_000_000,
        }
    }

    /// ESP32-S3 (Xtensa LX7). Same mailbox layout as classic ESP32.
    pub fn esp32s3() -> Self {
        QemuConfig {
            arch: QemuArch::Xtensa,
            machine: "esp32s3".to_string(),
            banks: vec![GpioBank {
                letter: '0',
                out_reg: mailbox::GPIO_OUT,
                in_reg: mailbox::GPIO_IN,
                width: 32,
            }],
            icount_shift: 2,
            frequency_hz: 240_000_000,
        }
    }

    /// ESP32-C3 (RISC-V RV32IMC), 160 MHz. Same RAM mailbox layout.
    pub fn esp32c3() -> Self {
        QemuConfig {
            arch: QemuArch::Riscv32,
            machine: "esp32c3".to_string(),
            banks: vec![GpioBank {
                letter: '0',
                out_reg: mailbox::GPIO_OUT,
                in_reg: mailbox::GPIO_IN,
                width: 32,
            }],
            icount_shift: 2,
            frequency_hz: 160_000_000,
        }
    }
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
    on_pin_change: Option<Box<dyn FnMut(PinId, bool) + Send>>,
    on_uart: Option<Box<dyn FnMut(u8) + Send>>,
    firmware_loaded: bool,
    /// Virtual time advanced so far, in cycles-equivalent.
    cycles: u64,
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
            cycles: 0,
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
        for bank in &banks {
            let new = self.read_out(bank);
            let prev = *self.last_out.get(&bank.letter).unwrap_or(&0);
            if new != prev {
                let changed = new ^ prev;
                if let Some(cb) = &mut self.on_pin_change {
                    for bit in 0..bank.width {
                        if (changed >> bit) & 1 != 0 {
                            let high = (new >> bit) & 1 != 0;
                            cb(PinId { port: bank.letter, bit }, high);
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
    /// (`cont`), hold a bounded wall-time window during which the esp32 machine
    /// advances its virtual clock at roughly wall rate, pause (`stop`), then read
    /// the GPIO mailbox and drain UART. The window is the requested virtual
    /// interval with a floor so the guest makes real progress each chunk (the
    /// esp32 boot ROM + 2nd-stage bootloader take ~1-2 s of virtual time to reach
    /// app_main, so the early chunks must let it run). This mirrors Renode's
    /// `RunFor`, which also blocks for the interval, with the difference that the
    /// pace is wall-bounded rather than instruction-counted.
    fn run_seconds(&mut self, seconds: f64) -> Result<()> {
        if !self.firmware_loaded {
            bail!("no firmware image booted in the QEMU machine");
        }
        self.qmp.cont().context("qmp cont")?;
        // Window: the requested virtual interval, floored at 8 ms so a chunk
        // always advances the guest enough to clear boot within a reasonable
        // number of chunks, and capped so a large step does not stall the co-sim.
        let window_ms = ((seconds * 1000.0).max(8.0)).min(50.0) as u64;
        std::thread::sleep(Duration::from_millis(window_ms));
        self.qmp.stop().context("qmp stop")?;

        self.cycles += (seconds * self.config.frequency_hz as f64).round() as u64;

        self.poll_gpio_edges();
        self.pump_uart_out();
        Ok(())
    }
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
        if let Some(bank) = self.config.banks.iter().find(|b| b.letter == pin.port).cloned() {
            let cur = *self.in_shadow.get(&bank.letter).unwrap_or(&0);
            let next = if high {
                cur | (1 << pin.bit)
            } else {
                cur & !(1 << pin.bit)
            };
            if next != cur {
                if let Some(g) = self.gdb.as_mut() {
                    if g.write_u32(bank.in_reg, next).is_ok() {
                        self.in_shadow.insert(bank.letter, next);
                    }
                }
            }
        }
    }

    fn set_analog_in(&mut self, _channel: u8, _volts: f64) {
        // ESP32 ADC (SAR ADC) is not modelled by the QEMU fork's peripheral set,
        // so ADC injection is a documented no-op (matching the Renode backend).
        // The demo couples through the GPIO/LED path, not the ADC.
    }

    fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool) + Send>) {
        self.on_pin_change = Some(cb);
    }

    fn uart_write(&mut self, bytes: &[u8]) {
        let _ = self.uart.write_bytes(bytes);
    }

    fn on_uart(&mut self, cb: Box<dyn FnMut(u8) + Send>) {
        self.on_uart = Some(cb);
    }

    fn on_i2c(&mut self, _cb: Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>) {
        // I2C interception is not wired for the QEMU backend (as for Renode).
    }

    fn on_spi(&mut self, _cb: Box<dyn FnMut(SpiEvent) -> u8 + Send>) {
        // SPI interception is not wired for the QEMU backend.
    }

    fn state(&self) -> McuState {
        McuState {
            pc: 0,
            cycles: self.cycles,
            sleeping: false,
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
        self.active_ports = Some(known);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn mailbox_layout() {
        assert_eq!(mailbox::GPIO_OUT, 0x5000_0000);
        assert_eq!(mailbox::GPIO_IN, 0x5000_0004);
        assert_eq!(mailbox::MAGIC, 0x5000_0008);
        assert_eq!(mailbox::MAGIC_VALUE, 0x6A6C_6E69);
    }

    #[test]
    fn ports_triple_distinct() {
        let (a, b, c) = free_port_triple().unwrap();
        assert!(a != b && b != c && a != c);
    }
}
