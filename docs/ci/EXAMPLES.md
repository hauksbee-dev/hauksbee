# Examples

Runnable inputs that ship in this repository, and the commands that use them.

## Install

```bash
scripts/install.sh        # build hauksbee + hauksbee-ci (release) into ~/.local/bin; --prefix, --symlink, --no-build
scripts/doctor.sh         # which tools (kicad-cli, simavr, qemu, renode, freerouting) are present
hauksbee doctor --backends
```

## First results

```bash
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --report   # bind table
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --drc      # copper shorts / clearance
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --lint     # connectivity + strap lint
hauksbee run crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb --drc   # a deliberate GND/+5V short
hauksbee run --example blinky --check --plain                               # embedded, no checkout needed
```

Reports exit 0 even with findings; `--strict` (alias `--fail-on-findings`)
exits 2 on gate-grade findings. `--plain` (alias `--explain`) rewrites each
finding as what it is, why it matters and what to do; on many similar
clearance findings it condenses after the first few (`--verbose` restores every
instance; `--json` always carries the full set).

| Surface | `--strict` fails on |
|---|---|
| `--drc` | a true copper short (clearance notes never gate) |
| `--lint` / `--resources` | any high- or medium-severity finding |
| `--si` | any real finding (info notes never gate) |
| `--usb-c` | a serious CC verdict |
| `--headless` | any stress fault raised during the co-sim |

## The web front door

```bash
hauksbee serve                 # http://127.0.0.1:3001
hauksbee serve --port 8080 --open
hauksbee run board.kicad_pcb --serve --port 3001
```

Drop a `.kicad_pcb` / `.kicad_sch` / `.brd` / `.PcbDoc` / gerber zip on the
page: verdict, findings grouped by check, a 2D copper map with probes, a checks
composer that runs the `hauksbee-ci` engine, and report export. Analysis runs
in the `hauksbee serve` process. Build the frontend once from a checkout:
`cd frontend && bun install && bun run build`.

The bundled Watchy (`crates/hauksbee-ci/examples/boards/watchy.kicad_pcb`,
MIT, `watchy.LICENSE`) is the "real board" case: an ESP32-S3 e-paper watch.

## CI specs

`crates/hauksbee-ci/examples/` (canonical) and `examples/ci-specs/`
([README](../../examples/ci-specs/README.md)). Spec reference: [CI.md](CI.md).

| Spec | Demonstrates | Verdict |
|---|---|---|
| `tarski_brownout.toml` | fuzzed power-up bit collapses the rail | RED (exit 1) |
| `tarski_brownout_repaired.toml` | same board, milliohm shunt `[[override]]` | GREEN |
| `blinky.toml` / `blinky-permissive.toml` | rail + UART + blink + no-faults | GREEN |
| `boot_gate_pass.toml` / `boot_gate_fail.toml` | `boot_coverage`: does firmware drive a Hi-Z gate in time? | GREEN / RED |
| `power_resistor_cool.toml` / `power_resistor_hot.toml` | `max_temp` at 25 C / 90 C ambient | GREEN / RED |
| `tolerance_divider.toml` / `tolerance_divider_corners.toml` | Monte-Carlo and corner ensembles | RED on the tight window |
| `lm75_thermostat.toml` / `lm75_thermostat_cold.toml` | `[[sensor]]` register-map I2C device | GREEN |
| `watchy.toml` | the bundled Watchy | GREEN |
| `pic_programmer_schematic.toml` | schematic-stage CI on a `.kicad_sch` | needs the KiCad demo project on disk |
| `watchy_v15_display_res*.toml` | `boot_coverage` on Watchy v1.5 (ESP32 QEMU) | need the historical board and the QEMU backend |
| `examples/ci-specs/olimex_wifi_burst_transient.toml` | `rail_window` under an ESP32 WiFi burst scenario | needs the Olimex board on disk |

```bash
hauksbee-ci run crates/hauksbee-ci/examples/blinky.toml
hauksbee-ci run --example blinky                       # embedded, no checkout
hauksbee-ci run crates/hauksbee-ci/examples/tarski_brownout.toml; echo $?   # 1
```

## Other bundled inputs

- SPICE decks: `examples/decks/*.cir` (`hauksbee sim examples/decks/divider.cir --op --print V(out)`; card set in [compatibility.md](../spice-compat/compatibility.md)).
- Board-as-Code: `hauksbee to-code <board> --out b.board`, then `check-code b.board` ([BOARD_AS_CODE.md](../ingest/BOARD_AS_CODE.md)).
- Model examples: `examples/models/lm75.toml`, `lm75_broken.toml` (`hauksbee models lint`).
- Demo boards with firmware: `testdata/boards/*.kicad_pcb` with `testdata/firmware/*` ([MCU.md](../cosim/MCU.md#recipes)).
- Sensor specs: `testdata/sensor-specs/`. Hardware traces: `testdata/hwtraces/`.

## Integrations

- [GitHub Action](../../integrations/github-action/README.md): prefers a prebuilt release binary, falls back to building from source.
- [KiCad plugin](../../integrations/kicad-plugin/README.md): run a spec from pcbnew.
- [pre-commit hook](../../integrations/pre-commit/README.md): gate commits on schematic- or layout-stage specs.
- [VS Code extension](../../editors/vscode-hauksbee-board/README.md): Board-as-Code editing.
- `.github/workflows/ci.yml`: this repository's own CI.
