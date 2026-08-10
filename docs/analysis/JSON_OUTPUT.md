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

## Numeric `error_budget`

Numeric evidence and firmware co-simulation results include an `error_budget`
with the solver settings actually used, solved method windows, a measured
residual when supported, failed intervals, and event timestamp precision. A
missing residual or model interval is an explicit unmeasured quantity, not
zero. Values inside `failed_windows` are invalid. See
[Numerical error budgets](ERROR_BUDGETS.md) for units and refusal semantics.

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
| `serious_count` | int | number of `serious` findings (DRC shorts, a destroyed part in co-sim, high-severity lint/SI). A co-sim fault that did not destroy the part grades `warning` and is counted by its surface's gate instead, see `verdict` below |
| `actionable_count` | int | findings a user can act on (serious + warnings + clearance groups) |

`verdict` is `"fail"` when `serious_count > 0`, or when the emitting surface's
own `--strict` gate fails on a finding the `serious` severity does not carry
(`--lint` gates on medium-severity findings, `--si` on any real finding
including its low ones but not its informational notes, the co-sim surface on
any raised fault; `--strict-boot` adds the boot advisory). It is `"invalid"`
when nothing gates but the run-level claim could not be judged: a
top-level `refusal`, AC or thermal reporting `valid:false`, undermined run-level
evidence, or unbound verdict-critical parts on a model-dependent surface.
Otherwise it is `"pass"`. Precedence is `fail` > `invalid` > `pass`, and a
gating run's own document matches its own exit code (2 / 3 / 0). That is not
the same as predicting a strict run from a non-strict one: on the co-sim path
the zero-activity refusal is only constructed under `--strict`, and the boot
advisory is only gate-grade under `--strict-boot`, so both documents change
with the flag. DRC shorts are excluded from `serious_count` when the board is
newer than the validated copper extraction (a `drc.version_warning` is set).
This is the same carve-out the exit gate makes.

When current-carrying / active parts have no model (an open power FET, an
unresolved main IC), the lint/SI/check surfaces add a `notes` entry (kind
`coverage`) whose message begins `INCONCLUSIVE:`, naming the count, the parts,
and the unlocking input. The same parts are in `bind.active_path_unresolved[]`
(an unresolved power FET appears there with `active_ic: false`, so do not
filter on `active_ic` when looking for them) or
`bind.resolved_but_open_active[]`, and the evidence spine carries the typed
`open_part` assumptions on affected claims. On the model-dependent surfaces
(`--lint`, `--si`, `--check`, `--resources`, the bare machine report, the
CC-scoped `--usb-c` claim and the co-sim report) those parts also make
`verdict` read `invalid`, and a `--strict` run of the same command exits 3 to
match: a clean result there would be vacuous. The copper (`--drc`) and
descriptive (`--report`) surfaces are exempt, on both the verdict and the exit
code: copper reads the layout, and `--report` describes the binding rather than
judging it (`docs/ci/CI.md` states that boundary).

Read `verdict` together with `notes` and the `bind` fields, never on its own:
`thermal.valid: true` under `--thermal --no-strict-thermal` means the table is
usable, not that the coverage is complete, and the top-level `verdict` can
still be `invalid` for the undermined evidence behind it while the opt-out
returns exit 0. That opt-out is one of three places the exit code and the
verdict deliberately part company, the same way omitting `--strict` leaves a
`fail` verdict at exit 0. The other two are on the co-sim path: an aborted
analog solve exits 3 (invalid for analysis) even where the document grades the
faults it observed as `fail`, because those faults may come from the
stale-voltage windows the solve failed on; and a runtime timing refusal exits
3 beside whatever verdict the document already printed. In all three the
document is the record of what was observed and the exit code is the policy;
where the gate is armed and the run is analysable they agree.

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

- `timing_coverage[]`, per live MCU `{mcu_ref, backend, cycle_exact,
  timestamp_precision_s, minimum_guaranteed_pulse_s, chunk_s}` measured at the
  actual chunk. Push callbacks report one-cycle precision; poll backends report
  their real slice, never an inferred silicon limit.
- `timing_refusals[]`, any runtime edge/PWL limit reached; under `--strict` a
  non-empty list exits INVALID rather than trusting a collapsed waveform.
- `substituted`: the firmware ran on a substitute core (also in `notes`).
- `adc_dropped[]`, ADC channels whose modelled voltage the platform could not
  inject (the firmware read nothing on that pin).
- `unexercised_buses[]`, bound I2C/SPI peripherals the platform's controller set
  never exercised (a `peripheral` assertion against one fails, not green-passes).
- `spi_framing[]`, per-bus framing tier: a `"heuristic"` tier means transaction
  boundaries were guessed at chunk edges. On an `"exact"` tier, `cs_provenance`
  says where the chip-select came from: `"spec"` (the peripheral's `cs_net`),
  `"model-roles"` (the `cs` pin role of the model bound to the board component
  the peripheral's `ref` names), or `"bitbang-pins"` (a bit-banged slave, whose
  CS pin comes from its GPIO wiring rather than a net lookup). All three are the
  same electrical fact and earn the same tier, but they fail differently, so the
  field is there for a consumer reproducing or overriding the result. Absent on
  the `backend` and `heuristic` tiers, where none was resolved.
- `short_pulses[]`, firmware GPIO pulses that rose and fell inside one solver
  chunk on a net clocking a TICK-evaluated sequential part, so that part never
  observed the pulse. Entries carry `{net, mcu_ref, pin, pulse_s, chunk_s,
  parts[]}`: the narrowest completed pulse, the chunk it fell inside, and the
  parts that missed it.
- `driver_contention[]`, nets where the firmware configured an MCU pin as a
  push-pull output while an enabled modelled push-pull output was already
  driving. Entries carry `{net, mcu_ref, pin, parts[], t_s}`; waveforms touching
  the net are untrustworthy from `t_s` on.
- `watchdog_limitations[]`, MCUs whose backend watchdog does not bite the way
  the part's does. Entries carry `{mcu_ref, limitation}`, where `limitation` is a
  whole sentence for a human: firmware that HANGS runs forever here, so every
  assertion about behaviour after a hang is fiction and the run cannot vouch for
  the recovery path.
- `watchdog_resets[]`, MCUs whose watchdog actually rebooted the core, as
  `{mcu_ref, resets}`. Not an error, a finding: an assertion that passed across a
  reboot was measuring a rebooted core. Read it with `watchdog_limitations[]`,
  since a backend that cannot reboot at all reports nothing here.
- `timing_limitations[]`, MCUs whose backend's simulated time carries a known
  systematic bias (the wall-clock-paced `qemu:` family, the STM32F103's
  deliberate TIMx-at-72MHz divergence). Entries carry `{mcu_ref, limitation}`,
  a whole sentence for a human: time-based assertions on these cores mean less
  than they look, and this array is where the run says so.

Every array above (`activity_summary[]`, `timing_coverage[]`,
`timing_refusals[]`, `failed_windows[]`, `spi_framing[]`,
`adc_dropped[]`, `unexercised_buses[]`, `short_pulses[]`,
`driver_contention[]`, `watchdog_limitations[]`, `watchdog_resets[]`,
`timing_limitations[]`) is omitted when empty; the scalars are always present.
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
