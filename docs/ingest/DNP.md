# Do-not-populate parts

A DNP flag means two opposite things in practice: "not on this assembly BOM
but it will be there" (a socketed module, a hand-fitted part) and "this link
is deliberately open" (a 0R bridge, a solder jumper, a config strap).

## Default policy

**DNP parts are simulated as fitted, except near-zero-ohm links, which stay
open** (fitting one merges the nets it bridges). A part is a link when it has
two or fewer pins and any of:

- resistance <= 0.5 ohm (`0`, `0R`, `0R0`, `R000`, `0.1`);
- a ferrite bead: value starting `FB`, or `ferrite` in the value **or** footprint;
- a bridging **value** (`JUMPER`, `SOLDER_BRIDGE`, `SOLDERBRIDGE`, `NET_TIE`,
  `NETTIE`, case-insensitive). The footprint is not consulted for this rule,
  so a net-tie footprint with a blank or house value is fitted; name it in
  `--no-fit` to keep it open.

Every run prints its decision:

```
do-not-populate: DNP parts are simulated as fitted (...), except near-zero-ohm links, which stay open (...)
  fitted:    A101 (Arduino_Nano_v3.x), DNP, fitted by default
  left open: R7 (0R), DNP link (near 0 ohm), left open: fitting it merges nets
```

## Overrides

| CLI | Spec key | Effect |
|---|---|---|
| `--fit R7` (repeatable, comma-separated) | `fit = ["R7"]` | fit these regardless of policy |
| `--no-fit A101` | `no_fit = ["A101"]` | leave these open regardless |
| `--honour-dnp` | `dnp = "honour"` | leave every DNP part out |
| `--fit-all-dnp` | `dnp = "fit-all"` | fit every DNP part, links included |
| (default) | `dnp = "fit-except-links"` | the policy above |

An unknown reference, or the same reference in both `fit` and `no_fit`, is an
error.

### Assembly variants

Put a population decision in its own TOML and reference it from the spec:

```toml
# hardware/prototype.variant.toml
name = "prototype without sensor"
fit = ["R7"]
no_fit = ["U4"]
```

```toml
board = "hardware/board.kicad_pcb"
variant = "hardware/prototype.variant.toml"
```

Variant lists merge with the spec's own `fit`/`no_fit`; identical decisions
deduplicate, contradictions refuse. `no_fit` can leave out an ordinary
(non-DNP) part of a superset layout. A selection that leaves every component
open is invalid, and a `[[peripheral]]` whose `ref` names a part left open
refuses.

## Firmware on a board with no processor

If the DNP policy removes the only processor and the run asks for firmware,
the run exits 3 and names the part with both ways out (`--fit <REF>`, or drop
`--firmware`).

## What DNP never affects

Copper DRC reads the layout and ignores the flag: a DNP footprint's pads and
traces are still checked. Component-level checks (converter topology, USB-C
CC, crystal/antenna SI, ampacity, ripple, boot straps, bus contention, device
decode, MCU pin coverage) skip parts the policy left open.
