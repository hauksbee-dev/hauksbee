# Examples

Everything you need to run hauksbee and learn from it, in files you can open and
run. This page indexes the [`examples/`](../../examples) tree, the distribution
[`scripts/`](../../scripts), and the captured terminal sessions.

## Get it running

One command builds and installs it. The other two are the usual next steps.

```bash
# Build both binaries and put them on PATH (no sudo; installs into ~/.local/bin)
scripts/install.sh

# See which backends are present and what each unlocks
scripts/doctor.sh

# Run a checked-in CI spec the way a pipeline would
scripts/ci.sh crates/hauksbee-ci/examples/blinky.toml
```

See the [scripts reference](#distribution-scripts) below for the full set.

## First useful result, in a minute

After `scripts/install.sh`, point hauksbee at the bundled blinky board. Each of
these prints one report and exits, so they are the fastest way to see it work:

```bash
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --report   # what each part bound to
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --drc       # copper shorts / clearance
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --lint      # connectivity + strap-pin lint
```

Blinky is a clean board, so those reports come back empty (which is the correct
verdict). To see what a *finding* looks like, run DRC on the bundled `boot_gate`
board, which has a deliberate copper short:

```bash
hauksbee run crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb --drc
# -> a table with two GND/+5V shorts and a "2 short(s)" summary
```

By default these `hauksbee run` reports are informational: they print findings
but exit 0. Add `--strict` to make them FAIL on a real problem (see
[Gate a pipeline](#gate-a-pipeline-with---strict) below). Or gate on
`hauksbee-ci` / `hauksbee check-code` for the full assertion/fault flow.

`hauksbee --help` lists every command. `hauksbee run --help` (or any
`<command> --help`) shows that command's flags with an example. Swap in your own
`.kicad_pcb`, `.kicad_sch`, `.brd`, `.d356`, or a folder of gerbers.

## Plain-language reports (`--plain`)

The expert tables assume you read electronics. Add `--plain` (or its alias
`--explain`) to any of the report flags to get the same findings translated for
someone who is *not* an EE. Each report gives a one-line verdict, then each
finding as **what it is**, **why it matters**, and **what to do**, ordered
worst-first.

```bash
hauksbee run crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb --drc --plain
```

The expert output (default) for that board is a table row:

```
│ short    │ GND  │ +5V  │ B.Cu │ 0.0000 │ 112.0,100.0 │
```

The same finding in `--plain`:

```
2 issues found, 2 serious.

1. [SERIOUS] Two separate connections, "GND" and "+5V", are touching
   (a component pad touches a component pad), near x=112.0 mm, y=100.0 mm on layer B.Cu.
     Why it matters: These are meant to be electrically separate. Where they
       touch they become one connection (a short), so "GND" and "+5V" will be
       forced to the same voltage ... if one is a power rail it can pull large
       current and overheat.
     What to do:     Pull the two pieces of copper apart so there is a clear gap
       between them, or remove the bit of copper that bridges them ...
```

`--plain` works on `--drc`, `--lint`, `--si`, `--resources`, and on the
`--headless` co-sim faults (over-current, over-voltage, etc.). It is opt-in:
the expert tables stay the default. hauksbee derives the plain text from the
*same* finding data, so the two never disagree. A clean board reads
`Looks healthy: no ... problems found.`

## Gate a pipeline (with `--strict`)

By default `--drc`/`--lint`/`--si`/`--resources` exit 0 even when they find
problems, so existing scripts that only read the text are unaffected. Add
`--strict` (alias `--fail-on-findings`) to make them exit non-zero when there is
a real defect. This turns a single command into a CI gate without a full
`hauksbee-ci` spec:

```bash
# Fails (exit 2) only if the board has copper shorts
hauksbee run my_board.kicad_pcb --drc --strict

# Fails if connectivity lint finds a high/medium issue
hauksbee run my_board.kicad_pcb --lint --strict

# Combine with --plain for a human-readable gate
hauksbee run my_board.kicad_pcb --lint --strict --plain
```

What fails the gate under `--strict`:

| Surface | Fails on |
|---|---|
| `--drc` | a true copper short (clearance-only near-shorts do **not** fail) |
| `--lint` / `--resources` | any high- or medium-severity finding (low notes do not) |
| `--si` | any real signal-integrity finding (informational computed values do not) |
| `--headless` | any electrical-stress fault raised during the co-sim |

The non-zero exit code is `2`. Without `--strict` every report still exits `0`.

## See a board live (the 2D/3D viewer)

`hauksbee run <board> --serve` (or the `hauksbee serve` subcommand) serves the
live 2D/3D web viewer. Note: bare `hauksbee run <board>` on a terminal now
opens the full-screen terminal dashboard (TUI) instead. Pass `--serve` for the
web frontend. The frontend is a build artifact and is not checked in, so build it
once:

```bash
cd frontend && bun install && bun run build && cd ..
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --serve
# ...or a real, open-source product board: the SQFMI Watchy (an ESP32-S3
# e-paper smartwatch; hardware MIT-licensed, boards/watchy.LICENSE):
hauksbee run crates/hauksbee-ci/examples/boards/watchy.kicad_pcb --serve
```

The Watchy is the "point it at a real board" case. hauksbee extracts the
circuit its copper implements and binds 56 of 75 parts from the stock model
library. It states plainly which active parts it does not recognise (the
TP4054 charger, the BMA423 IMU, the e-paper panel) instead of guessing. Add
`--report` for the static verdict, or `--serve` for the live 2D/3D viewer.

It prints the URL to open (`http://127.0.0.1:3001` by default). Change the
port with `--port`. If you run it before building the frontend, hauksbee still
starts the websocket and tells you exactly this build step rather than serving
a blank page.

## Drop a board, get a report (the web front door)

For someone who will never touch a terminal, `hauksbee serve` is a "drop your
board, get a report" web page. It needs no board on the command line and no
frontend build (the page is self-contained):

```bash
hauksbee serve            # then open the printed http://127.0.0.1:3001
hauksbee serve --port 8080
```

Open the URL, drop a `.kicad_pcb` / `.kicad_sch` / `.brd` / gerber `.zip` onto
the page (or click to choose one), and the browser shows:

- a one-line overall verdict, colour-coded (green healthy / amber minor / red serious),
- every finding as **what it is / why it matters / what to do**, grouped into
  Copper spacing (DRC), Connectivity & wiring, and Signal integrity,
- a simple 2D map of where the parts sit.

It runs the *same* checks and the *same* plain-language translation as the CLI
(`--drc`/`--lint`/`--si` + `--plain`). Nothing leaves the machine: the
analysis runs locally, inside the `hauksbee serve` process.

This local flow is the whole product for a single user. A **hosted** version
(one URL anyone visits, no install) would also need:

- a small upload service running this same `analyze` behind a real web server,
- per-request sandboxing / resource limits (untrusted board files),
- a size/rate limit beyond the built-in 32 MiB body cap, and
- a privacy stance on uploaded boards (they are proprietary).

The analysis core is already a pure `bytes -> JSON` function, so a hosted
deployment is a packaging job, not new analysis work.

## Board-as-Code examples

[`examples/board-as-code/`](../../examples/board-as-code). Decompile a real
`.kicad_pcb` to editable text, edit it, recompile, re-simulate. Full DSL
reference: [`docs/ingest/BOARD_AS_CODE.md`](../ingest/BOARD_AS_CODE.md).

| Example | What it is |
|---|---|
| [`blinky.board`](../../examples/board-as-code/blinky.board) | The 5-component ATmega328P demo board as DSL: the smallest real example to read. |
| [`stormduino.board`](../../examples/board-as-code/stormduino.board) | A real 51-component corpus board, with repeated hardware factored into `fn` blocks. |
| [`tarski_miswire_repair`](../../crates/hauksbee-engine/examples/tarski_miswire_repair.rs) | **The headline:** repair the Tarski inhibitory-synapse miswire as a one-line code edit, run through simulation. `cargo run --release -p hauksbee-engine --example tarski_miswire_repair`. |

The edit-then-recheck loop and the miswire walkthrough (with expected output)
are in the [board-as-code README](../../examples/board-as-code/README.md).

## hauksbee-ci spec examples

[`examples/ci-specs/`](../../examples/ci-specs) + the canonical specs in
[`crates/hauksbee-ci/examples/`](../../crates/hauksbee-ci/examples). Spec reference:
[`docs/ci/CI.md`](CI.md).

| Spec | Demonstrates | Verdict |
|---|---|---|
| [`tarski_brownout.toml`](../../crates/hauksbee-ci/examples/tarski_brownout.toml) | The flagship brownout regression: a fuzzed power-up bit collapses the rail | **RED** (exit 1) |
| [`tarski_brownout_repaired.toml`](../../crates/hauksbee-ci/examples/tarski_brownout_repaired.toml) | Same board, milliohm-shunt repair applied | **GREEN** |
| [`blinky.toml`](../../crates/hauksbee-ci/examples/blinky.toml) | Rail + UART + blink + no-faults assertions (the template spec) | **GREEN** |
| [`olimex_wifi_burst_transient.toml`](../../examples/ci-specs/olimex_wifi_burst_transient.toml) | Scenario/transient: a `rail_window` assertion riding an ESP32 WiFi burst | **GREEN** |
| [`boot_gate_pass`](../../crates/hauksbee-ci/examples/boot_gate_pass.toml) / [`fail`](../../crates/hauksbee-ci/examples/boot_gate_fail.toml) | `boot-coverage`: does the firmware drive a Hi-Z gate in time? | PASS / FAIL |
| [`watchy_v15_display_res`](../../crates/hauksbee-ci/examples/watchy_v15_display_res.toml) / [`undriven`](../../crates/hauksbee-ci/examples/watchy_v15_display_res_undriven.toml) † | `boot-coverage` on the real Watchy v1.5 e-paper RES# (ESP32 QEMU) | PASS / FAIL |
| [`pic_programmer_schematic.toml`](../../crates/hauksbee-ci/examples/pic_programmer_schematic.toml) † | Schematic-stage CI on a `.kicad_sch` (no PCB yet) | PASS |

† These two specs run against boards in the developer board corpus: the
historical-revision Watchy v1.5 and KiCad's `pic_programmer` demo. This corpus
is not redistributed in this repo, and the Watchy spec also needs the
Espressif QEMU ESP32 backend. Both specs come from the known-fault validation
campaign that calibrated the checks. Their integration tests skip cleanly when
the corpus or backend is absent. To run a real board here with no extra setup,
use `hauksbee run boards/watchy.kicad_pcb --report` above.

More detail and the run-and-expected-verdict for each:
[ci-specs README](../../examples/ci-specs/README.md).

## Terminal sessions (real captured output)

[`examples/sessions/`](../../examples/sessions). Actual runs of the headline
flows, each file labeled with the command that produced it.

| Flow | Transcript |
|---|---|
| Report a board (bind table) | [`01_report_board.txt`](../../examples/sessions/01_report_board.txt) |
| Run DRC | [`02_drc.txt`](../../examples/sessions/02_drc.txt) |
| The lint + SI arsenal (real strap-pin finding) | [`03_lint_si_arsenal.txt`](../../examples/sessions/03_lint_si_arsenal.txt) |
| Boot firmware headless | [`04_boot_firmware_headless.txt`](../../examples/sessions/04_boot_firmware_headless.txt) |
| CI spec GREEN | [`05_ci_spec_green.txt`](../../examples/sessions/05_ci_spec_green.txt) |
| CI spec RED | [`06_ci_spec_red.txt`](../../examples/sessions/06_ci_spec_red.txt) |
| CI spec repaired GREEN | [`07_ci_spec_repaired_green.txt`](../../examples/sessions/07_ci_spec_repaired_green.txt) |
| Transient `rail_window` spec | [`08_ci_spec_transient.txt`](../../examples/sessions/08_ci_spec_transient.txt) |
| Board-as-code loop | [`09_board_as_code_loop.txt`](../../examples/sessions/09_board_as_code_loop.txt) |
| Environment doctor | [`10_doctor.txt`](../../examples/sessions/10_doctor.txt) |
| Boot-coverage PASS / FAIL | [`11_boot_coverage_pass_fail.txt`](../../examples/sessions/11_boot_coverage_pass_fail.txt) |
| Miswire repaired as a code edit | [`12_miswire_repair_demo.txt`](../../examples/sessions/12_miswire_repair_demo.txt) |

Honest notes on raw escapes / stderr artifacts in those captures:
[sessions README](../../examples/sessions/README.md).

## Distribution scripts

[`scripts/`](../../scripts). Every script takes `--help` and is idempotent and
CI-safe (colours auto-disable when not on a TTY or when `NO_COLOR` is set).

| Script | What it does |
|---|---|
| [`install.sh`](../../scripts/install.sh) | Build `hauksbee` + `hauksbee-ci` (release) and install them onto PATH. `--prefix`, `--symlink`, `--no-build`. |
| [`doctor.sh`](../../scripts/doctor.sh) | Report which tools (kicad-cli, simavr, qemu, renode, freerouting) are present and what each unlocks. |
| [`ci.sh`](../../scripts/ci.sh) | Run one or more specs the pleasant-in-CI way: finds/builds the binary, writes a JUnit file per spec, exits non-zero if any spec is RED. |
| [`bundle.sh`](../../scripts/bundle.sh) | Build a versioned binary bundle (the two bins + db + integrations + examples + scripts) as a `.tar.gz` with a checksum. |

## Releases and the GitHub Action

- [`.github/workflows/release.yml`](../../.github/workflows/release.yml) builds the
  binaries on macOS and Linux on a `vX.Y.Z` tag. It attaches the bundles to
  the GitHub Release.
- The composite [GitHub Action](../../integrations/github-action) **prefers a
  prebuilt release binary** and falls back to building from source. Users do
  not have to compile.
- The [KiCad plugin](../../integrations/kicad-plugin) finds a prebuilt or local
  binary automatically and only offers to compile as a last resort.
- The [pre-commit hook](../../integrations/pre-commit) gates commits on
  schematic-stage / layout-stage specs.
