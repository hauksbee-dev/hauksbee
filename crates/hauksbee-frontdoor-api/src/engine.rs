//! The compute-engine boundary consumed by a front door.

use crate::protocol::{
    BoardInfo, LivePeripheralSpec, LiveRegisterMapSpec, PowerSupplyConfig, SimFrame, SolverControls,
};

/// A live simulation engine independent of any transport or server runtime.
pub trait Engine: Send + 'static {
    fn board_info(&self) -> BoardInfo;
    /// Advance simulation by `dt` seconds and produce a frame.
    fn step(&mut self, dt: f64) -> SimFrame;
    fn reset(&mut self);
    fn set_controls(&mut self, controls: SolverControls);
    fn controls(&self) -> SolverControls;
    /// Write bytes to an MCU's serial input.
    fn serial(&mut self, mcu: &str, data: &[u8]);
    /// Drive a bound input source.
    fn set_input(&mut self, source: &str, value: f64);
    /// Configure the power supply on a supply net. Default no-op for engines
    /// without configurable supplies.
    fn set_power_supply(&mut self, _net: &str, _supply: PowerSupplyConfig) {}
    /// Live-control a peripheral by id. Returns true if a peripheral matched.
    fn set_peripheral(&mut self, _id: &str, _value: f64) -> bool {
        false
    }
    /// Attach a control to the running circuit. Default is an explicit refusal.
    fn attach_peripheral(&mut self, _spec: LivePeripheralSpec) -> Result<(), String> {
        Err("this engine does not support attaching live peripherals".into())
    }
    /// Attach a source-bound declarative bus device to the live simulation.
    fn attach_register_map(&mut self, _spec: LiveRegisterMapSpec) -> Result<(), String> {
        Err("this engine does not support attaching live register-map devices".into())
    }

    /// A human-readable reason when the analog solve is failing irrecoverably.
    fn analog_failure(&self) -> Option<String> {
        None
    }

    /// Smallest step worth asking this engine for, in simulated seconds.
    /// Zero means the engine may be paced arbitrarily fine.
    fn min_step_dt(&self) -> f64 {
        0.0
    }
}
