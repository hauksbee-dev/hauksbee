# Extending hauksbee

This page lists one walkthrough per extension type. Each walkthrough starts
from a real datasheet or file format, follows the actual steps against the
current code, and ends with the test that proves the extension works. Each
walkthrough assumes you can read TOML and run `cargo`. Except for
[new device physics](new-device-physics.md), each walkthrough assumes
**no knowledge of the hauksbee source code**.

## I want to add a ___

| I want to add a… | Walkthrough | Rust required? |
|---|---|---|
| **analog part** (LDO, op-amp, diode, BJT, MOSFET, comparator) | [add-an-analog-part.md](add-an-analog-part.md) | no, one `[[models]]` entry |
| I2C/SPI **sensor** (register map, e.g. BME280) | [add-a-sensor.md](add-a-sensor.md) | no, one TOML file |
| **logic IC** (gates, flip-flops, shift registers) | [add-a-logic-ic.md](add-a-logic-ic.md) | no, one TOML entry |
| **MCU / chip** (a new Renode/QEMU part, so its firmware co-sims exactly) | [add-an-mcu-variant.md](add-an-mcu-variant.md) | no, two TOML files, no recompile |
| **hardware trace** (scope/LA capture as a CI gate) | [add-a-hardware-trace.md](add-a-hardware-trace.md) | no, two TOML files + the instrument export |
| **board file format** (a new reader) | [add-a-board-format.md](add-a-board-format.md) | yes, one trait impl in a fork |
| **device physics** (a new solver element) | [new-device-physics.md](new-device-physics.md) | yes, core change, checklist-enforced |
| shareable **model pack** (bundle of the above data) | [make-a-model-pack.md](make-a-model-pack.md) | no, a directory + manifest |

## The extension hierarchy

The rows above go from cheapest to most invasive:

1. **Data.** Model TOML, sensor specs, logic specs, and SoC descriptors. No
   recompile is needed. The loader validates each file at load time and
   fails loud on error.
2. **Packs.** Versioned bundles of the data layer above. Share and pin a pack
   ([docs/models/PACKS.md](../models/PACKS.md)).
3. **Plugins-by-trait.** Board readers, and I2C/SPI peripherals, implement a
   small, stable trait in a fork. This needs one registration line and no
   core edits.
4. **Core.** New `Device` variants and stamps, written in Rust by design. An
   enforced checklist guards this layer, so a missed integration site cannot
   ship.

One design rule applies to every layer: bad data fails loud. Every validator
in these walkthroughs produces a named error for each failure category. It
never produces a generic parse error, and it never falls back silently. When
a walkthrough shows a validation rule, that rule exists because a past run
without it corrupted a result.

## Shared tooling

- `hauksbee models lint <file>` validates a `[[models]]` database file,
  including any `[models.logic]` block compiled through the same path board
  binding uses, or a `[sensor]` spec. From a checkout, run:
  `cargo run -p hauksbee-engine --bin hauksbee -- models lint <file>`.
- `hauksbee models resolve <board>` shows, for each component, which model
  entry won and from which priority layer. Model and pack authors use this
  to debug.
- `hauksbee models add | list | remove` manages packs
  ([make-a-model-pack.md](make-a-model-pack.md)).
- `model-extract` drafts a model TOML from a PDF datasheet with an LLM. Use
  this draft as a starting point, then lint and correct it. This is a
  separate binary, not a `hauksbee` subcommand, and it is not installed on
  PATH. Run it from a checkout:
  `cargo run -p hauksbee-models --bin model-extract -- <datasheet.pdf>`. See
  [../MODELS.md](../models/MODELS.md#pointing-hauksbee-at-a-datasheet) for the workflow.

## Deeper references

- [docs/models/MODELS.md](../models/MODELS.md): the model database and matching.
- [docs/models/PACKS.md](../models/PACKS.md): the pack format reference.
- [docs/cosim/PERIPHERALS.md](../cosim/PERIPHERALS.md): the peripheral layer sensors plug into.
- [docs/cosim/MCU.md](../cosim/MCU.md): the co-sim backends SoC descriptors configure.
