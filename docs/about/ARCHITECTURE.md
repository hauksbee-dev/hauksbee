# Hauksbee Architecture

Hauksbee takes a real PCB design (KiCad first), extracts the circuit it
implements, and brings it to life: a fast, physically accurate analog
simulation co-simulated with emulated microcontrollers, rendered live on the
actual board layout. No existing tool (open or commercial, as of mid-2026)
does this end-to-end; Proteus VSM is closest but simulates from its own
schematic, never from a layout.

## Lineage

Hauksbee generalizes the Tarski-Emulator, which proved the approach on one
board: closed-form integration where topology permits, event-driven digital,
firmware on an emulated MCU coupled through pin-level hooks. Tarski's
hand-built network is replaced by automatic extraction and a general solver,
without giving up the speed that made it many times faster than ngspice.

## Pipeline

```
.kicad_pcb ──forge-sexpr/model──▶ typed board
.kicad_sch ──forge-sexpr────────▶ derived netlist   (see docs/ingest/SCHEMATICS.md)
.brd / .d356 ───────────────────▶ typed board
gerber + drill + P&P ───────────▶ reconstructed board (see docs/ingest/GERBER.md)
        │
        ▼
hauksbee-extract: pads ⇒ nets ⇒ connectivity graph ⇒ component instances
        │                                   ▲
        ▼                                   │ model binding
hauksbee-models: lib_id/value/part-number ⇒ device model
   (built-in defaults │ user SPICE │ datasheet extraction via codex)
        │
        ▼
hauksbee-ir: Circuit IR — devices, nodes, parameters, parasitics (optional)
        │
        ▼
hauksbee-solve: partitioned hybrid solver           hauksbee-mcu: MCU backends
   linear subcircuits → state-space exp.    ◀───▶    AVR/STM32/ESP32/nRF/RISC-V
   nonlinear islands  → MNA + Newton                 pin/ADC/UART/I2C hooks
   digital            → event queue                  lockstep co-sim (docs/cosim/MCU.md)
        │
        ▼
hauksbee-server: websocket protocol (frames, probes, controls)
        │                         └─ front door: drop a board, get a report
        ▼                            (`hauksbee serve`, browser, no CLI)
frontend: board-accurate 2D/3D render, live signal flow, probes, controls
```

## Solver philosophy (why we beat ngspice)

1. **Partition before solving.** Connectivity graph is split at device
   boundaries into islands. Purely linear islands get state-space form
   `x' = Ax + Bu` solved by matrix exponential (exact at any step size, the
   Tarski trick generalized). Digital components are event-driven. Only
   genuinely nonlinear analog islands pay for MNA + Newton, and each island
   solves its own small matrix instead of one giant one.
2. **Compile, don't interpret.** The IR can be lowered to specialized Rust
   (or a precompiled stamp plan) so the per-step inner loop is flat code
   with fixed sparsity — no per-step model dispatch.
3. **Solver debugging controls.** Every effect (parasitics, temperature
   dependence, charge storage, tolerances) is toggleable; granularity is
   adjustable. Turning physics off is a feature for debugging and speed.

## Accuracy

Unlike Tarski-Emulator's bespoke behavioral models, device models are real:
SPICE-level diode/BJT/MOSFET equations, temperature-dependent, validated
against ngspice on reference circuits to tight tolerances. Behavioral models
remain available for digital ICs and for speed toggles.

## Checks and analyses

The static checks and the dynamic analyses each have their own write-up:

- **DRC / copper shorts**: [SHORTS.md](../checks/SHORTS.md).
- **Connectivity lint, strap-pins, resource conflicts**: [RESOURCE_CONFLICTS.md](../checks/RESOURCE_CONFLICTS.md).
- **Signal integrity** (crystal load caps, decoupling, antenna keepout, USB skew, and controlled-impedance from trace geometry + stackup, a quasi-static closed-form estimate, not a field solve): [SI_CHECKS.md](../checks/SI_CHECKS.md).
- **Transients and brownouts**: [TRANSIENTS.md](../checks/TRANSIENTS.md).
- **AC / small-signal** (Bode, phase margin, gain crossover; averaged small-signal about the DC operating point, not cycle-by-cycle switching): [AC_ANALYSIS.md](../analysis/AC_ANALYSIS.md).
- **Steady-state thermal** (per-device junction temperature `Tj = Tambient + P * theta_JA`, not a board thermal field solve): [THERMAL.md](../checks/THERMAL.md).

Each report runs from `hauksbee run <board> --drc/--lint/--si/--resources/--thermal` or `--ac`. They are informational and exit 0 by default; add `--plain` (alias `--explain`) for a non-engineer verdict, or `--strict` (alias `--fail-on-findings`) to fail a pipeline directly. For the full assertion flow, including `phase_margin` / `ac_gain` / `max_temp`, see [CI.md](../ci/CI.md). Runnable examples, including the `hauksbee serve` web front door, are in [EXAMPLES.md](../ci/EXAMPLES.md).

## Repos

- `kicad-forge` (sibling): lossless KiCad parse/produce, typed model,
  board-to-code with repeat detection.
- `hauksbee` (this repo): extraction, models, solver, MCU co-sim, server, UI.
