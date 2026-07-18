# The `--json` output schema

`hauksbee run <board> --json` (and the per-check `--report --json`, `--ac --json`,
etc.) writes one JSON object to stdout, designed to be parsed by a CI step or an
agent. This documents every field so a consumer does not have to read the source.

Stability: fields are **additive**. New fields may appear; existing fields keep
their meaning. Empty/absent sections are omitted (`skip_serializing_if`), so
always treat a missing section as "that analysis did not run", not "it failed".

## Top-level verdict

Every success document begins with a machine rollup, sharing the `{"ok": ...}`
shape with the hard-error envelope (`{"ok": false, "error": "..."}`), so success
and failure parse the same way:

| field | type | meaning |
|---|---|---|
| `ok` | bool | `true` iff `verdict == "pass"` |
| `verdict` | string | `"pass"` \| `"fail"` \| `"invalid"` (see below) |
| `serious_count` | int | number of serious findings (DRC shorts + co-sim stress faults + serious lint/SI) |
| `actionable_count` | int | findings a user can act on (serious + warnings + clearance groups) |

`verdict` is `"fail"` when `serious_count > 0`; `"invalid"` when nothing is
serious but an analysis that ran could not be judged (AC or thermal reported
`valid:false`); `"pass"` otherwise. DRC shorts are excluded from `serious_count`
when the board is newer than the validated copper extraction (a
`drc.version_warning` is set) — the same carve-out the exit gate makes.

## Sections

`board` (string) and `bind` (object) are always present. The rest appear only
when the corresponding analysis ran.

| section | present when | shape |
|---|---|---|
| `board` | always | board name |
| `bind` | always | `BindSummary`: `resolved`, `unresolved`, `non_ignored`, `critical_parts_bound` (+ `_n`/`_total`), `mcu_bound`, `active_path_unresolved[]`, `resolved_but_open_active[]` |
| `findings` | lint / SI / co-sim faults ran | array of `Finding` (below) |
| `drc` | `--report`/`--drc` | `clearance_rule_mm`, `primitive_count`, `shorts[]`, `violations[]`, `at_limit[]`, optional `version_warning` |
| `ac` | `--ac` | `valid` (+ `reason`), `nets[]` (`{net, points:[[freq,mag_db,phase]]}`), `no_signal_path_nets[]`, `not_found_nets[]`, `coverage` |
| `thermal` | `--thermal` | `valid` (+ `reason`), `ambient_c`, `devices[]` (`{reference, tj_c, over_limit}`), `coverage` |
| `boot_gates` | boot-state panel | per-transistor-gate power-up state (informational) |
| `notes` | always non-empty on real runs | array of `{kind, message}` — bind roles, MCU substitution, coverage caveats; **informational, never gating** |
| `cosim` | a firmware co-sim ran | `CosimJson` (below) |

### `Finding` (the uniform finding shape)

`findings[]`, and each `drc.shorts[]`/`drc.violations[]` entry, carry:

| field | type | meaning |
|---|---|---|
| `check` | string | which analysis produced it (`drc`, `si`, `lint`, `cosim`, …) |
| `kind` | string | finding subtype |
| `severity` | string | `"serious"` \| `"warning"` \| `"note"` \| `"info"` |
| `nets` / `refs` | string[] | involved nets / component references |
| `location_mm` / `layer` | optional | board location and copper layer |
| `actionable` | bool | whether a user can act on it |
| `plain` | string | human one-line description |
| `fix` | string (optional) | suggested remediation |

DRC `shorts[]` and `violations[]` now carry `plain` and `fix` too, so every
finding category is uniform.

### `cosim` (co-sim coverage, incl. the honesty surfaces)

Beyond activity (`total_toggles`, `uart_seen`, `activity_summary[]`) and
`analog_valid` (+ `failed_windows[]`), the co-sim block surfaces every coverage
degradation so a run that silently lost fidelity never reads as healthy:

- `substituted` — the firmware ran on a substitute core (also in `notes`).
- `adc_dropped[]` — ADC channels whose modeled voltage the platform could not
  inject (the firmware read nothing on that pin).
- `unexercised_buses[]` — bound I2C/SPI peripherals the platform's controller set
  never exercised (a `peripheral` assertion against one fails, not green-passes).
- `spi_framing[]` — per-bus framing tier; a `"heuristic"` tier means transaction
  boundaries were guessed at chunk edges.

## Example

```jsonc
{
  "ok": false,
  "verdict": "fail",
  "serious_count": 1,
  "actionable_count": 2,
  "board": "my_board",
  "bind": { "resolved": 42, "unresolved": 0, "mcu_bound": true, "...": "..." },
  "drc": {
    "clearance_rule_mm": 0.2,
    "shorts": [{
      "net_a": "GND", "net_b": "VCC", "layer": "F.Cu", "gap_mm": 0.0,
      "loc_mm": [12.4, 30.1], "severity": "serious",
      "plain": "GND shorts VCC on F.Cu at (12.40, 30.10) mm (gap 0.000 mm)",
      "fix": "separate the two nets' copper — widen the gap or reroute…"
    }],
    "violations": [], "at_limit": []
  }
}
```
