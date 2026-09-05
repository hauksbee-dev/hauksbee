# `hauksbee-ci run --json`: the machine-readable contract

`hauksbee-ci run <spec>... --json` prints one JSON object per spec on stdout,
one per line (NDJSON). The schema is
`crates/hauksbee-ci/schemas/hauksbee-ci-report.schema.json` (draft-07),
generated from the Rust types and drift-tested
(`crates/hauksbee-ci/tests/ci_report_schema_drift.rs`).

The single-board surface `hauksbee run <board> --json` is a separate shape
with its own schema version: [../analysis/JSON_OUTPUT.md](../analysis/JSON_OUTPUT.md).

## Two line kinds

Branch on `ok` first.

| `ok` | meaning | keys |
|---|---|---|
| `true` | the spec ran | the report line below |
| `false` | the spec never ran (unreadable spec, unknown net/ref, missing board or firmware) | `ok`, `error` |

An error line contributes exit 2; a multi-spec invocation keeps going. A usage
error before any spec is resolved (no spec argument, unknown `--example`)
prints nothing and exits 2.

## The report line

Every key is always present except `refusal` (exit 3 only).

| key | type | meaning |
|---|---|---|
| `schema_version` | integer | currently `2` |
| `ok` | `true` | discriminator |
| `spec_name` | string | the spec's `name`, or its file stem |
| `board` | string | the board path as written in the spec |
| `passed` | bool | the overall verdict: `exit_code == 0` |
| `assertions_passed` | bool | every assertion passed or carried an active waiver |
| `run_valid` | bool | the run was trustworthy |
| `exit_code` | integer | this spec's contribution: 0, 1 or 3 (never 2) |
| `analog_abort` | bool | the analog co-sim tripped the consecutive-failed-chunk abort |
| `refusal` | object | exit 3 only: `claim`, `missing_prerequisite`, `valid_partial_conclusions[]`, `next_action` |
| `seeds` | integer | ensemble members run |
| `elapsed_s` | number | wall-clock seconds |
| `coverage` | string or `null` | tolerance-ensemble coverage claim; `null` when not an ensemble |
| `substitutions` | string[] | MCUs co-simulated on a substitute core |
| `coverage_warnings` | string[] | co-sim coverage holes (dropped ADC injection, unexercised bus, watchdog or timing limitation) |
| `timing_coverage` | object[] | per MCU: `timestamp_precision_s`, `minimum_guaranteed_pulse_s`, `chunk_s`, backend, cycle-exact flag |
| `timing_refusals` | string[] | unrepresentable timing claims; any entry makes the run INVALID |
| `dead_rails` | string[] | nets that name a supply but nothing powered |
| `waiver_notes` | string[] | lapsed waivers, waivers that matched nothing, a malformed waiver file |
| `inventory` | object[] | input files consumed (board, spec, firmware, BOM, placement, variant, waivers, models) with content hashes |
| `assumptions` | object[] | the typed assumption registry |
| `evidence` | object[] | one causal evidence map per evaluated assertion |
| `results` | object[] | one entry per assertion, in spec order |

`exit_code` is the spec's verdict; the process exits with the worst code of
the invocation, escalated to 2 if a requested `--junit` file could not be
written. Gate on the process code; read the field to attribute a verdict.

Reading the verdict: `passed` folds in trustworthiness and is the field to
gate on. `assertions_passed: true` with `run_valid: false` is a refusal, never
green; read `refusal`. Non-empty `substitutions`, `coverage_warnings` or
`dead_rails` mean the verdict covers less than it looks.

### `results[]`

An `hwtrace` assertion expands to one entry per channel and feature.

| key | type | meaning |
|---|---|---|
| `label` | string | the assertion's label |
| `kind` | string | the spec's `kind` token |
| `passed` | bool | held on every member |
| `invalid` | bool | could not be evaluated |
| `detail` | string | the one-line measurement |
| `failing_seed` | integer or `null` | first failing member |
| `failing_seeds` | integer[] | every failing member |
| `seeds_total` | integer | members evaluated |
| `why` | string, **absent unless red** | the observed shortfall in one sentence |
| `waived` | string, **absent unless waived** | reason and expiry of the covering waiver |
| `evidence` | object, **absent when none** | the assertion's causal evidence map |

`why`, `waived` and `evidence` are omitted, never `null`. A waived failure
keeps `passed: false` while `assertions_passed` is `true`.

| `passed` | `invalid` | `waived` | outcome |
|---|---|---|---|
| true | false | absent | pass |
| false | false | absent | fail, gating |
| false | false | present | fail, waived, not gating |
| false | true | absent | INVALID; cannot be waived; exit 3 |

## Examples

```json
{"schema_version":2,"ok":true,"spec_name":"power resistor within thermal limits",
 "board":"boards/power_resistor.kicad_pcb","passed":true,"assertions_passed":true,
 "run_valid":true,"exit_code":0,"analog_abort":false,"seeds":1,"elapsed_s":0.196,
 "coverage":null,"substitutions":[],"coverage_warnings":[],"dead_rails":[],"waiver_notes":[],
 "results":[{"label":"Tj(R1) <= 125 C","kind":"max_temp","passed":true,"invalid":false,
   "detail":"Tj(R1) peak 91.7C (<= 125C)","failing_seed":null,"failing_seeds":[],"seeds_total":1}]}
```

A red run adds `"why"` and a `failing_seed`; a waived one adds `"waived"` and
flips `passed`/`exit_code` green at the top level. A refusal has
`run_valid:false`, `exit_code:3`, and `invalid:true` on the affected result.
`analog_abort:true` with empty `results` is the other route to exit 3.

## Compatibility

`schema_version` bumps only when meaning changes. Additive without a bump: a
new top-level key, a new `results` key, a new `kind` token, new wording in
`detail`/`why`/`coverage`, a new element in the qualifier arrays. Breaking:
removing or renaming a key, changing a type or nullability, changing meaning.
Consumers must ignore unknown keys; the schema leaves `additionalProperties`
open. Always true: `ok` on every line, `schema_version` on every report line,
`passed == (exit_code == 0)`, exit codes as in [CI.md](CI.md#exit-codes).

Validate with any draft-07 validator:

```bash
hauksbee-ci run ci/*.toml --json > lines.ndjson
python3 -c 'import json,sys,jsonschema; s=json.load(open("crates/hauksbee-ci/schemas/hauksbee-ci-report.schema.json")); [jsonschema.validate(json.loads(l), s) for l in open("lines.ndjson")]'
```
