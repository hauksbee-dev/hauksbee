//! Synchronous MCU input-responder registry (05 §1.5, generalized).
//!
//! A firmware bit-bang loop that READS a pin inside the same `run_micros` that
//! toggles its clock (74HC165 readback, bit-banged SPI MISO, a soft-I2C read)
//! only works if the modeled device answers between the firmware's own
//! instructions. The MCU trait's `on_input_responder` hook provides exactly
//! that on push backends: on every GPIO output edge the responder runs
//! synchronously and its returned input-pin drives are applied before the
//! firmware's next instruction (`hauksbee-mcu/src/avr.rs`, the per-port IRQ
//! hook). Poll backends (Renode/QEMU) keep the hook's no-op default — their
//! responder tier is deliberately coarse (05 §1.5), and nothing in this module
//! assumes any particular backend feature.
//!
//! The hook takes ONE closure per MCU. This module is the multiplexer that
//! lets several protocol responders share it: each [`InputResponder`] declares
//! the output pins it consumes edges from, and the [`ResponderRegistry`]
//! dispatches each edge to exactly the responders keyed on that pin. The
//! registry is what 05 §1.5 calls "input responder callbacks keyed on
//! (MCU, input pin)": the MCU key is the registry instance (one per live MCU,
//! held by the scheduler), the pin key is the dispatch map here.
//!
//! Registered protocols:
//!   * [`Hc165Responder`] — the original consumer: MCU-bit-banged 74HC165
//!     parallel-in/serial-out chains (the B2 readback fix), unchanged in
//!     behaviour, now arriving through the registry instead of owning the
//!     hook outright.
//!   * [`BitBangSpiResponder`] — firmware bit-bangs SCLK/MOSI/CS on GPIOs and
//!     reads MISO from an existing byte-level [`SpiBus`] slave model.
//!   * [`SoftI2cResponder`] — firmware bit-bangs SCL/SDA on GPIOs; a small
//!     I2C protocol engine over pin edges routes the transaction to the
//!     existing [`I2cBus`] slave models.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::digital::{Hc165Chain, LogicLevels};

/// One bit-banged input protocol instance: consumes MCU GPIO *output* edges
/// on its watched pins and answers by driving MCU *input* pins.
///
/// `on_edge` runs synchronously inside the MCU's run loop (from the backend's
/// GPIO output hook, under its callback lock), so implementations must be
/// cheap and must never block: lock only leaf resources (a device model, a
/// voltage snapshot), never the scheduler or an MCU.
pub trait InputResponder: Send {
    /// The MCU GPIO output pins this responder consumes edges from. Fixed for
    /// the responder's lifetime: the registry indexes these once at
    /// registration and never re-asks.
    fn watched_pins(&self) -> Vec<(char, u8)>;

    /// Handle one GPIO output edge on a watched pin (`high` is the pin's new
    /// level). Returns the MCU input pins to drive — applied immediately,
    /// before the firmware's next instruction.
    fn on_edge(&mut self, pin: (char, u8), high: bool) -> Vec<((char, u8), bool)>;
}

/// Multiplexes one MCU's single `on_input_responder` slot across many
/// [`InputResponder`]s, dispatching each output edge only to the responders
/// keyed on that pin.
///
/// A pin miss is one `HashMap` lookup — the same cheap early-return the
/// original single-purpose 165 closure had, preserved so a busy non-protocol
/// pin (a status LED toggling in the firmware's hot loop) costs nothing.
/// Multiple responders may watch the same pin (e.g. two protocols sharing a
/// clock line); their drives concatenate in registration order.
#[derive(Default)]
pub struct ResponderRegistry {
    responders: Vec<Box<dyn InputResponder>>,
    /// pin -> indices into `responders` watching it.
    by_pin: HashMap<(char, u8), Vec<usize>>,
}

impl ResponderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a responder, indexing its watched pins for dispatch.
    pub fn register(&mut self, responder: Box<dyn InputResponder>) {
        let idx = self.responders.len();
        for pin in responder.watched_pins() {
            self.by_pin.entry(pin).or_default().push(idx);
        }
        self.responders.push(responder);
    }

    /// Route one GPIO output edge to every responder watching `pin`,
    /// concatenating their input-pin drives. This is the body of the single
    /// closure the scheduler installs via `Mcu::on_input_responder`.
    pub fn dispatch(&mut self, pin: (char, u8), high: bool) -> Vec<((char, u8), bool)> {
        let Some(indices) = self.by_pin.get(&pin) else {
            return Vec::new();
        };
        let mut drives = Vec::new();
        for &i in indices {
            drives.extend(self.responders[i].on_edge(pin, high));
        }
        drives
    }

    pub fn is_empty(&self) -> bool {
        self.responders.is_empty()
    }

    pub fn len(&self) -> usize {
        self.responders.len()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 74HC165 chain responder
// ─────────────────────────────────────────────────────────────────────────────

/// The 74HC165 read-chain as a registry citizen: forwards PL / SCLK edges to
/// the shared [`Hc165Chain`], which samples the latch inputs on a PL load and
/// presents the next QH bit on MISO. Behaviour is exactly the closure the
/// scheduler used to install directly (the B2 readback fix); only the
/// dispatch route changed. The chain stays `Arc`-shared with the scheduler so
/// `hc165_chain_pins()` / `hc165_loaded_words()` introspection keeps working.
pub struct Hc165Responder {
    chain: Arc<Mutex<Hc165Chain>>,
    levels: LogicLevels,
    /// The scheduler-refreshed node-voltage snapshot the PL-load sampling
    /// reads (the latch input levels at the last solved operating point).
    volts: Arc<Mutex<Vec<f64>>>,
    pl_n: (char, u8),
    clk: (char, u8),
}

impl Hc165Responder {
    pub fn new(
        chain: Arc<Mutex<Hc165Chain>>,
        levels: LogicLevels,
        volts: Arc<Mutex<Vec<f64>>>,
    ) -> Self {
        let (pl_n, clk) = {
            let c = chain.lock().unwrap_or_else(|e| e.into_inner());
            (c.pl_n, c.clk)
        };
        Hc165Responder {
            chain,
            levels,
            volts,
            pl_n,
            clk,
        }
    }
}

impl InputResponder for Hc165Responder {
    fn watched_pins(&self) -> Vec<(char, u8)> {
        vec![self.pl_n, self.clk]
    }

    fn on_edge(&mut self, pin: (char, u8), high: bool) -> Vec<((char, u8), bool)> {
        // Same lock order as the pre-registry closure: voltage snapshot first,
        // then the chain (both leaf locks; nothing here takes them together
        // the other way round).
        let v = self.volts.lock().unwrap_or_else(|e| e.into_inner());
        let node_v = |n: hauksbee_ir::NodeId| v.get(n.0 as usize).copied().unwrap_or(0.0);
        let mut ch = self.chain.lock().unwrap_or_else(|e| e.into_inner());
        match ch.on_edge(pin, high, &node_v, &self.levels) {
            Some((miso, level)) => vec![(miso, level)],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A responder that records every edge it sees and answers with a fixed
    /// drive, for registry-dispatch proofs.
    struct Probe {
        pins: Vec<(char, u8)>,
        seen: Vec<((char, u8), bool)>,
        answer: Vec<((char, u8), bool)>,
    }

    impl InputResponder for Probe {
        fn watched_pins(&self) -> Vec<(char, u8)> {
            self.pins.clone()
        }
        fn on_edge(&mut self, pin: (char, u8), high: bool) -> Vec<((char, u8), bool)> {
            self.seen.push((pin, high));
            self.answer.clone()
        }
    }

    #[test]
    fn registry_dispatches_only_to_watchers() {
        let mut reg = ResponderRegistry::new();
        reg.register(Box::new(Probe {
            pins: vec![('B', 5)],
            seen: Vec::new(),
            answer: vec![(('B', 4), true)],
        }));
        reg.register(Box::new(Probe {
            pins: vec![('D', 2)],
            seen: Vec::new(),
            answer: Vec::new(),
        }));

        // An edge on a watched pin answers; an unwatched pin answers nothing.
        assert_eq!(reg.dispatch(('B', 5), true), vec![(('B', 4), true)]);
        assert!(reg.dispatch(('C', 0), true).is_empty());
        assert!(reg.dispatch(('D', 2), false).is_empty());
    }

    #[test]
    fn shared_pin_concatenates_in_registration_order() {
        let mut reg = ResponderRegistry::new();
        reg.register(Box::new(Probe {
            pins: vec![('B', 5)],
            seen: Vec::new(),
            answer: vec![(('B', 4), true)],
        }));
        reg.register(Box::new(Probe {
            pins: vec![('B', 5)],
            seen: Vec::new(),
            answer: vec![(('C', 1), false)],
        }));
        assert_eq!(
            reg.dispatch(('B', 5), false),
            vec![(('B', 4), true), (('C', 1), false)]
        );
    }
}
