# Hauksbee

**Your design files already contain a running system. Hauksbee executes it.**

Hand Hauksbee a layout, schematic, fab archive, BOM, placement file or
compiled firmware. It reconstructs the circuit the copper implements, binds
device models, solves it, runs the static checks, and can boot the firmware on
an emulated MCU wired to the solved board. Every finding keeps the input, net,
component and model that produced it.

The refusal is part of the product. If an input is partial, an active part has
no adequate model, or two manufacturing records contradict each other,
Hauksbee reports the valid partial result and declines the stronger claim. It
does not turn missing evidence into a green board.

## Get Hauksbee

On macOS, download `Hauksbee.app` from the
[latest release](https://github.com/hauksbee-dev/hauksbee/releases/latest),
unzip it, and double-click: it starts the engine locally and opens the web
interface. For the CLI, one line installs the released binaries:

```bash
curl -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.sh | bash
```

Windows (PowerShell):

```powershell
irm https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.ps1 | iex
```

From a checkout:

```bash
scripts/install-sims.sh --avr          # libsimavr, for the in-process AVR backend
cargo build --release
target/release/hauksbee run --example blinky --check --plain
target/release/hauksbee serve          # browser UI: drop a board, get a verdict
```

`hauksbee serve` opens a local page with sample boards, a live 2D copper view,
probes, a checks composer and report export. Nothing leaves your machine.

## Inputs

- KiCad `.kicad_pcb`, `.kicad_sch` (with hierarchy) and netlists.
- Eagle `.brd` (+ companion `.sch`), Altium `.PcbDoc`, IPC-2581, ODB++.
- Gerber + drill archives (X2, `.gbrjob`), IPC-D-356.
- BOM, pick-and-place, and fitted / no-fit variants, reconciled before binding.
- Firmware images or PlatformIO projects for the supported MCU backends.

## Checks and analyses

- Copper shorts and clearances, connectivity lint, boot straps, resource
  conflicts, I2C loading, device configuration decoding, DNP policy, USB-C CC
  classification.
- Trace ampacity, closed-form signal-integrity estimates, ripple, back-power
  paths, behavioural power ICs.
- DC operating point, transient, small-signal AC with gain and phase margin,
  per-device thermal estimate.
- Firmware co-simulation: AVR (in-process, libsimavr), Renode (STM32, RP2040,
  nRF52, RISC-V) and Espressif QEMU (ESP32) as external processes. GPIO, UART,
  ADC, I2C and SPI couple to the solved circuit.

## Models

```bash
hauksbee models coverage board.kicad_pcb
hauksbee models new U3 --board board.kicad_pcb --kind vreg --out models/u3.toml
hauksbee models lint models/u3.toml
hauksbee run board.kicad_pcb --models-dir models --check --plain
```

An unresolved active part is named with its nets and the analyses it
invalidates. It is bound open, never given invented behaviour. Model packs
install with `hauksbee models add`. See [MODELS](docs/models/MODELS.md).

## Gate it

Specs assert voltages, toggles, boot coverage, rail windows, faults,
temperature, peripherals, model coverage and AC margins, with scenarios and
fuzzed initial states. See [CI](docs/ci/CI.md).

```bash
hauksbee-ci init my_board.kicad_pcb
hauksbee-ci check ci/my_board.toml
hauksbee-ci run ci/my_board.toml --json --junit out.xml
```

Exit codes for `hauksbee run`: 0 clean, 1 input not analysed, 2 findings under
`--strict`, 3 the result could not be trusted. For `hauksbee-ci run`: 0 held,
1 an assertion failed, 2 spec or board invalid, 3 not trustworthy.

An MCP server for agents ships as `hauksbee-mcp`.

## Repository map

- `crates/hauksbee-extract`: CAD, fab, schematic, BOM and placement ingestion.
- `crates/hauksbee-models`: device and MCU model resolution.
- `crates/hauksbee-ir`: circuit representation and evidence types.
- `crates/hauksbee-solve`: DC, transient and AC solvers.
- `crates/hauksbee-mcu`: MCU backends and circuit coupling.
- `crates/hauksbee-engine`: binder, checks, scheduler, reports, CLI.
- `crates/hauksbee-ci`: CI specs, assertions, reports.
- `crates/hauksbee-server`, `frontend/`: local web UI.
- `crates/hauksbee-mcp`: agent tools over stdio.

Start with [START_HERE](docs/START_HERE.md). Contributing notes are in
[CONTRIBUTING](CONTRIBUTING.md).

## Licence

Apache-2.0 (see [LICENSE](LICENSE) and [NOTICE](NOTICE)). A binary built with
the `avr` feature links GPL-3.0 libsimavr and is distributed under GPL-3.0;
builds without it use Renode and QEMU as separate processes.

Source, releases and issues: <https://github.com/hauksbee-dev/hauksbee>.
