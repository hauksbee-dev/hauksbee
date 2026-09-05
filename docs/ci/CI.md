# CI for hardware: `hauksbee-ci`

A spec is a TOML file checked in beside the board. `hauksbee-ci run` boots
the firmware on the emulated board headless, runs the spec's simulated
duration, evaluates every `[[assert]]`, and reports with an exit code, a
JUnit file, JSON and GitHub annotations.

## Commands

```
hauksbee-ci init <board> [--out PATH]            # scaffold <board-stem>.toml with detected MCU, supplies, rails
hauksbee-ci check <spec>... [--no-board] [--json] [--models-dir DIR]
hauksbee-ci run <spec>... | --example NAME  [--junit OUT.XML] [--json] [--quiet] [--seed N] [--models-dir DIR]
hauksbee-ci hook install | uninstall             # pre-commit gate (.pre-commit-config.yaml entry, or .git/hooks/pre-commit)
hauksbee-ci github-action [--write [PATH]]       # print, or write .github/workflows/hauksbee.yml
```

- `init` writes into the current directory (or `--out`, a `.toml` path or a
  directory), with `board` relative to where the spec lands; refuses to
  overwrite. With no argument it looks for exactly one board file.
- `check` validates without simulating: TOML and vocabulary, field bounds,
  block structure, cross-references, and (unless `--no-board`) every net and
  component reference against the loaded board, plus BOM/placement/variant/
  firmware resolution. `--json` prints one array per spec of
  `{line, col, code, message, fix}`. Exit 0 valid, 2 otherwise.
- `run` takes several specs (or a glob): one summary, one merged `--junit`
  document (a `<testsuite>` per spec), one JSON line per spec, and the worst
  exit code (3 > 2 > 1 > 0). `--seed N` re-runs one ensemble member
  byte-identically. `--quiet` keeps only the exit code.
- `--models-dir` layers above the builtin db, installed packs and the user
  model dirs, exactly as `hauksbee run --models-dir` ([MODELS.md](../models/MODELS.md)).

```bash
hauksbee-ci run crates/hauksbee-ci/examples/blinky.toml
hauksbee-ci run crates/hauksbee-ci/examples/tarski_brownout.toml; echo $?   # 1
hauksbee-ci run crates/hauksbee-ci/examples/tarski_brownout_repaired.toml --junit results.xml
```

## Spec format

Unknown keys, nets and references are errors (with near-matches). Paths are
relative to the spec file.

### Top level

| Key | Type | Default | Meaning |
|---|---|---|---|
| `name` | string | `"hauksbee-ci"` | label in reports |
| `board` | path | required | any format `hauksbee run` accepts |
| `bom`, `bom_columns`, `placement`, `variant` | path, [string], path, path | none | assembly inputs ([BOM.md](../ingest/BOM.md), [DNP.md](../ingest/DNP.md)) |
| `schematic` | path | none | Eagle `.sch` companion, identity-validated and inventoried |
| `firmware` | path | none | `.elf`/`.hex`, PlatformIO project dir, or a zip of either |
| `mcu` | string | none | informational only; the MCU comes from the board's part value via `[[models]] kind = "mcu"` routing |
| `[mcu] descriptor_dir` | path | none | SoC descriptor override directory ([add-a-microcontroller.md](../extending/add-a-microcontroller.md)) |
| `duration_ms` | float | `100` | simulated time |
| `frame_ms` | float | `1` | sampling cadence |
| `[timing]` | table | none | `min_pulse_us`, `max_edge_error_us` |
| `ambient_c` | float | `25` | ambient for `max_temp` |
| `asbuilt` | path | none | `.asbuilt.toml` rework overlay (cut traces, jumpers, lifted pins, fitted values), fail-loud; same file as `hauksbee run --asbuilt` |
| `fit`, `no_fit` | [string] | `[]` | DNP overrides |
| `dnp` | string | `"fit-except-links"` | `fit-except-links` \| `fit-all` \| `honour` |
| `suppress_rail` | [string] | `[]` | nets whose auto-rail is removed (fed only through the board) |
| `[[supply]]`, `[[net_drive]]`, `[[override]]` | blocks | none | below |
| `[[tolerance]]`, `[ensemble]`, `[fuzz]` | | none | below |
| `[[scenario]]`, `[[profile]]`, `[decoupling]` | | none | [TRANSIENTS.md](../checks/TRANSIENTS.md) |
| `[ac]` | table | none | [AC_ANALYSIS.md](../analysis/AC_ANALYSIS.md#ci) |
| `[[peripheral]]`, `[[sensor]]` | blocks | none | below |
| `[[assert]]` | blocks | at least one | below |

### Supplies, drives, overrides

```toml
[[supply]]
net = "+5V"
kind = "bench"            # ideal | bench | wall | usb | battery
volts = 5.0
current_limit_a = 1.0
```

Per kind: `ideal` and `bench` take `volts` (required) and bench
`current_limit_a`; `wall` takes `volts`, `r_out_ohms`, `ripple_vpp`,
`ripple_hz`; `usb` takes `usb = "5v0.5a" | "5v1.5a" | "5v3a"`; `battery`
takes `chemistry = "liion" | "lipo" | "alkaline" | "nimh" | "lifepo4" | "lfp"`,
`cells`, `capacity_mah`, `soc`, `r_internal_ohms`, and optional
`protection_trip_a`, `protection_delay_ms`, `protection_reset_a`.

```toml
[[net_drive]]             # force a net for the whole run
net = "WSEL"
volts = 5.0

suppress_rail = ["ANALOG_VDD"]

[[override]]              # swap a value before binding (how a repair is expressed)
ref = "R_Shunt15301"
value = "0.05"
tolerance = 1.0           # optional: this part's own spread (percent)
```

### Timing

```toml
[timing]
min_pulse_us = 20.0
max_edge_error_us = 4.0
```

simavr stamps every edge at its cycle. Renode and QEMU poll after each
chunk, so the chunk shrinks to the tighter of `min_pulse_us / 2` and
`max_edge_error_us`; a request needing a chunk below 1 us is refused. A
`toggle` assertion on a poll backend must declare `min_pulse_us`. An unmet
contract makes every assertion INVALID (exit 3) and cannot be waived. Every
report publishes the per-MCU timestamp precision, pulse floor and chunk.

### Fuzzing

```toml
[fuzz]
seeds = 16
nets = ["WSEL", "OE_N", "RCLK"]   # undefined power-up bits
levels = [0.0, 5.0]
```

Seed 0 is the all-low baseline; an assertion must hold on every seed.

### Component tolerances

```toml
[[tolerance]]
ref = "R*"                 # literal or glob (* only); last matching rule wins
percent = 10.0
distribution = "gaussian"  # "uniform" (default) | "gaussian" (sigma = tol/3, truncated)

[ensemble]
seeds = 24                 # Monte-Carlo members (default 16)
mode = "monte-carlo"       # "monte-carlo" (default) | "corners"
```

Monte-Carlo samples every toleranced part per member (seed 0 nominal);
values are a pure function of (seed, reference, rule), so `--seed N`
reproduces a member exactly. Corner mode enumerates all `2^n` min/max
combinations (member k puts component i at max when bit i of k is set; capped
at 10 components; no nominal member; does not compose with `[fuzz]`) plus a
Latin-hypercube interior sample (4 probes for one component, 6 for two, 8
from three); an interior probe failing where every corner passed fails the
assertion. A green corner run bounds the worst case only where the response
is monotonic per value, and the report says so.

### Peripherals and sensors

```toml
[[peripheral]]
id = "BTN1"
type = "pushbutton"       # pushbutton | toggle | potentiometer | encoder | stimulus |
                          # i2c_eeprom | i2c_lm75 | spi_eeprom | spi_mcp3008 | vcd_sink
net = "SW_IN"             # or ref/pin, a/wiper/b, net_a/net_b per type
# bounce_ms, initial, r_total, vhigh, address, size, temp_c, vref, nets = [...] per type

[[sensor]]
id = "U2_temp"
spec_file = "sensors/lm75.toml"   # or: spec = """ [sensor] ... """
controller = "spi2"               # optional SPI controller
cs_net = "CS_IMU"                 # optional explicit chip-select net (exact framing)
[sensor.inputs]
temperature_c = 40.0              # unknown names are refused
```

Runnable: `crates/hauksbee-ci/examples/lm75_thermostat.toml`;
`crates/hauksbee-ci/tests/peripherals.rs` holds copy-paste fixtures.

### Assertions

At least one is required. `name` is an optional label on any kind.

| Kind | Fields | Checks |
|---|---|---|
| `voltage` | `net`, `min` and/or `max`, `after_ms` | the net's worst dip / rise in the window |
| `uart` | `contains` or `matches` (regex), `mcu` (ref; default all) | firmware serial output |
| `toggle` | `net`, `freq_hz` + `tolerance` (fraction, default 0.25) or `min_toggles` | a count is trustworthy on every backend; a frequency rests on the clock table in [MCU.md](../cosim/MCU.md#clock-fidelity) |
| `no_faults` | | no over-current / over-voltage / over-power / reverse-bias / over-temperature fault |
| `max_current` | `ref`, `amps` | peak through-current (resistors and diodes) |
| `max_temp` | `ref`, `celsius` (may be omitted only when the model carries a datasheet Tj max) | steady-state Tj ([THERMAL.md](../checks/THERMAL.md)); a target that never dissipates fails, not passes |
| `boot_coverage` (alias `boot-coverage`) | `net`, `min`, `deadline_ms` | the firmware actively drives a Hi-Z control net to >= `min` V within the deadline, with no fault during the boot window |
| `model_coverage` | `min_critical` (fraction of active ICs bound), `max_active_unresolved`, `min_resolved`; at least one | how much of the board bound to a real model; a failure names the parts |
| `phase_margin` / `ac_gain` | `net`, `min`/`max`, `freq_hz` (ac_gain) | need an `[ac]` block |
| `rail_window` | `net`, `scenario`, `min`/`max`, `dip_below`, `for_max_ms`, `recover_to`, `recover_within_ms` | rail over a scenario window ([TRANSIENTS.md](../checks/TRANSIENTS.md)) |
| `protection_trip` | `supply_net`, `expect_trip`, `scenario` | a battery protection cutoff fired or did not |
| `peripheral` | `id`, `field` (or `bytes`) | a peripheral's end-of-run state; fails, never passes, on an unexercised bus |
| `hwtrace` | `trace = "trace.toml"` | simulated waveform against a captured scope CSV or logic-analyser VCD (`testdata/hwtraces/`), one result per channel and feature |

```toml
[[assert]]
kind = "voltage"
net = "ANALOG_VDD"
min = 4.9
after_ms = 50

[[assert]]
kind = "boot_coverage"
net = "GATE_CTRL"
min = 3.0
deadline_ms = 20.0

[[assert]]
kind = "model_coverage"
min_critical = 1.0
max_active_unresolved = 0
```

`boot_coverage` decides a class the netlist cannot: a gate, enable, reset or
chip-select driven only by a GPIO that is Hi-Z at reset. The two-sided demo is
`boot_gate_pass.toml` / `boot_gate_fail.toml` on
`crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb` with
`testdata/firmware/boot_gate_{a,b}/`.

## Waivers

One `hauksbee-waivers.toml` beside the board reaches every gate:
`hauksbee run --check --strict`, the single-check gates, and `hauksbee-ci run`.

```toml
[[waive]]
check = "si"                      # "si", "lint", "drc", or "ci"
kind = "controlled_impedance"     # the rule as it appears in --json (for "ci": the assertion kind)
nets = ["USB_DP", "USB_DM"]       # or refs = ["U3"]; every listed net must be in the finding (AND)
reason = "measured 92 ohm on the fab's stackup"
until = "2026-12-31"
```

`reason`, `until`, and `nets` or `refs` are required. On expiry the finding
comes back and the lapsed waiver is named. Waived findings are printed in a
`== Waived ==` section, appear as `<skipped>` in JUnit with the reason, as a
`::warning` on GitHub, and in the `waived` JSON field; they never turn the
exit code red. An INVALID result cannot be waived. Clearance violations do
not gate and are not waivable. A waiver file that does not parse is a
warning and every finding gates. Waivers that matched nothing are reported.

## Output

- **Terminal**: `PASS`/`FAIL` per assertion with the measured value and a
  one-line `why:` on a red, then `GREEN`/`RED`. A GREEN run suggests
  `hauksbee-ci hook install` until the hook exists; a RED run links to this
  page's section for the failing kind.
- **JUnit** (`--junit`): one `<testcase>` per assertion; waived failures are
  `<skipped>`; multi-spec runs merge into one document.
- **JSON** (`--json`): NDJSON, one object per spec, schema
  `crates/hauksbee-ci/schemas/hauksbee-ci-report.schema.json`;
  field-by-field in [JSON_OUTPUT.md](JSON_OUTPUT.md).
- **GitHub annotations** when `GITHUB_ACTIONS` is set: `::error` per failing
  or INVALID assertion (capped at 8 plus an overflow line), `::warning` for
  dead rails, waived failures, substitutions and coverage holes (capped at 9).

## Schematic-stage CI

Point `board` at a `.kicad_sch` hierarchy root and the same spec runs before
a layout exists ([SCHEMATICS.md](../ingest/SCHEMATICS.md)). For a project
with both, the verdict is the same at either stage
(`crates/hauksbee-ci/tests/schematic_ci.rs`). KiCad's schematic editor has
no plugin API; use the pre-commit hook, the CLI, or the pcbnew plugin (which
discovers specs whose `board` is the project's `.kicad_sch`).

## Wiring it into a repo

```bash
hauksbee-ci hook install           # idempotent; preserves an existing hook
hauksbee-ci github-action --write  # .github/workflows/hauksbee.yml; refuses to overwrite a diverged file
```

- Pre-commit: `integrations/pre-commit/` (`.pre-commit-config.yaml` entry,
  [README](../../integrations/pre-commit/README.md)).
- GitHub Actions: `integrations/github-action/` with `mode: spec`
  (`spec` or `specs` inputs, globs allowed, one merged JUnit) or `mode: check`
  (`board` input, runs `hauksbee run <board> --check --strict`); with neither,
  it auto-detects exactly one spec in `ci/` or the root, else exactly one
  board file, and fails on ambiguity. `example-workflow.yml` is the annotated
  version ([README](../../integrations/github-action/README.md)).
- KiCad: `integrations/kicad-plugin/` runs a spec from pcbnew.
- Any CI: `hauksbee-ci run ci/*.toml --junit results.xml` and consume the
  exit code.

### The zero-config gate

```bash
hauksbee run my_board.kicad_pcb --check --strict --junit hauksbee.xml --sarif hauksbee.sarif
```

`--junit` / `--sarif` on `hauksbee run` write the selected surface with
waivers applied: one `<testsuite>` per check and a `<testcase>` per finding
(gating findings as `<failure>`, a clean check as a passing `no findings`
case); SARIF 2.1.0 with rule ids `check/kind`, gating findings as `error`,
non-gating as `warning`. A whole-run refusal is a JUnit `<error>` / SARIF
`hauksbee/invalid-for-analysis`; usage errors use `hauksbee/run-error`. Paths
receive a fail-closed pending document first and are finalized once, and a
path aliasing an input or the other artifact is refused.

## Exit codes

`hauksbee-ci run`:

| exit | meaning |
|---|---|
| 0 | every assertion held, or every failure is covered by an active waiver |
| 1 | at least one assertion failed with no waiver |
| 2 | spec/board error, usage error, or a requested output file that could not be written |
| 3 | invalid for analysis: the analog solve aborted, an assertion's window overlapped a failed span, a timing contract could not be met, or an evidence map is undermined |

A waived failure exits 0 while its own result reports `passed: false`. A
multi-spec run exits with the worst code (3 > 2 > 1 > 0).

`hauksbee run <board>` reports (`--drc`, `--lint`, `--si`, `--usb-c`,
`--check`, bare `--json`) exit 0 even with findings unless `--strict` (alias
`--fail-on-findings`):

| exit | meaning |
|---|---|
| 0 | clean, or a report-only run without `--strict` |
| 1 | the board could not be read or the analysis could not be set up |
| 2 | gate-grade findings under `--strict` (or `--strict-boot`; co-sim stress faults also gate); also a usage error |
| 3 | invalid for analysis: aborted analog solve, zero-activity co-sim under `--strict`, thermal table with no usable coverage or PARTIAL coverage (`--no-strict-thermal` opts out), undermined evidence or unbound verdict-critical parts on a model-dependent surface under `--strict` |

What `--strict` gates on: `--drc` true shorts (clearance notes never),
`--lint`/`--resources` high and medium findings, `--si` any real finding,
`--usb-c` a serious CC verdict, `--check` the union. On KiCad 10+ boards
possibly-phantom shorts do not gate. `--drc` and `--report` are exempt from
the INCONCLUSIVE escalation. The static surfaces print the same document with
or without `--strict`; on the co-sim path the zero-activity and analog-abort
refusals and the boot advisory only reach the document under their flag, and
an aborted solve or a runtime timing refusal exits 3 even beside a `fail`
document.

## Worked example: the Tarski brownout

`testdata/tarski_brownout_cell.net` is a cell where `ANALOG_VDD` is fed
through a "shunt" of 1 kOhm and a 74HC595 weight bit powers up undefined.
`crates/hauksbee-ci/examples/tarski_brownout.toml` drives `+5V`/`+5P`,
suppresses the auto-rail on `ANALOG_VDD`, fuzzes the weight bit over 8 seeds,
and asserts `ANALOG_VDD >= 4.9 V`:

```
[FAIL] ANALOG_VDD comes up across all power-up register states
      seed 1: ANALOG_VDD: min=0.802V < required 4.9V <- FAILED HERE [settled 0.802V]; passed 5/8 seeds (failing: 1, 2, 7)
```

`tarski_brownout_repaired.toml` adds one `[[override]]` (`R_Shunt15301` to
`0.05`) and holds at 4.987 V across all eight seeds. Both are pinned by
`crates/hauksbee-ci/tests/flagship_brownout.rs`.

## Limitations

- Renode and QEMU must be installed; a missing emulator is an error, never a
  fallback to another core. GPIO on those backends is polled per chunk.
- `max_current` tracks resistors and diodes; `no_faults` covers the rest.
- Fuzzing perturbs named nets' initial levels, not MCU RAM.
- Ensemble members run serially.
- The sub-sheet guard detects a sub-sheet referenced from the same or parent
  directory; point specs at the root `.kicad_sch` regardless.
