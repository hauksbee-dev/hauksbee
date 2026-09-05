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
//! Long-form how-and-why: docs/how-and-why/hauksbee-models/sensor_spec.md.
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

use std::collections::HashSet;

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
    pub fn natural_width(self) -> usize {
        match self {
            Encoding::U8 => 1,
            Encoding::U16Be
            | Encoding::U16Le
            | Encoding::I16Be
            | Encoding::I16Le
            | Encoding::Q71Be => 2,
            Encoding::U20BeXlsb => 3,
            Encoding::Raw => 0,
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

    /// Constant bytes for an identity / config register (WHO_AM_I etc).
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
    pub fn read_len(&self) -> usize {
        if let Some(b) = self.bytes {
            return b;
        }
        if let Some(c) = &self.r#const {
            return c.len();
        }
        self.encoding.map(|e| e.natural_width()).unwrap_or(1).max(1)
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
    pub fn extract(&self, value: u32) -> u32 {
        let [high, low] = self.bits;
        let width = high - low + 1;
        let mask = if width >= 32 {
            u32::MAX
        } else {
            (1u32 << width) - 1
        };
        (value >> low) & mask
    }
}

/// One pointer-framed writable register (`i2c_pointer` write style): the
/// firmware writes the register pointer, then the payload bytes. When the
/// payload reaches the encoding's width it is decoded and stored under `store`
/// (a float variable read expressions may reference), and each declared field
/// is extracted and stored under its own name.
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

fn default_channel_select() -> ChannelSelectSpec {
    ChannelSelectSpec {
        source: ChannelSource::Auto,
        bits: None,
    }
}

/// One command family of a command-framed write protocol (the MCP4728 shape):
/// the first write byte selects the command by mask/value, then the remaining
/// bytes decode as fixed-size big-endian groups whose bit fields update the
/// per-channel state.
///
/// Matching is FIRST-MATCH-WINS in spec order, so a more specific mask (the
/// MCP4728 Sequential Write, mask 0xF8) must be listed before a broader one it
/// overlaps (Multi/Single, mask 0xC0). A first byte matching no command makes
/// the whole transaction accepted-and-ignored; the honest analogue of a real
/// part ACKing a command family the model does not implement; the interpreter
/// counts these so a test can assert nothing was silently dropped.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteCommandSpec {
    /// Diagnostic name ("fast_write").
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
/// channel's state + inputs + stores) producing a voltage. The engine binds a
/// net driver to the output by name/channel; at each transaction end
/// (`on_stop(ctx)`) the law is evaluated and pushed onto the net.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSpec {
    /// Stable name ("vout_a") the engine binds a driver to.
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
/// 24-byte status/EEPROM frame). Each entry is an evalexpr producing one byte
/// (rounded, clamped to 0..=255). With `per_channel = true` the byte list is
/// emitted once per channel (channel 0 first), and each channel's exprs see
/// that channel's state; reads wrap at the frame end.
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
    /// (true for most ST / InvenSense parts). Default true.
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
}

fn default_true() -> bool {
    true
}
fn default_addr_mask() -> u8 {
    0x7f
}

fn default_channels() -> usize {
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
    pub fn register_key(&self, addr: u8) -> u8 {
        if self.bus == Bus::Spi && self.protocol.style == ProtocolStyle::SpiReg {
            addr & self.protocol.addr_mask
        } else {
            addr
        }
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
    pub fn from_toml(src: &str) -> Result<Self, SensorSpecError> {
        let spec: SensorSpec = toml::from_str(src)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Serialise back to TOML (used by the round-trip validation).
    pub fn to_toml(&self) -> Result<String, SensorSpecError> {
        Ok(toml::to_string(self)?)
    }

    /// Convenience accessor.
    pub fn sensor(&self) -> &Sensor {
        &self.sensor
    }

    /// Structural validation: bus/address coherence, register encodings,
    /// expr/const exclusivity, protocol/bus agreement, byte widths, and the
    /// write side (framing coherence, bit ranges, namespace uniqueness,
    /// expression references).
    pub fn validate(&self) -> Result<(), SensorSpecError> {
        let s = &self.sensor;
        check!(!s.name.trim().is_empty(), "sensor.name must not be empty");

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
             [[sensor.write_command]], or a [sensor.read_frame]"
        );

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
                if key != r.addr {
                    format!(" (post-mask 0x{key:02x})")
                } else {
                    String::new()
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

    /// The write side: framing coherence, bit ranges, namespace uniqueness,
    /// expression references.
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
        let spec = SensorSpec::from_toml(&spi_spec("spi_mode = 3")).unwrap();
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
            LM75.replace("style = \"i2c_pointer\"", "style = \"i2c_pointer\"\nspi_mode = 2"),
            LM75.replace("[sensor.protocol]", "[sensor.read_frame]\nbytes = [\"1.0\"]\n[sensor.protocol]"),
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
    }
}
