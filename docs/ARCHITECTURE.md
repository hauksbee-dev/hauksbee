# Galvani Architecture

Galvani takes a real PCB design (KiCad first), extracts the circuit it
implements, and brings it to life: a fast, physically accurate analog
simulation co-simulated with emulated microcontrollers, rendered live on the
actual board layout. No existing tool (open or commercial, as of mid-2026)
does this end-to-end; Proteus VSM is closest but simulates from its own
schematic, never from a layout.

## Lineage

Galvani generalizes the Tarski-Emulator, which proved the approach on one
board: closed-form integration where topology permits, event-driven digital,
firmware on an emulated MCU coupled through pin-level hooks. Tarski's
hand-built network is replaced by automatic extraction and a general solver,
without giving up the speed that made it many times faster than ngspice.

## Pipeline

```
.kicad_pcb ──forge-sexpr/model──▶ typed board
.kicad_sch ──forge-sexpr────────▶ derived netlist   (see docs/SCHEMATICS.md)
.brd / .d356 ───────────────────▶ typed board
gerber + drill + P&P ───────────▶ reconstructed board (see docs/GERBER.md)
        │
        ▼
galvani-extract: pads ⇒ nets ⇒ connectivity graph ⇒ component instances
        │                                   ▲
        ▼                                   │ model binding
galvani-models: lib_id/value/part-number ⇒ device model
   (built-in defaults │ user SPICE │ datasheet extraction via codex)
        │
        ▼
galvani-ir: Circuit IR — devices, nodes, parameters, parasitics (optional)
        │
        ▼
galvani-solve: partitioned hybrid solver           galvani-mcu: MCU backends
   linear subcircuits → state-space exp.    ◀───▶    AVR/STM32/ESP32/nRF/RISC-V
   nonlinear islands  → MNA + Newton                 pin/ADC/UART/I2C hooks
   digital            → event queue                  lockstep co-sim (docs/MCU.md)
        │
        ▼
galvani-server: websocket protocol (frames, probes, controls)
        │
        ▼
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

## Repos

- `kicad-forge` (sibling): lossless KiCad parse/produce, typed model,
  board-to-code with repeat detection.
- `galvani` (this repo): extraction, models, solver, MCU co-sim, server, UI.
