# Peripherals

Peripherals are the interactive layer over a board: things you attach at runtime
that act as inputs, outputs, and controls during a co-simulation. They are not
parts on the board model. They are the pushbutton you press, the potentiometer
you turn, the I2C sensor the firmware polls, the logic analyser recording a net.

A peripheral either drives/reads a circuit net (controls, sinks) or speaks a
digital bus to the emulated MCU (I2C/SPI slaves). All of them plug into the same
co-sim loop through one trait, so the scheduler ticks them uniformly.

## The `Peripheral` trait

`hauksbee_engine::peripherals::Peripheral` is what the scheduler drives. The key
methods, called once per co-sim chunk:

```
pre_solve(ctx)    push a commanded level onto a net BEFORE the analog solve
post_solve(ctx)   sample the freshly-solved node voltages AFTER the solve
set_value(v)      apply a live command (button press, pot position, ...)
state()           numeric readout for frames / assertions
```

`TickCtx` hands the peripheral the circuit (so a control can mutate its stamped
source), the latest solved node voltages, the sim time, and the chunk length.

Controls drive nets the same way the power supplies and GPIO drivers already do:
an ideal `Vsource` behind a series resistor, or a switchable contact resistor.
Updating the control between chunks is just mutating that device's value; MNA
resolves contention with everything else on the net. This is the Tarski trick
the whole engine is built on, reused: behavioral sources updated between chunks,
the linear solve unchanged.

A `PeripheralSet` owns the attached peripherals plus a time-sorted **timeline**
of events. Each chunk the scheduler fires any due events (`press BTN1 at 100ms`)
then ticks `pre_solve`, solves, then ticks `post_solve`.

### Three classes

| Class | Examples | How it couples |
|-------|----------|----------------|
| Analog / contact controls | pushbutton, toggle, potentiometer, rotary encoder, stimulus | drive/observe a net (Thevenin source or contact resistor) |
| Digital bus slaves | 24Cxx I2C EEPROM, LM75 I2C sensor, 25xx SPI EEPROM, MCP3008 SPI ADC | answer the MCU's `on_i2c` / `on_spi` hooks |
| Output sinks | VCD logger | observe nets, record to a file |

## Analog / contact controls

All attach by **net name** or by **connector ref + pin** (the ref+pin is resolved
to the pin's net at bind time, so the control only ever sees a node).

- **Pushbutton**: momentary contact between a net and a reference (default
  ground). Released = open, pressed = closed (1 Ω). Optional **contact-bounce
  model**: a press or release chatters open/closed for `bounce_ms` before
  settling, so debounce firmware is actually exercised.
- **Toggle switch**: latching SPST; holds its state, idealised as debounced.
- **Potentiometer**: three-terminal (`a`, wiper, `b`) with a `position` in
  0..1 splitting the track resistance between the two legs. Wire `a` to a rail
  and `b` to ground and the wiper is an analog input the firmware can read.
- **Rotary encoder**: drives two nets with a quadrature A/B Gray-code sequence
  from a commanded detent position. Stepping the position advances the phase one
  quadrature edge per detent, in the right direction.
- **Stimulus**: a generic voltage or current source: DC, sine, PWL, or noise.
  Voltage stimulus sits behind a 50 Ω series resistor (measurable, never a hard
  short); current stimulus injects into the net.

Live control: `set_value(v)` interprets `v` per kind (button: `>=0.5` pressed;
pot/encoder: position; stimulus: DC level).

## Digital bus slaves

These let firmware talk to realistic parts that are **not on the board model**,
or stand in for bound-but-unmodelled board parts.

### I2C

`I2cBus` is a router: it owns one or more `I2cSlave`s and is registered as the
MCU's `on_i2c` callback. Each bus event is dispatched to the slave whose 7-bit
address matches. Two concrete devices:

- **24Cxx EEPROM** (`Eeprom24c`); 16-bit word address, auto-incrementing
  page writes and current-address reads. Backing memory is readable for
  assertions (`contains(bytes)`).
- **LM75 temperature sensor** (`Lm75`); the real datasheet register map:
  pointer register selects Temp (0x00) / Conf (0x01) / Thyst (0x02) / Tos
  (0x03). The temperature register encodes T in **0.125 °C/LSB, left-justified,
  big-endian** (the LM75A 11-bit format, which 9-bit reads also accept). The
  reported temperature is configurable and live-settable.

### SPI

`SpiBus` owns one `SpiSlave` (simavr does not surface chip-select, so one active
slave per bus). Two concrete devices:

- **25xx EEPROM** (`Spi25Eeprom`); WREN/WRDI/RDSR/READ/WRITE instruction set,
  16-bit addressing, write-enable latch. Memory readable for assertions.
- **MCP3008 ADC** (`Mcp3008`); 8-channel 10-bit ADC; per-channel input voltage
  is settable and converted to counts on the standard 3-byte transfer.

### Master reads on the AVR TWI hook

Slaves are readable as well as writable: the TWI
hook handles `TWI_COND_READ` by asking the slave for its byte and raising
the TWI input IRQ with `READ | ACK` carrying the data, and `I2cEvent` has a
`Read { addr }` variant for this path. The integration proof reads an LM75 and
decodes real temperature bytes end to end.

### Declarative register-map devices (and the write side)

Hand-coding each bus device in Rust does not scale, so a device can instead be
a **TOML spec** (`testdata/sensor-specs/*.toml`, schema in
`hauksbee-models/src/sensor_spec.rs`) that the generic `RegisterMapSensor`
interpreter realizes as a live `I2cSlave`/`SpiSlave`. The read side maps
physical inputs through `evalexpr` value expressions into datasheet register
packings (BME280, MPU6050, LM75 are byte-identical to hand-coded models).

The spec also describes the **write side**: what firmware
writes do:

- **Pointer-framed write registers** decode into stored variables that read
  expressions reference, so a config write changes what a later read returns.
  ADS1115 (config selects mux/PGA for the conversion register) and INA219
  (calibration feeds the current/power math) are the shipped proofs, each
  fixture-anchored to datasheet worked-example numbers.
- **Command-framed writes** (the MCP4728 shape: mask-matched command byte,
  fixed-size data groups, per-channel state) update state whose **output
  voltage laws drive analog nets**. The MCP4728 is pure data
  (`testdata/sensor-specs/mcp4728.toml`); the scheduler binds the binder-stamped
  VOUT `PinDriver`s to the spec's outputs, and the slave drives them itself in
  the ctx-bearing `on_stop`, delivered once per chunk, after the
  MCU ran and before the analog solve, because the byte events arrive inside
  the MCU callback where no circuit context exists (and the solve could not
  see a mid-chunk voltage anyway).
- **Bit extraction is framing-layer data**, never expression math: `evalexpr`
  has no bit operations, so fields are declared `[high, low]` bit ranges the
  interpreter extracts in Rust. Everything downstream (state updates, voltage
  laws, read-back frames) is expressions over the extracted names.

What the write side does **not** do, stated rather than faked:

- **SPI writes are accept-and-ignore** (validation rejects SPI write blocks;
  the ignored bytes are counted). No shipped device needs them yet.
- **No timing.** Conversion/update latency is not modeled: an ADS1115
  single-shot completes instantly, a DAC write lands at the next chunk solve.
- **Undeclared writes are ACKed and counted** (`ignored_write_bytes` in the
  slave's `state()`), like a real part ACKing a command family the model
  omits, an eaten config write is observable, never silent.
- **SSD1306 is deferred (design sketch).** A display is a write-only
  command/data STREAM, not a register map: the control byte (0x00 command /
  0x40 data) selects an interpreter, commands set an addressing-mode cursor,
  and data bytes fill a 128×64 framebuffer, a **sink with megabytes of
  addressable state**, not per-channel scalars with a voltage law. Forcing it
  into `state`/`output` would mean thousands of fake "channels" and no net to
  drive. The honest shape is a third device class: `write_command`-style
  framing feeding a `framebuffer` sink block (page/column cursor semantics in
  Rust, dimensions and command opcodes as data) with assertions over pixel
  regions instead of net voltages. That lands with the W5 extensibility SDK
  rather than being bent into the net-driving schema here.

### Bus-speed honesty (chunk-rate limits)

Interception is at the **byte / transaction level** through simavr's hardware
TWI and SPI peripheral models, not by sampling SCL/SDA/SCK edges. simavr clocks
each bus byte internally and raises one IRQ per byte/condition, all consumed
inside a single `run_micros` chunk. So:

- The **achievable bus speed is whatever the firmware's prescaler asks for**
  (I2C 100 kHz / 400 kHz, SPI at the SPR/SPI2X rate). It is **not** bounded by
  the analog chunk rate, because the bytes never cross a chunk boundary.
- A **bit-banged** master (software toggling SCL/SDA or SCK/MOSI on GPIO) is a
  different story: those edges alias at the chunk poll rate exactly like any
  other GPIO (see `docs/cosim/MCU.md`). Bit-banged MHz bus traffic is **not** resolved.
  This framework targets the hardware TWI/SPI peripherals.
- The **Renode `on_i2c` / `on_spi` hooks are wired** through generated C#
  bridge peripherals (an `II2CPeripheral` / `ISPIPeripheral` per slave address,
  loaded into the running machine over the Monitor). A hardware-TWI/SPI sensor
  therefore co-simulates on the Renode ARM/RISC-V backends the same way it does
  on simavr, see the `i2c_sensor_cosim_renode` / `spi_sensor_cosim_renode`
  integration tests. (Bit-banged masters are still the exception above.)

## Output sinks

- **VCD sink** (`VcdSink`); samples a chosen set of nets after every solve,
  decides each one's logic level with thresholds + hysteresis, and records a
  timestamped change on every flip. `write()` emits a **gtkwave-compatible**
  Value Change Dump (IEEE 1364, 1 ps timescale). It composes with everything:
  any net the firmware or another peripheral drives can be logged without
  touching it.

## Live control over the websocket

The protocol extension is **additive and backward compatible** (the existing
frontend keeps working unchanged):

- `BoardInfo.peripherals: [{id, kind}]`; the attached peripherals, so a UI can
  build controls for them.
- `ClientMessage::SetPeripheral { id, value }`; live-control a peripheral.
  `SetInput { source, value }` is also routed to a peripheral of that id as a
  fallback, so a frontend slider wired to a peripheral id works with no change.
- Peripheral state is folded into `SimFrame.component_states` keyed by id
  (e.g. `{"pressed":1}`, `{"position":0.5}`, `{"temp_c":40}`,
  `{"transitions":20}`).

## hauksbee-ci: the `[[peripheral]]` spec section

A spec attaches peripherals for a headless run, with type-specific config and a
timeline of events. Assertions can reference peripheral state.

```toml
[[peripheral]]
id = "BTN1"
type = "pushbutton"        # pushbutton|toggle|potentiometer|encoder|stimulus|
                           # i2c_eeprom|i2c_lm75|spi_eeprom|spi_mcp3008|vcd_sink
net = "BUTTON"             # attach by net name ...
# ref = "J1"               # ... or by connector ref + pin
# pin = "3"
to = "GND"                 # button/toggle: other terminal (default GND)
bounce_ms = 5.0            # optional contact-bounce model

[[peripheral.event]]       # timeline: press at 100ms, release at 150ms
t_ms = 100
value = 1
[[peripheral.event]]
t_ms = 150
value = 0

[[peripheral]]
id = "U2"
type = "i2c_lm75"
address = 0x48
temp_c = 40.0

[[peripheral]]
id = "VCD"
type = "vcd_sink"
nets = ["CLK", "DATA"]
vcd_path = "out/trace.vcd"
```

### Peripheral assertions

```toml
# EEPROM contents contain bytes (hex or ASCII).
[[assert]]
kind = "peripheral"
id = "U2"
bytes = "48 69"            # or bytes = "Hi"

# A peripheral state field is in range (transitions, temp_c, position, ...).
[[assert]]
kind = "peripheral"
id = "VCD"
field = "transitions"
min = 15
max = 25
```

The `field` name is per peripheral `type`, each exposes its own state keys.
The full vocabulary (matched exactly; an unknown field errors at load):

| peripheral `type` | `field` keys | meaning |
|---|---|---|
| `pushbutton` | `pressed` | 1 while held, else 0 |
| `toggle` | `closed` | 1 when closed, else 0 |
| `potentiometer` | `position` | wiper fraction 0..1 |
| `encoder` | `detents`, `a`, `b` | accumulated detents; the two quadrature line levels |
| `stimulus` | `value` | the last driven value |
| `vcd_sink` | `transitions`, `nets` | edge count captured; number of nets watched |
| `load` | `current_a`, `peak_a` | instantaneous and peak sink current |
| `i2c_eeprom` | `size`, `ptr`, `page_size` | byte capacity; current address pointer; page size |
| `i2c_lm75` | `temp_c`, `pointer` | configured temperature; register pointer |
| `spi_eeprom` | `size`, `wel` | byte capacity; write-enable-latch state |
| `spi_mcp3008` | `vref`, `ch0`..`ch7` | reference voltage; per-channel input voltage |

(For `i2c_eeprom` / `spi_eeprom` contents, prefer the `bytes = "..."` form above
over a `field`.) The keys are produced by each peripheral's `state()` in
`crates/hauksbee-engine/src/peripherals/`.

## Proofs (integration tests)

1. **I2C temperature sensor co-sim** -
   `crates/hauksbee-engine/tests/i2c_sensor_cosim.rs` (avr feature).
   AVR firmware (`testdata/firmware/i2c_thermostat`) reads the LM75 over hardware
   TWI and drives PB0 from the temperature vs a 30 °C threshold. The test sweeps
   the configured temperature `[10, 25, 29, 31, 35, 50, 28, 15] °C` and asserts
   the GPIO (net `FLAG`) is HIGH exactly when `T >= 30 °C`. The firmware prints
   the decoded temperature over UART, confirming the master-read path returns the
   real datasheet-encoded bytes.
2. **CI button press drives a net** -
   `testdata/ci/button_press.toml` (run by `crates/hauksbee-ci/tests/peripherals.rs`).
   A pushbutton is pressed at 100 ms and released at 150 ms on a net pulled to
   +5 V through 10 kΩ. Asserts the net settles back high after release and
   toggles exactly twice from the timed press/release.
3. **VCD sink**: `crates/hauksbee-ci/tests/peripherals.rs` and the
   `peripherals::sink` unit test. A timed PWL square wave drives a net; the sink
   logs it; the written VCD is validated for a 1 ps timescale, a wire
   declaration, and ~20 known transitions.

## Honest limitations

- **Bus slaves couple on every hardware-peripheral backend.** simavr (AVR TWI/
  SPI), the Renode ARM/RISC-V backends (generated C# bridge peripherals), and the
  QEMU ESP32 backend (RAM-mailbox bus cells) all route a slave model's traffic;
  the framework is backend-agnostic at the trait level.
- **Hardware peripheral, not bit-bang.** Byte-level interception means a
  software bit-banged bus master is not resolved (its edges alias at the chunk
  rate, like any GPIO). Use the firmware's TWI/SPI peripheral.
- **SPI chip-select is inferred.** simavr does not surface CS, so the bus treats
  the co-sim chunk boundary as a CS deassert (resets the slave's command state
  machine). Well-formed transfers complete within a chunk, so this is correct in
  practice; pathological multi-chunk transfers with no idle gap could mis-frame.
- **One SPI slave per bus** (no CS to select among several).
- **The declarative write side is I2C-only and untimed** (see the section
  above): SPI write phases and conversion/update latency are stated
  non-features, and unmodeled write bytes are counted, not decoded.
- **Contact bounce is a deterministic chatter model**, not a measured
  statistical bounce profile: a fixed ~5-cycle open/close burst across the
  configured window. It is enough to exercise debounce logic, not to reproduce a
  specific switch's bounce signature.
