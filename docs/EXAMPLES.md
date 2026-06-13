# Examples

Everything you need to run hauksbee and learn from it, in files you can open and
run. This page indexes the [`examples/`](../examples) tree, the distribution
[`scripts/`](../scripts), and the captured terminal sessions.

## Get it running in one command

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

`hauksbee --help` lists every command, and `hauksbee run --help` (or any
`<command> --help`) shows that command's flags with an example. Swap in your own
`.kicad_pcb`, `.kicad_sch`, `.brd`, `.d356`, or a folder of gerbers.

## See a board live (the 2D/3D viewer)

`hauksbee run <board>` with no report flag serves the interactive viewer. The
frontend is a build artifact and is not checked in, so build it once:

```bash
cd frontend && bun install && bun run build && cd ..
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb
```

It prints the URL to open (`http://127.0.0.1:3001` by default; change with
`--port`). If you run it before building the frontend, hauksbee still starts the
websocket and tells you exactly this build step rather than serving a blank page.

## Board-as-Code examples

[`examples/board-as-code/`](../examples/board-as-code). Decompile a real
`.kicad_pcb` to editable text, edit it, recompile, re-simulate. Full DSL
reference: [`docs/BOARD_AS_CODE.md`](./BOARD_AS_CODE.md).

| Example | What it is |
|---|---|
| [`blinky.board`](../examples/board-as-code/blinky.board) | The 5-component ATmega328P demo board as DSL: the smallest real example to read. |
| [`stormduino.board`](../examples/board-as-code/stormduino.board) | A real 51-component corpus board, with repeated hardware factored into `fn` blocks. |
| [`tarski_miswire_repair`](../crates/hauksbee-engine/examples/tarski_miswire_repair.rs) | **The headline:** repair the Tarski inhibitory-synapse miswire as a one-line code edit, run through simulation. `cargo run --release -p hauksbee-engine --example tarski_miswire_repair`. |

The edit-then-recheck loop and the miswire walkthrough (with expected output)
are in the [board-as-code README](../examples/board-as-code/README.md).

## hauksbee-ci spec examples

[`examples/ci-specs/`](../examples/ci-specs) + the canonical specs in
[`crates/hauksbee-ci/examples/`](../crates/hauksbee-ci/examples). Spec reference:
[`docs/CI.md`](./CI.md).

| Spec | Demonstrates | Verdict |
|---|---|---|
| [`tarski_brownout.toml`](../crates/hauksbee-ci/examples/tarski_brownout.toml) | The flagship brownout regression: a fuzzed power-up bit collapses the rail | **RED** (exit 1) |
| [`tarski_brownout_repaired.toml`](../crates/hauksbee-ci/examples/tarski_brownout_repaired.toml) | Same board, milliohm-shunt repair applied | **GREEN** |
| [`blinky.toml`](../crates/hauksbee-ci/examples/blinky.toml) | Rail + UART + blink + no-faults assertions (the template spec) | **GREEN** |
| [`olimex_wifi_burst_transient.toml`](../examples/ci-specs/olimex_wifi_burst_transient.toml) | Scenario/transient: a `rail_window` assertion riding an ESP32 WiFi burst | **GREEN** |
| [`boot_gate_pass`](../crates/hauksbee-ci/examples/boot_gate_pass.toml) / [`fail`](../crates/hauksbee-ci/examples/boot_gate_fail.toml) | `boot-coverage`: does the firmware drive a Hi-Z gate in time? | PASS / FAIL |
| [`watchy_v15_display_res`](../crates/hauksbee-ci/examples/watchy_v15_display_res.toml) / [`undriven`](../crates/hauksbee-ci/examples/watchy_v15_display_res_undriven.toml) | `boot-coverage` on the real Watchy v1.5 e-paper RES# (ESP32 QEMU) | PASS / FAIL |
| [`pic_programmer_schematic.toml`](../crates/hauksbee-ci/examples/pic_programmer_schematic.toml) | Schematic-stage CI on a `.kicad_sch` (no PCB yet) | PASS |

More detail and the run-and-expected-verdict for each:
[ci-specs README](../examples/ci-specs/README.md).

## Terminal sessions (real captured output)

[`examples/sessions/`](../examples/sessions). Actual runs of the headline
flows, each file labelled with the command that produced it.

| Flow | Transcript |
|---|---|
| Report a board (bind table) | [`01_report_board.txt`](../examples/sessions/01_report_board.txt) |
| Run DRC | [`02_drc.txt`](../examples/sessions/02_drc.txt) |
| The lint + SI arsenal (real strap-pin finding) | [`03_lint_si_arsenal.txt`](../examples/sessions/03_lint_si_arsenal.txt) |
| Boot firmware headless | [`04_boot_firmware_headless.txt`](../examples/sessions/04_boot_firmware_headless.txt) |
| CI spec GREEN | [`05_ci_spec_green.txt`](../examples/sessions/05_ci_spec_green.txt) |
| CI spec RED | [`06_ci_spec_red.txt`](../examples/sessions/06_ci_spec_red.txt) |
| CI spec repaired GREEN | [`07_ci_spec_repaired_green.txt`](../examples/sessions/07_ci_spec_repaired_green.txt) |
| Transient `rail_window` spec | [`08_ci_spec_transient.txt`](../examples/sessions/08_ci_spec_transient.txt) |
| Board-as-code loop | [`09_board_as_code_loop.txt`](../examples/sessions/09_board_as_code_loop.txt) |
| Environment doctor | [`10_doctor.txt`](../examples/sessions/10_doctor.txt) |
| Boot-coverage PASS / FAIL | [`11_boot_coverage_pass_fail.txt`](../examples/sessions/11_boot_coverage_pass_fail.txt) |
| Miswire repaired as a code edit | [`12_miswire_repair_demo.txt`](../examples/sessions/12_miswire_repair_demo.txt) |

Honest notes on raw escapes / stderr artifacts in those captures:
[sessions README](../examples/sessions/README.md).

## Distribution scripts

[`scripts/`](../scripts). Every script takes `--help` and is idempotent and
CI-safe (colours auto-disable when not on a TTY or when `NO_COLOR` is set).

| Script | What it does |
|---|---|
| [`install.sh`](../scripts/install.sh) | Build `hauksbee` + `hauksbee-ci` (release) and install them onto PATH. `--prefix`, `--symlink`, `--no-build`. |
| [`doctor.sh`](../scripts/doctor.sh) | Report which tools (kicad-cli, simavr, qemu, renode, freerouting) are present and what each unlocks. |
| [`ci.sh`](../scripts/ci.sh) | Run one or more specs the pleasant-in-CI way: finds/builds the binary, writes a JUnit file per spec, exits non-zero if any spec is RED. |
| [`bundle.sh`](../scripts/bundle.sh) | Build a versioned binary bundle (the two bins + db + integrations + examples + scripts) as a `.tar.gz` with a checksum. |

## Releases and the GitHub Action

- [`.github/workflows/release.yml`](../.github/workflows/release.yml) builds the
  binaries on macOS and Linux on a `vX.Y.Z` tag and attaches the bundles to the
  GitHub Release.
- The composite [GitHub Action](../integrations/github-action) **prefers a
  prebuilt release binary** and falls back to building from source, so users do
  not have to compile.
- The [KiCad plugin](../integrations/kicad-plugin) finds a prebuilt or local
  binary automatically and only offers to compile as a last resort.
- The [pre-commit hook](../integrations/pre-commit) gates commits on
  schematic-stage / layout-stage specs.
