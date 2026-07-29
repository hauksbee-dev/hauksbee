# Project layout: how to set up a board so hauksbee can run it

You do **not** need a CI pipeline to use hauksbee. The core workflow is one
local command pointed at a few files that live in your hardware repo.
GitHub Actions, a KiCad plugin, and a pre-commit hook are optional wrappers
around that same command. Start local, and add them later if you want.

## The minimum you need

To boot-and-check a board, hauksbee needs, at most, three things:

1. **A board file**: the design hauksbee extracts the circuit from. Any
   supported format: `*.kicad_pcb`, `*.kicad_sch`, Eagle `*.brd`, Altium
   `*.PcbDoc`, IPC-D-356 `*.d356`, or a gerber set.
2. **Firmware (optional)**: a compiled `*.elf` (or `*.hex`) for the board's MCU,
   if you want the firmware co-simulated in lockstep with the analogue circuit.
   Omit it for a purely electrical check.
3. **A spec**: a small TOML file listing the power supplies to attach and the
   assertions that must hold. This is what turns "simulate the board" into "pass
   or fail the build."

## A recommended directory layout

Keep the spec checked in next to the board. This way, a hardware change and
the test that guards it move together:

```
my-board/
├── hardware/
│   └── my_board.kicad_pcb        # the design file hauksbee reads
├── firmware/
│   ├── src/…                     # your firmware source
│   └── build/my_board.elf        # the compiled ELF hauksbee co-simulates
├── ci/
│   ├── power-up.toml             # spec: rails come up, no faults
│   └── wifi-burst.toml           # spec: a load transient must not brown out
└── README.md
```

Nothing here is mandatory. Hauksbee takes explicit paths, so you can arrange
files however you like. This layout is only the one the examples assume.

## The spec file

A spec describes **one** headless co-simulation and what must be true for it to
pass. The board and (optional) firmware are referenced by path, relative to the
spec file:

```toml
name = "my_board power-up"
board = "../hardware/my_board.kicad_pcb"
firmware = "../firmware/build/my_board.elf"   # omit for an electrical-only run
duration_ms = 50

# Attach the supplies the board expects.
[[supply]]
net = "+3.3V"
kind = "wall"
volts = 3.3

# Assert what must hold.
[[assert]]
kind = "rail"
net = "+3.3V"
min = 3.2
max = 3.4

[[assert]]
kind = "no_faults"          # nothing exceeds its rating
```

The available supply kinds, scenario/load profiles, and assertion kinds (`rail`,
`rail_window`, `no_faults`, `uart`, `temperature`, loop stability, hardware-trace
comparison, …) are documented in [`CI.md`](ci/CI.md), with runnable examples in
[`EXAMPLES.md`](ci/EXAMPLES.md) and under [`../examples/ci-specs/`](../examples/ci-specs/).

## Running it, locally, one command

```bash
hauksbee-ci run ci/power-up.toml
echo $?          # 0 = green, 1 = red
```

That is the whole loop. Hauksbee extracts the circuit from the board file,
binds every component to a device model, attaches the supplies, and boots
the firmware on the emulated MCU, if given. It then runs the simulation and
evaluates the assertions. It exits non-zero if any assertion fails, and it
writes a JUnit XML report and inline annotations any CI system can ingest.

For a quick look without a spec, point `hauksbee` straight at the board:

```bash
hauksbee run hardware/my_board.kicad_pcb --report --plain     # what got modelled + a plain verdict
hauksbee run hardware/my_board.kicad_pcb --firmware firmware/build/my_board.elf --plain
```

## Adding a pipeline later (optional)

When you want the check to run on every push, the same spec drops into CI
unchanged:

- **GitHub Actions**: see [`../integrations/github-action/`](../integrations/github-action/).
- **KiCad plugin**: run it from the PCB editor: [`../integrations/kicad-plugin/`](../integrations/kicad-plugin/).
- **pre-commit hook**: gate commits locally: [`../integrations/pre-commit/`](../integrations/pre-commit/).

Each of these just invokes `hauksbee-ci run <spec>`, the same command you
already run by hand.
