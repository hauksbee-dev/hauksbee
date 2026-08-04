# Examples

Everything you need to run hauksbee and learn from it, in files you can open and
run. This page indexes the [`examples/`](../../examples) tree, the distribution
[`scripts/`](../../scripts), and the captured terminal sessions.

Prerequisites: a checkout of this repo plus a Rust toolchain (the install
script below builds from source), or the prebuilt release bundle if you would
rather not compile.

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
# -> a SHORTS list with two GND/+5V touches and a "2 short(s)" summary
```

By default these `hauksbee run` reports are informational: they print findings
but exit 0. Add `--strict` to make them FAIL on a real problem (see
[Gate a pipeline](#gate-a-pipeline-with---strict) below). Or gate on
`hauksbee-ci` / `hauksbee check-code` (the same gate for Board-as-Code `.board`
sources, see [BOARD_AS_CODE.md](../ingest/BOARD_AS_CODE.md)) for the full
assertion/fault flow.

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

The expert output (default) for that board:

```
note: this board has no routed copper (no track segments): the spacing check had only pads to compare, so a clean result here says nothing about routing that does not exist yet.
DRC: 20 primitive(s), clearance rule 0.200 mm

SHORTS (2):
  [SERIOUS] GND touches +5V on B.Cu (gap 0.0000 mm) at x=112.0, y=100.0
  [SERIOUS] GND touches +5V on F.Cu (gap 0.0000 mm) at x=112.0, y=100.0

2 short(s), 0 below-rule group(s), 0 at-limit group(s).
note: gate-grade finding(s) above, but this is a report command so the exit code is 0. Add --strict to exit 2 on them (exit contract: 0 = clean or report-only, 1 = input error such as a missing or unreadable file, 2 = findings under --strict, 3 = invalid for analysis), or gate CI with hauksbee-ci.
```

The first note is there because this demo board carries pads and no tracks: the
spacing check says so rather than letting a thin result read as a clean one. The
last line is on stderr, and it is there because this run exited 0. A report
command prints what it found and does not gate; `--strict` is what turns the
same findings into exit 2. Without the note, a pipeline could read the green tick
next to a serious short and conclude the board is clean.

The same finding in `--plain` (the per-finding text is elided here; the tool
prints each paragraph in full):

```
note: this board has no routed copper (no track segments): the spacing check had only pads to compare, so a clean result here says nothing about routing that does not exist yet.
2 issues found, 2 serious.

1. [SERIOUS] Two separate connections, "GND" and "+5V", are touching,
   near x=112.0 mm, y=100.0 mm on the back copper layer (B.Cu).
     Why it matters: These are meant to be electrically separate. Where they
       touch they become one connection (a short), so "GND" and "+5V" will be
       forced to the same voltage ... if one is a power rail it can pull large
       current and overheat.
     What to do:     Pull the two pieces of copper apart so there is a clear gap
       between them, or remove the bit of copper that bridges them ...

2. [SERIOUS] ... the same short where it crosses the front copper layer (F.Cu),
   same wording, same coordinates ...

Summary: 2 short(s), 0 net pair(s) below the clearance rule, 0 at minimum clearance (no margin).
note: gate-grade finding(s) above, but this is a report command so the exit code is 0. Add --strict to exit 2 on them (exit contract: 0 = clean or report-only, 1 = input error such as a missing or unreadable file, 2 = findings under --strict, 3 = invalid for analysis), or gate CI with hauksbee-ci.
```

On a board with many similar near-miss clearance findings, `--plain` prints
the first few in full and condenses the rest into one line per rule and layer
("...and 17 more net pairs like this on the back copper layer (B.Cu) (89
locations, tightest 0.150 mm vs your 0.200 mm rule); pass --verbose for every
instance."), then closes with the same one-line summary. `--verbose` restores
every instance; `--json` always carries the complete set.

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
circuit its copper implements and binds 59 of its 67 simulatable parts from
the stock model library. It states plainly which active parts it does not
recognise (the BMA423 IMU, and `U1`, the SR2HARU e-paper panel) instead of
guessing, and names them again in the bottom line as the reason the analog and
thermal results on their nets are not to be trusted. Add
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
analysis runs locally, inside the `hauksbee serve` process. (A hosted
version would be a packaging job around the same `bytes -> JSON` core, not new
analysis work.)

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
| [`boot_gate_pass`](../../crates/hauksbee-ci/examples/boot_gate_pass.toml) / [`fail`](../../crates/hauksbee-ci/examples/boot_gate_fail.toml) | `boot_coverage`: does the firmware drive a Hi-Z gate in time? | **GREEN** / **RED** |
| [`watchy_v15_display_res`](../../crates/hauksbee-ci/examples/watchy_v15_display_res.toml) / [`undriven`](../../crates/hauksbee-ci/examples/watchy_v15_display_res_undriven.toml) † | `boot_coverage` on the real Watchy v1.5 e-paper RES# (ESP32 QEMU) | **GREEN** / **RED** |
| [`pic_programmer_schematic.toml`](../../crates/hauksbee-ci/examples/pic_programmer_schematic.toml) † | Schematic-stage CI on a `.kicad_sch` (no PCB yet) | **GREEN** |

† These three specs (the Watchy v1.5 pair and the pic_programmer schematic)
run against boards in the developer board corpus: the historical-revision
Watchy v1.5 and KiCad's `pic_programmer` demo. That corpus is not redistributed
in this repo, and the Watchy pair also needs the Espressif QEMU ESP32 backend.
Their integration tests skip cleanly when the corpus or backend is absent.

**Running these yourself takes more than `fetch-corpus.sh`, and it is worth
saying why rather than letting you find out.** The `board` paths in those three
specs are `board-corpus/famous/watchy_history/v1.5/...` and
`board-corpus/kicad-demos-src/demos/pic_programmer/...`, and
`scripts/fetch-corpus.sh` **does not produce those paths**. The fetch writes one
directory per `corpus.toml` id (`board-corpus/watchy_history_v1_5/`,
`board-corpus/kicad_demos/`), with no `famous/` level and different directory
names. The specs were written against the maintainers' hand-built corpus layout
and the two have never agreed, so a fetched corpus leaves these specs pointing at
nothing. Fetch and then re-point the `board` line, or symlink the layout, until
the two are reconciled.

To run a real board here with no extra setup at all, use
`hauksbee run boards/watchy.kicad_pcb --report` above, or
`hauksbee-ci run --example blinky`.

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
