# Extending hauksbee

Worked walkthroughs, one per extension type. Each starts from a real datasheet
or file format, walks the actual steps against the current code, and ends with
the test that proves the extension works. They assume you can read TOML and run
`cargo`, and — for everything except [new device physics](new-device-physics.md)
— they assume **no knowledge of hauksbee's source**.

## I want to add a ___

| I want to add a… | Walkthrough | Rust required? |
|---|---|---|
| **analog part** (LDO, op-amp, diode, BJT, MOSFET, comparator) | [add-an-analog-part.md](add-an-analog-part.md) | no — one `[[models]]` entry |
| I2C/SPI **sensor** (register map, e.g. BME280) | [add-a-sensor.md](add-a-sensor.md) | no — one TOML file |
| **logic IC** (gates, flip-flops, shift registers) | [add-a-logic-ic.md](add-a-logic-ic.md) | no — one TOML entry |
| **MCU / chip** (a new Renode/QEMU part, so its firmware co-sims exactly) | [add-an-mcu-variant.md](add-an-mcu-variant.md) | no — two TOML files, no recompile |
| **hardware trace** (scope/LA capture as a CI gate) | [add-a-hardware-trace.md](add-a-hardware-trace.md) | no — two TOML files + the instrument export |
| **board file format** (a new reader) | [add-a-board-format.md](add-a-board-format.md) | yes — one trait impl in a fork |
| **device physics** (a new solver element) | [new-device-physics.md](new-device-physics.md) | yes — core change, checklist-enforced |
| shareable **model pack** (bundle of the above data) | [make-a-model-pack.md](make-a-model-pack.md) | no — a directory + manifest |

## The extension hierarchy

The rows above are ordered from cheapest to most invasive
(`docs/dev-plans/06-extensibility-sdk.md` §0):

1. **Data.** Model TOML, sensor specs, logic specs, SoC descriptors. No
   recompile; validated fail-loud at load.
2. **Packs.** Versioned bundles of (1) you can share and pin
   ([docs/PACKS.md](../PACKS.md)).
3. **Plugins-by-trait.** Board readers (and I2C/SPI peripherals) implement a
   small stable trait in a fork. One registration line, no core edits.
4. **Core.** New `Device` variants and stamps. Rust, deliberately — and guarded
   by an enforced checklist so a missed integration site cannot ship.

A design rule shared by every layer: **bad data fails loud.** Every validator
in these walkthroughs produces a named error for each failure category, never a
generic parse error and never a silent fallback. When a walkthrough shows you a
validation rule, that rule exists because its absence once corrupted a result.

## Shared tooling

- `hauksbee models lint <file>` — validates a `[[models]]` db file (including
  any `[models.logic]` block, compiled through the same path board binding
  uses) or a `[sensor]` spec. From a checkout:
  `cargo run -p hauksbee-engine --bin hauksbee -- models lint <file>`.
- `hauksbee models resolve <board>` — per component, which model entry won and
  from which priority layer. The debugging surface for model and pack authors.
- `hauksbee models add | list | remove` — pack management
  ([make-a-model-pack.md](make-a-model-pack.md)).
- `model-extract` — auto-draft a model TOML from a PDF datasheet via an LLM, as a
  starting point you then lint and correct. It is a separate binary (not a
  `hauksbee` subcommand) and is not installed on PATH; run it from a checkout:
  `cargo run -p hauksbee-models --bin model-extract -- <datasheet.pdf>`. See
  [../MODELS.md](../MODELS.md#pointing-hauksbee-at-a-datasheet) for the workflow.

## Deeper references

- [docs/MODELS.md](../MODELS.md) — the model database and matching.
- [docs/PACKS.md](../PACKS.md) — the pack format reference.
- [docs/PERIPHERALS.md](../PERIPHERALS.md) — the peripheral layer sensors plug into.
- [docs/MCU.md](../MCU.md) — the co-sim backends SoC descriptors configure.
- `docs/dev-plans/06-extensibility-sdk.md` — the design plan behind all of this.
- `docs/dev-plans/04-spice-compat.md` §1 — the six-touchpoint hazard table for
  device physics.
