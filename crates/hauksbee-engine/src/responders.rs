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
use crate::peripherals::i2c::I2cBus;
use crate::peripherals::spi::SpiBus;
use hauksbee_mcu::I2cEvent;

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
/// **SPI mode coverage.** This responder models SPI **mode 0** (CPOL=0, CPHA=0:
/// sample MOSI on the rising edge, shift MISO on the falling edge, present bit 7
/// at CS-assert). The other three modes are both caught loudly, so nothing is
/// silently mistimed:
///
/// * **CPOL=1 (modes 2 & 3)** idle SCLK HIGH — caught at CS-assert by the "CS
///   asserted with SCLK high" fault below.
/// * **CPHA=1 (modes 1 & 3)** change MOSI on the *leading* (rising) edge rather
///   than the trailing one, so a MOSI transition arrives *while SCLK is high* —
///   illegal for mode 0. The per-edge model sees that ordering directly (unlike
///   the CS-assert idle level, which cannot separate mode 0 from mode 1), so a
///   MOSI edge during a SCLK-high window faults the responder in `on_edge`.
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
            // Mode-0 masters change MOSI only while SCLK is LOW (the shift phase),
            // holding it stable across the rising sample edge. A MOSI transition
            // during a SCLK-HIGH window is the CPHA=1 (mode 1 / mode 3) signature:
            // those masters drive new data on the leading edge. The idle SCLK
            // level at CS-assert cannot tell mode 0 from mode 1, but this edge
            // ordering can — so we fault rather than silently mistime the byte.
            if self.selected && !self.faulted && self.sclk && high != self.mosi {
                self.fault(
                    "MOSI changed while SCLK high — that is the CPHA=1 (SPI mode 1/3) \
                     shift phase; only mode 0 (CPOL=0, CPHA=0) is modeled for \
                     bit-banged SPI",
                );
            }
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

// ─────────────────────────────────────────────────────────────────────────────
// Soft I2C (firmware bit-bangs SCL/SDA as GPIOs)
// ─────────────────────────────────────────────────────────────────────────────

/// Where the soft-I2C engine is within a transaction. Bit counters live in
/// the variants; SDA/SCL line levels live on the responder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum I2cPhase {
    /// No transaction open (before START / after STOP).
    Idle,
    /// Clocking a master-driven byte: the address byte (`is_addr`) or write
    /// data. `acc` accumulates MSB-first; `nbits` counts SCL rising edges.
    MasterBits { is_addr: bool, nbits: u8, acc: u8 },
    /// The ACK clock after a master byte: the SLAVE owns SDA. `driven` is set
    /// once the ACK level went onto the wire (at the falling edge entering the
    /// ack clock); the next falling edge leaves the state. `read_next` routes
    /// an acked address-with-R into the read phase.
    SlaveAck {
        ack: bool,
        read_next: bool,
        driven: bool,
    },
    /// Clocking a slave-driven read byte: WE drive SDA, shifting `byte` out
    /// MSB-first; `nbits` counts the master's sampling (rising) edges.
    ReadBits { nbits: u8, byte: u8 },
    /// The ACK clock after a read byte: the MASTER owns SDA. `sampled` is
    /// `Some(ack)` once the master's level was read on the rising edge.
    MasterAck { sampled: Option<bool> },
    /// A NACK ended the dialogue (unknown address, or master NACK after the
    /// final read byte): ignore clocks until STOP or repeated START.
    AwaitStop,
}

/// Firmware bit-bangs SCL/SDA as plain GPIOs; this responder is a small I2C
/// protocol engine over the pin edges — START/STOP detection, address
/// decoding, ACK generation, byte clocking — routing the transaction to the
/// EXISTING [`I2cBus`] slave models and answering SDA synchronously, so the
/// firmware's `digitalRead(SDA)` inside its own clock loop sees the slave's
/// bit (05 §1.5).
///
/// ## The honest subset (stated; everything else refused or absent, loudly)
///
/// * **Single master.** There is no arbitration and no detection of a second
///   master (none can exist in-sim: the one firmware owns the pins).
/// * **No clock stretching.** The modeled slaves answer instantaneously and
///   never hold SCL; a real peripheral that stretches would run FASTER here,
///   never slower, and firmware relying on stretching semantics is outside
///   the subset.
/// * **7-bit addressing, standard framing.** START/repeated-START/STOP are
///   recognized from any state (SDA transition while SCL high), exactly the
///   asynchronous resync a real slave performs — so a mid-byte START is
///   treated as a START, not an error, matching silicon.
/// * **Push-pull master waveform.** The responder observes the master through
///   PORT-register edges. Classic open-drain emulation that toggles ONLY the
///   direction register (DDR out+low vs. input-release, PORT held at 0)
///   produces no PORT edges at all and is therefore INVISIBLE — a documented
///   no-answer, not a wrong answer: the firmware sees a dead bus (perpetual
///   NACK), and this doc plus the fixture show the supported pattern (drive
///   SDA push-pull; switch it to input for the ACK bit and read bytes).
/// * **An address no attached slave models is NACKed** (SDA left high), with
///   a once-per-address warning — never a fake ACK.
///
/// SDA is bidirectional: the responder tracks the MASTER's driven level from
/// the PORT edges, separately from the level IT drives into the MCU's input
/// register. Slave-driven bits (ACKs, read bytes) go onto the wire at the SCL
/// falling edge (data changes while the clock is low, per the spec) and are
/// re-asserted at the following rising edge, so a firmware that flips SDA to
/// input only just before reading still sees the bit.
pub struct SoftI2cResponder {
    bus: Arc<Mutex<I2cBus>>,
    scl_pin: (char, u8),
    sda_pin: (char, u8),
    /// Last seen master-driven line levels (from PORT edges).
    scl: bool,
    sda: bool,
    phase: I2cPhase,
    /// 7-bit address + R/W of the open transaction.
    addr: u8,
    /// True when the open transaction's address matched an attached slave.
    active: bool,
    /// The SDA level this responder is currently driving into the MCU input,
    /// while a slave-owned phase holds the line.
    drive: Option<bool>,
    /// Addresses already warned about (unmodeled slave), to keep the honest
    /// NACK loud but not spammy.
    warned_addrs: Vec<u8>,
    /// Per-edge waveform trace to stderr (`HAUKSBEE_SOFT_I2C_TRACE=1`),
    /// cached at construction: this runs on every SCL/SDA edge.
    trace: bool,
}

impl SoftI2cResponder {
    pub fn new(bus: Arc<Mutex<I2cBus>>, scl_pin: (char, u8), sda_pin: (char, u8)) -> Self {
        SoftI2cResponder {
            bus,
            scl_pin,
            sda_pin,
            scl: false,
            sda: false,
            phase: I2cPhase::Idle,
            addr: 0,
            active: false,
            drive: None,
            warned_addrs: Vec::new(),
            trace: std::env::var_os("HAUKSBEE_SOFT_I2C_TRACE").is_some(),
        }
    }

    fn dispatch(&mut self, ev: I2cEvent) -> Option<u8> {
        self.bus.lock().unwrap_or_else(|e| e.into_inner()).dispatch(ev)
    }

    /// Put a slave-driven level on the wire (records it for re-assertion).
    fn drive_sda(&mut self, level: bool, out: &mut Vec<((char, u8), bool)>) {
        self.drive = Some(level);
        out.push((self.sda_pin, level));
    }

    /// Release the line to the master: drive the idle-high pull-up level once
    /// and stop re-asserting.
    fn release_sda(&mut self, out: &mut Vec<((char, u8), bool)>) {
        self.drive = None;
        out.push((self.sda_pin, true));
    }

    /// Fetch the next read byte from the addressed slave and put its MSB on
    /// the wire (entry into `ReadBits`, always at a falling edge).
    fn begin_read_byte(&mut self, out: &mut Vec<((char, u8), bool)>) {
        let byte = match self.dispatch(I2cEvent::Read { addr: self.addr }) {
            Some(b) => b,
            None => {
                // Structurally impossible while `active` (the address matched
                // at Start); if a slave vanished mid-transaction, say so and
                // present the open-bus level rather than inventing data.
                eprintln!(
                    "ERROR: soft I2C: slave 0x{:02x} answered Start but not Read; \
                     presenting 0xFF (open bus)",
                    self.addr
                );
                0xFF
            }
        };
        self.phase = I2cPhase::ReadBits { nbits: 0, byte };
        self.drive_sda(byte & 0x80 != 0, out);
    }

    /// A START (or repeated START) was seen: begin address capture. Never
    /// dispatches a Stop — a repeated START legitimately ends the previous
    /// transfer without one (the register-read idiom: write pointer, Sr,
    /// read), and the slave models' `on_start` handles the rollover.
    fn on_start(&mut self, out: &mut Vec<((char, u8), bool)>) {
        self.phase = I2cPhase::MasterBits {
            is_addr: true,
            nbits: 0,
            acc: 0,
        };
        if self.drive.is_some() {
            self.release_sda(out);
        }
    }

    /// A STOP was seen: close the transaction. The Stop event is recorded by
    /// the bus and its ctx-bearing `on_stop` is delivered by the scheduler's
    /// chunk-boundary `flush_stops`, same as the hardware-TWI path.
    fn on_stop(&mut self, out: &mut Vec<((char, u8), bool)>) {
        if self.active {
            self.dispatch(I2cEvent::Stop { addr: self.addr });
        }
        self.active = false;
        self.phase = I2cPhase::Idle;
        if self.drive.is_some() {
            self.release_sda(out);
        }
    }

    /// A master byte completed (8th rising edge): decode it, decide the ACK.
    fn on_master_byte(&mut self, is_addr: bool, acc: u8) {
        if is_addr {
            let addr = acc >> 1;
            let read = acc & 1 != 0;
            self.addr = addr;
            let known = self
                .bus
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .addresses()
                .contains(&addr);
            // Dispatch the Start regardless (the bus tracks its own active
            // address; an unmatched Start clears it) — but only a MODELED
            // address gets the ACK. An unknown address is NACKed honestly,
            // and loudly once, never fake-ACKed.
            self.dispatch(I2cEvent::Start { addr, read });
            if !known && !self.warned_addrs.contains(&addr) {
                self.warned_addrs.push(addr);
                eprintln!(
                    "WARN: soft I2C: firmware addressed 0x{addr:02x} but no attached \
                     slave models it — NACKing (honest no-answer)"
                );
            }
            self.active = known;
            self.phase = I2cPhase::SlaveAck {
                ack: known,
                read_next: known && read,
                driven: false,
            };
        } else {
            // Write data byte. The models accept-and-count every write (the
            // hardware-TWI path has the same always-ACK shape), so an active
            // transaction ACKs; an inactive one (data pushed after an address
            // NACK) stays NACKed.
            if self.active {
                self.dispatch(I2cEvent::Write {
                    addr: self.addr,
                    data: acc,
                });
            }
            self.phase = I2cPhase::SlaveAck {
                ack: self.active,
                read_next: false,
                driven: false,
            };
        }
    }
}

impl InputResponder for SoftI2cResponder {
    fn watched_pins(&self) -> Vec<(char, u8)> {
        vec![self.scl_pin, self.sda_pin]
    }

    fn on_edge(&mut self, pin: (char, u8), high: bool) -> Vec<((char, u8), bool)> {
        let mut out = Vec::new();

        if self.trace {
            eprintln!(
                "soft-i2c edge {:?}={} phase={:?} sda={} scl={}",
                pin, high, self.phase, self.sda, self.scl
            );
        }

        if pin == self.sda_pin {
            let prev = self.sda;
            self.sda = high;
            // SDA transitions while SCL is high are the framing symbols; while
            // SCL is low they are ordinary data-bit setup (level recorded
            // above, sampled at the next rising edge).
            if self.scl && prev != high {
                if high {
                    self.on_stop(&mut out);
                } else {
                    self.on_start(&mut out);
                }
            }
            return out;
        }

        if pin != self.scl_pin {
            return out;
        }
        let rising = high && !self.scl;
        let falling = !high && self.scl;
        self.scl = high;

        if rising {
            match self.phase {
                I2cPhase::MasterBits { is_addr, nbits, acc } => {
                    let acc = (acc << 1) | u8::from(self.sda);
                    if nbits + 1 == 8 {
                        self.on_master_byte(is_addr, acc);
                    } else {
                        self.phase = I2cPhase::MasterBits {
                            is_addr,
                            nbits: nbits + 1,
                            acc,
                        };
                    }
                }
                I2cPhase::MasterAck { .. } => {
                    // Master's ACK (low) / NACK (high) is on the wire now.
                    self.phase = I2cPhase::MasterAck {
                        sampled: Some(!self.sda),
                    };
                }
                // Slave-owned clock-high windows: the master is sampling what
                // we drove at the last falling edge. Re-assert it so a
                // firmware that switched SDA to input only after that edge
                // still reads the bit (the input-register raise is refreshed
                // on the read side of the clock).
                I2cPhase::SlaveAck { driven: true, .. } | I2cPhase::ReadBits { .. } => {
                    if let Some(level) = self.drive {
                        out.push((self.sda_pin, level));
                    }
                    if let I2cPhase::ReadBits { nbits, byte } = self.phase {
                        // Count the master's sample; after the 8th the next
                        // clock is the master's ACK.
                        if nbits + 1 == 8 {
                            self.phase = I2cPhase::MasterAck { sampled: None };
                        } else {
                            self.phase = I2cPhase::ReadBits {
                                nbits: nbits + 1,
                                byte,
                            };
                        }
                    }
                }
                I2cPhase::Idle | I2cPhase::AwaitStop | I2cPhase::SlaveAck { .. } => {}
            }
            return out;
        }
        if !falling {
            return out;
        }

        // Falling edge: the slave's turn to change the wire (data valid while
        // the clock is high, changes while low).
        match self.phase {
            I2cPhase::SlaveAck {
                ack,
                read_next,
                driven: false,
            } => {
                // Entering the ACK clock: put ACK (low) / NACK (high) on the
                // wire before the master raises SCL to sample it.
                self.drive_sda(!ack, &mut out);
                self.phase = I2cPhase::SlaveAck {
                    ack,
                    read_next,
                    driven: true,
                };
            }
            I2cPhase::SlaveAck {
                ack,
                read_next,
                driven: true,
            } => {
                // Leaving the ACK clock.
                if read_next {
                    self.begin_read_byte(&mut out);
                } else if ack {
                    // Acked address-for-write or write byte: the master's
                    // next byte follows.
                    self.release_sda(&mut out);
                    self.phase = I2cPhase::MasterBits {
                        is_addr: false,
                        nbits: 0,
                        acc: 0,
                    };
                } else {
                    // NACK delivered: nothing more until STOP / Sr.
                    self.release_sda(&mut out);
                    self.phase = I2cPhase::AwaitStop;
                }
            }
            I2cPhase::ReadBits { nbits, byte } if nbits > 0 => {
                // Shift the next bit out (bit 7 went out at entry).
                self.drive_sda(byte & (0x80 >> nbits) != 0, &mut out);
            }
            I2cPhase::MasterAck { sampled } => match sampled {
                None => {
                    // The falling edge after our last data bit: hand the wire
                    // to the master for its ACK/NACK.
                    self.release_sda(&mut out);
                }
                Some(true) => {
                    // Master ACK: it wants the next byte.
                    self.begin_read_byte(&mut out);
                }
                Some(false) => {
                    // Master NACK: last byte of the read; STOP or Sr follows.
                    self.phase = I2cPhase::AwaitStop;
                }
            },
            _ => {}
        }
        out
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

    /// CPHA=1 (mode 1) idles SCLK LOW exactly like mode 0, so the CS-assert
    /// polarity guard cannot catch it — but a mode-1 master changes MOSI on the
    /// leading edge, i.e. while SCLK is HIGH. The per-edge responder sees that
    /// ordering and must fault rather than silently mistime the byte.
    #[test]
    fn bitbang_spi_refuses_cpha1_clock_phase() {
        let (mut m, _bus) = spi_master();
        m.edge(PINS.cs_n, true); // idle
        m.select(); // CS low, SCLK low: passes the polarity guard
        assert!(!m.resp.faulted(), "mode 1 idles SCLK low — not caught at CS assert");

        // Mode-1 shift phase: raise SCLK (leading edge), THEN drive the bit.
        m.edge(PINS.sclk, true);
        m.edge(PINS.mosi, true); // MOSI changes while SCLK high -> CPHA=1 tell
        assert!(
            m.resp.faulted(),
            "MOSI transition during a SCLK-high window must fault as mode 1/3"
        );
    }

    /// A well-behaved mode-0 master that happens to re-drive MOSI to the SAME
    /// level while SCLK is high must NOT fault (no transition, no phase signal).
    #[test]
    fn bitbang_spi_tolerates_idempotent_mosi_while_high() {
        let (mut m, _bus) = spi_master();
        m.edge(PINS.cs_n, true);
        m.select();
        m.edge(PINS.mosi, true); // set bit while SCLK low (legal mode-0 setup)
        m.edge(PINS.sclk, true); // sample
        m.edge(PINS.mosi, true); // redundant same-level write while high: no edge
        assert!(
            !m.resp.faulted(),
            "an idempotent MOSI write (no level change) is not a phase signal"
        );
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

    // ── Soft I2C ─────────────────────────────────────────────────────────────

    const SCL: (char, u8) = ('D', 2);
    const SDA: (char, u8) = ('D', 3);

    const I2C_SPEC: &str = r#"
[sensor]
name = "MINI6050"
bus = "i2c"
i2c_address = 0x68

[[sensor.input]]
name = "val"
default = 0.0

[[sensor.register]]
addr = 0x75
const = [0x68]

[[sensor.register]]
addr = 0x41
bytes = 2
encoding = "i16_be"
expr = "val"

[sensor.protocol]
style = "i2c_pointer"
"#;

    /// A bit-level soft-I2C master driving the responder the way the fixture
    /// firmware drives the real pins: push-pull SDA for master bits, sampling
    /// the responder's SDA drive (last drive wins) for ACKs and read bytes.
    struct I2cMaster {
        resp: SoftI2cResponder,
        /// The SDA input-pin level as the MCU would read it.
        sda_in: bool,
    }

    impl I2cMaster {
        fn edge(&mut self, pin: (char, u8), high: bool) {
            for (p, level) in self.resp.on_edge(pin, high) {
                assert_eq!(p, SDA, "responder must only drive SDA");
                self.sda_in = level;
            }
        }
        fn init(&mut self) {
            // Firmware init: both lines driven high (bus idle).
            self.edge(SDA, true);
            self.edge(SCL, true);
        }
        fn start(&mut self) {
            // SDA falls while SCL high, then SCL falls.
            self.edge(SDA, true);
            self.edge(SCL, true);
            self.edge(SDA, false);
            self.edge(SCL, false);
        }
        fn stop(&mut self) {
            self.edge(SDA, false);
            self.edge(SCL, true);
            self.edge(SDA, true);
        }
        /// Write one byte; returns the slave's ACK (true = acked).
        fn write_byte(&mut self, byte: u8) -> bool {
            for i in (0..8).rev() {
                self.edge(SDA, (byte >> i) & 1 != 0);
                self.edge(SCL, true);
                self.edge(SCL, false);
            }
            // ACK clock: the slave drove SDA at the falling edge above; the
            // master samples while SCL is high.
            self.edge(SCL, true);
            let ack = !self.sda_in;
            self.edge(SCL, false);
            ack
        }
        /// Read one byte, answering with `ack` (true = ACK = more bytes).
        fn read_byte(&mut self, ack: bool) -> u8 {
            let mut byte = 0u8;
            for _ in 0..8 {
                self.edge(SCL, true);
                byte = (byte << 1) | u8::from(self.sda_in);
                self.edge(SCL, false);
            }
            // Master ACK/NACK (push-pull), sampled by the slave on the rising
            // edge.
            self.edge(SDA, !ack);
            self.edge(SCL, true);
            self.edge(SCL, false);
            byte
        }
    }

    fn i2c_master(val: f64) -> (I2cMaster, Arc<Mutex<I2cBus>>) {
        let mut sensor = RegisterMapSensor::from_toml(I2C_SPEC).unwrap();
        sensor.set_input("val", val);
        let bus = Arc::new(Mutex::new(
            I2cBus::new("U3").with_slave(Box::new(sensor)),
        ));
        let resp = SoftI2cResponder::new(bus.clone(), SCL, SDA);
        (
            I2cMaster {
                resp,
                sda_in: true,
            },
            bus,
        )
    }

    /// PROOF: the classic pointered register read — START, addr+W (acked),
    /// pointer byte, repeated START, addr+R (acked), data bytes with a master
    /// ACK between and a NACK to end, STOP — recovered entirely from pin
    /// edges and answered by the byte-level RegisterMapSensor.
    #[test]
    fn soft_i2c_reads_registers_via_repeated_start() {
        let (mut m, _bus) = i2c_master(1234.0);
        m.init();

        // WHO_AM_I (0x75), single-byte read.
        m.start();
        assert!(m.write_byte(0x68 << 1), "address+W must ACK");
        assert!(m.write_byte(0x75), "pointer byte must ACK");
        m.start(); // repeated START
        assert!(m.write_byte((0x68 << 1) | 1), "address+R must ACK");
        let who = m.read_byte(false);
        m.stop();
        assert_eq!(who, 0x68, "WHO_AM_I over soft I2C");

        // Two-byte i16_be register (0x41) = 1234.
        m.start();
        assert!(m.write_byte(0x68 << 1));
        assert!(m.write_byte(0x41));
        m.start();
        assert!(m.write_byte((0x68 << 1) | 1));
        let hi = m.read_byte(true);
        let lo = m.read_byte(false);
        m.stop();
        assert_eq!(i16::from_be_bytes([hi, lo]), 1234);
    }

    /// An address no attached slave models is NACKed — the honest no-answer,
    /// never a fake ACK — and a following good transaction still works.
    #[test]
    fn soft_i2c_nacks_unknown_address() {
        let (mut m, _bus) = i2c_master(0.0);
        m.init();

        m.start();
        assert!(!m.write_byte(0x21 << 1), "unmodeled address must NACK");
        m.stop();

        m.start();
        assert!(m.write_byte(0x68 << 1), "modeled address still ACKs");
        m.stop();
    }

    /// A START mid-byte resyncs the engine (the asynchronous START detection
    /// real slaves perform) instead of erroring or staying desynced.
    #[test]
    fn soft_i2c_resyncs_on_mid_byte_start() {
        let (mut m, _bus) = i2c_master(0.0);
        m.init();

        // Begin an address byte, abandon it three bits in with a new START.
        m.start();
        for bit in [true, false, true] {
            m.edge(SDA, bit);
            m.edge(SCL, true);
            m.edge(SCL, false);
        }
        m.edge(SDA, true); // setup for the framing violation
        m.edge(SCL, true);
        m.edge(SDA, false); // SDA falls while SCL high: START
        m.edge(SCL, false);

        // The fresh address byte must be captured cleanly.
        assert!(m.write_byte(0x68 << 1), "post-resync address must ACK");
        m.stop();
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
