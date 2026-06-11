# Galvani

**Hand it a PCB. Watch it come alive.**

Galvani ingests real PCB design files, figures out the circuit they
implement, and simulates it — physically accurate analog, emulated
microcontrollers running real firmware, all rendered live on the actual
board layout. Talk to the MCU over a virtual serial port as if the board
were on your desk.

No other tool does this end to end. Schematic simulators (LTspice, ngspice,
Falstad) never see the board. MCU simulators (Wokwi, SimulIDE) use
behavioral circuits and breadboards. Proteus co-simulates firmware with
SPICE but only from its own schematic — never from a layout. Galvani starts
from the layout.

## What works with

- **KiCad** `.kicad_pcb`, versions 5 through 10 (including the 20260206
  name-only net format), plus schematic netlist exports for pin-level detail
- **Eagle** `.brd` (Arduino, Adafruit, SparkFun ecosystem)
- **IPC-D-356** fab netlists — the universal fallback: every EDA tool
  (Altium, Allegro, PADS...) exports one with fab files
- Anything KiCad can import (Altium, EasyEDA, CADSTAR...) by converting once

## Crates

| crate | what |
|---|---|
| `galvani-extract` | design files → components + nets + connectivity, with lint |
| `galvani-models` | component → simulation model: built-in library, user SPICE, datasheet extraction (codex-backed) |
| `galvani-ir` | circuit intermediate representation |
| `galvani-solve` | the solver: MNA + Newton where needed, closed-form where possible, every effect toggleable |
| `galvani-mcu` | MCU emulation (simavr-backed AVR; pin/ADC/UART/I2C/SPI coupling) |
| `galvani-server` | websocket server streaming live simulation frames |
| `frontend/` | the board, alive: 2D render, signal flow, probes, controls |

Sibling repo [`kicad-forge`](../kicad-forge): lossless KiCad file
parse/produce (byte-exact round-trip), typed board model, and
board-to-code decompilation with repeat detection.

## Speed

Galvani descends from the Tarski-Emulator, which simulated one specific
3,400-component neuromorphic board orders of magnitude faster than ngspice
by refusing to do unnecessary work: closed-form integration for RC
dynamics, event-driven digital, no giant matrix. Galvani generalizes that:
partition the circuit, give every island the cheapest solver that is
exactly right for it, and compile the hot path.

## Physical accuracy

Real device equations (diode, Gummel-Poon BJT, MOSFET), temperature
dependence, component tolerances, optional parasitics. Validated against
ngspice on reference circuits; faster because of architecture, not because
of corner-cutting. When you want corner-cutting for speed or debugging,
every effect is a switch you control.
