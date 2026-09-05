# Extending Hauksbee

Every extension below is data (TOML), loaded at run time with no recompile.

| I want to add a... | How | Reference |
|---|---|---|
| analog part (LDO, op-amp, diode, BJT, MOSFET, comparator) | one `[[models]]` entry | [MODELS.md](../models/MODELS.md) |
| I2C/SPI sensor (register map) | a `[sensor]` TOML, or `[models.peripheral] kind = "register_map"` in a card | [MODELS.md](../models/MODELS.md#register-map-peripherals) |
| logic IC | a `[models.logic]` block | `hauksbee models lint` validates it |
| MCU variant of a supported family | a `.soc.toml` descriptor + a `[[models]] kind = "mcu"` routing entry | [add-an-mcu-variant.md](add-an-mcu-variant.md) |
| MCU family Hauksbee does not support | the same two files plus a firmware fixture and test; vendored peripheral models only if the emulator lacks them | [add-a-microcontroller.md](add-a-microcontroller.md) |
| shareable model pack | a directory with `pack.toml` and `models/*.toml` | [MODELS.md](../models/MODELS.md#model-packs) |

A new emulator backend (a new `Mcu` trait implementation) and a new solver
device kind are Rust changes by design.

## Where validation happens

| File | At load | `hauksbee models lint` | `hauksbee models add` |
|---|---|---|---|
| `[[models]]` db file in a user dir or `--models-dir` | parse, non-empty `[match]`, regex compile | full | n/a |
| `[[models]]` db file inside a pack | as above | full | full, before anything is copied |
| `.soc.toml` MCU descriptor | full; an invalid descriptor aborts the run | full, plus an inspection printout | n/a |

Loading a model db file does **not** run per-kind parameter validation, so
run `hauksbee models lint <file>` on anything you author. Packaging is the
only path that runs it automatically.

## Shared tooling

```bash
hauksbee models lint <file>                    # [[models]] db, [sensor] spec, or [soc] descriptor
hauksbee models resolve <board> [--models-dir DIR]   # which entry won, from which layer
hauksbee models add <path|url> | list [--builtin] | remove <name>
hauksbee models new <REF> --board <board> [--kind K] [--out F | --pack-dir D]
hauksbee models coverage <board> [--json] [--require REF:CAPABILITY]
hauksbee models prepare <board> --pack-dir DIR [--models-dir DIR] [--yes]
hauksbee models extract --pdf <datasheet.pdf> --part <PART> [--kind K] [--out-dir D] [--yes]
```

From a checkout: `cargo run -p hauksbee-engine --bin hauksbee -- models lint <file>`.
The datasheet extractor also ships standalone:
`cargo run -p hauksbee-models --bin model-extract -- --pdf <p> --part <PART>`.
