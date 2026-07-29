# Hauksbee for agents

Hauksbee is CI for hardware. Hand it a PCB design file. Hauksbee
reconstructs the circuit from the copper. It simulates the circuit with
real device physics, co-simulates firmware on an emulated MCU, and checks
the result like a test suite. Every surface an agent needs is
machine-readable. Nothing requires the browser UI.

## The three commands

```bash
hauksbee run <board> --json            # full analysis, one JSON object on stdout
hauksbee-ci init <board>               # scaffold a check spec from the board (prints its path)
hauksbee-ci run <spec.toml> --json     # run the spec's assertions, JSON verdict
```

`<board>` is any supported input: KiCad `.kicad_pcb` / `.kicad_sch`, Eagle
`.brd`, Altium `.PcbDoc`, IPC-D-356 `.d356`, a gerber folder or zip, or
Board-as-Code `.board` (bare or zipped). Firmware for co-sim
(`--firmware <f>` on `run`, `firmware =` in a spec) is a compiled
`.elf`/`.hex`, a PlatformIO project directory, or a zip of either. Hauksbee
finds the built image inside automatically, or builds it with your `pio`.

## Exit codes (both binaries)

| Code | Meaning |
|---|---|
| 0 | Green: all checks/assertions held. |
| 1 | Red: an assertion failed (the JSON says which and on which seed). |
| 2 | Input error: bad spec, missing board, unreadable file. |
| 3 | Invalid for analysis: the analog solve aborted, results are not trustworthy. Never treat as green OR as an ordinary failure. |

Trust the JSON `passed` field only together with `run_valid`. `hauksbee-ci
run --json` reports `passed` (the process verdict, false on exit 3),
`assertions_passed`, and `run_valid` as separate fields.

## JSON output shapes

- `hauksbee run <board> --json`: top-level verdict plus per-section findings.
  Schema: `docs/analysis/JSON_OUTPUT.md`.
- `hauksbee-ci run <spec> --json`: `{ok, passed, assertions_passed, run_valid,
  exit_code, analog_abort, coverage, substitutions, coverage_warnings,
  results[]}` where each result is `{label, kind, passed, invalid, detail,
  failing_seed, failing_seeds, seeds_total}`.
- Honesty qualifiers are data, not prose. Substitute MCU cores, dropped ADC
  injections, and coverage holes appear in `substitutions` /
  `coverage_warnings`. Surface them to the user. A green run with a
  substitution is not the same claim as a green run on real silicon.

## The spec is the contract

A `hauksbee-ci` spec is one TOML file: the board, optional firmware, power
supplies, and assertions (`voltage`, `uart`, `toggle`, `no_faults`,
`max_current`, `max_temp`, `boot-coverage`, `rail_window`, tolerance
ensembles, transient scenarios). Full format: `docs/ci/CI.md`. Recommended
agent loop:

```bash
hauksbee-ci init board.kicad_pcb        # detects supplies, MCU, rail nets
# edit the scaffolded [[assert]] blocks
hauksbee-ci run board.toml --json       # iterate until exit 0
hauksbee-ci run board.toml --junit out.xml   # for CI ingestion
```

`--seed N` re-runs one failing fuzz/tolerance member in isolation,
reproducing the full run's values exactly. Under `GITHUB_ACTIONS` both
binaries emit `::error` annotations.

## Other machine surfaces

- `hauksbee run <board> --report|--drc|--lint|--si|--resources|--usb-c
  --json` runs a single report. `--strict` makes findings fail the exit code.
- `--headless --firmware f.elf --seconds N` runs the co-sim and prints
  summary stats. `--probe NET --probe-csv out.csv` captures waveforms.
- `hauksbee serve` exposes the same engine over HTTP on localhost:
  `POST /api/analyze` (raw board bytes, `X-Board-Filename` header),
  `POST /api/analyze-with-firmware` (multipart `board` + `firmware`),
  `POST /api/check` (multipart `board` + `firmware` + `spec`, where `spec` is
  the TOML body without `board`/`firmware` keys). All return JSON, always
  HTTP 200 with `{ok:false,error}` on input problems. Hauksbee refuses
  browser-origin cross-site requests. Non-browser clients are unaffected.
- SPICE subset: `hauksbee sim deck.cir` with the supported/refused card
  contract in `docs/spice-compat/compatibility.md`. Refusals are loud and
  line-numbered. Never retry a refused card. Change the deck instead.

## The MCP server

`hauksbee-mcp` is a stdio MCP server: JSON-RPC 2.0, newline-delimited,
protocol revision 2025-06-18. It also accepts 2025-03-26 and 2024-11-05.
Launch it as a subprocess, and speak MCP over its stdin/stdout:

```json
{ "command": "hauksbee-mcp" }
```

It declares only the `tools` capability. Five tools:

| tool | arguments | returns |
|---|---|---|
| `analyze_board` | `board_path`, `firmware_path?` | the front-door report JSON (headline, `serious`/`total`, per-section findings, `bind` coverage, `nets`, `supplies`, `notes`, and `cosim` when firmware ran) |
| `run_checks` | `board_path`, `spec_toml`, `firmware_path?` | the `hauksbee-ci --json` verdict: `{passed, assertions_passed, run_valid, exit_code, analog_abort, seeds, coverage, substitutions, coverage_warnings, results[]}` |
| `list_capabilities` | none | the scope table as data: report kinds, assertion kinds, board/firmware formats, and MCU backend availability probed on this machine with the engine's own discovery (doctor-style `builtin/ok/absent/disabled`) |
| `board_to_code` | `board_path` | `{board, code}`: the editable Board-as-Code text form (text formats only) |
| `run_script` | `source` | `{result, logs}`: code mode, below |

`spec_toml` is the spec body WITHOUT `board`/`firmware` keys. The server
injects them from the path arguments. Every result arrives both as a
`content` text block and as `structuredContent`, the same object.

### The refusal shape

This is the exit-3 doctrine, expressed as data. When a run cannot be vouched
for (firmware on a board with no runnable MCU, an aborted analog solve, an
assertion window over a failed span), the tool result is:

```json
{ "status": "invalid_for_analysis", "reason": "...", "report": { ... } }
```

(`run_checks` attaches the per-assertion data under `"result"` instead of
`"report"`.) It is a successful tool call (`isError: false`): a refusal is an
answer, not a malfunction. Never read it as pass or fail. Never average it
away. Never retry expecting a different outcome. Genuine input errors
(missing file, bad TOML) come back with `isError: true` and an `error`
message instead. Coverage holes on answerable runs stay data fields
(`substitutions`, `coverage_warnings`, `cosim.adc_dropped`, ...), same as
the CLI JSON.

### Code mode (`run_script`)

Instead of many tool round-trips, submit one JavaScript program that runs
server-side in an embedded QuickJS sandbox and returns a composed result.
The sandbox's only capability is the global `hauksbee` object:

- `hauksbee.analyzeBoard(path, firmwarePath?)`
- `hauksbee.runChecks(path, specToml, firmwarePath?)`
- `hauksbee.listCapabilities()`
- `hauksbee.boardToCode(path)`

Each returns the same object the corresponding tool returns. The sandbox
captures `console.log` into `logs`. No filesystem, network, timers, or
imports exist. The script runs as a function body: `return` its
(JSON-serializable) result. The sandbox THROWS refusals as the structured
refusal object (`e.status === "invalid_for_analysis"`), so a script cannot
mistake one for data. Catch it to branch on it. Tool input errors throw
`{error}`. The sandbox kills scripts after 120 seconds.

### Worked example

Request:

```json
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{
  "name":"run_script",
  "arguments":{"source":"const r = hauksbee.analyzeBoard('boards/button_pullup.kicad_pcb');\nconsole.log('findings:', r.total);\nlet checks = null;\nif (r.serious === 0) {\n  checks = hauksbee.runChecks('boards/button_pullup.kicad_pcb',\n    'duration_ms = 10\\n[[assert]]\\nkind = \"no_faults\"\\n');\n}\nreturn {board: r.file_name, serious: r.serious, checksPassed: checks && checks.passed};"}}}
```

Response:

```json
{"jsonrpc":"2.0","id":7,"result":{
  "content":[{"type":"text","text":"{\"result\":{\"board\":\"button_pullup.kicad_pcb\",\"serious\":0,\"checksPassed\":true},\"logs\":[\"findings: 0\"]}"}],
  "structuredContent":{"result":{"board":"button_pullup.kicad_pcb","serious":0,"checksPassed":true},"logs":["findings: 0"]},
  "isError":false}}
```

## Ground rules for agents

- Reports exit 0 by default even with findings. Gate on `--strict` or a spec.
- Exit 3 means the run refused to vouch for itself. Do not average it away.
- Treat a finding on a known-good board as a hauksbee bug, not noise. False
  positives are the failure mode this project optimizes against.
- The plain-language rendering (`--plain`) and the JSON carry the same
  facts. Parse the JSON, and show humans the plain text.
