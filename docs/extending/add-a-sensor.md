# Add a sensor: BME280, from datasheet to passing fixture

**Goal.** Model an I2C (or SPI) register-map sensor purely as data: one
`[sensor]` TOML file that the generic engine interpreter
(`RegisterMapSensor` in `crates/hauksbee-engine/src/peripherals/register_map.rs`)
realizes as a bus slave. No Rust. The worked example is the Bosch BME280
(humidity/pressure/temperature); the shipped spec this walkthrough retraces is
`docs/hunts/specs/bme280.toml`.

**What you need:** the part's datasheet (register map + electrical worked
example), and `hauksbee models lint`.

## How the model works

A register-map sensor answers firmware reads from a table of registers. The
spec declares three things:

1. **Inputs**: named physical quantities the simulation drives
   (`temperature_c`, or raw ADC counts, see the design note below).
2. **Registers**: address → either constant bytes (`const`, for chip IDs and
   config registers) or a value: an `expr` over the inputs plus an `encoding`
   that packs the number into wire bytes.
3. **Protocol**: how the firmware addresses registers (`i2c_pointer`:
   write the register pointer, read N bytes, auto-increment; or `spi_reg`:
   command byte = R/W bit + address).

The schema and validator live in
`crates/hauksbee-models/src/sensor_spec.rs`; the engine evaluates expressions
and speaks the bus. The spec is the shared contract both sides validate
against.

## Step 1, bus, address, and the input decision

```toml
[sensor]
name        = "BME280"
bus         = "i2c"
i2c_address = 0x76   # SDO=GND. Use 0x77 for SDO=VDDIO.
```

Then decide what your inputs *are*. For a linear part (LM75: temperature in,
scaled integer out) the input is the physical quantity and `expr` is the
datasheet's forward formula. The BME280 cannot do that, and the reason is worth
understanding because you will hit it on other parts:

> **Why the BME280 exposes raw ADC counts, not °C/Pa.** The spec's `expr`
> mechanism is *forward*: physical input → the number packed into register
> bytes. The BME280 datasheet gives the *reverse* (raw → physical Bosch
> compensation), it is non-invertible per register (pressure and humidity both
> depend on `t_fine`, derived from the temperature ADC, coupled, not
> per-register), and the fixed-point routine needs integer bit ops `evalexpr`
> does not have. So the shipped spec's inputs are `adc_press`, `adc_temp`,
> `adc_hum`, the natural register-map quantities, and the raw→physical
> compensation is applied by the *consumer* (the firmware in a real co-sim,
> the fixture test otherwise). The full rationale is the header comment of
> `docs/hunts/specs/bme280.toml`.

```toml
[[sensor.input]]
name    = "adc_temp"
default = 519888.0   # datasheet worked example → t_fine=128422 → 25.08 °C
```

Defaults matter: pick the datasheet's worked-example values so the sensor is
in a known-good state before any test drives it.

## Step 2, identity and control registers (const bytes)

Everything the firmware reads but your model doesn't compute is a `const`
register. The chip ID is the one you cannot skip, drivers hard-gate on it:

```toml
[[sensor.register]]
addr  = 0xD0
const = [0x60]       # BME280 = 0x60; a BMP280 reports 0x58 here
```

Same pattern for `reset` (0xE0), `ctrl_hum` (0xF2), `status` (0xF3),
`ctrl_meas` (0xF4), `config` (0xF5).

## Step 3, calibration blocks, one const byte per address

The BME280 firmware burst-reads 26 calibration bytes from 0x88. In
`i2c_pointer` protocol a burst auto-increments across the register map, so the
calibration block is declared as **one const register per byte address**, with
no gaps (the shipped spec even declares the reserved 0xA0 so the burst stays
contiguous):

```toml
[[sensor.register]]
addr  = 0x88   # dig_T1 lo (27504 = 0x6B70)
const = [0x70]
[[sensor.register]]
addr  = 0x89   # dig_T1 hi
const = [0x6B]
# ... through 0xA1, then the humidity block 0xE1..0xE7
```

Use the datasheet Appendix's example trimming values, and note each byte's
meaning in a comment; the fixture test in step 6 depends on these exact
numbers.

## Step 4, data registers: encoding + expr

A multi-byte value is **one** register with a multi-byte encoding; the burst
covers its interior bytes via the register's read length:

```toml
[[sensor.register]]
addr     = 0xF7   # press_msb/lsb/xlsb — 20-bit raw ADC pressure count
encoding = "u20_be_xlsb"
expr     = "adc_press"

[[sensor.register]]
addr     = 0xFD   # hum_msb/lsb — 16-bit raw ADC humidity count
encoding = "u16_be"
expr     = "adc_hum"
```

Available encodings (`Encoding` in `sensor_spec.rs`): `u8`, `u16_be`,
`u16_le`, `i16_be`, `i16_le`, `q7.1_be` (the LM75A packing), `u20_be_xlsb`
(the Bosch 20-bit MSB/LSB/XLSB frame, added *for* the BME280), and `raw`
(const-only). `scale`/`offset` apply a linear pre-scale before encoding,
`encoded_value = expr * scale + offset`, e.g. a register that stores
temperature as `T * 100 + 4000` (the SHT31-style offset-centigrade packing):

```toml
[[sensor.register]]
addr     = 0x00
encoding = "u16_be"
expr     = "temp_c"
scale    = 100.0
offset   = 4000.0
```

If
your part needs a packing none of these produce, that is a small Rust addition
to `Encoding`; the `u20_be_xlsb` doc comment is the template for justifying
one.

Finish with the protocol block:

```toml
[sensor.protocol]
style = "i2c_pointer"
```

(For the SPI variant of the same part: `bus = "spi"`, drop `i2c_address`, use
`style = "spi_reg"` with `rw_read_is_high`/`addr_mask`, and keep the **raw
datasheet register addresses**; the interpreter masks the R/W bit off both
sides. See `docs/hunts/specs/bmp280.toml` for a full worked SPI spec.)

## Step 5, lint it

```
cargo run -p hauksbee-engine --bin hauksbee -- models lint docs/hunts/specs/bme280.toml
```

Green looks like:

```
sensor 'BME280': ok
1 item(s) checked, 0 finding(s) — clean
```

The validator catches: duplicate addresses (post-mask for SPI), an `expr`
referencing an undeclared input, `bytes` disagreeing with the encoding's
natural width, a register that is both const and expr, missing `i2c_address`,
an SPI `addr_mask` that includes bit 7. Every failure is a named message, so
fix what it says and re-run.

**Trap, evalexpr equality is type-strict.** Every spec variable is bound as a
float, and `evalexpr`'s `==` never equates float with integer: `pd == 0` is
*false* even when `pd` is 0.0. Always write float literals in expressions:
`pd == 0.0`, `if(gain_bit == 1.0, …)`. This is documented (with the rationale
for why bit-field extraction is data, not expressions) in the write-side
section of `sensor_spec.rs`.

## Step 6; the test that proves it

The proving pattern is a **datasheet-anchored fixture**: drive the spec's
inputs with the datasheet's worked-example values, read the registers the way
firmware would (burst reads through the interpreter), run the datasheet's own
raw→physical math on the bytes, and assert the physical answer the datasheet
prints. The shipped BME280 fixture
(`declarative_bme280_decodes_datasheet_worked_example` in
`crates/hauksbee-engine/src/peripherals/register_map.rs`) does exactly this:
it `include_str!`s the real spec file (so the test pins the shipped data, not
a copy), reads the calibration burst and the 0xF7 data burst, runs the Bosch
int32 compensation, and asserts 25.08 °C / 100656 Pa.

```
cargo test -p hauksbee-engine declarative_bme280
```

Green looks like:

```
test peripherals::register_map::tests::declarative_bme280_decodes_datasheet_worked_example ... ok
test peripherals::register_map::tests::declarative_bme280_spi_chip_id ... ok
```

For your own sensor, write the same shape: cite the datasheet section next to
every constant, and make the assertion a number printed *in the datasheet*,
not a number your model produced once. A fixture that asserts the model
against itself proves nothing.

One honest caveat: this closing proof pattern is a Rust test, so it needs a
hauksbee **checkout** to run in. The data-only promise holds for *using* the
part, writing the spec, `hauksbee models lint`, `models resolve`, and the
co-sim attaching it at runtime need no checkout, but pinning it with a
datasheet-anchored test the way the shipped parts are pinned does.

## Step 7, use it in a co-sim

A CI spec attaches declarative sensors per run
(`crates/hauksbee-ci/src/spec.rs`, `SensorAttach`):

```toml
[[sensor]]
id        = "bme0"
spec_file = "sensors/bme280.toml"   # relative to the CI spec file
[sensor.inputs]
adc_temp = 519888.0                 # override any input's default
```

The runner parses the spec, wires it onto the emulated MCU's I2C bus, and your
firmware talks to it exactly as it would to the real part. See
[docs/cosim/PERIPHERALS.md](../cosim/PERIPHERALS.md) for the layer this plugs into.

## Beyond read-only sensors

Parts the firmware *writes* (ADC config registers, DAC codes) use the write
side of the same schema: `[[sensor.write_register]]` (pointer-framed, with
declared bit fields), `[[sensor.write_command]]` (command-framed, the MCP4728
shape), `[[sensor.state]]` + `[[sensor.output]]` (per-channel state driving an
analog net through a voltage law). The shipped `ads1115.toml`, `ina219.toml`,
and `mcp4728.toml` specs in `docs/hunts/specs/` are the worked examples, and
the write side is currently modeled for I2C only, an SPI spec with write
blocks is *rejected* by validation rather than silently mis-parsed (a stated
limitation, not a capability).

---

Next: [add-a-logic-ic.md](add-a-logic-ic.md) for digital parts, or
[make-a-model-pack.md](make-a-model-pack.md) to ship what you built.
