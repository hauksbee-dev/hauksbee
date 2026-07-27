# Start here

Hauksbee is CI for hardware. Hand it a real PCB design file (KiCad, Eagle, Altium
`.PcbDoc`, IPC-D-356, or gerber-only fab output) and it works out the circuit the
copper actually implements, runs that circuit headless with real device physics,
co-simulates the firmware on an emulated MCU, and checks the board the way a test
suite checks a function: rails, faults, shorts, USB-C compliance, signal
integrity, thermal, loop stability. The bug that used to cost a fab spin and a
fortnight at the bench fails a test instead.

## Install and first run

One command builds and installs both binaries; then point it at the bundled
blinky board.

```bash
scripts/install.sh                                      # build hauksbee + hauksbee-ci onto PATH
hauksbee serve                                          # web front door: drop a board, read the report
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --report
```

No board of your own yet? The `hauksbee serve` page has one-click samples
(a small clean board, a real smartwatch, and a board + firmware pair that runs
a live co-sim), so the first report needs no file at all.

Full walkthrough, more example boards, and captured sessions: [`docs/ci/EXAMPLES.md`](ci/EXAMPLES.md).

## Prepare your own project

[`PROJECT_LAYOUT.md`](PROJECT_LAYOUT.md) is the hand-holding guide: the (at
most) three files hauksbee needs, where each comes from (KiCad `.kicad_pcb` as
is, Eagle `.brd`, Altium `.PcbDoc`, a zip of gerbers, PlatformIO firmware), a
recommended repo layout, and the one local command that runs it. CI wrappers
are optional and come later.

## Your next four reads

1. [`docs/ci/EXAMPLES.md`](ci/EXAMPLES.md): get it running. Install, the first useful
   result in a minute, and every runnable example and script.
2. [`docs/ci/CI.md`](ci/CI.md): the point of the tool. Boot the board headless on every
   commit and assert on it, with a GitHub Action, JUnit output, and exit codes.
3. [`docs/cosim/MCU.md`](cosim/MCU.md): MCU co-simulation. Which chips are supported (AVR,
   STM32, ESP32/-C3, nRF52840, RISC-V) and how firmware couples to the analog solve.
4. [`docs/about/ARCHITECTURE.md`](about/ARCHITECTURE.md): the mental model. How a board file
   becomes an extracted circuit, a bound model set, a partitioned solve, and a
   co-simulation.

From there, the rest of `docs/` covers each capability in depth: the authoritative
scope document ([`CAPABILITIES.md`](about/CAPABILITIES.md), every layer and the full
MCU coverage matrix), analysis ([`AC_ANALYSIS.md`](analysis/AC_ANALYSIS.md),
[`THERMAL.md`](checks/THERMAL.md), [`TRANSIENTS.md`](checks/TRANSIENTS.md)), checks
([`SHORTS.md`](checks/SHORTS.md), [`SI_CHECKS.md`](checks/SI_CHECKS.md),
[`RESOURCE_CONFLICTS.md`](checks/RESOURCE_CONFLICTS.md),
[`DEVICE_DECODE.md`](checks/DEVICE_DECODE.md)), ingest ([`SCHEMATICS.md`](ingest/SCHEMATICS.md),
[`GERBER.md`](ingest/GERBER.md), [`ALTIUM.md`](ingest/ALTIUM.md),
[`BOARD_AS_CODE.md`](ingest/BOARD_AS_CODE.md)), models ([`MODELS.md`](models/MODELS.md),
[`PERIPHERALS.md`](cosim/PERIPHERALS.md)), backends and cross-checking
([`SIMULATORS.md`](cosim/SIMULATORS.md), [`ORACLES.md`](cosim/ORACLES.md)), deployment
([`DOCKER.md`](ci/DOCKER.md)), positioning ([`COMPARISON.md`](about/COMPARISON.md)), and the
honest [`LIMITATIONS.md`](about/LIMITATIONS.md).

## Add your own parts & chips

A part hauksbee doesn't recognise binds OPEN (the report says "N% resolved" and
names it), closing that gap is one small TOML file, no recompile:

- an **analog part** (LDO, op-amp, diode, BJT, MOSFET, comparator) →
  [`extending/add-an-analog-part.md`](extending/add-an-analog-part.md)
- an I2C/SPI **sensor** → [`extending/add-a-sensor.md`](extending/add-a-sensor.md)
- a **new MCU / chip** so its firmware co-sims *exactly* instead of on a
  substitute core → [`extending/add-an-mcu-variant.md`](extending/add-an-mcu-variant.md)
  (two TOML files). `hauksbee models list --builtin` shows the chips already shipped.

The full menu is [`extending/README.md`](extending/README.md).

## Going deeper

[`docs/DOCS_MAP.md`](DOCS_MAP.md) maps every doc in the tree to the question it
answers.
