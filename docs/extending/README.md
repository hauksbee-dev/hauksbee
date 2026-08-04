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
| **MCU variant** (a sibling of a family hauksbee already supports) | [add-an-mcu-variant.md](add-an-mcu-variant.md) | no, two TOML files, no recompile |
| **MCU family** (a part hauksbee does not support at all) | [add-a-microcontroller.md](add-a-microcontroller.md) | no for a part the emulator models; one static list for a part it does not |
| **hardware trace** (scope/LA capture as a CI gate) | [add-a-hardware-trace.md](add-a-hardware-trace.md) | no, two TOML files + the instrument export |
| **board file format** (a new reader) | [add-a-board-format.md](add-a-board-format.md) | yes, one trait impl in a fork |
| **device physics** (a new solver element) | [new-device-physics.md](new-device-physics.md) | yes, core change, checklist-enforced |
| shareable **model pack** (bundle of the above data) | [make-a-model-pack.md](make-a-model-pack.md) | no, a directory + manifest |

## The extension hierarchy

The rows above go from cheapest to most invasive:

1. **Data.** Model TOML, sensor specs, logic specs, and SoC descriptors. No
   recompile is needed. Where each kind is validated differs, and it matters:
   see "Where validation actually happens" below.
2. **Packs.** Versioned bundles of the data layer above. Share and pin a pack
   ([docs/models/PACKS.md](../models/PACKS.md)).
3. **Plugins-by-trait.** Board readers, and I2C/SPI peripherals, implement a
   small, stable trait in a fork. This needs one registration line and no
   core edits.
4. **Core.** New `Device` variants and stamps, written in Rust by design. An
   enforced checklist guards this layer, so a missed integration site cannot
   ship.

One design rule applies to every layer: bad data fails loud *where it is
validated*. Every validator in these walkthroughs produces a named error for
each failure category, never a generic parse error and never a silent
fallback. When a walkthrough shows a validation rule, that rule exists because
a past run without it corrupted a result.

## Where validation actually happens

Three different gates, at three different moments. Knowing which one covers
your file is the difference between "hauksbee will catch my mistake" and "my
mistake ships".

| Your file | Checked at load? | Checked by `models lint`? | Checked at install? |
|---|---|---|---|
| a `[[models]]` db file in a user dir or `--models-dir` | parse, non-empty `[match]`, regex compile only | yes, fully | n/a |
| a `[[models]]` db file inside a pack | as above | yes, fully | **yes, fully, before anything is copied** |
| a `.soc.toml` MCU descriptor | **yes, fully, and aborts the run** | yes, the loader's own validation plus the checks that catch a descriptor which runs and observes the wrong register | n/a |

The row to read twice is the first. Loading a model db file checks that the TOML
parses, that every entry has at least one populated `[match]` rule (an
all-empty one would match every component on the board), and that every regex
compiles. It does **not** run the per-kind parameter validation. That lives in
`hauksbee models lint`, and nothing calls it for you.

The gap is not hypothetical: two entries in hauksbee's own built-in database
load fine and fail lint. `hauksbee models lint crates/hauksbee-models/db/bjt.toml`
reports `model 's8050': missing required param 'vaf'`, and the diodes file
reports `model 'led_blue': param 'is' = 0.0000000000000000000001 is outside
physical range`. Both still resolve and bind. Put either entry inside a pack and
`hauksbee models add` refuses to install it, because the pack path *does* run the
full validation up front:

```
$ hauksbee models add ./badpack
error: pack model file 'parts.toml' failed validation: model 's8050': model 's8050': missing required param 'vaf'
```

So: **run `hauksbee models lint <file>` on anything you author.** It is not
redundant with loading, and packaging is the only route that runs it
automatically.

A `[sensor]` register-map spec and a `[models.logic]` block ride the same three
gates as the model entry they sit in: `models lint` checks both standalone, and
`hauksbee models add` re-checks them (compiling every logic block through the
engine's own bind path) before it copies a pack.

The `.soc.toml` row is the strict one. A descriptor that exists but does not
validate aborts the run, naming the file and the field, rather than falling back
to a lower-priority descriptor:

```
$ HAUKSBEE_MCU_DIR=./mcu hauksbee run board.kicad_pcb --firmware app.elf --headless
error: resolving MCU descriptor for 'renode:stm32f103': invalid SoC descriptor
./mcu/stm32f103.soc.toml: unknown e_machine "EM_NONSENSE": expected one of
EM_ARM, EM_RISCV, EM_XTENSA, EM_AVR
```

## Shared tooling

- `hauksbee models lint <file>` validates a `[[models]]` database file,
  including any `[models.logic]` block compiled through the same path board
  binding uses, a `[sensor]` spec, or a `[soc]` MCU descriptor (which it also
  prints back as an inspection: which register each GPIO port reads, which buses
  exist, which ADC channels are injectable, and which capabilities the descriptor
  leaves absent). From a checkout, run:
  `cargo run -p hauksbee-engine --bin hauksbee -- models lint <file>`.
- `hauksbee models resolve <board>` shows, for each component, which model
  entry won and from which priority layer. Model and pack authors use this
  to debug.
- `hauksbee models add | list | remove` manages packs
  ([make-a-model-pack.md](make-a-model-pack.md)).
- `hauksbee models extract --pdf <datasheet.pdf> --part <PART>` drafts a model
  TOML from a PDF datasheet with an LLM. Use the draft as a starting point,
  then lint and correct it. Both flags are required and there are no
  positional arguments: the part number is what the entry claims, so the
  command will not guess it. `--kind` is optional (omit it and the extractor
  works it out from the datasheet), `--out-dir` defaults to your user model
  directory, and `--yes` skips the consent prompt for scripts that already have
  it. The same extractor also ships as a standalone binary that takes the same
  flags, for use without the engine:
  `cargo run -p hauksbee-models --bin model-extract -- --pdf <p> --part <PART>`.
  See
  [the datasheet workflow in MODELS.md](../models/MODELS.md#pointing-hauksbee-at-a-datasheet).

## Deeper references

- [docs/models/MODELS.md](../models/MODELS.md): the model database and matching.
- [docs/models/PACKS.md](../models/PACKS.md): the pack format reference.
- [docs/cosim/PERIPHERALS.md](../cosim/PERIPHERALS.md): the peripheral layer sensors plug into.
- [docs/cosim/MCU.md](../cosim/MCU.md): the co-sim backends SoC descriptors configure.
