# Start here

Hauksbee reads a layout, schematic, fab archive, BOM, placement file or
compiled firmware, reconstructs the circuit the copper implements, binds device
models, runs static and numerical checks, and can boot the firmware against the
solved board. A part with no adequate model is named and bound open; the
stronger conclusion is refused rather than guessed.

## Install, or build

One line installs the released binaries (`hauksbee`, `hauksbee-ci`,
`hauksbee-mcp`) and verifies their checksums:

```bash
curl -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.sh | bash
```

Windows uses `irm https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.ps1 | iex`.
From a checkout:

```bash
scripts/install-sims.sh --avr        # libsimavr (AVR co-sim); Renode/QEMU are optional
cargo build --release
target/release/hauksbee doctor --json
target/release/hauksbee run --example blinky --check --plain
target/release/hauksbee serve        # drop a board in the browser
```

## The CLI in four commands

```bash
hauksbee run board.kicad_pcb --check --plain      # every static check, readable
hauksbee run board.kicad_pcb --check --json       # the same, machine-readable
hauksbee run board.kicad_pcb --firmware fw.hex --headless --seconds 2
hauksbee-ci run ci/board.toml --junit out.xml     # assertions as a CI gate
```

`--strict` turns findings into exit 2 and an untrustworthy result into exit 3.

## Where to read next

- [Capabilities](about/CAPABILITIES.md): every command and flag, one page.
- [CI specs and assertions](ci/CI.md), with [examples](ci/EXAMPLES.md).
- [Models: coverage, packs, authoring](models/MODELS.md).
- [MCU co-simulation backends](cosim/MCU.md) and [installing them](cosim/SIMULATORS.md).
- Input formats: [BOM](ingest/BOM.md), [DNP](ingest/DNP.md), [Eagle](ingest/EAGLE.md),
  [Altium](ingest/ALTIUM.md), [Gerber](ingest/GERBER.md), [schematics](ingest/SCHEMATICS.md).
- Checks: [shorts](checks/SHORTS.md), [SI](checks/SI_CHECKS.md),
  [thermal](checks/THERMAL.md), [transients](checks/TRANSIENTS.md),
  [resource conflicts](checks/RESOURCE_CONFLICTS.md), [device decode](checks/DEVICE_DECODE.md).
- [Limitations](about/LIMITATIONS.md).

Releases and the issue tracker live at <https://github.com/hauksbee-dev/hauksbee>.
