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

Full walkthrough, more example boards, and captured sessions: [`docs/EXAMPLES.md`](EXAMPLES.md).

## Your next four reads

1. [`docs/EXAMPLES.md`](EXAMPLES.md): get it running. Install, the first useful
   result in a minute, and every runnable example and script.
2. [`docs/CI.md`](CI.md): the point of the tool. Boot the board headless on every
   commit and assert on it, with a GitHub Action, JUnit output, and exit codes.
3. [`docs/MCU.md`](MCU.md): MCU co-simulation. Which chips are supported (AVR,
   STM32, ESP32/-C3, nRF52840, RISC-V) and how firmware couples to the analog solve.
4. [`docs/ARCHITECTURE.md`](ARCHITECTURE.md): the mental model. How a board file
   becomes an extracted circuit, a bound model set, a partitioned solve, and a
   co-simulation.

From there, the rest of `docs/` covers each capability in depth: the authoritative
scope document ([`CAPABILITIES.md`](CAPABILITIES.md), every layer plus the common
misconceptions), analysis ([`AC_ANALYSIS.md`](AC_ANALYSIS.md),
[`THERMAL.md`](THERMAL.md), [`TRANSIENTS.md`](TRANSIENTS.md)), checks
([`SHORTS.md`](SHORTS.md), [`SI_CHECKS.md`](SI_CHECKS.md),
[`RESOURCE_CONFLICTS.md`](RESOURCE_CONFLICTS.md),
[`DEVICE_DECODE.md`](DEVICE_DECODE.md)), ingest ([`SCHEMATICS.md`](SCHEMATICS.md),
[`GERBER.md`](GERBER.md), [`ALTIUM.md`](ALTIUM.md),
[`BOARD_AS_CODE.md`](BOARD_AS_CODE.md)), models ([`MODELS.md`](MODELS.md),
[`PERIPHERALS.md`](PERIPHERALS.md)), backends and cross-checking
([`SIMULATORS.md`](SIMULATORS.md), [`ORACLES.md`](ORACLES.md)), deployment
([`DOCKER.md`](DOCKER.md)), positioning ([`COMPARISON.md`](COMPARISON.md)), and the
honest [`LIMITATIONS.md`](LIMITATIONS.md).

## Going deeper

- [`docs/how-and-why/`](how-and-why/): the code companion explanations, how each
  crate works and why it is built that way.
- [`docs/dev-plans/`](dev-plans/): the design plans and research behind the tool's
  direction (design history).
- [`docs/record/`](record/): the project's evidence trail. The bug-hunt campaigns,
  known-fault calibration, flagship benchmark, and how the tool was proven honest.
- [`docs/hunts/`](hunts/): the bug-hunt working directory, per-board narratives.
- [`docs/DOCS_MAP.md`](DOCS_MAP.md): the full classification of every doc.
