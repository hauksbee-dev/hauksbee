//! Generic declarative register-map sensor interpreter.
//!
//! [`RegisterMapSensor`] reads a [`SensorSpec`] (the declarative `[sensor]`
//! TOML defined in `hauksbee-models`) and *realizes* it as a live bus slave:
//! it implements [`I2cSlave`] (the `i2c_pointer` protocol) AND [`SpiSlave`]
//! (the `spi_reg` protocol), producing the same bytes a hand-coded model would.
//!
//! Crucially it does NOT wrap or delegate to `Lm75` / `Mcp3008`: every byte is
//! computed here from the spec — the register pointer selects a
//! [`RegisterSpec`], its `expr` is evaluated against the current input values
//! with `evalexpr`, and the resulting number is packed per the register's
//! [`Encoding`]. A declarative LM75 is therefore byte-for-byte identical to the
//! hand-coded `Lm75` because both emit the same datasheet packing, not because
//! one calls the other.
//!
//! It attaches through the existing bus peripherals unchanged: build an
//! [`I2cBus`] with the sensor as a slave (`add_slave` / `with_slave`), or a
//! [`SpiBus`] (`SpiBus::new`). The `on_i2c` / `on_spi` Renode/simavr bridge is
//! untouched — this is just another slave.

use std::collections::HashMap;

use evalexpr::{
    build_operator_tree, ContextWithMutableVariables, DefaultNumericTypes, HashMapContext,
    Node as EvalNode, Value,
};

use hauksbee_models::sensor_spec::{Bus, Encoding, ProtocolStyle, RegisterSpec, SensorSpec};

use super::i2c::I2cSlave;
use super::spi::SpiSlave;

/// A precompiled register: the spec plus its parsed expression (if any).
struct CompiledRegister {
    spec: RegisterSpec,
    program: Option<EvalNode<DefaultNumericTypes>>,
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
        // which should not happen for an encoded register — treat as 0.0.
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
                 This is a bug — all declared inputs should be present.",
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
fn eval_number(program: &EvalNode<DefaultNumericTypes>, inputs: &HashMap<String, f64>) -> Option<f64> {
    let mut ctx = HashMapContext::<DefaultNumericTypes>::new();
    for (k, v) in inputs {
        let _ = ctx.set_value(k.clone(), Value::from_float(*v));
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
        Encoding::Raw => Vec::new(),
    }
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

    /// addr -> compiled register.
    registers: HashMap<u8, CompiledRegister>,
    /// Stable ascending register addresses (for I2C auto-increment).
    addr_order: Vec<u8>,
    /// Live input values, seeded from each input's `default`.
    inputs: HashMap<String, f64>,

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
        // Validate that every register's expr actually compiles. This is an
        // engine-side check (evalexpr lives here, not in hauksbee-models) that
        // complements the token-level identifier check in SensorSpec::validate().
        for r in &spec.sensor.registers {
            if let Some(expr) = r.expr.as_deref() {
                build_operator_tree::<DefaultNumericTypes>(expr).map_err(|e| {
                    hauksbee_models::sensor_spec::SensorSpecError::Invalid(format!(
                        "register 0x{:02x} expr {:?} failed to compile: {}",
                        r.addr, expr, e
                    ))
                })?;
            }
        }
        Ok(Self::from_spec(spec))
    }

    /// Build from an already-parsed (and validated) spec.
    ///
    /// # Panics
    ///
    /// This constructor is public but does NOT re-validate the spec. If two
    /// registers collide to the same key under [`Sensor::register_key`] (e.g. an
    /// SPI spec declaring both `0x50` and `0xD0`, which both mask to `0x50`), one
    /// would silently overwrite the other in the register map. Rather than
    /// produce a sensor that quietly drops a register, this panics with a message
    /// pointing the caller at `from_toml`/`validate`. Construct via
    /// [`RegisterMapSensor::from_toml`] (which validates) to avoid this.
    pub fn from_spec(spec: SensorSpec) -> Self {
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
            let program = r
                .expr
                .as_deref()
                .and_then(|e| build_operator_tree::<DefaultNumericTypes>(e).ok());
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

        RegisterMapSensor {
            name: s.name,
            bus: s.bus,
            i2c_address: s.i2c_address.unwrap_or(0),
            rw_read_is_high: s.protocol.rw_read_is_high,
            addr_mask: s.protocol.addr_mask,
            spi_reg_protocol,
            registers,
            addr_order,
            inputs,
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
    /// Returns `[0xFF]` for any address not declared in the spec — this matches
    /// typical I2C/SPI open-drain bus idle behaviour and makes undeclared reads
    /// visible rather than silently returning another register's data.
    ///
    /// `addr` is the RAW, externally-supplied address: it is normalized via
    /// [`Self::normalize_addr`] before lookup so callers may pass the raw
    /// datasheet register address (e.g. 0xD0) for an SPI sensor and still hit the
    /// post-mask key (0x50) the register is stored under.
    /// Public so tests can compare against a hand-coded model directly.
    pub fn register_bytes(&self, addr: u8) -> Vec<u8> {
        let key = self.normalize_addr(addr);
        self.registers
            .get(&key)
            .map(|r| r.bytes(&self.inputs))
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

    fn on_start(&mut self, read: bool) {
        if read {
            // Repeated START for the read phase: load the addressed register.
            self.refill_i2c_read();
        } else {
            self.got_pointer = false;
        }
    }

    fn on_write(&mut self, data: u8) {
        if !self.got_pointer {
            // First write byte selects the register pointer.
            self.pointer = data;
            self.got_pointer = true;
            self.refill_i2c_read();
        }
        // Subsequent writes are register writes (config). The declarative model
        // is read-only for now; we accept and ignore them so the firmware's
        // config writes don't stall, matching a sensor that NAKs nothing.
    }

    fn on_read(&mut self) -> u8 {
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

    fn on_stop(&mut self) {
        self.got_pointer = false;
    }

    fn state(&self) -> HashMap<String, f64> {
        let mut m = HashMap::new();
        m.insert("pointer".into(), self.pointer as f64);
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

// ── SPI: spi_reg protocol ──────────────────────────────────────────────────

impl SpiSlave for RegisterMapSensor {
    fn transfer(&mut self, mosi: u8) -> u8 {
        if self.spi_first_byte {
            // Command byte: high bit = R/W per `rw_read_is_high`, low bits (per
            // `addr_mask`) = register address.
            let read_bit = (mosi & 0x80) != 0;
            self.spi_is_read = if self.rw_read_is_high { read_bit } else { !read_bit };
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
            // Write phase: accept and ignore (read-only declarative model).
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

    /// Read the LM75 temperature register over a bus and return (msb, lsb).
    fn read_temp_i2c(bus: &mut I2cBus) -> (u8, u8) {
        bus.dispatch(I2cEvent::Start { addr: 0x48, read: false });
        bus.dispatch(I2cEvent::Write { addr: 0x48, data: 0x00 });
        bus.dispatch(I2cEvent::Start { addr: 0x48, read: true });
        let msb = bus.dispatch(I2cEvent::Read { addr: 0x48 }).unwrap();
        let lsb = bus.dispatch(I2cEvent::Read { addr: 0x48 }).unwrap();
        bus.dispatch(I2cEvent::Stop { addr: 0x48 });
        (msb, lsb)
    }

    /// PROOF: a declarative LM75 is byte-identical to the hand-coded `Lm75`
    /// across a temperature sweep, through the real I2C bus dispatch path. The
    /// bytes come from the generic interpreter reading the spec — it never
    /// touches `Lm75`.
    #[test]
    fn declarative_lm75_is_byte_identical_to_handcoded() {
        for &t in &[
            -40.0, -10.0, -0.5, 0.0, 0.125, 12.5, 22.0, 25.0, 36.6, 80.0, 100.0, 124.875,
        ] {
            // Hand-coded reference.
            let mut hand = I2cBus::new("I2C").with_slave(Box::new(Lm75::new(0x48, t)));
            let hand_bytes = read_temp_i2c(&mut hand);

            // Declarative interpreter (genuinely spec-driven).
            let mut sensor = RegisterMapSensor::from_toml(LM75_SPEC).unwrap();
            sensor.set_input("temperature_c", t);
            let mut decl = I2cBus::new("I2C").with_slave(Box::new(sensor));
            let decl_bytes = read_temp_i2c(&mut decl);

            assert_eq!(
                decl_bytes, hand_bytes,
                "declarative LM75 bytes {decl_bytes:?} != hand-coded {hand_bytes:?} at {t} °C"
            );
        }
    }

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

    /// PROOF: a small declarative SPI sensor returns the WHO_AM_I constant and a
    /// driven i16 data register correctly through `RegisterMapSensor as
    /// SpiSlave`.
    #[test]
    fn declarative_spi_sensor_reads_who_am_i_and_data() {
        let mut sensor = RegisterMapSensor::from_toml(SPI_IMU_SPEC).unwrap();
        sensor.set_input("gyro_x", 1234.0);
        let mut bus = SpiBus::new("SPI", Box::new(sensor));

        // WHO_AM_I (0x0f) read: cmd = 0x80 | 0x0f, then one data byte.
        let _status = bus.transfer(0x80 | 0x0f);
        let who = bus.transfer(0x00);
        assert_eq!(who, 0x42, "WHO_AM_I should be 0x42");

        // CS deassert between transactions.
        bus.slave_mut::<RegisterMapSensor>().unwrap().deselect();

        // Data register 0x22 read: i16 little-endian for 1234 = 0x04D2 ->
        // bytes [0xD2, 0x04].
        let _status = bus.transfer(0x80 | 0x22);
        let lo = bus.transfer(0x00);
        let hi = bus.transfer(0x00);
        let value = i16::from_le_bytes([lo, hi]);
        assert_eq!(value, 1234, "data register should decode to 1234");
        assert_eq!([lo, hi], [0xD2, 0x04]);
    }

    // A BMP280-like SPI spec whose chip-ID register is declared at its RAW
    // datasheet address 0xD0 (NOT the hand-masked 0x50). The firmware reads it
    // with command byte 0xD0 (read bit 7 set | addr 0x50).
    const BMP280_RAW_ADDR_SPEC: &str = r#"
[sensor]
name = "BMP280"
bus = "spi"

[[sensor.register]]
addr = 0xD0
const = [0x58]

[sensor.protocol]
style = "spi_reg"
rw_read_is_high = true
addr_mask = 0x7f
"#;

    // Same sensor but declared with the PRE-MASKED address 0x50 (the old hand-
    // masked style). Must still resolve identically — backward compatibility.
    const BMP280_PREMASKED_SPEC: &str = r#"
[sensor]
name = "BMP280"
bus = "spi"

[[sensor.register]]
addr = 0x50
const = [0x58]

[sensor.protocol]
style = "spi_reg"
rw_read_is_high = true
addr_mask = 0x7f
"#;

    /// PROOF of the fix: a spec declaring the chip-ID at the RAW datasheet
    /// address 0xD0, read via command byte 0xD0, returns 0x58 (not 0xFF). The
    /// interpreter masks the R/W bit off internally so the natural datasheet
    /// address resolves. A second assertion proves the pre-masked 0x50 spec read
    /// via the same 0xD0 command ALSO returns 0x58 (backward compatible).
    #[test]
    fn spi_raw_datasheet_addr_resolves_and_is_backward_compatible() {
        // Raw datasheet address 0xD0.
        let sensor = RegisterMapSensor::from_toml(BMP280_RAW_ADDR_SPEC).unwrap();
        let mut bus = SpiBus::new("SPI", Box::new(sensor));
        let _status = bus.transfer(0xD0); // read bit set | masked addr 0x50
        let id = bus.transfer(0x00);
        assert_eq!(id, 0x58, "raw-addr (0xD0) chip-ID read should return 0x58");

        // Pre-masked address 0x50 — same command byte must yield the same byte.
        let sensor = RegisterMapSensor::from_toml(BMP280_PREMASKED_SPEC).unwrap();
        let mut bus = SpiBus::new("SPI", Box::new(sensor));
        let _status = bus.transfer(0xD0);
        let id = bus.transfer(0x00);
        assert_eq!(id, 0x58, "pre-masked (0x50) chip-ID read should also return 0x58");
    }

    /// Two SPI registers that collide to the same post-mask address (0x50 and
    /// 0xD0 both mask to 0x50) are genuinely indistinguishable on the 7-bit SPI
    /// address field, so the spec must be rejected by validation.
    #[test]
    fn spi_post_mask_address_collision_is_rejected() {
        let colliding = r#"
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
        let res = RegisterMapSensor::from_toml(colliding);
        assert!(
            res.is_err(),
            "SPI registers colliding post-mask (0x50 & 0xD0) must be rejected"
        );
    }

    /// Finding 1 regression guard: the public `register_bytes` helper must honor
    /// the same raw-address contract as `transfer`. A spec declaring the chip-ID
    /// at the RAW datasheet address 0xD0 is stored under the post-mask key 0x50;
    /// `register_bytes(0xD0)` must normalize the raw address and return the const
    /// (0x58), not miss the lookup and return the 0xFF idle byte.
    #[test]
    fn register_bytes_normalizes_raw_spi_addr() {
        let sensor = RegisterMapSensor::from_toml(BMP280_RAW_ADDR_SPEC).unwrap();
        // Raw datasheet address passed directly to the public helper.
        assert_eq!(
            sensor.register_bytes(0xD0),
            vec![0x58],
            "register_bytes(0xD0) on a raw-0xD0 SPI spec must normalize and return the const"
        );
        // The pre-masked address must of course also resolve.
        assert_eq!(
            sensor.register_bytes(0x50),
            vec![0x58],
            "register_bytes(0x50) (pre-masked) must resolve to the same register"
        );
    }

    /// Finding 2 regression guard: `from_spec` is public and unchecked. Feeding
    /// it a spec whose two registers collide post-mask (0x50 and 0xD0 both mask
    /// to 0x50) must PANIC rather than silently overwrite one in the map. We
    /// build the SensorSpec via raw TOML deserialization (bypassing
    /// `SensorSpec::from_toml`'s validate()) to exercise the unchecked boundary.
    #[test]
    #[should_panic(expected = "collides with an existing register")]
    fn from_spec_panics_on_post_mask_collision() {
        let colliding = r#"
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
        // Deserialize WITHOUT validating, to reach the unchecked from_spec path.
        let spec: SensorSpec = toml::from_str(colliding).unwrap();
        let _ = RegisterMapSensor::from_spec(spec);
    }

    /// The interpreter must not be a thin wrapper: feeding a negative i16 value
    /// exercises the two's-complement packing path the spec dictates.
    #[test]
    fn spi_negative_i16_packs_correctly() {
        let mut sensor = RegisterMapSensor::from_toml(SPI_IMU_SPEC).unwrap();
        sensor.set_input("gyro_x", -2.0);
        let mut bus = SpiBus::new("SPI", Box::new(sensor));
        let _ = bus.transfer(0x80 | 0x22);
        let lo = bus.transfer(0x00);
        let hi = bus.transfer(0x00);
        assert_eq!(i16::from_le_bytes([lo, hi]), -2);
    }
}
