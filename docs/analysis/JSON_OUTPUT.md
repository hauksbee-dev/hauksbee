# The `hauksbee run --json` schema

`hauksbee run <board> --json`, alone or with any of
`--report/--drc/--lint/--si/--resources/--usb-c/--thermal/--ac/--headless`,
writes one JSON object to stdout. Fields are additive: new fields may appear,
existing ones keep their meaning, and an absent section means "that analysis
did not run". The schema is
`crates/hauksbee-engine/schemas/hauksbee-run-report.schema.json`, generated
from the Rust types (regenerate with
`UPDATE_RUN_SCHEMA=1 cargo test -p hauksbee-engine --test run_report_schema_drift`).
`schema_version` is currently `3` and bumps only on a breaking change.

The `hauksbee-ci run --json` stream is a different shape: [../ci/JSON_OUTPUT.md](../ci/JSON_OUTPUT.md).

Net names are the real KiCad names: escapes decoded (`{slash}` -> `/`) and
render braces dropped (`SCL_{2}` -> `SCL_2`).

## Top-level verdict

Success documents and the hard-error envelope (`{"ok": false, "error": "..."}`)
share the `ok` key.

| field | type | meaning |
|---|---|---|
| `ok` | bool | `verdict == "pass"` |
| `verdict` | string | `"pass"`, `"fail"`, or `"invalid"` |
| `serious_count` | int | `serious` findings (DRC shorts, a destroyed part, high-severity lint/SI) |
| `actionable_count` | int | serious + warnings + clearance groups |

`verdict` is `fail` when `serious_count > 0` or the surface's own `--strict`
gate would fail (`--lint` on medium findings, `--si` on any real finding, the
co-sim on any raised fault, `--strict-boot` on the boot advisory). It is
`invalid` when nothing gates but the claim could not be judged: a top-level
`refusal`, AC or thermal `valid:false`, undermined run-level evidence, or
unbound verdict-critical parts on a model-dependent surface. Precedence:
`fail` > `invalid` > `pass`. A strict run's document matches its exit code
(2 / 3 / 0). The exit code and the verdict part company only where documented
in [CI.md](../ci/CI.md#exit-codes): report-only runs, `--no-strict-thermal`,
an aborted analog solve, a runtime timing refusal.

DRC shorts are excluded from `serious_count` when `drc.version_warning` is set
(board newer than the validated copper extraction).

When active parts have no model, the lint/SI/check surfaces add a `notes`
entry (kind `coverage`) beginning `INCONCLUSIVE:`; the parts are in
`bind.active_path_unresolved[]` or `bind.resolved_but_open_active[]`, and on
model-dependent surfaces (`--lint`, `--si`, `--check`, `--resources`, the bare
report, `--usb-c`, co-sim) `verdict` reads `invalid`. `--drc` and `--report`
are exempt.

## Sections

| section | present when | shape |
|---|---|---|
| `board` | always | board name |
| `bind` | always | `resolved`, `unresolved`, `non_ignored`, `critical_parts_bound` (`"4/6"`, executable-behaviour coverage, not CAD coverage), `critical_parts_bound_n`, `critical_parts_total`, `mcu_bound`, `active_path_unresolved[]`, `resolved_but_open_active[]` |
| `inputs` | board plus any `--bom` / `--placement` / `--schematic` | `{path, kind, format, sha256?, contributed[], ignored[], identity[]}` per artifact |
| `findings` | lint / SI / co-sim faults ran | `Finding[]` |
| `drc` | `--drc` / `--check` | `clearance_rule_mm`, `primitive_count`, `shorts[]`, `violations[]`, `at_limit[]`, `suppression_note`, optional `version_warning` |
| `ac` | `--ac` | `valid` (+ `reason`), `nets[] {net, points:[[freq,mag_db,phase]]}`, `no_signal_path_nets[]`, `not_found_nets[]`, `coverage` |
| `thermal` | `--thermal` | `valid` (+ `reason`), `ambient_c`, `devices[] {reference, tj_c, over_limit}`, `coverage` |
| `boot_gates` | co-sim ran | per-transistor-gate power-up state (informational) |
| `notes` | any note fired | `{kind, message}[]`: bind roles, MCU substitution, coverage caveats. A note never gates on its own |
| `cosim` | a firmware co-sim ran | `CosimJson` below |
| `waived` | a waiver overruled a finding | `{check, kind, subject, reason, until}[]` |
| `error_budget` | numeric results | solver settings used, solved method windows, measured residual, failed intervals, event timestamp precision |

### `Finding`

| field | type | meaning |
|---|---|---|
| `check` | string | `drc`, `si`, `lint`, `cosim`, ... |
| `kind` | string | finding subtype (the waiver `kind`) |
| `severity` | string | `serious`, `warning`, `note`, `info` |
| `gating` | bool | this finding is a reason the run fails its gate. Wider than `serious`: medium lint findings, every real SI finding and every co-sim fault gate; a `note` never does. Read this, not `severity` |
| `nets` / `refs` | string[] | involved nets / references |
| `location_mm` / `layer` | optional | board location and copper layer |
| `actionable` | bool | a user can act on it |
| `message` | string | expert one-liner (what a waiver matches on) |
| `plain` | string | plain-language form |
| `fix` | string, optional | suggested remediation |

Copper shorts never appear in `findings[]`; they are `drc.shorts[]`:
`net_a`, `net_b`, `layer`, `gap_mm` (<= 0), `loc_mm [x,y]`, `severity`
(`serious`, or `note` under `version_warning`), `plain`, `fix`.
`drc.violations[]` and `drc.at_limit[]` are clearance groups with no
`severity`: `net_a`, `net_b`, `layer`, `count`, `below_count`, `at_limit`,
`min_gap_mm`, `min_gap_loc_mm`, `rule_mm`, `plain`, `fix`.

### `waived`

A waiver can only overrule a `serious` finding. The finding leaves
`serious_count` and `actionable_count` and lands in `waived[]` with the
`check`, `kind`, `subject` (the message, or `"<net_a> to <net_b> on <layer>"`
for a short), `reason` and `until`.

### `cosim`

Scalars (always present): `mcu_ref`, `backend` (e.g. `"simavr:atmega328p"`),
`requested_part`, `wall_s`, `realtime_factor` (measured sim-seconds per
wall-second, `0` when unmeasured), `total_toggles`, `uart_seen`,
`analog_valid`, `substituted`.

Arrays (omitted when empty):

| array | entries |
|---|---|
| `activity_summary[]` | `{net, toggles, v_min, v_max}` |
| `failed_windows[]` | analog spans no fallback could solve; values inside are invalid |
| `fallback_windows[]` | `{start_s, end_s, method, fidelity_note, error_estimate_v?}`: spans solved on a second-class integration rung |
| `timing_coverage[]` | `{mcu_ref, backend, cycle_exact, timestamp_precision_s, minimum_guaranteed_pulse_s, chunk_s}` |
| `timing_refusals[]` | runtime edge/PWL limits reached; non-empty exits 3 under `--strict` |
| `adc_dropped[]` | ADC channels whose injection the platform could not deliver |
| `unexercised_buses[]` | bound I2C/SPI peripherals the platform never exercised |
| `spi_framing[]` | per bus: tier `exact` / `backend` / `heuristic`; on `exact`, `cs_provenance` is `spec`, `model-roles` or `bitbang-pins` |
| `short_pulses[]` | `{net, mcu_ref, pin, pulse_s, chunk_s, parts[]}`: pulses inside one chunk that a tick-evaluated part never saw |
| `driver_contention[]` | `{net, mcu_ref, pin, parts[], t_s}`: two push-pull drivers on one net |
| `watchdog_limitations[]` | `{mcu_ref, limitation}`: the backend's watchdog does not bite like the part's |
| `watchdog_resets[]` | `{mcu_ref, resets}`: reboots that happened |
| `timing_limitations[]` | `{mcu_ref, limitation}`: known systematic time bias (the wall-paced `qemu:` family, the F103's 72 MHz TIMx) |

## CI artifact flags

`run --junit <file>` and `run --sarif <file>` write the selected surface
(waivers applied) as JUnit XML and SARIF 2.1.0. A finding is a JUnit
`<failure>` / SARIF `error` when `gating` is set; rule ids are `check/kind`.
A whole-run refusal is a JUnit `<error>` and SARIF `hauksbee/invalid-for-analysis`;
usage errors use `hauksbee/run-error`. Paths first receive a fail-closed
pending document, finalized once at the terminal outcome; a path that aliases
an input or the other artifact is refused. Under GitHub Actions a failing
`--strict` gate also prints `::error` annotations.

## Example

```jsonc
{
  "ok": false, "verdict": "fail", "serious_count": 1, "actionable_count": 2,
  "board": "my_board",
  "bind": { "resolved": 42, "unresolved": 0, "mcu_bound": true, "...": "..." },
  "drc": {
    "clearance_rule_mm": 0.2,
    "shorts": [{ "net_a": "GND", "net_b": "VCC", "layer": "F.Cu", "gap_mm": 0.0,
      "loc_mm": [12.4, 30.1], "severity": "serious",
      "plain": "GND shorts VCC on F.Cu at (12.40, 30.10) mm (gap 0.000 mm)",
      "fix": "separate the two nets' copper: widen the gap or reroute" }],
    "violations": [], "at_limit": []
  }
}
```
