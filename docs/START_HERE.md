# Start here

Hauksbee executes the evidence that describes a real electronic product. Give
it a layout, schematic, fab archive, BOM, placement file, fitted variant, or
compiled firmware. It reconstructs the circuit the copper implements, binds
device models, runs static and numerical checks, and can boot the firmware
against the solved board. CI is an optional consumer of those results, not the
definition of the tool.

The unusual part is the cross-checking. IPC-D-356 inside a fab archive can be
the authority for connectivity; a contradictory BOM can stop analysis before
model binding; a firmware-controlled net can remain undecidable statically and
become testable only when the firmware runs. If an active part has no adequate
model, Hauksbee names it, keeps the valid partial result, and refuses the
stronger conclusion instead of guessing.

## Install and first run

The quickest path needs no terminal at all: on macOS, download `Hauksbee.app`
from the [latest release](https://github.com/hauksbee-dev/hauksbee/releases/latest),
unzip, and double-click. It starts the engine on your machine and opens the
web interface in your browser, with sample boards ready to run. The app is
signed and notarised; nothing you drop on it leaves your machine.

For the CLI, one line downloads and installs the released binaries
(`hauksbee`, `hauksbee-ci`, `hauksbee-mcp`), verifying checksums:

```bash
curl -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.sh | bash
```

Windows uses the PowerShell twin
(`irm .../scripts/get-hauksbee.ps1 | iex`), and CI can pull
`ghcr.io/hauksbee-dev/hauksbee:slim` or `:full` instead; see
[DOCKER](ci/DOCKER.md).

From a checkout, the first command checks or installs simulator
prerequisites; the second builds the web front door and the three binaries,
then installs them:

```bash
scripts/install-sims.sh --check
scripts/install.sh
hauksbee doctor --json
hauksbee run --example blinky --check --plain
hauksbee serve
```

In the browser, the `hauksbee serve` page has one-click samples: a real
smartwatch, a board-plus-firmware pair that runs a live co-sim, and a minimal
board to compare against.

Curious about the solver rather than the board checks? `hauksbee sim --example
rlc_ringdown --tran` runs a bundled SPICE deck. It is not a first run: it writes
about 1,100 rows of raw CSV to stdout, one per timestep, with no verdict. Send it
somewhere you can plot it (`--out ring.csv`, and `--print V(out)` to keep one
column), or use `--op` for the single-row operating point instead.

Full walkthrough, more example boards, and captured sessions: [`docs/ci/EXAMPLES.md`](ci/EXAMPLES.md).

## Prepare your own project

[`PROJECT_LAYOUT.md`](PROJECT_LAYOUT.md) is the step-by-step guide. It
covers the three files hauksbee needs at most, where each comes from (KiCad
`.kicad_pcb` as is, Eagle `.brd`, Altium `.PcbDoc`, a zip of gerbers,
PlatformIO firmware), a recommended repo layout, and the one local command
that runs it. CI wrappers are optional, and you add them later.

## Your next four reads

1. [`docs/about/CAPABILITIES.md`](about/CAPABILITIES.md): the authoritative
   scope and per-backend evidence matrix.
2. [`docs/about/ARCHITECTURE.md`](about/ARCHITECTURE.md): how a board or fab
   input becomes an extracted circuit, bound model set, solve, and co-simulation.
3. [`docs/models/MODELS.md`](models/MODELS.md): inspect unresolved parts,
   scaffold or extract a model, validate it, and prove that it binds.
4. [`docs/models/BOARD_MODELING_WORKFLOW.md`](models/BOARD_MODELING_WORKFLOW.md):
   click-to-model browser workflow, approval-gated pack preparation, full
   behavior layers, reference-board proof, and multi-board regeneration.
5. [`docs/ci/EXAMPLES.md`](ci/EXAMPLES.md): runnable examples and, when useful,
   the CI/spec surface over the same engine.

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
that gap needs model data and no recompile. Start with the board itself:

```bash
hauksbee models resolve board.kicad_pcb --json
hauksbee models new U3 --board board.kicad_pcb --out models/u3.toml
hauksbee models lint models/u3.toml
hauksbee run board.kicad_pcb --models-dir models --report --plain
```

The scaffold deliberately refuses to guess an IC kind or datasheet value.
Supply those from identified source material, or use the consent-gated
`models extract --pdf ... --part ...` workflow to create a draft that must
still pass review, lint, binding, and positive/negative tests.

The worked formats are:

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

[`docs/DOCS_MAP.md`](DOCS_MAP.md) maps every doc in the shipped set to the
question it answers. The development tree also carries internal working notes
(plans, hunt logs, teaching drafts) that the map deliberately does not index and
a release does not carry.
