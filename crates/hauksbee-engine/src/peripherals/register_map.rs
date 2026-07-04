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

    fn on_stop(&mut self, _ctx: &mut super::TickCtx) {
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

    // ── §6 sensor-coverage fixtures: BME280 + MPU6050 ────────────────────────
    //
    // Per 05-cosim-fidelity §6.2/§7.2: each new sensor lands with a fixture that
    // "reads a known register value through the bound bus and asserts the decoded
    // physical quantity". These load the SHIPPED specs (docs/hunts/specs/*.toml)
    // so the fixture proves the exact spec that ships, drive them through the real
    // `I2cBus` dispatch path (no injection, no hand-coded model), and assert the
    // decoded physical output against datasheet worked-example numbers.

    /// The canonical shipped BME280 spec (single source of truth for the model).
    const BME280_SPEC: &str = include_str!("../../../../docs/hunts/specs/bme280.toml");
    /// The canonical shipped MPU6050 spec.
    const MPU6050_SPEC: &str = include_str!("../../../../docs/hunts/specs/mpu6050.toml");

    /// Pointered burst read of `n` bytes starting at register `reg` from the I2C
    /// slave at 7-bit `addr`, through the real bus dispatch path.
    fn i2c_read_burst(bus: &mut I2cBus, addr: u8, reg: u8, n: usize) -> Vec<u8> {
        bus.dispatch(I2cEvent::Start { addr, read: false });
        bus.dispatch(I2cEvent::Write { addr, data: reg });
        bus.dispatch(I2cEvent::Start { addr, read: true });
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(bus.dispatch(I2cEvent::Read { addr }).unwrap());
        }
        bus.dispatch(I2cEvent::Stop { addr });
        out
    }

    // ── Bosch BME280/BMP280 integer compensation (datasheet §4.2.3, §8.2) ─────
    // Direct ports of the datasheet reference C. `wrapping_*` reproduces C's
    // modular int32 semantics exactly (the routines are designed not to wrap for
    // valid inputs, so this is faithful, not lossy). These run in the FIXTURE —
    // they are the raw→physical consumer the model deliberately does not embed.

    /// Returns `(t_fine, T)` where `T` is temperature in 0.01 °C.
    fn bme280_compensate_t(adc_t: i32, t1: i32, t2: i32, t3: i32) -> (i32, i32) {
        let var1 = (((adc_t >> 3) - (t1 << 1)).wrapping_mul(t2)) >> 11;
        let d = (adc_t >> 4) - t1;
        let var2 = ((d.wrapping_mul(d) >> 12).wrapping_mul(t3)) >> 14;
        let t_fine = var1 + var2;
        let t = (t_fine * 5 + 128) >> 8;
        (t_fine, t)
    }

    /// Returns pressure in Pa (int32 routine).
    #[allow(clippy::too_many_arguments)]
    fn bme280_compensate_p(
        adc_p: i32, t_fine: i32,
        p1: i32, p2: i32, p3: i32, p4: i32, p5: i32, p6: i32, p7: i32, p8: i32, p9: i32,
    ) -> u32 {
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
        let mut p: u32 =
            ((1_048_576 - adc_p) as u32).wrapping_sub((var2 >> 12) as u32).wrapping_mul(3125);
        if p < 0x8000_0000 {
            p = (p << 1) / (var1 as u32);
        } else {
            p = (p / (var1 as u32)) * 2;
        }
        let w1 = p9.wrapping_mul((((p >> 3) * (p >> 3)) >> 13) as i32) >> 12;
        let w2 = ((p >> 2) as i32).wrapping_mul(p8) >> 13;
        ((p as i32) + ((w1 + w2 + p7) >> 4)) as u32
    }

    /// Returns humidity in Q22.10 %RH (divide by 1024 for %RH).
    #[allow(clippy::too_many_arguments)]
    fn bme280_compensate_h(
        adc_h: i32, t_fine: i32,
        h1: i32, h2: i32, h3: i32, h4: i32, h5: i32, h6: i32,
    ) -> u32 {
        let v0 = t_fine - 76800;
        let term_a = (((adc_h << 14) - (h4 << 20) - h5.wrapping_mul(v0)) + 16384) >> 15;
        let y1 = v0.wrapping_mul(h6) >> 10;
        let y2 = (v0.wrapping_mul(h3) >> 11) + 32768;
        let y3 = y1.wrapping_mul(y2) >> 10;
        let y4 = (y3 + 2_097_152).wrapping_mul(h2) + 8192;
        let big = y4 >> 14;
        let mut v = term_a.wrapping_mul(big);
        v = v - ((((v >> 15).wrapping_mul(v >> 15) >> 7).wrapping_mul(h1)) >> 4);
        let v = v.clamp(0, 419_430_400);
        (v >> 12) as u32
    }

    /// FIXTURE: BME280 chip-ID gate reads 0x60 through the bound I2C bus, and a
    /// burst read of the raw data registers decodes — via the datasheet Bosch
    /// compensation — to the datasheet Appendix worked-example physical values.
    ///
    /// Authority: the trimming + raw ADC inputs are the Bosch datasheet Appendix
    /// (§8.2) worked example; the compensated temperature 25.08 °C is the exact
    /// datasheet-published result. The pressure 100656 Pa is the int32 routine's
    /// result on the same inputs (the int64 Q24.8 routine gives 100653.25 Pa; the
    /// ~3 Pa gap is documented int32 precision loss). Humidity uses realistic
    /// BME280 trimming (dig_H4=317, others per the spec) → 54.45 %RH.
    #[test]
    fn declarative_bme280_decodes_datasheet_worked_example() {
        let sensor = RegisterMapSensor::from_toml(BME280_SPEC).unwrap();
        let addr = 0x76;
        let mut bus = I2cBus::new("I2C").with_slave(Box::new(sensor));

        // Identity gate (datasheet §5.4.1): CHIP_ID 0xD0 == 0x60 (BME280).
        let id = i2c_read_burst(&mut bus, addr, 0xD0, 1);
        assert_eq!(id, vec![0x60], "BME280 chip-ID must be 0x60");

        // Calibration blocks: 26 bytes from 0x88, 7 bytes from 0xE1.
        let cal1 = i2c_read_burst(&mut bus, addr, 0x88, 26);
        let cal2 = i2c_read_burst(&mut bus, addr, 0xE1, 7);
        let le16 = |b: &[u8], i: usize| i16::from_le_bytes([b[i], b[i + 1]]) as i32;
        let leu16 = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as i32;
        let (t1, t2, t3) = (leu16(&cal1, 0), le16(&cal1, 2), le16(&cal1, 4));
        let p1 = leu16(&cal1, 6);
        let (p2, p3, p4) = (le16(&cal1, 8), le16(&cal1, 10), le16(&cal1, 12));
        let (p5, p6, p7) = (le16(&cal1, 14), le16(&cal1, 16), le16(&cal1, 18));
        let (p8, p9) = (le16(&cal1, 20), le16(&cal1, 22));
        let h1 = cal1[25] as i32; // 0xA1
        let h2 = le16(&cal2, 0); // 0xE1/0xE2
        let h3 = cal2[2] as i32; // 0xE3
        // dig_H4/H5 nibble packing (datasheet §4.2.2):
        let h4 = ((cal2[3] as i8 as i32) << 4) | (cal2[4] & 0x0F) as i32;
        let h5 = ((cal2[5] as i8 as i32) << 4) | ((cal2[4] >> 4) as i32);
        let h6 = cal2[6] as i8 as i32;
        // Full coefficient round-trip through the bus. Covering EVERY pressure
        // coefficient here (not just p1) is deliberate: a wrong two's-complement
        // byte in the spec (e.g. dig_P2/dig_P8) would otherwise only surface as a
        // small pressure error downstream — this pins each to its datasheet value.
        assert_eq!(
            (t1, t2, t3),
            (27504, 26435, -1000),
            "temperature trimming must round-trip to the datasheet values"
        );
        assert_eq!(
            (p1, p2, p3, p4, p5, p6, p7, p8, p9),
            (36477, -10685, 3024, 2855, 140, -7, 15500, -14600, 6000),
            "pressure trimming must round-trip to the datasheet values"
        );
        assert_eq!(
            (h1, h2, h3, h4, h5, h6),
            (75, 362, 0, 317, 0, 30),
            "humidity trimming (incl. H4/H5 nibble packing) must round-trip"
        );

        // Raw data burst: 8 bytes from 0xF7 = press[3] temp[3] hum[2].
        let d = i2c_read_burst(&mut bus, addr, 0xF7, 8);
        assert_eq!(
            &d[0..3],
            &[0x65, 0x5A, 0xC0],
            "press bytes must be the u20_be_xlsb packing of 415148"
        );
        assert_eq!(
            &d[3..6],
            &[0x7E, 0xED, 0x00],
            "temp bytes must be the u20_be_xlsb packing of 519888"
        );
        let adc_p = ((d[0] as i32) << 12) | ((d[1] as i32) << 4) | ((d[2] as i32) >> 4);
        let adc_t = ((d[3] as i32) << 12) | ((d[4] as i32) << 4) | ((d[5] as i32) >> 4);
        let adc_h = ((d[6] as i32) << 8) | (d[7] as i32);
        assert_eq!((adc_p, adc_t, adc_h), (415148, 519888, 30000));

        // Compensate (the firmware/test consumer path).
        let (t_fine, t) = bme280_compensate_t(adc_t, t1, t2, t3);
        assert_eq!(t_fine, 128422, "t_fine (datasheet Appendix) must be 128422");
        assert_eq!(t, 2508, "temperature must be 25.08 °C (datasheet published)");

        let pa = bme280_compensate_p(adc_p, t_fine, p1, p2, p3, p4, p5, p6, p7, p8, p9);
        assert_eq!(pa, 100656, "pressure must be 100656 Pa (int32 routine)");

        let h_q = bme280_compensate_h(adc_h, t_fine, h1, h2, h3, h4, h5, h6);
        assert_eq!(h_q, 55759, "humidity must be 55759 (Q22.10)");
        let rh = h_q as f64 / 1024.0;
        assert!((54.0..55.0).contains(&rh), "humidity ≈ 54.45 %RH, got {rh}");
    }

    /// FIXTURE: MPU6050 WHO_AM_I reads 0x68 through the bound I2C bus, and a
    /// burst read of the data registers decodes — via the linear scale factors —
    /// to the driven physical quantities (accel Z = +1 g, gyro X = 250 °/s,
    /// temp = 25 °C). Here the whole forward map is expressible as evalexpr
    /// value expressions (`expr * scale + offset`), so no encoding addition is
    /// needed; the fixture proves the round trip physical → raw → physical.
    #[test]
    fn declarative_mpu6050_decodes_driven_quantities() {
        let mut sensor = RegisterMapSensor::from_toml(MPU6050_SPEC).unwrap();
        sensor.set_input("accel_z_g", 1.0);
        sensor.set_input("gyro_x_dps", 250.0);
        sensor.set_input("temp_c", 25.0);
        let addr = 0x68;
        let mut bus = I2cBus::new("I2C").with_slave(Box::new(sensor));

        // Identity gate (register map §4.32): WHO_AM_I 0x75 == 0x68.
        let who = i2c_read_burst(&mut bus, addr, 0x75, 1);
        assert_eq!(who, vec![0x68], "MPU6050 WHO_AM_I must be 0x68");

        // Data burst: 14 bytes from 0x3B = accel XYZ (6) temp (2) gyro XYZ (6).
        let d = i2c_read_burst(&mut bus, addr, 0x3B, 14);
        let be = |b: &[u8], i: usize| i16::from_be_bytes([b[i], b[i + 1]]);
        let ax = be(&d, 0);
        let ay = be(&d, 2);
        let az = be(&d, 4);
        let temp_raw = be(&d, 6);
        let gx = be(&d, 8);

        // ±2 g full scale → 16384 LSB/g. Z = +1 g → 0x4000.
        assert_eq!(az, 16384, "accel Z = +1 g must be 16384 LSB");
        assert_eq!([d[4], d[5]], [0x40, 0x00], "accel Z bytes big-endian");
        assert_eq!((ax, ay), (0, 0), "accel X/Y at rest = 0");
        // ±250 °/s → 131 LSB/(°/s). X = 250 °/s → 32750.
        assert_eq!(gx, 32750, "gyro X = 250 °/s must be 32750 LSB");
        // Temp: T = raw/340 + 36.53.
        let temp_c = temp_raw as f64 / 340.0 + 36.53;
        assert!(
            (temp_c - 25.0).abs() < 0.05,
            "decoded temperature must be ≈ 25 °C, got {temp_c}"
        );
    }

    /// FIXTURE (SPI address-byte convention): the SAME BME280 register map over
    /// SPI resolves the chip-ID at its RAW datasheet address 0xD0. The command
    /// byte folds the R/W flag into bit 7 (0xD0 = read | addr 0x50); the
    /// interpreter masks it off both the command and the stored key, so the raw
    /// datasheet address is what the spec declares. Documents item-2's SPI note.
    #[test]
    fn declarative_bme280_spi_chip_id() {
        // Minimal SPI variant of the BME280 register map (same addresses).
        let spi = r#"
[sensor]
name = "BME280"
bus  = "spi"

[[sensor.register]]
addr  = 0xD0
const = [0x60]

[sensor.protocol]
style           = "spi_reg"
rw_read_is_high = true
addr_mask       = 0x7f
"#;
        let sensor = RegisterMapSensor::from_toml(spi).unwrap();
        let mut bus = SpiBus::new("SPI", Box::new(sensor));
        let _status = bus.transfer(0xD0); // read bit 7 set | masked addr 0x50
        let id = bus.transfer(0x00);
        assert_eq!(id, 0x60, "BME280 SPI chip-ID at raw 0xD0 must read 0x60");
    }
}
