# Hauksbee

**CI for hardware. Hand it a PCB; it tells you what blows up before you order boards.**

Software runs its tests on every commit. Hardware never got that loop: you change the layout, send it to fab, wait three weeks, populate the board, and only then find the bug with a scope. Hauksbee closes the loop. It takes a real PCB design file, works out the circuit it actually implements, and runs the board headless on every commit, so the bugs that used to cost a fab spin and a fortnight at the bench fail a test instead.

No other tool does this from the layout. Schematic simulators never see the board, MCU simulators use breadboards, and Proteus VSM co-simulates only from its own schematic. Hauksbee starts from the copper. See [`docs/COMPARISON.md`](docs/COMPARISON.md) for the full matrix.

**New here? Start with [`docs/START_HERE.md`](docs/START_HERE.md):** the user path, install, and your next four reads. The authoritative scope document is [`docs/CAPABILITIES.md`](docs/CAPABILITIES.md): what every layer does, which MCU architectures the firmware co-sim covers (AVR via libsimavr; STM32/nRF52/RISC-V via Renode; ESP32 family via Espressif QEMU), and a common-misconceptions section. The project's evidence trail (bug-hunt campaigns, known-fault calibration, benchmarks) lives in [`docs/record/`](docs/record/).

[**Watch the showcase**](frontend/capture/out/hauksbee_showcase.mp4) (a dozen boards running headless, ~2.5 min).

![A board, live in 2D with net activity](frontend/screenshots/beauty/2d-live.png)

---

## What it does

The fastest way in needs no terminal at all: run `hauksbee serve`, open the page it prints, drop a board on it, and read a plain-language report with a 2D map of the parts. Everything below is the same engine, from the command line.

Point it at any PCB design and it will:

- **Ingest** it: KiCad, Eagle, Altium `.PcbDoc` ([`docs/ALTIUM.md`](docs/ALTIUM.md)), IPC-D-356, and gerber-only boards that ship no CAD at all, reverse-extracted from copper geometry alone ([`docs/GERBER.md`](docs/GERBER.md), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)).
- **Simulate** the analogue circuit with real device physics, co-simulating the firmware in lockstep on an emulated MCU across AVR, STM32, ESP32/-C3, nRF52840 and SiFive RISC-V. GPIO and UART co-sim run on every backend; ADC injection and I2C/SPI peripheral-slave models are on the AVR backend today, with the per-backend coverage matrix and the reason stated plainly in [`docs/MCU.md`](docs/MCU.md).
- **Check** it: copper shorts, USB-C CC compliance, boot strap-pins, MCU resource conflicts, signal integrity (now including a controlled-impedance estimate for USB and Ethernet from trace geometry and stackup, quasi-static closed-form, not a field solve), trace ampacity, behavioural power-IC models and transient brownouts, each tuned against a known-good corpus so it does not cry wolf ([`docs/SHORTS.md`](docs/SHORTS.md), [`docs/RESOURCE_CONFLICTS.md`](docs/RESOURCE_CONFLICTS.md), [`docs/SI_CHECKS.md`](docs/SI_CHECKS.md), [`docs/TRANSIENTS.md`](docs/TRANSIENTS.md)).
- **Analyse** it past the static checks: a small-signal AC sweep for Bode plots, phase margin and gain crossover (averaged about the DC operating point, not cycle-by-cycle switching) ([`docs/AC_ANALYSIS.md`](docs/AC_ANALYSIS.md)), and a steady-state thermal pass that turns each part's dissipation into a junction temperature and flags the ones that run too hot (per-device `Tj = Tambient + P * theta_JA`, not a board thermal field solve) ([`docs/THERMAL.md`](docs/THERMAL.md)).
- **Catch** the bug before you fab, in a headless pipeline with a GitHub Action, a KiCad plugin and a pre-commit hook, with assertions for rails, faults, temperature and loop stability ([`docs/CI.md`](docs/CI.md)), and runnable examples in [`docs/EXAMPLES.md`](docs/EXAMPLES.md).

![Fault state: a part exceeds its rating and the log explains why](frontend/screenshots/beauty/faults.png)

---

## The evidence

**Validated against documented faults.** Eight in-scope faults that were fixed between two public design revisions, across four famous boards (ZSWatch DevKit, Watchy, Olimex ESP32-EVB, MNT Reform), run on both the faulty and the fixed file. The revision history is the ground truth. Six were caught statically, one was executed two-sided via firmware co-sim, and one is an honest miss whose firmware-decidable path is named. No check was forced to inflate the count. Full matrix and the rejected-check evidence: [`docs/record/KNOWN_FAULTS_VALIDATION.md`](docs/record/KNOWN_FAULTS_VALIDATION.md).

**The most famous USB-C fault ever shipped, re-derived cold.** Fed a reconstruction of the Raspberry Pi 4's USB-C power-in (CC1 and CC2 tied to one shared 5.1 kΩ pulldown, the design that shipped on tens of millions of units), hauksbee's generic USB-C classifier derives the failure from spec thresholds alone: both CC pins land at 0.1338 V, below the 0.20 V vRa threshold, so a compliant source declares an Audio Adapter Accessory and withholds VBUS. Every solved voltage was recomputed by hand from the USB Type-C spec and matched to better than 0.01%. Full derivation: [`docs/record/BUG_HUNT.md`](docs/record/BUG_HUNT.md).

---

## Quickstart

**Build from source:**

```bash
scripts/install.sh                                   # build hauksbee + hauksbee-ci, put them on PATH
```

**Or run it in Docker** (no local toolchain needed; the slim image carries `hauksbee` + `hauksbee-ci`, the model db and AVR co-sim). The published image lands with the first public release; during private beta, build from source above:

```bash
docker run --rm -v "$PWD:/work" ghcr.io/etm-code/hauksbee:slim \
  hauksbee run path/to/board.kicad_pcb --report
```

The slim and full images and more `docker run` examples are in [`docs/DOCKER.md`](docs/DOCKER.md).

**Then use it — first run, against a board that ships in this repo:**

```bash
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --report --plain   # which parts were modelled, plain bottom line
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --drc --plain      # the copper-short report, in plain language
hauksbee-ci run crates/hauksbee-ci/examples/blinky.toml                             # run a CI spec the way a pipeline would
```

**Then swap in your own board** (`my_board.kicad_pcb` is a placeholder for your file):

```bash
hauksbee run my_board.kicad_pcb                       # no flags on a terminal: a full-screen dashboard (TUI)
hauksbee run my_board.kicad_pcb --si --plain         # signal integrity (USB/Ethernet impedance, rise times)
hauksbee run my_board.kicad_pcb --list-nets          # list net names (for --ac-node / --ac-loop)
hauksbee run my_board.kicad_pcb --lint --strict      # exit non-zero on a real defect, to gate a pipeline
hauksbee-ci init my_board.kicad_pcb                  # scaffold a CI spec beside the board (prints the path; edit, then run)
hauksbee serve                                       # web front door (long-running): open the page, drop a board, read the report
```

Reports exit 0 by default even when they find something; `--strict` (alias `--fail-on-findings`) makes them fail a build, and `--plain` (alias `--explain`) rewrites any finding as what it is, why it matters and what to do. Runnable specs, board-as-code examples and captured sessions are in [`docs/EXAMPLES.md`](docs/EXAMPLES.md); the test campaign is in [`docs/record/TEST_CAMPAIGN.md`](docs/record/TEST_CAMPAIGN.md).

**Prebuilt binary (once a public release exists):**

> The prebuilt install needs a published, publicly-downloadable GitHub release. While hauksbee is in private beta there is none yet, so build from source (above) for now.

```bash
curl -fsSL https://raw.githubusercontent.com/ETM-Code/hauksbee/main/scripts/get-hauksbee.sh | bash
```

This fetches the latest release for your OS/arch, verifies the sha256 checksum, and installs `hauksbee` + `hauksbee-ci` to `~/.local/bin`. If that directory is not on your `PATH`, the installer prints the exact line to add. macOS users: if Gatekeeper blocks the binary on first run, remove the quarantine flag with `xattr -d com.apple.quarantine ~/.local/bin/hauksbee ~/.local/bin/hauksbee-ci`. `scripts/test-install-mock.sh` exercises the whole download/verify/install flow against a local mock so the installer is proven before any release goes out.

---

## Firmware co-sim and CI specs

Firmware co-sim boots your compiled image on an emulated MCU and runs it in
lockstep with the analog solve of the as-built board — so a GPIO the firmware
drives actually moves the copper it's wired to, and you assert on the result.
There are two ways in.

**One-off, from the command line** — point `run` at a board and a firmware image:

```bash
hauksbee run my_board.kicad_pcb --firmware build/app.elf            # TUI dashboard, live
hauksbee run my_board.kicad_pcb --firmware build/app.hex --report --plain   # one-shot verdict
```

The MCU is detected from the board (an ESP32-S3 boots Espressif QEMU, an
ATmega328P libsimavr, an STM32/nRF52 Renode); `.elf` and `.hex` are both
accepted. If a needed emulator isn't installed, hauksbee tells you the one
command to get it (`hauksbee install esp-qemu`, or `scripts/install-sims.sh`).

**As a repeatable check** — a `.toml` spec captures the board, the firmware, how
the board is powered, and the assertions that must hold, so a pipeline can gate
on it. Scaffold one from a board with `hauksbee-ci init my_board.kicad_pcb`, then
run it:

```bash
hauksbee-ci run ci/boot.toml                    # exit 0 = all assertions held
hauksbee-ci run ci/boot.toml --junit out.xml    # emit JUnit XML for CI
```

A spec reads as board-as-code. This one boots real firmware and asserts that a
MOSFET gate is actively driven within 20 ms of reset (the full example is
[`crates/hauksbee-ci/examples/boot_gate_pass.toml`](crates/hauksbee-ci/examples/boot_gate_pass.toml)):

```toml
name = "boot-coverage: MOSFET gate driven promptly"
board = "boards/boot_gate.kicad_pcb"      # .kicad_pcb / .kicad_sch / .brd / .d356
firmware = "firmware/boot_gate.hex"       # optional; ELF or hex, relative to the spec
mcu = "atmega328p"                        # usually auto-detected from the board
duration_ms = 50

[[supply]]                                # how the board is powered
net = "+5V"
kind = "ideal"
volts = 5.0

[[assert]]                               # 1+ assertions; all must hold
kind = "boot-coverage"                    # the gate net must reach a logic high...
net = "GATE_CTRL"
min = 3.0
deadline_ms = 20.0                        # ...within 20 ms of reset

[[assert]]
kind = "no_faults"                        # and nothing is over-stressed meanwhile
```

Assertions cover rails and forced net drives, timed peripheral events (button
presses, sensor inputs), `voltage` / `toggle` / `boot-coverage` / `no_faults`,
thermal limits, and tolerance/AC sweeps. More runnable specs and board-as-code
examples are in [`docs/EXAMPLES.md`](docs/EXAMPLES.md); the full spec schema is
documented at the top of [`crates/hauksbee-ci/src/spec.rs`](crates/hauksbee-ci/src/spec.rs).

---

## Simulators / firmware co-sim

AVR (ATmega328P) co-simulation links libsimavr directly into the engine, so
there is no separate simulator process to launch. simavr is GPL-3.0 and this
MIT repo does not vendor it, so a source build links it from the system — one
command installs it:

```bash
scripts/install-sims.sh --avr    # build + install libsimavr (AVR co-sim)
```

Prefer to skip AVR? Build the GPL-free subset instead:

```bash
cargo build -p hauksbee-engine --no-default-features --features renode,qemu
```

For STM32, nRF52, SiFive RISC-V, ESP32, and ESP32-C3 you need the external
simulator backends:

```bash
scripts/install-sims.sh          # install Renode + Espressif QEMU
scripts/install-sims.sh --check  # verify hauksbee will find them
```

Renode covers STM32 / nRF52 / RISC-V; the Espressif QEMU fork covers the full
ESP32 family. Both are detected from an external install (GPL/size; not bundled)
using the same detect-don't-bundle pattern as the KiCad and ngspice oracles.
Full details — discovery order, env-var overrides, manual install steps, and the
Gatekeeper note for macOS — are in [`docs/SIMULATORS.md`](docs/SIMULATORS.md).

---

## The honest verdict on the hunt

Pointed at two dozen famous open-hardware boards over five rounds, hauksbee found no unreported defect, and zero false positives from the lint. These are shipped, reviewed, working boards, so a clean electrical verdict is the correct one. The real yield was about ten bugs in hauksbee itself, surprises that turned out to be tool defects rather than board defects, each chased to ground and fixed. The clean sweep is evidence of the tool's honesty; the known-fault table is the proof of its teeth. Full write-up: [`docs/record/FAMOUS_SWEEP.md`](docs/record/FAMOUS_SWEEP.md).

Then we pointed it where unknown bugs actually survive: **thin-review, single-maintainer, freshly-shipped boards with no issue history** — the opposite of the famous survivors. Across five such boards, hauksbee turned up **three genuine, previously-unreported findings**, each chased to the design file, hand-verified, prior-art-checked, and put past a fresh context-isolated skeptic:

- **FPV-Drone-STM32F411 ESC board** — a **+3.3 V-to-GND copper short** on the bottom copper (GND pour overlaps the 3.3 V trace, actual clearance 0.0000 mm), confirmed independently by KiCad's own DRC and present byte-for-byte in the exported gerber. Built as drawn, the ESC is dead on arrival. (A DRC-class defect: KiCad's built-in DRC catches it too — the board was simply never DRC'd before shipping.)
- **LibreSolar mppt-1210-hus** — the input bulk electrolytic runs at **~1.66x its ripple-current rating** (~5.0 A rms vs 3.0 A) at the board's rated 10 A charge: a lifetime/derating overstress, not a power-up failure. Hauksbee-assisted hand finding (it extracted the topology and part values; the ripple physics is the analyst's).
- **INGBZGMBH PD-Sink-Trigger-Board** — the rotary voltage selector **mis-codes its top two detents** against the CYPD3177 EZ-PD BCR decode table ("15 V" requests 12 V; "20 V" never reaches 20 V), so the advertised 20 V / 100 W is unreachable. Functional defect, no safety hazard.

Three real findings, zero false positives shipped, on boards nobody had reviewed. The targeting thesis (bugs survive on unproven boards, not on shipped survivors) held. Per-board write-ups and the tooling-gap backlog the hunt exposed are in [`docs/hunts/`](docs/hunts/) ([`SUMMARY.md`](docs/hunts/SUMMARY.md)).

A later **firmware co-sim** hunt ([`docs/hunts/HUNT_2026-06-30.md`](docs/hunts/HUNT_2026-06-30.md)) added a fourth finding of a different character — a **cross-layer power-up ignition fault** that only the co-sim differentiator can see. On `explosion33/RocketryIgniter` (ATmega328P dual-igniter e-match board), the firmware's `SoftwareSerial` constructor enables the AVR internal pull-up on its RX pin during C++ static init — *before `setup()` runs* — and that RX pin is wired straight to one of the two MOSFET igniter gates, which has no pull-down. The gate charges to ~VCC through the ~30 kΩ pull-up, the FET turns on, and an e-match fires at power-up; the firmware's intended fire path is an unimplemented `//To Do`. hauksbee co-sim settles the gate at 5.000 V and `hauksbee run --firmware --headless --plain` names it — copper (no pull-down) → silicon (AVR pull-up) → firmware (a serial port mapped onto a pyrotechnic gate) → consequence (ignition), confirmed across seven axes by a fresh hardware-skeptic. Catching it first required fixing a silent co-sim solver bug (a crystal mis-bound as a 16-gigafarad capacitor) and surfacing it to a layperson required a new boot-safety advisory — both in the same hunt log.

## A note on speed

Hauksbee's matrix-exponential fast path wins in the PCB regime, many small RC islands hanging off shared rails, where exact large steps replace thousands of small ones. It is benchmarked at ~23x ngspice wall-clock on a half-wave rectifier and ~6x on a 90-block synapse array (`#[ignore]` benches, observations rather than guarantees). What *is* test-asserted is the accuracy: agreement with ngspice and analytic theory to fractions of a percent, and every speed claim is gated behind an accuracy cross-check. The method and the full table are in [`docs/COMPARISON.md`](docs/COMPARISON.md).

---

## Architecture

```
.kicad_pcb / .brd / .PcbDoc / .d356 / gerber ──▶ extract: pads ⇒ nets ⇒ connectivity ⇒ components
        │                                          ▲ model binding
        ▼                                          │ (built-in │ user SPICE │ datasheet via codex)
   Circuit IR ──▶ partitioned hybrid solver  ◀──▶  MCU backends (AVR/STM32/ESP32/nRF/RISC-V)
        │            linear → matrix exponential       GPIO+UART lockstep on all; ADC/I2C/SPI on AVR
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

Upstream repo [`kicad-forge`](https://github.com/ETM-Code/kicad-forge): lossless KiCad parse/produce (byte-exact round-trip), the typed board model hauksbee extracts from, and board-to-code decompilation. Its `forge-sexpr`, `forge-model`, and `forge-codegen` crates are vendored into this repo under [`vendor/kicad-forge`](vendor/kicad-forge) so a fresh clone builds with no sibling checkout; see [`vendor/kicad-forge/VENDORED.md`](vendor/kicad-forge/VENDORED.md).

---

## Where it came from

Hauksbee was built for one board no simulation tool could honestly check: Tarski, a 3,443-component analogue neuromorphic accelerator (Project Tarski, University of Galway). That board got a bespoke emulator, fast because it integrated the *intended* circuit in closed form, which was exactly its blind spot: it could never see a base wired where a collector should be. Hauksbee simulates the board you actually drew, device by device, and finds the bugs the bespoke one was structurally incapable of finding.

The name is for [Francis Hauksbee](https://en.wikipedia.org/wiki/Francis_Hauksbee), who built the first machine to make the electrostatic spark on demand. Bringing a dead board to life is roughly the same trick.

See the website at [hauksbee.dev](https://hauksbee.dev). Honest limitations are catalogued in [`docs/LIMITATIONS.md`](docs/LIMITATIONS.md). Exactly which SPICE cards `hauksbee sim` accepts or refuses — enforced against the loader so it cannot drift — is the [SPICE compatibility statement](docs/spice-compat/compatibility.md).

---

## Acknowledgements

Hauksbee stands on a lot of open-source work. The substantial ones:

**MCU co-simulation backends**
- [simavr](https://github.com/buserror/simavr) (GPL-3.0) — cycle-accurate AVR / ATmega328P emulation, linked in-process via FFI, behind the `avr` backend.
- [Renode](https://renode.io) by Antmicro (MIT) — STM32, nRF52 and SiFive RISC-V emulation, driven headless over its Monitor protocol.
- [QEMU](https://www.qemu.org) and [Espressif's QEMU fork](https://github.com/espressif/qemu) (GPL-2.0) — ESP32 / ESP32-S3 / ESP32-C3 emulation with the SoC peripherals modelled.

**PCB tooling**
- [KiCad](https://www.kicad.org) (GPL-3.0) — the Altium `.PcbDoc` binary record parsers are ported field-by-field from KiCad's open-source Altium importer ([`docs/ALTIUM.md`](docs/ALTIUM.md)); `kicad-cli` is used for DRC cross-checks.
- [freerouting](https://github.com/freerouting/freerouting) — the open-source autorouter hauksbee hands recompiled board-as-code off to for production routing (separate process, invoked headless).
- [ngspice](https://ngspice.sourceforge.io) — the reference SPICE engine the solver is cross-validated against for accuracy and benchmarked beside.

**Rust ecosystem** — [num-complex](https://crates.io/crates/num-complex) (complex MNA for the AC solver), [cfb](https://crates.io/crates/cfb) (OLE2 parsing for Altium files), [evalexpr](https://crates.io/crates/evalexpr) (behavioural device-model expressions), [clap](https://crates.io/crates/clap) (CLI), [axum](https://crates.io/crates/axum) + [tower-http](https://crates.io/crates/tower-http) + [tokio](https://crates.io/crates/tokio) (the web front door), [serde](https://crates.io/crates/serde) (serialisation), [bindgen](https://crates.io/crates/bindgen) (the simavr FFI bindings), and the wider Rust crate ecosystem.

## License

Hauksbee's own source is **MIT** (see [`LICENSE`](LICENSE)). One caveat, stated plainly: the optional `avr` backend links **libsimavr, which is GPL-3.0**, so a binary built with the default features (which include `avr`) is a combined work covered by GPL-3.0. Build with `--no-default-features --features renode,qemu` for a GPL-free binary — Renode and the Espressif QEMU fork run as separate processes reached over TCP, so they impose no link-time licence obligation.
