# Capabilities and scope

## Binaries

| Binary | Purpose |
|---|---|
| `hauksbee` | analyse a board (`run`), Board-as-Code (`to-code`, `from-code`, `merge-ses`, `check-code`), SPICE decks (`sim`), the web front door (`serve`), backend discovery (`doctor`), model tooling (`models`), simulator installs (`install`) |
| `hauksbee-ci` | run specs (`run`), validate them (`check`), scaffold (`init`), wire into a repo (`hook`, `github-action`) |
| `hauksbee-mcp` | the same engine as a stdio MCP server |

`--quiet` (global) suppresses `note:` lines on stderr.

## Inputs

`hauksbee run <BOARD>` and a spec's `board` accept `.kicad_pcb`, `.kicad_sch`
(hierarchy root, [SCHEMATICS.md](../ingest/SCHEMATICS.md)), `.net` (KiCad
netlist), Eagle `.brd` ([EAGLE.md](../ingest/EAGLE.md)), Altium `.PcbDoc`
([ALTIUM.md](../ingest/ALTIUM.md)), IPC-D-356 `.d356`, IPC-2581, ODB++, a
gerber folder or zip ([GERBER.md](../ingest/GERBER.md)), or Board-as-Code
`.board`. Content is sniffed; the extension does not matter.
`--example blinky` runs an embedded board.

Identity inputs: `--bom FILE`, `--bom-column ROLE=HEADER`, `--placement FILE`
([BOM.md](../ingest/BOM.md)); `--schematic FILE` (Eagle companion,
[SHORTS.md](../checks/SHORTS.md)). DNP policy: `--fit`, `--no-fit`,
`--honour-dnp`, `--fit-all-dnp` ([DNP.md](../ingest/DNP.md)). Models:
`--models-dir DIR`. Rework: `--asbuilt FILE` (`.asbuilt.toml`).

## Static analysis: `hauksbee run <board>`

| Flag | Prints | Gate under `--strict` |
|---|---|---|
| `--report` | the bind table (component to model); `--verbose` shows every row | never gates |
| `--drc` | copper shorts and clearance ([SHORTS.md](../checks/SHORTS.md)) | true shorts |
| `--lint` | connectivity lint, strap-pin boot states, resource conflicts, device decode, output contention | high/medium findings |
| `--resources` | the MCU resource-conflict subset ([RESOURCE_CONFLICTS.md](../checks/RESOURCE_CONFLICTS.md)) | high/medium |
| `--si` | signal integrity, ampacity, ripple ([SI_CHECKS.md](../checks/SI_CHECKS.md)) | any real finding |
| `--ampacity` | IPC-2221 trace capacity for power-like nets | capacity only |
| `--usb-c` | USB-C CC termination classification (Rd/Ra, what a compliant source sees, VBUS) | a serious CC verdict |
| `--thermal [--ambient C]` | per-device junction temperature ([THERMAL.md](../checks/THERMAL.md)) | partial coverage exits 3 by default; `--no-strict-thermal` |
| `--ac F:F:N[:dec|oct|lin]` with `--ac-node`, `--ac-csv`, `--ac-loop` | Bode table, loop margins ([AC_ANALYSIS.md](../analysis/AC_ANALYSIS.md)) | invalid exits 3 |
| `--check` (alias `--all`) | bind, DRC, lint, SI, USB-C in one report | the union |
| `--list-nets` | sorted net names | |

Modifiers: `--plain` (alias `--explain`) rewrites findings as what / why /
what to do; `--json` emits the documented object
([JSON_OUTPUT.md](../analysis/JSON_OUTPUT.md)); `--strict` (alias
`--fail-on-findings`) exits 2 on gate-grade findings and 3 when the run cannot
be judged; `--oracle` cross-checks `--drc` against `kicad-cli`
([ORACLES.md](../cosim/ORACLES.md)); `--junit FILE` and `--sarif FILE` write
the selected surface as CI artifacts; `--verbose` prints every repeated
record. When active parts have no model, model-dependent surfaces print
`INCONCLUSIVE:` and read `verdict: invalid`.

A `hauksbee-waivers.toml` beside the board overrules individual findings
(reason and expiry required); syntax in [CI.md](../ci/CI.md#waivers).

## Firmware co-simulation

```
hauksbee run <board> --firmware <img> --headless --seconds N [--chunk-us US] [--probe NETS --probe-csv FILE]
```

`--firmware` takes a compiled `.elf`/`.hex`, a PlatformIO project directory
(built with your `pio run`), or a zip of either. Backends: AVR in-process
(libsimavr), STM32/nRF52/RISC-V/RP2040 through Renode, ESP32/S3/C3 through
Espressif QEMU, all behind one `Mcu` trait ([MCU.md](../cosim/MCU.md),
[SIMULATORS.md](../cosim/SIMULATORS.md)). GPIO, UART, ADC, I2C and SPI couple
to the solved circuit. `--strict` fails on any stress fault; `--strict-boot`
also fails on the boot-safety advisory (a transistor control net driven or
pulled HIGH from reset with no bias resistor). `--plain`/`--json` include a
boot-state panel of every identified gate (`driven HIGH and held`, `driven LOW
and held`, `never driven (floating)`; JSON `boot_gates`).

Host serial: `--serial-attach [--serial-transport pty|tcp] [--serial-wait SECS]
[--serial-no-pace] [--serial-mcu REF]` opens a device path onto the emulated
UART so unmodified host software talks to the simulated board
([MCU.md](../cosim/MCU.md#serial-attach)).

Live view: `--serve [--port N] [--open|--no-open]`, or `hauksbee serve`.

## CI: `hauksbee-ci`

```
hauksbee-ci init <board> [--out PATH]
hauksbee-ci check <spec>... [--no-board] [--json] [--models-dir DIR]
hauksbee-ci run <spec>... | --example NAME  [--junit OUT.XML] [--json] [--quiet] [--seed N] [--models-dir DIR]
hauksbee-ci hook install | uninstall
hauksbee-ci github-action [--write [PATH]]
```

A spec is TOML: `board`, optional `firmware`, `[[supply]]`, `[[net_drive]]`,
`[[override]]`, `[fuzz]`, `[[tolerance]]`/`[ensemble]`, `[[scenario]]`,
`[[peripheral]]`, `[[sensor]]`, `[ac]`, `[timing]`, and at least one
`[[assert]]`. Assertion kinds: `voltage`, `uart`, `toggle`, `no_faults`,
`max_current`, `max_temp`, `peripheral`, `rail_window`, `protection_trip`,
`boot_coverage`, `phase_margin`, `ac_gain`, `hwtrace`, `model_coverage`.
Exit: 0 green, 1 red, 2 spec/board error, 3 invalid. Full reference:
[CI.md](../ci/CI.md); JSON: [ci/JSON_OUTPUT.md](../ci/JSON_OUTPUT.md).

## Board-as-Code

```
hauksbee to-code <board> [--out FILE]
hauksbee from-code <code> [--out FILE] [--relayout | --incremental]
    [--route | --route-grid | --route-dsn FILE] [--route-strict] [--route-timeout SECS]
    [--route-passes N] [--freerouting-jar JAR] [--json]
hauksbee merge-ses <code> <ses> [--out FILE] [--route-strict] [--json]
hauksbee check-code <code> [--seconds N] [--destructive] [--ambient C] [--json]
```

`check-code` recompiles, binds, runs the stress monitor and exits non-zero
on a fault (1 when a part is destroyed); it drops into a pre-commit hook.
`--route` uses freerouting (`$FREEROUTING_JAR`, `tools/` up the tree,
`~/.local/share/freerouting`) and falls back to the in-tree grid A*;
`--route-dsn` exports a Specctra DSN for any router, merged back with
`merge-ses`. DSL syntax and round-trip limits:
[BOARD_AS_CODE.md](../ingest/BOARD_AS_CODE.md). The VS Code extension in
`editors/vscode-hauksbee-board/` edits `.board` files.

## SPICE decks

```
hauksbee sim <deck.cir> | --example NAME  [--op | --tran | --ac | --dc] [--print PROBE...]
    [--out FILE] [--format csv|raw|both]
```

Runs the deck's own analysis (a `.tran` card runs a transient, else `.op`) or
the forced one. `--format raw` writes an ngspice ASCII rawfile; `both` needs
`--out`. A well-formed deck that cannot be answered (`.ac` with no AC source)
exits 3; a malformed deck exits 2. Decks: `examples/decks/`. Supported and
refused cards: [compatibility.md](../spice-compat/compatibility.md).

## Models

`hauksbee models lint | add | remove | list [--builtin] | resolve | new |
coverage | prepare | extract` ([MODELS.md](../models/MODELS.md),
[extending/README.md](../extending/README.md)).

## Environment

`hauksbee doctor [--backends] [--json]`; `hauksbee install esp-qemu | renode
[--yes]`. Variables: `HAUKSBEE_RENODE`, `HAUKSBEE_QEMU_XTENSA`,
`HAUKSBEE_QEMU_RISCV32`, `HAUKSBEE_QEMU_DIR`, `HAUKSBEE_MCU_DIR`,
`HAUKSBEE_LLM_API_KEY`, `HAUKSBEE_LLM_BASE_URL`, `HAUKSBEE_LLM_MODEL`,
`HAUKSBEE_EXTRACT_YES`, `SIMAVR_INCLUDE_DIR`, `SIMAVR_LIB_DIR`,
`FREEROUTING_JAR`.

## MCP server

`hauksbee-mcp` exposes `analyze_board(board_path, firmware_path?,
schematic_path?)`, `run_checks(board_path, spec_toml, firmware_path?,
schematic_path?)`, `list_capabilities()`, `model_coverage(board_path,
models_dir?)`, `board_to_code(board_path)`, and `run_script(source)`: a
QuickJS sandbox with one global, `hauksbee`, offering `analyzeBoard`,
`runChecks`, `listCapabilities`, `modelCoverage`, `boardToCode`; the script
runs as a function body, `console.log` is captured, refusals are thrown as
`{status: "invalid_for_analysis"}`, and scripts are killed after 120 s. A
refusal is returned as `{"status":"invalid_for_analysis","reason":...}` and is
never a pass or a fail.

## Integrations

- [GitHub Action](../../integrations/github-action/README.md): `mode: spec`
  (`spec`/`specs` inputs) or `mode: check` (`board` input), auto-detected when
  omitted; outputs `passed` and `junit`.
- [KiCad plugin](../../integrations/kicad-plugin/README.md): run a spec from pcbnew.
- [pre-commit](../../integrations/pre-commit/README.md).
- [VS Code](../../editors/vscode-hauksbee-board/README.md).
