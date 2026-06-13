//! SPI slave framework and two concrete devices: a 25xx SPI EEPROM and an
//! MCP3008 8-channel ADC.
//!
//! ## How it plugs into the co-sim
//!
//! The MCU backend surfaces each byte the firmware clocks out as a
//! [`SpiEvent`] (MOSI byte) through `Mcu::on_spi`, and the handler returns the
//! MISO byte the slave drives back on the same transfer. An [`SpiBus`] owns one
//! slave (chip-select is not surfaced by the simavr SPI IRQ, so a single
//! active slave per bus is the supported topology) and threads the byte stream
//! through the slave's command state machine.
//!
//! ## Bus-speed honesty (chunk-rate limit)
//!
//! Like I2C, interception is byte-level through simavr's hardware SPI
//! peripheral: a `SPDR` write clocks one byte and raises one IRQ, all inside a
//! single `run_micros` chunk, so the SPI clock rate is whatever the firmware's
//! SPR/SPI2X prescaler sets and is not bounded by the analog chunk rate. A
//! *bit-banged* SPI master (software-toggled SCK/MOSI on GPIO) is bounded by
//! the chunk poll rate exactly like any GPIO and is out of scope here, matching
//! the limitation documented for I2C and in docs/MCU.md. The Renode `on_spi`
//! hook is a documented no-op, so these slaves bind to the AVR backend today.

use std::collections::HashMap;



use super::{Peripheral, TickCtx};

/// A device on the SPI bus that exchanges one byte per transfer.
pub trait SpiSlave: Send {
    /// Exchange a byte: receive the firmware's MOSI byte, return MISO.
    fn transfer(&mut self, mosi: u8) -> u8;

    /// Chip-select deasserted: end the current transaction. simavr does not
    /// surface CS, so the engine calls this between co-sim chunks as a frame
    /// boundary heuristic; well-formed transfers complete within a chunk.
    fn deselect(&mut self) {}

    fn state(&self) -> HashMap<String, f64> {
        HashMap::new()
    }

    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

/// The SPI bus peripheral: a single active slave whose byte stream is fed from
/// the MCU's `on_spi` callback.
pub struct SpiBus {
    id: String,
    slave: Box<dyn SpiSlave>,
}

impl SpiBus {
    pub fn new(id: &str, slave: Box<dyn SpiSlave>) -> Self {
        SpiBus {
            id: id.to_string(),
            slave,
        }
    }

    /// Exchange one byte (body of the `on_spi` closure).
    pub fn transfer(&mut self, mosi: u8) -> u8 {
        self.slave.transfer(mosi)
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
        &self.id
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
        self.slave.state()
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
            SpiCmd::Idle => {
                match mosi {
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
                }
            }
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
                    let b = self.mem.get(self.addr as usize % self.mem.len()).copied().unwrap_or(0xFF);
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
        let frac = (self.channels[ch] / self.vref).clamp(0.0, 1.0);
        (frac * 1023.0).round() as u16
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
    fn mcp3008_reads_channel_voltage() {
        let mut adc = Mcp3008::new(5.0);
        adc.set_channel(3, 2.5); // half scale -> ~512 counts
        let mut bus = SpiBus::new("SPI", Box::new(adc));
        bus.transfer(0x01); // start byte
        let hi = bus.transfer(3 << 4); // single-ended, channel 3
        let lo = bus.transfer(0x00);
        let counts = (((hi & 0x03) as u16) << 8) | lo as u16;
        // 2.5/5.0 * 1023 ≈ 512.
        assert!((counts as i32 - 512).abs() <= 2, "counts {counts} not ~512");
    }
}
