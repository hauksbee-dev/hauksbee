//! Shared test-only helpers. The I2C transactions serve both `i2c::tests` and
//! `register_map::tests` (the hand-coded and declarative sensor suites drive
//! the same bus protocol), so a framing mistake shows up in both at once.

use crate::peripherals::i2c::I2cBus;
use crate::peripherals::spi::SpiSlave;
use crate::MutexExt;
use hauksbee_ir::{Circuit, Device, DeviceId, SourceKind};
use hauksbee_mcu::I2cEvent;
use std::sync::{Arc, Mutex};

/// One pointer-framed write transaction: START, `bytes`, STOP.
pub fn i2c_write(bus: &mut I2cBus, addr: u8, bytes: &[u8]) {
    bus.dispatch(I2cEvent::Start { addr, read: false });
    for &data in bytes {
        bus.dispatch(I2cEvent::Write { addr, data });
    }
    bus.dispatch(I2cEvent::Stop { addr });
}

/// Pointered burst read of `n` bytes starting at register `reg`.
pub fn i2c_read_burst(bus: &mut I2cBus, addr: u8, reg: u8, n: usize) -> Vec<u8> {
    bus.dispatch(I2cEvent::Start { addr, read: false });
    bus.dispatch(I2cEvent::Write { addr, data: reg });
    bus.dispatch(I2cEvent::Start { addr, read: true });
    let out = (0..n)
        .map(|_| bus.dispatch(I2cEvent::Read { addr }).unwrap())
        .collect();
    bus.dispatch(I2cEvent::Stop { addr });
    out
}

/// The DC level currently programmed on a bound output driver's Vsource.
pub fn dc_source_volts(circuit: &Circuit, vsource: DeviceId) -> f64 {
    match circuit.devices[vsource.0 as usize] {
        Device::Vsource {
            kind: SourceKind::Dc(v),
            ..
        } => v,
        _ => panic!("driver vsource"),
    }
}

/// An SPI slave that answers every byte with `miso` and records each MOSI
/// byte in `got`; it offers no `miso_preview`.
pub struct FixedSpiSlave {
    pub miso: u8,
    pub got: Arc<Mutex<Vec<u8>>>,
}

impl FixedSpiSlave {
    pub fn new(miso: u8) -> Self {
        Self {
            miso,
            got: Arc::default(),
        }
    }
}

impl SpiSlave for FixedSpiSlave {
    fn transfer(&mut self, mosi: u8) -> u8 {
        self.got.lock_or_recover().push(mosi);
        self.miso
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
