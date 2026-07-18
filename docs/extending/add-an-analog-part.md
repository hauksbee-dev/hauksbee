# Add an analog part: an LDO / op-amp / discrete, one `[[models]]` entry

**Goal.** Bind a stock analog part the built-in DB doesn't ship — an LDO, op-amp,
diode, BJT, MOSFET, or comparator — by hand-writing one `[[models]]` TOML entry.
**No recompile, no LLM in the loop, no source knowledge.** Validated fail-loud at
load, so a typo is caught by `hauksbee models lint` before it ever reaches a run.

When a part on your board is *not* modelled, hauksbee binds it OPEN and says so
("N% resolved", "simulated as OPEN") — this is how you close that gap.

> **Shortcut:** to auto-draft this entry from a PDF datasheet instead of writing
> it by hand, run the extractor and then lint/correct its output:
> `cargo run -p hauksbee-models --bin model-extract -- part.pdf`
> (see [../MODELS.md](../MODELS.md#pointing-hauksbee-at-a-datasheet)). The
> hand-written path below is still worth reading — it is what you edit the draft
> into, and what `hauksbee models lint` checks.

## The shape of a model entry

Every entry is one `[[models]]` block with four parts: an `id`, a `[models.match]`
rule that maps a board part value to it, a `[models.params]` block of the
kind-specific device parameters, and a `[models.pins]` map from footprint pad
numbers to device roles. Optionally `[models.ratings]` for the stress/destruction
monitor.

### Example 1 — an LDO regulator (`kind = "vreg"`)

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

[models.pins]           # SOT-23-3 pinout — map YOUR footprint's pad numbers
"1" = "gnd"
"2" = "in"              # VIN
"3" = "out"             # VOUT  (load-bearing: the regulator sources this net)

[models.ratings]        # optional — enables over-current / over-temp faults
max_current_a = 0.25
```

### Example 2 — an op-amp (`kind = "opamp"`)

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

`hauksbee models lint` enforces these (the authoritative bounds live in
`crates/hauksbee-models/src/validation.rs`, and the full table is in
[`crates/hauksbee-models/db/README.md`](../../crates/hauksbee-models/db/README.md)):

| kind | required params | notes |
|---|---|---|
| `diode` | `is`, `n`, `rs` | Shockley + series R |
| `bjt_npn` / `bjt_pnp` | `is`, `bf`, `nf`, `vaf` | Gummel-Poon-lite |
| `nmos` / `pmos` | `vto`, `kp` | `kp` runs into the tens–hundreds for power FETs |
| `vreg` | `vout`, `dropout_v`, `iq_a` | LDO / regulator |
| `opamp` | `gain`, `rail_lo`, `rail_hi` | `rail_lo < rail_hi` |
| `comparator` | `out_lo`, `out_hi`, `hysteresis` | `out_lo < out_hi` |
| `analog_switch` | `ron`, `roff` | `ron < roff` |

## Pin roles (what to put on the right of `[models.pins]`)

The role strings are consumed verbatim by the binder — use these canonical names
(a wrong role binds the pin OPEN):

- **diode:** `anode`, `cathode`
- **bjt:** `base`, `emitter`, `collector`
- **mosfet:** `gate`, `source`, `drain`
- **vreg:** `in`, `out`, `gnd` — `out` is the regulated net the model sources
- **opamp:** `out`, `in_plus`, `in_minus` (append `_a`/`_b`/`_c`/`_d` per unit on a
  multi-op-amp package — the binder recognises the letter suffixes only, up to
  four channels; a single-channel part uses the bare roles)
- **comparator:** `out`, `in_plus`, `in_minus`
- **analog_switch:** `com`, `s0`, `s1`, `ctrl`

The fastest way to get a correct `[models.pins]` block for an unfamiliar footprint
is to `hauksbee to-code <a board that already uses the part>` and copy the pad
numbers, or read the closest entry in `crates/hauksbee-models/db/*.toml`.

## Lint it, then prove it binds

```bash
# 1. Validate the entry (params in range, ratings positive-finite, shape legal):
hauksbee models lint my-parts.toml
#    → "model 'mcp1700': ok"

# 2. Prove it actually binds against your board. `run --report --plain` performs
#    the full bind and NAMES anything still simulated as OPEN (the surface that
#    proves your pins connected):
hauksbee run my_board.kicad_pcb --report --plain --models-dir .
#    `models resolve` is complementary — it shows which model entry each part
#    MATCHED and from which priority layer, but it does not exercise pin binding,
#    so it will not tell you a pin role is wrong:
#    hauksbee models resolve my_board.kicad_pcb --models-dir .
```

`--models-dir .` layers your file above the built-in DB for this run. To install
it permanently, drop the file in `~/.hauksbee/models/` (loads on every run) or
ship it as a [model pack](make-a-model-pack.md).

## Adding a whole MCU / chip

An MCU is a different animal — its firmware runs on an emulated core, so it needs
a SoC descriptor (register map + backend) in addition to a routing entry. That is
the [add-an-mcu-variant](add-an-mcu-variant.md) two-file recipe (still no
recompile). If a board's MCU is co-simulated on a *substitute* core, the co-sim
report tells you so and points you here.
