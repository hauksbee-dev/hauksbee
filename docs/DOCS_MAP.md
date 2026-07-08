# Docs map: user path vs project record

`docs/` mixes two audiences: docs a new adopter needs to get value from the tool
(**USER PATH**), and the project's evidence trail, war stories, and internal
conventions (**PROJECT RECORD**). This map classifies every doc and records where
it lives after the START_HERE split.

- **USER PATH** docs stay at `docs/` top level.
- **PROJECT RECORD** docs move to `docs/record/`.
- `docs/dev-plans/` (design plans 00-09 + `research/`), `docs/how-and-why/` (code
  companion explanations), `docs/hunts/` (bug-hunt working directory), and
  `docs/assets/` (images) stay where they are: they are already scoped
  directories. `hunts/` is record-class; the `docs/record/` README points at it.

There is no docs index (no mdbook `SUMMARY.md`, no `mkdocs.yml`); the entry point
is `docs/START_HERE.md`, and the repo-root `README.md` leads with it.

## Top-level docs

| File | Role | Class | Destination |
|---|---|---|---|
| `AC_ANALYSIS.md` | AC / small-signal sweep: Bode, phase margin, gain crossover | USER | `docs/` (stay) |
| `ALTIUM.md` | Altium `.PcbDoc` ingest | USER | `docs/` (stay) |
| `ARCHITECTURE.md` | End-to-end pipeline and design overview | USER | `docs/` (stay) |
| `BOARD_AS_CODE.md` | Decompile/edit/recompile a board as editable code | USER | `docs/` (stay) |
| `CAPABILITIES.md` | Authoritative scope: every layer, MCU coverage, misconceptions | USER | `docs/` (stay) |
| `CI.md` | Headless hardware CI: the core workflow | USER | `docs/` (stay) |
| `COMPARISON.md` | Feature matrix and positioning vs the field | USER | `docs/` (stay) |
| `DEVICE_DECODE.md` | Config-pin divider decode check (`device_decode` lint class) | USER | `docs/` (stay) |
| `DOCKER.md` | Container images and how to run them | USER | `docs/` (stay) |
| `EXAMPLES.md` | Install, first run, runnable examples index | USER | `docs/` (stay) |
| `GERBER.md` | Gerber + pick-and-place reverse extraction | USER | `docs/` (stay) |
| `LIMITATIONS.md` | Honest limitations, triage and closure | USER | `docs/` (stay) |
| `MCU.md` | MCU co-sim backends and supported chips | USER | `docs/` (stay) |
| `MODELS.md` | Device models: built-in, SPICE, datasheet extraction | USER | `docs/` (stay) |
| `ORACLES.md` | Using ground-truth oracles (`--oracle`): kicad-cli, ngspice detection | USER | `docs/` (stay) |
| `PERIPHERALS.md` | Runtime peripherals (buttons, pots, I2C/SPI slaves) | USER | `docs/` (stay) |
| `RESOURCE_CONFLICTS.md` | MCU internal resource-conflict check | USER | `docs/` (stay) |
| `SCHEMATICS.md` | Schematic (`.kicad_sch`) extraction | USER | `docs/` (stay) |
| `SHORTS.md` | Copper short / clearance detection and simulation | USER | `docs/` (stay) |
| `SI_CHECKS.md` | Signal-integrity static checks (`--si`) | USER | `docs/` (stay) |
| `SIMULATORS.md` | Installing the external co-sim backends (Renode, Espressif QEMU) | USER | `docs/` (stay) |
| `THERMAL.md` | Steady-state junction-temperature check | USER | `docs/` (stay) |
| `TRANSIENTS.md` | Transient scenarios: load steps, brownout, inrush | USER | `docs/` (stay) |
| `BUG_HUNT.md` | Tarski InputSystem bug-hunt war story | RECORD | `docs/record/` |
| `FAMOUS_BUGS.md` | Dossier: famous bugs to re-derive + hunt corpus | RECORD | `docs/record/` |
| `FAMOUS_SWEEP.md` | Famous-board sweep campaign (rounds 1-5) | RECORD | `docs/record/` |
| `HUNT_TARGETS.md` | Dossier: which boards to hunt next | RECORD | `docs/record/` |
| `KNOWN_FAULTS_VALIDATION.md` | Known-fault calibration campaign | RECORD | `docs/record/` |
| `NAMING.md` | How the name was chosen (naming convention record) | RECORD | `docs/record/` |
| `TARSKI_RESULTS.md` | Flagship benchmark results / evidence | RECORD | `docs/record/` |
| `TEST_CAMPAIGN.md` | Test-campaign evidence: every claim traces to a test | RECORD | `docs/record/` |
| `VIDEO_PLAN.md` | Internal showcase-video storyboard | RECORD | `docs/record/` |

## Subdirectories (unchanged)

| Path | Role | Class | Destination |
|---|---|---|---|
| `docs/dev-plans/` | Design plans 00-09, perf notes, `research/` (dossiers, maps, the saga) | RECORD (already scoped) | stay |
| `docs/how-and-why/` | Code companion explanations (per-crate how-and-why) | USER (already scoped) | stay |
| `docs/spice-compat/` | SPICE compatibility statement (`compatibility.md`, the enforced supported/refused card list) + ngspice cross-check results (`results.md`) | USER (already scoped) | stay |
| `docs/hunts/` | Bug-hunt working directory: per-board narratives, briefs, results, report assets | RECORD (already scoped) | stay |
| `docs/assets/*` | Images embedded by docs | n/a | stay |

## Counts

- USER PATH: 23 top-level docs.
- PROJECT RECORD: 9 top-level docs moved to `docs/record/`, plus the `docs/hunts/`
  working directory and `docs/dev-plans/` (both stay in place, already scoped).
