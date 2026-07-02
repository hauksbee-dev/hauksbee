//! Peripherals: things you attach to a board that act as inputs, outputs, and
//! interactive controls during a co-simulation.
//!
//! A peripheral is anything that is *not* a part on the board model but that
//! the firmware or the analog circuit interacts with at runtime: a pushbutton
//! wired to a net, a potentiometer on a connector pin, an I2C temperature
//! sensor the firmware polls, a VCD logger recording transitions. They are the
//! interactive layer over the static board.
//!
//! Three classes, all behind the [`Peripheral`] trait so the scheduler ticks
//! them uniformly:
//!
//! 1. **Analog / contact controls** ([`controls`]) attach to a net (or a
//!    connector ref+pin resolved to a net) and drive it through the same
//!    Thevenin / ideal-source machinery the power supplies and GPIO drivers
//!    use. Pushbutton, toggle, potentiometer, rotary encoder, generic stimulus.
//! 2. **Digital bus slaves** ([`i2c`], [`spi`]) plug into the MCU's `on_i2c` /
//!    `on_spi` hooks and answer like a real part that is not on the board
//!    (24Cxx EEPROM, LM75 temperature sensor, 25xx SPI EEPROM).
//! 3. **Output sinks** ([`sink`]) observe nets and record them. The VCD logger
//!    writes a gtkwave-compatible trace of digital transitions.
//!
//! The scheduler calls, per chunk:
//!   - [`Peripheral::pre_solve`] before the analog solve, so a control can push
//!     its commanded voltage onto its net (timeline events fire here);
//!   - [`Peripheral::post_solve`] after the solve, so a sink can sample the
//!     freshly-solved node voltages.
//!
//! Bus slaves do their work inside the MCU hook callbacks, not in these two,
//! but they still implement the trait so they live in one collection.

use std::collections::HashMap;

use hauksbee_ir::{Circuit, NodeId};

pub mod controls;
pub mod i2c;
pub mod load;
pub mod register_map;
pub mod sink;
pub mod spi;

pub use controls::{
    Encoder, Potentiometer, Pushbutton, Stimulus, ToggleSwitch,
};
pub use load::DynamicLoad;
pub use i2c::{Bme280, Eeprom24c, I2cBus, I2cSlave, Mcp4728};
pub use register_map::RegisterMapSensor;
pub use sink::VcdSink;
pub use spi::{Mcp3008, Spi25Eeprom, SpiBus, SpiSlave};

/// Context handed to a peripheral each chunk so it can read the solved circuit
/// and command its driver. Kept small and borrow-friendly.
pub struct TickCtx<'a> {
    /// The circuit, so a control can mutate its stamped source / driver.
    pub circuit: &'a mut Circuit,
    /// Latest solved node voltages, indexed by `NodeId.0`.
    pub node_volts: &'a [f64],
    /// Current simulation time (s) at the *start* of this chunk.
    pub t: f64,
    /// Chunk length (s).
    pub dt: f64,
}

impl TickCtx<'_> {
    /// Voltage on a node from the last solve (0 if out of range).
    pub fn volts(&self, node: NodeId) -> f64 {
        self.node_volts.get(node.0 as usize).copied().unwrap_or(0.0)
    }
}

/// A live peripheral attached to a board. Implementors stamp whatever circuit
/// devices they need at construction (via [`controls`] helpers) and react each
/// chunk.
pub trait Peripheral: Send {
    /// Stable identifier for live control / state readout (e.g. "BTN1").
    fn id(&self) -> &str;

    /// Human-facing kind string ("pushbutton", "i2c_lm75", "vcd_sink", ...).
    fn kind(&self) -> &'static str;

    /// Called before the analog solve. Controls push their commanded level onto
    /// their net here; timeline events are applied first by the owning
    /// [`PeripheralSet`]. Default no-op.
    fn pre_solve(&mut self, _ctx: &mut TickCtx) {}

    /// Called after the analog solve. Sinks sample node voltages here. Default
    /// no-op.
    fn post_solve(&mut self, _ctx: &mut TickCtx) {}

    /// Apply a live control command (from the websocket or a timeline event).
    /// `value` is interpreted per kind (button: >=0.5 pressed; pot/encoder:
    /// position; stimulus: amplitude/offset). Default no-op.
    fn set_value(&mut self, _value: f64) {}

    /// Numeric state readout for frames (e.g. {"pressed":1.0} or
    /// {"position":0.5}). Default empty.
    fn state(&self) -> HashMap<String, f64> {
        HashMap::new()
    }

    /// Downcast hook so the engine can reach a concrete peripheral (e.g. read an
    /// EEPROM's bytes for an assertion). Default returns self as `Any`.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A timeline event: at time `t_s`, set peripheral `target` to `value`.
#[derive(Debug, Clone)]
pub struct TimelineEvent {
    pub target: String,
    pub t_s: f64,
    pub value: f64,
}

/// The collection of peripherals attached to one board, plus a scheduled
/// timeline. The scheduler owns one of these and ticks it each chunk.
#[derive(Default)]
pub struct PeripheralSet {
    pub peripherals: Vec<Box<dyn Peripheral>>,
    /// Timeline events sorted by time; applied as sim time passes them.
    timeline: Vec<TimelineEvent>,
    /// Index of the next unfired timeline event.
    next_event: usize,
}

impl PeripheralSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a peripheral.
    pub fn push(&mut self, p: Box<dyn Peripheral>) {
        self.peripherals.push(p);
    }

    /// Number of attached peripherals.
    pub fn len(&self) -> usize {
        self.peripherals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peripherals.is_empty()
    }

    /// Add timeline events, keeping the list time-sorted.
    pub fn add_events(&mut self, mut events: Vec<TimelineEvent>) {
        self.timeline.append(&mut events);
        self.timeline
            .sort_by(|a, b| a.t_s.partial_cmp(&b.t_s).unwrap_or(std::cmp::Ordering::Equal));
        self.next_event = 0;
    }

    /// Fire any timeline events whose time has been reached by `t`.
    pub fn fire_due_events(&mut self, t: f64) {
        while self.next_event < self.timeline.len() && self.timeline[self.next_event].t_s <= t + 1e-12 {
            let ev = self.timeline[self.next_event].clone();
            for p in &mut self.peripherals {
                if p.id() == ev.target {
                    p.set_value(ev.value);
                }
            }
            self.next_event += 1;
        }
    }

    /// Tick every peripheral's pre-solve step.
    pub fn pre_solve(&mut self, ctx: &mut TickCtx) {
        for p in &mut self.peripherals {
            p.pre_solve(ctx);
        }
    }

    /// Tick every peripheral's post-solve step.
    pub fn post_solve(&mut self, ctx: &mut TickCtx) {
        for p in &mut self.peripherals {
            p.post_solve(ctx);
        }
    }

    /// Apply a live command to a named peripheral. Returns true if found.
    pub fn set_value(&mut self, id: &str, value: f64) -> bool {
        let mut hit = false;
        for p in &mut self.peripherals {
            if p.id() == id {
                p.set_value(value);
                hit = true;
            }
        }
        hit
    }

    /// State of every peripheral, keyed by id, for frame reporting.
    pub fn states(&self) -> HashMap<String, HashMap<String, f64>> {
        self.peripherals
            .iter()
            .map(|p| (p.id().to_string(), p.state()))
            .collect()
    }

    /// Find a peripheral by id and downcast to a concrete type.
    pub fn get<T: 'static>(&self, id: &str) -> Option<&T> {
        self.peripherals
            .iter()
            .find(|p| p.id() == id)
            .and_then(|p| p.as_any().downcast_ref::<T>())
    }
}
