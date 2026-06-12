//! Public API traits and types for galvani-mcu.

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
    /// STOP condition.
    Stop { addr: u8 },
}

/// An event on the SPI bus, as seen from the perspective of an external
/// peripheral intercepting firmware transfers.
#[derive(Debug, Clone)]
pub struct SpiEvent {
    /// Byte clocked out of MOSI.
    pub mosi: u8,
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
    /// The callback receives the pin and the new logic level.  It is called
    /// synchronously from within [`run_cycles`] / [`run_micros`] on the same
    /// thread.
    fn on_pin_change(&mut self, cb: Box<dyn FnMut(PinId, bool) + Send>);

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
}
