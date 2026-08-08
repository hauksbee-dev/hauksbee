# Peripherals

Peripherals are the interactive layer over a board: things you attach at
runtime that act as inputs, outputs, and controls during a co-simulation.
They are not parts on the board model. They are the pushbutton you press, the
potentiometer you turn, the I2C sensor the firmware polls, the logic analyser
recording a net.

A peripheral either drives/reads a circuit net (controls, sinks) or speaks a
digital bus to the emulated MCU (I2C/SPI slaves). All of them plug into the
same co-sim loop through one trait, so the scheduler ticks them uniformly.

## The `Peripheral` trait

`hauksbee_engine::peripherals::Peripheral` is what the scheduler drives. The
key methods, called once per co-sim chunk:

```
pre_solve(ctx)    push a commanded level onto a net BEFORE the analog solve
post_solve(ctx)   sample the freshly-solved node voltages AFTER the solve
set_value(v)      apply a live command (button press, pot position, ...)
state()           numeric readout for frames / assertions
```

`TickCtx` hands the peripheral the circuit (so a control can mutate its
stamped source), the latest solved node voltages, the sim time, and the
chunk length.

Controls drive nets the same way the power supplies and GPIO drivers already
do: an ideal `Vsource` behind a series resistor, or a switchable contact
resistor. Updating the control between chunks just mutates that device's
value. MNA resolves contention with everything else on the net. This is the
Tarski trick the whole engine builds on, reused here: behavioral sources
update between chunks, and the linear solve stays unchanged.

A `PeripheralSet` owns the attached peripherals plus a time-sorted
**timeline** of events. Each chunk, the scheduler fires any due events
(`press BTN1 at 100ms`), then ticks `pre_solve`, solves, then ticks
`post_solve`.

### Three classes

| Class | Examples | How it couples |
|-------|----------|----------------|
| Analog / contact controls | pushbutton, toggle, potentiometer, rotary encoder, stimulus | drive/observe a net (Thevenin source or contact resistor) |
| Digital bus slaves | 24Cxx I2C EEPROM, LM75 I2C sensor, 25xx SPI EEPROM, MCP3008 SPI ADC | answer the MCU's `on_i2c` / `on_spi` hooks |
| Output sinks | VCD logger | observe nets, record to a file |

## Analog / contact controls

All attach by **net name** or by **connector ref + pin** (the binder resolves
ref+pin to the pin's net at bind time, so the control only ever sees a node).

- **Pushbutton**: momentary contact between a net and a reference (default
  ground). Released means open, pressed means closed (1 Ω). It carries an
  optional **contact-bounce model**: a press or release chatters open/closed
  for `bounce_ms` before it settles, so debounce firmware gets actually
  exercised.
- **Toggle switch**: latching SPST. It holds its state, idealised as
  debounced.
- **Potentiometer**: three-terminal (`a`, wiper, `b`) with a `position` in
  0..1 that splits the track resistance between the two legs. Wire `a` to a
  rail and `b` to ground, and the wiper becomes an analog input the firmware
  can read.
- **Rotary encoder**: drives two nets with a quadrature A/B Gray-code
  sequence from a commanded detent position. Stepping the position advances
  the phase one quadrature edge per detent, in the right direction.
- **Stimulus**: a generic voltage or current source: DC, sine, PWL, or noise.
  Voltage stimulus sits behind a 50 Ω series resistor (measurable, never a
  hard short); current stimulus injects into the net.

Live control: `set_value(v)` interprets `v` per kind (button: `>=0.5`
pressed; pot/encoder: position; stimulus: DC level).

## Digital bus slaves

These let firmware talk to realistic parts that are **not on the board
model**, or stand in for bound-but-unmodelled board parts.

Parallel memories that are physically present on the PCB are different: they
bind as ordinary digital components rather than attached peripherals. The
built-in AT28C256-class model provides a 32 KiB × 8 erased array, CE/OE/WE bus
gating, WE-edge writes, 64-byte page boundaries, the 150 µs byte-load deadline,
and the datasheet software-data-protection enable/program and disable sequences.
On simavr, direct GPIO and 74HC595-supplied address bits are
resolved on each firmware edge, and a read drives the MCU's input pins before
its next instruction. This is the path a real parallel EEPROM programmer uses;
no synthetic I2C/SPI adapter is inserted. See
[`logic_spec.md`](../how-and-why/hauksbee-models/logic_spec.md) for the model
contract and its timing boundary.

### I2C

`I2cBus` is a router: it owns one or more `I2cSlave`s and registers as the
MCU's `on_i2c` callback. It dispatches each bus event to the slave whose
7-bit address matches. Two concrete devices:

- **24Cxx EEPROM** (`Eeprom24c`); 16-bit word address, auto-incrementing
  page writes and current-address reads. Backing memory is readable for
  assertions (`contains(bytes)`).
- **LM75 temperature sensor** (`Lm75`); the real datasheet register map:
  a pointer register selects Temp (0x00) / Conf (0x01) / Thyst (0x02) / Tos
  (0x03). The temperature register encodes T in **0.125 °C/LSB,
  left-justified, big-endian** (the LM75A 11-bit format, which 9-bit reads
  also accept). The reported temperature is configurable and live-settable.

### SPI

`SpiBus` owns one `SpiSlave`, so one slave per bus is active. Chip-select,
when it is known, frames that slave's transactions rather than selecting among
several (three framing tiers, see "SPI transaction framing" below). Two
concrete devices:

- **25xx EEPROM** (`Spi25Eeprom`); WREN/WRDI/RDSR/READ/WRITE instruction
  set, 16-bit addressing, write-enable latch. Memory readable for
  assertions.
- **MCP3008 ADC** (`Mcp3008`); 8-channel 10-bit ADC. Each channel's input
  voltage is settable and converts to counts on the standard 3-byte
  transfer.

### Master reads on the AVR TWI hook

Slaves are readable as well as writable. The TWI hook handles
`TWI_COND_READ` by asking the slave for its byte and raising the TWI input
IRQ with `READ | ACK` carrying the data, and `I2cEvent` carries a
`Read { addr }` variant for this path. The integration proof reads an LM75
and decodes real temperature bytes end to end.

### Declarative register-map devices (and the write side)

Hand-coding each bus device in Rust does not scale, so a device can instead
be a **TOML spec** (`testdata/sensor-specs/*.toml`, schema in
`hauksbee-models/src/sensor_spec.rs`) that the generic `RegisterMapSensor`
interpreter realizes as a live `I2cSlave`/`SpiSlave`. The read side maps
physical inputs through `evalexpr` value expressions into datasheet register
packings. LM75 is the one part with a hand-coded counterpart to check against,
and the declarative spec comes out byte-identical to it
(`declarative_lm75_is_byte_identical_to_handcoded` in
`crates/hauksbee-engine/src/peripherals/register_map.rs`). BME280 and MPU6050
have no hand-coded twin, so they are anchored to datasheet worked examples
instead (`declarative_bme280_decodes_datasheet_worked_example`,
`declarative_mpu6050_decodes_driven_quantities`); the `Bme280` name exported
from `peripherals::i2c` is a type alias for `Lm75`, not a second model.

A CI spec attaches one of these specs with a `[[sensor]]` block (inline `spec`
or a `spec_file` path, plus `[sensor.inputs]` overrides and an optional SPI
`controller`), separate from the `[[peripheral]]` list. See
[`docs/ci/CI.md`](../ci/CI.md) and `SensorAttach` in
`crates/hauksbee-ci/src/spec.rs`.

The spec also describes the **write side**: what firmware writes do:

- **Pointer-framed write registers** decode into stored variables that read
  expressions reference, so a config write changes what a later read
  returns. ADS1115 (config selects mux/PGA for the conversion register) and
  INA219 (calibration feeds the current/power math) are the shipped proofs,
  each fixture-anchored to datasheet worked-example numbers.
- **Command-framed writes** (the MCP4728 shape: mask-matched command byte,
  fixed-size data groups, per-channel state) update state whose **output
  voltage laws drive analog nets**. The MCP4728 is pure data
  (`testdata/sensor-specs/mcp4728.toml`); the scheduler binds the
  binder-stamped VOUT `PinDriver`s to the spec's outputs, and the slave
  drives them itself in the ctx-bearing `on_stop`, delivered once per chunk,
  after the MCU runs and before the analog solve, because the byte events
  arrive inside the MCU callback, where no circuit context exists (and the
  solve could not see a mid-chunk voltage anyway).
- **Bit extraction is framing-layer data**, never expression math:
  `evalexpr` has no bit operations, so fields are declared `[high, low]` bit
  ranges the interpreter extracts in Rust. Everything downstream (state
  updates, voltage laws, read-back frames) is expressions over the extracted
  names.

What the write side does **not** do, stated rather than faked:

- **SPI writes are accept-and-ignore** (validation rejects SPI write blocks;
  the ignored bytes are counted). No shipped device needs them yet.
- **No timing.** hauksbee does not model conversion/update latency: an
  ADS1115 single-shot completes instantly, and a DAC write lands at the next
  chunk solve.
- **Undeclared writes are ACKed and counted** (`ignored_write_bytes` in the
  slave's `state()`), like a real part ACKing a command family the model
  omits. An eaten config write is observable, never silent. The counter is a
  single total over four distinct cases: an undeclared command family (and
  every following byte of it), payload past a declared write register's
  natural width, payload for a pointer register the spec does not declare, and
  every byte of a SPI write phase. It also only appears in `state()` **when
  the count is non-zero**, so `field = "ignored_write_bytes"` on a run that
  ignored nothing fails with "has no state field" rather than reading 0.
- **SSD1306 is deferred (design sketch).** A display is a write-only
  command/data STREAM, not a register map: the control byte (0x00 command /
  0x40 data) selects an interpreter, commands set an addressing-mode cursor,
  and data bytes fill a 128×64 framebuffer, a **sink with megabytes of
  addressable state**, not per-channel scalars with a voltage law. Forcing
  it into `state`/`output` would mean thousands of fake "channels" and no
  net to drive. The honest shape is a third device class:
  `write_command`-style framing feeding a `framebuffer` sink block
  (page/column cursor semantics in Rust, dimensions and command opcodes as
  data) with assertions over pixel regions instead of net voltages. That
  lands with the W5 extensibility SDK rather than being bent into the
  net-driving schema here.

### Bus-speed honesty (chunk-rate limits)

Interception happens at the **byte / transaction level** through simavr's
hardware TWI and SPI peripheral models, not by sampling SCL/SDA/SCK edges.
simavr clocks each bus byte internally and raises one IRQ per byte/condition,
all consumed inside a single `run_micros` chunk. So:

- The **achievable bus speed is whatever the firmware's prescaler asks for**
  (I2C 100 kHz / 400 kHz, SPI at the SPR/SPI2X rate). The analog chunk rate
  does **not** bound it, because the bytes never cross a chunk boundary.
- A **bit-banged** master (software toggling SCL/SDA or SCK/MOSI on GPIO)
  takes a different path, and it splits by backend. On a **push** backend
  (simavr) those edges are resolved exactly: the `Mcu::on_input_responder`
  hook runs a responder synchronously on every GPIO output edge and applies
  its returned input-pin drives before the firmware's next instruction, so a
  soft bus routes into the same byte-level slave models the hardware path
  uses. `BitBangSpiResponder` and `SoftI2cResponder`
  (`crates/hauksbee-engine/src/responders.rs`) are the two shipped protocol
  responders, multiplexed onto that single hook by a `ResponderRegistry`
  keyed on watched output pin. On **poll** backends (Renode, QEMU) the hook
  keeps its no-op default, so soft-bus edges alias at the chunk poll rate
  exactly like any other GPIO (see `docs/cosim/MCU.md`), and bit-banged MHz
  traffic there is **not** resolved. The push-backend proofs:
  `crates/hauksbee-engine/tests/soft_i2c_cosim.rs` (firmware bit-bangs I2C on
  PD2/PD3, deliberately not the hardware TWI pins, and reads the declarative
  MPU-6050 with repeated-START framing) and
  `crates/hauksbee-engine/tests/bitbang_spi_cosim.rs` (firmware bit-bangs SPI
  mode 0 on PD4..PD7 and reads the declarative ICM-42605).
- The **Renode `on_i2c` / `on_spi` hooks are wired** through generated C#
  bridge peripherals (an `II2CPeripheral` / `ISPIPeripheral` per slave
  address, loaded into the running machine over the Monitor). A
  hardware-TWI/SPI sensor therefore co-simulates on the Renode ARM/RISC-V
  backends the same way it does on simavr, **on the platforms whose SoC
  descriptor names bus controllers**; see the `i2c_sensor_cosim_renode` /
  `spi_sensor_cosim_renode` integration tests, and the coupling table in
  `docs/cosim/MCU.md` for which platforms those are. (Bit-banged masters on
  a poll backend stay the exception above.)

## Output sinks

- **VCD sink** (`VcdSink`); samples a chosen set of nets after every solve,
  decides each one's logic level with thresholds + hysteresis, and records a
  timestamped change on every flip. `write()` emits a **gtkwave-compatible**
  Value Change Dump (IEEE 1364, 1 ps timescale). It composes with everything:
  you can log any net the firmware or another peripheral drives without
  touching it.

## Live control over the websocket

The protocol extension is **additive and backward compatible** (the existing
frontend keeps working unchanged):

- `BoardInfo.peripherals: [{id, kind}]` lists the attached peripherals, so a
  UI can build controls for them.
- `ClientMessage::SetPeripheral { id, value }` live-controls a peripheral.
  `SetInput { source, value }` also routes to a peripheral of that id as a
  fallback, so a frontend slider wired to a peripheral id works with no
  change.
- Peripheral state folds into `SimFrame.component_states` keyed by id
  (e.g. `{"pressed":1}`, `{"position":0.5}`, `{"transitions":20}`, and for an
  `i2c_bus` `{"slaves":1,"0x48_temp_c":40}`, since a bus prefixes each slave's
  keys with its address).

## The host is a peripheral too: `run --serial-attach`

The peripherals above are things hauksbee models on the board's behalf. The
other half of "attach something to the running board" is attaching *your own
software* to it, which is a host serial port rather than a `Peripheral`:

```
hauksbee run board.kicad_pcb --firmware fw.elf --serial-attach --serial-wait 30
```

That prints a device path (`/dev/ttys006`), and your pyserial script, vendor
tool, or `minicom` opens it exactly as it opens a real USB serial cable. The
mechanism, the attach/detach reporting, and its honest limits are documented in
[MCU.md](MCU.md#talking-to-the-board-from-your-own-software---serial-attach).
The two layers compose: peripherals answer the firmware's buses while your
software drives its command protocol, in the same co-sim run.

## hauksbee-ci: the `[[peripheral]]` spec section

A spec attaches peripherals for a headless run, with type-specific config and
a timeline of events. Assertions can reference peripheral state.

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
id = "U4"
type = "i2c_eeprom"
address = 0x50
size = 256

[[peripheral]]
id = "U3"
type = "spi_mcp3008"
vref = 5.0
ref = "U3"                 # the board component this slave IS; if its model maps a
                           # `cs` pin, that alone buys exact framing
cs_net = "SPI_CS"          # optional override: exact SPI framing off the real CS
                           # edges, and it wins over the model's `cs` pin

[[peripheral]]
id = "VCD"
type = "vcd_sink"
nets = ["CLK", "DATA"]
vcd_path = "out/trace.vcd"
```

A `[[scenario]]` block also attaches a peripheral, without appearing in this
list: each one installs a `DynamicLoad` current sink on the part's supply net
under the id `load_<scenario id>` (the scenario's own `id`, defaulting to its
`part`). That id is what the load reports its `current_a` / `peak_a` under. It
is not addressable by a `peripheral` assertion, though, because that
assertion's `id` must name a declared `[[peripheral]]` or `[[sensor]]`; a
scenario's current is asserted through `rail_window` and `protection_trip`
scoped to the scenario instead.

### Peripheral assertions

```toml
# EEPROM contents contain bytes (hex or ASCII).
[[assert]]
kind = "peripheral"
id = "U4"
bytes = "48 69"            # or bytes = "Hi"

# A peripheral state field is in range (transitions, position, ...).
[[assert]]
kind = "peripheral"
id = "VCD"
field = "transitions"
min = 15
max = 25

# An I2C bus prefixes each slave's keys with the slave's 7-bit address.
[[assert]]
kind = "peripheral"
id = "U2"
field = "0x48_temp_c"
min = 39.9
max = 40.1
```

The `field` name is per peripheral `type`; each exposes its own state keys.
The full vocabulary, matched exactly:

| peripheral `type` | `field` keys | meaning |
|---|---|---|
| `pushbutton` | `pressed` | 1 while held, else 0 |
| `toggle` | `closed` | 1 when closed, else 0 |
| `potentiometer` | `position` | wiper fraction 0..1 |
| `encoder` | `detents`, `a`, `b` | accumulated detents; the two quadrature line levels |
| `stimulus` | `value` | the last driven value |
| `vcd_sink` | `transitions`, `nets` | edge count captured; number of nets watched |
| `i2c_eeprom` | `slaves`, `0x<addr>_size`, `0x<addr>_ptr`, `0x<addr>_page_size` | slaves on the bus; byte capacity; current address pointer; page size |
| `i2c_lm75` | `slaves`, `0x<addr>_temp_c`, `0x<addr>_pointer` | slaves on the bus; configured temperature; register pointer |
| `spi_eeprom` | `size`, `wel` | byte capacity; write-enable-latch state |
| `spi_mcp3008` | `vref`, `ch0`..`ch7` | reference voltage; per-channel input voltage |
| `[[scenario]]` load | `current_a`, `peak_a` | instantaneous and peak sink current |

An `i2c_*` peripheral is an `I2cBus` router, so its state is the bus's own
`slaves` count plus each attached slave's keys **prefixed with that slave's
7-bit address**: `0x48_temp_c` for an LM75 at the default address. A SPI
peripheral is one `SpiBus` with one slave, so it passes the slave's keys
through unprefixed.

The assertion's `id` is checked when the spec loads (it must name a declared
`[[peripheral]]` or `[[sensor]]`), but the `field` name is not. An unknown
field fails at **evaluation**, surfacing as a FAILED assertion whose detail
names the field and lists the ones the peripheral actually produced, so a typo
costs a run rather than being caught up front:

```
[FAIL] unprefixed i2c field must fail at evaluation
      U2 has no state field 'temp_c' (have: ["0x48_pointer", "0x48_temp_c", "slaves"])
```

(For `i2c_eeprom` / `spi_eeprom` contents, prefer the `bytes = "..."` form
above over a `field`.) Each peripheral's `state()` in
`crates/hauksbee-engine/src/peripherals/` produces these keys.

## Proofs (integration tests)

1. **I2C temperature sensor co-sim**:
   `crates/hauksbee-engine/tests/i2c_sensor_cosim.rs` (avr feature). AVR
   firmware (`testdata/firmware/i2c_thermostat`) reads the LM75 over
   hardware TWI and drives PB0 from the temperature vs a 30 °C threshold.
   The test sweeps the configured temperature `[10, 25, 29, 31, 35, 50, 28,
   15] °C` and asserts the GPIO (net `FLAG`) reads HIGH exactly when
   `T >= 30 °C`. The firmware prints the decoded temperature over UART,
   confirming the master-read path returns the real datasheet-encoded
   bytes.
2. **CI button press drives a net**:
   `testdata/ci/button_press.toml` (run by
   `crates/hauksbee-ci/tests/peripherals.rs`). A pushbutton is pressed at
   100 ms and released at 150 ms on a net pulled to +5 V through 10 kΩ. The
   test asserts the net settles back high after release and toggles exactly
   twice from the timed press/release.
3. **VCD sink**: `crates/hauksbee-ci/tests/peripherals.rs` and the
   `peripherals::sink` unit test. A timed PWL square wave drives a net; the
   sink logs it; the test validates the written VCD for a 1 ps timescale, a
   wire declaration, and ~20 known transitions.

## Honest limitations

- **Bus-slave coupling is per platform, not universal.** The trait layer is
  backend-agnostic; what routes a slave model's traffic is not. simavr
  decodes AVR TWI/SPI directly. On Renode it works on the platforms whose SoC
  descriptor names bus controllers (STM32F103/F4 `i2c1`, STM32F103 `spi1` and
  F4 `spi1-3`, nRF52840 `twi0`/`twi1`/`spi2`, RP2040 `i2c0`/`i2c1`, the last
  proven end-to-end in both directions). `sifive_fe310.soc.toml` declares no
  controllers at all, and `rp2040.soc.toml` declares none for SPI because the
  vendored PL022 bit-bangs onto GPIO pins and never dispatches to a registered
  `ISPIPeripheral`, so a bridge there would see nothing. Either way a slave
  bound to a controller-less bus is recorded UNEXERCISED, surfaced on every
  report surface, and a CI `peripheral` assertion against it FAILS. Under QEMU, hauksbee-ci emits a
  loud warning that an `[[peripheral]]` bus slave or a `[[sensor]]` is a
  NO-OP on that backend, and the shipped ESP32 I2C proof instead rides the
  machine's own emulated tmp105, into which the scheduler pushes the modeled
  LM75's temperature each chunk. The per-coupling table in
  [MCU.md](MCU.md) is the authority.
- **Bit-bang is push-backend only.** Byte-level interception of the hardware
  TWI/SPI peripherals is the main path everywhere. A software bit-banged bus
  master is resolved exactly on the push backend (simavr) through the
  synchronous input responders above, and not at all on the poll backends
  (Renode, QEMU), where its edges alias at the chunk rate like any GPIO.
- **SPI transaction framing has three tiers**, reported per slave so a
  verdict never hides which one it got. Exact framing is reached by more than one
  route; they all give the same tier because they all give the same electrical
  fact, and the route is reported alongside it (`cs_provenance` in the `--json`
  coverage) because they fail differently:
  - **Exact, from the spec** (`cs_provenance: "spec"`): the peripheral's
    `cs_net` resolved to the MCU GPIO pin that drives it, so
    `select`/`deselect` fire on the true active-low falling and rising edges,
    interleaved in cycle order with the byte transfers. Available on push
    backends (simavr).
  - **Exact, from the model's pin roles** (`cs_provenance: "model-roles"`):
    no `cs_net` was declared, but the peripheral's `ref` names a board
    component whose bound model maps a `cs` pin, so the CS net is read off
    that pad. Nothing to declare: the model DB already knows which pad is
    chip-select on the parts it covers (the MCP3008 and the 25AA/25LC SPI
    EEPROM ship with it). The part must be assembled and identity-trusted to
    supply one, so a DNP or identity-refused slave contributes nothing, and a
    `ref` naming no board component is a load-time error rather than a quiet
    drop to the heuristic. A declared `cs_net` always wins, which is how a
    wrong pad map or a buffered chip-select stays correctable by hand.
  - **Exact, from bit-bang wiring** (`cs_provenance: "bitbang-pins"`): a
    bit-banged SPI slave (see "bit-bang is push-backend only" above), whose CS
    pin comes from the GPIO wiring its responder was attached with rather than
    from a net lookup. The responder owns the CS edges, so the framing is real.
  - **Backend**: the emulator surfaces CS itself (Renode hardware NSS
    `FinishTransmission` arriving as a `deselect` event), which frames the
    transaction precisely with no resolved CS pin. Detected dynamically the
    first time such an event lands, and it takes precedence over Exact when
    reported.
  - **Heuristic**: no CS net from either route and no backend CS event, so the
    bus treats the co-sim chunk boundary as a CS deassert. Wrong in two
    documented ways: two transactions inside one chunk merge, and a
    chunk-spanning transaction is truncated. This is the genuine remainder,
    an unrouted chip-select or a part the model DB does not cover, and it is
    disclosed rather than papered over. hauksbee-ci appends the tier to the
    assertion's own detail text: `[SPI framing: HEURISTIC; transaction
    boundaries guessed at chunk edges; two transactions in one chunk merge and
    a boundary-spanning one is truncated. Declare cs_net, or point `ref` at a
    modelled part, for exact framing]`.
- **One SPI slave per bus.** CS frames a transaction here, it does not select
  among several slaves on one bus.
- **The declarative write side is I2C-only and untimed** (see the section
  above): SPI write phases and conversion/update latency are stated
  non-features, and unmodeled write bytes are counted, not decoded.
- **Contact bounce is a deterministic chatter model**, not a measured
  statistical bounce profile: a fixed ~5-cycle open/close burst across the
  configured window. It is enough to exercise debounce logic, not to
  reproduce a specific switch's bounce signature.
