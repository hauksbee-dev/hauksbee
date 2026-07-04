# Hauksbee capabilities and scope

> Read this first if you are integrating, evaluating, or building automation
> around hauksbee. The two most common mistakes — treating it as "just a DRC
> wrapper" and assuming firmware co-simulation is ATmega-only — are addressed
> explicitly in the [Common misconceptions](#common-misconceptions) section at
> the end.

---

## Scope at a glance

| Capability | What it catches | Commodity or differentiated? | Needs external simulator? |
|---|---|---|---|
| Copper short / clearance DRC (`--drc`) | Net-to-net copper touches and below-clearance gaps | **Commodity** — KiCad's built-in DRC finds these too | No |
| Lint (`--lint`) | Connectivity, strap-pin boot states, designator / footprint / value sanity, MCU resource conflicts | Differentiated (strap-pin and resource-conflict logic is hauksbee's own) | No |
| Bind report (`--report`) | Component → device-model mapping for every part on the board | Utility / transparency | No |
| Signal integrity (`--si`) | SI physics: controlled-impedance USB/Ethernet estimates, stubs, series termination | Differentiated | No |
| Trace ampacity (`--ampacity`) | IPC-2221 current-capacity vs routed trace width | Differentiated | No |
| Thermal (`--thermal`) | Steady-state junction temperature per dissipating device (Tj = Tambient + P × θJA) | Differentiated | No |
| AC analysis (`--ac`) | Small-signal Bode, gain crossover, phase margin | Differentiated | No |
| Firmware co-sim (`--firmware --headless`) | Firmware-driven GPIO/peripheral faults, actual rail voltages under real firmware load | **The differentiator** | AVR: no. STM32/nRF52/RISC-V: Renode. ESP32 family: Espressif QEMU |
| Behavioral goal assertions (`hauksbee-ci run`) | Whether the firmware makes the hardware do its job: rails, UART output, blink rate, boot timing, temperature, loop stability | **The differentiator** | Same as co-sim above |
| Board-as-Code (`to-code` / `from-code` / `check-code`) | Edit-simulate loop: catch miswire, stress faults, and thermal issues in a pre-commit hook | Differentiated | Optional (co-sim in `check-code` uses whichever backend the board's MCU needs) |

---

## Layer 1: Static analysis (`hauksbee run <board>`)

All flags below print a report and exit 0. Add `--strict` to fail on real
defects (exits 2), or `--plain` (`--explain`) to rewrite findings as
plain-language what / why / what-to-do. They compose: `--drc --plain --strict`
gives a human-readable DRC gate.

### `--report`

Prints the component-to-device-model bind table: every reference designator,
its footprint, value, and the device model hauksbee bound to it from the model
DB. Useful for confirming that the binder resolved the parts you expect before
running a heavier analysis.

### `--drc`

Geometric copper short and clearance report. Detects net-to-net copper touches
(gap ≤ 0) and below-clearance gaps.

**This is commodity.** KiCad's built-in DRC finds exactly the same class of
fault. A DRC short found by hauksbee is confirmable — and should be confirmed —
with `kicad-cli pcb drc`. hauksbee's differentiator is not finding shorts; it
is running the rest of the pipeline (analog solve, firmware co-sim, behavioral
assertions) after finding them.

Pair with `--oracle` to cross-check hauksbee's DRC result against
`kicad-cli pcb drc` in one command. The oracle is detected from an existing
KiCad install; it is not bundled. See [`docs/ORACLES.md`](ORACLES.md).

### `--lint`

Connectivity and design-file quality check. Covers:

- Connectivity: floating pins, missing supply connections
- Strap-pin boot states: MCU boot-configuration pins (STM32 BOOT0, AVR RESET,
  RP2040 BOOTSEL/QSPI_SS, ESP32 strapping pins) checked against the required
  strap levels from the part's datasheet
- MCU resource conflicts: two peripherals mapped to the same pin, conflicting
  timer assignments, and similar internal-resource clashes
- Design-file QC: placeholder values (`DNF`, `TBD`), mismatched
  designator/footprint/value combinations

`--resources` prints only the MCU resource-conflict subset of the lint.

### `--si`

Signal-integrity and physics static check. Includes a controlled-impedance
estimate for USB and Ethernet traces derived from trace geometry and stackup
(quasi-static closed-form, not a field solve), stub detection, and series
termination checks. See [`docs/SI_CHECKS.md`](SI_CHECKS.md).

### `--ampacity`

IPC-2221 trace current-capacity check for power-like routed nets. Reports
capacity, not actual current (actual current comes from the co-sim or a spec's
`[[supply]]`).

### `--thermal`

Runs a short headless co-sim and prints a steady-state junction temperature
estimate per dissipating device: `Tj = Tambient + P × θJA`. Per-device, not a
board thermal field solve. Configurable ambient with `--ambient <C>` (default
25 °C).

### `--ac`

Small-signal AC frequency sweep. Linearises about the DC operating point and
produces a Bode table (magnitude dB + phase) for one or more output nets. Use
`--ac-loop <net>` to extract gain crossover and phase margin for a feedback
loop. Outputs to terminal or to CSV with `--ac-csv`. Full details in
[`docs/AC_ANALYSIS.md`](AC_ANALYSIS.md).

---

## Layer 2: Oracles (detect, don't bundle)

hauksbee cross-checks its own results against independent authoritative tools:

- **KiCad `kicad-cli pcb drc`** — DRC oracle. Invoked with `--oracle` on a
  `--drc` run. Not bundled: KiCad is ~1.4 GB and GPL-3.0. hauksbee
  auto-detects an existing install (PATH, standard application locations,
  prefers the highest version found).
- **ngspice** — analog oracle. hauksbee's transient and AC results are
  cross-checked against ngspice; every solver speed claim is gated behind an
  accuracy check. Not required at runtime; used for validation.

Oracles confirm hauksbee is right, they are not how hauksbee runs. The MCU
simulator backends (Renode, Espressif QEMU) follow the same detect-don't-bundle
pattern. Full rationale and install instructions: [`docs/ORACLES.md`](ORACLES.md).

---

## Layer 3: Firmware co-simulation — the differentiator

This is what no other tool does from the layout. hauksbee boots real firmware on
an emulated MCU and its GPIO and peripheral edges drive currents and voltages
through the analog circuit solver in lockstep. Every static check becomes a
dynamic check: rails are measured under actual firmware-driven load; GPIO
transitions fire in the correct order; boot timing is observed in simulated time.

The entry point is `hauksbee run <board> --firmware <img> --headless [--seconds N]`.

### Boot-safety advisory (`--headless --plain` / `--json`)

When firmware co-sim runs, hauksbee reports a power-up hazard the netlist alone
cannot adjudicate: a **control net that switches a transistor/relay, is driven
(or pulled) HIGH and held from reset, and has no bias resistor** setting a safe
default — a MOSFET gate / relay / motor enable / igniter energised at power-up.
*"control net 'X' switches a transistor/relay and is driven HIGH and held from
the moment the board powers up … confirm the polarity and that this is
intended."*

Three conditions must all hold, and the **switch requirement is the zero-FP
guard**: it is what separates a genuine load-control net (e.g. the igniter gate
fed by a mis-mapped `SoftwareSerial` pull-up) from an ordinary `INPUT_PULLUP`
button input, which also reads HIGH at boot but switches nothing. It is honest,
advisory data, **never a fault on its own** (a held-high enable that *should* be
high is correct), so the plain report stops saying "Looks healthy" and instead
says "no failures, but N worth a look" and names the net; a board whose firmware
only toggles a signal stays "healthy". The advisory also appears in `--json` as a
note with `kind: "boot_control_net"`. Pass `--strict-boot` to fail CI (exit 2) on
it; by default it does not affect the exit code.

**The boot-state panel (`--plain` / `--json`).** Alongside the held-high warning,
co-sim prints an informational panel of every MOSFET/transistor gate it can
identify and what the firmware does to it at power-up — `driven HIGH and held`,
`driven LOW and held`, or `never driven (floating)`:

```
Power-up state of MOSFET / transistor gates — what the firmware does to each
switch the moment the board powers up. Verify each is the level you intend:
  Q1  IgnitOne   pulled HIGH (weak internal pull-up)  <- switched at power-up
  Q2  IgnitTwo   driven LOW and held
  Q3  FanGate    never driven (floating)              <- undefined until firmware drives it
```

It distinguishes a strong push-pull `driven HIGH` from a weak `pulled HIGH (weak
internal pull-up)` — the latter is exactly the igniter case (a serial RX pin
mis-mapped onto the gate enables its pull-up), and naming the *mechanism* tells a
non-engineer the gate went high by accident, not by design.

This is **reported, not judged** — which is what makes it safe for a non-engineer
and lets it cover the cases the *warning* can't. The held-high **warning** is a
verdict (and the only thing `--strict-boot` gates on), so it must be zero-FP. The
**panel** is data: it shows the floating case (a forgotten gate-drive) and the
held-low case without ever asserting a fault, so an ambiguous net is just a line
the user reads against what they know the board is for — no false alarm, no CI
break. It deliberately makes no channel-type safety claim (a HIGH gate is "on"
for a low-side N-MOSFET but "off" for a high-side P-MOSFET, which the copper can't
disambiguate); it reports the level and flags the active/undefined ones to
verify. Gates are identified by a `G`/`GATE`/`B`/`BASE` pad name where present,
else by footprint convention (SOT-23 pin 1, Power-SO-8 pin 4, …); a transistor
whose control terminal can't be reliably identified is omitted rather than
mislabelled. In `--json` the same rows appear under `boot_gates`.

For a deadline-gated, *named* pass/fail check on a specific net, use the
`boot-coverage` assertion in `hauksbee-ci` (`boot_gate_pass.toml` /
`boot_gate_fail.toml`). The held-high warning is exactly how the
`explosion33/RocketryIgniter` power-up ignition fault surfaces from one command
(see [`hunts/HUNT_2026-06-30.md`](hunts/HUNT_2026-06-30.md)).

### The `Mcu` trait and three backends

All three backends expose the identical lockstep surface: `run_micros`,
GPIO/ADC/UART exchange via `PinId { port, bit }` and byte streams. The engine's
scheduler couples to any of them without change. The trait lives in
`crates/hauksbee-mcu/src/traits.rs`; the backends in
`crates/hauksbee-mcu/src/`.

#### AVR — `AvrMcu` (libsimavr, linked in-process)

Backed by libsimavr, linked directly into the engine — the AVR co-sim runs
in-process, with no separate simulator to launch. simavr is GPL-3.0 and this
MIT repo deliberately does **not** vendor it, so a source build links it from
the system. Get there one of three ways:

- install it with one command — `scripts/install-sims.sh --avr` (builds and
  installs libsimavr + libelf into the prefix the build links against);
- point the build at an existing copy via `SIMAVR_INCLUDE_DIR` /
  `SIMAVR_LIB_DIR`; or
- build the GPL-free subset without AVR —
  `cargo build -p hauksbee-engine --no-default-features --features renode,qemu`.

Supported parts (anything simavr knows by name, two shipped convenience
constructors):

- **ATmega328P** at 16 MHz (`AvrMcu::atmega328p_16mhz()`) — the Arduino Uno /
  Nano class
- **ATmega2560** and other simavr-known AVR MCUs via `AvrMcu::new("atmega2560",
  freq)`

Supply rails configurable (`set_rails`); GPIO output hooks cover ports A–H;
UART, I2C (TWI), and SPI all bidirectional.

Licensing note: a binary built with the `avr` feature links libsimavr (GPL-3.0)
and is subject to the GPL.

#### Renode — `RenodeBackend` (external, GPL-free over sockets)

Drives a headless Renode process over its Monitor TCP protocol and a UART socket
terminal. Links no GPL code; a `--no-default-features --features renode,qemu`
build is GPL-free.

Shipped named configs and proven support status (from `crates/hauksbee-mcu/src/renode/mod.rs`
and [`docs/MCU.md`](MCU.md)):

| Config constructor | Platform | Proven on this branch |
|---|---|---|
| `RenodeConfig::stm32f103()` | STM32F103C8 (Cortex-M3, "blue pill") — PA–PG, USART1, 8 MHz HSI | **Proven**: UART boot banner, GPIO toggle, solved LED current |
| `RenodeConfig::stm32f4_discovery()` | STM32F4 Discovery (STM32F407, Cortex-M4) — PA–PE, USART2, 16 MHz | Config shipped; not run on this branch |
| `RenodeConfig::nrf52840()` | nRF52840 (Cortex-M4, two 32-bit GPIO ports) — gpio0/gpio1, uart0, 64 MHz | **Proven (UART boot)**: Zephyr shell `uart:~$` |
| `RenodeConfig::sifive_fe310()` | SiFive FE310 / HiFive1 (RISC-V RV32) — gpio0, uart0, 16 MHz | **Proven (UART boot)**: Zephyr shell boot with PRCI clock fix |

RP2040 (Cortex-M0+) is in the device model DB (strap-pin lint works) but no
`RenodeConfig::rp2040()` constructor is shipped; RP2040 firmware co-sim is not
a named, tested configuration.

nRF5340 (ZSWatch-class) is not in the Renode 1.16.1 portable distribution. No
config is claimed for it.

GPIO exchange mechanism: after each `RunFor` chunk the backend reads each
port's output-data register (ODR) over the Monitor, diffs the snapshot, and
fires per-bit edge callbacks. STM32F1 ODR offset `0x0C`, STM32F4 `0x14`, nRF52
`0x504`, FE310 `0x0C`. GPIO input: `sysbus.<port> OnGPIO <bit> <bool>`.

ADC injection: not yet wired for the Renode backend (platform-specific in
Renode; the STM32F103 demo couples through GPIO). I2C and SPI peripheral
interception: not yet wired (these are documented no-ops).

#### Espressif QEMU — `QemuBackend` (external, ESP32 family)

Drives a headless Espressif QEMU process (the fork with full ESP32 SoC
peripheral models) over QMP + gdbstub control channels and a UART socket.
Renode as of 1.16.1 ships no `esp32.repl` or `esp32c3.repl`, so the ESP32 path
is a separate backend. This is not a fallback; it is the intended and tested
path.

Shipped named configs:

| Config constructor | Machine | Architecture | Proven on this branch |
|---|---|---|---|
| `QemuConfig::esp32()` | `esp32` | Xtensa LX6 (dual-core, 240 MHz) | **Proven**: UART boot, GPIO toggle, solved LED current, stable across runs |
| `QemuConfig::esp32s3()` | `esp32s3` | Xtensa LX7 (240 MHz) | Config shipped; not run (no S3 firmware built) |
| `QemuConfig::esp32c3()` | `esp32c3` | RISC-V RV32IMC (160 MHz) | **Proven**: UART boot, GPIO toggle, solved LED current |

GPIO observation goes through a RAM mailbox in RTC slow memory
(`0x5000_0000`): the Espressif QEMU `esp32.gpio` model does not implement
read-back of `GPIO_OUT_REG` (verified empirically; host reads return 0). The
demo firmware mirrors its GPIO output word to the mailbox; the backend diffs
that word and synthesises edges. This is the only ESP32-specific wrinkle; the
edge synthesis is otherwise identical to the Renode ODR-poll. GPIO input is
pushed over the gdbstub `M` packet.

ADC injection: not modelled by the Espressif QEMU fork; documented no-op.

### Installing the external simulators

AVR works without any install. For everything else:

```
scripts/install-sims.sh          # install Renode + Espressif QEMU
scripts/install-sims.sh --check  # verify hauksbee will find them
```

Full discovery order, env-var overrides (`HAUKSBEE_RENODE`,
`HAUKSBEE_QEMU_XTENSA`, `HAUKSBEE_QEMU_RISCV32`, `HAUKSBEE_QEMU_DIR`), macOS
Gatekeeper steps, and manual install instructions are in
[`docs/SIMULATORS.md`](SIMULATORS.md).

Integration tests skip cleanly when the simulator is absent rather than
failing.

---

## Layer 4: Behavioral goal assertions (`hauksbee-ci`)

`hauksbee-ci run <spec.toml>` is the CI gate. It boots the firmware on the
emulated PCB, runs the spec's simulated duration, evaluates every `[[assert]]`
block, and prints a per-assertion GREEN / RED verdict. A run with any failed
assertion exits non-zero, gating CI. `--junit <out.xml>` writes JUnit XML for
the CI test-report step; `--quiet` suppresses the human report while preserving
the exit code. GitHub Actions annotations are emitted automatically when
`GITHUB_ACTIONS` is set.

Exit codes: 0 all assertions passed, 1 at least one assertion failed, 2 spec
or usage error.

### Spec anatomy

A spec is a TOML file checked in alongside the hardware design:

```toml
name = "power-up sanity"
board  = "hardware/board.kicad_pcb"
firmware = "firmware/build/app.elf"
mcu    = "atmega328p"
duration_ms = 200

[[supply]]
net  = "+5V"
kind = "ideal"          # ideal | bench | wall | usb | battery
volts = 5.0

[[assert]]
kind = "voltage"
net  = "ANALOG_VDD"
min  = 4.9
after_ms = 50
```

Board files: `.kicad_pcb`, `.kicad_sch`, `.brd`, `.d356`, or gerbers.

### Assertion kinds (from `crates/hauksbee-ci/src/spec.rs`)

| Kind | What it checks |
|---|---|
| `voltage` | A net's voltage is within `[min, max]` (optionally `after_ms` settling time) |
| `uart` | UART output `contains` a substring or `matches` a regex |
| `toggle` | A net toggles at `freq_hz` ± `tolerance`, or at least `min_toggles` times |
| `no_faults` | No stress fault is raised at any point during the run |
| `max_current` | Peak current through a named component (`ref`) stays below `amps` |
| `max_temp` | Steady-state junction temperature of a named component stays below `celsius` (or the device's own max if omitted) |
| `peripheral` | A simulated peripheral's state: EEPROM byte sequence, sensor field in a range |
| `rail_window` | Voltage bounds within a named scenario's load window; dip duration and recovery time |
| `protection_trip` | Whether a battery BMS protection circuit trips (or must not trip) |
| `boot-coverage` | A control net is driven to ≥ `min` volts within `deadline_ms` of reset, with no fault during the boot window — answers whether firmware drives a Hi-Z control input in time |
| `phase_margin` | Loop phase margin from the AC sweep is within `[min, max]` degrees |
| `ac_gain` | A net's AC gain is within `[min, max]` dB at an optional `freq_hz` |

Additional spec features: `[[supply]]` (ideal / bench / wall / USB / battery
models with ripple and BMS protection); `[[net_drive]]` (force a net to a fixed
voltage); `[[peripheral]]` (attach pushbuttons, toggles, potentiometers,
encoders, stimulus sources, I2C/SPI device models, VCD sinks — with event
timelines); `[[scenario]]` (transient load profiles for inrush / sag / brownout
dynamics); `[fuzz]` (run across N random initial-state seeds; all assertions
must hold on every seed); `[ac]` (drive the `phase_margin` and `ac_gain`
assertions).

### Canonical demo: `boot_gate_pass.toml` / `boot_gate_fail.toml`

These two specs in `crates/hauksbee-ci/examples/` are the canonical demo of
the behavioral layer. Both point at the same board (a bare ATmega328P driving a
2N7002 MOSFET gate with no pull resistor on the gate net, so at reset the net
is genuinely undefined — a case the netlist alone cannot adjudicate). They
differ only in firmware:

- `boot_gate_pass.toml` — firmware A configures PB0 as an output and drives it
  HIGH promptly. The `boot-coverage` assertion passes: `GATE_CTRL` reaches
  ≥ 3.0 V within 20 ms of reset.
- `boot_gate_fail.toml` — firmware B never configures PB0. The gate floats for
  the whole run. The `boot-coverage` assertion fails, naming `GATE_CTRL` as the
  net the firmware never drove. `hauksbee-ci run boot_gate_fail.toml` exits 1.

Run them:
```
hauksbee-ci run crates/hauksbee-ci/examples/boot_gate_pass.toml
hauksbee-ci run crates/hauksbee-ci/examples/boot_gate_fail.toml
```

### Behavioral device runtime

Converters, FSMs, protection laws, pull resistors, and open-drain outputs
participate in the analog solve. The behavioral runtime in `behavioral.rs`
ensures that device models respond dynamically during co-simulation, not just
at DC. This is how `boot-coverage` can observe a net transitioning mid-run and
how `rail_window` can observe voltage sag under a transient load.

---

## Layer 5: Board-as-Code

Three commands form the edit–simulate loop:

- **`hauksbee to-code <board>`** — decompile a `.kicad_pcb` into editable
  Board-as-Code text.
- **`hauksbee from-code <code>`** — recompile Board-as-Code back into a
  `.kicad_pcb`.
- **`hauksbee check-code <code> [--seconds N] [--destructive]`** — recompile,
  bind, run the stress monitor for `--seconds` of simulated time (default 0.2 s),
  and print a fault report. Exits non-zero if a fault is raised. Add
  `--destructive` to let the stress monitor destroy parts (shows consequences
  of miswire or over-stress). Add `--ambient <C>` for the thermal estimate.

`check-code` drops straight into a script or pre-commit hook.

---

## Input formats

`hauksbee run` and `hauksbee-ci` accept `.kicad_pcb`, `.kicad_sch`,
`.brd` (Eagle), `.PcbDoc` (Altium — see [`docs/ALTIUM.md`](ALTIUM.md)),
`.d356` (IPC-D-356 netlist), or a directory of gerbers
(reverse-extracted from copper geometry alone — see [`docs/GERBER.md`](GERBER.md)).

---

## Common misconceptions

### "Firmware co-simulation only works on ATmega / Arduino boards"

**Wrong.**

The AVR backend (ATmega328P, ATmega2560, and anything else simavr knows) links
libsimavr in-process — installed with `scripts/install-sims.sh --avr`, or
skipped entirely (see the AVR section above). But hauksbee's co-sim layer has three
backends behind one uniform `Mcu` trait, and the other two cover a wide range
of modern architectures:

- **STM32F103 (Cortex-M3), STM32F407 (Cortex-M4)** — via Renode
- **nRF52840 (Cortex-M4, Bluetooth)** — via Renode
- **SiFive FE310 / HiFive1 (RISC-V RV32)** — via Renode
- **ESP32 (Xtensa LX6), ESP32-S3 (Xtensa LX7)** — via Espressif QEMU
- **ESP32-C3 (RISC-V RV32IMC)** — via Espressif QEMU

STM32, nRF52840, ESP32, and ESP32-C3 are all proven end-to-end on this branch
(see `crates/hauksbee-engine/tests/stm32_renode_cosim.rs`,
`esp32_qemu_cosim.rs`, `renode_riscv_arm_cosim.rs`). The external simulators
install via `scripts/install-sims.sh` and are found automatically at runtime.

The correct description: **hauksbee's firmware co-sim covers AVR (via
libsimavr), STM32/nRF52/RISC-V (via Renode), and the full ESP32 family (via
Espressif QEMU).**

### "hauksbee is just a DRC wrapper"

**Wrong.**

DRC (copper shorts and clearance) is one report flag (`--drc`), it is
commodity — KiCad's built-in DRC finds the same class of fault — and it is
explicitly labelled as such in the scope table above.

hauksbee's differentiator is the firmware co-simulation and behavioral
assertion pipeline. It boots real firmware on an emulated MCU, drives the MCU's
GPIO and peripheral outputs through an analog circuit solver in lockstep, and
lets you assert — in a file that lives next to your hardware repo — that the
firmware makes the hardware behave correctly. A DRC run never touches firmware.
A `hauksbee-ci` run boots the firmware, measures rails, checks UART output,
watches GPIO blink rates, and gates CI on the result.

No other PCB-CI tool starts from the copper layout and runs firmware against the
simulated board. That is the scope of this tool.

---

## Where to go next

- [`docs/SIMULATORS.md`](SIMULATORS.md) — install Renode and Espressif QEMU,
  discovery order, env-var overrides
- [`docs/ORACLES.md`](ORACLES.md) — the DRC and analog oracles
- [`docs/MCU.md`](MCU.md) — full co-simulation architecture, per-board recipes,
  proven integration test results
- [`docs/CI.md`](CI.md) — GitHub Action, KiCad plugin, pre-commit hook
- [`docs/EXAMPLES.md`](EXAMPLES.md) — runnable examples
- [`docs/AC_ANALYSIS.md`](AC_ANALYSIS.md) — AC sweep and loop-stability details
- [`docs/THERMAL.md`](THERMAL.md) — thermal analysis details
