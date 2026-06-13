# Hauksbee

**CI for hardware. Hand it a PCB; it tells you what blows up before you order boards.**

Software runs its tests on every commit. Hardware never got that loop: you change the layout, send it to fab, wait three weeks, populate the board, and only then find the bug with a scope. Hauksbee closes the loop. It takes a real PCB design file, works out the circuit it actually implements, and runs the board headless on every commit, so the bugs that used to cost a fab spin and a fortnight at the bench fail a test instead.

No other tool does this from the layout. Schematic simulators never see the board, MCU simulators use breadboards, and Proteus VSM co-simulates only from its own schematic. Hauksbee starts from the copper. See [`docs/COMPARISON.md`](docs/COMPARISON.md) for the full matrix.

[**Watch the showcase**](frontend/capture/out/hauksbee_showcase.mp4) (a dozen boards running headless, ~2.5 min).

![A board, live in 2D with net activity](frontend/screenshots/beauty/2d-live.png)

---

## What it does

The fastest way in needs no terminal at all: run `hauksbee serve`, open the page it prints, drop a board on it, and read a plain-language report with a 2D map of the parts. Everything below is the same engine, from the command line.

Point it at any PCB design and it will:

- **Ingest** it: KiCad, Eagle, Altium `.PcbDoc` ([`docs/ALTIUM.md`](docs/ALTIUM.md)), IPC-D-356, and gerber-only boards that ship no CAD at all, reverse-extracted from copper geometry alone ([`docs/GERBER.md`](docs/GERBER.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)).
- **Simulate** the analogue circuit with real device physics, co-simulating the firmware in lockstep on an emulated MCU across AVR, STM32, ESP32/-C3, nRF52840 and SiFive RISC-V ([`docs/MCU.md`](docs/MCU.md)).
- **Check** it: copper shorts, USB-C CC compliance, boot strap-pins, MCU resource conflicts, signal integrity (now including a controlled-impedance estimate for USB and Ethernet from trace geometry and stackup, quasi-static closed-form, not a field solve), trace ampacity, behavioural power-IC models and transient brownouts, each tuned against a known-good corpus so it does not cry wolf ([`docs/SHORTS.md`](docs/SHORTS.md), [`docs/RESOURCE_CONFLICTS.md`](docs/RESOURCE_CONFLICTS.md), [`docs/SI_CHECKS.md`](docs/SI_CHECKS.md), [`docs/TRANSIENTS.md`](docs/TRANSIENTS.md)).
- **Analyse** it past the static checks: a small-signal AC sweep for Bode plots, phase margin and gain crossover (averaged about the DC operating point, not cycle-by-cycle switching) ([`docs/AC_ANALYSIS.md`](docs/AC_ANALYSIS.md)), and a steady-state thermal pass that turns each part's dissipation into a junction temperature and flags the ones that run too hot (per-device `Tj = Tambient + P * theta_JA`, not a board thermal field solve) ([`docs/THERMAL.md`](docs/THERMAL.md)).
- **Catch** the bug before you fab, in a headless pipeline with a GitHub Action, a KiCad plugin and a pre-commit hook, with assertions for rails, faults, temperature and loop stability ([`docs/CI.md`](docs/CI.md)), and runnable examples in [`docs/EXAMPLES.md`](docs/EXAMPLES.md).

![Fault state: a part exceeds its rating and the log explains why](frontend/screenshots/beauty/faults.png)

---

## The evidence

**Validated against documented faults.** Eight in-scope faults that were fixed between two public design revisions, across four famous boards (ZSWatch DevKit, Watchy, Olimex ESP32-EVB, MNT Reform), run on both the faulty and the fixed file. The revision history is the ground truth. Six were caught statically, one was executed two-sided via firmware co-sim, and one is an honest miss whose firmware-decidable path is named. No check was forced to inflate the count. Full matrix and the rejected-check evidence: [`docs/KNOWN_FAULTS_VALIDATION.md`](docs/KNOWN_FAULTS_VALIDATION.md).

**The most famous USB-C fault ever shipped, re-derived cold.** Fed a reconstruction of the Raspberry Pi 4's USB-C power-in (CC1 and CC2 tied to one shared 5.1 kΩ pulldown, the design that shipped on tens of millions of units), hauksbee's generic USB-C classifier derives the failure from spec thresholds alone: both CC pins land at 0.1338 V, below the 0.20 V vRa threshold, so a compliant source declares an Audio Adapter Accessory and withholds VBUS. Every solved voltage was recomputed by hand from the USB Type-C spec and matched to better than 0.01%. Full derivation: [`docs/BUG_HUNT.md`](docs/BUG_HUNT.md).

---

## Quickstart

```bash
scripts/install.sh                                   # build hauksbee + hauksbee-ci, put them on PATH
hauksbee serve                                       # web front door: open the page, drop a board, read the report
hauksbee run my_board.kicad_pcb --report             # extract, bind, and report on any board
hauksbee run my_board.kicad_pcb --drc --plain        # the copper-short report, in plain language
hauksbee run my_board.kicad_pcb --lint --strict      # exit non-zero on a real defect, to gate a pipeline
hauksbee-ci run ci/power-up.toml                      # run a CI spec the way a pipeline would
```

Reports exit 0 by default even when they find something; `--strict` (alias `--fail-on-findings`) makes them fail a build, and `--plain` (alias `--explain`) rewrites any finding as what it is, why it matters and what to do. Runnable specs, board-as-code examples and captured sessions are in [`docs/EXAMPLES.md`](docs/EXAMPLES.md); the test campaign is in [`docs/TEST_CAMPAIGN.md`](docs/TEST_CAMPAIGN.md).

---

## The honest verdict on the hunt

Pointed at two dozen famous open-hardware boards over five rounds, hauksbee found no unreported defect, and zero false positives from the lint. These are shipped, reviewed, working boards, so a clean electrical verdict is the correct one. The real yield was about ten bugs in hauksbee itself, surprises that turned out to be tool defects rather than board defects, each chased to ground and fixed. The clean sweep is evidence of the tool's honesty; the known-fault table is the proof of its teeth. Full write-up: [`docs/FAMOUS_SWEEP.md`](docs/FAMOUS_SWEEP.md).

## A note on speed

Hauksbee's matrix-exponential fast path wins in the PCB regime, many small RC islands hanging off shared rails, where exact large steps replace thousands of small ones. It is benchmarked at ~23x ngspice wall-clock on a half-wave rectifier and ~6x on a 90-block synapse array (`#[ignore]` benches, observations rather than guarantees). What *is* test-asserted is the accuracy: agreement with ngspice and analytic theory to fractions of a percent, and every speed claim is gated behind an accuracy cross-check. The method and the full table are in [`docs/COMPARISON.md`](docs/COMPARISON.md).

---

## Architecture

```
.kicad_pcb / .brd / .PcbDoc / .d356 / gerber ──▶ extract: pads ⇒ nets ⇒ connectivity ⇒ components
        │                                          ▲ model binding
        ▼                                          │ (built-in │ user SPICE │ datasheet via codex)
   Circuit IR ──▶ partitioned hybrid solver  ◀──▶  MCU backends (AVR/STM32/ESP32/nRF/RISC-V)
        │            linear → matrix exponential       pin/ADC/UART/I2C/SPI lockstep co-sim
        ▼            nonlinear → MNA + Newton
   server (websocket) ──▶ frontend: 2D/3D render, signal flow, probes, scope
                      └─▶ front door (`serve`): drop a board, get a plain report
```

Partition the circuit at device boundaries and give every island the cheapest solver that is exactly right for it: linear islands get matrix exponentials (exact at any step size), nonlinear islands get MNA + Newton, digital is event-driven. Full write-up in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

| crate | what |
|---|---|
| `hauksbee-extract` | design files → components + nets + connectivity, with lint |
| `hauksbee-models` | component → device model: built-in library, user SPICE, datasheet extraction |
| `hauksbee-ir` | circuit intermediate representation |
| `hauksbee-solve` | the solver: exact where possible, MNA+Newton where needed, every effect toggleable |
| `hauksbee-mcu` | MCU emulation and pin/ADC/UART/I2C/SPI coupling |
| `hauksbee-engine` | the whole pipeline wired together: bind → build → co-sim, plus examples and benchmarks |
| `hauksbee-ci` | the headless CI runner |
| `hauksbee-server` | websocket server streaming live simulation frames |
| `frontend/` | the board, alive: 2D/3D render, signal flow, probes, scope |

Sibling repo [`kicad-forge`](../kicad-forge): lossless KiCad parse/produce (byte-exact round-trip), the typed board model hauksbee extracts from, and board-to-code decompilation.

---

## Where it came from

Hauksbee was built for one board no simulation tool could honestly check: Tarski, a 3,443-component analogue neuromorphic accelerator (Project Tarski, University of Galway). That board got a bespoke emulator, fast because it integrated the *intended* circuit in closed form, which was exactly its blind spot: it could never see a base wired where a collector should be. Hauksbee simulates the board you actually drew, device by device, and finds the bugs the bespoke one was structurally incapable of finding.

The name is for [Francis Hauksbee](https://en.wikipedia.org/wiki/Francis_Hauksbee), who built the first machine to make the electrostatic spark on demand. Bringing a dead board to life is roughly the same trick.

See the website at [hauksbee.dev](https://hauksbee.dev). Honest limitations are catalogued in [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md). MIT licensed.
