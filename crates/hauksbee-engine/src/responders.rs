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
use crate::peripherals::spi::SpiBus;

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

// ─────────────────────────────────────────────────────────────────────────────
// Bit-banged SPI (MISO answered synchronously)
// ─────────────────────────────────────────────────────────────────────────────

/// The MCU GPIO pins of one bit-banged SPI topology, all on the same MCU.
/// `miso` is the MCU *input* pin the responder drives; the other three are
/// firmware outputs the responder consumes edges from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitBangSpiPins {
    pub sclk: (char, u8),
    pub mosi: (char, u8),
    pub miso: (char, u8),
    /// Active-low chip select. Required: without a CS edge there is no honest
    /// frame boundary for a bit-level bridge (the byte-level path at least has
    /// the chunk heuristic; a bit stream without CS has nothing).
    pub cs_n: (char, u8),
}

/// Firmware bit-bangs SCLK/MOSI/CS on GPIOs; this responder clocks the bits
/// into the EXISTING byte-level [`SpiBus`] slave model and answers MISO
/// synchronously, so `digitalRead(MISO)` inside the firmware's own clock loop
/// sees the slave's bit (05 §1.5).
///
/// ## Supported waveform (stated subset — refused loudly outside it)
///
/// * **SPI mode 0** (CPOL=0, CPHA=0): SCLK idles LOW at CS assert; the master
///   samples MISO on the RISING edge; the slave shifts on the FALLING edge.
///   A CS assert with SCLK high is a mode violation and faults the responder.
/// * **MSB first**, 8-bit frames. A CS deassert mid-byte is reported.
///
/// ## Bridging a bit stream to a byte-level model, honestly
///
/// The slave API is `transfer(mosi) -> miso`, one whole byte per call — but at
/// the moment the slave must present MISO bit 7, the master's MOSI byte has
/// not arrived yet. Physically there is no paradox (a real slave preloads its
/// shift register; MISO byte N cannot depend on MOSI byte N), so the bridge
/// asks the slave for that preloaded byte via [`SpiSlave::miso_preview`]
/// (`SpiBus::miso_preview`), shifts ITS bits out on the falling edges, and at
/// the byte boundary (8th rising edge, when the MOSI byte is complete) makes
/// the real `transfer` call — cross-checking that the reply equals what was
/// presented. Two refusal paths, both loud, never silently wrong:
///
/// * preview/transfer mismatch: the model's reply depended on the incoming
///   byte — the preview contract is broken; error + responder disabled.
/// * no preview (`None`): the bridge presents LOW bits; if the transfer then
///   returns nonzero, the slave meant to say something the firmware did not
///   see — error + responder disabled. (A slave that genuinely answers 0x00,
///   e.g. during a command/write phase, is exact.)
///
/// [`SpiSlave::miso_preview`]: crate::peripherals::spi::SpiSlave::miso_preview
pub struct BitBangSpiResponder {
    bus: Arc<Mutex<SpiBus>>,
    pins: BitBangSpiPins,
    /// CS currently asserted (we are mid-transaction).
    selected: bool,
    /// Last seen SCLK / MOSI output levels (edges carry only the new level, so
    /// the responder tracks the other line's state itself).
    sclk: bool,
    mosi: bool,
    /// Rising edges consumed for the byte in flight (0..8).
    nbits: u8,
    /// MOSI accumulator, MSB first.
    acc: u8,
    /// The MISO byte currently being shifted out (`None` = the slave gave no
    /// preview and LOW bits are being presented pending the byte-end check).
    presented: Option<u8>,
    /// The NEXT byte's preview, fetched at the byte boundary and promoted to
    /// `presented` on the following falling edge (mode 0: the slave shifts a
    /// fresh bit 7 out on the falling edge after the last sample).
    pending: Option<Option<u8>>,
    /// Set on the first waveform/contract violation: the responder stops
    /// answering entirely (the firmware sees a dead MISO line, loudly broken)
    /// rather than continuing with possibly-wrong bits.
    faulted: bool,
}

impl BitBangSpiResponder {
    pub fn new(bus: Arc<Mutex<SpiBus>>, pins: BitBangSpiPins) -> Self {
        BitBangSpiResponder {
            bus,
            pins,
            selected: false,
            sclk: false,
            mosi: false,
            nbits: 0,
            acc: 0,
            presented: Some(0),
            pending: None,
            faulted: false,
        }
    }

    /// True once a waveform or preview-contract violation disabled this
    /// responder for the rest of the run.
    pub fn faulted(&self) -> bool {
        self.faulted
    }

    fn fault(&mut self, msg: &str) {
        let id = {
            let bus = self.bus.lock().unwrap_or_else(|e| e.into_inner());
            bus.id().to_string()
        };
        eprintln!(
            "ERROR: bit-banged SPI '{id}': {msg}; responder disabled for the rest of the run \
             (MISO will not answer)"
        );
        self.faulted = true;
    }

    /// The current bit of the presented byte, MSB first: after `nbits` rising
    /// edges the wire carries bit `7 - nbits`. `None` preview presents LOW.
    fn miso_bit(&self) -> bool {
        let byte = self.presented.unwrap_or(0);
        (byte >> (7 - self.nbits)) & 1 != 0
    }
}

impl InputResponder for BitBangSpiResponder {
    fn watched_pins(&self) -> Vec<(char, u8)> {
        vec![self.pins.sclk, self.pins.mosi, self.pins.cs_n]
    }

    fn on_edge(&mut self, pin: (char, u8), high: bool) -> Vec<((char, u8), bool)> {
        if pin == self.pins.mosi {
            self.mosi = high;
            return Vec::new();
        }

        if pin == self.pins.cs_n {
            if !high && !self.selected {
                // CS assert: fresh transaction.
                self.selected = true;
                self.nbits = 0;
                self.acc = 0;
                self.pending = None;
                if self.faulted {
                    return Vec::new();
                }
                if self.sclk {
                    self.fault(
                        "CS asserted with SCLK high — only SPI mode 0 (CPOL=0, CPHA=0) \
                         is modeled for bit-banged SPI",
                    );
                    return Vec::new();
                }
                let mut bus = self.bus.lock().unwrap_or_else(|e| e.into_inner());
                bus.cs_assert();
                self.presented = bus.miso_preview();
                drop(bus);
                // Present bit 7 immediately: mode 0 slaves drive MISO from CS
                // assert, before the first clock.
                return vec![(self.pins.miso, self.miso_bit())];
            }
            if high && self.selected {
                // CS deassert: end of transaction.
                self.selected = false;
                if self.nbits != 0 && !self.faulted {
                    eprintln!(
                        "WARN: bit-banged SPI '{}': CS deasserted mid-byte ({} of 8 bits \
                         clocked) — the firmware aborted a transfer",
                        self.bus.lock().unwrap_or_else(|e| e.into_inner()).id(),
                        self.nbits,
                    );
                }
                self.bus
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .cs_deassert();
            }
            return Vec::new();
        }

        // SCLK edge.
        self.sclk = high;
        if !self.selected || self.faulted {
            return Vec::new();
        }
        if high {
            // Rising edge: the sampling edge for BOTH directions in mode 0.
            // The MISO bit the firmware is about to digitalRead was driven on
            // the previous falling edge (or at CS assert); the MOSI bit on the
            // wire right now joins the accumulator.
            self.acc = (self.acc << 1) | u8::from(self.mosi);
            self.nbits += 1;
            if self.nbits == 8 {
                let (actual, next) = {
                    let mut bus = self.bus.lock().unwrap_or_else(|e| e.into_inner());
                    let actual = bus.transfer(self.acc);
                    (actual, bus.miso_preview())
                };
                match self.presented {
                    Some(p) if p == actual => {}
                    Some(p) => {
                        self.fault(&format!(
                            "miso_preview promised 0x{p:02x} but transfer returned \
                             0x{actual:02x} — the slave's reply depends on the incoming \
                             byte, which bit-level bridging cannot honor"
                        ));
                        return Vec::new();
                    }
                    None if actual == 0x00 => {}
                    None => {
                        self.fault(&format!(
                            "slave provides no miso_preview and its reply was 0x{actual:02x}; \
                             the firmware read LOW bits instead — bit-banged reads from this \
                             slave are refused"
                        ));
                        return Vec::new();
                    }
                }
                self.nbits = 0;
                self.acc = 0;
                self.pending = Some(next);
            }
            return Vec::new();
        }
        // Falling edge: the slave shifts the next bit out (mode 0). At a byte
        // boundary, promote the prefetched next byte first.
        if let Some(next) = self.pending.take() {
            self.presented = next;
        }
        vec![(self.pins.miso, self.miso_bit())]
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

    // ── Bit-banged SPI ───────────────────────────────────────────────────────

    use crate::peripherals::register_map::RegisterMapSensor;
    use crate::peripherals::spi::SpiSlave;

    const PINS: BitBangSpiPins = BitBangSpiPins {
        sclk: ('B', 1),
        mosi: ('B', 2),
        miso: ('B', 3),
        cs_n: ('B', 0),
    };

    const SPI_SPEC: &str = r#"
[sensor]
name = "MINIMU"
bus = "spi"

[[sensor.input]]
name = "gyro_x"
default = 0.0

[[sensor.register]]
addr = 0x0f
const = [0x42]

[[sensor.register]]
addr = 0x22
bytes = 2
encoding = "i16_le"
expr = "gyro_x"

[sensor.protocol]
style = "spi_reg"
rw_read_is_high = true
addr_mask = 0x7f
"#;

    /// A bit-level SPI master driving the responder the way firmware drives
    /// the real pins: set MOSI, raise SCLK, "digitalRead" MISO (the level the
    /// responder last drove), lower SCLK. Mirrors the mode-0 bit-bang loop of
    /// the firmware fixture.
    struct SpiMaster {
        resp: BitBangSpiResponder,
        /// The MISO input-pin level as the MCU would see it (last drive wins).
        miso: bool,
    }

    impl SpiMaster {
        fn edge(&mut self, pin: (char, u8), high: bool) {
            for (p, level) in self.resp.on_edge(pin, high) {
                assert_eq!(p, PINS.miso, "responder must only drive MISO");
                self.miso = level;
            }
        }
        fn select(&mut self) {
            self.edge(PINS.cs_n, false);
        }
        fn deselect(&mut self) {
            self.edge(PINS.cs_n, true);
        }
        fn xfer(&mut self, mosi: u8) -> u8 {
            let mut got = 0u8;
            for i in (0..8).rev() {
                self.edge(PINS.mosi, (mosi >> i) & 1 != 0);
                self.edge(PINS.sclk, true);
                got = (got << 1) | u8::from(self.miso);
                self.edge(PINS.sclk, false);
            }
            got
        }
    }

    fn spi_master() -> (SpiMaster, Arc<Mutex<SpiBus>>) {
        let mut sensor = RegisterMapSensor::from_toml(SPI_SPEC).unwrap();
        sensor.set_input("gyro_x", 1234.0);
        let bus = Arc::new(Mutex::new(SpiBus::new("U2", Box::new(sensor))));
        let resp = BitBangSpiResponder::new(bus.clone(), PINS);
        (SpiMaster { resp, miso: false }, bus)
    }

    /// PROOF: a full bit-banged mode-0 read transaction against the byte-level
    /// RegisterMapSensor answers WHO_AM_I and a burst data read bit-exactly,
    /// across two CS-framed transactions.
    #[test]
    fn bitbang_spi_reads_who_am_i_and_data() {
        let (mut m, _bus) = spi_master();

        // CS idles high from firmware init: a rising "edge" while deselected.
        m.edge(PINS.cs_n, true);

        m.select();
        let _status = m.xfer(0x80 | 0x0f);
        let who = m.xfer(0x00);
        m.deselect();
        assert_eq!(who, 0x42, "WHO_AM_I over bit-banged SPI");
        assert!(!m.resp.faulted());

        m.select();
        let _status = m.xfer(0x80 | 0x22);
        let lo = m.xfer(0x00);
        let hi = m.xfer(0x00);
        m.deselect();
        assert_eq!(i16::from_le_bytes([lo, hi]), 1234);
        assert!(!m.resp.faulted());
    }

    /// SCLK high at CS assert is not mode 0: the responder must refuse loudly
    /// (fault flag; MISO never answers) instead of clocking garbage.
    #[test]
    fn bitbang_spi_refuses_non_mode0_clock_polarity() {
        let (mut m, _bus) = spi_master();
        m.edge(PINS.sclk, true); // idle-high clock (mode 2/3 shape)
        m.edge(PINS.cs_n, false);
        assert!(m.resp.faulted(), "CS assert with SCLK high must fault");
        // A subsequent clocked byte answers nothing (MISO stays at its init
        // level) rather than a wrong value.
        m.edge(PINS.sclk, false);
        let got = m.xfer(0x80 | 0x0f);
        assert_eq!(got, 0x00, "a faulted responder must not drive MISO");
    }

    /// A slave with no `miso_preview` that replies nonzero: the bridge
    /// presented LOW bits the firmware already consumed, so it must fault
    /// rather than continue as if the read were good.
    #[test]
    fn bitbang_spi_faults_on_previewless_nonzero_reply() {
        struct Opaque;
        impl SpiSlave for Opaque {
            fn transfer(&mut self, _mosi: u8) -> u8 {
                0xAB
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                self
            }
        }
        let bus = Arc::new(Mutex::new(SpiBus::new("U9", Box::new(Opaque))));
        let resp = BitBangSpiResponder::new(bus, PINS);
        let mut m = SpiMaster { resp, miso: false };
        m.edge(PINS.cs_n, true);
        m.select();
        let _ = m.xfer(0x03);
        assert!(
            m.resp.faulted(),
            "nonzero reply with no preview must fault the responder"
        );
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
