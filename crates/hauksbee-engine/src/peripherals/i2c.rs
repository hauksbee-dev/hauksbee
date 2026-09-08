//! I2C slave framework and two concrete devices: a 24Cxx EEPROM and an LM75
//! temperature sensor with its real datasheet register map.
//!
//! ## How it plugs into the co-sim
//!
//! The MCU backend already surfaces firmware TWI activity as [`I2cEvent`]s
//! through `Mcu::on_i2c`. An [`I2cBus`] owns one or more [`I2cSlave`]s and is
//! registered as that callback: each event is dispatched to the slave whose
//! 7-bit address matches the transaction. A slave reacts to writes (pointer +
//! data bytes) and answers reads (register data clocked back to the firmware).
//!
//! The read path was the missing piece on AVR (the TWI hook ACK'd writes but
//! never injected a reply byte); it is wired now, so an LM75 read returns real
//! temperature bytes.
//!
//! ## Bus-speed honesty (chunk-rate limit)
//!
//! Interception is at the *transaction* level via simavr's TWI peripheral
//! model, not by sampling SCL/SDA edges. simavr clocks the whole TWI byte
//! internally and raises one IRQ per byte/condition, so the achievable bus
//! speed is whatever the firmware's TWI prescaler asks for (standard 100 kHz /
//! fast 400 kHz both work); it is not bounded by the analog chunk rate,
//! because the bytes are consumed inside a single `run_micros` call. A
//! *bit-banged* I2C master (GPIO toggling SCL/SDA in software) is a different
//! story: those edges alias at the chunk poll rate exactly like any other GPIO
//! (see docs/cosim/MCU.md), so this framework targets the hardware TWI peripheral.
//! For Renode the `on_i2c` hook is a documented no-op, so these slaves bind to
//! the AVR backend today.

use std::collections::HashMap;

use hauksbee_mcu::I2cEvent;

use super::{BusActivity, Peripheral, TickCtx};

/// A device that answers on the I2C bus at a fixed 7-bit address.
pub trait I2cSlave: Send {
    /// The device's 7-bit bus address.
    fn address(&self) -> u8;

    /// A START matched this device. `read` is the R/W bit (true = master read).
    fn on_start(&mut self, _read: bool) {}

    /// A data byte was written by the firmware.
    fn on_write(&mut self, _data: u8) {}

    /// The firmware is reading a byte; return the byte to clock back.
    fn on_read(&mut self) -> u8;

    /// A STOP condition ended a transaction addressed to this device (05 §3.1).
    ///
    /// The [`TickCtx`] is the same context the peripheral `pre_solve` /
    /// `post_solve` hooks receive, so a slave can convert its accumulated
    /// register writes into net voltages (e.g. a DAC driving its VOUT nets
    /// through their [`crate::drivers::PinDriver`]s).
    ///
    /// **Delivery is deferred to the chunk boundary.** The byte-level events
    /// arrive through the MCU's `on_i2c` callback during `run_micros`, where no
    /// `&mut Circuit` exists to build a `TickCtx` from. [`I2cBus::dispatch`]
    /// therefore only *records* the STOP; the scheduler delivers the hook once
    /// per chunk via [`I2cBus::flush_stops`], after the MCU ran and before the
    /// analog solve, which is the earliest moment the solver could see a
    /// driven net anyway (the chunk-rate limit documented in the module
    /// header). Transaction *state* resets must therefore not live here; every
    /// slave already re-arms on the next `on_start`.
    fn on_stop(&mut self, _ctx: &mut TickCtx) {}

    /// Numeric state for frame reporting.
    fn state(&self) -> HashMap<String, f64> {
        HashMap::new()
    }

    /// Drain protocol work performed since the previous analogue chunk.
    /// Slaves that do not opt in remain electrically idle rather than gaining
    /// an invented load profile.
    fn take_activity(&mut self) -> BusActivity {
        BusActivity::default()
    }

    /// If this is a temperature sensor, its current reading in milli-degrees
    /// Celsius. Lets a backend that runs the firmware against a real emulated
    /// I2C device (the QEMU ESP32 `tmp105`) push the modeled temperature into
    /// that device, instead of serving bytes through `on_read`. Default: None.
    fn temperature_mc(&self) -> Option<i32> {
        None
    }

    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// The I2C bus peripheral: a router over attached slaves. Register its
/// [`I2cBus::dispatch`] as the MCU's `on_i2c` callback.
pub struct I2cBus {
    id: String,
    /// Address of the slave currently addressed (set on START), if any matched.
    active: Option<u8>,
    /// Every modeled address the current transaction has touched since the
    /// last STOP. A repeated START (Sr) legitimately re-addresses the bus
    /// without a STOP, so this ADDS on each matched START; a STOP ends the
    /// WHOLE transaction and must reach every member, not just the
    /// last-addressed slave (a DAC written in leg one still commits its
    /// output net even when leg two re-addressed an EEPROM).
    txn_addrs: Vec<u8>,
    slaves: Vec<Box<dyn I2cSlave>>,
    /// Addresses that saw a STOP since the last [`I2cBus::flush_stops`]. The
    /// ctx-bearing `on_stop` cannot run inside the MCU callback (no `TickCtx`
    /// exists there), so `dispatch` records the STOP here and the scheduler
    /// delivers it at the chunk boundary.
    stop_pending: Vec<u8>,
    /// Below a model card's source-bound operating threshold the bus device is
    /// electrically off: it does not ACK, mutate storage, or return data.
    powered: bool,
}

impl I2cBus {
    pub fn new(id: &str) -> Self {
        I2cBus {
            id: id.to_string(),
            active: None,
            txn_addrs: Vec::new(),
            slaves: Vec::new(),
            stop_pending: Vec::new(),
            powered: true,
        }
    }

    pub fn with_slave(mut self, slave: Box<dyn I2cSlave>) -> Self {
        self.slaves.push(slave);
        self
    }

    pub fn add_slave(&mut self, slave: Box<dyn I2cSlave>) {
        self.slaves.push(slave);
    }

    /// 7-bit addresses currently modeled on this bus.
    pub fn addresses(&self) -> Vec<u8> {
        self.slaves.iter().map(|s| s.address()).collect()
    }

    /// `(address, milli_celsius)` for every temperature sensor on the bus. Used by
    /// a backend (QEMU) that pushes the modeled temperature into its own emulated
    /// I2C device rather than serving bytes through the `on_i2c` callback.
    pub fn temperature_sensors(&self) -> Vec<(u8, i32)> {
        self.slaves
            .iter()
            .filter_map(|s| s.temperature_mc().map(|mc| (s.address(), mc)))
            .collect()
    }

    /// Dispatch one I2C event to the matching slave. Returns the reply byte for
    /// a Read event (`None` otherwise). This is the body of the `on_i2c`
    /// closure the engine installs.
    pub fn dispatch(&mut self, ev: I2cEvent) -> Option<u8> {
        if !self.powered {
            if matches!(ev, I2cEvent::Start { .. } | I2cEvent::Stop { .. }) {
                self.active = None;
                self.txn_addrs.clear();
                self.stop_pending.clear();
            }
            return None;
        }
        match ev {
            I2cEvent::Start { addr, read } => {
                self.active = self
                    .slaves
                    .iter()
                    .any(|s| s.address() == addr)
                    .then_some(addr);
                if self.active.is_some() && !self.txn_addrs.contains(&addr) {
                    self.txn_addrs.push(addr);
                }
                if let Some(s) = self.slave_mut(addr) {
                    s.on_start(read);
                }
                None
            }
            I2cEvent::Write { addr, data } => {
                let a = self.active.unwrap_or(addr);
                if let Some(s) = self.slave_mut(a) {
                    s.on_write(data);
                }
                None
            }
            I2cEvent::Read { addr } => {
                let a = self.active.unwrap_or(addr);
                self.slave_mut(a).map(|s| s.on_read())
            }
            I2cEvent::Stop { addr } => {
                self.active = None;
                // A physical STOP ends the whole transaction: every address it
                // touched via (repeated) START gets its transaction end, plus
                // the named address, honored, not overridden, so a caller can
                // route a Stop to a specific slave. Record them; the
                // ctx-bearing on_stop is delivered by `flush_stops` at the
                // chunk boundary (see the trait docs).
                let mut ended = std::mem::take(&mut self.txn_addrs);
                if !ended.contains(&addr) {
                    ended.push(addr);
                }
                for a in ended {
                    if self.slave_mut(a).is_some() && !self.stop_pending.contains(&a) {
                        self.stop_pending.push(a);
                    }
                }
                None
            }
        }
    }

    pub fn set_powered(&mut self, powered: bool) {
        if self.powered && !powered {
            self.active = None;
            self.txn_addrs.clear();
            self.stop_pending.clear();
        }
        self.powered = powered;
    }

    pub fn powered(&self) -> bool {
        self.powered
    }

    pub fn take_activity(&mut self) -> BusActivity {
        let mut activity = BusActivity::default();
        for slave in &mut self.slaves {
            activity.merge(slave.take_activity());
        }
        activity
    }

    /// Deliver the deferred transaction-end hooks: call `on_stop(ctx)` on every
    /// slave that saw a STOP since the last flush. The scheduler calls this once
    /// per chunk, after the MCU ran and before the analog solve; the first
    /// point a `&mut Circuit` is borrowable, and the earliest the solve could
    /// see a driven net. Multiple transactions to one slave inside a chunk
    /// collapse to one delivery from the final state, exactly the resolution
    /// the chunk-rate analog side has.
    pub fn flush_stops(&mut self, ctx: &mut TickCtx) {
        let pending = std::mem::take(&mut self.stop_pending);
        for addr in pending {
            if let Some(s) = self.slave_mut(addr) {
                s.on_stop(ctx);
            }
        }
    }

    /// Call `on_stop(ctx)` on EVERY slave, pending or not. Used at attach time
    /// to seed the analog nets with each slave's power-on outputs (e.g. a DAC's
    /// code-0 VOUT ≈ 0 V) before any firmware transaction has happened.
    pub fn drive_all(&mut self, ctx: &mut TickCtx) {
        self.stop_pending.clear();
        self.txn_addrs.clear();
        for s in &mut self.slaves {
            s.on_stop(ctx);
        }
    }

    fn slave_mut(&mut self, addr: u8) -> Option<&mut Box<dyn I2cSlave>> {
        self.slaves.iter_mut().find(|s| s.address() == addr)
    }

    /// Borrow a concrete slave by address for assertions / temperature sweeps.
    pub fn slave<T: 'static>(&self, addr: u8) -> Option<&T> {
        self.slaves
            .iter()
            .find(|s| s.address() == addr)
            .and_then(|s| s.as_any().downcast_ref::<T>())
    }

    /// Mutable borrow of a concrete slave by address (e.g. to set temperature).
    pub fn slave_mut_t<T: 'static>(&mut self, addr: u8) -> Option<&mut T> {
        self.slaves
            .iter_mut()
            .find(|s| s.address() == addr)
            .and_then(|s| (s.as_any_mut()).downcast_mut::<T>())
    }
}

impl Peripheral for I2cBus {
    fn id(&self) -> &str {
        &self.id
    }
    fn kind(&self) -> &'static str {
        "i2c_bus"
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("slaves".into(), self.slaves.len() as f64);
        m.insert("powered".into(), if self.powered { 1.0 } else { 0.0 });
        for s in &self.slaves {
            for (k, v) in s.state() {
                m.insert(format!("0x{:02x}_{k}", s.address()), v);
            }
        }
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 24Cxx EEPROM
// ─────────────────────────────────────────────────────────────────────────────

/// A generic 24Cxx I2C EEPROM. The default 16-bit word address covers
/// 24C32..24C512; [`Eeprom24c::with_word_address_bytes`] selects the one-byte
/// phase used by 24C01/02-class parts. Write protocol: word-address bytes then
/// data bytes, auto-incrementing *within the write page*, like
/// the real parts, a page write that runs past the page boundary wraps the low
/// address bits back to the page start and overwrites from there (24C32/24C64
/// datasheets: "the address roll over during write is from the last byte of the
/// current page to the first byte of the same page"). Read protocol:
/// current-address reads return successive bytes, rolling over the whole array
/// (reads are not page-bound on the real parts either).
pub struct Eeprom24c {
    addr: u8,
    mem: Vec<u8>,
    /// Internal byte pointer (auto-increments on read/write).
    ptr: usize,
    /// Bytes consumed of the word-address phase in the current write.
    addr_phase: u8,
    addr_acc: usize,
    /// Number of word-address bytes consumed before data (1 or 2).
    word_address_bytes: u8,
    /// Write-page size in bytes (power of two). Defaults to 32, the 24C32/24C64
    /// page size; the smallest parts this 16-bit-address model covers.
    /// Configure via [`Eeprom24c::with_page_size`] for larger parts (64 for
    /// 24C128/24C256, 128 for 24C512).
    page_size: usize,
    activity: BusActivity,
}

impl Eeprom24c {
    /// The default write-page size (bytes): the 24C32/24C64 datasheet value.
    pub const DEFAULT_PAGE_SIZE: usize = 32;

    /// `size` is the total byte capacity (e.g. 4096 for a 24C32 = 32 kbit).
    pub fn new(address: u8, size: usize) -> Self {
        Eeprom24c {
            addr: address,
            mem: vec![0xFF; size.max(1)],
            ptr: 0,
            addr_phase: 0,
            addr_acc: 0,
            word_address_bytes: 2,
            page_size: Self::DEFAULT_PAGE_SIZE,
            activity: BusActivity::default(),
        }
    }

    /// Select a one- or two-byte word-address phase. Refuse any other width
    /// rather than silently consuming the first firmware data byte as address.
    pub fn with_word_address_bytes(mut self, bytes: u8) -> Self {
        assert!(
            matches!(bytes, 1 | 2),
            "24Cxx word-address width must be 1 or 2 bytes, got {bytes}"
        );
        self.word_address_bytes = bytes;
        self
    }

    /// Set the write-page size (bytes) for the modeled part. Must be a power of
    /// two (every 24Cxx page size is); a non-power-of-two or zero panics, since
    /// it describes an EEPROM that does not exist.
    pub fn with_page_size(mut self, page_size: usize) -> Self {
        assert!(
            page_size.is_power_of_two(),
            "24Cxx page size must be a power of two, got {page_size}"
        );
        self.page_size = page_size;
        self
    }

    /// Read the backing memory (for assertions).
    pub fn contents(&self) -> &[u8] {
        &self.mem
    }

    /// True if `needle` appears anywhere in the EEPROM contents.
    pub fn contains(&self, needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        self.mem.windows(needle.len()).any(|w| w == needle)
    }
}

impl I2cSlave for Eeprom24c {
    fn address(&self) -> u8 {
        self.addr
    }

    fn on_start(&mut self, read: bool) {
        // A repeated START for a read keeps the pointer (current-address read).
        if !read {
            self.addr_phase = 0;
            self.addr_acc = 0;
        }
    }

    fn on_write(&mut self, data: u8) {
        if self.addr_phase < self.word_address_bytes {
            self.addr_acc = (self.addr_acc << 8) | data as usize;
            self.addr_phase += 1;
            if self.addr_phase == self.word_address_bytes {
                self.ptr = self.addr_acc % self.mem.len();
            }
        } else {
            // Data byte: store and auto-increment within the page. Real
            // 24Cxx parts wrap only the low (page-offset) address bits on a
            // page write, so a write running past the page boundary rolls
            // back to the page start rather than spilling into the next
            // page. Pages never exceed the array, so page_size is clamped
            // to the memory size for tiny test configurations.
            if self.ptr < self.mem.len() {
                self.mem[self.ptr] = data;
            }
            self.activity.write_units = self.activity.write_units.saturating_add(1);
            let page = self.page_size.min(self.mem.len());
            let page_base = self.ptr - (self.ptr % page);
            self.ptr = (page_base + (self.ptr % page + 1) % page) % self.mem.len();
        }
    }

    fn on_read(&mut self) -> u8 {
        let b = self.mem.get(self.ptr).copied().unwrap_or(0xFF);
        self.ptr = (self.ptr + 1) % self.mem.len();
        self.activity.read_units = self.activity.read_units.saturating_add(1);
        b
    }

    fn take_activity(&mut self) -> BusActivity {
        std::mem::take(&mut self.activity)
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("size".into(), self.mem.len() as f64);
        m.insert("ptr".into(), self.ptr as f64);
        m.insert("page_size".into(), self.page_size as f64);
        m.insert("word_address_bytes".into(), self.word_address_bytes as f64);
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
// LM75 temperature sensor (real datasheet register map)
// ─────────────────────────────────────────────────────────────────────────────

/// LM75 / LM75A digital temperature sensor.
///
/// Register map (datasheet): a pointer register selects one of four registers.
///   0x00 Temp   (read-only, 2 bytes, 11-bit, 0.125 °C/LSB left-justified in
///               the upper bits; classic LM75 is 9-bit 0.5 °C, but we present the
///               LM75A 11-bit format which the 9-bit reads also accept)
///   0x01 Conf   (1 byte)
///   0x02 Thyst  (2 bytes)
///   0x03 Tos    (2 bytes, overtemp shutdown threshold)
///
/// The temperature register encodes T as a signed value in units of
/// 0.125 °C, stored left-justified: `raw = round(T / 0.125) << 5`, big-endian.
/// Reads auto-return the two temperature bytes (MSB then LSB).
pub struct Lm75 {
    addr: u8,
    pointer: u8,
    temp_c: f64,
    conf: u8,
    thyst_c: f64,
    tos_c: f64,
    /// Which byte of a multi-byte read we are on (0 = MSB).
    read_byte: u8,
    /// Whether the current write has consumed the pointer byte yet.
    got_pointer: bool,
    /// Accumulator for multi-byte register writes (Thyst/Tos/Conf).
    write_acc: Vec<u8>,
}

impl Lm75 {
    /// Default LM75 address with all address pins low is 0x48.
    pub const DEFAULT_ADDR: u8 = 0x48;

    pub fn new(address: u8, temp_c: f64) -> Self {
        Lm75 {
            addr: address,
            pointer: 0,
            temp_c,
            conf: 0,
            thyst_c: 75.0,
            tos_c: 80.0,
            read_byte: 0,
            got_pointer: false,
            write_acc: Vec::new(),
        }
    }

    /// Set the temperature the sensor reports (used by the sweep test and the
    /// live websocket control).
    pub fn set_temp_c(&mut self, t: f64) {
        self.temp_c = t;
    }

    pub fn temp_c(&self) -> f64 {
        self.temp_c
    }

    /// Encode a Celsius temperature into the LM75A 11-bit register value
    /// (0.125 °C/LSB, left-justified, big-endian), returning (msb, lsb).
    fn temp_bytes(&self) -> (u8, u8) {
        let counts = (self.temp_c / 0.125).round() as i32; // 11-bit signed
        let raw = ((counts << 5) & 0xFFFF) as u16; // left-justify into 16 bits
        ((raw >> 8) as u8, (raw & 0xFF) as u8)
    }

    fn reg_bytes(&self) -> Vec<u8> {
        match self.pointer {
            0x00 => {
                let (m, l) = self.temp_bytes();
                vec![m, l]
            }
            0x01 => vec![self.conf],
            0x02 => temp_to_be(self.thyst_c),
            0x03 => temp_to_be(self.tos_c),
            _ => vec![0xFF],
        }
    }
}

fn temp_to_be(t: f64) -> Vec<u8> {
    let counts = (t / 0.125).round() as i32;
    let raw = ((counts << 5) & 0xFFFF) as u16;
    vec![(raw >> 8) as u8, (raw & 0xFF) as u8]
}

impl I2cSlave for Lm75 {
    fn address(&self) -> u8 {
        self.addr
    }

    fn temperature_mc(&self) -> Option<i32> {
        Some((self.temp_c * 1000.0).round() as i32)
    }

    fn on_start(&mut self, read: bool) {
        self.read_byte = 0;
        if !read {
            self.got_pointer = false;
            self.write_acc.clear();
        }
    }

    fn on_write(&mut self, data: u8) {
        if !self.got_pointer {
            self.pointer = data & 0x03;
            self.got_pointer = true;
        } else {
            // Writing register data (Conf/Thyst/Tos). Accumulate.
            self.write_acc.push(data);
            match self.pointer {
                0x01 => self.conf = data,
                0x02 if self.write_acc.len() == 2 => {
                    self.thyst_c = be_to_temp(self.write_acc[0], self.write_acc[1]);
                }
                0x03 if self.write_acc.len() == 2 => {
                    self.tos_c = be_to_temp(self.write_acc[0], self.write_acc[1]);
                }
                _ => {}
            }
        }
    }

    fn on_read(&mut self) -> u8 {
        let bytes = self.reg_bytes();
        let b = bytes.get(self.read_byte as usize).copied().unwrap_or(0);
        self.read_byte = (self.read_byte + 1) % (bytes.len().max(1) as u8);
        b
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("temp_c".into(), self.temp_c);
        m.insert("pointer".into(), self.pointer as f64);
        m
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

fn be_to_temp(msb: u8, lsb: u8) -> f64 {
    let raw = ((msb as u16) << 8 | lsb as u16) as i16;
    // Right-justify (drop the 5 unused LSBs) and scale.
    (raw >> 5) as f64 * 0.125
}

/// Alias kept for the brief's "BME280 or LM75" choice: LM75 is the modelled
/// part. `Bme280` re-exports nothing real; we expose the chosen sensor under a
/// clear name so callers pick `Lm75`.
pub type Bme280 = Lm75;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lm75_encodes_and_reads_temperature() {
        let mut bus = I2cBus::new("I2C").with_slave(Box::new(Lm75::new(Lm75::DEFAULT_ADDR, 25.0)));
        // Point to temp register (0x00) then read two bytes.
        bus.dispatch(I2cEvent::Start {
            addr: 0x48,
            read: false,
        });
        bus.dispatch(I2cEvent::Write {
            addr: 0x48,
            data: 0x00,
        });
        bus.dispatch(I2cEvent::Start {
            addr: 0x48,
            read: true,
        });
        let msb = bus.dispatch(I2cEvent::Read { addr: 0x48 }).unwrap();
        let lsb = bus.dispatch(I2cEvent::Read { addr: 0x48 }).unwrap();
        bus.dispatch(I2cEvent::Stop { addr: 0x48 });
        // Decode back: 25 °C.
        let t = be_to_temp(msb, lsb);
        assert!((t - 25.0).abs() < 0.13, "decoded {t} not ~25");
    }

    #[test]
    fn lm75_negative_temperature() {
        let s = Lm75::new(0x48, -10.0);
        let (m, l) = s.temp_bytes();
        let t = be_to_temp(m, l);
        assert!((t + 10.0).abs() < 0.13, "decoded {t} not ~-10");
    }

    #[test]
    fn eeprom_write_then_read_back() {
        let mut bus = I2cBus::new("I2C").with_slave(Box::new(Eeprom24c::new(0x50, 256)));
        // Write "Hi" at address 0x0010.
        for ev in [
            I2cEvent::Start {
                addr: 0x50,
                read: false,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0x00,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0x10,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: b'H',
            },
            I2cEvent::Write {
                addr: 0x50,
                data: b'i',
            },
            I2cEvent::Stop { addr: 0x50 },
        ] {
            bus.dispatch(ev);
        }
        let ee = bus.slave::<Eeprom24c>(0x50).unwrap();
        assert!(ee.contains(b"Hi"), "EEPROM should contain 'Hi'");
        assert_eq!(ee.contents()[0x10], b'H');
        assert_eq!(ee.contents()[0x11], b'i');
    }

    #[test]
    fn one_byte_word_address_models_at24cs01_primary_array() {
        // Microchip AT24CS01 DS20006330A: 128-byte array, one 7-bit word
        // address carried in one byte, and an 8-byte write page. If the generic
        // model still consumed two address bytes, 0xA5 would become the low
        // address rather than data and this assertion would fail.
        let eeprom = Eeprom24c::new(0x50, 128)
            .with_word_address_bytes(1)
            .with_page_size(8);
        let mut bus = I2cBus::new("I2C").with_slave(Box::new(eeprom));
        for ev in [
            I2cEvent::Start {
                addr: 0x50,
                read: false,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0x7f,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0xa5,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0x5a,
            },
            I2cEvent::Stop { addr: 0x50 },
        ] {
            bus.dispatch(ev);
        }
        let ee = bus.slave::<Eeprom24c>(0x50).unwrap();
        assert_eq!(ee.contents()[0x7f], 0xa5);
        assert_eq!(ee.contents()[0x78], 0x5a, "8-byte page wraps at 0x7f");
        assert_eq!(ee.state()["word_address_bytes"], 1.0);
    }

    #[test]
    fn eeprom_page_write_wraps_at_page_boundary_not_into_next_page() {
        // 24C32-class part: 32-byte write pages. Start a page write two bytes
        // before the end of page 0 (address 0x001E) and stream four data bytes:
        // the real part stores the first two at 0x1E/0x1F, then rolls the low
        // address bits over to the START of the same page (0x00/0x01). It never
        // spills linearly into page 1 (0x20).
        let mut bus = I2cBus::new("I2C").with_slave(Box::new(Eeprom24c::new(0x50, 4096)));
        let mut evs = vec![
            I2cEvent::Start {
                addr: 0x50,
                read: false,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0x00,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0x1E,
            },
        ];
        for data in [0xA0, 0xA1, 0xA2, 0xA3] {
            evs.push(I2cEvent::Write { addr: 0x50, data });
        }
        evs.push(I2cEvent::Stop { addr: 0x50 });
        for ev in evs {
            bus.dispatch(ev);
        }
        let ee = bus.slave::<Eeprom24c>(0x50).unwrap();
        assert_eq!(ee.contents()[0x1E], 0xA0);
        assert_eq!(ee.contents()[0x1F], 0xA1);
        assert_eq!(
            ee.contents()[0x00],
            0xA2,
            "third byte wraps to the page start"
        );
        assert_eq!(ee.contents()[0x01], 0xA3);
        assert_eq!(
            ee.contents()[0x20],
            0xFF,
            "the next page must be untouched (no linear spill)"
        );
    }

    #[test]
    fn eeprom_page_size_is_configurable() {
        // A 64-byte-page part (24C128/24C256 class): a write crossing 0x3F
        // wraps to 0x00. (With the default 32-byte page it would wrap to 0x20
        // instead, asserted untouched below, so this proves the configured
        // size is honored, not just that some wrap happened.)
        let mut bus =
            I2cBus::new("I2C").with_slave(Box::new(Eeprom24c::new(0x50, 4096).with_page_size(64)));
        for ev in [
            I2cEvent::Start {
                addr: 0x50,
                read: false,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0x00,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0x3F,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0xB0,
            },
            I2cEvent::Write {
                addr: 0x50,
                data: 0xB1,
            },
            I2cEvent::Stop { addr: 0x50 },
        ] {
            bus.dispatch(ev);
        }
        let ee = bus.slave::<Eeprom24c>(0x50).unwrap();
        assert_eq!(ee.contents()[0x3F], 0xB0);
        assert_eq!(ee.contents()[0x00], 0xB1, "wraps at the 64-byte page end");
        assert_eq!(
            ee.contents()[0x20],
            0xFF,
            "a default-32-byte-page wrap would land here"
        );
        assert_eq!(
            ee.contents()[0x40],
            0xFF,
            "no spill into the next 64-byte page"
        );
    }

    /// Records `on_stop` deliveries, to prove STOP routing across a
    /// repeated-START chain.
    struct StopSpy {
        addr: u8,
        stops: usize,
    }

    impl I2cSlave for StopSpy {
        fn address(&self) -> u8 {
            self.addr
        }
        fn on_read(&mut self) -> u8 {
            0
        }
        fn on_stop(&mut self, _ctx: &mut TickCtx) {
            self.stops += 1;
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    /// PROOF: a physical STOP ends the WHOLE transaction. A repeated-START
    /// chain that re-addresses a second slave (write 0x60, Sr, read 0x48,
    /// STOP naming only 0x48) still delivers the transaction end to BOTH,
    /// the first-leg device is not skipped just because the last START went
    /// elsewhere. And a same-address chain stays a single delivery (no
    /// duplicate Stops from the Sr).
    #[test]
    fn stop_reaches_every_slave_addressed_via_repeated_start() {
        use hauksbee_ir::Circuit;

        let mut bus = I2cBus::new("I2C")
            .with_slave(Box::new(StopSpy {
                addr: 0x60,
                stops: 0,
            }))
            .with_slave(Box::new(StopSpy {
                addr: 0x48,
                stops: 0,
            }));

        // Leg 1: write to 0x60. Leg 2 (repeated START, no STOP between): read
        // from 0x48. One physical STOP, naming the last-addressed slave.
        for ev in [
            I2cEvent::Start {
                addr: 0x60,
                read: false,
            },
            I2cEvent::Write {
                addr: 0x60,
                data: 0x08,
            },
            I2cEvent::Start {
                addr: 0x48,
                read: true,
            },
            I2cEvent::Read { addr: 0x48 },
            I2cEvent::Stop { addr: 0x48 },
        ] {
            bus.dispatch(ev);
        }
        let mut circuit = Circuit::default();
        let volts: Vec<f64> = Vec::new();
        let mut ctx = TickCtx {
            circuit: &mut circuit,
            node_volts: &volts,
            t: 0.0,
            dt: 1e-3,
        };
        bus.flush_stops(&mut ctx);
        assert_eq!(
            bus.slave::<StopSpy>(0x60).unwrap().stops,
            1,
            "first-leg slave must see the transaction end"
        );
        assert_eq!(bus.slave::<StopSpy>(0x48).unwrap().stops, 1);

        // Same-address repeated START (the register-read idiom): exactly one
        // delivery, not one per leg.
        for ev in [
            I2cEvent::Start {
                addr: 0x48,
                read: false,
            },
            I2cEvent::Write {
                addr: 0x48,
                data: 0x00,
            },
            I2cEvent::Start {
                addr: 0x48,
                read: true,
            },
            I2cEvent::Read { addr: 0x48 },
            I2cEvent::Stop { addr: 0x48 },
        ] {
            bus.dispatch(ev);
        }
        let mut ctx = TickCtx {
            circuit: &mut circuit,
            node_volts: &volts,
            t: 0.0,
            dt: 1e-3,
        };
        bus.flush_stops(&mut ctx);
        assert_eq!(
            bus.slave::<StopSpy>(0x48).unwrap().stops,
            2,
            "one more, not two"
        );
        assert_eq!(
            bus.slave::<StopSpy>(0x60).unwrap().stops,
            1,
            "untouched slave stays at 1"
        );
    }

    #[test]
    fn unaddressed_slave_ignored() {
        let mut bus = I2cBus::new("I2C").with_slave(Box::new(Lm75::new(0x48, 25.0)));
        // No slave at 0x20: read returns None (no injection -> firmware sees 0xFF).
        bus.dispatch(I2cEvent::Start {
            addr: 0x20,
            read: true,
        });
        assert_eq!(bus.dispatch(I2cEvent::Read { addr: 0x20 }), None);
    }
}
