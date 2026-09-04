# Start here

Hauksbee executes the evidence that describes a real electronic product. Give
it a layout, schematic, fab archive, BOM, placement file, or compiled
firmware. It reconstructs the circuit the copper implements, binds device
models, runs static and numerical checks, and can boot the firmware against
the solved board. If an active part has no adequate model, Hauksbee names it,
keeps the valid partial result, and refuses the stronger conclusion.

## Build and first run

```bash
scripts/install-sims.sh --avr        # libsimavr (AVR co-sim); Renode/QEMU are optional
cargo build --release
target/release/hauksbee doctor --json
target/release/hauksbee run --example blinky --check --plain
target/release/hauksbee serve
```

In the browser, drop a board on the page. You get the plain-language verdict,
the full report, live 2D copper with probes, and a checks composer that runs
the same `hauksbee-ci` engine.

## The CLI in four commands

```bash
hauksbee run board.kicad_pcb --check --plain      # every static check, readable
hauksbee run board.kicad_pcb --check --json       # the same, machine-readable
hauksbee run board.kicad_pcb --firmware fw.hex --headless --seconds 2
hauksbee-ci run ci/board.toml --junit out.xml     # assertions as a CI gate
```

`--strict` turns findings into exit 2 and an untrustworthy result into exit 3.

## Where to read next

- [CI specs and assertions](ci/CI.md), with [examples](ci/EXAMPLES.md).
- [Models: coverage, packs, authoring](models/MODELS.md).
- [MCU co-simulation backends](cosim/MCU.md).
- [Input formats](ingest/) and [checks](checks/).
- [Capabilities](about/CAPABILITIES.md) and [limitations](about/LIMITATIONS.md).
