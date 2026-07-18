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
//! Long-form how-and-why: docs/how-and-why/hauksbee-mcu/traits.md.

use anyhow::Result;
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
    /// This used to be swallowed — the run loop broke out of its step loop but
    /// still reported a clean `Ok`, so a crashed MCU was indistinguishable from
    /// a healthy chunk. Backends that cannot detect a crash leave it false.
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
    /// within [`run_cycles`] / [`run_micros`] on the same thread.
    ///
    /// The cycle stamp is what lets the co-sim replay a sub-µs `shiftOut` SCLK
    /// burst in the exact order (and multiplicity) the firmware produced it,
    /// rather than collapsing the whole chunk to a resting level (numerical lore
    /// #8, `docs/learn/tarski-saga.md` §5). On push backends
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
    /// responder is invoked with the pin and its new level, and returns a list
    /// of input pins to drive — applied *immediately*, before the firmware's
    /// next instruction, within the same `run_micros`. This is the mechanism
    /// that lets a firmware bit-bang a clock and `digitalRead` the resulting
    /// serial-out bit in the SAME tight loop: e.g. a 74HC165 presenting its next
    /// QH bit onto MISO on each SCLK edge. Resolving the response per output edge
    /// (not once per analog chunk) is the read-direction analogue of the
    /// edge-driven 74HC595 write path.
    ///
    /// The default is a no-op for backends that cannot drive an input pin
    /// synchronously from within their run loop (Renode / QEMU push state once
    /// per chunk).
    #[allow(clippy::type_complexity)]
    fn on_input_responder(
        &mut self,
        _responder: Box<dyn FnMut(PinId, bool) -> Vec<(PinId, bool)> + Send>,
    ) {
    }

    // ---- UART ----

    /// Inject bytes into the MCU's UART RX (as if the host sent them).
    fn uart_write(&mut self, bytes: &[u8]);

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
    /// The default implementation forwards to [`on_spi`], which is correct for
    /// single-controller backends (AVR, QEMU). Multi-controller backends (Renode
    /// with multiple SPI controllers) override this to route each controller to
    /// its own bridge/callback, so a slave attached to "spi2" only receives
    /// traffic from the spi2 controller, not from spi3.
    fn on_spi_controller(
        &mut self,
        _controller: &str,
        cb: Box<dyn FnMut(SpiEvent) -> u8 + Send>,
    ) {
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
    /// every pin look "not configured" (the conservative direction — a
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
    /// coverage warning on every report surface — a dropped injection means the
    /// firmware never saw the modeled voltage, so results on that pin are
    /// meaningless and must never read as a healthy co-sim.
    ///
    /// Default empty: backends that deliver every injection (simavr) report no
    /// drops. Sorted ascending so the reported order is deterministic.
    fn adc_dropped_channels(&self) -> Vec<u8> {
        Vec::new()
    }

    /// Whether this backend can actually host engine-provided I2C slave models —
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
