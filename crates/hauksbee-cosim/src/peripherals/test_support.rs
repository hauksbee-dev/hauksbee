//! Shared I2C test-only transaction helpers used by both `i2c::tests` and
//! `register_map::tests` (the hand-coded and declarative sensor test suites
//! drive the same bus protocol against different slave implementations), so a
//! framing mistake shows up in both suites at once.

use crate::peripherals::i2c::I2cBus;
use hauksbee_mcu::I2cEvent;

/// One pointer-framed write transaction: START, `bytes`, STOP.
pub(crate) fn i2c_write(bus: &mut I2cBus, addr: u8, bytes: &[u8]) {
    bus.dispatch(I2cEvent::Start { addr, read: false });
    for &data in bytes {
        bus.dispatch(I2cEvent::Write { addr, data });
    }
    bus.dispatch(I2cEvent::Stop { addr });
}

/// Pointered burst read of `n` bytes starting at register `reg`.
pub(crate) fn i2c_read_burst(bus: &mut I2cBus, addr: u8, reg: u8, n: usize) -> Vec<u8> {
    bus.dispatch(I2cEvent::Start { addr, read: false });
    bus.dispatch(I2cEvent::Write { addr, data: reg });
    bus.dispatch(I2cEvent::Start { addr, read: true });
    let out = (0..n)
        .map(|_| bus.dispatch(I2cEvent::Read { addr }).unwrap())
        .collect();
    bus.dispatch(I2cEvent::Stop { addr });
    out
}
