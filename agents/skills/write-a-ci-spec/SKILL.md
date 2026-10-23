---
name: write-a-ci-spec
description: Write, validate, and run a hauksbee-ci spec - a TOML file that powers a board, optionally boots firmware, and asserts on rails, UART, toggles, faults, temperature, and transient scenarios. Use when asked to add hardware CI, gate a board in a pipeline, check a rail under load, or produce JUnit or machine-readable results.
---

# Write and run a hauksbee-ci spec

A spec is one TOML file: the board, optional firmware, supplies, and at least
one `[[assert]]`. Full format: `docs/ci/CI.md`. JSON output contract:
`docs/ci/JSON_OUTPUT.md`.

## 1. Scaffold from the board

```bash
cd ci && hauksbee-ci init ../hardware/board.kicad_pcb
```

`init` writes `<board-stem>.toml` into the current directory (or `--out
PATH`), with the detected MCU, supplies and rails filled in and every line
commented. The `board = ...` path is relative to the spec. It refuses to
overwrite an existing spec. The scaffold supplies rails as `bench` or `usb`
sources, never `ideal`: an ideal source pins its net, so a voltage assertion on
that net cannot fail for a board reason.

## 2. Edit the assertions

A minimal firmware spec, verified against the bundled blinky board:

```toml
name = "blinky power-up"
board = "blinky.kicad_pcb"
firmware = "demo.hex"          # .elf/.hex, a PlatformIO project dir, or a zip
duration_ms = 1000

[[supply]]
net = "+5V"
kind = "bench"                 # ideal | bench | wall | usb | battery
volts = 5.0
current_limit_a = 2.0

[[assert]]
kind = "voltage"
net = "+5V"
min = 4.75
max = 5.25
after_ms = 50

[[assert]]
kind = "uart"
contains = "hauksbee-demo v1"

[[assert]]
kind = "toggle"
net = "D13"
freq_hz = 5.0
tolerance = 0.2

[[assert]]
kind = "no_faults"
```

Other assertion kinds include `max_current`, `max_temp`, `rail_window`,
`boot_coverage`, `pulse_width`, `protection_trip`, `phase_margin`, `ac_gain`,
`peripheral`, `hwtrace`, `model_coverage`, and the firmware-behaviour kinds `reaches`, `bus`, `resets` and `uart_baud` (the last three need the co-sim bench record and report INVALID until a run supplies it). To start from what the firmware does rather than from numbers, run `hauksbee-ci init <board> --firmware <image> --suggest-asserts`, and add one check with `hauksbee-ci add <spec> <kind> key=value...` (`--observe` takes unset numbers from a nominal run). `docs/ci/CI.md` documents each
one's fields.

When the user can say what the board is FOR, rather than which voltage a net
should sit at, use the outcome kinds. `logic_level` checks that a net is read
as a valid level by every input it drives. `fires` checks that a comparator
fires, optionally at a `target` trip level. Add a `[search]` block of
`[[search.sweep]]` nets (`volts` or `amps`) to give the conditions those
outcomes must hold across. A failure names its worst condition, ranked culprit
parts, and a solved repair value.

Transients (load steps, WiFi bursts, inrush) are not a `hauksbee run` flag. They
are a `[[profile]]` (the load current shape), a `[[scenario]]` that attaches it
to a supply net, and a `rail_window` assertion. `init` writes a commented stub,
and `examples/ci-specs/olimex_wifi_burst_transient.toml` is a complete example.

Tolerance ensembles use `[[tolerance]]` plus `[ensemble]`, and power-up state
fuzzing uses `[fuzz]`. Both run several seeds and fail if any seed fails.

## 3. Validate without simulating

```bash
hauksbee-ci check spec.toml
```

Checks TOML syntax, fields, bounds and cross-references, and loads the board to
check every net and ref. It exits 0 (`spec.toml: OK`) or 2 with a located
error and suggestions:

```text
typo.toml:25:8: [unknown-net] 'D12' (referenced in assert) is not a net on the board (did you mean: D13, +5V, ADC0, GND?)
```

## 4. Run

```bash
hauksbee-ci run spec.toml                     # human report
hauksbee-ci run spec.toml --json              # one JSON object on stdout
hauksbee-ci run spec.toml --junit results.xml # JUnit for the CI test-report step
hauksbee-ci run ci/*.toml --junit results.xml # several specs, merged, worst exit code
```

Exit codes for `hauksbee-ci run` (different from `hauksbee run`):

| exit | meaning |
|---|---|
| 0 | GREEN: every assertion held, or every failure is covered by an active waiver |
| 1 | RED: an assertion failed with no active waiver |
| 2 | spec or board error, usage error, or an output file that could not be written |
| 3 | INVALID: an aborted solve, a window over a failed solve span, an assertion whose evidence is `undermined` (for example an unmodelled active part that can drive the asserted net), or a host-paced clock (ESP32 QEMU) cannot judge the claim: firmware content that is absent at the end of the run, or a level that holds only inside its window (`INVALID: host-paced clock:`) |

In `--json`, read `passed`, `assertions_passed`, and `run_valid` together.
On exit 3, `assertions_passed` can be `true` while `passed` and `run_valid`
are `false`. A spec error prints `{"ok":false,"error":"..."}` and exits 2.
Each `results[]` entry has `label`, `kind`, `passed`, `invalid`, `detail`,
`evidence`, and for ensembles `failing_seed`, `failing_seeds`, `seeds_total`.

Other `run` flags: `--quiet` (no human report), `--seed N` (rerun one ensemble
member exactly), `--models-dir DIR`, `--waveforms DIR` (CSV of every asserted
net), and `--emit-manifest FILE` (an immutable manifest that `hauksbee
reproduce FILE` replays).

## 5. Wire it in

```bash
hauksbee-ci hook install            # pre-commit gate for this repo
hauksbee-ci github-action --write   # writes .github/workflows/hauksbee.yml
```

The hook and the action look for specs in `ci/` and the repo root; set
`HAUKSBEE_CI_SPECS` (colon-separated) to change that. GitLab, Jenkins, Azure
and Buildkite blocks are in `docs/ci/RECIPES.md`.

## Rules

- Never assert on a net fed by an `ideal` supply and call the result a gate.
- A green run with `evidence: qualified` carries caveats. Report them.
- To accept a judged failure, add a `hauksbee-waivers.toml` entry with a
  `reason` and an `until` date (see "Waiving a finding" in `docs/ci/CI.md`).
  Do not delete the assertion.
- For a RED or INVALID result, use the `debug-a-red-run` skill.

Over MCP, `run_checks` takes `board_path`, `spec_toml` (the spec without
`board`, `firmware`, or `schematic` keys), and optional `firmware_path`, and
returns the same object as `--json`.
