# Architecture

Hauksbee takes a PCB design, extracts the circuit it implements, binds device
models, and simulates it: a fast analog solve co-simulated with an emulated
microcontroller, rendered on the actual layout.

## Pipeline

```
board / schematic / fab files / BOM / placement
        |  hauksbee-extract      (readers, DRC geometry, lint, SI)
        v
   ExtractedBoard
        |  hauksbee-models + hauksbee-bind binder     (device models)
        v
   Circuit IR (hauksbee-ir)
        |  hauksbee-solve        (DC, transient, AC)   <->   hauksbee-mcu (AVR / Renode / QEMU)
        v
   scheduler + peripherals (hauksbee-cosim), static checks (hauksbee-checks),
   stress monitor (hauksbee-bind), reports + front door   (hauksbee-engine)
        |
        +-- hauksbee (CLI + web front door, hauksbee-server + frontend/)
        +-- hauksbee-ci (specs, assertions, JUnit)
        +-- hauksbee-mcp (stdio MCP server)
```

## Solver

1. **Partition before solving.** The connectivity graph is split at device
   boundaries into islands. Linear islands are solved in state-space form by
   matrix exponential (exact at any step); digital parts are event-driven;
   only nonlinear analog islands pay for MNA + Newton, each on its own small
   matrix.
2. **Device models are SPICE-level** (diode, BJT, MOSFET, temperature
   dependent), validated against ngspice. Behavioural models cover digital
   and power ICs.
3. **Every effect is toggleable** (parasitics, temperature dependence, charge
   storage, tolerances) for debugging and speed.

## Checks

| Check | Flag | Doc |
|---|---|---|
| Copper shorts / clearance | `--drc` | [SHORTS](../checks/SHORTS.md) |
| Connectivity lint, straps, resource conflicts, device decode | `--lint`, `--resources` | [RESOURCE_CONFLICTS](../checks/RESOURCE_CONFLICTS.md), [DEVICE_DECODE](../checks/DEVICE_DECODE.md) |
| Signal integrity, ampacity, ripple | `--si`, `--ampacity` | [SI_CHECKS](../checks/SI_CHECKS.md) |
| Transients, brownout | `hauksbee-ci` scenarios | [TRANSIENTS](../checks/TRANSIENTS.md) |
| AC / small-signal | `--ac` | [AC_ANALYSIS](../analysis/AC_ANALYSIS.md) |
| Steady-state thermal | `--thermal` | [THERMAL](../checks/THERMAL.md) |

`--check` (alias `--all`) runs every static check at once. Reports exit 0 by
default; `--strict` gates. See [CI.md](../ci/CI.md#exit-codes) for the exit
contract.

## Layout

- `vendor/kicad-forge`: lossless KiCad parse/produce and board-to-code.
- `crates/`: one crate per stage, listed in the README's repository map.
- Binaries: `hauksbee`, `hauksbee-ci`, `hauksbee-mcp`.
