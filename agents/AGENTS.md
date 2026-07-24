# Hauksbee for agents

Hauksbee is CI for hardware: hand it a PCB design file and it reconstructs the
circuit from the copper, simulates it with real device physics, co-simulates
firmware on an emulated MCU, and checks the result like a test suite. Every
surface an agent needs is machine-readable; nothing requires the browser UI.

## The three commands

```bash
hauksbee run <board> --json            # full analysis, one JSON object on stdout
hauksbee-ci init <board>               # scaffold a check spec from the board (prints its path)
hauksbee-ci run <spec.toml> --json     # run the spec's assertions, JSON verdict
```

`<board>` is any supported input: KiCad `.kicad_pcb` / `.kicad_sch`, Eagle
`.brd`, Altium `.PcbDoc`, IPC-D-356 `.d356`, a gerber folder or zip, or
Board-as-Code `.board` (bare or zipped). Firmware for co-sim
(`--firmware <f>` on `run`, `firmware =` in a spec) is a compiled `.elf`/`.hex`,
a PlatformIO project directory, or a zip of either; the built image inside is
found (or built with your `pio`) automatically.

## Exit codes (both binaries)

| Code | Meaning |
|---|---|
| 0 | Green: all checks/assertions held. |
| 1 | Red: an assertion failed (the JSON says which and on which seed). |
| 2 | Input error: bad spec, missing board, unreadable file. |
| 3 | Invalid for analysis: the analog solve aborted, results are not trustworthy. Never treat as green OR as an ordinary failure. |

Trust the JSON's `passed` field only alongside `run_valid`: `hauksbee-ci run
--json` reports `passed` (the process verdict, false on exit 3),
`assertions_passed`, and `run_valid` separately.

## JSON output shapes

- `hauksbee run <board> --json`: top-level verdict plus per-section findings.
  Schema: `docs/analysis/JSON_OUTPUT.md`.
- `hauksbee-ci run <spec> --json`: `{ok, passed, assertions_passed, run_valid,
  exit_code, analog_abort, coverage, substitutions, coverage_warnings,
  results[]}` where each result is `{label, kind, passed, invalid, detail,
  failing_seed, failing_seeds, seeds_total}`.
- Honesty qualifiers are data, not prose: substitute MCU cores, dropped ADC
  injections, and coverage holes appear in `substitutions` /
  `coverage_warnings`. Surface them; a green run with a substitution is not
  the same claim as a green run on the real silicon.

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
  --json` for single-report runs; `--strict` makes findings fail the exit code.
- `--headless --firmware f.elf --seconds N` runs the co-sim and prints
  summary stats; `--probe NET --probe-csv out.csv` captures waveforms.
- `hauksbee serve` exposes the same engine over HTTP on localhost:
  `POST /api/analyze` (raw board bytes, `X-Board-Filename` header),
  `POST /api/analyze-with-firmware` (multipart `board` + `firmware`),
  `POST /api/check` (multipart `board` + `firmware` + `spec`, where `spec` is
  the TOML body without `board`/`firmware` keys). All return JSON, always
  HTTP 200 with `{ok:false,error}` on input problems. Browser-origin
  cross-site requests are refused; non-browser clients are unaffected.
- SPICE subset: `hauksbee sim deck.cir` with the supported/refused card
  contract in `docs/spice-compat/compatibility.md`. Refusals are loud and
  line-numbered; never retry a refused card, change the deck.

## Ground rules for agents

- Reports exit 0 by default even with findings; gate on `--strict` or a spec.
- Exit 3 means the run refused to vouch for itself. Do not average it away.
- A finding on a known-good board is treated as a hauksbee bug, not noise;
  false positives are the failure mode this project optimizes against.
- The plain-language rendering (`--plain`) and the JSON carry the same facts;
  parse the JSON, show humans the plain text.
