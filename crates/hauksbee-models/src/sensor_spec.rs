//! Declarative register-map sensor specification.
//!
//! Instead of hand-coding each I2C/SPI sensor model in Rust (today `Lm75`,
//! `Mcp3008` in `hauksbee-engine`), a sensor is described DECLARATIVELY here:
//! its bus, address / SPI framing, register map, and per-register value
//! encoding. The engine's generic `RegisterMapSensor` interpreter realizes the
//! spec as an `I2cSlave` / `SpiSlave`; the datasheet extractor
//! (`model-extract --kind i2c_sensor`) fills the spec in from a datasheet.
//!
//! This module owns the *format* and *validation* only, no bus behaviour and
//! no `evalexpr` evaluation (that lives engine-side, where the expression
//! evaluator already is). It is the shared contract both the interpreter and
//! the extractor validate against.
//!
//!
//! ## TOML shape
//!
//! ```toml
//! [sensor]
//! name = "LM75"
//! bus = "i2c"                 # "i2c" | "spi"
//! i2c_address = 0x48          # required iff bus = "i2c"
//!
//! [[sensor.input]]
//! name = "temperature_c"
//! default = 25.0
//!
//! [[sensor.register]]
//! addr = 0x00
//! bytes = 2
//! encoding = "q7.1_be"
//! expr = "temperature_c"
//!
//! [[sensor.register]]
//! addr = 0x01
//! const = [0x00]
//!
//! [sensor.protocol]
//! style = "i2c_pointer"       # I2C: master writes register addr, then reads N
//! # For SPI:
//! #   style = "spi_reg"       # first byte = (rw_bit<<7 | addr)
//! #   rw_read_is_high = true
//! #   addr_mask = 0x7f
//! ```
//!
//! ## Write side
//!
//! Firmware writes are described by three additional block families, see the
//! "Write side" section below for the full types and the Rust/expression
//! boundary rationale:
//!
//! ```toml
//! [[sensor.write_register]]   # pointer-framed R/W register (ADS1115 config)
//! addr = 0x01
//! encoding = "u16_be"
//! store = "config"            # decoded value, referencable from read exprs
//! [[sensor.write_register.field]]
//! name = "mux"                # bit field extracted Rust-side (no evalexpr bit ops)
//! bits = [14, 12]
//!
//! [[sensor.write_command]]    # command-framed writes (MCP4728 fast write)
//! name = "fast_write"
//! match_mask = 0xC0
//! match_value = 0x00
//! group_bytes = 2
//! [[sensor.write_command.field]]
//! name = "code_bits"
//! bits = [11, 0]
//! [sensor.write_command.update]
//! code = "code_bits"          # per-channel state update (evalexpr)
//!
//! [[sensor.output]]           # decoded state -> driven-net voltage law
//! name = "vout_a"
//! channel = 0
//! expr = "if(pd == 0.0, (code / 4096) * vref * gain, 0.0)"
//! ```
//!
//! ### SPI register addresses: write the raw datasheet value
//!
//! For an `spi_reg` sensor the R/W direction bit is folded into bit 7 of the
//! command byte, and the interpreter recovers the register address as
//! `cmd & addr_mask` (default `0x7f`). You may therefore write a register `addr`
//! as the **raw datasheet register address**, e.g. the BMP280 chip-ID register,
//! which the datasheet lists as `0xD0`. The interpreter normalizes both stored
//! keys and the incoming command address by `addr_mask`, so `0xD0` and the
//! pre-masked `0x50` resolve to the same register (backward compatible). Because
//! addresses are compared post-mask, two SPI registers that collide to the same
//! `addr & addr_mask` are rejected by validation.
//!
//! I2C (`i2c_pointer`) uses a full 8-bit register pointer with no folded
//! direction bit, so its `addr` values are used raw (unmasked).
//!
//! ## Pins and strap-selected addresses
//!
//! ```toml
//! [sensor.pins]               # pad -> role, the shape of a card's [models.pins]
//! 1 = "sda"
//! 2 = "scl"
//! 3 = "a0"                    # address strap
//! 4 = "gnd"
//! 5 = "vdd"
//! 6 = "int"                   # interrupt / alert output
//!
//! [[sensor.address_select]]   # strap levels -> the 7-bit address they select
//! address = 0x48              # i2c_address must be one of the rows
//! straps = { a0 = "low" }
//! [[sensor.address_select]]
//! address = 0x49
//! straps = { a0 = "high" }
//! ```
//!
//! The bus roles are `scl`/`sda` (I2C) or `cs`/`sck`/`mosi`/`miso` (SPI), the
//! register-map peripheral's default roles, so a card embedding this spec binds
//! them without restating the pad map. [`Sensor::resolve_address`] turns the
//! board's strap levels into the address, or into the default marked assumed.
//!

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::check::expr_identifiers;

/// Which physical bus the sensor lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bus {
    I2c,
    Spi,
}

/// The wire protocol the firmware uses to address registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolStyle {
    /// I2C: the master writes the 1-byte register address (the "pointer"), then
    /// reads N bytes; sequential reads auto-increment across the register map.
    I2cPointer,
    /// SPI: the first byte of a transfer is `(rw_bit << 7) | addr`; subsequent
    /// transfers stream the addressed register's bytes (read) or are consumed
    /// (write). `rw_read_is_high` / `addr_mask` tune the framing.
    ///
    /// Because the R/W bit lives in bit 7 of the command byte, the interpreter
    /// recovers the register address as `cmd & addr_mask`. Register `addr` values
    /// in the spec may be written as the **raw datasheet register address** (e.g.
    /// BMP280 chip-ID `0xD0`); they are normalized by `addr_mask` internally, so a
    /// raw `0xD0` and a pre-masked `0x50` address the same register.
    SpiReg,
}

/// How a register's numeric value is packed into the read bytes.
///
/// The endian variants are the obvious integer packings. `Q71Be` is the LM75
/// temperature format. `Raw` means the register carries only `const` bytes
/// (no expression / numeric value).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    U8,
    U16Be,
    U16Le,
    I16Be,
    I16Le,
    /// LM75A temperature register packing, big-endian.
    ///
    /// The signed temperature is quantised at **0.125 °C/LSB** into an 11-bit
    /// two's-complement count, then left-justified into a 16-bit word
    /// (`(round(T / 0.125) << 5) & 0xFFFF`) and emitted MSB-first. This is the
    /// exact packing the LM75A datasheet specifies and the hand-coded `Lm75`
    /// produces; a 9-bit (0.5 °C) master reads the top 9 bits and sees the same
    /// temperature. A declarative LM75 spec using this encoding is therefore
    /// byte-identical to `Lm75` across the full temperature range.
    ///
    /// **Naming note:** the identifier `q7.1_be` is historical (the classic 9-bit
    /// LM75 is sometimes described as Q7.1 with 0.5 °C resolution). This
    /// implementation uses the LM75A's 11-bit / 0.125 °C resolution, which is a
    /// strict superset: any value expressible in the 9-bit format is also correct
    /// in the 11-bit format (the extra 2 bits of the count are zero for 0.5 °C
    /// multiples). Do not use this encoding expecting exactly 0.5 °C steps; it
    /// encodes at 0.125 °C resolution. If you need strict 9-bit / 0.5 °C packing,
    /// apply `scale = 2.0` and use `i16_be` with a right-shift-aware decoder.
    #[serde(rename = "q7.1_be")]
    Q71Be,
    /// Bosch BME280/BMP280 20-bit pressure/temperature packing, big-endian,
    /// spread across three bytes as `MSB, LSB, XLSB` where the 20-bit unsigned
    /// ADC count occupies `MSB[7:0] LSB[7:0] XLSB[7:4]` (XLSB's low nibble is
    /// unused / zero). This is the exact layout of the BME280 `press`/`temp`
    /// data registers (datasheet §5.4.6/§5.4.7): a read of `press_msb`
    /// auto-increments through `press_lsb`, `press_xlsb`.
    ///
    /// **Why this is a distinct encoding.** The existing integer encodings top
    /// out at 16 bits (`u16_*`/`i16_*`), so a 20-bit ADC count cannot be packed
    /// by them, and the value expressions (`evalexpr`) operate on scalars; they
    /// cannot themselves emit a 3-byte MSB/LSB/XLSB frame with a shifted low
    /// nibble. This encoding is the "minor encoding addition" the co-sim-fidelity
    /// BME280-style packed frame. It packs a **raw ADC count** (the
    /// register's `expr` supplies the count, e.g. an `adc_press` input); the
    /// raw↔physical Bosch compensation is applied by the firmware / test
    /// consumer, not here (see the BME280 spec header for why the compensation
    /// is not expressible as a forward `evalexpr` value expression).
    ///
    /// Packing: `count = round(value).clamp(0, 0xFFFFF)` (20-bit unsigned);
    /// `bytes = [ (count>>12)&0xFF, (count>>4)&0xFF, (count<<4)&0xF0 ]`.
    #[serde(rename = "u20_be_xlsb")]
    U20BeXlsb,
    /// Constant-byte register: no numeric value, `const` carries the bytes.
    Raw,
}

impl Encoding {
    /// The natural byte width of this encoding (the default for `bytes`).
    #[must_use]
    pub const fn natural_width(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16Be | Self::U16Le | Self::I16Be | Self::I16Le | Self::Q71Be => 2,
            Self::U20BeXlsb => 3,
            Self::Raw => 0,
        }
    }
}

/// A settable physical input the engine/test can drive (e.g. `temperature_c`),
/// referenced by name from register `expr`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputSpec {
    pub name: String,
    #[serde(default)]
    pub default: f64,
}

/// One register: an address plus how a read of it produces bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterSpec {
    /// Register address (the I2C pointer value or the SPI register index).
    pub addr: u8,

    /// How many bytes a read of this register returns. Defaults to the
    /// encoding's natural width (or the `const` length for a raw register).
    #[serde(default)]
    pub bytes: Option<usize>,

    /// How the numeric value is packed (omit for a pure-`const` register).
    #[serde(default)]
    pub encoding: Option<Encoding>,

    /// An `evalexpr` expression over the input names → the numeric value to
    /// encode. Omit for a `const` register.
    #[serde(default)]
    pub expr: Option<String>,

    /// Constant bytes for an identity / config register (`WHO_AM_I` etc).
    #[serde(default)]
    pub r#const: Option<Vec<u8>>,

    /// Optional linear pre-scale applied to `expr` before integer encoding:
    /// `encoded_value = expr * scale + offset`.
    #[serde(default)]
    pub scale: Option<f64>,
    #[serde(default)]
    pub offset: Option<f64>,
}

impl RegisterSpec {
    /// Effective number of bytes a read of this register returns.
    #[must_use]
    pub fn read_len(&self) -> usize {
        self.bytes
            .or_else(|| self.r#const.as_ref().map(Vec::len))
            .unwrap_or_else(|| self.encoding.map_or(1, Encoding::natural_width).max(1))
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Write side: what the FIRMWARE writes, and what those writes do.
//
// The read side above maps physical inputs → register bytes. The write side is
// the mirror: firmware bytes → decoded values → (a) stored variables that read
// expressions may reference (write→read coupling: an ADS1115 config write
// selects what the conversion register reads), and (b) per-channel device state
// that OUTPUT laws map to driven analog nets (a DAC code becoming a VOUT
// voltage on a net).
//
// ## The Rust/expression boundary (deliberate, documented)
//
// evalexpr has NO integer bit operations (the same constraint that makes the
// BME280 spec expose raw ADC counts). Write
// decode is bit-field surgery (a 12-bit DAC code straddling two bytes, a 3-bit
// mux field inside a config word), so the bit extraction lives HERE, in the
// declarative framing layer, as data ([`BitFieldSpec`]): a named field is a
// contiguous bit range of the decoded integer. Everything downstream of the
// extraction, state updates, output voltage laws, read-back expressions, is
// evalexpr over the extracted names. Faking bit ops inside expressions
// (division/modulo chains) would work for some fields and silently mis-decode
// others (sign, straddles); a declared bit range cannot.
//
// ## evalexpr equality footgun
//
// evalexpr `==` is TYPE-strict: a variable set as a float never equals an
// integer literal (`pd == 0` is false even when pd is 0.0). Every variable in
// these specs is bound as a float, so comparisons in `update`/`expr`/frame
// strings must use float literals: `pd == 0.0`, `if(gain_bit == 1.0, …)`.
// ─────────────────────────────────────────────────────────────────────────────

/// A named contiguous bit range `[high, low]` (inclusive, LSB = bit 0) of a
/// decoded integer value. The extracted field is exposed to expressions as a
/// float variable under `name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitFieldSpec {
    pub name: String,
    /// `[high, low]`, both inclusive. `bits = [14, 12]` is a 3-bit field.
    pub bits: [u8; 2],
}

impl BitFieldSpec {
    /// Extract this field from a decoded integer.
    #[must_use]
    pub const fn extract(&self, value: u32) -> u32 {
        let [high, low] = self.bits;
        let width = high - low + 1;
        (value >> low) & (u32::MAX >> 32u8.saturating_sub(width))
    }
}

/// One pointer-framed writable register (`i2c_pointer` write style): the
/// firmware writes the register pointer, then the payload bytes.
///
/// When the payload reaches the encoding's width it is decoded and stored under
/// `store` (a float variable read expressions may reference), and each declared
/// field is extracted and stored under its own name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteRegisterSpec {
    /// Register address (the I2C pointer value).
    pub addr: u8,
    /// How the payload bytes decode to the stored value. Must be a fixed-width
    /// integer encoding (`u8`/`u16_be`/`u16_le`/`i16_be`/`i16_le`); `raw` and
    /// the >16-bit read packings have no defined write decode and are rejected.
    pub encoding: Encoding,
    /// Variable name the decoded value is stored under.
    pub store: String,
    /// Initial stored value before any write (the register's POR value). Field
    /// variables are seeded by extracting from this default.
    #[serde(default)]
    pub default: f64,
    /// Named bit fields extracted from the decoded value (Rust-side, see the
    /// module boundary note above).
    #[serde(default, rename = "field")]
    pub fields: Vec<BitFieldSpec>,
}

/// A per-channel persistent state variable (e.g. a DAC channel's `code`).
/// With `sensor.channels = N`, each state holds N independent values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSpec {
    pub name: String,
    #[serde(default)]
    pub default: f64,
}

/// How a write command selects the channel each data group lands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelSource {
    /// Cursor auto-increments per completed group, reset to 0 at each write
    /// START (the MCP4728 Fast Write).
    Auto,
    /// Cursor seeded from a bit field of the PREFIX byte, then auto-increments
    /// per group (the MCP4728 Sequential Write). Requires `prefix = true`.
    PrefixBits,
    /// Taken from a bit field of each group's decoded value (the MCP4728
    /// Multi/Single Write, whose command byte repeats per group).
    GroupBits,
}

/// The channel-select block of a write command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSelectSpec {
    pub source: ChannelSource,
    /// `[high, low]` of the channel field, of the prefix byte for
    /// `prefix_bits`, of the group value for `group_bits`. Unused for `auto`.
    #[serde(default)]
    pub bits: Option<[u8; 2]>,
}

const fn default_channel_select() -> ChannelSelectSpec {
    ChannelSelectSpec {
        source: ChannelSource::Auto,
        bits: None,
    }
}

/// One command family of a command-framed write protocol (the MCP4728 shape).
///
/// The first write byte selects the command by mask/value, then the remaining
/// bytes decode as fixed-size big-endian groups whose bit fields update the
/// per-channel state.
///
/// Matching is FIRST-MATCH-WINS in spec order, so a more specific mask (the
/// MCP4728 Sequential Write, mask 0xF8) must be listed before a broader one it
/// overlaps (Multi/Single, mask 0xC0). A first byte matching no command makes
/// the whole transaction accepted-and-ignored; the honest analogue of a real
/// part ACK-ing a command family the model does not implement; the interpreter
/// counts these so a test can assert nothing was silently dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteCommandSpec {
    /// Diagnostic name ("`fast_write`").
    pub name: String,
    /// The command matches when `first_byte & match_mask == match_value`.
    pub match_mask: u8,
    pub match_value: u8,
    /// If true the matched byte is a PREFIX consumed before the data groups
    /// (it is not part of any group); if false it is byte 0 of the first group.
    #[serde(default)]
    pub prefix: bool,
    /// Bytes per repeating data group (1..=4). Each completed group forms a
    /// big-endian integer the fields are extracted from; groups decode greedily
    /// as bytes arrive, so state updates land mid-transaction exactly like a
    /// real part with its latch pin held active.
    pub group_bytes: usize,
    /// Which channel each group's updates land on.
    #[serde(default = "default_channel_select")]
    pub channel: ChannelSelectSpec,
    /// Named bit fields of each group's decoded value.
    #[serde(default, rename = "field")]
    pub fields: Vec<BitFieldSpec>,
    /// Per-channel state updates applied per completed group: state name →
    /// evalexpr over this command's fields + the channel's CURRENT state +
    /// inputs + stores. All right-hand sides evaluate against the pre-update
    /// snapshot, then commit together, so update order cannot matter
    /// (`vref = "if(vref_bit == 1.0, 2.048, vref)"` reads the old vref).
    pub update: std::collections::BTreeMap<String, String>,
}

/// A driven analog output: an evalexpr law over the device's variables (one
/// channel's state + inputs + stores) producing a voltage.
///
/// The engine binds a net driver to the output by name/channel; at each
/// transaction end (`on_stop(ctx)`) the law is evaluated and pushed onto the
/// net.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSpec {
    /// Stable name ("`vout_a`") the engine binds a driver to.
    pub name: String,
    /// Which channel's state the expression sees. Default 0.
    #[serde(default)]
    pub channel: usize,
    /// evalexpr voltage law, e.g.
    /// `"if(pd == 0.0, (code / 4096) * vref * gain, 0.0)"`.
    pub expr: String,
}

/// A streamed read frame for command-framed devices that answer a master read
/// with a fixed byte sequence instead of a pointered register (the MCP4728's
/// 24-byte status/EEPROM frame).
///
/// Each entry is an evalexpr producing one byte (rounded, clamped to 0..=255).
/// With `per_channel = true` the byte list is emitted once per channel (channel
/// 0 first), and each channel's exprs see that channel's state; reads wrap at
/// the frame end.
///
/// Mutually exclusive with `[[sensor.register]]`: a device either answers
/// pointered reads or streams a frame, both at once has no defined wire
/// meaning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadFrameSpec {
    #[serde(default)]
    pub per_channel: bool,
    pub bytes: Vec<String>,
}

/// The wire-protocol block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProtocolSpec {
    pub style: ProtocolStyle,
    /// SPI only: is the read bit a 1 in the high bit of the command byte?
    /// (true for most ST / `InvenSense` parts). Default true.
    #[serde(default = "default_true")]
    pub rw_read_is_high: bool,
    /// SPI only: mask applied to the command byte to recover the register addr.
    /// Default 0x7f.
    #[serde(default = "default_addr_mask")]
    pub addr_mask: u8,
    /// SPI only: the datasheet-declared clock mode, `(CPOL, CPHA)`:
    /// `0 = (0,0)`, `1 = (0,1)`, `2 = (1,0)`, `3 = (1,1)`. Governs the idle
    /// clock level and which edge samples vs. shifts on the bit-banged SPI
    /// responder. Default `0` (CPOL=0, CPHA=0); the historical behaviour, so
    /// specs that omit it are unchanged.
    #[serde(default)]
    pub spi_mode: u8,
    /// SPI only: every mode the datasheet allows (a part sampling on the
    /// rising edge often accepts 0 and 3); `spi_mode` is the one simulated.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spi_modes: Vec<u8>,
}

/// A board-strap level an address-select row names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StrapTie {
    Low,
    High,
    /// Tied straight to the bus data line. Four-level address pins (TI's
    /// `INA2xx` and `ADS1xxx` families) read SDA and SCL as two more levels.
    Sda,
    /// Tied straight to the bus clock line (see [`StrapTie::Sda`]).
    Scl,
}

/// One `[[sensor.address_select]]` row: the strap levels that select `address`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressSelect {
    pub address: u8,
    /// `[sensor.pins]` role -> the level it is tied to.
    pub straps: BTreeMap<String, StrapTie>,
}

/// The I2C address a board's wiring selects. `assumed` says why the address
/// is the spec's default rather than one the board's straps determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressChoice {
    pub address: u8,
    pub assumed: Option<String>,
}

const fn default_true() -> bool {
    true
}
const fn default_addr_mask() -> u8 {
    0x7f
}

const fn default_channels() -> usize {
    1
}

/// The body of the `[sensor]` table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sensor {
    pub name: String,
    pub bus: Bus,
    /// Required iff `bus = "i2c"`.
    #[serde(default)]
    pub i2c_address: Option<u8>,
    /// `[sensor.pins]`: pad number -> role, the shape of `[models.pins]`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub pins: BTreeMap<String, String>,
    /// Strap-selected alternate I2C addresses.
    #[serde(
        default,
        rename = "address_select",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub address_selects: Vec<AddressSelect>,
    #[serde(default, rename = "input")]
    pub inputs: Vec<InputSpec>,
    #[serde(default, rename = "register")]
    pub registers: Vec<RegisterSpec>,
    // ── Write side ──
    /// How many independent channels the per-channel `state` variables have
    /// (an MCP4728 has 4). Default 1.
    #[serde(default = "default_channels")]
    pub channels: usize,
    /// Per-channel persistent state variables.
    #[serde(default, rename = "state")]
    pub states: Vec<StateSpec>,
    /// Pointer-framed writable registers (mutually exclusive with
    /// `write_command`; the first write byte is either a pointer or a
    /// command, not both).
    #[serde(default, rename = "write_register")]
    pub write_registers: Vec<WriteRegisterSpec>,
    /// Command-framed write protocol (the MCP4728 shape).
    #[serde(default, rename = "write_command")]
    pub write_commands: Vec<WriteCommandSpec>,
    /// Driven analog outputs (voltage laws over state).
    #[serde(default, rename = "output")]
    pub outputs: Vec<OutputSpec>,
    /// Streamed read frame (mutually exclusive with `register`).
    #[serde(default)]
    pub read_frame: Option<ReadFrameSpec>,
    pub protocol: ProtocolSpec,
    /// Provenance and uncertainty, the same contract as a card's `[models.source]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<crate::schema::ModelSourceSpec>,
}

impl Sensor {
    /// The single source of truth for "what map key does a register live under".
    ///
    /// For an `spi_reg` sensor the R/W direction bit is folded into bit 7 of the
    /// command byte, so the interpreter recovers the register address as
    /// `addr & addr_mask`. Normalizing the stored key the same way lets a spec
    /// author write the raw datasheet address (e.g. BMP280 `0xD0`) while the
    /// pre-masked `0x50` resolves identically. I2C (`i2c_pointer`) uses a full
    /// 8-bit pointer with no folded direction bit, so its keys stay raw.
    ///
    /// Both `SensorSpec::validate`'s post-mask dedup and the engine's
    /// `RegisterMapSensor::from_spec` key construction call this, so there is one
    /// definition of the key mapping. The engine's live-transaction
    /// `normalize_addr` is the runtime analog and must stay consistent with it.
    #[must_use]
    pub fn register_key(&self, addr: u8) -> u8 {
        if self.bus == Bus::Spi && self.protocol.style == ProtocolStyle::SpiReg {
            addr & self.protocol.addr_mask
        } else {
            addr
        }
    }

    /// The bus roles `[sensor.pins]` must name for this bus.
    #[must_use]
    pub const fn bus_roles(&self) -> &'static [&'static str] {
        match self.bus {
            Bus::I2c => &["scl", "sda"],
            Bus::Spi => &["cs", "sck", "mosi", "miso"],
        }
    }

    /// The address the board's straps select. `level_of(role)` reports how the
    /// board ties a strap role, or why it cannot tell (floating, on a signal
    /// net). Without `[[sensor.address_select]]` the address is `i2c_address`;
    /// with it, an undeterminable strap or a combination the table does not list
    /// yields `i2c_address` marked assumed, never a silently chosen row. `None`
    /// for an SPI sensor.
    pub fn resolve_address(
        &self,
        level_of: impl Fn(&str) -> Result<StrapTie, String>,
    ) -> Option<AddressChoice> {
        let address = self.i2c_address?;
        let Some(first) = self.address_selects.first() else {
            return Some(AddressChoice {
                address,
                assumed: None,
            });
        };
        let assume = |why: String| AddressChoice {
            address,
            assumed: Some(format!(
                "I2C address 0x{address:02x} is assumed (the spec default): {why}"
            )),
        };
        let mut levels = BTreeMap::new();
        for role in first.straps.keys() {
            match level_of(role) {
                Ok(level) => levels.insert(role.clone(), level),
                Err(why) => return Some(assume(why)),
            };
        }
        let Some(row) = self.address_selects.iter().find(|row| row.straps == levels) else {
            let why = "match no [[sensor.address_select]] row";
            return Some(assume(format!("the board's strap levels {levels:?} {why}")));
        };
        Some(AddressChoice {
            address: row.address,
            assumed: None,
        })
    }
}

/// Top-level wrapper so the TOML root is `[sensor]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorSpec {
    pub sensor: Sensor,
}

/// A spec-validation failure.
#[derive(Debug, thiserror::Error)]
pub enum SensorSpecError {
    #[error("TOML parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("serialising sensor spec: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("validation: {0}")]
    Invalid(String),
}

/// `return Err(SensorSpecError::Invalid(format!(..)))`.
macro_rules! invalid {
    ($($arg:tt)*) => {
        return Err(SensorSpecError::Invalid(format!($($arg)*)))
    };
}

/// Refuse the spec unless `cond` holds, naming the offending field.
macro_rules! check {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            invalid!($($arg)*);
        }
    };
}

/// Refuse a `[high, low]` bit range that does not fit a `width_bits` value.
fn check_bits(what: &str, bits: [u8; 2], width_bits: usize) -> Result<(), SensorSpecError> {
    let [high, low] = bits;
    check!(
        high >= low,
        "{what}: bits = [{high}, {low}] must be [high, low] with high >= low"
    );
    check!(
        (high as usize) < width_bits,
        "{what}: bit {high} is out of range for a {width_bits}-bit value"
    );
    Ok(())
}

/// Every identifier `expr` references must be in `known`.
fn check_refs(
    what: impl std::fmt::Display,
    expr: &str,
    known: &HashSet<&str>,
) -> Result<(), SensorSpecError> {
    for tok in expr_identifiers(expr) {
        check!(
            known.contains(tok),
            "{what} references unknown variable '{tok}'"
        );
    }
    Ok(())
}

impl SensorSpec {
    /// Parse a `[sensor]` spec from TOML and validate it.
    ///
    /// # Errors
    ///
    /// A [`SensorSpecError`] when `src` is not a valid `[sensor]` spec in TOML, or
    /// when the parsed spec fails [`Self::validate`].
    pub fn from_toml(src: &str) -> Result<Self, SensorSpecError> {
        let spec: Self = toml::from_str(src)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Serialise back to TOML (used by the round-trip validation).
    ///
    /// # Errors
    ///
    /// A [`SensorSpecError`] when the TOML serialiser rejects the spec.
    pub fn to_toml(&self) -> Result<String, SensorSpecError> {
        Ok(toml::to_string(self)?)
    }

    /// Convenience accessor.
    #[must_use]
    pub const fn sensor(&self) -> &Sensor {
        &self.sensor
    }

    /// Structural validation: bus/address coherence, register encodings,
    /// expr/const exclusivity, protocol/bus agreement, byte widths, and the
    /// write side (framing coherence, bit ranges, namespace uniqueness,
    /// expression references).
    ///
    /// # Errors
    ///
    /// A [`SensorSpecError`] naming the first rule above that the spec breaks.
    // One flat walk, one rule per line, in the order the spec's fields are declared.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), SensorSpecError> {
        let s = &self.sensor;
        check!(!s.name.trim().is_empty(), "sensor.name must not be empty");
        if let Some(Err(e)) = s
            .source
            .as_ref()
            .map(crate::schema::ModelSourceSpec::validate)
        {
            invalid!("sensor.source: {e}");
        }

        // SPI mode is 0..=3 always (CPOL/CPHA are one bit each); a larger value
        // is a spec typo that must fail loud rather than be silently truncated
        // when the responder derives CPOL/CPHA from it, and a nonzero mode on
        // an I2C sensor is the same class of mistake.
        check!(
            s.protocol.spi_mode <= 3,
            "protocol.spi_mode {} is out of range; SPI modes are 0..=3 \
             (0=CPOL0/CPHA0, 1=CPOL0/CPHA1, 2=CPOL1/CPHA0, 3=CPOL1/CPHA1)",
            s.protocol.spi_mode
        );
        check!(
            s.bus == Bus::Spi || s.protocol.spi_mode == 0,
            "protocol.spi_mode = {} is SPI-only, but bus = \"i2c\"",
            s.protocol.spi_mode
        );
        let modes = &s.protocol.spi_modes;
        check!(
            modes.is_empty()
                || modes.contains(&s.protocol.spi_mode) && modes.iter().all(|m| *m <= 3),
            "protocol.spi_modes {modes:?} must list modes 0..=3 including spi_mode {}",
            s.protocol.spi_mode
        );
        match s.bus {
            Bus::I2c => {
                let Some(a) = s.i2c_address else {
                    invalid!("bus = \"i2c\" requires i2c_address");
                };
                check!(a <= 0x7f, "i2c_address 0x{a:02x} is not a 7-bit address");
                check!(
                    s.protocol.style == ProtocolStyle::I2cPointer,
                    "bus = \"i2c\" requires protocol.style = \"i2c_pointer\""
                );
            }
            Bus::Spi => {
                check!(
                    s.i2c_address.is_none(),
                    "bus = \"spi\" must not set i2c_address"
                );
                check!(
                    s.protocol.style == ProtocolStyle::SpiReg,
                    "bus = \"spi\" requires protocol.style = \"spi_reg\""
                );
                // A mask with bit 7 set would fold the R/W bit into the register
                // address, producing reads from unexpected register slots.
                check!(
                    s.protocol.addr_mask & 0x80 == 0,
                    "protocol.addr_mask 0x{:02x} includes bit 7 (the R/W bit); \
                     masks must cover only the address bits (e.g. 0x7f)",
                    s.protocol.addr_mask
                );
            }
        }
        check!(
            !(s.registers.is_empty()
                && s.write_registers.is_empty()
                && s.write_commands.is_empty()
                && s.read_frame.is_none()),
            "a sensor needs at least one [[sensor.register]], [[sensor.write_register]], \
             [[sensor.write_command]], or a [sensor.read_frame]; a command-driven part with no \
             register map states what a measurement read returns as [sensor.read_frame]"
        );

        self.validate_pins()?;
        self.validate_write_side()?;
        // Names a read expression may reference: every variable is bound as a
        // float at evaluation time, and `i2c_address` is a builtin the
        // interpreter injects. Per-channel state is NOT visible here.
        let names: HashSet<&str> = s
            .inputs
            .iter()
            .map(|i| i.name.as_str())
            .chain(s.write_registers.iter().flat_map(|w| {
                std::iter::once(w.store.as_str()).chain(w.fields.iter().map(|f| f.name.as_str()))
            }))
            .chain(std::iter::once("i2c_address"))
            .collect();

        // Register-address dedup uses the SAME key mapping the interpreter will
        // (`Sensor::register_key`: post-mask for SPI, raw for I2C), so a spec
        // author may write the raw datasheet address and two registers that
        // collide post-mask fail loud here rather than silently overwrite.
        let mut seen_addrs = HashSet::new();
        for r in &s.registers {
            let key = s.register_key(r.addr);
            check!(
                seen_addrs.insert(key),
                "duplicate register addr 0x{:02x}{}",
                r.addr,
                if key == r.addr {
                    String::new()
                } else {
                    format!(" (post-mask 0x{key:02x})")
                }
            );
            match (r.encoding, &r.r#const, &r.expr) {
                (None | Some(Encoding::Raw), Some(bytes), None) => {
                    check!(
                        !bytes.is_empty(),
                        "register 0x{:02x} is const but has no bytes",
                        r.addr
                    );
                }
                (Some(Encoding::Raw), None, Some(_)) => {
                    invalid!(
                        "register 0x{:02x} uses encoding \"raw\" but also an expr",
                        r.addr
                    );
                }
                (Some(enc), None, Some(expr)) => {
                    check!(
                        r.read_len() != 0,
                        "register 0x{:02x} has zero read length",
                        r.addr
                    );
                    // A `bytes` that disagrees with the encoding's natural width
                    // would make the interpreter truncate or pad the reply; an
                    // LLM-extracted spec must fail loudly instead.
                    let natural = enc.natural_width();
                    if let Some(declared) = r.bytes.filter(|_| natural > 0) {
                        check!(
                            declared == natural,
                            "register 0x{:02x} declares bytes={declared} but encoding {enc:?} \
                             produces {natural} bytes; bytes must equal the encoding's natural \
                             width or be omitted",
                            r.addr
                        );
                    }
                    for tok in expr_identifiers(expr) {
                        check!(
                            names.contains(tok),
                            "register 0x{:02x} expr references unknown input '{tok}'",
                            r.addr
                        );
                    }
                }
                (None, None, Some(_)) => {
                    invalid!("register 0x{:02x} has an expr but no encoding", r.addr);
                }
                _ => invalid!(
                    "register 0x{:02x} must be either a const register or an \
                     (encoding + expr) register, not both/neither",
                    r.addr
                ),
            }
        }
        Ok(())
    }

    /// `[sensor.pins]` names the bus roles; every address-select row is a
    /// distinct 7-bit address over the same `[sensor.pins]` strap roles, and
    /// one of them is `i2c_address`.
    fn validate_pins(&self) -> Result<(), SensorSpecError> {
        let s = &self.sensor;
        let has_role = |role: &str| s.pins.values().any(|r| r == role);
        for (pad, role) in &s.pins {
            check!(
                !pad.trim().is_empty() && !role.trim().is_empty(),
                "sensor.pins: pad '{pad}' -> role '{role}' must both be non-empty"
            );
        }
        if !s.pins.is_empty() {
            for role in s.bus_roles() {
                check!(
                    has_role(role),
                    "sensor.pins declares no '{role}' pin, which bus = \"{}\" needs",
                    if s.bus == Bus::I2c { "i2c" } else { "spi" }
                );
            }
        }
        let Some(first) = s.address_selects.first() else {
            return Ok(());
        };
        check!(
            s.bus == Bus::I2c,
            "sensor.address_select is only valid for bus = \"i2c\""
        );
        let (mut addresses, mut combos) = (HashSet::new(), HashSet::new());
        for row in &s.address_selects {
            let what = format!("sensor.address_select 0x{:02x}", row.address);
            check!(row.address <= 0x7f, "{what} is not a 7-bit address");
            check!(addresses.insert(row.address), "{what} is listed twice");
            check!(!row.straps.is_empty(), "{what} names no straps");
            check!(
                row.straps.keys().eq(first.straps.keys()),
                "{what} must name the same strap roles as the first row"
            );
            check!(
                combos.insert(&row.straps),
                "{what} repeats another row's strap levels"
            );
            for role in row.straps.keys() {
                check!(
                    has_role(role),
                    "{what}: strap '{role}' is not a role in [sensor.pins]"
                );
            }
        }
        check!(
            s.i2c_address.is_some_and(|a| addresses.contains(&a)),
            "i2c_address must be one of the sensor.address_select addresses"
        );
        Ok(())
    }

    /// The write side: framing coherence, bit ranges, namespace uniqueness,
    /// expression references.
    // One flat walk over the write side, one rule per line, in declaration order.
    #[allow(clippy::too_many_lines)]
    fn validate_write_side(&self) -> Result<(), SensorSpecError> {
        let s = &self.sensor;
        let has_write_side = !s.write_registers.is_empty()
            || !s.write_commands.is_empty()
            || !s.outputs.is_empty()
            || s.read_frame.is_some();
        // The SPI write phase stays accept-and-ignore: no current device needs
        // it, and an untested SPI write decode would be fake coverage.
        check!(
            !has_write_side || s.bus == Bus::I2c,
            "the write side (write_register/write_command/output/read_frame) is modeled for \
             bus = \"i2c\" only; the SPI write phase is accept-and-ignore (a stated \
             limitation, not a capability)"
        );
        check!(
            s.write_registers.is_empty() || s.write_commands.is_empty(),
            "write_register (pointer framing) and write_command (command framing) are \
             mutually exclusive: the first write byte cannot be both a register pointer and \
             a command byte"
        );
        check!(
            s.read_frame.is_none() || s.registers.is_empty(),
            "read_frame and [[sensor.register]] are mutually exclusive: a device either \
             streams a fixed frame on reads or answers the register pointer, not both"
        );
        check!(s.channels != 0, "sensor.channels must be >= 1");

        // One flat namespace: inputs, stores, write-register fields and states
        // all coexist in the evaluation context, so every name is globally
        // unique. A command's fields may repeat ACROSS commands (only one
        // command is active per transaction) but not against the globals.
        let declared = s
            .inputs
            .iter()
            .map(|i| ("input", i.name.as_str()))
            .chain(s.write_registers.iter().flat_map(|w| {
                std::iter::once(("write_register store", w.store.as_str())).chain(
                    w.fields
                        .iter()
                        .map(|f| ("write_register field", f.name.as_str())),
                )
            }))
            .chain(s.states.iter().map(|st| ("state", st.name.as_str())));
        let mut names: HashSet<&str> = HashSet::new();
        for (kind, n) in declared {
            check!(
                n != "i2c_address",
                "{kind} 'i2c_address' shadows the builtin variable"
            );
            check!(!n.trim().is_empty(), "{kind} has an empty name");
            check!(
                names.insert(n),
                "duplicate variable name '{n}' ({kind}); inputs, stores, fields, and states \
                 share one expression namespace"
            );
        }
        for c in &s.write_commands {
            let mut per_cmd = names.clone();
            for f in &c.fields {
                check!(
                    f.name != "i2c_address",
                    "write_command field 'i2c_address' shadows the builtin variable"
                );
                check!(
                    per_cmd.insert(f.name.as_str()),
                    "write_command '{}' field '{}' collides with another variable in the \
                     expression namespace",
                    c.name,
                    f.name
                );
            }
        }

        let mut seen_wr = HashSet::new();
        for w in &s.write_registers {
            check!(
                seen_wr.insert(w.addr),
                "duplicate write_register addr 0x{:02x}",
                w.addr
            );
            // q7.1_be / u20_be_xlsb are READ packings; a write decode nothing
            // exercises would be fake coverage.
            check!(
                matches!(
                    w.encoding,
                    Encoding::U8
                        | Encoding::U16Be
                        | Encoding::U16Le
                        | Encoding::I16Be
                        | Encoding::I16Le
                ),
                "write_register 0x{:02x}: encoding {:?} has no defined write decode \
                 (fixed-width integer encodings only)",
                w.addr,
                w.encoding
            );
            for f in &w.fields {
                check_bits(
                    &format!("write_register 0x{:02x} field '{}'", w.addr, f.name),
                    f.bits,
                    w.encoding.natural_width() * 8,
                )?;
            }
        }

        let state_names: HashSet<&str> = s.states.iter().map(|st| st.name.as_str()).collect();
        for c in &s.write_commands {
            let cmd = format!("write_command '{}'", c.name);
            check!(
                c.match_value & !c.match_mask == 0,
                "{cmd}: match_value 0x{:02x} has bits outside match_mask 0x{:02x}, so it can \
                 never match",
                c.match_value,
                c.match_mask
            );
            check!(
                (1..=4).contains(&c.group_bytes),
                "{cmd}: group_bytes must be 1..=4"
            );
            let group_bits = c.group_bytes * 8;
            for f in &c.fields {
                check_bits(&format!("{cmd} field '{}'", f.name), f.bits, group_bits)?;
            }
            let channel_width = match c.channel.source {
                ChannelSource::Auto => {
                    check!(
                        c.channel.bits.is_none(),
                        "{cmd}: channel source \"auto\" takes no bits"
                    );
                    None
                }
                ChannelSource::PrefixBits => {
                    check!(
                        c.prefix,
                        "{cmd}: channel source \"prefix_bits\" requires prefix = true"
                    );
                    Some(("prefix_bits", 8))
                }
                ChannelSource::GroupBits => Some(("group_bits", group_bits)),
            };
            if let Some((source, width)) = channel_width {
                let Some(bits) = c.channel.bits else {
                    invalid!("{cmd}: channel source \"{source}\" needs bits");
                };
                check_bits(&format!("{cmd} channel"), bits, width)?;
            }
            check!(
                !c.update.is_empty(),
                "{cmd} has no update entries; a command that decodes to nothing is a spec bug \
                 (unmodeled commands are simply not declared)"
            );
            let mut update_names = names.clone();
            update_names.insert("i2c_address");
            update_names.extend(c.fields.iter().map(|f| f.name.as_str()));
            for (target, expr) in &c.update {
                check!(
                    state_names.contains(target.as_str()),
                    "{cmd} updates unknown state '{target}'"
                );
                check_refs(format!("{cmd} update '{target}'"), expr, &update_names)?;
            }
        }

        // Outputs and the read frame see the states too (a channel context is
        // supplied at evaluation).
        names.insert("i2c_address");
        let mut seen_outputs = HashSet::new();
        for o in &s.outputs {
            check!(
                seen_outputs.insert(o.name.as_str()),
                "duplicate output name '{}'",
                o.name
            );
            check!(
                o.channel < s.channels,
                "output '{}' channel {} is out of range (channels = {})",
                o.name,
                o.channel,
                s.channels
            );
            check_refs(format!("output '{}' expr", o.name), &o.expr, &names)?;
        }
        if let Some(frame) = &s.read_frame {
            check!(
                !frame.bytes.is_empty(),
                "read_frame.bytes must not be empty"
            );
            for (i, b) in frame.bytes.iter().enumerate() {
                check_refs(format!("read_frame byte {i}"), b, &names)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LM75: &str = r#"
[sensor]
name = "LM75"
bus = "i2c"
i2c_address = 0x48

[[sensor.input]]
name = "temperature_c"
default = 25.0

[[sensor.register]]
addr = 0x00
bytes = 2
encoding = "q7.1_be"
expr = "temperature_c"

[[sensor.register]]
addr = 0x01
const = [0x00]

[sensor.protocol]
style = "i2c_pointer"
"#;

    /// ADS1115-shaped pointer-framed write register with bit fields, and a read
    /// register whose expr references the extracted fields.
    const WRITE_REG_SPEC: &str = r#"
[sensor]
name = "MINIADC"
bus = "i2c"
i2c_address = 0x48

[[sensor.input]]
name = "a0"
default = 0.0

[[sensor.write_register]]
addr = 0x01
encoding = "u16_be"
store = "config"
default = 33667.0   # 0x8383

[[sensor.write_register.field]]
name = "pga"
bits = [11, 9]

[[sensor.register]]
addr = 0x00
encoding = "i16_be"
expr = "a0 * 32768 / if(pga == 1.0, 4.096, 2.048)"

[sensor.protocol]
style = "i2c_pointer"
"#;

    /// MCP4728-shaped command framing: match/prefix/groups/channel/update.
    const WRITE_CMD_SPEC: &str = r#"
[sensor]
name = "MINIDAC"
bus = "i2c"
i2c_address = 0x60
channels = 4

[[sensor.state]]
name = "code"
default = 0.0

[[sensor.state]]
name = "pd"
default = 0.0

[[sensor.write_command]]
name = "fast_write"
match_mask = 0xC0
match_value = 0x00
group_bytes = 2

[[sensor.write_command.field]]
name = "pd_bits"
bits = [13, 12]

[[sensor.write_command.field]]
name = "code_bits"
bits = [11, 0]

[sensor.write_command.update]
code = "code_bits"
pd = "pd_bits"

[[sensor.output]]
name = "vout_a"
channel = 0
expr = "if(pd == 0.0, code / 4096 * 4.096, 0.0)"

[sensor.protocol]
style = "i2c_pointer"
"#;

    /// LM75 plus a pad map and a two-strap address table (three of the four
    /// combinations listed).
    fn strapped() -> String {
        LM75.replace(
            "[sensor.protocol]",
            "[sensor.pins]\n1 = \"sda\"\n2 = \"scl\"\n3 = \"alert\"\n4 = \"gnd\"\n5 = \"a1\"\n\
             6 = \"a0\"\n8 = \"vdd\"\n\
             [[sensor.address_select]]\naddress = 0x48\nstraps = { a0 = \"low\", a1 = \"low\" }\n\
             [[sensor.address_select]]\naddress = 0x49\nstraps = { a0 = \"high\", a1 = \"low\" }\n\
             [[sensor.address_select]]\naddress = 0x4a\nstraps = { a0 = \"low\", a1 = \"high\" }\n\
             [sensor.protocol]",
        )
    }

    #[test]
    fn pins_and_address_select_round_trip_and_resolve_from_straps() {
        let spec = SensorSpec::from_toml(&strapped()).unwrap();
        let back = SensorSpec::from_toml(&spec.to_toml().unwrap()).unwrap();
        assert_eq!(back.sensor.pins, spec.sensor.pins);
        assert_eq!(back.sensor.address_selects, spec.sensor.address_selects);
        assert_eq!(back.sensor.pins.get("3").map(String::as_str), Some("alert"));
        let s = &spec.sensor;
        let tied = |a0, a1| move |role: &str| Ok(if role == "a0" { a0 } else { a1 });
        let chosen = s
            .resolve_address(tied(StrapTie::High, StrapTie::Low))
            .unwrap();
        assert_eq!((chosen.address, chosen.assumed), (0x49, None));
        let unlisted = s
            .resolve_address(tied(StrapTie::High, StrapTie::High))
            .unwrap();
        assert_eq!(unlisted.address, 0x48);
        assert!(unlisted.assumed.unwrap().contains("match no"));
        let floating = s
            .resolve_address(|role: &str| Err(format!("strap role '{role}' is floating")))
            .unwrap();
        assert_eq!(floating.address, 0x48);
        assert!(floating.assumed.unwrap().contains("assumed"));
        let plain = SensorSpec::from_toml(LM75).unwrap().sensor;
        let unstrapped = plain.resolve_address(|_: &str| Err(String::new())).unwrap();
        assert_eq!((unstrapped.address, unstrapped.assumed), (0x48, None));
    }

    fn spi_spec(protocol_extra: &str) -> String {
        format!(
            "[sensor]\nname = \"MINIMAG\"\nbus = \"spi\"\n[[sensor.input]]\nname = \"x\"\ndefault = 0.0\n\
             [[sensor.register]]\naddr = 0x0f\nconst = [0x42]\n[[sensor.register]]\naddr = 0x10\nbytes = 2\n\
             encoding = \"i16_le\"\nexpr = \"x\"\n[sensor.protocol]\nstyle = \"spi_reg\"\n{protocol_extra}\n"
        )
    }

    #[test]
    fn lm75_parses_and_round_trips() {
        let spec = SensorSpec::from_toml(LM75).unwrap();
        assert_eq!(spec.sensor.name, "LM75");
        assert_eq!(spec.sensor.bus, Bus::I2c);
        assert_eq!(spec.sensor.i2c_address, Some(0x48));
        assert_eq!(spec.sensor.registers.len(), 2);
        let temp = &spec.sensor.registers[0];
        assert_eq!(
            (temp.addr, temp.encoding, temp.read_len()),
            (0x00, Some(Encoding::Q71Be), 2)
        );

        let reparsed = SensorSpec::from_toml(&spec.to_toml().unwrap()).unwrap();
        assert_eq!(reparsed.sensor.registers.len(), 2);
        assert_eq!(reparsed.sensor.registers[0].encoding, Some(Encoding::Q71Be));
    }

    #[test]
    fn write_register_and_command_specs_parse_with_field_extraction() {
        let spec = SensorSpec::from_toml(WRITE_REG_SPEC).unwrap();
        let w = &spec.sensor.write_registers[0];
        assert_eq!(w.store, "config");
        assert_eq!(w.fields[0].bits, [11, 9]);
        assert_eq!(w.fields[0].extract(0x8383), 1);

        let spec = SensorSpec::from_toml(WRITE_CMD_SPEC).unwrap();
        let s = &spec.sensor;
        assert_eq!(s.channels, 4);
        let c = &s.write_commands[0];
        assert_eq!(c.channel.source, ChannelSource::Auto);
        assert_eq!(c.update.len(), 2);
        assert_eq!(c.fields[0].extract(0x3800), 3);
        assert_eq!(c.fields[1].extract(0x3800), 0x800);
    }

    #[test]
    fn spi_spec_parses_with_mode() {
        let spec =
            SensorSpec::from_toml(&spi_spec("rw_read_is_high = true\naddr_mask = 0x7f")).unwrap();
        assert_eq!(spec.sensor.bus, Bus::Spi);
        assert_eq!(spec.sensor.protocol.style, ProtocolStyle::SpiReg);
        assert!(spec.sensor.protocol.rw_read_is_high);
        assert_eq!(spec.sensor.protocol.addr_mask, 0x7f);
        assert_eq!(spec.sensor.protocol.spi_mode, 0);
        let spec = SensorSpec::from_toml(&spi_spec("spi_mode = 3\nspi_modes = [0, 3]")).unwrap();
        assert_eq!(spec.sensor.protocol.spi_mode, 3);
    }

    #[test]
    fn malformed_specs_are_rejected() {
        let cases = [
            LM75.replace("i2c_address = 0x48\n", ""),
            LM75.replace("expr = \"temperature_c\"", "expr = \"undeclared_thing * 2\""),
            WRITE_CMD_SPEC.replace(
                "[[sensor.output]]",
                "[[sensor.write_register]]\naddr = 0x02\nencoding = \"u8\"\nstore = \"x\"\n\n[[sensor.output]]",
            ),
            WRITE_CMD_SPEC.replace("bits = [13, 12]", "bits = [16, 12]"),
            WRITE_CMD_SPEC.replace("pd = \"pd_bits\"", "gain = \"pd_bits\""),
            WRITE_REG_SPEC.replace("store = \"config\"", "store = \"a0\""),
            WRITE_REG_SPEC.replace("encoding = \"u16_be\"\nstore", "encoding = \"q7.1_be\"\nstore"),
            spi_spec("spi_mode = 4"),
            spi_spec("spi_mode = 1\nspi_modes = [0, 3]"),
            LM75.replace("style = \"i2c_pointer\"", "style = \"i2c_pointer\"\nspi_mode = 2"),
            LM75.replace("[sensor.protocol]", "[sensor.read_frame]\nbytes = [\"1.0\"]\n[sensor.protocol]"),
            strapped().replace("address = 0x48\nstraps", "address = 0x4b\nstraps"),
            strapped().replace("6 = \"a0\"", "6 = \"addr\""),
            strapped().replace("2 = \"scl\"", "2 = \"clk\""),
            strapped().replace("a0 = \"high\", a1 = \"low\"", "a0 = \"high\""),
            strapped().replace("a0 = \"high\", a1 = \"low\"", "a0 = \"low\", a1 = \"high\""),
            "[sensor]\nname = \"X\"\nbus = \"spi\"\n[[sensor.write_register]]\naddr = 0x01\nencoding = \"u8\"\n\
             store = \"cfg\"\n[sensor.protocol]\nstyle = \"spi_reg\"\n"
                .to_string(),
        ];
        for (i, bad) in cases.iter().enumerate() {
            assert!(
                SensorSpec::from_toml(bad).is_err(),
                "case {i} must be rejected"
            );
        }
        let sourced = format!(
            "{LM75}[sensor.source]\ntier = \"datasheet-derived\"\nuncertainty = [{{ status = \"interval\", \
             parameter = \"temperature_c\", low = 2.0, high = -2.0, unit = \"C\", \
             kind = \"specification-limits\", basis = \"Table 1\" }}]\n"
        );
        let error = SensorSpec::from_toml(&sourced).unwrap_err().to_string();
        assert!(error.contains("sensor.source: uncertainty 0"), "{error}");
        let good = SensorSpec::from_toml(
            &sourced.replace("low = 2.0, high = -2.0", "low = -2.0, high = 2.0"),
        );
        assert!(SensorSpec::from_toml(&good.unwrap().to_toml().unwrap())
            .unwrap()
            .sensor
            .source
            .is_some());
    }
}
