# Hauksbee vs the field

## The claim

No tool, open or commercial (verified June 2026), does what hauksbee does.
hauksbee ingests a real PCB layout file, extracts the circuit it implements,
and runs it: physically accurate analog co-simulated with real firmware on
emulated MCUs, rendered live on the actual board.

## Feature matrix

| | PCB file in | Analog accuracy | MCU firmware | Live board render | Open |
|---|---|---|---|---|---|
| **Hauksbee** | KiCad 5-10, Eagle, IPC-D-356, gerber/P&P | SPICE-class devices, validated vs ngspice | AVR, STM32, ESP32/-C3, nRF52840, RISC-V FE310 | yes, adaptive to any board | yes |
| Proteus VSM | no (own schematic, PCB is output-only) | mixed-mode SPICE | yes, 750+ MCUs | no (schematic anim.) | no, $$$ |
| Wokwi | no (breadboard JSON) | behavioral only | yes (avr8js etc., closed glue) | breadboard, not PCB | partially |
| SimulIDE | no | simplified nodal, "not very accurate" | simavr/gpsim | schematic anim. | GPL |
| KiCad + ngspice | schematic only, layout uninvolved | ngspice | no | no | yes |
| LTspice | no | excellent | no | no | freeware |
| Qucs-S | no | ngspice/Xyce backends | no | no | yes |
| Falstad CircuitJS | no | didactic | no | no | yes |
| Altium SI | layout (SI/PI only) | signal-integrity extraction | no | partial | no |

## Performance vs ngspice

Two different kinds of number live in the table below, and they are not equally
load-bearing:

- The **speed** figures (the `vs ngspice` column) are **benchmark observations**,
  not test-enforced guarantees. They come from `#[ignore]`d benches in
  `crates/hauksbee-solve/tests/perf.rs` that *print* a ratio. No test asserts a
  speed ratio, and the numbers vary with machine, ngspice build, and process
  start-up. Treat them as "what we measured on Apple Silicon against ngspice 46,"
  not as a contract.
- The **accuracy** figures (the `accuracy` column) are the asserted ones. The
  always-on suite (`tests/analytic.rs`, plus `tests/ngspice.rs` and
  `tests/kicad_vectors.rs` when an ngspice binary is present) gates every run on
  hard error bounds: half-wave rectifier deck <2% rel, CE amplifier deck <0.2%
  rel, RC ladder deck <1% rel, diode DC point <0.1%, BJT mirror ratio within 3%,
  and the KiCad-authored vectors at <1% (rectifier) and <2% (3x-2N2222
  amplifier). Those are the numbers we stand behind.

Same netlists, same tolerances, wall-clock. ngspice 46, Apple Silicon.

| circuit | hauksbee | vs ngspice (observed) | accuracy (asserted) |
|---|---|---|---|
| half-wave rectifier, 5ms tran | 2.05 ms | ~23x wall-clock (48.1 ms incl. process start) | <2% rel |
| synapse array, 90 blocks (partitioned) | 6.2-7.1x vs own monolithic | ~6x | 1.05e-7 vs monolithic |
| small RC island, exact exponential steps | 100x fewer steps at equal accuracy (~35x wall) | n/a (measured against its own monolithic reference) | 9.6e-10 vs analytic |
| RC ladder 1000 stages | 13.7k steps/s (Auto keeps monolithic: sparse LU already optimal there) | n/a (partitioned vs monolithic comparison) | <1e-6 vs monolithic |
| KiCad-authored vectors: rectifier / 3x-2N2222 amplifier | runs both via SpiceLoader | same netlists | 2.5e-5 / 0.92% max rel vs ngspice |

The accuracy column entries marked "vs analytic" / "vs monolithic" are asserted
inside the same benches in `perf.rs`: each bench passes only if the partitioned
result agrees with its monolithic or analytic reference. The always-on
cross-check tests assert the "vs ngspice" accuracy entries only when an
ngspice binary is present. The relevant tests early-return and skip otherwise.
They do not fail.

File-layer performance (forge-sexpr span CST): the 85MB Jetson AGX Thor
baseboard parses in about 230 ms and emits byte-exact in about 114 ms. A 44MB
production board extracts to a bound circuit in under a second.

"Any PCB" evidence (bind_sweep over the corpus): 19/19 boards extract and
bind with zero failures across KiCad 5-10 and Eagle formats. The Jetson AGX
Thor baseboard resolves 81.4% of simulatable parts, pic_programmer 92.3%.
The stormduino Uno clone goes further: hauksbee finds and instantiates its
ATmega328P, and it boots demo firmware through the solved circuit
(`docs/record/TEST_CAMPAIGN.md`).

Honest engineering note: the matrix-exponential fast path wins where circuits
fragment into many small islands (exactly the PCB regime: per-component RC
dynamics off shared rails) and where exact large steps replace thousands of
small ones. On one giant tridiagonal ladder the classic sparse LU is already
optimal and the partitioner correctly leaves it alone.

Architecture, not corner-cutting: partitioned islands (linear leads to exact
matrix exponential steps, nonlinear leads to small per-island MNA+Newton,
digital leads to events), symbolic factorization reuse, stamp-plan
compilation. hauksbee asserts the accuracy cross-checks against ngspice and
analytic solutions. The speed observations above came from runs that also
passed those accuracy bounds, so they are honest measurements rather than
corner-cutting. Still, the asserted guarantee is accuracy, not a speed ratio.

## Unique capabilities

- Faults come from the copper, not the intent: hauksbee simulates the circuit
  the layout actually implements, so a wiring defect that a behavioral model
  of the intended design cannot represent shows up as electrical stress in
  simulation. hauksbee derived one production miswire independently from the
  netlist this way. The board is private, so the reproducing tests are
  catalogued in `docs/about/PRIVATE_SUITE.md` rather than shipped.
- Datasheet → model extraction (codex-backed) when a part has no model.
- Board-to-code decompilation (kicad-forge): repeated blocks become
  functions. Layout anomalies become diffs. hauksbee found 19 block clusters
  covering 99.8% of a 3,443-component production board.
- Solver debugging dials: every physical effect is a toggle, granularity is
  continuous, and a user can force partitioning off for ground truth.
- Strict lossless parsing that caught real corruption in KiCad's own demo
  corpus (royalblue54L_feather, 349 unbalanced teardrop blocks), asserted as
  a must-fail.
- Gerber + pick-and-place reverse extraction (`docs/ingest/GERBER.md`): ingests boards
  that ship only manufacturing files (uConsole, Inkplate 6), reconstructing nets
  and copper from fab data. No other tool in this table ingests a PCB at all,
  let alone a CAD-less one. The gerber path widens "any board" to boards with no
  CAD. Validated closed-loop at 99.0-100% net-partition agreement over located
  pads.
- Multi-architecture firmware co-sim (`docs/cosim/MCU.md`): AVR, STM32, and ESP32 +
  ESP32-C3 proven end-to-end (firmware drives a net through the solved circuit).
  nRF52840 and SiFive FE310 RISC-V are proven to UART boot through the same
  lockstep trait, with the GPIO-current bridge not yet exercised on those two
  (see the per-architecture proof status in `docs/cosim/MCU.md`).
  Wokwi and Proteus emulate more part numbers, but from their own non-PCB
  inputs. hauksbee's breadth is across CPU architectures co-simulated against
  a circuit extracted from the real layout.
- Known-fault validation (`docs/record/KNOWN_FAULTS_VALIDATION.md`): eight in-scope
  faults documented in real boards' revision history, six caught statically, one
  executed via firmware co-sim, one honest static miss. Every catch is proven
  two-sided: it also stays silent on a clean counterpart, the fixed revision
  where the netlist reflects the fix, or a constructed clean twin where it does
  not. No other tool here ships a calibration of this kind. That calibration is
  what lets the clean famous-board sweep read as honesty rather than blindness.
