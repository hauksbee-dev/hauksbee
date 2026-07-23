# Hauksbee vs the field

> Status: structure + qualitative findings final; quantitative cells marked
> TBD are filled by the benchmark campaign in `testdata/` and `tests/`.

## The claim

No tool, open or commercial (verified June 2026), does what hauksbee does:
ingest a real PCB layout file, extract the circuit it implements, and run it
— physically accurate analog co-simulated with real firmware on emulated
MCUs, rendered live on the actual board.

## Feature matrix

| | PCB file in | Analog accuracy | MCU firmware | Live board render | Open |
|---|---|---|---|---|---|
| **Hauksbee** | KiCad 5-10, Eagle, IPC-D-356, gerber/P&P | SPICE-class devices, validated vs ngspice | AVR, STM32, ESP32/-C3, nRF52840, RISC-V FE310 | yes, adaptive to any board | yes |
| Proteus VSM | no (own schematic; PCB is output-only) | mixed-mode SPICE | yes, 750+ MCUs | no (schematic anim.) | no, $$$ |
| Wokwi | no (breadboard JSON) | behavioral only | yes (avr8js etc., closed glue) | breadboard, not PCB | partially |
| SimulIDE | no | simplified nodal, "not very accurate" | simavr/gpsim | schematic anim. | GPL |
| KiCad + ngspice | schematic only; layout uninvolved | ngspice | no | no | yes |
| LTspice | no | excellent | no | no | freeware |
| Qucs-S | no | ngspice/Xyce backends | no | no | yes |
| Falstad CircuitJS | no | didactic | no | no | yes |
| Altium SI | layout (SI/PI only) | signal-integrity extraction | no | partial | no |

## Performance vs ngspice

Two different kinds of number live in the table below, and they are not equally
load-bearing:

- The **speed** figures (the `vs ngspice` column) are **benchmark observations**,
  not test-enforced guarantees. They come from `#[ignore]`d benches in
  `crates/hauksbee-solve/tests/perf.rs` that *print* a ratio; no test asserts a
  speed ratio, and the numbers vary with machine, ngspice build, and process
  start-up. Treat them as "what we measured on Apple Silicon against ngspice 46",
  not as a contract.
- The **accuracy** figures (the `accuracy` column) are the asserted ones. The
  always-on suite (`tests/analytic.rs`, and `tests/ngspice.rs` when an ngspice
  binary is present) gates every run on hard error bounds: rectifier <1% rel, CE
  amplifier <2% rel, RC ladder <1% rel, diode DC point <0.1%, BJT mirror ratio
  within 3%. Those are the numbers we stand behind.

Same netlists, same tolerances, wall-clock. ngspice 46, Apple Silicon.

| circuit | hauksbee | vs ngspice (observed) | accuracy (asserted) |
|---|---|---|---|
| half-wave rectifier, 10ms tran | 2.05 ms | ~23x wall-clock (48.1 ms incl. process start) | <1% rel |
| synapse array, 90 Tarski-like blocks (partitioned) | 6.2-7.1x vs own monolithic | ~6x | 1.05e-7 vs monolithic |
| small RC island, exact exponential steps | 100x fewer steps at equal accuracy (~35x wall) | — | 9.6e-10 vs analytic |
| RC ladder 1000 stages | 13.7k steps/s (Auto keeps monolithic: sparse LU already optimal there) | — | partitioned vs monolithic 3.8e-4 |
| KiCad-authored vectors: rectifier / 3x-2N2222 amplifier | runs both via SpiceLoader | same netlists | 2.5e-5 / 0.92% max rel vs ngspice |

The accuracy column entries marked "vs analytic" / "vs monolithic" are checked by
the partitioned-vs-monolithic and analytic asserts in the always-on suite; the
"vs ngspice" accuracy entries are asserted only when an ngspice binary is present
(the relevant tests early-return and skip otherwise, they do not fail).

File-layer performance (forge-sexpr span CST): the 85MB Jetson AGX Thor
baseboard parses in ~230ms, emits byte-exact in ~114ms (2.1x end-to-end vs
the string CST); the 44MB Tarski board extracts to a bound circuit in
under a second.

"Any PCB" evidence (bind_sweep over the corpus): 19/19 boards extract and
bind with zero failures across KiCad 5-10 and Eagle formats — Jetson AGX
Thor baseboard 81.4% of simulatable parts resolved (788 analog devices),
vme-wren 77.8%, multichannel mixer 80.7%, pic_programmer 92.3%, and both
ATmega boards find and instantiate their MCU.

Honest engineering note: the matrix-exponential fast path wins where circuits
fragment into many small islands (exactly the PCB regime: per-component RC
dynamics off shared rails) and where exact large steps replace thousands of
small ones. On one giant tridiagonal ladder the classic sparse LU is already
optimal and the partitioner correctly leaves it alone.

Architecture, not corner-cutting: partitioned islands (linear → exact matrix
exponential steps; nonlinear → small per-island MNA+Newton; digital →
events), symbolic factorization reuse, stamp-plan compilation. The accuracy
cross-checks against ngspice and analytic solutions are what is asserted; the
speed observations above were taken on runs that also passed those accuracy
bounds, so they are honest measurements rather than corner-cutting, but the
asserted guarantee is accuracy, not a speed ratio.

## vs the bespoke Tarski-Emulator

The predecessor hand-modeled one board. Hauksbee must match its usefulness
without the hand-modeling:

| | Tarski-Emulator | Hauksbee |
|---|---|---|
| board support | 1 (hardcoded behavioral net) | any KiCad/Eagle/D356 |
| extraction | hand-written simplifier | automatic binder + model db |
| device physics | closed-form behavioral | SPICE-class, temperature, tolerances |
| MCU | simavr ATmega328P | same bridge, generalized API |
| hardware bug discovery | models the intended circuit (cannot see wiring defects) | independently derived the inhibitory miswire: 689mA through a 100mA junction when INH_Q4 enables (docs/record/BUG_HUNT.md Finding 15) |

## Unique capabilities

- Datasheet → model extraction (codex-backed) when a part has no model.
- Board-to-code decompilation (kicad-forge): repeated blocks become
  functions; layout anomalies become diffs. Found 19 block clusters covering
  99.8% of the Tarski board's 3,443 components in 0.09s.
- Solver debugging dials: every physical effect is a toggle; granularity is
  continuous; partitioning can be forced off for ground truth.
- Strict lossless parsing that caught real corruption in KiCad's own demo
  corpus (royalblue54L_feather, missing paren ×349).
- Gerber + pick-and-place reverse extraction (`docs/ingest/GERBER.md`): ingests boards
  that ship only manufacturing files (uConsole, Inkplate 6), reconstructing nets
  and copper from fab data. No other tool in this table ingests a PCB at all,
  let alone a CAD-less one; the gerber path widens "any board" to boards with no
  CAD. Validated closed-loop at 99.0-100% net-partition agreement over located
  pads.
- Multi-architecture firmware co-sim (`docs/cosim/MCU.md`): AVR, STM32, and ESP32 +
  ESP32-C3 proven end-to-end (firmware drives a net through the solved circuit);
  nRF52840 and SiFive FE310 RISC-V proven to UART boot through the same lockstep
  trait, with the GPIO-current bridge not yet exercised on those two (see the
  per-architecture proof status in `docs/cosim/MCU.md`).
  Wokwi and Proteus emulate more part numbers, but from their own non-PCB inputs;
  hauksbee's breadth is across CPU architectures co-simulated against a circuit
  extracted from the real layout.
- Known-fault validation (`docs/record/KNOWN_FAULTS_VALIDATION.md`): eight in-scope
  faults documented in real boards' revision history, six caught statically, one
  executed via firmware co-sim, each catch two-sided (flags the faulty revision,
  clean on the fix). No other tool here ships a calibration of this kind; it is
  what lets the clean famous-board sweep be read as honesty rather than blindness.
