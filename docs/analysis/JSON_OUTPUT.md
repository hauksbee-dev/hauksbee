# The `--json` output schema

`hauksbee run <board> --json` (and the per-check `--report --json`, `--ac --json`,
etc.) writes one JSON object to stdout, designed to be parsed by a CI step or an
agent. This documents every field so a consumer does not have to read the source.

Stability: fields are **additive**. New fields may appear. Existing fields
keep their meaning. Empty/absent sections are omitted
(`skip_serializing_if`), so always treat a missing section as "that analysis
did not run", not "it failed".

## `schema_version` and the generated schema

Every document carries a top-level `schema_version` (integer, currently `1`).
It bumps only on a breaking change (a field removed or changed in meaning);
additive fields never bump it. The machine-checkable contract is the JSON
Schema generated from the Rust types at
`crates/hauksbee-engine/schemas/hauksbee-run-report.schema.json`; a drift test
keeps the file and the types identical (regenerate with
`UPDATE_RUN_SCHEMA=1 cargo test -p hauksbee-engine --test run_report_schema_drift`).

## Net names are the real KiCad names

Net names in every field (`nets`, `bind` lists, `--list-nets`, DRC shorts)
are the names the schematic shows: KiCad file-syntax escapes are decoded
(`/GPIO0{slash}XTAL1` arrives as `/GPIO0/XTAL1`) and render markup braces are
dropped (`SCL_{2}` arrives as `SCL_2`). Consumers that previously saw the
escaped spellings should match on the real names.

## CI artifact flags

`run --junit <file>` and `run --sarif <file>` write the full static suite
(the `--check` findings, waivers already applied) as JUnit XML and SARIF
2.1.0 respectively, alongside whatever report was requested. Serious
findings map to JUnit failures / SARIF `error` results. Under GitHub Actions
a failing `--strict` gate also prints `::error` workflow annotations for each
gate-grade finding.

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

`verdict` is `"fail"` when `serious_count > 0`. It is `"invalid"` when
nothing is serious but an analysis that ran could not be judged (AC or
thermal reported `valid:false`). Otherwise it is `"pass"`. DRC shorts are
excluded from `serious_count` when the board is newer than the validated
copper extraction (a `drc.version_warning` is set). This is the same
carve-out the exit gate makes.

## Sections

`board` (string) and `bind` (object) are always present. The rest appear only
when the corresponding analysis ran.

| section | present when | shape |
|---|---|---|
| `board` | always | board name |
| `bind` | always | `BindSummary`: `resolved`, `unresolved`, `non_ignored`, `critical_parts_bound` (the `"4/6"` display string), `critical_parts_bound_n`, `critical_parts_total`, `mcu_bound`, `active_path_unresolved[]`, `resolved_but_open_active[]` |
| `inputs` | `--report --json` with input inventory context | array of `{path, kind, format, sha256?, contributed[], ignored[], identity[]}` for the board and any `--bom` / `--placement` artifacts |
| `findings` | lint / SI / co-sim faults ran | array of `Finding` (below) |
| `drc` | `--drc` / `--check` | `clearance_rule_mm`, `primitive_count`, `shorts[]`, `violations[]`, `at_limit[]`, optional `version_warning` |
| `ac` | `--ac` | `valid` (+ `reason`), `nets[]` (`{net, points:[[freq,mag_db,phase]]}`), `no_signal_path_nets[]`, `not_found_nets[]`, `coverage` |
| `thermal` | `--thermal` | `valid` (+ `reason`), `ambient_c`, `devices[]` (`{reference, tj_c, over_limit}`), `coverage` |
| `boot_gates` | boot-state panel | per-transistor-gate power-up state (informational) |
| `notes` | at least one note fired | array of `{kind, message}`, bind roles, MCU substitution, coverage caveats; **informational, never gating** |
| `cosim` | a firmware co-sim ran | `CosimJson` (below) |
| `waived` | a waiver overruled a finding | array of `{check, kind, subject, reason, until}` (below) |

### `Finding` (the uniform finding shape)

Each `findings[]` entry carries:

| field | type | meaning |
|---|---|---|
| `check` | string | which analysis produced it (`drc`, `si`, `lint`, `cosim`, …) |
| `kind` | string | finding subtype |
| `severity` | string | `"serious"` \| `"warning"` \| `"note"` \| `"info"` |
| `nets` / `refs` | string[] | involved nets / component references |
| `location_mm` / `layer` | optional | board location and copper layer |
| `actionable` | bool | whether a user can act on it |
| `message` | string | the expert one-line message (the text the waiver matcher matches on) |
| `plain` | string | the same finding in plain language; equals `message` when no dedicated plain template applies |
| `fix` | string (optional) | suggested remediation; omitted when no fix template applies to this kind |

The DRC entries are their own shapes, not `Finding`s, though they carry the same
`plain` / `fix` text so every category reads uniformly.

`drc.shorts[]` (one real short, touching copper):

| field | type | meaning |
|---|---|---|
| `net_a` / `net_b` | string | the two shorted nets |
| `layer` | string | copper layer, e.g. `"F.Cu"` |
| `gap_mm` | float | the measured gap (`<= 0` for a short) |
| `loc_mm` | [float; 2] | board location of the short |
| `severity` | string | `"serious"`, downgraded to `"note"` when `version_warning` is set (the shorts may be phantom on an unvalidated board format) |
| `plain` | string | human one-line description |
| `fix` | string | suggested remediation |

`drc.violations[]` and `drc.at_limit[]` (clearance findings grouped by
`net_a`/`net_b`/layer/root cause, one line with a count, and **no** `severity`
field):

| field | type | meaning |
|---|---|---|
| `net_a` / `net_b` | string | the two nets |
| `layer` | string | copper layer |
| `count` | int | locations in the group |
| `below_count` | int | how many of `count` are genuinely below the rule; the rest sit exactly at it |
| `at_limit` | bool | true when every member is exactly at the rule (no margin, not below) |
| `min_gap_mm` | float | the tightest gap in the group |
| `min_gap_loc_mm` | [float; 2] | location of that tightest gap, so a UI can pan to the worst spot |
| `rule_mm` | float | the clearance rule for this pair |
| `plain` | string | human one-line description |
| `fix` | string | suggested remediation |

### `waived` (findings a waiver overruled)

A waiver can only overrule a **serious** finding (waiving a note would suppress
information without changing an outcome). When it fires, the finding is dropped
from *both* `serious_count` and `actionable_count`, so the verdict can go green
on its account. The finding is never silently discarded: it lands in the
top-level `waived[]` array, which a pipeline can watch grow.

| field | type | meaning |
|---|---|---|
| `check` | string | which check produced the overruled finding |
| `kind` | string | the rule it was matched on |
| `subject` | string | what was overruled: the finding's own `message` for lint/SI/co-sim, `"<net_a> to <net_b> on <layer>"` for a DRC short |
| `reason` | string | why the board's owner judged it wrong or acceptable |
| `until` | string | the date the waiver stops applying, after which it gates again |

### `cosim` (co-sim coverage, incl. the honesty surfaces)

The block identifies what actually ran and how fast:

| field | type | meaning |
|---|---|---|
| `mcu_ref` | string | the board reference the firmware ran on, e.g. `"U1"` |
| `backend` | string | the emulator and core that ran it, e.g. `"simavr:atmega328p"` |
| `requested_part` | string | the part the board asked for, e.g. `"ATmega328P"` |
| `wall_s` | float | wall-clock seconds the headless co-sim loop consumed; `0` when the producing path did not measure |
| `realtime_factor` | float | *achieved* sim seconds per wall second, measured rather than assumed; `0` when unmeasured |

Beyond that, activity (`total_toggles`, `uart_seen`, `activity_summary[]`) and
`analog_valid` (+ `failed_windows[]`), the co-sim block surfaces every coverage
degradation so a run that silently lost fidelity never reads as healthy:

- `substituted`: the firmware ran on a substitute core (also in `notes`).
- `adc_dropped[]`, ADC channels whose modelled voltage the platform could not
  inject (the firmware read nothing on that pin).
- `unexercised_buses[]`, bound I2C/SPI peripherals the platform's controller set
  never exercised (a `peripheral` assertion against one fails, not green-passes).
- `spi_framing[]`, per-bus framing tier: a `"heuristic"` tier means transaction
  boundaries were guessed at chunk edges.
- `short_pulses[]`, firmware GPIO pulses that rose and fell inside one solver
  chunk on a net clocking a TICK-evaluated sequential part, so that part never
  observed the pulse. Entries carry `{net, mcu_ref, pin, pulse_s, chunk_s,
  parts[]}`: the narrowest completed pulse, the chunk it fell inside, and the
  parts that missed it.
- `driver_contention[]`, nets where the firmware configured an MCU pin as a
  push-pull output while an enabled modelled push-pull output was already
  driving. Entries carry `{net, mcu_ref, pin, parts[], t_s}`; waveforms touching
  the net are untrustworthy from `t_s` on.

Every array above (`activity_summary[]`, `failed_windows[]`, `spi_framing[]`,
`adc_dropped[]`, `unexercised_buses[]`, `short_pulses[]`,
`driver_contention[]`) is omitted when empty; the scalars are always present.
A clean short run on `blinky.kicad_pcb` with `testdata/firmware/demo/demo.hex`:

```json
{
  "activity_summary": [
    { "net": "D13", "toggles": 1, "v_max": 4.600922828787499, "v_min": 0.0 }
  ],
  "analog_valid": true,
  "backend": "simavr:atmega328p",
  "mcu_ref": "U1",
  "realtime_factor": 0.5798839706103853,
  "requested_part": "ATmega328P",
  "substituted": false,
  "total_toggles": 1,
  "uart_seen": true,
  "wall_s": 0.344896583
}
```

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
      "fix": "separate the two nets' copper: widen the gap or reroute…"
    }],
    "violations": [], "at_limit": []
  }
}
```
