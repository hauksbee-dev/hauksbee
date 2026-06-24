//! Declarative register-map sensor specification.
//!
//! Instead of hand-coding each I2C/SPI sensor model in Rust (today `Lm75`,
//! `Mcp3008` in `hauksbee-engine`), a sensor is described DECLARATIVELY here:
//! its bus, address / SPI framing, register map, and per-register value
//! encoding. The engine's generic `RegisterMapSensor` interpreter realizes the
//! spec as an `I2cSlave` / `SpiSlave`; the datasheet extractor
//! (`model-extract --kind i2c_sensor`) fills the spec in from a datasheet.
//!
//! This module owns the *format* and *validation* only — no bus behaviour and
//! no `evalexpr` evaluation (that lives engine-side, where the expression
//! evaluator already is). It is the shared contract both the interpreter and
//! the extractor validate against.
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
//! ### SPI register addresses: write the raw datasheet value
//!
//! For an `spi_reg` sensor the R/W direction bit is folded into bit 7 of the
//! command byte, and the interpreter recovers the register address as
//! `cmd & addr_mask` (default `0x7f`). You may therefore write a register `addr`
//! as the **raw datasheet register address** — e.g. the BMP280 chip-ID register,
//! which the datasheet lists as `0xD0`. The interpreter normalizes both stored
//! keys and the incoming command address by `addr_mask`, so `0xD0` and the
//! pre-masked `0x50` resolve to the same register (backward compatible). Because
//! addresses are compared post-mask, two SPI registers that collide to the same
//! `addr & addr_mask` are rejected by validation.
//!
//! I2C (`i2c_pointer`) uses a full 8-bit register pointer with no folded
//! direction bit, so its `addr` values are used raw (unmasked).

use serde::{Deserialize, Serialize};

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
    /// multiples). Do not use this encoding expecting exactly 0.5 °C steps — it
    /// encodes at 0.125 °C resolution. If you need strict 9-bit / 0.5 °C packing,
    /// apply `scale = 2.0` and use `i16_be` with a right-shift-aware decoder.
    #[serde(rename = "q7.1_be")]
    Q71Be,
    /// Constant-byte register: no numeric value, `const` carries the bytes.
    Raw,
}

impl Encoding {
    /// The natural byte width of this encoding (the default for `bytes`).
    pub fn natural_width(self) -> usize {
        match self {
            Encoding::U8 => 1,
            Encoding::U16Be | Encoding::U16Le | Encoding::I16Be | Encoding::I16Le | Encoding::Q71Be => 2,
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
}

fn default_true() -> bool {
    true
}
fn default_addr_mask() -> u8 {
    0x7f
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
    /// expr/const exclusivity, protocol/bus agreement, byte widths.
    pub fn validate(&self) -> Result<(), SensorSpecError> {
        let s = &self.sensor;
        let err = |m: String| SensorSpecError::Invalid(m);

        if s.name.trim().is_empty() {
            return Err(err("sensor.name must not be empty".into()));
        }

        match s.bus {
            Bus::I2c => {
                let a = s
                    .i2c_address
                    .ok_or_else(|| err("bus = \"i2c\" requires i2c_address".into()))?;
                if a > 0x7f {
                    return Err(err(format!("i2c_address 0x{a:02x} is not a 7-bit address")));
                }
                if s.protocol.style != ProtocolStyle::I2cPointer {
                    return Err(err(
                        "bus = \"i2c\" requires protocol.style = \"i2c_pointer\"".into(),
                    ));
                }
            }
            Bus::Spi => {
                if s.i2c_address.is_some() {
                    return Err(err("bus = \"spi\" must not set i2c_address".into()));
                }
                if s.protocol.style != ProtocolStyle::SpiReg {
                    return Err(err(
                        "bus = \"spi\" requires protocol.style = \"spi_reg\"".into(),
                    ));
                }
                // addr_mask must not include bit 7 (the R/W bit in the command byte).
                // A mask with bit 7 set would silently fold the direction bit into the
                // register address, producing reads from unexpected register slots.
                if s.protocol.addr_mask & 0x80 != 0 {
                    return Err(err(format!(
                        "protocol.addr_mask 0x{:02x} includes bit 7 (the R/W bit); \
                         masks must cover only the address bits (e.g. 0x7f)",
                        s.protocol.addr_mask
                    )));
                }
            }
        }

        if s.registers.is_empty() {
            return Err(err("a sensor needs at least one [[sensor.register]]".into()));
        }

        // Inputs referenced by exprs must be declared; collect names.
        let input_names: std::collections::HashSet<&str> =
            s.inputs.iter().map(|i| i.name.as_str()).collect();

        // Register-address dedup uses the SAME key mapping the interpreter will:
        // `Sensor::register_key` (post-mask for SPI, raw for I2C). For SPI this
        // lets a spec author write the raw datasheet address (e.g. 0xD0 instead
        // of the pre-masked 0x50); two registers that collide to the same
        // post-mask key are genuinely indistinguishable on the wire, so that must
        // fail loud here rather than silently overwrite in the engine map.
        let mut seen_addrs = std::collections::HashSet::new();
        for r in &s.registers {
            let key = s.register_key(r.addr);
            if !seen_addrs.insert(key) {
                return Err(err(format!(
                    "duplicate register addr 0x{:02x}{}",
                    r.addr,
                    if key != r.addr {
                        format!(" (post-mask 0x{:02x})", key)
                    } else {
                        String::new()
                    }
                )));
            }

            let has_expr = r.expr.is_some();
            let has_const = r.r#const.is_some();

            match (&r.encoding, has_const, has_expr) {
                // Raw / const-only register.
                (None, true, false) | (Some(Encoding::Raw), true, false) => {
                    if r.r#const.as_ref().map(|c| c.is_empty()).unwrap_or(true) {
                        return Err(err(format!(
                            "register 0x{:02x} is const but has no bytes",
                            r.addr
                        )));
                    }
                }
                // Encoded register driven by an expr.
                (Some(enc), false, true) => {
                    if *enc == Encoding::Raw {
                        return Err(err(format!(
                            "register 0x{:02x} uses encoding \"raw\" but also an expr",
                            r.addr
                        )));
                    }
                    if r.read_len() == 0 {
                        return Err(err(format!(
                            "register 0x{:02x} has zero read length",
                            r.addr
                        )));
                    }
                    // Reject `bytes` that disagrees with the encoding's natural width.
                    // Allowing a mismatch silently would make the interpreter return a
                    // different number of bytes than the declared `bytes` field — either
                    // truncating data or padding with unrelated bytes. Validation must
                    // catch this so LLM-extracted specs fail loudly instead of emitting
                    // plausible-but-wrong bus traffic.
                    if let Some(declared_bytes) = r.bytes {
                        let natural = enc.natural_width();
                        if natural > 0 && declared_bytes != natural {
                            return Err(err(format!(
                                "register 0x{:02x} declares bytes={declared_bytes} but \
                                 encoding {:?} produces {natural} bytes; \
                                 bytes must equal the encoding's natural width or be omitted",
                                r.addr, enc
                            )));
                        }
                    }
                    // Cheap reference check: every bare identifier in the expr
                    // that isn't a number/operator should be a declared input.
                    for tok in expr_identifiers(r.expr.as_deref().unwrap_or("")) {
                        if !input_names.contains(tok.as_str()) {
                            return Err(err(format!(
                                "register 0x{:02x} expr references unknown input '{tok}'",
                                r.addr
                            )));
                        }
                    }
                }
                (None, false, true) => {
                    return Err(err(format!(
                        "register 0x{:02x} has an expr but no encoding",
                        r.addr
                    )));
                }
                _ => {
                    return Err(err(format!(
                        "register 0x{:02x} must be either a const register or an \
                         (encoding + expr) register, not both/neither",
                        r.addr
                    )));
                }
            }
        }

        Ok(())
    }
}

/// Extract the bare identifiers from an expression (anything that starts with a
/// letter or `_`), so the validator can check they are declared inputs. This is
/// deliberately conservative: it tolerates numbers and operators and only flags
/// undeclared *names*.
fn expr_identifiers(expr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    fn flush(cur: &mut String, out: &mut Vec<String>) {
        if !cur.is_empty() {
            // Keep tokens that start with a letter or underscore (skip numbers).
            if cur.chars().next().map(|c| c.is_alphabetic() || c == '_').unwrap_or(false) {
                out.push(cur.clone());
            }
            cur.clear();
        }
    }
    for c in expr.chars() {
        if c.is_alphanumeric() || c == '_' {
            cur.push(c);
        } else {
            flush(&mut cur, &mut out);
        }
    }
    flush(&mut cur, &mut out);
    // evalexpr's built-in function namespaces show up as bare identifiers here;
    // drop a small allowlist so they aren't mistaken for inputs.
    out.retain(|t| !matches!(t.as_str(), "math" | "min" | "max" | "abs" | "round" | "floor" | "ceil"));
    out
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

    #[test]
    fn parses_and_validates_lm75() {
        let spec = SensorSpec::from_toml(LM75).unwrap();
        assert_eq!(spec.sensor.name, "LM75");
        assert_eq!(spec.sensor.bus, Bus::I2c);
        assert_eq!(spec.sensor.i2c_address, Some(0x48));
        assert_eq!(spec.sensor.registers.len(), 2);
        let temp = &spec.sensor.registers[0];
        assert_eq!(temp.addr, 0x00);
        assert_eq!(temp.encoding, Some(Encoding::Q71Be));
        assert_eq!(temp.read_len(), 2);
    }

    #[test]
    fn round_trips_through_toml() {
        let spec = SensorSpec::from_toml(LM75).unwrap();
        let back = spec.to_toml().unwrap();
        let reparsed = SensorSpec::from_toml(&back).unwrap();
        assert_eq!(reparsed.sensor.name, spec.sensor.name);
        assert_eq!(reparsed.sensor.registers.len(), spec.sensor.registers.len());
        assert_eq!(
            reparsed.sensor.registers[0].encoding,
            spec.sensor.registers[0].encoding
        );
    }

    #[test]
    fn rejects_i2c_without_address() {
        let bad = r#"
[sensor]
name = "X"
bus = "i2c"
[[sensor.register]]
addr = 0
const = [1]
[sensor.protocol]
style = "i2c_pointer"
"#;
        assert!(SensorSpec::from_toml(bad).is_err());
    }

    #[test]
    fn rejects_unknown_input_in_expr() {
        let bad = r#"
[sensor]
name = "X"
bus = "i2c"
i2c_address = 0x10
[[sensor.register]]
addr = 0
bytes = 2
encoding = "i16_be"
expr = "undeclared_thing * 2"
[sensor.protocol]
style = "i2c_pointer"
"#;
        assert!(SensorSpec::from_toml(bad).is_err());
    }

    #[test]
    fn spi_spec_parses() {
        let spi = r#"
[sensor]
name = "MINIMAG"
bus = "spi"

[[sensor.input]]
name = "x"
default = 0.0

[[sensor.register]]
addr = 0x0f
const = [0x42]

[[sensor.register]]
addr = 0x10
bytes = 2
encoding = "i16_le"
expr = "x"

[sensor.protocol]
style = "spi_reg"
rw_read_is_high = true
addr_mask = 0x7f
"#;
        let spec = SensorSpec::from_toml(spi).unwrap();
        assert_eq!(spec.sensor.bus, Bus::Spi);
        assert_eq!(spec.sensor.protocol.style, ProtocolStyle::SpiReg);
        assert!(spec.sensor.protocol.rw_read_is_high);
        assert_eq!(spec.sensor.protocol.addr_mask, 0x7f);
    }
}
