# Galvani vs the field

> Status: structure + qualitative findings final; quantitative cells marked
> TBD are filled by the benchmark campaign in `testdata/` and `tests/`.

## The claim

No tool, open or commercial (verified June 2026), does what galvani does:
ingest a real PCB layout file, extract the circuit it implements, and run it
— physically accurate analog co-simulated with real firmware on emulated
MCUs, rendered live on the actual board.

## Feature matrix

| | PCB file in | Analog accuracy | MCU firmware | Live board render | Open |
|---|---|---|---|---|---|
| **Galvani** | KiCad 5-10, Eagle, IPC-D-356 | SPICE-class devices, validated vs ngspice | simavr (AVR), extensible | yes, adaptive to any board | yes |
| Proteus VSM | no (own schematic; PCB is output-only) | mixed-mode SPICE | yes, 750+ MCUs | no (schematic anim.) | no, $$$ |
| Wokwi | no (breadboard JSON) | behavioral only | yes (avr8js etc., closed glue) | breadboard, not PCB | partially |
| SimulIDE | no | simplified nodal, "not very accurate" | simavr/gpsim | schematic anim. | GPL |
| KiCad + ngspice | schematic only; layout uninvolved | ngspice | no | no | yes |
| LTspice | no | excellent | no | no | freeware |
| Qucs-S | no | ngspice/Xyce backends | no | no | yes |
| Falstad CircuitJS | no | didactic | no | no | yes |
| Altium SI | layout (SI/PI only) | signal-integrity extraction | no | partial | no |

## Performance vs ngspice

Same netlists, same tolerances, wall-clock. ngspice 46, Apple Silicon.

| circuit | galvani | vs ngspice | accuracy |
|---|---|---|---|
| half-wave rectifier, 10ms tran | 1.77 ms | ~38x wall-clock | <1% rel |
| synapse array, 90 Tarski-like blocks (partitioned) | 6.2-7.1x vs own monolithic | ~6x | 1.05e-7 vs monolithic |
| small RC island, exact exponential steps | 100x fewer steps at equal accuracy (~35x wall) | — | 9.6e-10 vs analytic |
| RC ladder 1000 stages | 11.6k steps/s (Auto keeps monolithic: sparse LU already optimal there) | TBD | bit-identical to monolithic |
| KiCad demo vectors (rectifier, sallen_key, amplifier-ac, laser_driver) | TBD | TBD | target ≤1% rel |

Honest engineering note: the matrix-exponential fast path wins where circuits
fragment into many small islands (exactly the PCB regime: per-component RC
dynamics off shared rails) and where exact large steps replace thousands of
small ones. On one giant tridiagonal ladder the classic sparse LU is already
optimal and the partitioner correctly leaves it alone.

Architecture, not corner-cutting: partitioned islands (linear → exact matrix
exponential steps; nonlinear → small per-island MNA+Newton; digital →
events), symbolic factorization reuse, stamp-plan compilation. Accuracy
cross-checks against ngspice gate every speed claim.

## vs the bespoke Tarski-Emulator

The predecessor hand-modeled one board. Galvani must match its usefulness
without the hand-modeling:

| | Tarski-Emulator | Galvani |
|---|---|---|
| board support | 1 (hardcoded behavioral net) | any KiCad/Eagle/D356 |
| extraction | hand-written simplifier | automatic binder + model db |
| device physics | closed-form behavioral | SPICE-class, temperature, tolerances |
| MCU | simavr ATmega328P | same bridge, generalized API |
| Tarski board inference run | baseline | TBD (target: same order of magnitude) |

## Unique capabilities

- Datasheet → model extraction (codex-backed) when a part has no model.
- Board-to-code decompilation (kicad-forge): repeated blocks become
  functions; layout anomalies become diffs. Found 19 block clusters covering
  99.8% of the Tarski board's 3,443 components in 0.09s.
- Solver debugging dials: every physical effect is a toggle; granularity is
  continuous; partitioning can be forced off for ground truth.
- Strict lossless parsing that caught real corruption in KiCad's own demo
  corpus (royalblue54L_feather, missing paren ×349).
