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
//! (see docs/MCU.md), so this framework targets the hardware TWI peripheral.
//! For Renode the `on_i2c` hook is a documented no-op, so these slaves bind to
//! the AVR backend today.

use std::collections::HashMap;

use hauksbee_mcu::I2cEvent;

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

// ─────────────────────────────────────────────────────────────────────────────
// MCP4728 quad 12-bit I2C DAC (real datasheet command set)
// ─────────────────────────────────────────────────────────────────────────────

/// MCP4728 — Microchip quad 12-bit voltage-output I2C DAC with internal
/// reference and EEPROM (datasheet DS22187E).
///
/// ## What is modelled
///
/// The four channels each hold a 12-bit DAC input register, a per-channel gain
/// bit (Gx) and reference-select bit (Vref), and a power-down state. The output
/// voltage of a channel is
///
/// ```text
///   VOUT = (code / 4096) * VREF * gain
/// ```
///
/// with the board configuration VREF = internal 2.048 V and gain = 2, so the
/// full-scale range is 0 .. 4.095 V and VOUT = code * 0.001 V (1 mV/LSB).
///
/// ## Command decode (Table 5-1)
///
/// The write byte after the address selects the command:
///   - **Fast Write** — the firmware's path. The top two bits of the first byte
///     are `0 0`; the byte carries `[0 0 PD1 PD0 D11 D10 D9 D8]` and the next
///     byte `[D7..D0]`. Channels auto-increment from A on each completed pair.
///   - **Multi-Write** (`010` in C2 C1 C0) and **Single Write** (`011`) and
///     **Sequential Write** (`010` + write-EEPROM bit): the channel comes from
///     the command byte's `DAC1 DAC0` field, and the two data bytes carry
///     `[Vref PD1 PD0 Gx D11..D8][D7..D0]`. These are decoded too so a host that
///     uses them lands the same code.
///
/// LDAC is held LOW by the board at init (`PIN_LATCH_DAC = PD2`), so a completed
/// channel write updates that channel's output register (VOUT) immediately; we
/// model the board's held-low LDAC and update VOUT on the spot. (The separate
/// address-reprogramming command and its LDAC-gated ACK is OUT OF SCOPE; this
/// model assumes the device is already addressed at its configured address.)
///
/// ## Readback
///
/// A read returns the datasheet read frame: for each channel, the DAC input
/// register and EEPROM register (3 bytes each, 6 per channel, 24 total). A
/// reader recovers the 12-bit code from the input-register bytes
/// `[RDY ... Vref PD1 PD0 Gx D11..D8][D7..D0]` (the upper nibble of the second
/// byte holds D11..D8). We expose enough of that frame for a `--verify-dacs`
/// style readback to recover the programmed codes.
pub struct Mcp4728 {
    addr: u8,
    /// Per-channel 12-bit DAC input register (the value written over I2C).
    input_reg: [u16; 4],
    /// Per-channel 12-bit DAC output register (drives VOUT). With LDAC held low
    /// this tracks `input_reg` on each write.
    output_reg: [u16; 4],
    /// Per-channel gain (1 or 2).
    gain: [u8; 4],
    /// Per-channel reference voltage in volts (internal 2.048 or external VDD).
    vref: [f64; 4],
    /// Per-channel power-down code (0 = normal; 1..3 = powered down via Rpd).
    pd: [u8; 4],
    /// Output series resistance (ROUT), ohms. Datasheet DC value ~1 Ω.
    pub rout: f64,
    /// Auto-incrementing channel cursor for Fast Write.
    fast_channel: usize,
    /// Bytes accumulated in the current write transaction.
    write_acc: Vec<u8>,
    /// Decoded command of the current write (set on the first byte).
    cmd: Mcp4728Cmd,
    /// Byte cursor for the read frame.
    read_byte: usize,
}

/// The command decoded from the first write byte of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mcp4728Cmd {
    /// Not yet decoded (no byte seen since START).
    None,
    /// Fast Write: 2 bytes per channel, auto-incrementing from A.
    Fast,
    /// Multi / Single Write: 3 bytes per channel (cmd + 2 data), channel taken
    /// from each group's command byte.
    Write,
    /// Sequential Write: ONE command byte (consumed in `on_write`, which seeds
    /// the channel cursor with its start channel), then back-to-back data pairs
    /// that auto-increment the channel. The command byte is NOT in `write_acc`.
    Seq,
    /// A command we do not model (e.g. write-address-bits); bytes are ignored.
    Other,
}

impl Mcp4728 {
    /// Factory-default 7-bit address (A2=A1=A0=0).
    pub const DEFAULT_ADDR: u8 = 0x60;

    /// Build an MCP4728 at `address` with the board configuration: internal
    /// VREF = 2.048 V and gain = 2 on every channel (full-scale 4.096 V). All
    /// channels start at code 0 (VOUT ≈ 0 V), matching a factory-unprogrammed
    /// EEPROM.
    pub fn new(address: u8) -> Self {
        Self::with_config(address, 2.048, 2)
    }

    /// Build an MCP4728 with an explicit reference voltage and gain on all four
    /// channels (e.g. internal 2.048 V, gain 2 for the Tarski board).
    pub fn with_config(address: u8, vref: f64, gain: u8) -> Self {
        Mcp4728 {
            addr: address,
            input_reg: [0; 4],
            output_reg: [0; 4],
            gain: [gain; 4],
            vref: [vref; 4],
            pd: [0; 4],
            rout: 1.0,
            fast_channel: 0,
            write_acc: Vec::new(),
            cmd: Mcp4728Cmd::None,
            read_byte: 0,
        }
    }

    /// The 12-bit input-register code for `channel` (0..3).
    pub fn code(&self, channel: usize) -> u16 {
        self.input_reg.get(channel).copied().unwrap_or(0)
    }

    /// The computed output voltage for `channel` (0..3), in volts:
    /// `VOUT = (code / 4096) * VREF * gain`, or 0 V when powered down.
    pub fn vout(&self, channel: usize) -> f64 {
        if channel >= 4 || self.pd[channel] != 0 {
            return 0.0;
        }
        let code = self.output_reg[channel] as f64;
        (code / 4096.0) * self.vref[channel] * self.gain[channel] as f64
    }

    /// Commit a completed channel write: latch the input register, and (because
    /// the board holds LDAC low) update the output register immediately.
    fn commit(&mut self, channel: usize, code: u16, pd: u8, gain: Option<u8>, vref: Option<f64>) {
        if channel >= 4 {
            return;
        }
        self.input_reg[channel] = code & 0x0FFF;
        self.pd[channel] = pd;
        if let Some(g) = gain {
            self.gain[channel] = g;
        }
        if let Some(v) = vref {
            self.vref[channel] = v;
        }
        // LDAC low -> VOUT updates on the spot.
        self.output_reg[channel] = self.input_reg[channel];
    }

    /// Decode the accumulated Fast-Write byte pairs. Each pair is
    /// `[0 0 PD1 PD0 D11..D8][D7..D0]`, channel auto-incrementing from the
    /// current cursor.
    fn decode_fast(&mut self) {
        while self.write_acc.len() >= 2 {
            let b1 = self.write_acc.remove(0);
            let b2 = self.write_acc.remove(0);
            let pd = (b1 >> 4) & 0x03;
            let code = (((b1 & 0x0F) as u16) << 8) | b2 as u16;
            let ch = self.fast_channel & 0x03;
            self.commit(ch, code, pd, None, None);
            self.fast_channel = (self.fast_channel + 1) & 0x03;
        }
    }

    /// Decode the accumulated Multi/Single-Write bytes. `write_acc` holds one
    /// command byte per channel: `[cmd][data_hi][data_lo]...`. Each 3-byte group
    /// is `[C2 C1 C0 W1 W0 DAC1 DAC0 UDAC][Vref PD1 PD0 Gx D11..D8][D7..D0]`.
    /// Sequential Write (one command, then auto-incrementing pairs) is handled
    /// separately by [`Mcp4728::decode_seq`].
    fn decode_write(&mut self) {
        // Process complete 3-byte groups (cmd + 2 data).
        while self.write_acc.len() >= 3 {
            let cmd = self.write_acc.remove(0);
            let d_hi = self.write_acc.remove(0);
            let d_lo = self.write_acc.remove(0);
            let channel = ((cmd >> 1) & 0x03) as usize;
            let vref_bit = (d_hi >> 7) & 0x01;
            let pd = (d_hi >> 5) & 0x03;
            let gain_bit = (d_hi >> 4) & 0x01;
            let code = (((d_hi & 0x0F) as u16) << 8) | d_lo as u16;
            let gain = if gain_bit == 1 { 2 } else { 1 };
            // Vref bit 1 = internal 2.048 V, 0 = external VDD. We keep the
            // channel's existing external/internal voltages; only switch the
            // reference magnitude to the internal 2.048 V when the bit selects
            // internal, otherwise leave the per-channel vref as configured.
            let vref = if vref_bit == 1 { Some(2.048) } else { None };
            self.commit(channel, code, pd, Some(gain), vref);
        }
    }

    /// Decode Sequential-Write data pairs. The command byte was consumed in
    /// `on_write` (it seeded `fast_channel` with the start channel), so
    /// `write_acc` holds only data pairs `[Vref PD1 PD0 Gx D11..D8][D7..D0]`,
    /// each writing the cursor channel and advancing it (A->B->C->D, wrapping).
    fn decode_seq(&mut self) {
        while self.write_acc.len() >= 2 {
            let d_hi = self.write_acc.remove(0);
            let d_lo = self.write_acc.remove(0);
            let vref_bit = (d_hi >> 7) & 0x01;
            let pd = (d_hi >> 5) & 0x03;
            let gain_bit = (d_hi >> 4) & 0x01;
            let code = (((d_hi & 0x0F) as u16) << 8) | d_lo as u16;
            let gain = if gain_bit == 1 { 2 } else { 1 };
            let vref = if vref_bit == 1 { Some(2.048) } else { None };
            let ch = (self.fast_channel & 0x03) as usize;
            self.commit(ch, code, pd, Some(gain), vref);
            self.fast_channel = (self.fast_channel + 1) & 0x03;
        }
    }

    /// Build the 24-byte read frame: for each channel A..D, the DAC input
    /// register (3 bytes) then the EEPROM register (3 bytes). Byte layout per
    /// register: `[RDY POR Vref PD1 PD0 Gx 0 0]` style status is simplified to
    /// `[0 0 0 0 0 0 0 0]` for the leading byte we don't model; the code is in
    /// the next two bytes as `[.... D11..D8][D7..D0]`. A reader recovers the
    /// 12-bit code from bytes 1 and 2 of each input-register triple.
    fn read_frame(&self) -> Vec<u8> {
        let mut f = Vec::with_capacity(24);
        for ch in 0..4 {
            let code = self.input_reg[ch];
            let vref_bit = if (self.vref[ch] - 2.048).abs() < 0.5 { 1 } else { 0 };
            let gain_bit = if self.gain[ch] >= 2 { 1 } else { 0 };
            // DAC input register triple.
            // Byte 0: RDY/BSY (1) + POR (1) + address bits — simplified status.
            f.push(0xC0 | ((self.addr & 0x07) << 1));
            // Byte 1: [Vref PD1 PD0 Gx D11 D10 D9 D8].
            f.push((vref_bit << 7)
                | (self.pd[ch] << 5)
                | (gain_bit << 4)
                | ((code >> 8) & 0x0F) as u8);
            // Byte 2: [D7..D0].
            f.push((code & 0xFF) as u8);
            // EEPROM register triple (mirror the input register here).
            f.push(0xC0 | ((self.addr & 0x07) << 1));
            f.push((vref_bit << 7)
                | (self.pd[ch] << 5)
                | (gain_bit << 4)
                | ((code >> 8) & 0x0F) as u8);
            f.push((code & 0xFF) as u8);
        }
        f
    }
}

impl I2cSlave for Mcp4728 {
    fn address(&self) -> u8 {
        self.addr
    }

    fn on_start(&mut self, read: bool) {
        self.read_byte = 0;
        if !read {
            self.write_acc.clear();
            self.cmd = Mcp4728Cmd::None;
            self.fast_channel = 0;
        }
    }

    fn on_write(&mut self, data: u8) {
        if self.cmd == Mcp4728Cmd::None {
            // First byte after the address decides the command family.
            let top2 = (data >> 6) & 0x03;
            if top2 == 0b00 {
                self.cmd = Mcp4728Cmd::Fast;
                self.write_acc.push(data);
            } else {
                let c = (data >> 5) & 0x07; // C2 C1 C0 (top three bits)
                let w = (data >> 3) & 0x03; // W1 W0 (the write sub-type)
                // 010 = the DAC-write family; W1W0: 00 = Multi, 10 = Sequential,
                // 11 = Single. 011 = single-channel write variant.
                if c == 0b010 && w == 0b10 {
                    // Sequential Write: consume the single command byte now (it
                    // carries only the start channel), then the trailing data
                    // pairs auto-increment from there. The command byte does NOT
                    // go into write_acc.
                    self.cmd = Mcp4728Cmd::Seq;
                    self.fast_channel = ((data >> 1) & 0x03) as usize;
                } else if c == 0b010 || c == 0b011 {
                    self.cmd = Mcp4728Cmd::Write;
                    self.write_acc.push(data);
                } else {
                    // Write-address-bits / general-purpose: not modelled here.
                    self.cmd = Mcp4728Cmd::Other;
                }
            }
            return;
        }
        if matches!(self.cmd, Mcp4728Cmd::Other) {
            return;
        }
        self.write_acc.push(data);
        // Decode greedily as soon as a full unit is available, so the channel
        // registers update during the transaction (LDAC low -> immediate VOUT).
        match self.cmd {
            Mcp4728Cmd::Fast => self.decode_fast(),
            Mcp4728Cmd::Write => self.decode_write(),
            Mcp4728Cmd::Seq => self.decode_seq(),
            _ => {}
        }
    }

    fn on_read(&mut self) -> u8 {
        let frame = self.read_frame();
        let b = frame.get(self.read_byte).copied().unwrap_or(0);
        self.read_byte = (self.read_byte + 1) % frame.len().max(1);
        b
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        for ch in 0..4 {
            let name = ['a', 'b', 'c', 'd'][ch];
            m.insert(format!("code_{name}"), self.input_reg[ch] as f64);
            m.insert(format!("vout_{name}"), self.vout(ch));
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

    /// Emit the EXACT byte pair the firmware sends for a Fast Write of a 12-bit
    /// code on one channel: byte_1 = (value >> 8) & 0x0F, byte_2 = value & 0xFF
    /// (device.cpp:182-183). PD bits are 0 (normal mode), top two bits 0.
    fn firmware_fast_write_pair(value: u16) -> (u8, u8) {
        let v = value & 0x0FFF;
        (((v >> 8) & 0x0F) as u8, (v & 0xFF) as u8)
    }

    #[test]
    fn mcp4728_firmware_fast_write_sets_vout() {
        // The firmware writes channel 0 of the device at 0x60 to code 2048.
        // With the board config (VREF 2.048, gain 2) that is exactly 2.048 V.
        let mut bus = I2cBus::new("U1101")
            .with_slave(Box::new(Mcp4728::new(Mcp4728::DEFAULT_ADDR)));
        let (b1, b2) = firmware_fast_write_pair(2048);
        for ev in [
            I2cEvent::Start { addr: 0x60, read: false },
            I2cEvent::Write { addr: 0x60, data: b1 },
            I2cEvent::Write { addr: 0x60, data: b2 },
            I2cEvent::Stop { addr: 0x60 },
        ] {
            bus.dispatch(ev);
        }
        let dac = bus.slave::<Mcp4728>(0x60).unwrap();
        assert_eq!(dac.code(0), 2048, "channel 0 latched code 2048");
        // VOUT = code * 0.001 V exactly (2048 -> 2.048 V).
        assert!(
            (dac.vout(0) - 2.048).abs() < 1e-9,
            "VOUT should be 2.048 V, got {}",
            dac.vout(0)
        );
        // The whole code range maps VOUT = code * 0.001 V.
        for &code in &[0u16, 1, 1000, 4095] {
            let mut b = I2cBus::new("U")
                .with_slave(Box::new(Mcp4728::new(Mcp4728::DEFAULT_ADDR)));
            let (h, l) = firmware_fast_write_pair(code);
            for ev in [
                I2cEvent::Start { addr: 0x60, read: false },
                I2cEvent::Write { addr: 0x60, data: h },
                I2cEvent::Write { addr: 0x60, data: l },
                I2cEvent::Stop { addr: 0x60 },
            ] {
                b.dispatch(ev);
            }
            let want = code as f64 * 0.001;
            let got = b.slave::<Mcp4728>(0x60).unwrap().vout(0);
            assert!((got - want).abs() < 1e-9, "code {code}: VOUT {got} != {want}");
        }
    }

    #[test]
    fn mcp4728_fast_write_auto_increments_channels() {
        // One Fast Write transaction with four pairs lands on channels A..D.
        let mut bus = I2cBus::new("U")
            .with_slave(Box::new(Mcp4728::new(Mcp4728::DEFAULT_ADDR)));
        let codes = [100u16, 2048, 3000, 4095];
        let mut evs = vec![I2cEvent::Start { addr: 0x60, read: false }];
        for &c in &codes {
            let (h, l) = firmware_fast_write_pair(c);
            evs.push(I2cEvent::Write { addr: 0x60, data: h });
            evs.push(I2cEvent::Write { addr: 0x60, data: l });
        }
        evs.push(I2cEvent::Stop { addr: 0x60 });
        for ev in evs {
            bus.dispatch(ev);
        }
        let dac = bus.slave::<Mcp4728>(0x60).unwrap();
        for (ch, &c) in codes.iter().enumerate() {
            assert_eq!(dac.code(ch), c, "channel {ch} code");
            assert!((dac.vout(ch) - c as f64 * 0.001).abs() < 1e-9);
        }
    }

    #[test]
    fn mcp4728_readback_recovers_code() {
        let mut bus = I2cBus::new("U")
            .with_slave(Box::new(Mcp4728::new(Mcp4728::DEFAULT_ADDR)));
        let (h, l) = firmware_fast_write_pair(2730);
        for ev in [
            I2cEvent::Start { addr: 0x60, read: false },
            I2cEvent::Write { addr: 0x60, data: h },
            I2cEvent::Write { addr: 0x60, data: l },
            I2cEvent::Stop { addr: 0x60 },
        ] {
            bus.dispatch(ev);
        }
        // Read the frame back: channel A input register is the first triple;
        // byte 1 carries D11..D8 in its low nibble, byte 2 carries D7..D0.
        bus.dispatch(I2cEvent::Start { addr: 0x60, read: true });
        let _status = bus.dispatch(I2cEvent::Read { addr: 0x60 }).unwrap();
        let hi = bus.dispatch(I2cEvent::Read { addr: 0x60 }).unwrap();
        let lo = bus.dispatch(I2cEvent::Read { addr: 0x60 }).unwrap();
        bus.dispatch(I2cEvent::Stop { addr: 0x60 });
        let recovered = (((hi & 0x0F) as u16) << 8) | lo as u16;
        assert_eq!(recovered, 2730, "readback recovers the programmed code");
    }

    #[test]
    fn mcp4728_three_instances_are_independent() {
        // Three DACs at 0x60/0x61/0x62 on one bus. Writing 0x60 must not touch
        // 0x61 or 0x62.
        let mut bus = I2cBus::new("U")
            .with_slave(Box::new(Mcp4728::new(0x60)))
            .with_slave(Box::new(Mcp4728::new(0x61)))
            .with_slave(Box::new(Mcp4728::new(0x62)));
        let (h, l) = firmware_fast_write_pair(4000);
        for ev in [
            I2cEvent::Start { addr: 0x60, read: false },
            I2cEvent::Write { addr: 0x60, data: h },
            I2cEvent::Write { addr: 0x60, data: l },
            I2cEvent::Stop { addr: 0x60 },
        ] {
            bus.dispatch(ev);
        }
        assert_eq!(bus.slave::<Mcp4728>(0x60).unwrap().code(0), 4000);
        assert_eq!(bus.slave::<Mcp4728>(0x61).unwrap().code(0), 0, "0x61 untouched");
        assert_eq!(bus.slave::<Mcp4728>(0x62).unwrap().code(0), 0, "0x62 untouched");
        assert!((bus.slave::<Mcp4728>(0x61).unwrap().vout(0)).abs() < 1e-12);
    }

    #[test]
    fn mcp4728_multi_write_command_decodes_channel() {
        // Multi-Write (C2C1C0 = 010), channel C (DAC1 DAC0 = 10), code 1500,
        // Vref=internal(1), PD=0, Gx=gain2(1). Byte layout:
        //   cmd  = 0b0100_1100  (010 | W1W0=01 | DAC=10 | UDAC=0)
        //   dhi  = [Vref PD1 PD0 Gx D11..D8] = 1 00 1 (1500>>8=0x5) -> 0b1001_0101
        //   dlo  = 1500 & 0xFF = 0xDC
        let mut bus = I2cBus::new("U")
            .with_slave(Box::new(Mcp4728::new(Mcp4728::DEFAULT_ADDR)));
        let cmd = 0b0100_1100u8;
        // dhi = [Vref=1, PD=00, Gx=1, D11..D8]. PD bits are 0 (left implicit).
        let dhi = (1u8 << 7) | (1 << 4) | (((1500u16 >> 8) & 0x0F) as u8);
        let dlo = (1500u16 & 0xFF) as u8;
        for ev in [
            I2cEvent::Start { addr: 0x60, read: false },
            I2cEvent::Write { addr: 0x60, data: cmd },
            I2cEvent::Write { addr: 0x60, data: dhi },
            I2cEvent::Write { addr: 0x60, data: dlo },
            I2cEvent::Stop { addr: 0x60 },
        ] {
            bus.dispatch(ev);
        }
        let dac = bus.slave::<Mcp4728>(0x60).unwrap();
        assert_eq!(dac.code(2), 1500, "channel C programmed via Multi-Write");
        assert!((dac.vout(2) - 1.500).abs() < 1e-9, "VOUT C = 1.5 V");
    }

    #[test]
    fn mcp4728_sequential_write_auto_increments_channels() {
        // Sequential Write: ONE command byte (start channel A), then four data
        // pairs that auto-increment A->B->C->D. cmd = 010 | W1W0=10 | DAC=00 |
        // UDAC=0 = 0b0101_0000 = 0x50. Each data hi = [Vref=1 PD=00 Gx=1 D11..D8].
        // Codes 100/200/300/400 -> A..D. This sequence is mis-framed by the old
        // 3-byte-per-group decoder; it must land each code in its own channel.
        let mut bus = I2cBus::new("U")
            .with_slave(Box::new(Mcp4728::new(Mcp4728::DEFAULT_ADDR)));
        let pair = |code: u16| -> [u8; 2] {
            let hi = (1u8 << 7) | (1 << 4) | (((code >> 8) & 0x0F) as u8);
            [hi, (code & 0xFF) as u8]
        };
        let mut evs = vec![I2cEvent::Start { addr: 0x60, read: false },
            I2cEvent::Write { addr: 0x60, data: 0x50 }];
        for code in [100u16, 200, 300, 400] {
            for b in pair(code) {
                evs.push(I2cEvent::Write { addr: 0x60, data: b });
            }
        }
        evs.push(I2cEvent::Stop { addr: 0x60 });
        for ev in evs {
            bus.dispatch(ev);
        }
        let dac = bus.slave::<Mcp4728>(0x60).unwrap();
        assert_eq!(
            [dac.code(0), dac.code(1), dac.code(2), dac.code(3)],
            [100, 200, 300, 400],
            "Sequential Write lands each code in its own auto-incremented channel"
        );
        assert!((dac.vout(3) - 0.400).abs() < 1e-9, "VOUT D = 0.4 V");
    }

    #[test]
    fn unaddressed_slave_ignored() {
        let mut bus = I2cBus::new("I2C").with_slave(Box::new(Lm75::new(0x48, 25.0)));
        // No slave at 0x20: read returns None (no injection -> firmware sees 0xFF).
        bus.dispatch(I2cEvent::Start { addr: 0x20, read: true });
        assert_eq!(bus.dispatch(I2cEvent::Read { addr: 0x20 }), None);
    }
}
