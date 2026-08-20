# Hauksbee capabilities and scope

> The authoritative scope document. Read this first if you are integrating,
> evaluating, or building automation around hauksbee: every layer, what is
> commodity versus differentiated, and exactly which MCUs the firmware co-sim
> covers.

---

## Scope at a glance

| Capability | What it catches | Commodity or differentiated? | Needs external simulator? |
|---|---|---|---|
| Copper short / clearance DRC (`--drc`) | Net-to-net copper touches and below-clearance gaps | **Commodity**: KiCad's built-in DRC finds these too | No |
| Lint (`--lint`) | Connectivity, strap-pin boot states, designator / footprint / value sanity, MCU resource conflicts | Differentiated (strap-pin and resource-conflict logic is hauksbee's own) | No |
| Bind report (`--report`) | Component → device-model mapping for every part on the board | Utility / transparency | No |
| Signal integrity (`--si`) | SI physics: controlled-impedance USB/Ethernet estimates, stubs, series termination | Differentiated | No |
| Trace ampacity (`--ampacity`) | IPC-2221 current-capacity vs routed trace width | Differentiated | No |
| Thermal (`--thermal`) | Steady-state junction temperature per dissipating device (Tj = Tambient + P × θJA) | Differentiated | AVR: no. Other MCUs: same as co-sim (it runs a short headless co-sim) |
| USB-C CC compliance (`--usb-c`) | The attach a compliant source sees from the receptacle's CC termination, and whether it applies VBUS. Flags the Raspberry-Pi-4-class shared-CC-pulldown fault | Differentiated | No |
| Full static suite (`--check`, alias `--all`) | Bind report + DRC + lint + SI in one pass | Convenience over the rows above | No |
| CI artifacts (`--junit`, `--sarif`) | The static suite as JUnit XML or SARIF 2.1.0, so a pipeline renders findings without a spec | Commodity formats, zero-config path | No |
| AC analysis (`--ac`) | Small-signal Bode, gain crossover, phase margin | Differentiated | No |
| Firmware co-sim (`--firmware --headless`) | Backend-dependent firmware-driven GPIO, peripheral, and simulated-rail checks | **The differentiator** | AVR: no. Supported STM32/nRF52840/RISC-V: Renode. Supported ESP32 variants: Espressif QEMU |
| Behavioral goal assertions (`hauksbee-ci run`) | Whether the firmware makes the hardware do its job: rails, UART output, blink rate, boot timing, temperature, loop stability. The two time-based kinds are clock-verified on every Renode part and AVR; the ESP32 family is wall-paced and says so at runtime (see below) | **The differentiator** | Same as co-sim above |
| Board-as-Code (`to-code` / `from-code` / `check-code`) | Edit-simulate loop: catch miswire, stress faults, and thermal issues in a pre-commit hook | Differentiated | Optional (co-sim in `check-code` uses whichever backend the board's MCU needs) |
| MCP server (`hauksbee-mcp`) | The same engine exposed to coding agents as a stdio MCP server | Differentiated | Same as the analysis it runs |

---

## Layer 1: Static analysis (`hauksbee run <board>`)

All flags below print a report and exit 0, even on a serious finding. That is
the report contract. The CLI prints a stderr note whenever a gate-grade
finding goes ungated. Add `--strict` to fail on real defects (exits 2). Add
`--plain` (`--explain`) to rewrite findings as plain-language what / why /
what-to-do. The flags combine: `--drc --plain --strict` gives a human-readable
DRC gate. Exit 3 means invalid-for-analysis: the run refuses to vouch for its
own result, for example an aborted analog solve. The full exit-code contract,
including `hauksbee-ci run` (0 green / 1 red / 2 spec error / 3 invalid), is
in [docs/ci/CI.md](../ci/CI.md#exit-codes-the-pipeline-contract).

### `--report`

Prints the component-to-device-model bind table: every reference designator,
its footprint, value, and the device model hauksbee bound to it from the model
DB. Useful for confirming that the binder resolved the parts you expect before
running a heavier analysis.

### `--drc`

Geometric copper short and clearance report. Detects net-to-net copper touches
(gap ≤ 0) and below-clearance gaps.

**This is commodity.** KiCad's built-in DRC finds exactly the same class of
fault. Confirm a DRC short found by hauksbee with `kicad-cli pcb drc`.
hauksbee does not differentiate itself by finding shorts. It differentiates
itself by running the rest of the pipeline (analog solve, firmware co-sim,
behavioral assertions) after finding them.

Pair with `--oracle` to cross-check hauksbee's DRC result against
`kicad-cli pcb drc` in one command. hauksbee detects the oracle from an
existing KiCad install. It does not bundle KiCad.
See [`docs/cosim/ORACLES.md`](../cosim/ORACLES.md).

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
termination checks. See [`docs/checks/SI_CHECKS.md`](../checks/SI_CHECKS.md).

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
[`docs/analysis/AC_ANALYSIS.md`](../analysis/AC_ANALYSIS.md).

### `--usb-c`

USB-C CC compliance report: models the attach a compliant source sees from
the receptacle's CC termination (Rd/Ra classification per the USB Type-C
spec's voltage windows) and reports whether the source applies VBUS. Flags
the Raspberry-Pi-4-class shared-CC-pulldown fault, and independent-Rd,
double-termination, and multi-receptacle cases.

### `--check` (alias `--all`)

The whole static suite in one pass: bind report, DRC, lint, and signal
integrity. This is the flag a pre-commit hook or a first look wants.

### CI artifacts (`--junit`, `--sarif`)

Either flag makes any `hauksbee run` publish the selected run surface as a file
a CI system already knows how to render: `--junit <out.xml>` for JUnit XML (a
testsuite per check, a testcase per finding, gate-grade findings as failures)
and `--sarif <out.sarif>`
for SARIF 2.1.0, which GitHub code scanning turns into pull-request
annotations. Waivers are applied first, so a waived finding is absent from the
file rather than present and ignored. Non-finding invalid/refused terminal
outcomes are JUnit errors and SARIF error results. Requested paths are invalidated before
analysis and committed once at the final outcome, so a failed or interrupted
run cannot leave a prior green artifact archiveable.

### Waivers

A finding that is wrong for your board can be overruled one finding at a
time, without switching the check off. A `hauksbee-waivers.toml` beside the
board names the finding, a required reason, and a required expiry; waived
findings are printed rather than hidden, and a lapsed waiver brings the
finding back. Syntax and rules: [docs/ci/CI.md](../ci/CI.md).

---

## Layer 2: Oracles (detect, do not bundle)

hauksbee cross-checks its own results against independent authoritative tools:

- **KiCad `kicad-cli pcb drc`**: DRC oracle. `--oracle` invokes it on a
  `--drc` run. hauksbee does not bundle KiCad: KiCad is about 1.4 GB and
  GPL-3.0. hauksbee auto-detects an existing install (PATH, standard
  application locations, and it prefers the highest version found).
- **ngspice**: analog oracle. hauksbee cross-checks its transient and AC
  results against ngspice. Every solver speed claim is gated behind an
  accuracy check. hauksbee does not require ngspice at runtime. It uses
  ngspice only for validation.

Oracles confirm that hauksbee is right. They are not how hauksbee runs. The
MCU simulator backends (Renode, Espressif QEMU) follow the same
detect-do-not-bundle pattern. Full rationale and install instructions:
[`docs/cosim/ORACLES.md`](../cosim/ORACLES.md).

Every production numeric result also carries a typed
[numerical error budget](../analysis/ERROR_BUDGETS.md): actual solver
tolerances, integration method by solved window, measured residual where the
path exposes one, explicit invalid spans, event timestamp precision, and only
model intervals with a supportable basis. Unknown is reported as unknown,
never as zero or guessed percentage accuracy.

---

## Layer 3: Firmware co-simulation

Hauksbee boots compiled firmware on a supported emulated MCU. Its GPIO and
peripheral edges drive currents and voltages through the analogue circuit
solver in lockstep. This enables dynamic checks in addition to the static
checks; it does not turn emulator observations into measured hardware.

Boot timing shows in simulated time on the backends whose clock rate is
measured against the part, which is every Renode part and AVR: on
`simavr:atmega328p` and `renode:rp2040` by construction, and on
`renode:stm32f103`, `renode:stm32f4_discovery`, `renode:nrf52840` (SysTick)
and `renode:sifive_fe310` (`mtime`) by the clock-truth gates, after each
descriptor was corrected to declare the part's clock instead of the stock
platform's. Firmware sleeping 20 ms costs 20 ms of simulated time there. The
`qemu:esp32` family remains the exception: its virtual time is paced against
the wall clock, so a frequency or deadline assertion there is approximate and
host-load dependent, and every affected run states that on its report
surfaces. See `docs/about/LIMITATIONS.md`.

Coverage in one sentence: **co-sim covers AVR (via libsimavr, in-process),
STM32 / nRF52 / RISC-V (via Renode), and the full ESP32 family (via Espressif
QEMU), all behind one `Mcu` trait.** The backend tables below give the
per-chip proven status.

The entry point is `hauksbee run <board> --firmware <img> --headless [--seconds N]`.

### Boot-safety advisory (`--headless --plain` / `--json`)

When firmware co-sim runs, hauksbee reports a power-up hazard that the netlist
alone cannot judge: a **control net that switches a transistor/relay, is driven
(or pulled) HIGH and held from reset, and has no bias resistor** to set a safe
default. This is a MOSFET gate, relay, motor enable, or igniter energized at
power-up. *"control net 'X' switches a transistor/relay and is driven HIGH and
held from the moment the board powers up... confirm the polarity and that this
is intended."*

Three conditions must all hold. The **switch requirement is the zero-false-positive
guard**. It separates a genuine load-control net (for example the igniter gate
fed by a mis-mapped `SoftwareSerial` pull-up) from an ordinary `INPUT_PULLUP`
button input, which also reads HIGH at boot but switches nothing. This is honest,
advisory data, **never a fault on its own**: a held-high enable that *should* be
high is correct. So the plain report stops saying "Looks healthy" and instead
says "no failures, but N worth a look" and names the net. A board whose firmware
only toggles a signal still reports "healthy." The advisory also appears in
`--json` as a note with `kind: "boot_control_net"`. Pass `--strict-boot` to fail
CI (exit 2) on it. By default it does not affect the exit code.

**The boot-state panel (`--plain` / `--json`).** Alongside the held-high warning,
co-sim prints an informational panel of every MOSFET/transistor gate it can
identify and what the firmware does to it at power-up, `driven HIGH and held`,
`driven LOW and held`, or `never driven (floating)`:

```
Power-up state of MOSFET / transistor gates: what the firmware does to each
switch the moment the board powers up. Verify each is the level you intend:
  Q1  IgnitOne   pulled HIGH (weak internal pull-up)  <- switched at power-up
  Q2  IgnitTwo   driven LOW and held
  Q3  FanGate    never driven (floating)              <- undefined until firmware drives it
```

The panel distinguishes a strong push-pull `driven HIGH` from a weak `pulled
HIGH (weak internal pull-up)`. The weak case is exactly the igniter fault (a
serial RX pin mis-mapped onto the gate enables its pull-up). Naming the
*mechanism* tells a non-engineer that the gate went high by accident, not by
design.

The panel is **reported, not judged**. That is what makes it safe for a
non-engineer, and it lets the panel cover cases the *warning* cannot. The
held-high **warning** is a verdict, and the only thing `--strict-boot` gates
on, so it must have zero false positives. The **panel** is data. It shows the
floating case (a forgotten gate-drive) and the held-low case without ever
asserting a fault, so an ambiguous net is just a line the user reads against
what they know the board is for: no false alarm, no CI break. The panel
deliberately makes no channel-type safety claim. A HIGH gate is "on" for a
low-side N-MOSFET but "off" for a high-side P-MOSFET, a distinction the copper
alone cannot show, so the panel reports the level and flags the
active/undefined ones to verify. hauksbee identifies gates by a
`G`/`GATE`/`B`/`BASE` pad name where present, else by footprint convention
(SOT-23 pin 1, Power-SO-8 pin 4, and similar cases). A transistor whose control
terminal cannot be reliably identified is omitted rather than mislabeled. In
`--json` the same rows appear under `boot_gates`.

For a deadline-gated, *named* pass/fail check on a specific net, use the
`boot-coverage` assertion in `hauksbee-ci` (`boot_gate_pass.toml` /
`boot_gate_fail.toml`). The held-high warning is exactly how the
`explosion33/RocketryIgniter` power-up ignition fault surfaces from one command.

### The `Mcu` trait and three backends

The `Mcu` trait defines a common lockstep surface, but implemented capabilities
and fidelity are backend- and part-specific. Missing or dropped paths are
reported rather than inferred. The trait lives in
`crates/hauksbee-mcu/src/traits.rs`. The backends live in
`crates/hauksbee-mcu/src/`.

Proof levels in this source tree, by named integration tests
(`crates/hauksbee-engine/tests/stm32_renode_cosim.rs`, `esp32_qemu_cosim.rs`,
`renode_riscv_arm_cosim.rs`): AVR, STM32, ESP32, and ESP32-C3 are proven
end-to-end (firmware drives a net through the solved circuit); nRF52840 and
FE310 are proven to UART boot; ESP32-S3 is wiring-proven (machine boots and
locksteps, app proof pending a flash image). On macOS/Linux the external
simulators install via `scripts/install-sims.sh`; Windows x64 uses
`scripts\install-sims-windows.ps1`. They are found automatically at runtime.

#### AVR, `AvrMcu` (libsimavr, linked in-process)

Backed by libsimavr, linked directly into the engine. The AVR co-sim runs
in-process, with no separate simulator to launch. simavr is GPL-3.0. This
Apache-2.0 repo deliberately does **not** vendor it, so a source build links it
from the system. Get there one of three ways:

- Install it with one command: `scripts/install-sims.sh --avr` (builds and
  installs libsimavr and libelf into the prefix the build links against).
- Point the build at an existing copy via `SIMAVR_INCLUDE_DIR` /
  `SIMAVR_LIB_DIR`.
- Build the GPL-free subset without AVR:
  `cargo build -p hauksbee-engine --no-default-features --features renode,qemu`.

Supported parts (anything simavr knows by name, plus two shipped convenience
constructors):

- **ATmega328P** at 16 MHz (`AvrMcu::atmega328p_16mhz()`), the Arduino Uno /
  Nano class
- **ATmega2560** and other simavr-known AVR MCUs via `AvrMcu::new("atmega2560",
  freq)`

Supply rails are configurable (`set_rails`). GPIO output hooks cover ports
A-H. UART, I2C (TWI), and SPI are all bidirectional.

Licensing note: a binary built with the `avr` feature links libsimavr (GPL-3.0)
and is subject to the GPL.

#### Renode, `RenodeBackend` (external, GPL-free over sockets)

Drives a headless Renode process over its Monitor TCP protocol and a UART
socket terminal. Links no GPL code. A `--no-default-features --features
renode,qemu` build is GPL-free.

Shipped named configs and proven support status (from `crates/hauksbee-mcu/src/renode/mod.rs`
and [`docs/cosim/MCU.md`](../cosim/MCU.md)):

| Config constructor | Platform | Proven in the current release |
|---|---|---|
| `RenodeConfig::stm32f103()` | STM32F103C8 (Cortex-M3, "blue pill"), PA-PG, USART1, 8 MHz HSI | **Proven**: UART boot banner, GPIO toggle, solved LED current |
| `RenodeConfig::stm32f4_discovery()` | STM32F4 Discovery (STM32F407, Cortex-M4), PA-PE, USART2, 16 MHz | Config shipped, not exercised by a named test |
| `RenodeConfig::nrf52840()` | nRF52840 (Cortex-M4, two 32-bit GPIO ports), gpio0/gpio1, uart0, 64 MHz | **Proven (UART boot)**: Zephyr shell `uart:~$` |
| `RenodeConfig::sifive_fe310()` | SiFive FE310 / HiFive1 (RISC-V RV32), one 32-bit GPIO port (`gpioInputs`), uart0, 16 MHz | **Proven (UART boot)**: Zephyr shell boot with PRCI clock fix |

| `RenodeConfig::rp2040()` | RP2040 (dual Cortex-M0+, Raspberry Pi Pico), 30-pin GPIO bank through the SIO, uart0, 125 MHz | **Proven**: stock pico-sdk firmware boots through the real boot ROM into `main`; UART0 banner, GP25 at 3.300 V through the solver, ADC inputs 0..3, I2C on `i2c0`/`i2c1` |

RP2040 is the one platform Renode does not supply: no rp2040 platform exists on
1.16.1 or on `master`, so hauksbee ships its own, with the peripheral models
vendored as C# that Renode compiles at run time from a support bundle unpacked
out of the binary. Nothing extra to install, but each machine creation compiles
about 377 kB of C#, which costs roughly eight seconds of bring-up. Not
available on it: the SPI bus-slave bridge (the vendored PL022 never dispatches
to a registered slave), PIO (upstream needs an x86-64-only native library), and
ADC input 4 (the on-die temperature sensor is not an external node). The second
core is declared and un-halted but no two-core firmware has been run.

nRF5340 (ZSWatch-class) has no platform in Renode 1.16.1 or on `master`, and
unlike RP2040 there is nothing to vendor: `renode-infrastructure` has no SPU
model (which the TrustZone-capable `cpuapp` image configures during start-up),
no nRF53 IPC model (`NRF_Bellboard` is the nRF54 mailbox), and nothing for the
network core. Cortex-M33 itself is supported and the nRF52-series peripheral
models exist, so the missing pieces are specific rather than total, but a
boot-only nRF5340 is days of model-writing rather than an afternoon of
vendoring. No config is claimed for it.

GPIO exchange mechanism: after each `RunFor` chunk the backend reads each
port's output-data register (ODR) over the Monitor, diffs the snapshot, and
fires per-bit edge callbacks. Offsets: STM32F1 ODR `0x0C`, STM32F4 `0x14`,
nRF52 `0x4` (peripheral-relative, see `crates/hauksbee-mcu/db/mcu/nrf52840.soc.toml`), FE310
`0x0C`. GPIO input: `sysbus.<port> OnGPIO <bit> <bool>`.

GPIO drive **direction** is also observable on the dir-mapped platforms. The
backend reads each port's direction/mode register alongside the ODR
(STM32F103 CRL/CRH, STM32F4 MODER, nRF52840 DIR, RP2040 SIO `GPIO_OE`), each
verified against the live model's read-back. It decodes a per-pin output mask
and reports it through the same `pins_configured_output` surface the AVR DDR
hooks feed. On those parts, the boot-state panel and `boot-coverage` can
therefore tell a **held-LOW output from a floating input**, just as on AVR.
FE310, which has no verified direction map, stays honestly direction-blind: its
diagnoses hedge ("undriven or driven LOW") instead of asserting Hi-Z.

ADC injection is wired per platform through an `AdcChannelMap` (a Monitor feed
command or result-word write). **No platform Renode itself ships carries a
map.** The stock STM32F103/F4/nRF52840 Renode 1.16.1 platform descriptions model
no ADC peripheral at all (verified live), and shipping a wrong-layout model or an
invented RAM word would be fake fidelity. RP2040 is mapped on inputs 0..3
because its converter is one of the models hauksbee vendors, so there is a real
peripheral to feed a voltage into. hauksbee DROPS an unmapped
channel's injections, and it surfaces that drop on all four batch report
surfaces (`hauksbee run` text, `--plain`, `--json` `CosimJson.adc_dropped` and
coverage notes, and all hauksbee-ci report formats), naming the channel, MCU,
and net. The interactive TUI carries it too, from the same signal through one
shared enumeration. The synchronous web report refuses the external backend
before this scheduler signal exists and points to live/CLI co-sim; the exact
per-surface matrix is in
[docs/cosim/MCU.md](../cosim/MCU.md#which-surface-carries-a-coverage-hole).
A board that knows where its counts land adds `[[soc.adc]]` to its own
descriptor, with no recompile needed.

I2C and SPI peripheral interception is wired through generated C# bridge
peripherals (an `II2CPeripheral` / `ISPIPeripheral` per slave address), so a
hardware-TWI/SPI sensor co-simulates on Renode exactly as it does on simavr.
See the `i2c_sensor_cosim_renode` / `spi_sensor_cosim_renode` integration
tests. Controllers by platform: STM32F103 (`i2c1`, `spi1`), STM32F4 Discovery
(`i2c1`, `spi2`/`spi3`), nRF52840 (`twi0`/`twi1`, `spi2`, names live-verified with
bridge registration, an end-to-end nRF sensor round-trip still awaits an nRF
bus firmware fixture), RP2040 (`i2c0`/`i2c1`, proven end-to-end in both
directions with real pico-sdk firmware; no SPI, because the vendored PL022
never dispatches to a registered slave). FE310 models no bus controllers.
hauksbee
records a sensor bound on such a platform as **unexercised** and surfaces that
on the same four batch surfaces, and a hauksbee-ci `peripheral` assertion
against it **fails** rather than green-passing on the slave's power-on defaults.
The run summary likewise prints the per-SPI-bus transaction-framing tier (exact /
backend / heuristic), flags it as a `--plain` heads-up and `--json` note when
heuristic, and attaches it to each CI peripheral assertion's detail. Exact
framing is reached two ways: the spec's `cs_net`, or the `cs` pin role of the
model bound to the component the peripheral's `ref` names, so a modelled slave
needs no chip-select net written out. Both routes still require the net to trace
to an MCU GPIO, so a buffered or externally driven chip-select stays heuristic,
and both are only as exact as the backend: CS edges are interleaved with the byte
stream on a push backend (simavr), while a poll backend samples GPIO once per
chunk and can miss a chip-select pulse inside one. The `--json` coverage records
which route it was. See
`docs/cosim/MCU.md` ("ADC / bus coverage by platform") for the full matrix.

#### Espressif QEMU, `QemuBackend` (external, ESP32 family)

Drives a headless Espressif QEMU process (the fork with full ESP32 SoC
peripheral models) over QMP + gdbstub control channels and a UART socket.
Renode as of 1.16.1 ships no `esp32.repl` or `esp32c3.repl`, so the ESP32 path
is a separate backend. This is not a fallback. It is the intended and tested
path.

Shipped named configs:

| Config constructor | Machine | Architecture | Proven in the current release |
|---|---|---|---|
| `QemuConfig::esp32()` | `esp32` | Xtensa LX6 (dual-core, 240 MHz) | **Proven**: UART boot, GPIO toggle, solved LED current, stable across runs |
| `QemuConfig::esp32s3()` | `esp32s3` | Xtensa LX7 (240 MHz) | **Wiring proven**: binds `qemu:esp32s3`, machine boots (blank flash), QMP/gdbstub/UART connect, lockstep steps. App proof pending an S3 flash image (needs esp-idf's esp32s3 toolchain) |
| `QemuConfig::esp32c3()` | `esp32c3` | RISC-V RV32IMC (160 MHz) | **Proven**: UART boot, GPIO toggle, solved LED current |

On the reviewed source build, GPIO observation reads the emulator's retained
low-bank OUT and ENABLE state through paired, live-probed `gpio-out` and
`gpio-enable` QOM properties. The backend therefore observes ordinary firmware
outputs and direction without a mailbox; edge synthesis remains poll-based like
Renode's ODR path. The pinned unpatched prebuilt still falls back loudly to the
legacy RTC-slow-memory output mailbox at `0x5000_0000`. GPIO input remains a
separate firmware contract pushed over the gdbstub `M` packet, and GPIO 32+
remains outside the proved descriptor bank.

ADC injection is not modeled by the Espressif QEMU fork. This is a documented
no-op.

### Installing the external simulators

The default Unix release bundle includes AVR support. Source builds install
libsimavr with `scripts/install-sims.sh --avr`; the Windows x64 permissive shape
does not include AVR. Install the external backends with:

```
scripts/install-sims.sh          # install Renode + Espressif QEMU
scripts/install-sims.sh --check  # verify hauksbee will find them
```

On Windows x64 PowerShell use `scripts\install-sims-windows.ps1` (or `-Check`).

Full discovery order, env-var overrides (`HAUKSBEE_RENODE`,
`HAUKSBEE_QEMU_XTENSA`, `HAUKSBEE_QEMU_RISCV32`, `HAUKSBEE_QEMU_DIR`), macOS
Gatekeeper steps, and manual install instructions are in
[`docs/cosim/SIMULATORS.md`](../cosim/SIMULATORS.md).

Integration tests skip cleanly when the simulator is absent rather than
failing.

---

## Layer 4: Behavioral goal assertions (`hauksbee-ci`)

`hauksbee-ci run <spec.toml>` is the CI gate. It boots the firmware on the
emulated PCB, runs the spec's simulated duration, evaluates every `[[assert]]`
block, and prints a per-assertion GREEN / RED verdict. A run with any failed
assertion exits non-zero, gating CI. `--junit <out.xml>` writes JUnit XML for
the CI test-report step. `--quiet` suppresses the human report while
preserving the exit code. GitHub Actions annotations appear automatically
when `GITHUB_ACTIONS` is set.

Exit codes: 0 all assertions passed, 1 at least one assertion failed, 2 spec
or usage error, 3 invalid-for-analysis (the run refuses to vouch for its own
result). The full contract is in [docs/ci/CI.md](../ci/CI.md#exit-codes-the-pipeline-contract).

### Spec anatomy

A spec is a TOML file checked in alongside the hardware design:

```toml
name = "power-up sanity"
board  = "hardware/board.kicad_pcb"
firmware = "firmware/build/app.elf"
mcu    = "atmega328p"   # informational label only: nothing reads it. The MCU is
                        # detected from the board part value + [[models]] kind="mcu"
                        # routing (this field cannot force a backend). See docs/ci/CI.md.
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

Board files: `.kicad_pcb`, `.kicad_sch`, `.brd`, `.PcbDoc`, `.net` (KiCad
netlist), `.d356`, Board-as-Code `.board`, or gerbers.

### Assertion kinds

All fourteen kinds, validated by the kind list in
`crates/hauksbee-ci/src/spec.rs` and implemented in
`crates/hauksbee-ci/src/assertions.rs`:

| Kind | What it checks |
|---|---|
| `voltage` | A net's voltage is within `[min, max]` (optionally `after_ms` settling time) |
| `uart` | UART output `contains` a substring or `matches` a regex |
| `toggle` | A net toggles at `freq_hz` ± `tolerance`, or at least `min_toggles` times |
| `no_faults` | No stress fault is raised at any point during the run |
| `max_current` | Peak current through a named component (`ref`) stays below `amps` |
| `max_temp` | Steady-state junction temperature of a named component stays below `celsius` (or the device's own max if omitted) |
| `peripheral` | A simulated peripheral's state: EEPROM byte sequence, sensor field in a range |
| `rail_window` | Voltage bounds within a named scenario's load window, dip duration, and recovery time |
| `protection_trip` | Whether a battery BMS protection circuit trips (or must not trip) |
| `boot-coverage` | A control net is driven to at least `min` volts within `deadline_ms` of reset, with no fault during the boot window. Answers whether firmware drives a Hi-Z control input in time |
| `phase_margin` | Loop phase margin from the AC sweep is within `[min, max]` degrees |
| `ac_gain` | A net's AC gain is within `[min, max]` dB at an optional `freq_hz` |
| `hwtrace` | Checks simulated waveforms against a scope CSV or logic-analyzer VCD. Provenance is explicit (`real` or `synthetic`); bundled fixtures are synthetic. |
| `model_coverage` | Pins the fraction of active ICs that must bind to a device model |

Additional spec features: `[[supply]]` (ideal / bench / wall / USB / battery
models with ripple and BMS protection), `[[net_drive]]` (force a net to a
fixed voltage), `[[peripheral]]` (attach pushbuttons, toggles,
potentiometers, encoders, stimulus sources, I2C/SPI device models, and VCD
sinks, with event timelines), `[[scenario]]` (transient load profiles for
inrush / sag / brownout dynamics), `[fuzz]` (run across N random
initial-state seeds, every assertion must hold on every seed), and `[ac]`
(drive the `phase_margin` and `ac_gain` assertions).

### Canonical demo: `boot_gate_pass.toml` / `boot_gate_fail.toml`

These two specs in `crates/hauksbee-ci/examples/` are the canonical demo of
the behavioral layer. Both point at the same board (a bare ATmega328P driving a
2N7002 MOSFET gate with no pull resistor on the gate net, so at reset the net
is genuinely undefined, a case the netlist alone cannot adjudicate). They
differ only in firmware:

- `boot_gate_pass.toml`, firmware A configures PB0 as an output and drives it
  HIGH promptly. The `boot-coverage` assertion passes: `GATE_CTRL` reaches
  ≥ 3.0 V within 20 ms of reset.
- `boot_gate_fail.toml`, firmware B never configures PB0. The gate floats for
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
makes device models respond dynamically during co-simulation, not just at DC.
This is how `boot-coverage` can observe a net transitioning mid-run and how
`rail_window` can observe voltage sag under a transient load.

---

## Layer 5: Board-as-Code

Three commands form the edit-simulate loop:

- **`hauksbee to-code <board>`**: decompile a `.kicad_pcb` into editable
  Board-as-Code text.
- **`hauksbee from-code <code>`**: recompile Board-as-Code back into a
  `.kicad_pcb`.
- **`hauksbee check-code <code> [--seconds N] [--destructive]`**: recompile,
  bind, run the stress monitor for `--seconds` of simulated time (default 0.2 s),
  and print a fault report. Exits non-zero if a fault is raised. Add
  `--destructive` to let the stress monitor destroy parts (shows consequences
  of miswire or over-stress). Add `--ambient <C>` for the thermal estimate.

`check-code` drops straight into a script or pre-commit hook.

The rest of the CLI surface, one line each:

- **`hauksbee merge-ses <code> <ses>`**: the return half of `from-code
  --route-dsn`: merge a FreeRouting `.ses` session back into the recompiled
  board.
- **`hauksbee sim <deck>`**: run a SPICE deck through the solver directly.
- **`hauksbee watch <board|code|spec>`**: re-run the right check on every
  file change (a `.kicad_pcb` runs `run --check`, a `.board` runs
  `check-code`, a `.toml` runs the spec).
- **`hauksbee doctor`**: report which co-sim backends this build can reach
  and where each simulator was found.
- **`hauksbee install esp-qemu`**: download Espressif's official prebuilt
  QEMU fork into the discovery path.

---

## The MCP server (`hauksbee-mcp`)

`hauksbee-mcp` is the third binary in every release bundle (alongside
`hauksbee` and `hauksbee-ci`): a stdio MCP server exposing the same engine,
so a coding agent can analyze boards, run specs, and read reports over
JSON-RPC without shelling out to the CLI. Documented in
[`crates/hauksbee-mcp/README.md`](../../crates/hauksbee-mcp/README.md).

---

## Input formats

`hauksbee run` and `hauksbee-ci` accept `.kicad_pcb`, `.kicad_sch`,
`.brd` (Eagle), `.PcbDoc` (Altium, see [`docs/ingest/ALTIUM.md`](../ingest/ALTIUM.md)),
`.net` (KiCad netlist export), `.d356` (IPC-D-356 netlist),
Board-as-Code `.board`, or a directory of gerbers
(reverse-extracted from copper geometry alone, see [`docs/ingest/GERBER.md`](../ingest/GERBER.md)).

---

## Adding your own parts & chips

The stock model library does not know every part on your board. An unmodeled
active part binds OPEN, and the report says so ("N of M critical parts
modeled", "simulated as OPEN"). Closing that gap is deliberately data-only:
no recompile, and hauksbee validates and fails loud at load time.

- **An analog part** (LDO, op-amp, diode, BJT, MOSFET, comparator) needs one
  `[[models]]` TOML entry: [`extending/add-an-analog-part.md`](../extending/add-an-analog-part.md).
- **An I2C/SPI sensor** (register map) needs one TOML file:
  [`extending/add-a-sensor.md`](../extending/add-a-sensor.md).
- **A whole MCU / chip**, so its firmware co-simulates on the *exact* part
  rather than a substitute core, needs a SoC descriptor plus a routing entry,
  two TOML files: [`extending/add-an-mcu-variant.md`](../extending/add-an-mcu-variant.md).
  Run `hauksbee models list --builtin` to see the chips already shipped, and
  copy the closest one as a template. When co-sim runs on a substitute core,
  it says so and points you at this recipe.

Share a bundle of any of the above as a versioned [model pack](../models/PACKS.md)
(`hauksbee models add <path|url>`). Full menu: [`extending/README.md`](../extending/README.md).

## Where to go next

- [`docs/cosim/SIMULATORS.md`](../cosim/SIMULATORS.md), install Renode and Espressif QEMU,
  discovery order, env-var overrides
- [`docs/cosim/ORACLES.md`](../cosim/ORACLES.md), the DRC and analog oracles
- [`docs/cosim/MCU.md`](../cosim/MCU.md), full co-simulation architecture, per-board recipes,
  proven integration test results
- [`docs/ci/CI.md`](../ci/CI.md), GitHub Action, KiCad plugin, pre-commit hook
- [`docs/ci/EXAMPLES.md`](../ci/EXAMPLES.md), runnable examples
- [`docs/analysis/AC_ANALYSIS.md`](../analysis/AC_ANALYSIS.md), AC sweep and loop-stability details
- [`docs/checks/THERMAL.md`](../checks/THERMAL.md), thermal analysis details
