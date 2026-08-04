# `hauksbee-ci run --json`: the machine-readable contract

`hauksbee-ci run <spec>... --json` prints one JSON object per spec on stdout,
one per line (NDJSON). That stream is a contract: pipelines parse it, the web
checks panel renders it, and the MCP `run_checks` tool returns it. This document
is what you can rely on, and what may change under you.

The shape is published as a JSON Schema at
`crates/hauksbee-ci/schemas/hauksbee-ci-report.schema.json` (draft-07, the same
dialect as the spec-input schema and the engine's run report). The file is
GENERATED from the Rust types that serialize the document, and
`crates/hauksbee-ci/tests/ci_report_schema_drift.rs` fails the build when the
two disagree, so a field cannot ship without appearing in the contract.

For the static single-board surface (`hauksbee run <board> --json`, no
assertions) see [../analysis/JSON_OUTPUT.md](../analysis/JSON_OUTPUT.md). The
two documents are separate shapes with separate schema versions.

## Two line kinds, told apart by `ok`

Branch on `ok` before reading anything else.

| `ok` | meaning | keys |
|---|---|---|
| `true` | the spec ran | the full report below |
| `false` | the spec never ran (unreadable spec, desynced net or component reference, missing board or firmware) | `ok`, `error` |

An error line contributes exit 2 and carries the same sentence stderr prints. A
multi-spec invocation keeps going after one, so a stream may mix both kinds:

```
{"schema_version":1,"ok":true,"spec_name":"power resistor within thermal limits", ...}
{"schema_version":1,"ok":true,"spec_name":"power resistor over thermal limit (hot ambient)", ...}
{"ok":false,"error":"no spec file at 'ci/typo.toml'. Check the path, or try a bundled example:\n  hauksbee-ci run crates/hauksbee-ci/examples/blinky.toml"}
```

One case prints no line at all: a usage error that happens before any spec is
resolved (no spec argument, `--example` naming an example that does not exist)
exits 2 with the message on stderr and an empty stdout. A consumer that reads
stdout must treat "no lines" as a possible outcome of a nonzero exit.

## The report line

Every key below is ALWAYS present on an `ok: true` line.

| key | type | always | meaning |
|---|---|---|---|
| `schema_version` | integer | yes | the shape version, currently `1` |
| `ok` | `true` | yes | the discriminator |
| `spec_name` | string | yes | the spec's `name`, or its file stem |
| `board` | string | yes | the board path as the spec wrote it |
| `passed` | boolean | yes | the OVERALL verdict: `exit_code == 0` |
| `assertions_passed` | boolean | yes | did every assertion pass or carry an active waiver |
| `run_valid` | boolean | yes | was the run trustworthy at all |
| `exit_code` | integer | yes | this spec's contribution: 0, 1, or 3 |
| `analog_abort` | boolean | yes | did the analog co-sim trip the consecutive-failed-chunk abort |
| `seeds` | integer | yes | ensemble members run (fuzz seeds, or tolerance members) |
| `elapsed_s` | number | yes | wall-clock seconds |
| `coverage` | string or `null` | yes, nullable | the tolerance-ensemble coverage claim; `null` when the run was not an ensemble |
| `substitutions` | array of string | yes, may be empty | MCUs co-simulated on a substitute core |
| `coverage_warnings` | array of string | yes, may be empty | co-sim coverage holes (dropped ADC injection, unexercised bus device, a watchdog that cannot bite, a watchdog that did) |
| `dead_rails` | array of string | yes, may be empty | nets that name a supply and that nothing powered |
| `waiver_notes` | array of string | yes, may be empty | lapsed waivers, active waivers that matched nothing, a malformed waiver file |
| `results` | array of object | yes | one entry per assertion, in spec order |

`coverage` is the one nullable key: present with the value `null`, not absent.
The distinction matters, because the two per-assertion optional keys below are
absent instead.

### Reading the verdict

`passed` is the process verdict and already folds in trustworthiness, so it is
the field to gate on. The other two exist because they answer different
questions and can disagree:

- `assertions_passed: true` with `run_valid: false` means nothing failed and
  nothing can be believed. The analog side collapsed; the assertions were not
  honestly evaluable. Treat it as a refusal, never as green.
- `assertions_passed: false` with `run_valid: true` is a trustworthy red: a real
  finding about the board.

A green run can still carry qualifiers. `substitutions`, `coverage_warnings`,
and `dead_rails` non-empty each mean the verdict covers less than it looks like
it does, and they are the reason a machine consumer should surface them rather
than reduce the document to one boolean. `waiver_notes` non-empty means the
board's waiver file needs attention.

### `results[]`, one entry per assertion

An `hwtrace` assertion expands to one entry per channel and feature, so
`results` can be longer than the spec's `[[assert]]` list.

| key | type | always | meaning |
|---|---|---|---|
| `label` | string | yes | the assertion's label |
| `kind` | string | yes | the spec's `kind` token (`voltage`, `uart`, `blink`, `no_faults`, `max_temp`, `hwtrace`, ...) |
| `passed` | boolean | yes | did it hold on every member |
| `invalid` | boolean | yes | could it not be honestly evaluated at all |
| `detail` | string | yes | the one-line measurement |
| `failing_seed` | integer or `null` | yes, nullable | first failing member, `null` on a pass |
| `failing_seeds` | array of integer | yes, may be empty | every failing member |
| `seeds_total` | integer | yes | members evaluated |
| `why` | string | **no, absent** | on a real red: the observed shortfall in one sentence |
| `waived` | string | **no, absent** | the reason and expiry of the active waiver covering this failure |

`why` and `waived` are the only two keys in the whole document that a consumer
may find MISSING. They are omitted, never `null`, so test for presence rather
than for null. A waived failure keeps `passed: false` here while lifting
`assertions_passed` to `true`: waived is visible, not hidden, and not gating.

Every outcome a `results` entry can express:

| `passed` | `invalid` | `waived` | outcome |
|---|---|---|---|
| `true` | `false` | absent | pass |
| `false` | `false` | absent | fail, gating |
| `false` | `false` | present | fail, waived, not gating |
| `false` | `true` | absent | INVALID, cannot be waived, forces exit 3 |

`invalid: true` with `passed: true` never occurs.

## Worked shapes

A green run, trimmed to the interesting keys:

```json
{"schema_version":1,"ok":true,"spec_name":"power resistor within thermal limits",
 "board":"boards/power_resistor.kicad_pcb","passed":true,"assertions_passed":true,
 "run_valid":true,"exit_code":0,"analog_abort":false,"seeds":1,"elapsed_s":0.196,
 "coverage":null,"substitutions":[],"coverage_warnings":[],"dead_rails":[],
 "waiver_notes":[],
 "results":[{"label":"Tj(R1) <= 125 C","kind":"max_temp","passed":true,
   "invalid":false,"detail":"Tj(R1) peak 91.7C (<= 125C)","failing_seed":null,
   "failing_seeds":[],"seeds_total":1}]}
```

A red run adds `why` and a `failing_seed`:

```json
{"passed":false,"assertions_passed":false,"run_valid":true,"exit_code":1,
 "results":[{"label":"Tj(R1) <= 125 C","kind":"max_temp","passed":false,
   "invalid":false,"detail":"Tj(R1) peak 156.7C > ceiling 125C <- FAILED HERE",
   "failing_seed":0,"failing_seeds":[0],"seeds_total":1,
   "why":"R1 ran 31.7 C hotter than your 125 C ceiling (peak 156.7 C)"}]}
```

The same run with a waiver beside the board: the assertion is still a failure,
the run is green, and the waiver reason travels with it.

```json
{"passed":true,"assertions_passed":true,"run_valid":true,"exit_code":0,
 "results":[{"label":"Tj(R1) <= 125 C","kind":"max_temp","passed":false,
   "invalid":false,"detail":"Tj(R1) peak 156.7C > ceiling 125C <- FAILED HERE",
   "failing_seed":0,"failing_seeds":[0],"seeds_total":1,
   "why":"R1 ran 31.7 C hotter than your 125 C ceiling (peak 156.7 C)",
   "waived":"bench-verified at 40 C ambient; the thermal model has no airflow term (until 2027-12-31)"}]}
```

A tolerance ensemble fills `coverage` and can fail on several members:

```json
{"passed":false,"assertions_passed":false,"run_valid":true,"exit_code":1,"seeds":4,
 "coverage":"tolerance corners: 4 deterministic min/max corner(s) over 2 component(s): bounds the worst case only where the response is monotonic in each value",
 "results":[{"label":"VOUT within the tight [2.4, 2.6] V window on every corner",
   "kind":"voltage","passed":false,"invalid":false,
   "detail":"corner 1: VOUT: min=2.250V < required 2.4V <- FAILED HERE, ...",
   "failing_seed":1,"failing_seeds":[1,2],"seeds_total":4,
   "why":"VOUT settled 0.150 V below your floor (2.250 V vs min 2.4 V)"}]}
```

A refusal: `run_valid: false`, `exit_code: 3`, and the affected assertion is
neither pass nor fail.

```json
{"passed":false,"assertions_passed":false,"run_valid":false,"exit_code":3,
 "analog_abort":false,
 "results":[{"label":"VOUT >= 3.0 V","kind":"voltage","passed":false,"invalid":true,
   "detail":"INVALID: the evaluation window overlaps a failed analog span",
   "failing_seed":null,"failing_seeds":[],"seeds_total":1}]}
```

`analog_abort: true` with an empty `results` is the other route to exit 3: the
co-sim aborted and no single assertion happened to cover the failed span.

## Compatibility policy

`schema_version` is `1`. It answers one question: has the meaning of this
document changed. It is not a build number and it does not move when fields are
added.

**Additive, and safe. These will happen without a version bump.**

- A new top-level key on the report line.
- A new key inside a `results` entry.
- A new `kind` token, or new wording inside `detail`, `why`, `coverage`, or any
  of the qualifier arrays. All of these are prose for a human, computed per
  assertion kind and per finding. Do not parse them, and do not match on them
  exactly.
- A new element in any of `substitutions`, `coverage_warnings`, `dead_rails`,
  `waiver_notes`, because a new honesty qualifier can start firing on a board
  that did not previously produce one.

Write consumers that ignore unknown keys. The published schema deliberately
leaves `additionalProperties` open on both line kinds so that a document from a
newer `hauksbee-ci` still validates against an older copy of the schema, and
`ci_report_schema_drift.rs` asserts it stays open.

**Breaking, and will bump `schema_version`.**

- Removing or renaming any key in the tables above.
- Changing a key's type, including making a currently-always-present key
  absent, or an always-present-and-nullable key non-nullable.
- Changing what a key MEANS while keeping its name and type. The one already
  scheduled is the invalid-for-analysis boundary: when the engine's
  Undermined verdict starts yielding invalid-for-analysis, a run that reports
  `run_valid: true` today will report `false`, and that is a semantic change
  even though no field moves.

**Guaranteed regardless of version.**

- `ok` is present on every line and is the discriminator.
- `schema_version` is present on every report line, so a consumer can refuse a
  version it does not understand rather than misread it.
- `passed` equals `exit_code == 0`.
- Exit codes keep the meanings in
  [CI.md](CI.md#exit-codes-the-pipeline-contract).

**Where the next bump goes.** The evidence work adds per-assertion evidence,
a component inventory, and an assumption registry to this document. Those are
additive keys and do not bump anything on their own; the semantic change above
does, once, to `schema_version = 2`. The constant lives in
`crates/hauksbee-ci/src/report.rs` as `CI_REPORT_SCHEMA_VERSION`, the
generated file's description quotes it, and the drift test fails until the
schema is regenerated with it, so the bump cannot land half-applied.

## Validating the stream yourself

The schema is a plain draft-07 file, so any validator works:

```bash
hauksbee-ci run ci/*.toml --json > lines.ndjson
python3 - lines.ndjson <<'PY'
import json, sys, jsonschema
schema = json.load(open("crates/hauksbee-ci/schemas/hauksbee-ci-report.schema.json"))
for n, line in enumerate(open(sys.argv[1]), 1):
    jsonschema.validate(json.loads(line), schema)
    print(f"line {n}: valid")
PY
```

Gate on the exit code, not on the JSON. The document tells you WHAT happened;
the exit code is the contract for whether the build should go red, and it
already folds in the waiver and trustworthiness rules.
