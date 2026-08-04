# Start here

Hauksbee is CI for hardware. Hand it a real PCB design file (KiCad, Eagle,
Altium `.PcbDoc`, IPC-D-356, or gerber-only fab output). Hauksbee works out
the circuit the copper actually implements, runs that circuit headless with
real device physics, and co-simulates the firmware on an emulated MCU. It
then checks the board the way a test suite checks a function: rails,
faults, shorts, USB-C compliance, signal integrity, thermal, loop
stability. A bug that used to cost a fab spin and a fortnight at the bench
now fails a test instead.

## Install and first run

**On a Mac, no terminal needed:** download `Hauksbee.app` (the
`hauksbee-<version>-darwin-<arch>-app.zip` asset) from the
[releases page](https://github.com/hauksbee-dev/hauksbee/releases). Unzip it and
double-click it. Your browser opens on the drop-zone. Drop a board and read
the report. Released apps are signed and notarised, so Gatekeeper accepts a
plain double-click; the full signing story is under the installer below
(mechanics: [`app/macos/SIGNING.md`](../app/macos/SIGNING.md)). The app works
on macOS only today. Windows support is tracked separately in
[`release-and-licensing.md`](about/release-and-licensing.md), and Linux
users take the installer line below.

**From a terminal**, use either the one-line installer:

```bash
curl -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.sh | bash
```

or one command that builds and installs the binaries from a checkout, then
points it at the bundled Watchy smartwatch board.

```bash
scripts/install.sh                                      # build hauksbee + hauksbee-ci + hauksbee-mcp onto PATH
hauksbee run crates/hauksbee-ci/examples/boards/watchy.kicad_pcb --report --plain
hauksbee serve                                          # web front door (long-running; Ctrl-C to stop)
```

**macOS signing, stated plainly.** Every macOS release binary is signed with
a Developer ID identity. `Hauksbee.app` is signed and notarised with the
ticket stapled, and the release workflow refuses to publish an app zip that is
not, so the app opens on a double-click with no Gatekeeper warning. The
tarball binaries are signed too, and notarised from launch onward; a bare
command-line binary cannot carry a stapled ticket, so Gatekeeper confirms the
notarisation online on first run, and a tarball fetched through a browser
opens cleanly. Only a pre-release or locally built unsigned bundle still needs
the one-time fallback `xattr -d com.apple.quarantine ~/.local/bin/hauksbee
~/.local/bin/hauksbee-ci ~/.local/bin/hauksbee-mcp`, while a copy installed by
the curl line above never carries the quarantine flag at all.

Parts hauksbee does not recognise bind OPEN (simulated as disconnected) and
are reported, never silently guessed. On the Watchy board, 59 of 67
non-ignored parts resolve, with the MCU bound, so a few warnings about
unresolved actives are expected, and the report's bottom line explains
exactly what they mean for the results.

No board of your own yet? Two routes, neither needing a file. In the terminal,
`hauksbee run --example blinky --report` works from a bare installed binary with
no checkout on disk: the board is compiled into the binary and unpacked to a temp
directory, so there is no path to get wrong. `hauksbee sim --example rlc_ringdown
--tran` is the SPICE-side equivalent, and either flag names what it does have if
you ask it for something it lacks. In the browser, the `hauksbee serve` page has
one-click samples: a real smartwatch, a board-plus-firmware pair that runs a live
co-sim, and a minimal board to compare against.

Full walkthrough, more example boards, and captured sessions: [`docs/ci/EXAMPLES.md`](ci/EXAMPLES.md).

## Prepare your own project

[`PROJECT_LAYOUT.md`](PROJECT_LAYOUT.md) is the step-by-step guide. It
covers the three files hauksbee needs at most, where each comes from (KiCad
`.kicad_pcb` as is, Eagle `.brd`, Altium `.PcbDoc`, a zip of gerbers,
PlatformIO firmware), a recommended repo layout, and the one local command
that runs it. CI wrappers are optional, and you add them later.

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

From there, the rest of `docs/` covers each capability in depth:

| File | Covers |
|---|---|
| [`about/CAPABILITIES.md`](about/CAPABILITIES.md) | Authoritative scope: every layer, the full MCU coverage matrix |
| [`analysis/AC_ANALYSIS.md`](analysis/AC_ANALYSIS.md) | AC / small-signal sweep: Bode, phase margin, gain crossover |
| [`checks/THERMAL.md`](checks/THERMAL.md) | Steady-state junction-temperature check |
| [`checks/TRANSIENTS.md`](checks/TRANSIENTS.md) | Transient scenarios: load steps, brownout, inrush |
| [`checks/SHORTS.md`](checks/SHORTS.md) | Copper short / clearance detection and simulation |
| [`checks/SI_CHECKS.md`](checks/SI_CHECKS.md) | Signal-integrity static checks (`--si`) |
| [`checks/RESOURCE_CONFLICTS.md`](checks/RESOURCE_CONFLICTS.md) | MCU internal resource-conflict check |
| [`checks/DEVICE_DECODE.md`](checks/DEVICE_DECODE.md) | Config-pin divider decode check |
| [`ingest/SCHEMATICS.md`](ingest/SCHEMATICS.md) | Schematic (`.kicad_sch`) extraction |
| [`ingest/GERBER.md`](ingest/GERBER.md) | Gerber + pick-and-place reverse extraction |
| [`ingest/ALTIUM.md`](ingest/ALTIUM.md) | Altium `.PcbDoc` binary ingest |
| [`ingest/BOARD_AS_CODE.md`](ingest/BOARD_AS_CODE.md) | Decompile / edit / recompile a board as editable code |
| [`models/MODELS.md`](models/MODELS.md) | Device models: built-in, SPICE, datasheet extraction |
| [`cosim/PERIPHERALS.md`](cosim/PERIPHERALS.md) | Runtime peripherals (buttons, pots, I2C/SPI slaves) |
| [`cosim/SIMULATORS.md`](cosim/SIMULATORS.md) | Installing the external backends (Renode, Espressif QEMU) |
| [`cosim/ORACLES.md`](cosim/ORACLES.md) | Ground-truth oracles (`--oracle`): kicad-cli, ngspice |
| [`ci/DOCKER.md`](ci/DOCKER.md) | Container images and how to run them |
| [`about/COMPARISON.md`](about/COMPARISON.md) | Feature matrix and positioning vs the field |
| [`about/LIMITATIONS.md`](about/LIMITATIONS.md) | Honest limitations, triage and closure |

## Add your own parts and chips

A part hauksbee does not recognise binds OPEN, meaning it is simulated as
disconnected rather than silently guessed. The report says "N% resolved"
(the share of parts bound to a device model) and names the part. Closing
that gap needs one small TOML file and no recompile:

- an **analog part** (LDO, op-amp, diode, BJT, MOSFET, comparator):
  [`extending/add-an-analog-part.md`](extending/add-an-analog-part.md)
- an I2C/SPI **sensor**: [`extending/add-a-sensor.md`](extending/add-a-sensor.md)
- a **new MCU / chip**, so its firmware co-sims *exactly* instead of on a
  substitute core:
  [`extending/add-an-mcu-variant.md`](extending/add-an-mcu-variant.md) (two
  TOML files). `hauksbee models list --builtin` shows the chips already
  shipped.

The full menu is in [`extending/README.md`](extending/README.md).

## Going deeper

[`docs/DOCS_MAP.md`](DOCS_MAP.md) maps every doc in the tree to the question it
answers.
