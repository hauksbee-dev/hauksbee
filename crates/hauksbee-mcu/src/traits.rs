//! The MCU abstraction: the lockstep contract between firmware and the analog
//! engine that every backend (native AVR, QEMU, Renode) implements.
//!
//! The core is the [`Mcu`] trait. A co-sim run advances firmware in bounded
//! chunks ([`Mcu::run_cycles`]/[`run_micros`](Mcu::run_micros)), couples pins in
//! both directions ([`Mcu::set_digital_in`]/[`set_analog_in`](Mcu::set_analog_in)
//! drive the firmware; [`Mcu::on_pin_change`] reports GPIO output edges back with
//! the MCU cycle at which they happened), and reads execution state
//! ([`McuState`], [`Mcu::frequency`]). The cycle stamp on each reported edge is
//! what lets the analog side replay a sub-microsecond bit-banged burst in the
//! exact order the firmware produced it; [`Mcu::cycle_exact`] tells the caller
//! whether those stamps are true edge cycles (push backends) or coarse
//! slice-boundary times (poll backends). This file also defines the small value
//! types the coupling speaks in: [`PinId`], [`McuState`], and the intercepted
//! bus events [`I2cEvent`]/[`SpiEvent`].
//!

use anyhow::{bail, Result};
use std::path::Path;

/// Identifies a single GPIO pin by port letter and bit index.
///
/// Example: `PinId { port: 'B', bit: 5 }` is Arduino D13 / ATmega328P PB5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PinId {
    /// Port letter: 'A', 'B', 'C', 'D', …
    pub port: char,
    /// Bit index within the port register (0-7).
    pub bit: u8,
}

impl PinId {
    /// Construct a pin identifier from a port letter and bit number.
    pub fn new(port: char, bit: u8) -> Self {
        Self { port, bit }
    }
}

/// One synchronous external-input ownership update.
///
/// `Drive` persists until the same responder emits `Release`; release is a
/// digital high-impedance handoff, not a fabricated HIGH level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PinDrive {
    Drive { pin: PinId, high: bool },
    Release { pin: PinId },
}

impl PinDrive {
    pub fn drive(pin: PinId, high: bool) -> Self {
        Self::Drive { pin, high }
    }

    pub fn release(pin: PinId) -> Self {
        Self::Release { pin }
    }

    pub fn pin(self) -> PinId {
        match self {
            Self::Drive { pin, .. } | Self::Release { pin } => pin,
        }
    }
}

/// Snapshot of MCU execution state returned by [`Mcu::state`].
#[derive(Debug, Clone)]
pub struct McuState {
    /// Program counter (byte address in flash).
    pub pc: u32,
    /// Total cycle count since reset.
    pub cycles: u64,
    /// True if the MCU is in a sleep / idle state awaiting an interrupt.
    pub sleeping: bool,
    /// True once the core has terminated cleanly and will make no further
    /// progress (simavr's `cpu_Done`, e.g. a `sleep` with interrupts disabled).
    pub done: bool,
    /// True once the core has crashed and will make no further progress
    /// (simavr's `cpu_Crashed`: illegal opcode, out-of-RAM write, stack death).
    /// Reported explicitly because breaking out of the step loop on its own
    /// still returns a clean `Ok`, which leaves a crashed MCU
    /// indistinguishable from a healthy chunk. Backends that cannot detect a
    /// crash leave it false.
    pub crashed: bool,
}

/// An event on the I2C (TWI) bus, as seen from the perspective of an
/// external peripheral intercepting firmware writes.
#[derive(Debug, Clone)]
pub enum I2cEvent {
    /// START condition followed by address byte (R/W bit stripped).
    Start {
        /// 7-bit device address.
        addr: u8,
        /// True if the firmware is reading from the peripheral.
        read: bool,
    },
    /// Data byte written by the firmware.
    Write { addr: u8, data: u8 },
    /// The master (firmware) is reading a byte from the peripheral. The
    /// handler's returned byte is clocked back to the firmware. This is the
    /// path that lets a slave reply with register data (e.g. a sensor read);
    /// it was the gap that left I2C slaves write-only.
    Read { addr: u8 },
    /// STOP condition.
    Stop { addr: u8 },
}

/// An event on the SPI bus, as seen from the perspective of an external
/// peripheral intercepting firmware transfers.
#[derive(Debug, Clone)]
pub struct SpiEvent {
    /// Byte clocked out of MOSI.
    pub mosi: u8,
    /// True when this event signals a chip-select deassert (FinishTransmission).
    /// The `mosi` field is meaningless when `deselect` is true; the callback
    /// return value is also ignored. Backends that cannot observe chip-select
    /// leave this false; the `SpiBus.post_solve` deselect path handles those.
    pub deselect: bool,
    /// MCU cycle counter at the instant of this transfer, from the same
    /// `current_cycle()` clock that stamps [`Mcu::on_pin_change`] edges. This is
    /// what lets the scheduler merge the byte stream against the CS-pin edge
    /// stream into ONE cycle-ordered event queue and frame transactions on real
    /// CS assert/deassert (05 §2). On push backends (simavr) it is exact: the C
    /// SPI IRQ fires synchronously inside `avr_run`, so the cycle read in the
    /// hook is the true transfer cycle. On poll backends (Renode/QEMU) it is the
    /// poll-boundary virtual time and coarse, the same tier `Mcu::cycle_exact`
    /// reports for pin edges; framing then falls back to arrival order within the
    /// chunk.
    pub cycle: u64,
}

/// Core trait for an emulated microcontroller.
///
/// Implementors provide cycle-accurate execution and pin-level coupling so
/// that peripheral models (shift registers, DACs, sensors) can be co-simulated
/// alongside the firmware.
pub trait Mcu {
    // ---- Firmware loading ----

    /// Load firmware from a `.hex` (Intel HEX) or `.elf` file.
    fn load_firmware(&mut self, path: &Path) -> Result<()>;

    // ---- Execution ----

    /// Run exactly `n` cycles (or as close as the underlying simulator allows).
    /// Returns the number of cycles actually executed.
    fn run_cycles(&mut self, n: u64) -> Result<u64>;

    /// Run for approximately `us` microseconds.
    fn run_micros(&mut self, us: u64) -> Result<()>;

    /// Run for approximately `ms` milliseconds.
    fn run_millis(&mut self, ms: u64) -> Result<()> {
        self.run_micros(ms * 1000)
    }

    /// Hard-reset the core, as if the board's RESET line were pulsed: PC back
    /// to the reset vector, registers and peripherals to their power-on state,
    /// the loaded firmware kept. This is the recovery path for a wedged
    /// firmware (e.g. one blocked forever on a serial protocol that went out
    /// of sync); a session "Reset" that rewinds sim time but not the core
    /// leaves the wedge in place.
    ///
    /// The default ERRORS rather than silently doing nothing, so a backend
    /// that cannot reboot its core is reported loudly by the caller instead of
    /// pretending the firmware restarted.
    fn reset(&mut self) -> Result<()> {
        bail!("this MCU backend cannot hard-reset its core; firmware state persists")
    }

    /// The MCU's clock frequency in Hz (e.g. 16_000_000 for a 16 MHz Arduino).
    fn frequency(&self) -> u64;

    // ---- Pin coupling ----

    /// Drive an external digital input pin HIGH or LOW.
    fn set_digital_in(&mut self, pin: PinId, high: bool);

    /// Inject an ADC voltage on the given channel (0-indexed, volts).
    ///
    /// The value is passed to simavr in millivolts internally; the trait
    /// surface exposes volts for convenience.
    fn set_analog_in(&mut self, channel: u8, volts: f64);

    /// Register a callback that fires on every GPIO output edge.
    ///
    /// The callback receives the pin, the new logic level, and the MCU cycle
    /// counter at the instant of the edge. It is called synchronously from
    /// within [`Mcu::run_cycles`] / [`Mcu::run_micros`] on the same thread.
    ///
    /// The cycle stamp is what lets the co-sim replay a sub-µs `shiftOut` SCLK
    /// burst in the exact order (and multiplicity) the firmware produced it,
    /// rather than collapsing the whole chunk to a resting level: a pulse train
    /// reduced to one level loses the energy its edges carried, so the edges
    /// have to survive and be integrated. On push backends
    /// (simavr) the stamp is exact: the C IRQ fires synchronously on every edge,
    /// so the cycle read inside the hook is the true edge time. On poll backends
    /// (Renode/QEMU) it is the poll boundary's virtual time and coarse; see
    /// [`Mcu::cycle_exact`].
    fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool, u64) + Send>);

    /// The MCU's current cycle counter (cycles since reset).
    ///
    /// Read synchronously, so calling it from inside an `on_pin_change` hook (as
    /// the simavr backend does) yields the exact cycle of the edge being
    /// reported. The default derives it from [`Mcu::state`]; backends with a
    /// cheaper direct read (simavr's `avr->cycle`) override it.
    fn current_cycle(&self) -> u64 {
        self.state().cycles
    }

    /// Whether this backend's edge cycle stamps are cycle-exact.
    ///
    /// True on push backends (simavr: the C IRQ fires on every edge, so each
    /// stamp is the real edge cycle). False on poll backends (Renode/QEMU diff
    /// the output registers per time slice, so every edge inside a slice shares
    /// the slice's virtual time and intra-slice ordering is lost). Downstream
    /// cadence and framing logic reads this to know whether the drained ordering
    /// can be trusted at sub-slice granularity (05 §1.1, amended: the drain
    /// carries the coarse flag rather than pretending the order is exact).
    fn cycle_exact(&self) -> bool {
        true
    }

    /// Register a synchronous input responder.
    ///
    /// On every GPIO output edge (the same edges `on_pin_change` sees) the
    /// responder is invoked with the pin, its new level, and the exact edge
    /// cycle, and returns a list of input pins to drive, applied *immediately*,
    /// before the firmware's next instruction, within the same `run_micros`. This is the mechanism
    /// that lets a firmware bit-bang a clock and `digitalRead` the resulting
    /// serial-out bit in the SAME tight loop: e.g. a 74HC165 presenting its next
    /// QH bit onto MISO on each SCLK edge. Resolving the response per output edge
    /// (not once per analog chunk) is the read-direction analogue of the
    /// edge-driven 74HC595 write path. New multipin device integrations should
    /// use [`Mcu::on_input_responder_batch`] so one hardware port write is
    /// evaluated atomically rather than in arbitrary bit order.
    ///
    /// The default is a no-op for backends that cannot drive an input pin
    /// synchronously from within their run loop (Renode / QEMU push state once
    /// per chunk).
    fn on_input_responder(
        &mut self,
        _responder: Box<dyn FnMut(PinId, bool, u64) -> Vec<PinDrive> + Send>,
    ) {
    }

    /// Whether [`Mcu::on_input_responder`] is invoked synchronously inside the
    /// run loop and applies its returned pin drives before the next guest
    /// instruction.
    ///
    /// Exact edge timestamps alone do not prove that the optional responder
    /// hook is implemented. The conservative default delegates to the stronger
    /// atomic-batch capability, so an atomic backend opts into both contracts;
    /// a legacy singleton backend must override this method explicitly.
    fn input_responder_synchronous(&self) -> bool {
        self.input_responder_batches_atomic()
    }

    /// Whether [`Mcu::on_input_responder_batch`] reports one complete hardware
    /// GPIO-port update per callback.
    ///
    /// The conservative default is false. Existing backends still receive the
    /// source-compatible singleton adapter below, which is sufficient for
    /// responders that consume one ordered edge at a time, but it cannot make
    /// multi-pin memory gates atomic. Backends may return true only when they
    /// override the batch hook and preserve that hardware update boundary.
    fn input_responder_batches_atomic(&self) -> bool {
        false
    }

    /// Whether direction-register transitions are synchronously reported to
    /// the responder. This is required before a pulled GPIO may participate in
    /// a memory fast path: changing INPUT->OUTPUT changes the physical level
    /// even when the PORT latch itself does not change.
    fn input_responder_tracks_direction(&self) -> bool {
        false
    }

    /// Register a synchronous responder for one atomic GPIO-port update.
    ///
    /// Push backends should report all pins changed by one hardware port write
    /// together so a device can evaluate multi-pin control gates against the
    /// externally observable final state. The default preserves source and
    /// single-edge behavior for existing backends, but does not make them
    /// atomic; see [`Mcu::input_responder_batches_atomic`].
    fn on_input_responder_batch(
        &mut self,
        mut responder: Box<dyn FnMut(&[(PinId, bool)], u64) -> Vec<PinDrive> + Send>,
    ) {
        self.on_input_responder(Box::new(move |pin, high, cycle| {
            responder(&[(pin, high)], cycle)
        }));
    }

    /// Register direction transitions for responder-watched GPIOs. `output`
    /// is the new direction and `port_high` is the current output latch. A
    /// backend advertising [`Mcu::input_responder_tracks_direction`] must call
    /// this synchronously from the same hardware-register write.
    fn on_input_responder_direction(
        &mut self,
        _responder: Box<dyn FnMut(&[(PinId, bool, bool)], u64) -> Vec<PinDrive> + Send>,
    ) {
    }

    // ---- UART ----

    /// Inject bytes into the MCU's UART RX (as if the host sent them).
    ///
    /// The contract is lossless-or-loud: an implementation must either deliver
    /// every byte to the firmware (buffering and metering them at whatever
    /// pace the emulated UART actually accepts; host records are routinely
    /// longer than any hardware RX fifo) or account for genuinely unavoidable
    /// drops in [`Mcu::uart_rx_overflow`]. Silent truncation wedges every
    /// record-based host protocol (the NEP-board study's SEV-1).
    fn uart_write(&mut self, bytes: &[u8]);

    /// Host serial bytes not delivered to firmware. This includes a pending
    /// buffer overflow and an external emulator UART socket/configuration
    /// failure. Zero on a healthy run; a caller must surface a non-zero value
    /// as a loud coverage warning. Default 0 for backends whose transport is
    /// synchronous and lossless.
    fn uart_rx_overflow(&self) -> u64 {
        0
    }

    /// Host serial bytes accepted by this backend but not yet presented to
    /// the emulated UART. A live-session caller uses this at teardown so
    /// queued input cannot be counted as delivered and then discarded with
    /// the MCU. External socket backends retain accepted bytes here until the
    /// next successful emulator advance; synchronous injectors may keep zero.
    fn uart_rx_pending(&self) -> usize {
        0
    }

    /// Register a callback that receives each byte the firmware sends over UART.
    fn on_uart(&mut self, cb: Box<dyn FnMut(u8) + Send>);

    // ---- Peripheral hooks ----

    /// Install a handler for I2C (TWI) bus events.
    ///
    /// The closure receives each [`I2cEvent`] and may return an optional reply
    /// byte (used when the firmware is reading from a peripheral).  Returning
    /// `None` causes simavr to ACK the transfer with no data byte injected.
    fn on_i2c(&mut self, cb: Box<dyn FnMut(I2cEvent) -> Option<u8> + Send>);

    /// Install a handler for SPI bus events.
    ///
    /// The closure receives each [`SpiEvent`] (one per byte transferred) and
    /// may return the MISO byte the peripheral would provide.
    fn on_spi(&mut self, cb: Box<dyn FnMut(SpiEvent) -> u8 + Send>);

    /// Install a handler for SPI bus events on a named controller.
    ///
    /// The default implementation forwards to [`Mcu::on_spi`], which is correct for
    /// single-controller backends (AVR, QEMU). Multi-controller backends (Renode
    /// with multiple SPI controllers) override this to route each controller to
    /// its own bridge/callback, so a slave attached to "spi2" only receives
    /// traffic from the spi2 controller, not from spi3.
    fn on_spi_controller(&mut self, _controller: &str, cb: Box<dyn FnMut(SpiEvent) -> u8 + Send>) {
        self.on_spi(cb);
    }

    // ---- Status ----

    /// Sample the current execution state without advancing the simulation.
    fn state(&self) -> McuState;

    // ---- Optional performance hint ----

    /// Hint which GPIO ports the engine actually wired, so a polling backend
    /// can avoid querying ports no component is attached to.
    ///
    /// The default is a no-op: backends that push edges (e.g. simavr) ignore
    /// this. The Renode backend uses it to read only the relevant ports' output
    /// registers each chunk instead of every port the platform defines.
    fn set_active_ports(&mut self, _ports: &[char]) {}

    /// GPIO pins the firmware ever configured as outputs (DDR/direction set),
    /// regardless of the level driven. Lets a caller tell an output-LOW-held pin
    /// (driven low) from one the firmware never configured (floating). This is
    /// observation metadata, not a drive: it never affects the analog solve.
    ///
    /// Default empty: a backend that cannot report direction state simply makes
    /// every pin look "not configured" (the conservative direction, a
    /// boot-state panel then reports "unknown/never driven" rather than asserting
    /// "driven low").
    fn pins_configured_output(&self) -> Vec<PinId> {
        Vec::new()
    }

    /// Whether this backend can actually observe pin drive DIRECTION, i.e.
    /// whether an empty [`Mcu::pins_configured_output`] means "no pin is an
    /// output" (trustworthy) rather than "I cannot tell" (the conservative
    /// default). A boot-state check uses this to decide whether it may assert
    /// Hi-Z on a pin absent from the configured-output set, or must hedge
    /// ("undriven OR held LOW").
    ///
    /// Default false: a backend that does not override
    /// [`Mcu::pins_configured_output`] returns an empty set, and claiming that
    /// empty set is authoritative would let a held-LOW output masquerade as
    /// floating. The AVR backend overrides true (it reads DDR); the Renode
    /// backend overrides true exactly when every polled port carries a
    /// direction-register descriptor (see `renode::DirMap`).
    fn drive_direction_observable(&self) -> bool {
        false
    }

    // ---- Co-sim coverage honesty ----

    /// ADC channels whose [`Mcu::set_analog_in`] injections were DROPPED because
    /// this backend has no injection recipe for them (e.g. a Renode platform with
    /// no `AdcChannelMap`). The scheduler reads this after a run to surface a
    /// coverage warning on every report surface, a dropped injection means the
    /// firmware never saw the modeled voltage, so results on that pin are
    /// meaningless and must never read as a healthy co-sim.
    ///
    /// Default empty: backends that deliver every injection (simavr) report no
    /// drops. Sorted ascending so the reported order is deterministic.
    fn adc_dropped_channels(&self) -> Vec<u8> {
        Vec::new()
    }

    /// How this backend's watchdog fidelity falls short of the part, or `None`
    /// when an armed watchdog that is never fed reboots the core the way
    /// silicon does.
    ///
    /// This is a coverage warning in exactly the sense
    /// [`Mcu::adc_dropped_channels`] is: the run happened, but one thing the
    /// board would have done did not, so a green result on that path means
    /// less than it looks. A firmware whose watchdog never bites in
    /// co-simulation is a firmware whose watchdog is untested, and the user
    /// has to be told rather than left to infer it from a passing run.
    ///
    /// Measured on the shipped backends:
    ///
    /// - `simavr` reboots at the right virtual time and reports the reboot
    ///   through [`Mcu::watchdog_resets`], so it returns `None`.
    /// - `renode:nrf52840` arms cleanly (RUNSTATUS reads 1, CRV reads back a
    ///   correct 32768 Hz reload) and then never fires: zero resets in 1.000 s
    ///   of simulated time where the part gives twenty.
    /// - `qemu:esp32*` has its timer-group watchdogs disabled at launch on
    ///   purpose, because a paused guest would otherwise be reset between
    ///   chunks. That is the right call for a co-simulator and the wrong thing
    ///   to leave unstated.
    ///
    /// The string is a whole sentence, because it is rendered verbatim on the
    /// four batch report surfaces (`hauksbee run` default text, `--plain`,
    /// `--json`, hauksbee-ci) and in `hauksbee models lint`, and nothing
    /// downstream should have to compose prose from a flag. The TUI renders it;
    /// the synchronous web report does not run the external backends that emit
    /// it. See `docs/cosim/MCU.md`.
    fn watchdog_limitation(&self) -> Option<String> {
        None
    }

    /// How this backend's TIMING fidelity falls short of the part, or `None`
    /// when a firmware delay costs the virtual time it costs on silicon (as
    /// measured by a clock-truth gate, not assumed from a declaration).
    ///
    /// Same coverage-honesty contract as [`Mcu::watchdog_limitation`]: the run
    /// happened, but time-based results on this core carry a known systematic
    /// bias, and a user reading a green time-based assertion has to be told
    /// rather than left to find the caveat in a doc. Two shipped cases:
    ///
    /// - `qemu:esp32*` paces virtual time by the host wall clock (no icount;
    ///   it breaks esp32 boot, measured), so virtual time is approximate and
    ///   host-load dependent even though the cont→stop window itself is now
    ///   measured from QEMU's RESUME/STOP event timestamps.
    /// - `renode:stm32f103` deliberately clocks its TIMx blocks at the
    ///   post-PLL 72 MHz against an 8 MHz reset-default core, so a bare-metal
    ///   TIMx time base runs 9x fast (stated in the descriptor).
    ///
    /// The string is a whole sentence rendered verbatim on the batch surfaces
    /// and TUI. The synchronous web report does not run the external backends
    /// that emit it.
    fn timing_limitation(&self) -> Option<String> {
        None
    }

    /// Times an unserviced watchdog rebooted the core during this run.
    ///
    /// Nonzero is not an error, it is a FINDING: firmware behaviour observed
    /// after a reboot belongs to a rebooted core, so an assertion that passed
    /// across one was not measuring what it claimed. Backends that cannot
    /// reboot at all report 0 and say so through
    /// [`Mcu::watchdog_limitation`], which is why the two are read together
    /// and neither means anything alone.
    fn watchdog_resets(&self) -> u64 {
        0
    }

    /// Whether this backend can actually host engine-provided I2C slave models,
    /// i.e. whether [`Mcu::on_i2c`] wires the callback to something the firmware's
    /// bus traffic reaches. False means a bound I2C peripheral is silently never
    /// exercised (the Renode backend with an empty `i2c_controllers` list), which
    /// the scheduler records and surfaces as a coverage warning.
    ///
    /// Default true: the in-process AVR core intercepts TWI natively, and the
    /// QEMU backend serves the RAM-mailbox path plus its machine's own emulated
    /// devices (its remaining gaps are surfaced by the CI-level QEMU warnings).
    fn i2c_bus_modeled(&self) -> bool {
        true
    }

    /// Whether this backend can host engine-provided SPI slave models on the
    /// given controller (`None` = the backend's default/first controller, the
    /// [`Mcu::on_spi`] path). False means a bound SPI peripheral is silently
    /// never exercised; see [`Mcu::i2c_bus_modeled`].
    fn spi_bus_modeled(&self, _controller: Option<&str>) -> bool {
        true
    }

    /// Hint which 7-bit I2C slave addresses are attached to the MCU-facing bus.
    ///
    /// Push-style backends can ignore this. Backends that need to register
    /// concrete bus peripherals with an external emulator use it to expose only
    /// the addresses the engine actually modeled.
    fn set_i2c_slave_addresses(&mut self, _addresses: &[u8]) {}

    /// Push a temperature reading into an emulated I2C temperature device at
    /// `addr` (7-bit), in milli-degrees Celsius, so the firmware reads it through
    /// its own I2C controller.
    ///
    /// Default: no-op. The simavr / Renode backends answer I2C reads through the
    /// `on_i2c` callback (the engine's modeled bytes). The QEMU backend, however,
    /// runs the firmware against QEMU's own emulated I2C device (the ESP32 machine
    /// ships a `tmp105` at 0x48), so it overrides this to set that device's
    /// temperature each frame. Backends without such a device ignore it.
    fn set_i2c_device_temperature(&mut self, _addr: u8, _milli_c: i32) {}
}

/// Convert a backend UART socket failure into the trait's lossless-or-loud
/// accounting signal. External emulators cannot return a `Result` through the
/// source-compatible [`Mcu::uart_write`] method, so they retain the failed byte
/// count and expose it through [`Mcu::uart_rx_overflow`].
pub(crate) fn account_uart_injection(
    backend: &str,
    bytes: usize,
    result: anyhow::Result<usize>,
    failed: &mut u64,
) -> usize {
    match result {
        Ok(accepted) => accepted,
        Err(error) => {
            let accepted = error
                .downcast_ref::<UartWriteFailure>()
                .map(|failure| failure.written)
                .unwrap_or(0)
                .min(bytes);
            let lost = bytes - accepted;
            if *failed == 0 {
                eprintln!(
                    "ERROR: {backend} UART injection failed after {accepted} byte(s); counting \
                     {lost} host byte(s) as lost: {error:#}"
                );
            }
            *failed = failed.saturating_add(lost as u64);
            accepted
        }
    }
}

#[derive(Debug)]
pub(crate) struct UartWriteFailure {
    pub written: usize,
    pub source: std::io::Error,
}

impl std::fmt::Display for UartWriteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "UART socket write failed after {} byte(s): {}",
            self.written, self.source
        )
    }
}

impl std::error::Error for UartWriteFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub(crate) fn write_uart_bytes_counted(
    writer: &mut impl std::io::Write,
    bytes: &[u8],
) -> anyhow::Result<usize> {
    let mut written = 0;
    while written < bytes.len() {
        match writer.write(&bytes[written..]) {
            Ok(0) => {
                return Err(UartWriteFailure {
                    written,
                    source: std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "UART socket accepted zero bytes",
                    ),
                }
                .into())
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(source) => return Err(UartWriteFailure { written, source }.into()),
        }
    }
    writer
        .flush()
        .map_err(|source| UartWriteFailure { written, source })?;
    Ok(written)
}

pub(crate) fn drain_uart_bytes(
    reader: &mut impl std::io::Read,
    backend: &str,
) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context as _;

    let mut out = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => anyhow::bail!("{backend} UART socket closed while draining firmware output"),
            Ok(count) => out.extend_from_slice(&chunk[..count]),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(out)
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(error).with_context(|| format!("reading {backend} UART socket"))
            }
        }
    }
}

#[cfg(test)]
mod uart_accounting_tests {
    use super::{account_uart_injection, drain_uart_bytes, write_uart_bytes_counted};

    #[test]
    fn failed_external_uart_write_is_counted_loudly() {
        let mut failed = 3;
        let accepted = account_uart_injection(
            "test backend",
            7,
            Err(anyhow::anyhow!("closed socket")),
            &mut failed,
        );
        assert_eq!(accepted, 0);
        assert_eq!(failed, 10);
    }

    #[test]
    fn partial_uart_write_counts_only_the_unwritten_suffix() {
        struct PrefixThenFail(bool);
        impl std::io::Write for PrefixThenFail {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0 {
                    Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
                } else {
                    self.0 = true;
                    Ok(bytes.len().min(3))
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let result = write_uart_bytes_counted(&mut PrefixThenFail(false), b"1234567");
        let mut failed = 0;
        let accepted = account_uart_injection("test backend", 7, result, &mut failed);
        assert_eq!(accepted, 3);
        assert_eq!(failed, 4);
    }

    #[test]
    fn uart_drain_refuses_eof_and_non_timeout_errors() {
        assert!(drain_uart_bytes(&mut std::io::Cursor::new(Vec::<u8>::new()), "test").is_err());

        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _bytes: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "reset",
                ))
            }
        }
        assert!(drain_uart_bytes(&mut Broken, "test").is_err());
    }
}
