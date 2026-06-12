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
//! fast 400 kHz both work) — it is not bounded by the analog chunk rate,
//! because the bytes are consumed inside a single `run_micros` call. A
//! *bit-banged* I2C master (GPIO toggling SCL/SDA in software) is a different
//! story: those edges alias at the chunk poll rate exactly like any other GPIO
//! (see docs/MCU.md), so this framework targets the hardware TWI peripheral.
//! For Renode the `on_i2c` hook is a documented no-op, so these slaves bind to
//! the AVR backend today.

use std::collections::HashMap;

use galvani_mcu::I2cEvent;

use super::Peripheral;

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

    /// STOP condition ended the transaction.
    fn on_stop(&mut self) {}

    /// Numeric state for frame reporting.
    fn state(&self) -> HashMap<String, f64> {
        HashMap::new()
    }

    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// The I2C bus peripheral: a router over attached slaves. Register its
/// [`I2cBus::handler`] as the MCU's `on_i2c` callback.
pub struct I2cBus {
    id: String,
    /// Address of the slave currently addressed (set on START), if any matched.
    active: Option<u8>,
    slaves: Vec<Box<dyn I2cSlave>>,
}

impl I2cBus {
    pub fn new(id: &str) -> Self {
        I2cBus {
            id: id.to_string(),
            active: None,
            slaves: Vec::new(),
        }
    }

    pub fn with_slave(mut self, slave: Box<dyn I2cSlave>) -> Self {
        self.slaves.push(slave);
        self
    }

    pub fn add_slave(&mut self, slave: Box<dyn I2cSlave>) {
        self.slaves.push(slave);
    }

    /// Dispatch one I2C event to the matching slave. Returns the reply byte for
    /// a Read event (`None` otherwise). This is the body of the `on_i2c`
    /// closure the engine installs.
    pub fn dispatch(&mut self, ev: I2cEvent) -> Option<u8> {
        match ev {
            I2cEvent::Start { addr, read } => {
                self.active = self.slaves.iter().any(|s| s.address() == addr).then_some(addr);
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
                let a = self.active.take().unwrap_or(addr);
                if let Some(s) = self.slave_mut(a) {
                    s.on_stop();
                }
                None
            }
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

/// A generic 24Cxx I2C EEPROM with a 16-bit word address (covers 24C32..24C512
/// addressing; smaller parts simply wrap). Write protocol: two address bytes
/// (hi, lo) then data bytes, auto-incrementing. Read protocol: current-address
/// reads return successive bytes.
pub struct Eeprom24c {
    addr: u8,
    mem: Vec<u8>,
    /// Internal byte pointer (auto-increments on read/write).
    ptr: usize,
    /// Bytes consumed of the 2-byte word-address phase in the current write.
    addr_phase: u8,
    addr_hi: u8,
}

impl Eeprom24c {
    /// `size` is the total byte capacity (e.g. 4096 for a 24C32 = 32 kbit).
    pub fn new(address: u8, size: usize) -> Self {
        Eeprom24c {
            addr: address,
            mem: vec![0xFF; size.max(1)],
            ptr: 0,
            addr_phase: 0,
            addr_hi: 0,
        }
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
        }
    }

    fn on_write(&mut self, data: u8) {
        match self.addr_phase {
            0 => {
                self.addr_hi = data;
                self.addr_phase = 1;
            }
            1 => {
                self.ptr = (((self.addr_hi as usize) << 8) | data as usize) % self.mem.len();
                self.addr_phase = 2;
            }
            _ => {
                // Data byte: store and auto-increment within the page.
                if self.ptr < self.mem.len() {
                    self.mem[self.ptr] = data;
                }
                self.ptr = (self.ptr + 1) % self.mem.len();
            }
        }
    }

    fn on_read(&mut self) -> u8 {
        let b = self.mem.get(self.ptr).copied().unwrap_or(0xFF);
        self.ptr = (self.ptr + 1) % self.mem.len();
        b
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("size".into(), self.mem.len() as f64);
        m.insert("ptr".into(), self.ptr as f64);
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
///               the upper bits; classic LM75 is 9-bit 0.5 °C — we present the
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
        bus.dispatch(I2cEvent::Start { addr: 0x48, read: false });
        bus.dispatch(I2cEvent::Write { addr: 0x48, data: 0x00 });
        bus.dispatch(I2cEvent::Start { addr: 0x48, read: true });
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
            I2cEvent::Start { addr: 0x50, read: false },
            I2cEvent::Write { addr: 0x50, data: 0x00 },
            I2cEvent::Write { addr: 0x50, data: 0x10 },
            I2cEvent::Write { addr: 0x50, data: b'H' },
            I2cEvent::Write { addr: 0x50, data: b'i' },
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
    fn unaddressed_slave_ignored() {
        let mut bus = I2cBus::new("I2C").with_slave(Box::new(Lm75::new(0x48, 25.0)));
        // No slave at 0x20: read returns None (no injection -> firmware sees 0xFF).
        bus.dispatch(I2cEvent::Start { addr: 0x20, read: true });
        assert_eq!(bus.dispatch(I2cEvent::Read { addr: 0x20 }), None);
    }
}
