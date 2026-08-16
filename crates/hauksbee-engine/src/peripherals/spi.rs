//! SPI slave framework and two concrete devices: a 25xx SPI EEPROM and an
//! MCP3008 8-channel ADC.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/peripherals.md.
//!
//! ## How it plugs into the co-sim
//!
//! The MCU backend surfaces each byte the firmware clocks out as a
//! [`hauksbee_mcu::SpiEvent`] (MOSI byte) through `Mcu::on_spi`, and the handler
//! returns the
//! MISO byte the slave drives back on the same transfer. An [`SpiBus`] owns one
//! slave and threads the byte stream through the slave's command state machine.
//! Several buses can share one controller: the simavr SPI IRQ does not carry
//! chip-select, so each bus tracks its own CS instead. When the binder resolved
//! the slave's CS net to an MCU pin, a live GPIO hook frames the bus off the real
//! CS edge and the scheduler's shared `on_spi` handler routes each byte to the
//! selected bus, so the deselected slaves never see a sibling's traffic.
//!
//! A bus whose CS did NOT resolve falls back to the chunk-boundary heuristic and
//! starts permanently selected, because there is no CS edge to gate it. Nothing
//! enforces that such a bus is alone on its controller: with two unresolved-CS
//! slaves the dispatcher hands every byte to the first selected bus it finds, and
//! the second never sees traffic. That is a coverage gap, not a silent wrong
//! answer, since `framing_mode` reports `Heuristic` for those buses and the
//! report surfaces it, but the heuristic is only CORRECT for a lone slave. The
//! fix for a real multi-slave board is to resolve its CS nets.
//!
//! ## Transaction framing: the ladder
//!
//! simavr's SPI IRQ carries the data byte and nothing else, so the CS boundary
//! has to come from somewhere else. There are three rungs, tried in this order,
//! and [`SpiFramingMode`] reports which one a bus actually got:
//!
//! 1. **Exact, spec-declared** ([`CsProvenance::SpecDeclared`]): the run spec
//!    names the CS net (`cs_net = "..."`), the runner traces it to the MCU pin
//!    driving it, and the GPIO edge stream frames every transaction.
//! 2. **Exact, from model pin roles** ([`CsProvenance::ModelRoles`]): the spec
//!    names no `cs_net`, but the peripheral names a board component whose bound
//!    model declares a `cs` pin role, so the net is read off that pad. Same
//!    electrical fact as rung 1, so the same tier; the provenance stays visible
//!    because "the model told us" and "you told us" fail differently.
//! 3. **Heuristic**: neither a CS net nor a backend CS event, so the chunk
//!    boundary stands in for a CS deassert. This rung is *wrong* in two
//!    recorded directions and says so (see [`SpiFramingMode::Heuristic`]);
//!    declaring `cs_net`, or using a part the model DB maps a `cs` role for,
//!    moves the bus off it.
//!
//! Two things sit outside the ladder. [`SpiFramingMode::Backend`]: the emulator
//! surfaces CS itself (Renode hardware NSS) with no net resolved at all, and it
//! takes precedence when reported. And a bit-banged slave (05 §1.5), whose CS pin
//! comes from the GPIO wiring its responder was attached with rather than from any
//! net lookup, so it reaches the exact tier by a third route and labels itself
//! [`CsProvenance::BitBangPins`].
//!
//! ## Bus-speed honesty (chunk-rate limit)
//!
//! Like I2C, interception is byte-level through simavr's hardware SPI
//! peripheral: a `SPDR` write clocks one byte and raises one IRQ, all inside a
//! single `run_micros` chunk, so the SPI clock rate is whatever the firmware's
//! SPR/SPI2X prescaler sets and is not bounded by the analog chunk rate. A
//! *bit-banged* SPI master (software-toggled SCK/MOSI on GPIO) is handled too,
//! and no longer at the chunk poll rate: the scheduler replays each pin
//! transition in cycle order from its edge log, so
//! [`crate::responders::BitBangSpiResponder`] sees sub-chunk SCLK edges as the
//! firmware produced them. Pinned end to end by
//! `crates/hauksbee-engine/tests/bitbang_spi_cosim.rs`. These slaves bind to
//! both the AVR backend (simavr SPI IRQ) and the Renode backend (the C# SPI
//! bridge in `hauksbee-mcu/src/renode`), routing every reply byte through
//! `on_spi` either way.

use std::collections::HashMap;

use super::{BusActivity, Peripheral, TickCtx};

/// A device on the SPI bus that exchanges one byte per transfer.
pub trait SpiSlave: Send {
    /// Exchange a byte: receive the firmware's MOSI byte, return MISO.
    fn transfer(&mut self, mosi: u8) -> u8;

    /// Chip-select ASSERTED (active-low falling edge): begin a fresh transaction
    /// by resetting the command state machine to its start-of-transaction state
    /// (05 §2.1). The default reuses [`Self::deselect`], which for the built-in slaves
    /// resets the sequence/command counter to idle without disturbing latched
    /// permission bits (the 25xx `deselect` only clears the write-enable latch
    /// when it lands mid-WRITE, and a select edge never does, because the previous
    /// transaction already ended, so a WREN issued in its own transaction still
    /// persists to the following WRITE transaction). Slaves whose start state
    /// differs from their end state override this.
    fn select(&mut self) {
        self.deselect();
    }

    /// Chip-select deasserted: end the current transaction. On a bus whose CS
    /// net resolved (from the spec or from the bound model's `cs` pin role) this
    /// fires on the real active-low rising edge. Only on the heuristic rung,
    /// where no CS net and no backend CS event exist, does the engine call it at
    /// the co-sim chunk boundary instead, and there it is a guess: well-formed
    /// transfers complete within a chunk, boundary-spanning ones do not.
    fn deselect(&mut self) {}

    /// True if the slave is partway through a multi-byte transaction (i.e. it
    /// has consumed at least one byte but not yet seen a deselect). The engine
    /// uses this only as a debug-time sanity check: a chunk-boundary deselect
    /// that lands here means a transfer spanned the boundary, which the
    /// chunk-cadence deselect heuristic cannot frame correctly. Default `false`
    /// for stateless slaves.
    fn mid_transaction(&self) -> bool {
        false
    }

    /// The byte this slave would shift out on the NEXT [`SpiSlave::transfer`],
    /// when that byte is already determined by prior traffic, `None` when the
    /// model cannot say. Must be non-advancing: calling it any number of times
    /// must not change the observable byte stream.
    ///
    /// Physical grounding: a real slave preloads its output shift register
    /// before the master clocks, so MISO byte N can never depend on MOSI
    /// byte N. The byte-level `transfer(mosi) -> miso` API conflates the two
    /// directions; this hook un-conflates them for bit-level consumers (the
    /// bit-banged SPI responder, 05 §1.5), which must present MISO bits before
    /// the master's byte has finished arriving. The responder cross-checks the
    /// preview against the eventual `transfer` return and refuses loudly on a
    /// mismatch, so a model whose reply genuinely depends on the incoming byte
    /// fails loud, never silently wrong.
    ///
    /// The default `None` leaves byte-level buses unaffected (they never call
    /// this); implement it on any slave that should be readable over
    /// bit-banged SPI.
    fn miso_preview(&mut self) -> Option<u8> {
        None
    }

    /// The datasheet-declared SPI clock mode `(CPOL, CPHA)` this slave expects
    /// the master to clock: `0 = (0,0)`, `1 = (0,1)`, `2 = (1,0)`, `3 = (1,1)`.
    /// Only the bit-banged SPI responder (05 §1.5), which reconstructs the wire
    /// timing from GPIO edges, consults this; the byte-level `on_spi` path is
    /// mode-agnostic (simavr's hardware SPI already clocks the configured mode).
    /// Default `0` (CPOL=0, CPHA=0); the historical assumption.
    fn spi_mode(&self) -> u8 {
        0
    }

    fn state(&self) -> HashMap<String, f64> {
        HashMap::new()
    }

    /// Drain protocol work performed since the previous analogue chunk.
    fn take_activity(&mut self) -> BusActivity {
        BusActivity::default()
    }

    /// Whether the slave is in a protocol-defined low-power state whose rail
    /// current can be selected by `[models.peripheral_power].low_power_a`.
    fn low_power_mode(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// Where the chip-select net that produced exact framing came from.
///
/// Both values give the same electrical fact (this net is the slave's CS) and
/// therefore the same framing tier; they differ in who supplied it, which is
/// what a reader needs to know to reproduce or override the result. Carried
/// inside [`SpiFramingMode::Exact`] so it cannot drift away from the tier it
/// justifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CsProvenance {
    /// The run spec named the net (`cs_net = "..."` on the peripheral). Always
    /// wins over the model-role route, so a board whose model map is wrong or
    /// incomplete stays overridable from the spec.
    SpecDeclared,
    /// No `cs_net` in the spec: the net was read off the bound model's `cs` pin
    /// role for the board component the peripheral names. Only an assembled,
    /// identity-trusted part can supply one (the binder's `FittedComponent`
    /// witness is the only door to a model), so a DNP or identity-refused slave
    /// contributes nothing and the bus stays on the heuristic.
    ModelRoles,
    /// The bus is a bit-banged SPI slave (05 §1.5) whose CS pin came from the
    /// GPIO wiring the responder was attached with, not from a `cs_net` and not
    /// from a model pad map. The framing is real (the responder owns the CS
    /// edges), but neither of the other two labels would be true of it, and a
    /// report that said `spec` here would be claiming a declaration nobody made.
    BitBangPins,
}

impl CsProvenance {
    /// Lower-case tag for JSON coverage: `"spec"` | `"model-roles"` |
    /// `"bitbang-pins"`.
    pub fn as_str(self) -> &'static str {
        match self {
            CsProvenance::SpecDeclared => "spec",
            CsProvenance::ModelRoles => "model-roles",
            CsProvenance::BitBangPins => "bitbang-pins",
        }
    }

    /// The clause a report appends after the `exact` tier, naming who supplied
    /// the CS net.
    pub fn describe(self) -> &'static str {
        match self {
            CsProvenance::SpecDeclared => "CS net declared by the spec",
            CsProvenance::ModelRoles => "CS net resolved from the bound model's pin roles",
            CsProvenance::BitBangPins => "CS pin taken from the bit-banged SPI wiring",
        }
    }
}

/// How a bus's transactions are being framed, surfaced per-slave in the co-sim
/// coverage so a consumer knows whether the CS boundaries are real or guessed
/// (05 §2.1). Precedence when reported: `Backend` (a real backend CS event was
/// observed) over `Exact` (a CS pin resolved) over `Heuristic`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpiFramingMode {
    /// Framed from the real CS-pin GPIO edge stream: the CS net resolved to the
    /// MCU pin that drives it, so `select`/`deselect` fire at the true active-low
    /// falling/rising edges, interleaved in cycle order with the byte transfers.
    /// Exact on push backends (simavr). The [`CsProvenance`] says which source
    /// named the net; the tier is the same either way, because the electrical
    /// fact is the same.
    Exact(CsProvenance),
    /// The backend surfaces CS itself (Renode hardware-NSS `FinishTransmission`
    /// -> a `deselect` `SpiEvent`), which frames the transaction precisely with no
    /// resolved CS pin. Detected dynamically the first time such an event lands.
    Backend,
    /// No resolved CS pin and no backend CS event: the chunk-boundary deselect
    /// heuristic frames transactions. Wrong in two documented ways (two
    /// transactions in one chunk merge; a boundary-spanning transaction
    /// truncates), reported honestly rather than silently guessed.
    Heuristic,
}

impl SpiFramingMode {
    /// Lower-case tag for JSON coverage: `"exact"` | `"backend"` | `"heuristic"`.
    pub fn as_str(self) -> &'static str {
        match self {
            SpiFramingMode::Exact(_) => "exact",
            SpiFramingMode::Backend => "backend",
            SpiFramingMode::Heuristic => "heuristic",
        }
    }

    /// Where the CS net came from, on the exact tier only. `None` on the backend
    /// tier (the emulator framed it, no net was resolved) and on the heuristic
    /// tier (there was no net to resolve).
    pub fn cs_provenance(self) -> Option<CsProvenance> {
        match self {
            SpiFramingMode::Exact(p) => Some(p),
            SpiFramingMode::Backend | SpiFramingMode::Heuristic => None,
        }
    }
}

/// A chip-select the caller actually resolved: the MCU pin that drives it, the
/// net it was traced from (when known), and who supplied that net.
///
/// One value rather than three loose arguments, because they are only meaningful
/// together: a pin with no net, or a pin labelled with a route that did not
/// produce it, would have the coverage report a tier justified by evidence nobody
/// held. `Option<ResolvedCs>` also says the thing the old
/// `(Option<pin>, Option<net>)` pair could not: either a chip select was resolved
/// or it was not.
///
/// This groups the evidence, it does not police it. The fields are plain data and
/// any caller inside the workspace can assert whatever it likes; what stops a
/// fabricated chip-select reaching a real run is the resolution path in
/// `hauksbee-ci`'s `resolve_cs_pin`, which is the only production constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedCs<N> {
    /// The MCU pin `(port, bit)` driving the chip-select net.
    pub pin: (char, u8),
    /// The CS net node, so the scheduler installs the frame on the MCU that
    /// actually drives THIS net rather than the first one owning the same
    /// chip-local `(port, bit)` tuple. `None` for callers that resolved a pin by
    /// other means (the bit-banged wiring).
    pub net: Option<N>,
    /// Which route produced the net. See [`CsProvenance`].
    pub provenance: CsProvenance,
}

/// The SPI bus peripheral: one slave whose byte stream is fed from the MCU's
/// `on_spi` callback, gated by this bus's chip-select so several buses can share
/// one controller.
pub struct SpiBus {
    id: String,
    slave: Box<dyn SpiSlave>,
    /// The MCU pin (port, bit) that drives this slave's chip-select net, when
    /// the binder resolved it (05 §2.1). `Some` selects real CS-edge framing;
    /// `None` leaves the bus on the chunk-boundary heuristic.
    cs_pin: Option<(char, u8)>,
    /// Who named the CS net that `cs_pin` was traced from. Set by whoever built
    /// the bus (the CI runner), because the scheduler only ever sees the already
    /// resolved pin and cannot tell a spec-declared net from a model-role one.
    /// Only meaningful when `cs_pin` is `Some`; [`SpiBus::set_cs_pin`] sets the
    /// two together so a resolved pin can never carry a provenance nobody earned.
    cs_provenance: CsProvenance,
    /// Set once a `deselect` `SpiEvent` from the backend framed this bus (Renode
    /// hardware-NSS `FinishTransmission`). Makes `framing_mode` report `Backend`
    /// so the coverage reflects that the backend, not the heuristic, owns framing.
    backend_deselect_seen: bool,
    /// The slave's declared SPI clock mode (`spi_mode()`), cached at construction
    /// so the bit-banged responder can read it without re-locking the slave. See
    /// [`SpiSlave::spi_mode`].
    spi_mode: u8,
    /// Whether this bus's chip-select is currently ASSERTED. On a multi-slave SPI
    /// controller the scheduler's shared `on_spi` handler routes each byte to the
    /// SELECTED bus (only one CS is asserted at a time), so the deselected slaves
    /// never see traffic addressed to a sibling. A bus with a resolved CS pin
    /// starts deselected (its CS edge selects it); a heuristic bus with no CS pin
    /// is always considered selected, because there is no CS to gate it. Nothing
    /// enforces that such a bus is the controller's only slave: two of them and
    /// the dispatcher gives every byte to the first, which is why the heuristic
    /// is only correct for a lone slave and why `framing_mode` reports it.
    /// See [`Scheduler::attach_spi_bus`].
    selected: bool,
    powered: bool,
}

impl SpiBus {
    pub fn new(id: &str, slave: Box<dyn SpiSlave>) -> Self {
        let spi_mode = slave.spi_mode();
        SpiBus {
            id: id.to_string(),
            slave,
            cs_pin: None,
            cs_provenance: CsProvenance::SpecDeclared,
            backend_deselect_seen: false,
            spi_mode,
            // A fresh bus with no CS pin yet is the single-slave / heuristic case:
            // treat it as selected so a lone slave always receives traffic.
            // `set_cs_pin(Some)` flips it to start deselected until its CS asserts.
            selected: true,
            powered: true,
        }
    }

    /// The declared SPI clock mode (0..=3) of this bus's slave, cached from
    /// [`SpiSlave::spi_mode`] at construction. Consumed by the bit-banged SPI
    /// responder to time its sample/shift edges (05 §1.5).
    pub fn spi_mode(&self) -> u8 {
        self.spi_mode
    }

    /// The MCU pin driving this slave's chip-select, if resolved.
    pub fn cs_pin(&self) -> Option<(char, u8)> {
        self.cs_pin
    }

    /// Record the resolved CS pin and, atomically, where it came from (called by
    /// the scheduler at attach time). A resolved pin moves the bus onto exact
    /// CS-edge framing, and the tier is reported with its [`CsProvenance`].
    ///
    /// The two arguments travel together on purpose. When provenance was a
    /// separate setter with a default, any attach path that resolved a pin and
    /// forgot to set it reported `exact (CS net declared by the spec)` for a bus
    /// no spec had declared a net for. Making the caller say both means a new
    /// route onto the exact tier cannot silently inherit someone else's story.
    pub fn set_cs_pin(&mut self, pin: Option<(char, u8)>, provenance: CsProvenance) {
        self.cs_provenance = provenance;
        // A resolved CS pin means selection is driven by the CS edge, so start
        // DESELECTED until the firmware asserts it. A bus with no CS pin stays
        // selected (the heuristic single-slave case).
        self.selected = pin.is_none();
        self.cs_pin = pin;
    }

    /// Whether this bus's chip-select is currently asserted. The scheduler's
    /// shared `on_spi` handler routes a byte to the selected bus when more than
    /// one SPI slave is on the controller.
    pub fn is_selected(&self) -> bool {
        self.selected
    }

    /// Which framing tier this bus is actually running on, for coverage.
    pub fn framing_mode(&self) -> SpiFramingMode {
        if self.backend_deselect_seen {
            SpiFramingMode::Backend
        } else if self.cs_pin.is_some() {
            SpiFramingMode::Exact(self.cs_provenance)
        } else {
            SpiFramingMode::Heuristic
        }
    }

    /// True when this bus frames its own transactions from a real CS source (a
    /// resolved CS pin OR a backend CS event), so the scheduler must NOT apply
    /// the chunk-boundary deselect heuristic to it, since doing so would truncate a
    /// transaction that legitimately spans a chunk boundary (05 §2, failure mode
    /// b: the debug warning below must stop firing on these buses).
    pub fn frames_itself(&self) -> bool {
        self.cs_pin.is_some() || self.backend_deselect_seen
    }

    /// Exchange one byte (body of the `on_spi` closure).
    pub fn transfer(&mut self, mosi: u8) -> u8 {
        if self.powered {
            self.slave.transfer(mosi)
        } else {
            0xff
        }
    }

    pub fn set_powered(&mut self, powered: bool) {
        if self.powered && !powered {
            self.selected = self.cs_pin.is_none();
            self.slave.deselect();
        }
        self.powered = powered;
    }

    pub fn powered(&self) -> bool {
        self.powered
    }

    pub fn take_activity(&mut self) -> BusActivity {
        self.slave.take_activity()
    }

    pub fn low_power_mode(&self) -> bool {
        self.slave.low_power_mode()
    }

    /// The slave's next MISO byte, when determined (see
    /// [`SpiSlave::miso_preview`]). Called by the bit-banged SPI responder at
    /// each byte boundary; the byte-level `on_spi` path never calls this.
    pub fn miso_preview(&mut self) -> Option<u8> {
        self.slave.miso_preview()
    }

    /// CS ASSERTED (active-low falling edge): begin a transaction. Interleaved in
    /// cycle order with `transfer` on the exact-framing path (05 §2.1).
    pub fn cs_assert(&mut self) {
        self.selected = true;
        self.slave.select();
    }

    /// CS DEASSERTED (active-low rising edge): end the transaction.
    pub fn cs_deassert(&mut self) {
        self.selected = false;
        self.slave.deselect();
    }

    /// A backend-surfaced CS deassert (`deselect` `SpiEvent`). Records that the
    /// backend frames CS (so coverage reports `Backend`) and ends the transaction.
    pub fn note_backend_deselect(&mut self) {
        self.backend_deselect_seen = true;
        self.selected = false;
        self.slave.deselect();
    }

    /// Deselect the slave (chip-select deassert). Called either from the
    /// `on_spi` closure when the backend surfaces a CS-deassert event
    /// (Renode `FinishTransmission`), or from `post_solve` at each chunk
    /// boundary for backends that don't surface CS.
    pub fn slave_deselect(&mut self) {
        self.slave.deselect();
    }

    /// True if the active slave is partway through a transaction. See
    /// [`SpiSlave::mid_transaction`]; used for the scheduler's debug-time
    /// chunk-boundary sanity check.
    pub fn slave_mid_transaction(&self) -> bool {
        self.slave.mid_transaction()
    }

    /// The bus identifier (the reference designator passed to [`SpiBus::new`]).
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn slave<T: 'static>(&self) -> Option<&T> {
        self.slave.as_any().downcast_ref::<T>()
    }
    pub fn slave_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.slave.as_any_mut().downcast_mut::<T>()
    }
}

impl Peripheral for SpiBus {
    fn id(&self) -> &str {
        SpiBus::id(self)
    }
    fn kind(&self) -> &'static str {
        "spi_bus"
    }

    fn post_solve(&mut self, _ctx: &mut TickCtx) {
        // Treat the chunk boundary as a CS deassert so the slave resets its
        // command state machine for the next transaction.
        self.slave.deselect();
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut state = self.slave.state();
        state.insert("powered".into(), if self.powered { 1.0 } else { 0.0 });
        state
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 25xx SPI EEPROM
// ─────────────────────────────────────────────────────────────────────────────

/// 25xx (25LCxxx / AT25) SPI EEPROM. Instruction set (datasheet):
///   0x06 WREN, 0x04 WRDI, 0x05 RDSR, 0x01 WRSR,
///   0x03 READ  (1 instr + 2 addr bytes, then read data),
///   0x02 WRITE (1 instr + 2 addr bytes, then write data).
/// 16-bit addressing, auto-incrementing within a transfer.
pub struct Spi25Eeprom {
    mem: Vec<u8>,
    state: SpiCmd,
    addr: u16,
    addr_bytes: u8,
    wel: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum SpiCmd {
    Idle,
    Read,
    Write,
    Rdsr,
    Wrsr,
}

impl Spi25Eeprom {
    pub fn new(size: usize) -> Self {
        Spi25Eeprom {
            mem: vec![0xFF; size.max(1)],
            state: SpiCmd::Idle,
            addr: 0,
            addr_bytes: 0,
            wel: false,
        }
    }

    pub fn contents(&self) -> &[u8] {
        &self.mem
    }

    pub fn contains(&self, needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        self.mem.windows(needle.len()).any(|w| w == needle)
    }
}

impl SpiSlave for Spi25Eeprom {
    fn transfer(&mut self, mosi: u8) -> u8 {
        match self.state {
            SpiCmd::Idle => match mosi {
                0x06 => {
                    self.wel = true;
                    0xFF
                }
                0x04 => {
                    self.wel = false;
                    0xFF
                }
                0x05 => {
                    self.state = SpiCmd::Rdsr;
                    0xFF
                }
                0x01 => {
                    self.state = SpiCmd::Wrsr;
                    0xFF
                }
                0x03 => {
                    self.state = SpiCmd::Read;
                    self.addr_bytes = 0;
                    self.addr = 0;
                    0xFF
                }
                0x02 => {
                    self.state = SpiCmd::Write;
                    self.addr_bytes = 0;
                    self.addr = 0;
                    0xFF
                }
                _ => 0xFF,
            },
            SpiCmd::Rdsr => {
                // Status: WEL in bit 1.
                let sr = if self.wel { 0x02 } else { 0x00 };
                self.state = SpiCmd::Idle;
                sr
            }
            SpiCmd::Wrsr => {
                self.state = SpiCmd::Idle;
                0xFF
            }
            SpiCmd::Read => {
                if self.addr_bytes < 2 {
                    self.addr = (self.addr << 8) | mosi as u16;
                    self.addr_bytes += 1;
                    0xFF
                } else {
                    let b = self
                        .mem
                        .get(self.addr as usize % self.mem.len())
                        .copied()
                        .unwrap_or(0xFF);
                    self.addr = self.addr.wrapping_add(1);
                    b
                }
            }
            SpiCmd::Write => {
                if self.addr_bytes < 2 {
                    self.addr = (self.addr << 8) | mosi as u16;
                    self.addr_bytes += 1;
                    0xFF
                } else {
                    if self.wel {
                        let i = self.addr as usize % self.mem.len();
                        self.mem[i] = mosi;
                    }
                    self.addr = self.addr.wrapping_add(1);
                    0xFF
                }
            }
        }
    }

    fn deselect(&mut self) {
        if self.state == SpiCmd::Write {
            self.wel = false; // write latch clears after a write transaction
        }
        self.state = SpiCmd::Idle;
        self.addr_bytes = 0;
    }

    fn mid_transaction(&self) -> bool {
        self.state != SpiCmd::Idle
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("size".into(), self.mem.len() as f64);
        m.insert("wel".into(), if self.wel { 1.0 } else { 0.0 });
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// JEDEC SPI NOR flash
// ──────────────────────────────────────────────────────────────────────────────

/// Conservative byte-level JEDEC SPI NOR behavior for boot and update flows.
///
/// The primitive implements WREN/WRDI, status reads, JEDEC identity,
/// normal/fast reads, page program, 4 KiB sector erase, chip erase, and deep
/// power-down/release. Programming preserves NOR physics (`1 -> 0` only), page
/// writes wrap within the selected page, and mutating commands require WEL.
/// Busy timing, protection, SFDP, and dual/quad/QPI transfers are outside it.
pub struct SpiNorFlash {
    mem: Vec<u8>,
    state: NorCmd,
    addr: usize,
    addr_bytes: u8,
    page_base: usize,
    page_offset: usize,
    page_size: usize,
    sector_size: usize,
    wel: bool,
    powered_down: bool,
    jedec_id: [u8; 3],
    jedec_index: usize,
    spi_mode: u8,
    activity: BusActivity,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NorCmd {
    Idle,
    Read,
    FastReadAddress,
    FastReadDummy,
    Program,
    Erase4k,
    Status1,
    Status2,
    Jedec,
}

impl SpiNorFlash {
    pub fn new(
        size: usize,
        page_size: usize,
        sector_size: usize,
        jedec_id: [u8; 3],
        spi_mode: u8,
    ) -> Self {
        assert!(size > 0, "SPI NOR size must be positive");
        assert!(
            page_size.is_power_of_two() && page_size <= size,
            "SPI NOR page size must be a power of two no larger than the array"
        );
        assert!(
            sector_size.is_power_of_two() && sector_size <= size,
            "SPI NOR sector size must be a power of two no larger than the array"
        );
        assert!(spi_mode <= 3, "SPI mode must be 0..3");
        Self {
            mem: vec![0xff; size],
            state: NorCmd::Idle,
            addr: 0,
            addr_bytes: 0,
            page_base: 0,
            page_offset: 0,
            page_size,
            sector_size,
            wel: false,
            powered_down: false,
            jedec_id,
            jedec_index: 0,
            spi_mode,
            activity: BusActivity::default(),
        }
    }

    pub fn contents(&self) -> &[u8] {
        &self.mem
    }

    fn begin_address(&mut self, state: NorCmd) {
        self.state = state;
        self.addr = 0;
        self.addr_bytes = 0;
    }

    fn address_byte(&mut self, byte: u8) -> bool {
        self.addr = ((self.addr << 8) | byte as usize) % self.mem.len();
        self.addr_bytes += 1;
        self.addr_bytes == 3
    }

    fn erase_sector(&mut self) {
        if !self.wel {
            return;
        }
        let base = self.addr - (self.addr % self.sector_size);
        let end = (base + self.sector_size).min(self.mem.len());
        self.mem[base..end].fill(0xff);
        self.wel = false;
    }
}

impl SpiSlave for SpiNorFlash {
    fn transfer(&mut self, mosi: u8) -> u8 {
        if self.powered_down {
            if self.state == NorCmd::Idle && mosi == 0xab {
                self.powered_down = false;
            }
            return 0xff;
        }

        match self.state {
            NorCmd::Idle => {
                match mosi {
                    0x06 => self.wel = true,
                    0x04 => self.wel = false,
                    0x05 => self.state = NorCmd::Status1,
                    0x35 => self.state = NorCmd::Status2,
                    0x9f => {
                        self.state = NorCmd::Jedec;
                        self.jedec_index = 0;
                    }
                    0x03 => self.begin_address(NorCmd::Read),
                    0x0b => self.begin_address(NorCmd::FastReadAddress),
                    0x02 => self.begin_address(NorCmd::Program),
                    0x20 => self.begin_address(NorCmd::Erase4k),
                    0x60 | 0xc7 if self.wel => {
                        self.mem.fill(0xff);
                        self.wel = false;
                        self.activity.write_units = self.activity.write_units.saturating_add(1);
                    }
                    0xb9 => self.powered_down = true,
                    0xab => {}
                    _ => {}
                }
                0xff
            }
            NorCmd::Status1 => {
                self.activity.read_units = self.activity.read_units.saturating_add(1);
                if self.wel {
                    0x02
                } else {
                    0x00
                }
            }
            NorCmd::Status2 => {
                self.activity.read_units = self.activity.read_units.saturating_add(1);
                0x00
            }
            NorCmd::Jedec => {
                let out = self.jedec_id.get(self.jedec_index).copied().unwrap_or(0xff);
                self.jedec_index += 1;
                self.activity.read_units = self.activity.read_units.saturating_add(1);
                out
            }
            NorCmd::Read => {
                if self.addr_bytes < 3 {
                    self.address_byte(mosi);
                    0xff
                } else {
                    let out = self.mem[self.addr];
                    self.addr = (self.addr + 1) % self.mem.len();
                    self.activity.read_units = self.activity.read_units.saturating_add(1);
                    out
                }
            }
            NorCmd::FastReadAddress => {
                if self.address_byte(mosi) {
                    self.state = NorCmd::FastReadDummy;
                }
                0xff
            }
            NorCmd::FastReadDummy => {
                self.state = NorCmd::Read;
                self.addr_bytes = 3;
                0xff
            }
            NorCmd::Program => {
                if self.addr_bytes < 3 {
                    if self.address_byte(mosi) {
                        self.page_base = self.addr - (self.addr % self.page_size);
                        self.page_offset = self.addr % self.page_size;
                    }
                } else if self.wel {
                    let i = self.page_base + self.page_offset;
                    if i < self.mem.len() {
                        self.mem[i] &= mosi;
                    }
                    self.page_offset = (self.page_offset + 1) % self.page_size;
                    self.activity.write_units = self.activity.write_units.saturating_add(1);
                }
                0xff
            }
            NorCmd::Erase4k => {
                if self.address_byte(mosi) {
                    let was_enabled = self.wel;
                    self.erase_sector();
                    if was_enabled {
                        self.activity.write_units = self.activity.write_units.saturating_add(1);
                    }
                    self.state = NorCmd::Idle;
                }
                0xff
            }
        }
    }

    fn select(&mut self) {
        self.state = NorCmd::Idle;
        self.addr = 0;
        self.addr_bytes = 0;
        self.jedec_index = 0;
    }

    fn deselect(&mut self) {
        if self.state == NorCmd::Program && self.addr_bytes == 3 {
            self.wel = false;
        }
        self.state = NorCmd::Idle;
        self.addr_bytes = 0;
        self.jedec_index = 0;
    }

    fn mid_transaction(&self) -> bool {
        self.state != NorCmd::Idle
    }

    fn miso_preview(&mut self) -> Option<u8> {
        match self.state {
            NorCmd::Status1 => Some(if self.wel { 0x02 } else { 0x00 }),
            NorCmd::Status2 => Some(0x00),
            NorCmd::Jedec => Some(self.jedec_id.get(self.jedec_index).copied().unwrap_or(0xff)),
            NorCmd::Read if self.addr_bytes == 3 => Some(self.mem[self.addr]),
            _ => Some(0xff),
        }
    }

    fn spi_mode(&self) -> u8 {
        self.spi_mode
    }

    fn state(&self) -> HashMap<String, f64> {
        HashMap::from([
            ("size".into(), self.mem.len() as f64),
            ("wel".into(), if self.wel { 1.0 } else { 0.0 }),
            (
                "powered_down".into(),
                if self.powered_down { 1.0 } else { 0.0 },
            ),
        ])
    }

    fn take_activity(&mut self) -> BusActivity {
        std::mem::take(&mut self.activity)
    }

    fn low_power_mode(&self) -> bool {
        self.powered_down
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ───────────────────────────────────────────────────────────────────────────
// MCP3008 8-channel 10-bit SPI ADC
// ─────────────────────────────────────────────────────────────────────────────

/// MCP3008 8-channel 10-bit ADC. Protocol (datasheet): the master clocks a
/// start bit, a SGL/DIFF + channel-select nibble, then reads back a null bit
/// and 10 data bits. The common 3-byte transfer is:
///   byte0 = 0x01 (start bit), byte1 = (SGL<<7)|(chan<<4), byte2 = 0x00.
/// The slave returns the 10-bit result split across byte1's low 2 bits and all
/// of byte2. Channel voltages are settable (0..vref) and converted to counts.
pub struct Mcp3008 {
    vref: f64,
    channels: [f64; 8],
    /// Bytes seen in the current transfer.
    seq: u8,
    sel_chan: usize,
    result: u16,
}

impl Mcp3008 {
    pub fn new(vref: f64) -> Self {
        Mcp3008 {
            vref: vref.max(1e-6),
            channels: [0.0; 8],
            seq: 0,
            sel_chan: 0,
            result: 0,
        }
    }

    /// Set a channel's input voltage (clamped to 0..vref).
    pub fn set_channel(&mut self, ch: usize, volts: f64) {
        if ch < 8 {
            self.channels[ch] = volts.clamp(0.0, self.vref);
        }
    }

    fn counts(&self, ch: usize) -> u16 {
        // MCP3008 datasheet (DS21295) transfer function: code = 1024 * Vin/Vref,
        // i.e. LSB = Vref/2^n = Vref/1024, with the code saturating at 2^n-1 =
        // 1023. Multiplying the fraction by 1023 (2^n-1) systematically
        // under-reads by up to ~1 LSB; use the 2^n full scale with a top-code
        // clamp for the Vin==Vref edge.
        let frac = (self.channels[ch] / self.vref).clamp(0.0, 1.0);
        ((frac * 1024.0).round() as u16).min(1023)
    }
}

impl SpiSlave for Mcp3008 {
    fn transfer(&mut self, mosi: u8) -> u8 {
        let out = match self.seq {
            0 => 0x00, // start byte; MISO undefined
            1 => {
                // Channel select in bits 6..4 (after SGL bit 7).
                self.sel_chan = ((mosi >> 4) & 0x07) as usize;
                self.result = self.counts(self.sel_chan);
                // Return high 2 bits of the 10-bit result (with leading null 0).
                ((self.result >> 8) & 0x03) as u8
            }
            _ => (self.result & 0xFF) as u8, // low 8 bits
        };
        self.seq = self.seq.saturating_add(1);
        out
    }

    fn deselect(&mut self) {
        self.seq = 0;
    }

    fn mid_transaction(&self) -> bool {
        self.seq != 0
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("vref".into(), self.vref);
        for (i, v) in self.channels.iter().enumerate() {
            m.insert(format!("ch{i}"), *v);
        }
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eeprom_write_read_roundtrip() {
        let mut bus = SpiBus::new("SPI", Box::new(Spi25Eeprom::new(256)));
        // WREN, then WRITE 0x0000 'O' 'K'.
        bus.transfer(0x06); // WREN
        bus.transfer(0x02); // WRITE
        bus.transfer(0x00);
        bus.transfer(0x00);
        bus.transfer(b'O');
        bus.transfer(b'K');
        // CS deassert (chunk boundary).
        bus.slave_mut::<Spi25Eeprom>().unwrap().deselect();
        // READ back.
        bus.transfer(0x03);
        bus.transfer(0x00);
        bus.transfer(0x00);
        let o = bus.transfer(0x00);
        let k = bus.transfer(0x00);
        assert_eq!([o, k], [b'O', b'K']);
        assert!(bus.slave::<Spi25Eeprom>().unwrap().contains(b"OK"));
    }

    #[test]
    fn spi_nor_reports_jedec_and_preserves_page_and_nor_semantics() {
        let mut flash = SpiNorFlash::new(8192, 8, 4096, [0xef, 0x40, 0x18], 0);

        flash.select();
        assert_eq!(flash.transfer(0x9f), 0xff);
        assert_eq!(flash.miso_preview(), Some(0xef));
        assert_eq!(
            [
                flash.transfer(0x00),
                flash.transfer(0x00),
                flash.transfer(0x00),
            ],
            [0xef, 0x40, 0x18]
        );
        flash.deselect();

        // WREN is its own transaction. Program at the final byte of an 8-byte
        // page; the second byte wraps to the first byte of that same page.
        flash.select();
        flash.transfer(0x06);
        flash.deselect();
        flash.select();
        for byte in [0x02, 0x00, 0x00, 0x07, 0xaa, 0x55] {
            flash.transfer(byte);
        }
        flash.deselect();
        assert_eq!(flash.contents()[7], 0xaa);
        assert_eq!(flash.contents()[0], 0x55);

        // A second program can clear more bits, but cannot turn a zero back to
        // one without erase: 0x55 & 0xff remains 0x55.
        flash.select();
        flash.transfer(0x06);
        flash.deselect();
        flash.select();
        for byte in [0x02, 0x00, 0x00, 0x00, 0xff] {
            flash.transfer(byte);
        }
        flash.deselect();
        assert_eq!(flash.contents()[0], 0x55);
    }

    #[test]
    fn spi_nor_sector_erase_requires_write_enable() {
        let mut flash = SpiNorFlash::new(8192, 256, 4096, [0xef, 0x40, 0x18], 0);

        flash.select();
        flash.transfer(0x06);
        flash.deselect();
        flash.select();
        for byte in [0x02, 0x00, 0x00, 0x10, 0x00] {
            flash.transfer(byte);
        }
        flash.deselect();
        assert_eq!(flash.contents()[0x10], 0x00);

        // No WREN: erase is ignored.
        flash.select();
        for byte in [0x20, 0x00, 0x00, 0x10] {
            flash.transfer(byte);
        }
        flash.deselect();
        assert_eq!(flash.contents()[0x10], 0x00);

        flash.select();
        flash.transfer(0x06);
        flash.deselect();
        flash.select();
        for byte in [0x20, 0x00, 0x00, 0x10] {
            flash.transfer(byte);
        }
        flash.deselect();
        assert_eq!(flash.contents()[0x10], 0xff);
    }

    #[test]
    fn spi_nor_reports_real_work_and_refuses_traffic_when_unpowered() {
        let flash = SpiNorFlash::new(8192, 256, 4096, [0xef, 0x40, 0x18], 0);
        let mut bus = SpiBus::new("FLASH", Box::new(flash));

        bus.set_powered(false);
        bus.cs_assert();
        assert_eq!(bus.transfer(0x9f), 0xff);
        assert_eq!(bus.transfer(0x00), 0xff);
        bus.cs_deassert();
        assert!(bus.take_activity().is_idle());

        bus.set_powered(true);
        bus.cs_assert();
        assert_eq!(bus.transfer(0x9f), 0xff);
        assert_eq!(bus.transfer(0x00), 0xef);
        assert_eq!(bus.transfer(0x00), 0x40);
        assert_eq!(bus.transfer(0x00), 0x18);
        bus.cs_deassert();
        let read = bus.take_activity();
        assert_eq!(read.read_units, 3);
        assert_eq!(read.write_units, 0);

        bus.cs_assert();
        bus.transfer(0x06);
        bus.cs_deassert();
        bus.cs_assert();
        for byte in [0x02, 0x00, 0x00, 0x10, 0xaa] {
            bus.transfer(byte);
        }
        bus.cs_deassert();
        let write = bus.take_activity();
        assert_eq!(write.write_units, 1);
        assert_eq!(bus.slave::<SpiNorFlash>().unwrap().contents()[0x10], 0xaa);

        bus.cs_assert();
        bus.transfer(0xb9);
        bus.cs_deassert();
        assert!(
            bus.low_power_mode(),
            "deep-power-down is observable by the rail model"
        );
        bus.cs_assert();
        bus.transfer(0xab);
        bus.cs_deassert();
        assert!(
            !bus.low_power_mode(),
            "release command restores the active current class"
        );

        bus.set_powered(false);
        bus.cs_assert();
        for byte in [0x06, 0x02, 0x00, 0x00, 0x10, 0x00] {
            assert_eq!(bus.transfer(byte), 0xff);
        }
        bus.cs_deassert();
        assert_eq!(bus.slave::<SpiNorFlash>().unwrap().contents()[0x10], 0xaa);
        assert!(bus.take_activity().is_idle());
    }

    #[test]
    fn mcp3008_reads_channel_voltage() {
        let mut adc = Mcp3008::new(5.0);
        adc.set_channel(3, 2.5); // half scale -> ~512 counts
        let mut bus = SpiBus::new("SPI", Box::new(adc));
        bus.transfer(0x01); // start byte
        let hi = bus.transfer(3 << 4); // single-ended, channel 3
        let lo = bus.transfer(0x00);
        let counts = (((hi & 0x03) as u16) << 8) | lo as u16;
        // 2.5/5.0 * 1024 = 512.
        assert!((counts as i32 - 512).abs() <= 1, "counts {counts} not ~512");
    }

    #[test]
    fn mcp3008_uses_2n_full_scale_not_2n_minus_1() {
        // Datasheet transfer function is code = 1024 * Vin/Vref (LSB = Vref/1024),
        // saturating at 1023, NOT code = 1023 * Vin/Vref which under-reads by up
        // to ~1 LSB. Read the code directly off the converter.
        let read = |v: f64| {
            let mut adc = Mcp3008::new(5.0);
            adc.set_channel(0, v);
            let mut bus = SpiBus::new("SPI", Box::new(adc));
            bus.transfer(0x01);
            let hi = bus.transfer(0 << 4);
            let lo = bus.transfer(0x00);
            (((hi & 0x03) as u16) << 8) | lo as u16
        };
        // One-quarter scale: 1024 * 0.25 = 256 exactly (1023*0.25 = 255.75 → 256
        // by rounding, so pick a fraction where the two formulas diverge).
        // 1/1024 of Vref should read code 1 (LSB = Vref/1024).
        assert_eq!(
            read(5.0 / 1024.0),
            1,
            "one LSB (Vref/1024) must read code 1"
        );
        // Full scale saturates at 1023, never overflowing the 10-bit field.
        assert_eq!(read(5.0), 1023, "Vin==Vref must saturate at 1023");
        // Mid-scale is exact.
        assert_eq!(read(2.5), 512, "half scale = 1024*0.5 = 512");
    }
}
