# Add an analog part: an LDO / op-amp / discrete, one `[[models]]` entry

**Goal.** Bind a stock analog part that the built-in DB does not ship: an LDO,
op-amp, diode, BJT, MOSFET, or comparator. Write one `[[models]]` TOML entry by
hand. This needs no recompile, no LLM in the loop, and no source knowledge. The
load step validates the entry and fails loud. `hauksbee models lint` catches a
typo before it reaches a run.

When a part on your board is *not* modeled, hauksbee binds it OPEN and says so
("N% resolved", "simulated as OPEN"). This guide shows how to close that gap.

> **Shortcut:** to auto-draft this entry from a PDF datasheet instead of
> writing it by hand, run the extractor. Then lint and correct its output:
> `hauksbee models extract --pdf part.pdf --part MCP1700`
> (both flags are required; there are no positional arguments. See
> [the datasheet workflow in MODELS.md](../models/MODELS.md#pointing-hauksbee-at-a-datasheet).)
> Read the hand-written path below too. It is the shape you edit the draft into,
> and what `hauksbee models lint` checks.

## The shape of a model entry

Every entry is one `[[models]]` block with four parts: an `id`, a `[models.match]`
rule that maps a board part value to it, a `[models.params]` block of the
kind-specific device parameters, and a `[models.pins]` map from footprint pad
numbers to device roles. Add an optional `[models.ratings]` block for the
stress and destruction monitor.

### Example 1, an LDO regulator (`kind = "vreg"`)

Save as `my-parts.toml`:

```toml
[[models]]
id = "mcp1700"
kind = "vreg"
description = "Microchip MCP1700-3302 250 mA LDO, 3.3 V fixed"

[models.match]
value_re = "(?i)^MCP1700"          # matches the board's part value (regex, case-insensitive)

[models.params]
# Source: Microchip MCP1700 datasheet (DS20001826). Required params for a vreg:
vout      = 3.3        # regulated output (V)   [range 0.5 .. 30]
dropout_v = 0.178      # dropout at full load (V) [0 .. 10]
iq_a      = 0.0000016  # quiescent current (A)   [0 .. 1]

[models.pins]           # SOT-23-3 pinout: map YOUR footprint's pad numbers
"1" = "gnd"
"2" = "in"              # VIN
"3" = "out"             # VOUT  (load-bearing: the regulator sources this net)

[models.ratings]        # optional; enables over-current / over-temp faults
max_current_a = 0.25
```

### Example 2, an op-amp (`kind = "opamp"`)

```toml
[[models]]
id = "tl072"
kind = "opamp"
description = "TI TL072 dual JFET op-amp, ±18 V"

[models.match]
value_re = "(?i)^TL072"

[models.params]
gain    = 200000.0     # open-loop DC gain (V/V) [1 .. 1e9]
rail_lo = -15.0        # output low rail (V)  [-60 .. 60, must be < rail_hi]
rail_hi = 15.0         # output high rail (V) [-60 .. 60]

[models.pins]           # a DUAL op-amp: suffix each unit's roles with _a / _b
"1" = "out_a"
"2" = "in_minus_a"
"3" = "in_plus_a"
"5" = "in_plus_b"
"6" = "in_minus_b"
"7" = "out_b"
```

## The required params, per kind

`hauksbee models lint` enforces these, and
`crates/hauksbee-models/src/validation.rs` is the authority for both the
required set and the ranges. [`crates/hauksbee-models/db/README.md`](../../crates/hauksbee-models/db/README.md)
carries a wider table covering the digital and MCU kinds too, but read it as a
guide rather than a spec: it lists `vth` under `analog_switch`, which the
validator does not require (`vth` is optional and defaults to 1.5 V). Where the
two disagree, `validation.rs` is what runs.

| kind | required params | notes |
|---|---|---|
| `diode` | `is`, `n`, `rs` | Shockley + series R |
| `bjt_npn` / `bjt_pnp` | `is`, `bf`, `nf`, `vaf` | Gummel-Poon-lite |
| `nmos` / `pmos` | `vto`, `kp` | `kp` runs into the tens to hundreds for power FETs |
| `vreg` | `vout`, `dropout_v`, `iq_a` | LDO / regulator |
| `opamp` | `gain`, `rail_lo`, `rail_hi` | `rail_lo < rail_hi` |
| `comparator` | `out_lo`, `out_hi`, `hysteresis` | `out_lo < out_hi` |
| `analog_switch` | `ron`, `roff` | `ron < roff` |

## Pin roles (what to put on the right of `[models.pins]`)

The binder reads the role strings verbatim. Use these canonical names. A wrong
role binds the pin OPEN:

- **diode:** `anode`, `cathode`
- **bjt:** `base`, `emitter`, `collector`
- **mosfet:** `gate`, `source`, `drain`
- **vreg:** `in`, `out`, `gnd`. The `out` role is the regulated net the model
  sources.
- **opamp:** `out`, `in_plus`, `in_minus`. Append `_a`/`_b`/`_c`/`_d` per unit
  on a multi-op-amp package. The binder recognizes the letter suffixes only,
  up to four channels. A single-channel part uses the bare roles.
- **comparator:** `out`, `in_plus`, `in_minus`
- **analog_switch:** `com`, `s0`, `s1`, `ctrl`, plus `vcc` / `vss` for the
  supply pins. `s0` is the **normally-closed** throw, the one that conducts when
  the control is LOW; `s1` is the normally-open throw. Getting those two the
  wrong way round routes `com` to the wrong throw in every control state, so
  check the datasheet's NC/NO labelling rather than pin order. Alternative
  spellings the binder accepts: `a` for `com`, `b1`/`nc` for `s0`, `b2`/`no` for
  `s1`, and `s`/`sel`/`in` for `ctrl`.
  - **An SPST part** (a single on/off gate) names only `com`, one throw and
    `ctrl`. Use `s0` when the switch is closed with the control low, and any
    other throw name (`in_out_b`) when it closes with the control high. Wiring
    `ctrl` as one of the throws is the mistake to avoid: it fabricates a
    conductive path from the signal to the control line.
  - **A multi-channel part** suffixes the roles per channel:
    `in_out_1a` / `in_out_1b` with `ctrl_1`, and so on up to `ctrl_4`.

The fastest way to build a correct `[models.pins]` block for an unfamiliar
footprint: run `hauksbee to-code <a board that already uses the part>` and
copy the pad numbers, or read the closest entry in
`crates/hauksbee-models/db/*.toml`.

## Lint it, then prove it binds

```bash
# 1. Validate the entry (params in range, ratings positive-finite, shape legal):
hauksbee models lint my-parts.toml
#    → "model 'mcp1700': ok"

# 2. Prove it actually binds against your board. `run --report --plain` performs
#    the full bind and NAMES anything still simulated as OPEN (the surface that
#    proves your pins connected):
hauksbee run my_board.kicad_pcb --report --plain --models-dir .
#    `models resolve` is complementary; it shows which model entry each part
#    MATCHED and from which priority layer, but it does not exercise pin binding,
#    so it will not tell you a pin role is wrong:
#    hauksbee models resolve my_board.kicad_pcb --models-dir .
```

`--models-dir .` layers your file above the built-in DB for this run. To
install it permanently, drop the file in `~/.hauksbee/models/` (loads on every
run) or ship it as a [model pack](make-a-model-pack.md).

## Adding a whole MCU / chip

An MCU is a different case. Its firmware runs on an emulated core, so it also
needs a SoC descriptor (register map + backend), not only a routing entry.
That is the [add-an-mcu-variant](add-an-mcu-variant.md) two-file recipe,
which also needs no recompile. If a board's MCU is co-simulated on a
*substitute* core, the co-sim report says so and points you here.
