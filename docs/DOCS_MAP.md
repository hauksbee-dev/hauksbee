# Docs map

The entry point is [`START_HERE.md`](START_HERE.md); this file maps every doc
to the question it answers. There is no generated index (no mdbook
`SUMMARY.md`, no `mkdocs.yml`); the repo-root `README.md` leads with
START_HERE.

The user docs are grouped by the question the reader is asking. Only the entry
points live at the `docs/` root: START_HERE, this map, and
[`PROJECT_LAYOUT.md`](PROJECT_LAYOUT.md) (how to prepare your own board +
firmware + spec, and the one local command that runs them).

## The user path, by question

**"How do I get my board file in?"**: [`ingest/`](ingest/)

| File | Covers |
|---|---|
| [`ingest/ALTIUM.md`](ingest/ALTIUM.md) | Altium `.PcbDoc` binary ingest |
| [`ingest/GERBER.md`](ingest/GERBER.md) | Gerber + pick-and-place reverse extraction |
| [`ingest/SCHEMATICS.md`](ingest/SCHEMATICS.md) | Schematic (`.kicad_sch`) extraction |
| [`ingest/DNP.md`](ingest/DNP.md) | Do-not-populate parts: what gets simulated, and how to change it |
| [`ingest/BOARD_AS_CODE.md`](ingest/BOARD_AS_CODE.md) | Decompile / edit / recompile a board as editable code |

**"What does it check, and can I trust the findings?"**: [`checks/`](checks/)

| File | Covers |
|---|---|
| [`checks/SHORTS.md`](checks/SHORTS.md) | Copper short / clearance detection and simulation |
| [`checks/SI_CHECKS.md`](checks/SI_CHECKS.md) | Signal-integrity static checks (`--si`) |
| [`checks/RESOURCE_CONFLICTS.md`](checks/RESOURCE_CONFLICTS.md) | MCU internal resource-conflict check |
| [`checks/DEVICE_DECODE.md`](checks/DEVICE_DECODE.md) | Config-pin divider decode check |
| [`checks/THERMAL.md`](checks/THERMAL.md) | Steady-state junction-temperature check |
| [`checks/TRANSIENTS.md`](checks/TRANSIENTS.md) | Transient scenarios: load steps, brownout, inrush |

**"What analysis can it run past the static checks?"**: [`analysis/`](analysis/)

| File | Covers |
|---|---|
| [`analysis/AC_ANALYSIS.md`](analysis/AC_ANALYSIS.md) | AC / small-signal sweep: Bode, phase margin, gain crossover |
| [`analysis/JSON_OUTPUT.md`](analysis/JSON_OUTPUT.md) | The `--json` output schema: top-level verdict + every section |

**"How does firmware co-sim work, and which chips?"**: [`cosim/`](cosim/)

| File | Covers |
|---|---|
| [`cosim/MCU.md`](cosim/MCU.md) | MCU co-sim backends, supported chips, firmware input tiers |
| [`cosim/PERIPHERALS.md`](cosim/PERIPHERALS.md) | Runtime peripherals (buttons, pots, I2C/SPI slaves) |
| [`cosim/SIMULATORS.md`](cosim/SIMULATORS.md) | Installing the external backends (Renode, Espressif QEMU) |
| [`cosim/ORACLES.md`](cosim/ORACLES.md) | Ground-truth oracles (`--oracle`): kicad-cli, ngspice |

**"How do I model my parts?"**: [`models/`](models/)

| File | Covers |
|---|---|
| [`models/MODELS.md`](models/MODELS.md) | Device models: built-in, SPICE, datasheet extraction |
| [`models/PACKS.md`](models/PACKS.md) | Model-pack format: bundle + share model/sensor/logic data |

**"How do I wire it into a pipeline?"**: [`ci/`](ci/)

| File | Covers |
|---|---|
| [`ci/CI.md`](ci/CI.md) | Headless hardware CI: the core workflow, spec format, assertions |
| [`ci/EXAMPLES.md`](ci/EXAMPLES.md) | Install, first run, every runnable example |
| [`ci/DOCKER.md`](ci/DOCKER.md) | Container images and how to run them |

**"What is this thing, honestly?"**: [`about/`](about/)

| File | Covers |
|---|---|
| [`about/ARCHITECTURE.md`](about/ARCHITECTURE.md) | End-to-end pipeline and design overview |
| [`about/CAPABILITIES.md`](about/CAPABILITIES.md) | Authoritative scope: every layer, full MCU coverage |
| [`about/COMPARISON.md`](about/COMPARISON.md) | Feature matrix and positioning vs the field |
| [`about/LIMITATIONS.md`](about/LIMITATIONS.md) | Honest limitations, triage and closure |
| [`about/release-and-licensing.md`](about/release-and-licensing.md) | Release process and the GPL boundary |

## Scoped directories

| Path | Covers |
|---|---|
| [`extending/`](extending/) | Contributor walkthroughs: add a part, sensor, logic IC, MCU variant, board format, model pack |
| [`spice-compat/`](spice-compat/) | SPICE compatibility statement + ngspice cross-check results |
| [`assets/`](assets/) | Images embedded by docs |

Root: `START_HERE.md`, `PROJECT_LAYOUT.md`, and this map.
