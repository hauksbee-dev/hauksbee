//! Generic declarative register-map sensor interpreter.
//! Long-form how-and-why: docs/how-and-why/hauksbee-engine/peripherals.md and
//! docs/how-and-why/hauksbee-models/sensor_spec.md (the format side).
//!
//! [`RegisterMapSensor`] reads a [`SensorSpec`] (the declarative `[sensor]`
//! TOML defined in `hauksbee-models`) and *realizes* it as a live bus slave:
//! it implements [`I2cSlave`] (the `i2c_pointer` protocol) AND [`SpiSlave`]
//! (the `spi_reg` protocol), producing the same bytes a hand-coded model would.
//!
//! Crucially it does NOT wrap or delegate to `Lm75` / `Mcp3008`: every byte is
//! computed here from the spec; the register pointer selects a
//! [`RegisterSpec`], its `expr` is evaluated against the current input values
//! with `evalexpr`, and the resulting number is packed per the register's
//! [`Encoding`]. A declarative LM75 is therefore byte-for-byte identical to the
//! hand-coded `Lm75` because both emit the same datasheet packing, not because
//! one calls the other.
//!
//! It attaches through the existing bus peripherals unchanged: build an
//! [`super::i2c::I2cBus`] with the sensor as a slave (`add_slave` /
//! `with_slave`), or a [`super::spi::SpiBus`] (`SpiBus::new`). The `on_i2c` / `on_spi` Renode/simavr bridge is
//! untouched; this is just another slave.
//!
//! ## Write side
//!
//! The interpreter also EXECUTES firmware writes per the spec's write side:
//! pointer-framed register writes decode into stored variables the read
//! expressions see (write→read coupling: an ADS1115 config write selects what
//! the conversion register reads), and command-framed writes (the MCP4728
//! shape) update per-channel state whose output voltage laws drive analog nets
//! through the ctx-bearing `on_stop`. Bit-field extraction happens
//! here in Rust from the spec's declared `[high, low]` ranges, evalexpr has
//! no bit operations, so the bit surgery is framing-layer data, never
//! expression math (see the boundary note in `sensor_spec.rs`). Write bytes
//! the spec does not declare are accepted-and-ignored but COUNTED
//! (`ignored_write_bytes`), so an eaten config write is observable.

use std::collections::HashMap;

use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, HashMapContext, Node as EvalNode, Value,
};

use hauksbee_models::sensor_spec::{
    Bus, ChannelSource, Encoding, OutputSpec, ProtocolStyle, RegisterSpec, SensorSpec,
    WriteCommandSpec, WriteRegisterSpec,
};

use hauksbee_bind::drivers::PinDriver;

use super::i2c::I2cSlave;
use super::spi::SpiSlave;

/// A precompiled register: the spec plus its parsed expression (if any).
struct CompiledRegister {
    spec: RegisterSpec,
    program: Option<EvalNode>,
}

impl CompiledRegister {
    /// Produce this register's read bytes given the current input values.
    fn bytes(&self, inputs: &HashMap<String, f64>) -> Vec<u8> {
        // Const register: emit the constant bytes verbatim, padded/truncated to
        // the declared read length.
        if let Some(c) = &self.spec.r#const {
            let len = self.spec.read_len();
            let mut out = c.clone();
            out.resize(len, 0);
            return out;
        }

        // Encoded register: evaluate the expr, apply scale/offset, pack.
        // A None program here means the spec passed validation without an expr,
        // which should not happen for an encoded register, treat as 0.0.
        let enc = self.spec.encoding.unwrap_or(Encoding::U8);
        let program = self.program.as_ref().expect(
            "RegisterMapSensor: encoded register has no compiled expression; \
             this indicates a spec that bypassed from_toml validation. \
             Always construct via RegisterMapSensor::from_toml.",
        );
        let value = eval_number(program, inputs).unwrap_or_else(|| {
            panic!(
                "RegisterMapSensor: expr evaluation failed at runtime for register 0x{:02x} \
                 (expr: {:?}); inputs: {:?}. \
                 This is a bug: all declared inputs should be present.",
                self.spec.addr,
                self.spec.expr,
                inputs.keys().collect::<Vec<_>>()
            )
        });
        let scaled = value * self.spec.scale.unwrap_or(1.0) + self.spec.offset.unwrap_or(0.0);
        encode(enc, scaled)
    }
}

/// Build an evalexpr context from the current input values and evaluate.
fn eval_number(program: &EvalNode, inputs: &HashMap<String, f64>) -> Option<f64> {
    let mut ctx = HashMapContext::new();
    for (k, v) in inputs {
        let _ = ctx.set_value(k.clone(), Value::Float(*v));
    }
    match program.eval_with_context(&ctx) {
        Ok(Value::Float(f)) => Some(f),
        Ok(Value::Int(i)) => Some(i as f64),
        Ok(Value::Boolean(b)) => Some(if b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// Pack a numeric value into bytes per the encoding. The integer encodings
/// round to nearest and saturate to the type's range; `Q71Be` reproduces the
/// LM75 datasheet packing exactly (0.125 °C/LSB, 11-bit count left-justified by
/// 5 into a 16-bit big-endian word).
fn encode(enc: Encoding, value: f64) -> Vec<u8> {
    match enc {
        Encoding::U8 => {
            let v = value.round().clamp(0.0, u8::MAX as f64) as u8;
            vec![v]
        }
        Encoding::U16Be => {
            let v = value.round().clamp(0.0, u16::MAX as f64) as u16;
            v.to_be_bytes().to_vec()
        }
        Encoding::U16Le => {
            let v = value.round().clamp(0.0, u16::MAX as f64) as u16;
            v.to_le_bytes().to_vec()
        }
        Encoding::I16Be => {
            let v = value.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            v.to_be_bytes().to_vec()
        }
        Encoding::I16Le => {
            let v = value.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
            v.to_le_bytes().to_vec()
        }
        Encoding::Q71Be => {
            // Exactly the LM75/LM75A packing the hand-coded model uses:
            //   counts = round(T / 0.125)   (11-bit signed temperature count)
            //   raw    = (counts << 5) & 0xFFFF   (left-justified into 16 bits)
            //   bytes  = [raw >> 8, raw & 0xFF]   (big-endian, MSB first)
            let counts = (value / 0.125).round() as i32;
            let raw = ((counts << 5) & 0xFFFF) as u16;
            vec![(raw >> 8) as u8, (raw & 0xFF) as u8]
        }
        Encoding::U20BeXlsb => {
            // Bosch BME280/BMP280 20-bit press/temp packing (datasheet §5.4.6):
            //   count = round(value), saturated to the 20-bit unsigned range
            //   bytes = [ MSB = count[19:12], LSB = count[11:4], XLSB = count[3:0]<<4 ]
            // The XLSB low nibble is unused on the wire (reads back as 0). The
            // register's `expr` supplies the RAW ADC count; the raw→physical
            // Bosch compensation is applied by the firmware / test consumer.
            let count = value.round().clamp(0.0, 0xF_FFFF as f64) as u32;
            let msb = ((count >> 12) & 0xFF) as u8;
            let lsb = ((count >> 4) & 0xFF) as u8;
            let xlsb = ((count << 4) & 0xF0) as u8;
            vec![msb, lsb, xlsb]
        }
        Encoding::Raw => Vec::new(),
    }
}

/// Decode written payload bytes into `(value, raw_bits)` per the encoding:
/// `value` is the signed numeric value the `store` variable holds, `raw_bits`
/// is the unsigned wire bit pattern the [`BitFieldSpec`] ranges index into
/// (two's complement for the signed encodings, so a field over a sign bit
/// extracts what is actually on the wire).
///
/// Only the fixed-width integer encodings decode; `SensorSpec` validation
/// rejects the read-only packings (`q7.1_be`, `u20_be_xlsb`, `raw`) as write
/// encodings, so this panicking on them indicates an unvalidated spec.
fn decode(enc: Encoding, bytes: &[u8]) -> (f64, u32) {
    match enc {
        Encoding::U8 => (bytes[0] as f64, bytes[0] as u32),
        Encoding::U16Be => {
            let v = u16::from_be_bytes([bytes[0], bytes[1]]);
            (v as f64, v as u32)
        }
        Encoding::U16Le => {
            let v = u16::from_le_bytes([bytes[0], bytes[1]]);
            (v as f64, v as u32)
        }
        Encoding::I16Be => {
            let v = i16::from_be_bytes([bytes[0], bytes[1]]);
            (v as f64, v as u16 as u32)
        }
        Encoding::I16Le => {
            let v = i16::from_le_bytes([bytes[0], bytes[1]]);
            (v as f64, v as u16 as u32)
        }
        other => panic!(
            "RegisterMapSensor: encoding {other:?} has no write decode; \
             SensorSpec::validate rejects it; this spec bypassed validation"
        ),
    }
}

/// Extract a `[high, low]`-inclusive bit range from a decoded integer (the
/// same semantics as `BitFieldSpec::extract`, for the channel-select field
/// which has no name).
fn extract_bits(bits: [u8; 2], value: u32) -> u32 {
    let [high, low] = bits;
    let width = high - low + 1;
    let mask = if width >= 32 {
        u32::MAX
    } else {
        (1u32 << width) - 1
    };
    (value >> low) & mask
}

/// A compiled write command: the spec plus its parsed update expressions.
struct CompiledCommand {
    spec: WriteCommandSpec,
    /// state name -> parsed update expression, iterated in BTreeMap order (the
    /// order cannot matter: all RHS evaluate against the pre-update snapshot).
    programs: Vec<(String, EvalNode)>,
}

/// A compiled output law, plus the net driver the engine attached (None until
/// [`RegisterMapSensor::attach_output_driver_for_channel`] binds one, an
/// unbound output still evaluates for `state()` / assertions, it just drives
/// nothing).
struct CompiledOutput {
    spec: OutputSpec,
    program: EvalNode,
    driver: Option<PinDriver>,
}

/// Which write-command state the current I2C write transaction is in.
enum CmdPhase {
    /// No byte since START: the next write byte selects the command.
    Awaiting,
    /// Matched `write_commands[idx]`; groups decode greedily.
    Active(usize),
    /// First byte matched no declared command: the transaction is accepted and
    /// ignored (counted in `ignored_write_bytes`).
    Ignored,
}

/// The generic interpreter. Owns the spec, a register lookup, the live input
/// values, and the small per-transaction state machine for whichever bus the
/// spec declares.
pub struct RegisterMapSensor {
    name: String,
    bus: Bus,
    i2c_address: u8,
    /// SPI framing.
    rw_read_is_high: bool,
    addr_mask: u8,
    spi_reg_protocol: bool,
    /// Declared SPI clock mode (0..=3); surfaced via `SpiSlave::spi_mode` so the
    /// bit-banged responder times its edges to the datasheet mode.
    spi_mode: u8,

    /// addr -> compiled register.
    registers: HashMap<u8, CompiledRegister>,
    /// Stable ascending register addresses (for I2C auto-increment).
    addr_order: Vec<u8>,
    /// Live input values, seeded from each input's `default`.
    inputs: HashMap<String, f64>,

    // ── Write side ──
    /// addr -> pointer-framed writable register.
    write_regs: HashMap<u8, WriteRegisterSpec>,
    /// Stored write variables (each write_register's `store` and its extracted
    /// fields), seeded from the register defaults. Shares the expression
    /// namespace with `inputs`.
    stores: HashMap<String, f64>,
    /// Command-framed write protocol, matched first-in-spec-order.
    write_cmds: Vec<CompiledCommand>,
    /// Per-channel state: name -> one value per channel.
    chan_state: HashMap<String, Vec<f64>>,
    channels: usize,
    /// Driven-net output laws.
    outputs: Vec<CompiledOutput>,
    /// Streamed read frame (per-channel byte expressions), replacing the
    /// pointered read path when present.
    read_frame: Option<(bool, Vec<EvalNode>)>,
    /// Payload bytes accumulated for the currently pointed write register.
    write_buf: Vec<u8>,
    /// Command-framing transaction state.
    cmd_phase: CmdPhase,
    /// Bytes accumulated toward the active command's next group.
    cmd_acc: Vec<u8>,
    /// Channel cursor for auto/prefix-seeded channel selection.
    chan_cursor: usize,
    /// Write bytes this model accepted but did not decode (undeclared command
    /// families, payload past a register's width, payload for a non-writable
    /// register). Surfaced in `state()` so "the model ate my config write"
    /// is observable instead of silent.
    ignored_write_bytes: u64,

    // ── I2C transaction state (i2c_pointer) ──
    pointer: u8,
    got_pointer: bool,
    /// Cached bytes of the register currently being read, and the cursor.
    read_buf: Vec<u8>,
    read_pos: usize,

    // ── SPI transaction state (spi_reg) ──
    spi_first_byte: bool,
    spi_is_read: bool,
    spi_addr: u8,
    spi_pos: usize,
}

impl RegisterMapSensor {
    /// Parse + validate a `[sensor]` spec and build the interpreter.
    ///
    /// Returns `Err` if the TOML is malformed, the spec fails structural
    /// validation, OR any register's `expr` string cannot be compiled by
    /// `evalexpr`. An uncompilable expression is a hard error: a spec that
    /// silently falls back to `0.0` bytes looks like a working zero-valued sensor
    /// and would silently corrupt firmware-visible bus traffic.
    pub fn from_toml(src: &str) -> Result<Self, hauksbee_models::sensor_spec::SensorSpecError> {
        let spec = SensorSpec::from_toml(src)?;
        // Validate that every expression actually compiles. This is an
        // engine-side check (evalexpr lives here, not in hauksbee-models) that
        // complements the token-level identifier check in SensorSpec::validate().
        let compile_check = |what: String,
                             expr: &str|
         -> Result<(), hauksbee_models::sensor_spec::SensorSpecError> {
            build_operator_tree(expr).map_err(|e| {
                hauksbee_models::sensor_spec::SensorSpecError::Invalid(format!(
                    "{what} expr {expr:?} failed to compile: {e}"
                ))
            })?;
            Ok(())
        };
        for r in &spec.sensor.registers {
            if let Some(expr) = r.expr.as_deref() {
                compile_check(format!("register 0x{:02x}", r.addr), expr)?;
            }
        }
        for c in &spec.sensor.write_commands {
            for (target, expr) in &c.update {
                compile_check(
                    format!("write_command '{}' update '{target}'", c.name),
                    expr,
                )?;
            }
        }
        for o in &spec.sensor.outputs {
            compile_check(format!("output '{}'", o.name), &o.expr)?;
        }
        if let Some(f) = &spec.sensor.read_frame {
            for (i, b) in f.bytes.iter().enumerate() {
                compile_check(format!("read_frame byte {i}"), b)?;
            }
        }
        Ok(Self::from_spec(spec))
    }

    /// Build from an already-parsed (and validated) spec.
    ///
    /// # Panics
    ///
    /// This constructor is public but does NOT re-validate the spec. If two
    /// registers collide to the same key under
    /// [`hauksbee_models::Sensor::register_key`] (e.g. an
    /// SPI spec declaring both `0x50` and `0xD0`, which both mask to `0x50`), one
    /// would silently overwrite the other in the register map. Rather than
    /// produce a sensor that quietly drops a register, this panics with a message
    /// pointing the caller at `from_toml`/`validate`. Construct via
    /// [`RegisterMapSensor::from_toml`] (which validates) to avoid this.
    /// Crate-private on purpose: every panic below documents "this spec
    /// bypassed validation", and the only door that guarantees validation is
    /// [`Self::from_toml`]. Public callers get the fallible, validating
    /// constructor; a malformed community model pack must be an error, not a
    /// process abort.
    pub(crate) fn from_spec(spec: SensorSpec) -> Self {
        let mut s = spec.sensor;

        let spi_reg_protocol = s.protocol.style == ProtocolStyle::SpiReg;
        // SPI folds the R/W bit into bit 7 of the command byte, so the incoming
        // address the firmware sends is `cmd & addr_mask` (see `transfer`). To
        // let a spec author write the *raw datasheet* register address (e.g. the
        // BMP280 chip-ID at 0xD0) AND keep pre-masked specs (0x50) working, we
        // normalize the stored register key. `Sensor::register_key` is the single
        // source of truth for that mapping (post-mask for SPI, raw for I2C); both
        // this and `SensorSpec::validate`'s dedup use it. The live-transaction
        // analog is `normalize_addr` below, which must stay consistent.
        let mut registers = HashMap::new();
        let mut addr_order = Vec::new();
        // Take the register list out so the loop can still call `s.register_key`
        // without a partial-move borrow conflict.
        let register_specs = std::mem::take(&mut s.registers);
        for r in register_specs {
            let program = r.expr.as_deref().and_then(|e| build_operator_tree(e).ok());
            let key = s.register_key(r.addr);
            if registers.contains_key(&key) {
                panic!(
                    "RegisterMapSensor::from_spec: register addr 0x{:02x} collides with an \
                     existing register at key 0x{:02x} (post-mask). Inserting it would silently \
                     overwrite the other register. This indicates an unvalidated spec that \
                     bypassed dedup checking. Always construct via RegisterMapSensor::from_toml \
                     (which runs SensorSpec::validate).",
                    r.addr, key,
                );
            }
            addr_order.push(key);
            registers.insert(key, CompiledRegister { spec: r, program });
        }
        addr_order.sort_unstable();

        let mut inputs = HashMap::new();
        for i in &s.inputs {
            inputs.insert(i.name.clone(), i.default);
        }

        // ── Write side ──
        // Stores seed from each write register's POR default; the fields seed
        // by extracting from that default's bit pattern, so a read expr that
        // references a field (the ADS1115 conversion law) is well-defined
        // before any firmware write happens.
        let mut write_regs = HashMap::new();
        let mut stores = HashMap::new();
        for w in std::mem::take(&mut s.write_registers) {
            let raw = (w.default.round() as i64 & ((1i64 << (w.encoding.natural_width() * 8)) - 1))
                as u32;
            stores.insert(w.store.clone(), w.default);
            for f in &w.fields {
                stores.insert(f.name.clone(), f.extract(raw) as f64);
            }
            write_regs.insert(w.addr, w);
        }
        let write_cmds = std::mem::take(&mut s.write_commands)
            .into_iter()
            .map(|c| {
                let programs = c
                    .update
                    .iter()
                    .map(|(k, expr)| {
                        let program = build_operator_tree(expr).unwrap_or_else(|e| {
                            panic!(
                                "RegisterMapSensor::from_spec: write_command '{}' update \
                                     '{k}' failed to compile ({e}); construct via from_toml",
                                c.name
                            )
                        });
                        (k.clone(), program)
                    })
                    .collect();
                CompiledCommand { spec: c, programs }
            })
            .collect();
        let channels = s.channels;
        let mut chan_state = HashMap::new();
        for st in &s.states {
            chan_state.insert(st.name.clone(), vec![st.default; channels]);
        }
        let outputs = std::mem::take(&mut s.outputs)
            .into_iter()
            .map(|o| {
                let program = build_operator_tree(&o.expr).unwrap_or_else(|e| {
                    panic!(
                        "RegisterMapSensor::from_spec: output '{}' failed to compile \
                             ({e}); construct via from_toml",
                        o.name
                    )
                });
                CompiledOutput {
                    spec: o,
                    program,
                    driver: None,
                }
            })
            .collect();
        let read_frame = s.read_frame.take().map(|f| {
            let programs = f
                .bytes
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    build_operator_tree(b).unwrap_or_else(|e| {
                        panic!(
                            "RegisterMapSensor::from_spec: read_frame byte {i} failed to \
                             compile ({e}); construct via from_toml"
                        )
                    })
                })
                .collect();
            (f.per_channel, programs)
        });

        RegisterMapSensor {
            name: s.name,
            bus: s.bus,
            i2c_address: s.i2c_address.unwrap_or(0),
            rw_read_is_high: s.protocol.rw_read_is_high,
            addr_mask: s.protocol.addr_mask,
            spi_reg_protocol,
            spi_mode: s.protocol.spi_mode,
            registers,
            addr_order,
            inputs,
            write_regs,
            stores,
            write_cmds,
            chan_state,
            channels,
            outputs,
            read_frame,
            write_buf: Vec::new(),
            cmd_phase: CmdPhase::Awaiting,
            cmd_acc: Vec::new(),
            chan_cursor: 0,
            ignored_write_bytes: 0,
            pointer: 0,
            got_pointer: false,
            read_buf: Vec::new(),
            read_pos: 0,
            spi_first_byte: true,
            spi_is_read: false,
            spi_addr: 0,
            spi_pos: 0,
        }
    }

    /// Sensor name (from the spec).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Which bus the spec declares.
    pub fn bus(&self) -> Bus {
        self.bus
    }

    /// Set a settable input (e.g. `temperature_c`) so a test/engine can sweep
    /// it live. Unknown names are ignored.
    pub fn set_input(&mut self, name: &str, value: f64) {
        if let Some(slot) = self.inputs.get_mut(name) {
            *slot = value;
        }
    }

    /// Read an input's current value (for assertions).
    pub fn input(&self, name: &str) -> Option<f64> {
        self.inputs.get(name).copied()
    }

    /// The modeled temperature in °C, if this sensor declares an LM75/TMP-style
    /// temperature register (`q7.1_be` encoding). This is the value QEMU pushes
    /// into its emulated `tmp105` device on the co-sim path; the same physical
    /// temperature the firmware would decode from the register bytes, recovered
    /// from the register's `expr` (with its `scale`/`offset`) rather than by
    /// unpacking the count. Returns `None` when the sensor models no such
    /// register, so a non-temperature RegisterMapSensor stays inert on that path.
    ///
    /// When several `q7.1_be` registers exist, the lowest address wins (the
    /// canonical LM75 temperature register is 0x00), scanned in stable order.
    fn modeled_temperature_c(&self) -> Option<f64> {
        let addr = self.addr_order.iter().copied().find(|a| {
            self.registers
                .get(a)
                .and_then(|r| r.spec.encoding)
                .map(|e| e == Encoding::Q71Be)
                .unwrap_or(false)
        })?;
        let reg = self.registers.get(&addr)?;
        let program = reg.program.as_ref()?;
        let value = eval_number(program, &self.inputs)?;
        Some(value * reg.spec.scale.unwrap_or(1.0) + reg.spec.offset.unwrap_or(0.0))
    }

    // ── Write side ─────────────────────────────────────────────────

    /// Override the spec's I2C address for this instance (a board carries
    /// several MCP4728s at 0x60/0x61/0x62 from one spec).
    pub fn set_i2c_address(&mut self, addr: u8) {
        self.i2c_address = addr;
    }

    /// A stored write variable (a write register's `store` or one of its
    /// extracted fields), for assertions.
    pub fn store(&self, name: &str) -> Option<f64> {
        self.stores.get(name).copied()
    }

    /// One channel's current value of a per-channel state variable.
    pub fn channel_state(&self, name: &str, channel: usize) -> Option<f64> {
        self.chan_state
            .get(name)
            .and_then(|v| v.get(channel))
            .copied()
    }

    /// Set a per-channel state value directly. Used by the engine to apply
    /// per-instance board configuration (an MCP4728's binder-resolved VREF and
    /// gain) over the spec defaults, and by tests. Unknown names / channels are
    /// ignored.
    pub fn set_channel_state(&mut self, name: &str, channel: usize, value: f64) {
        if let Some(v) = self.chan_state.get_mut(name) {
            if let Some(slot) = v.get_mut(channel) {
                *slot = value;
            }
        }
    }

    /// Bind a net driver to the (first) output declared for `channel`. Returns
    /// false when no output declares that channel. The driver is pushed at
    /// every ctx-bearing `on_stop` with the output law's current voltage.
    pub fn attach_output_driver_for_channel(&mut self, channel: usize, driver: PinDriver) -> bool {
        for o in &mut self.outputs {
            if o.spec.channel == channel {
                o.driver = Some(driver);
                return true;
            }
        }
        false
    }

    /// Evaluate an output law's current voltage by name (whether or not a
    /// driver is bound).
    pub fn output_volts(&self, name: &str) -> Option<f64> {
        self.outputs
            .iter()
            .find(|o| o.spec.name == name)
            .map(|o| self.eval_output(o))
    }

    /// Write bytes accepted but not decoded (undeclared command families,
    /// payload past a register's width, payload for a non-writable register).
    pub fn ignored_write_bytes(&self) -> u64 {
        self.ignored_write_bytes
    }

    /// Merged expression variables: inputs + stores + the builtin
    /// `i2c_address`, plus one channel's state when a channel context applies.
    fn vars(&self, channel: Option<usize>) -> HashMap<String, f64> {
        let mut m = self.inputs.clone();
        for (k, v) in &self.stores {
            m.insert(k.clone(), *v);
        }
        m.insert("i2c_address".into(), self.i2c_address as f64);
        if let Some(ch) = channel {
            for (k, v) in &self.chan_state {
                if let Some(val) = v.get(ch) {
                    m.insert(k.clone(), *val);
                }
            }
        }
        m
    }

    fn eval_output(&self, o: &CompiledOutput) -> f64 {
        let vars = self.vars(Some(o.spec.channel));
        eval_number(&o.program, &vars).unwrap_or_else(|| {
            panic!(
                "RegisterMapSensor: output '{}' law failed to evaluate; \
                 all referenced variables should be declared (spec bug)",
                o.spec.name
            )
        })
    }

    /// Commit a completed pointer-framed register write: decode the payload,
    /// store the value, extract the declared bit fields.
    fn commit_write_register(&mut self, addr: u8) {
        let Some(w) = self.write_regs.get(&addr) else {
            return;
        };
        let (value, raw) = decode(w.encoding, &self.write_buf);
        let mut updates = vec![(w.store.clone(), value)];
        for f in &w.fields {
            updates.push((f.name.clone(), f.extract(raw) as f64));
        }
        for (k, v) in updates {
            self.stores.insert(k, v);
        }
    }

    /// Drain completed command groups from `cmd_acc`, applying each group's
    /// state updates. Greedy, so state lands mid-transaction exactly like the
    /// real part with its latch held active (the MCP4728's board-held-low
    /// LDAC).
    fn drain_command_groups(&mut self, cmd_idx: usize) {
        loop {
            let (group_bytes, spec_channel, fields): (usize, _, Vec<_>) = {
                let c = &self.write_cmds[cmd_idx].spec;
                (c.group_bytes, c.channel.clone(), c.fields.clone())
            };
            if self.cmd_acc.len() < group_bytes {
                return;
            }
            // Group value: big-endian fold of the wire bytes.
            let mut value: u32 = 0;
            for b in self.cmd_acc.drain(..group_bytes) {
                value = (value << 8) | b as u32;
            }
            // Channel for this group.
            let channel = match spec_channel.source {
                ChannelSource::GroupBits => {
                    let bits = spec_channel.bits.expect("validated");
                    (extract_bits(bits, value) as usize) % self.channels
                }
                // Auto / prefix-seeded: the cursor, advanced after the group.
                _ => self.chan_cursor % self.channels,
            };
            // Evaluate ALL updates against the pre-update snapshot, then
            // commit together (spec contract: update order cannot matter).
            let mut vars = self.vars(Some(channel));
            for f in &fields {
                vars.insert(f.name.clone(), f.extract(value) as f64);
            }
            let mut committed: Vec<(String, f64)> = Vec::new();
            for (target, program) in &self.write_cmds[cmd_idx].programs {
                let v = eval_number(program, &vars).unwrap_or_else(|| {
                    panic!(
                        "RegisterMapSensor: write_command '{}' update '{}' failed to \
                         evaluate (spec bug)",
                        self.write_cmds[cmd_idx].spec.name, target
                    )
                });
                committed.push((target.clone(), v));
            }
            for (target, v) in committed {
                if let Some(slot) = self
                    .chan_state
                    .get_mut(&target)
                    .and_then(|v| v.get_mut(channel))
                {
                    *slot = v;
                }
            }
            if !matches!(spec_channel.source, ChannelSource::GroupBits) {
                self.chan_cursor = (self.chan_cursor + 1) % self.channels;
            }
        }
    }

    /// Build the streamed read frame (per-channel byte expressions), rounded
    /// and clamped to u8 like every other encoding boundary.
    fn build_read_frame(&self) -> Vec<u8> {
        let Some((per_channel, programs)) = &self.read_frame else {
            return Vec::new();
        };
        let channel_range = if *per_channel { 0..self.channels } else { 0..1 };
        let mut out = Vec::with_capacity(programs.len() * channel_range.len());
        for ch in channel_range {
            let vars = self.vars(Some(ch));
            for (i, p) in programs.iter().enumerate() {
                let v = eval_number(p, &vars).unwrap_or_else(|| {
                    panic!("RegisterMapSensor: read_frame byte {i} failed to evaluate (spec bug)")
                });
                out.push(v.round().clamp(0.0, 255.0) as u8);
            }
        }
        out
    }

    /// Map a raw, externally-supplied address to the key registers are stored
    /// under. For SPI (`spi_reg`) the registers map is keyed post-mask
    /// (`addr & addr_mask`) so a raw datasheet address (e.g. 0xD0) resolves to
    /// the same slot the masked command byte (0x50) hits; for I2C the full 8-bit
    /// pointer is the key, so the address passes through unchanged. This is the
    /// live-transaction analog of `Sensor::register_key` (the spec-side source of
    /// truth used by `from_spec`/`validate`) and must stay consistent with it.
    ///
    /// Masking is idempotent, so applying this to an already-masked address (as
    /// the SPI `transfer` path does before calling `register_bytes`) is a no-op:
    /// `(mosi & m) & m == mosi & m`.
    fn normalize_addr(&self, addr: u8) -> u8 {
        if self.spi_reg_protocol {
            addr & self.addr_mask
        } else {
            addr
        }
    }

    /// The bytes a read of `addr` currently produces (the spec-driven encoding).
    /// Returns `[0xFF]` for any address not declared in the spec; this matches
    /// typical I2C/SPI open-drain bus idle behaviour and makes undeclared reads
    /// visible rather than silently returning another register's data.
    ///
    /// `addr` is the RAW, externally-supplied address: it is normalized via
    /// `normalize_addr` before lookup so callers may pass the raw
    /// datasheet register address (e.g. 0xD0) for an SPI sensor and still hit the
    /// post-mask key (0x50) the register is stored under.
    /// Public so tests can compare against a hand-coded model directly.
    pub fn register_bytes(&self, addr: u8) -> Vec<u8> {
        let key = self.normalize_addr(addr);
        // Reads see the merged variables: physical inputs PLUS the stored
        // write variables, so a written config register feeds the read-side
        // expressions (write→read coupling).
        self.registers
            .get(&key)
            .map(|r| r.bytes(&self.vars(None)))
            .unwrap_or_else(|| vec![0xFF])
    }

    /// (Re)load the read buffer for the current I2C pointer.
    fn refill_i2c_read(&mut self) {
        self.read_buf = self.register_bytes(self.pointer);
        self.read_pos = 0;
    }

    /// Advance the I2C pointer to the next register address (auto-increment for
    /// sequential reads), wrapping at the end of the map.
    ///
    /// If the current pointer is not a declared register address, auto-increment
    /// is not performed: the pointer stays at its current (unknown) value so that
    /// continued reads return `0xFF` rather than silently jumping into a real
    /// register's data.
    fn advance_pointer(&mut self) {
        if self.addr_order.is_empty() {
            return;
        }
        let Some(idx) = self.addr_order.iter().position(|&a| a == self.pointer) else {
            // Unknown pointer: do not advance into a declared register.
            return;
        };
        let next = (idx + 1) % self.addr_order.len();
        self.pointer = self.addr_order[next];
    }
}

// ── I2C: i2c_pointer protocol ──────────────────────────────────────────────

impl I2cSlave for RegisterMapSensor {
    fn address(&self) -> u8 {
        self.i2c_address
    }

    fn temperature_mc(&self) -> Option<i32> {
        self.modeled_temperature_c()
            .map(|c| (c * 1000.0).round() as i32)
    }

    fn on_start(&mut self, read: bool) {
        if read {
            // Repeated START for the read phase: a frame-streaming device
            // (MCP4728) loads its full frame; a pointered device loads the
            // addressed register.
            if self.read_frame.is_some() {
                self.read_buf = self.build_read_frame();
                self.read_pos = 0;
            } else {
                self.refill_i2c_read();
            }
        } else {
            self.got_pointer = false;
            self.write_buf.clear();
            self.cmd_phase = CmdPhase::Awaiting;
            self.cmd_acc.clear();
            self.chan_cursor = 0;
        }
    }

    fn on_write(&mut self, data: u8) {
        // Command-framed device: the first byte selects the command,
        // the rest decode as its groups. There is no register pointer.
        if !self.write_cmds.is_empty() {
            match self.cmd_phase {
                CmdPhase::Awaiting => {
                    let matched = self
                        .write_cmds
                        .iter()
                        .position(|c| data & c.spec.match_mask == c.spec.match_value);
                    match matched {
                        Some(idx) => {
                            let c = &self.write_cmds[idx].spec;
                            if c.prefix {
                                // Prefix byte: consumed here, possibly seeding
                                // the channel cursor; not part of any group.
                                if c.channel.source == ChannelSource::PrefixBits {
                                    let bits = c.channel.bits.expect("validated");
                                    self.chan_cursor =
                                        (extract_bits(bits, data as u32) as usize) % self.channels;
                                }
                            } else {
                                self.cmd_acc.push(data);
                            }
                            self.cmd_phase = CmdPhase::Active(idx);
                            // A group that completes on this very byte
                            // (group_bytes == 1) must drain NOW: the Active arm
                            // only runs on the NEXT write, and a repeated START
                            // or STOP clears cmd_acc, so a single-byte command
                            // was silently dropped with no state change. drain
                            // is a no-op until a full group is buffered, so
                            // prefix bytes and multi-byte commands are untouched.
                            self.drain_command_groups(idx);
                        }
                        None => {
                            // A command family the spec does not declare:
                            // accept-and-ignore (a real part ACKs it), counted
                            // so the omission is observable.
                            self.cmd_phase = CmdPhase::Ignored;
                            self.ignored_write_bytes += 1;
                        }
                    }
                }
                CmdPhase::Active(idx) => {
                    self.cmd_acc.push(data);
                    self.drain_command_groups(idx);
                }
                CmdPhase::Ignored => {
                    self.ignored_write_bytes += 1;
                }
            }
            return;
        }

        // Pointer-framed device.
        if !self.got_pointer {
            // First write byte selects the register pointer.
            self.pointer = data;
            self.got_pointer = true;
            self.write_buf.clear();
            self.refill_i2c_read();
            return;
        }
        // Subsequent writes are register payload. A declared write register
        // decodes and commits as soon as its full width has arrived (further
        // bytes are ignored-and-counted: none of the modeled parts
        // auto-increment writes). Payload for an undeclared register is
        // accepted and ignored, so the firmware's config writes never stall,
        // and it is counted rather than silently dropped.
        let key = self.normalize_addr(self.pointer);
        if let Some(w) = self.write_regs.get(&key) {
            let width = w.encoding.natural_width();
            if self.write_buf.len() < width {
                self.write_buf.push(data);
                if self.write_buf.len() == width {
                    self.commit_write_register(key);
                }
            } else {
                self.ignored_write_bytes += 1;
            }
        } else {
            self.ignored_write_bytes += 1;
        }
    }

    fn on_read(&mut self) -> u8 {
        if self.read_frame.is_some() {
            // Streamed frame: wrap at the end, like the datasheet read frames
            // (a master that keeps clocking sees the frame again).
            if self.read_buf.is_empty() {
                self.read_buf = self.build_read_frame();
            }
            let b = self.read_buf.get(self.read_pos).copied().unwrap_or(0xFF);
            self.read_pos = (self.read_pos + 1) % self.read_buf.len().max(1);
            return b;
        }
        if self.read_pos >= self.read_buf.len() {
            // Past the end of the current register: auto-increment across the
            // map (sequential read) and continue.
            self.advance_pointer();
            self.refill_i2c_read();
        }
        let b = self.read_buf.get(self.read_pos).copied().unwrap_or(0xFF);
        self.read_pos += 1;
        b
    }

    fn on_stop(&mut self, ctx: &mut super::TickCtx) {
        self.got_pointer = false;
        // Drive every bound output net from its law's current value.
        // Evaluate first (immutable pass), then push (drivers mutate ctx).
        let volts: Vec<(usize, f64)> = self
            .outputs
            .iter()
            .enumerate()
            .filter(|(_, o)| o.driver.is_some())
            .map(|(i, o)| (i, self.eval_output(o)))
            .collect();
        for (i, v) in volts {
            if let Some(drv) = &self.outputs[i].driver {
                drv.set_volts(ctx.circuit, v);
            }
        }
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("pointer".into(), self.pointer as f64);
        for (k, v) in &self.inputs {
            m.insert(k.clone(), *v);
        }
        for (k, v) in &self.stores {
            m.insert(k.clone(), *v);
        }
        for (k, per_ch) in &self.chan_state {
            for (ch, v) in per_ch.iter().enumerate() {
                m.insert(format!("{k}_{ch}"), *v);
            }
        }
        for o in &self.outputs {
            m.insert(o.spec.name.clone(), self.eval_output(o));
        }
        if self.ignored_write_bytes > 0 {
            m.insert(
                "ignored_write_bytes".into(),
                self.ignored_write_bytes as f64,
            );
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

// ── SPI: spi_reg protocol ──────────────────────────────────────────────────

impl SpiSlave for RegisterMapSensor {
    fn spi_mode(&self) -> u8 {
        self.spi_mode
    }

    fn transfer(&mut self, mosi: u8) -> u8 {
        if self.spi_first_byte {
            // Command byte: high bit = R/W per `rw_read_is_high`, low bits (per
            // `addr_mask`) = register address.
            let read_bit = (mosi & 0x80) != 0;
            self.spi_is_read = if self.rw_read_is_high {
                read_bit
            } else {
                !read_bit
            };
            self.spi_addr = mosi & self.addr_mask;
            self.spi_first_byte = false;
            self.spi_pos = 0;
            if self.spi_is_read {
                self.read_buf = self.register_bytes(self.spi_addr);
            }
            // The command byte itself returns a don't-care (status) byte.
            return 0x00;
        }

        if self.spi_is_read {
            // Stream the addressed register's bytes, auto-incrementing across
            // the map for multi-register burst reads.
            if self.spi_pos >= self.read_buf.len() {
                // Advance to next register address in order.
                if let Some(idx) = self.addr_order.iter().position(|&a| a == self.spi_addr) {
                    let next = (idx + 1) % self.addr_order.len().max(1);
                    self.spi_addr = self.addr_order[next];
                }
                self.read_buf = self.register_bytes(self.spi_addr);
                self.spi_pos = 0;
            }
            let b = self.read_buf.get(self.spi_pos).copied().unwrap_or(0xFF);
            self.spi_pos += 1;
            b
        } else {
            // SPI write phase: accept-and-ignore, COUNTED. The declarative
            // write side is modeled for I2C only (SensorSpec::validate rejects
            // SPI write blocks); no current device needs SPI register writes,
            // and an untested decode path would be fake coverage. A stated
            // limitation, observable via `ignored_write_bytes`.
            self.ignored_write_bytes += 1;
            self.spi_pos += 1;
            0x00
        }
    }

    fn deselect(&mut self) {
        // Chunk boundary / CS deassert: reset the SPI transaction.
        if self.spi_reg_protocol {
            self.spi_first_byte = true;
            self.spi_is_read = false;
            self.spi_pos = 0;
        }
    }

    /// Exact, non-advancing mirror of [`SpiSlave::transfer`]'s return value.
    /// Every branch of `transfer` computes its MISO byte from state set by
    /// PRIOR bytes (command exchange and write phase return the fixed 0x00
    /// status don't-care; read bytes stream `read_buf` / the auto-increment
    /// successor), never from the incoming MOSI byte, which is why this
    /// sensor is fully previewable and hence readable over bit-banged SPI.
    /// A unit test locks the preview to the transfer stream byte-for-byte.
    fn miso_preview(&mut self) -> Option<u8> {
        if self.spi_first_byte || !self.spi_is_read {
            return Some(0x00);
        }
        if self.spi_pos >= self.read_buf.len() {
            // The next transfer would auto-increment to the next register in
            // order (or re-refill the same address when it is not in the
            // order, matching `transfer`); preview that register's first byte
            // without committing the advance.
            let addr = self
                .addr_order
                .iter()
                .position(|&a| a == self.spi_addr)
                .map(|idx| self.addr_order[(idx + 1) % self.addr_order.len().max(1)])
                .unwrap_or(self.spi_addr);
            return Some(self.register_bytes(addr).first().copied().unwrap_or(0xFF));
        }
        Some(self.read_buf[self.spi_pos])
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("spi_addr".into(), self.spi_addr as f64);
        for (k, v) in &self.inputs {
            m.insert(k.clone(), *v);
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
    use crate::peripherals::i2c::{I2cBus, Lm75};
    use crate::peripherals::spi::SpiBus;
    use hauksbee_mcu::I2cEvent;

    const LM75_SPEC: &str = r#"
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

    const SPI_IMU_SPEC: &str = r#"
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

    /// A BMP280-like SPI spec with its chip-ID at the RAW datasheet address
    /// (0xD0), or the pre-masked 0x50; both must read via command byte 0xD0.
    fn bmp280_spi_spec(addr: u8) -> String {
        format!(
            r#"
[sensor]
name = "BMP280"
bus = "spi"

[[sensor.register]]
addr = {addr:#x}
const = [0x58]

[sensor.protocol]
style = "spi_reg"
rw_read_is_high = true
addr_mask = 0x7f
"#
        )
    }

    const COLLIDING_SPI_SPEC: &str = r#"
[sensor]
name = "BMP280"
bus = "spi"

[[sensor.register]]
addr = 0x50
const = [0x58]

[[sensor.register]]
addr = 0xD0
const = [0x59]

[sensor.protocol]
style = "spi_reg"
rw_read_is_high = true
addr_mask = 0x7f
"#;

    fn sensor(spec: &str) -> RegisterMapSensor {
        RegisterMapSensor::from_toml(spec).unwrap()
    }

    fn i2c_bus(sensor: RegisterMapSensor) -> I2cBus {
        I2cBus::new("I2C").with_slave(Box::new(sensor))
    }

    /// One pointer-framed write transaction: START, `bytes`, STOP.
    fn i2c_write(bus: &mut I2cBus, addr: u8, bytes: &[u8]) {
        bus.dispatch(I2cEvent::Start { addr, read: false });
        for &data in bytes {
            bus.dispatch(I2cEvent::Write { addr, data });
        }
        bus.dispatch(I2cEvent::Stop { addr });
    }

    /// Pointered burst read of `n` bytes starting at register `reg`.
    fn i2c_read_burst(bus: &mut I2cBus, addr: u8, reg: u8, n: usize) -> Vec<u8> {
        bus.dispatch(I2cEvent::Start { addr, read: false });
        bus.dispatch(I2cEvent::Write { addr, data: reg });
        bus.dispatch(I2cEvent::Start { addr, read: true });
        let out = (0..n)
            .map(|_| bus.dispatch(I2cEvent::Read { addr }).unwrap())
            .collect();
        bus.dispatch(I2cEvent::Stop { addr });
        out
    }

    fn i2c_write_u16(bus: &mut I2cBus, addr: u8, reg: u8, value: u16) {
        i2c_write(bus, addr, &[reg, (value >> 8) as u8, (value & 0xFF) as u8]);
    }

    fn i2c_read_u16(bus: &mut I2cBus, addr: u8, reg: u8) -> u16 {
        let d = i2c_read_burst(bus, addr, reg, 2);
        u16::from_be_bytes([d[0], d[1]])
    }

    fn spi_read(bus: &mut SpiBus, command: u8, n: usize) -> Vec<u8> {
        let _status = bus.transfer(command);
        (0..n).map(|_| bus.transfer(0x00)).collect()
    }

    /// A declarative LM75 is byte-identical to the hand-coded `Lm75` across a
    /// temperature sweep and reports the same `temperature_mc`; a spec with no
    /// temperature register reports none.
    #[test]
    fn declarative_lm75_matches_handcoded_bytes_and_temperature_mc() {
        for &t in &[
            -40.0, -10.0, -0.5, 0.0, 0.125, 12.5, 25.0, 36.6, 100.0, 124.875,
        ] {
            let mut hand = I2cBus::new("I2C").with_slave(Box::new(Lm75::new(0x48, t)));
            let mut decl = sensor(LM75_SPEC);
            decl.set_input("temperature_c", t);
            assert_eq!(
                I2cSlave::temperature_mc(&decl),
                Lm75::new(0x48, t).temperature_mc()
            );
            assert_eq!(
                I2cSlave::temperature_mc(&decl),
                Some((t * 1000.0).round() as i32)
            );
            let mut decl = i2c_bus(decl);
            assert_eq!(
                i2c_read_burst(&mut decl, 0x48, 0x00, 2),
                i2c_read_burst(&mut hand, 0x48, 0x00, 2),
                "at {t} °C"
            );
        }
        let whoami_only = sensor(
            "[sensor]\nname = \"IDPART\"\nbus = \"i2c\"\ni2c_address = 0x10\n\
             [[sensor.register]]\naddr = 0x00\nconst = [0xAB]\n\
             [sensor.protocol]\nstyle = \"i2c_pointer\"\n",
        );
        assert_eq!(I2cSlave::temperature_mc(&whoami_only), None);
    }

    #[test]
    fn declarative_spi_sensor_reads_who_am_i_and_signed_data() {
        let mut s = sensor(SPI_IMU_SPEC);
        s.set_input("gyro_x", 1234.0);
        let mut bus = SpiBus::new("SPI", Box::new(s));
        assert_eq!(spi_read(&mut bus, 0x80 | 0x0f, 1), vec![0x42]);
        bus.slave_mut::<RegisterMapSensor>().unwrap().deselect();
        let d = spi_read(&mut bus, 0x80 | 0x22, 2);
        assert_eq!(d, vec![0xD2, 0x04]);
        assert_eq!(i16::from_le_bytes([d[0], d[1]]), 1234);

        let mut s = sensor(SPI_IMU_SPEC);
        s.set_input("gyro_x", -2.0);
        let mut bus = SpiBus::new("SPI", Box::new(s));
        let d = spi_read(&mut bus, 0x80 | 0x22, 2);
        assert_eq!(i16::from_le_bytes([d[0], d[1]]), -2);
    }

    /// `miso_preview` equals the byte the very next `transfer` returns at
    /// every byte position (command, streaming, auto-increment hop, across a
    /// CS deassert) and never advances the stream.
    #[test]
    fn miso_preview_matches_transfer_stream_byte_for_byte() {
        let mut s = sensor(SPI_IMU_SPEC);
        s.set_input("gyro_x", 1234.0);
        for mosi in [0x80 | 0x0f, 0x00, 0x00, 0x00, 0x00] {
            let preview = s.miso_preview();
            assert_eq!(preview, s.miso_preview(), "preview must be non-advancing");
            assert_eq!(
                preview,
                Some(SpiSlave::transfer(&mut s, mosi)),
                "mosi 0x{mosi:02x}"
            );
        }
        SpiSlave::deselect(&mut s);
        for mosi in [0x80 | 0x22, 0x00, 0x00] {
            let preview = s.miso_preview();
            assert_eq!(preview, Some(SpiSlave::transfer(&mut s, mosi)));
        }
    }

    /// A raw datasheet address (0xD0) and its pre-masked form (0x50) resolve
    /// identically through `transfer` and `register_bytes`; two registers
    /// colliding post-mask are rejected by validation and panic in the
    /// unchecked `from_spec` path.
    #[test]
    fn spi_raw_datasheet_addresses_normalize_and_collisions_are_refused() {
        for addr in [0xD0, 0x50] {
            let s = sensor(&bmp280_spi_spec(addr));
            assert_eq!(s.register_bytes(0xD0), vec![0x58]);
            assert_eq!(s.register_bytes(0x50), vec![0x58]);
            let mut bus = SpiBus::new("SPI", Box::new(s));
            assert_eq!(
                spi_read(&mut bus, 0xD0, 1),
                vec![0x58],
                "spec addr {addr:#x}"
            );
        }
        assert!(RegisterMapSensor::from_toml(COLLIDING_SPI_SPEC).is_err());
    }

    #[test]
    #[should_panic(expected = "collides with an existing register")]
    fn from_spec_panics_on_post_mask_collision() {
        let spec: SensorSpec = toml::from_str(COLLIDING_SPI_SPEC).unwrap();
        let _ = RegisterMapSensor::from_spec(spec);
    }

    // ── Shipped sensor specs against datasheet worked examples ───────────────

    const BME280_SPEC: &str = include_str!("../../../../testdata/sensor-specs/bme280.toml");
    const MPU6050_SPEC: &str = include_str!("../../../../testdata/sensor-specs/mpu6050.toml");
    const ADS1115_SPEC: &str = include_str!("../../../../testdata/sensor-specs/ads1115.toml");
    const INA219_SPEC: &str = include_str!("../../../../testdata/sensor-specs/ina219.toml");
    const MCP4728_SPEC: &str = include_str!("../../../../testdata/sensor-specs/mcp4728.toml");

    // Direct ports of the Bosch datasheet reference C (§4.2.3, §8.2);
    // `wrapping_*` reproduces C's modular int32 semantics.
    fn bme280_compensate_t(adc_t: i32, t1: i32, t2: i32, t3: i32) -> (i32, i32) {
        let var1 = (((adc_t >> 3) - (t1 << 1)).wrapping_mul(t2)) >> 11;
        let d = (adc_t >> 4) - t1;
        let var2 = ((d.wrapping_mul(d) >> 12).wrapping_mul(t3)) >> 14;
        let t_fine = var1 + var2;
        (t_fine, (t_fine * 5 + 128) >> 8)
    }

    fn bme280_compensate_p(adc_p: i32, t_fine: i32, p: [i32; 9]) -> u32 {
        let [p1, p2, p3, p4, p5, p6, p7, p8, p9] = p;
        let mut var1 = (t_fine >> 1) - 64000;
        let mut var2 = ((var1 >> 2).wrapping_mul(var1 >> 2) >> 11).wrapping_mul(p6);
        var2 = var2 + (var1.wrapping_mul(p5) << 1);
        var2 = (var2 >> 2) + (p4 << 16);
        let a = p3.wrapping_mul((var1 >> 2).wrapping_mul(var1 >> 2) >> 13) >> 3;
        let b = p2.wrapping_mul(var1) >> 1;
        var1 = (a + b) >> 18;
        var1 = (32768 + var1).wrapping_mul(p1) >> 15;
        if var1 == 0 {
            return 0;
        }
        let mut p: u32 = ((1_048_576 - adc_p) as u32)
            .wrapping_sub((var2 >> 12) as u32)
            .wrapping_mul(3125);
        if p < 0x8000_0000 {
            p = (p << 1) / (var1 as u32);
        } else {
            p = (p / (var1 as u32)) * 2;
        }
        let w1 = p9.wrapping_mul((((p >> 3) * (p >> 3)) >> 13) as i32) >> 12;
        let w2 = ((p >> 2) as i32).wrapping_mul(p8) >> 13;
        ((p as i32) + ((w1 + w2 + p7) >> 4)) as u32
    }

    fn bme280_compensate_h(adc_h: i32, t_fine: i32, h: [i32; 6]) -> u32 {
        let [h1, h2, h3, h4, h5, h6] = h;
        let v0 = t_fine - 76800;
        let term_a = (((adc_h << 14) - (h4 << 20) - h5.wrapping_mul(v0)) + 16384) >> 15;
        let y1 = v0.wrapping_mul(h6) >> 10;
        let y2 = (v0.wrapping_mul(h3) >> 11) + 32768;
        let y3 = y1.wrapping_mul(y2) >> 10;
        let y4 = (y3 + 2_097_152).wrapping_mul(h2) + 8192;
        let mut v = term_a.wrapping_mul(y4 >> 14);
        v = v - ((((v >> 15).wrapping_mul(v >> 15) >> 7).wrapping_mul(h1)) >> 4);
        (v.clamp(0, 419_430_400) >> 12) as u32
    }

    /// The shipped BME280 spec: chip-ID 0x60, trimming coefficients that
    /// round-trip to the datasheet Appendix values, and raw data that
    /// compensates to 25.08 °C, 100656 Pa and ≈54.45 %RH.
    #[test]
    fn declarative_bme280_decodes_datasheet_worked_example() {
        let addr = 0x76;
        let mut bus = i2c_bus(sensor(BME280_SPEC));
        assert_eq!(i2c_read_burst(&mut bus, addr, 0xD0, 1), vec![0x60]);

        let cal1 = i2c_read_burst(&mut bus, addr, 0x88, 26);
        let cal2 = i2c_read_burst(&mut bus, addr, 0xE1, 7);
        let le16 = |b: &[u8], i: usize| i16::from_le_bytes([b[i], b[i + 1]]) as i32;
        let leu16 = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as i32;
        let (t1, t2, t3) = (leu16(&cal1, 0), le16(&cal1, 2), le16(&cal1, 4));
        let p = [
            leu16(&cal1, 6),
            le16(&cal1, 8),
            le16(&cal1, 10),
            le16(&cal1, 12),
            le16(&cal1, 14),
            le16(&cal1, 16),
            le16(&cal1, 18),
            le16(&cal1, 20),
            le16(&cal1, 22),
        ];
        let h = [
            cal1[25] as i32,
            le16(&cal2, 0),
            cal2[2] as i32,
            ((cal2[3] as i8 as i32) << 4) | (cal2[4] & 0x0F) as i32,
            ((cal2[5] as i8 as i32) << 4) | ((cal2[4] >> 4) as i32),
            cal2[6] as i8 as i32,
        ];
        assert_eq!((t1, t2, t3), (27504, 26435, -1000));
        assert_eq!(p, [36477, -10685, 3024, 2855, 140, -7, 15500, -14600, 6000]);
        assert_eq!(h, [75, 362, 0, 317, 0, 30]);

        let d = i2c_read_burst(&mut bus, addr, 0xF7, 8);
        assert_eq!(&d[0..3], &[0x65, 0x5A, 0xC0]);
        assert_eq!(&d[3..6], &[0x7E, 0xED, 0x00]);
        let adc_p = ((d[0] as i32) << 12) | ((d[1] as i32) << 4) | ((d[2] as i32) >> 4);
        let adc_t = ((d[3] as i32) << 12) | ((d[4] as i32) << 4) | ((d[5] as i32) >> 4);
        let adc_h = ((d[6] as i32) << 8) | (d[7] as i32);
        assert_eq!((adc_p, adc_t, adc_h), (415148, 519888, 30000));

        let (t_fine, t) = bme280_compensate_t(adc_t, t1, t2, t3);
        assert_eq!((t_fine, t), (128422, 2508));
        assert_eq!(bme280_compensate_p(adc_p, t_fine, p), 100656);
        let h_q = bme280_compensate_h(adc_h, t_fine, h);
        assert_eq!(h_q, 55759);
        let rh = h_q as f64 / 1024.0;
        assert!((54.0..55.0).contains(&rh), "{rh}");
    }

    #[test]
    fn declarative_mpu6050_decodes_driven_quantities() {
        let mut s = sensor(MPU6050_SPEC);
        s.set_input("accel_z_g", 1.0);
        s.set_input("gyro_x_dps", 250.0);
        s.set_input("temp_c", 25.0);
        let addr = 0x68;
        let mut bus = i2c_bus(s);
        assert_eq!(i2c_read_burst(&mut bus, addr, 0x75, 1), vec![0x68]);
        let d = i2c_read_burst(&mut bus, addr, 0x3B, 14);
        let be = |i: usize| i16::from_be_bytes([d[i], d[i + 1]]);
        assert_eq!((be(0), be(2)), (0, 0), "accel X/Y at rest");
        assert_eq!(be(4), 16384, "accel Z = +1 g at 16384 LSB/g");
        assert_eq!(be(8), 32750, "gyro X = 250 °/s at 131 LSB/(°/s)");
        let temp_c = be(6) as f64 / 340.0 + 36.53;
        assert!((temp_c - 25.0).abs() < 0.05, "{temp_c}");
    }

    // ── Write side ───────────────────────────────────────────────────────────

    /// A config write register whose `pga` field selects the data register's
    /// full-scale range.
    const WRITE_COUPLING_SPEC: &str = r#"
[sensor]
name = "MINIADC"
bus = "i2c"
i2c_address = 0x48

[[sensor.input]]
name = "a0"
default = 1.0

[[sensor.write_register]]
addr = 0x01
encoding = "u16_be"
store = "config"
default = 1024.0   # 0x0400: pga = 010 -> 2.048 FSR

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

    #[test]
    fn pointer_write_commits_store_and_couples_read() {
        let addr = 0x48;
        let mut bus = i2c_bus(sensor(WRITE_COUPLING_SPEC));
        assert_eq!(
            i2c_read_u16(&mut bus, addr, 0x00) as i16,
            16000,
            "FSR 2.048 by default"
        );

        i2c_write_u16(&mut bus, addr, 0x01, 0x0200);
        {
            let s = bus.slave::<RegisterMapSensor>(addr).unwrap();
            assert_eq!(s.store("config"), Some(0x0200 as f64));
            assert_eq!(s.store("pga"), Some(1.0));
            assert_eq!(s.ignored_write_bytes(), 0);
        }
        assert_eq!(
            i2c_read_u16(&mut bus, addr, 0x00) as i16,
            8000,
            "FSR 4.096 after the write"
        );

        // Payload past the u16 width and payload for a read-only register are
        // accepted but counted.
        i2c_write(&mut bus, addr, &[0x01, 0x02, 0x00, 0xAA]);
        i2c_write(&mut bus, addr, &[0x00, 0x55]);
        assert_eq!(
            bus.slave::<RegisterMapSensor>(addr)
                .unwrap()
                .ignored_write_bytes(),
            2
        );
    }

    /// A command-framed 2-channel DAC: match, greedy 2-byte groups, auto
    /// channel increment with START reset, field extraction and output laws.
    const MINI_DAC_SPEC: &str = r#"
[sensor]
name = "MINIDAC"
bus = "i2c"
i2c_address = 0x60
channels = 2

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

[[sensor.output]]
name = "vout_b"
channel = 1
expr = "if(pd == 0.0, code / 4096 * 4.096, 0.0)"

[sensor.protocol]
style = "i2c_pointer"
"#;

    #[test]
    fn single_byte_write_command_drains_on_the_completing_byte() {
        const ONE_BYTE_CMD_SPEC: &str = r#"
[sensor]
name = "ONEBYTE"
bus = "i2c"
i2c_address = 0x30
channels = 1

[[sensor.state]]
name = "x"
default = 0.0

[[sensor.write_command]]
name = "set_x"
match_mask = 0x00
match_value = 0x00
group_bytes = 1

[[sensor.write_command.field]]
name = "val"
bits = [7, 0]

[sensor.write_command.update]
x = "val"

[sensor.protocol]
style = "i2c_pointer"
"#;
        let mut bus = i2c_bus(sensor(ONE_BYTE_CMD_SPEC));
        i2c_write(&mut bus, 0x30, &[0xAA]);
        let s = bus.slave::<RegisterMapSensor>(0x30).unwrap();
        assert_eq!(s.channel_state("x", 0), Some(170.0));
    }

    #[test]
    fn command_write_updates_channels_and_output_laws() {
        let addr = 0x60;
        let mut bus = i2c_bus(sensor(MINI_DAC_SPEC));
        // Channel 0 code 2048; channel 1 code 1024 with PD = 01.
        i2c_write(&mut bus, addr, &[0x08, 0x00, 0x14, 0x00]);
        let s = bus.slave::<RegisterMapSensor>(addr).unwrap();
        assert_eq!(s.channel_state("code", 0), Some(2048.0));
        assert_eq!(s.channel_state("code", 1), Some(1024.0));
        assert_eq!(s.channel_state("pd", 1), Some(1.0));
        assert!((s.output_volts("vout_a").unwrap() - 2.048).abs() < 1e-9);
        assert_eq!(s.output_volts("vout_b"), Some(0.0));

        i2c_write(&mut bus, addr, &[0x01, 0x00]);
        let s = bus.slave::<RegisterMapSensor>(addr).unwrap();
        assert_eq!(
            s.channel_state("code", 0),
            Some(256.0),
            "cursor reset on START"
        );
        assert_eq!(
            s.channel_state("code", 1),
            Some(1024.0),
            "channel 1 untouched"
        );

        i2c_write(&mut bus, addr, &[0x80, 0xFF]);
        let s = bus.slave::<RegisterMapSensor>(addr).unwrap();
        assert_eq!(s.ignored_write_bytes(), 2, "unmodeled command counted");
        assert_eq!(s.channel_state("code", 0), Some(256.0));
    }

    /// The STOP is recorded at dispatch and delivered on the scheduler-cadence
    /// flush, where the output law lands on the bound driver.
    #[test]
    fn on_stop_drives_bound_output_net() {
        use crate::peripherals::TickCtx;
        use hauksbee_ir::{Circuit, Device, SourceKind};

        let mut circuit = Circuit::default();
        let net = circuit.node("VOUT_A");
        let driver = PinDriver::stamp(&mut circuit, net, "VOUT_A", "minidac_a", 1.0);
        let vsource = driver.vsource;
        let mut s = sensor(MINI_DAC_SPEC);
        assert!(s.attach_output_driver_for_channel(0, driver));
        let mut bus = i2c_bus(s);
        i2c_write(&mut bus, 0x60, &[0x08, 0x00]);

        let src_volts = |c: &Circuit| match c.devices[vsource.0 as usize] {
            Device::Vsource {
                kind: SourceKind::Dc(v),
                ..
            } => v,
            _ => panic!("driver vsource"),
        };
        assert_eq!(src_volts(&circuit), 0.0, "not driven before flush");
        let volts = vec![0.0; 4];
        let mut ctx = TickCtx {
            circuit: &mut circuit,
            node_volts: &volts,
            t: 0.0,
            dt: 1e-3,
        };
        bus.flush_stops(&mut ctx);
        assert!((src_volts(&circuit) - 2.048).abs() < 1e-9);
    }

    /// TI SBAS444: POR config 0x8583 is AIN0-AIN1 at ±2.048 V; 0xC383 selects
    /// AIN0/GND at ±4.096 V (125 uV/LSB); full scale clips at 0x7FFF.
    #[test]
    fn declarative_ads1115_config_write_selects_mux_and_pga() {
        let mut s = sensor(ADS1115_SPEC);
        s.set_input("a0", 0.5);
        s.set_input("a1", 0.3);
        let addr = 0x48;
        let mut bus = i2c_bus(s);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x00) as i16, 3200);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x01), 0x8583);

        i2c_write_u16(&mut bus, addr, 0x01, 0xC383);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x01), 0xC383);
        {
            let s = bus.slave::<RegisterMapSensor>(addr).unwrap();
            assert_eq!(s.store("mux"), Some(4.0));
            assert_eq!(s.store("pga"), Some(1.0));
        }
        bus.slave_mut_t::<RegisterMapSensor>(addr)
            .unwrap()
            .set_input("a0", 1.024);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x00), 0x2000);

        i2c_write_u16(&mut bus, addr, 0x01, 0x8583);
        {
            let s = bus.slave_mut_t::<RegisterMapSensor>(addr).unwrap();
            s.set_input("a0", 0.0);
            s.set_input("a1", 0.5);
        }
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x00) as i16, -8000);
        bus.slave_mut_t::<RegisterMapSensor>(addr)
            .unwrap()
            .set_input("a0", 5.0);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x00), 0x7FFF);
    }

    /// TI SBOS448 §8.5.1: R_SHUNT = 0.1 Ω, Cal = 0x1000; at 2 A / 12 V the
    /// shunt reads 20000, bus 0x5DC2, current 20000 and power 12000, and
    /// current/power stay zero until calibration is written.
    #[test]
    fn declarative_ina219_calibration_write_feeds_current_and_power() {
        let mut s = sensor(INA219_SPEC);
        s.set_input("shunt_v", 0.2);
        s.set_input("bus_v", 12.0);
        let addr = 0x40;
        let mut bus = i2c_bus(s);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x00), 0x399F);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x01) as i16, 20000);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x02), 0x5DC2);
        assert_eq!(
            i2c_read_u16(&mut bus, addr, 0x04),
            0,
            "current 0 before cal"
        );
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x03), 0, "power 0 before cal");

        i2c_write_u16(&mut bus, addr, 0x05, 0x1000);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x04), 20000);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x03), 12000);

        i2c_write_u16(&mut bus, addr, 0x05, 0x1001);
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x05), 0x1000, "FS0 forced low");
        assert_eq!(i2c_read_u16(&mut bus, addr, 0x04), 20000);
    }

    // ── The MCP4728 as a data instance of the schema ─────────────────────────

    fn mcp4728_bus(addrs: &[u8]) -> I2cBus {
        let mut bus = I2cBus::new("U");
        for &addr in addrs {
            let mut s = sensor(MCP4728_SPEC);
            s.set_i2c_address(addr);
            bus = bus.with_slave(Box::new(s));
        }
        bus
    }

    fn dac_code(bus: &I2cBus, addr: u8, channel: usize) -> u16 {
        bus.slave::<RegisterMapSensor>(addr)
            .unwrap()
            .channel_state("code", channel)
            .unwrap() as u16
    }

    fn dac_vout(bus: &I2cBus, addr: u8, channel: usize) -> f64 {
        let name = ["vout_a", "vout_b", "vout_c", "vout_d"][channel];
        bus.slave::<RegisterMapSensor>(addr)
            .unwrap()
            .output_volts(name)
            .unwrap()
    }

    /// The exact byte pair the firmware sends for a Fast Write of `value`.
    fn fast_write_pair(value: u16) -> [u8; 2] {
        let v = value & 0x0FFF;
        [((v >> 8) & 0x0F) as u8, (v & 0xFF) as u8]
    }

    /// A Multi/Sequential-Write data pair: Vref=internal, PD=00, gain 2.
    fn write_pair(code: u16) -> [u8; 2] {
        [
            (1u8 << 7) | (1 << 4) | (((code >> 8) & 0x0F) as u8),
            (code & 0xFF) as u8,
        ]
    }

    /// Fast Write sets VOUT = code * 0.001 V across the code range (internal
    /// VREF 2.048 V, gain 2), auto-increments channels within one transaction,
    /// reads back, and leaves other instances on the bus untouched.
    #[test]
    fn mcp4728_fast_write_sets_vout_auto_increments_and_reads_back() {
        for &code in &[0u16, 1, 1000, 2048, 4095] {
            let mut bus = mcp4728_bus(&[0x60]);
            i2c_write(&mut bus, 0x60, &fast_write_pair(code));
            assert_eq!(dac_code(&bus, 0x60, 0), code);
            assert!(
                (dac_vout(&bus, 0x60, 0) - code as f64 * 0.001).abs() < 1e-9,
                "code {code}"
            );
        }

        let mut bus = mcp4728_bus(&[0x60, 0x61, 0x62]);
        let codes = [100u16, 2048, 3000, 4095];
        let bytes: Vec<u8> = codes.iter().flat_map(|&c| fast_write_pair(c)).collect();
        i2c_write(&mut bus, 0x60, &bytes);
        for (ch, &c) in codes.iter().enumerate() {
            assert_eq!(dac_code(&bus, 0x60, ch), c, "channel {ch}");
        }
        assert_eq!(dac_code(&bus, 0x61, 0), 0, "0x61 untouched");
        assert_eq!(dac_code(&bus, 0x62, 0), 0, "0x62 untouched");

        // Read the frame back: byte 1 of the channel A triple carries D11..D8
        // in its low nibble, byte 2 carries D7..D0.
        bus.dispatch(I2cEvent::Start {
            addr: 0x60,
            read: true,
        });
        let _status = bus.dispatch(I2cEvent::Read { addr: 0x60 }).unwrap();
        let hi = bus.dispatch(I2cEvent::Read { addr: 0x60 }).unwrap();
        let lo = bus.dispatch(I2cEvent::Read { addr: 0x60 }).unwrap();
        bus.dispatch(I2cEvent::Stop { addr: 0x60 });
        assert_eq!((((hi & 0x0F) as u16) << 8) | lo as u16, 100);
    }

    /// Multi-Write (C2C1C0 = 010, W1W0 = 01) names its channel in the command
    /// byte; Sequential Write (W1W0 = 10) sends one command byte then
    /// auto-incrementing data pairs.
    #[test]
    fn mcp4728_multi_and_sequential_write_decode_channels() {
        let mut bus = mcp4728_bus(&[0x60]);
        let [dhi, dlo] = write_pair(1500);
        i2c_write(&mut bus, 0x60, &[0b0100_1100, dhi, dlo]);
        assert_eq!(dac_code(&bus, 0x60, 2), 1500, "channel C via Multi-Write");
        assert!((dac_vout(&bus, 0x60, 2) - 1.500).abs() < 1e-9);

        let mut bus = mcp4728_bus(&[0x60]);
        let mut bytes = vec![0x50];
        bytes.extend([100u16, 200, 300, 400].iter().flat_map(|&c| write_pair(c)));
        i2c_write(&mut bus, 0x60, &bytes);
        assert_eq!(
            (0..4)
                .map(|ch| dac_code(&bus, 0x60, ch))
                .collect::<Vec<_>>(),
            [100, 200, 300, 400]
        );
        assert!((dac_vout(&bus, 0x60, 3) - 0.400).abs() < 1e-9);
    }
}
