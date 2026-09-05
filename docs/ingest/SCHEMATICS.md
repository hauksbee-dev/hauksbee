# Schematic extraction (`.kicad_sch`)

Hauksbee simulates a board from its schematic alone, before a layout exists.
Point `hauksbee run` or a spec's `board` key at the **hierarchy root**
`.kicad_sch`; sub-sheets are resolved by file (`Sheetfile`). Pointing at a
sub-sheet is refused with a message naming the root.

The reader (`crates/hauksbee-extract/src/schematic.rs`) produces the same
`ExtractedBoard` every other reader does, so binding, checks, the solver and
co-sim are unchanged. Absent: geometry-dependent physics (copper parasitics,
pad positions, DRC). Component positions for rendering come from symbol
placements.

## How the netlist is derived

A schematic stores no nets; they are computed geometrically, as eeschema does:

1. **Geometric connectivity.** Every pin, wire endpoint, junction, label and
   sheet pin is placed in absolute coordinates (library symbols are y-up, the
   canvas y-down; rotation is CCW on screen; `mirror x`/`mirror y` apply after
   rotation). Coordinates snap to a 0.001 mm grid and coincident points are
   unioned.
2. **Named unification.** Local labels unify within a sheet; global labels
   and power symbols unify across the design. A power symbol names its net
   after its `Value` only when its pin is `power_in`; a `power_out` symbol
   (`PWR_FLAG`) never names a net. A symbol is a power source when its library
   symbol carries `(power)` or its reference starts `#PWR`. Net names are
   unescaped (`VPP{slash}MCLR` -> `VPP/MCLR`) and render braces dropped
   (`SCL_{2}` -> `SCL_2`) by `crates/hauksbee-extract/src/netname.rs`.
3. **Hierarchy.** A child's `hierarchical_label "X"` is the parent's
   `(sheet (pin "X"))`. A reused sub-sheet is expanded once per instance with
   its own references from the `(instances ...)` block. Multi-unit parts fold
   into one component.
4. **Wire incidence.** Anything lying along a wire segment (not only at its
   ends) is on that wire.
5. **Implicit power pins.** A hidden `power_in` pin imposes its pin name as a
   global net; visible `power_in` pins are wired normally.
6. **Buses** carry no connectivity of their own; members travel by name. Bus
   labels and sheet pins expand: vector `D[0..7]` -> `D0`..`D7`; group
   `USB{DP DM}` -> `USB.DP`, `USB.DM`; anonymous group `{A B[0..1]}` -> `A`,
   `B0`, `B1`; `(bus_alias ...)` definitions expand when referenced. A bus
   crossing a sheet pin under a different name maps members positionally
   (`DQ7` <-> `DPC7`).

Unnamed nets take KiCad's `Net-(Ref-PadN)` form.

## Validation

For a project with both a schematic and a layout, the derived netlist must
induce the same partition of pins into nets as the layout. The diagnostic:

```
cargo run -p hauksbee-extract --example sch_diag -- <project-dir> <project-stem>
# SCH: 82 comps, 85 nets, 53 multi-pin nets
# PCB: 82 comps, 84 nets, 53 multi-pin nets
# shared pins: 266 / PCB nets that split in SCH: 0 / SCH nets that merge PCB nets: 0
```

`DETAIL=1` / `MERGE=1` print the offending nets. A runnable schematic-stage
spec ships at `crates/hauksbee-ci/examples/pic_programmer_schematic.toml`.

## Coverage and limits

- KiCad 6 through 10 s-expression `.kicad_sch` are supported; the parser keys
  off structure, not the version stamp.
- **KiCad 5 legacy `.sch`** (non-s-expression) is not parsed. The layout of
  such a project is still read by the PCB extractor.
- **Net-tie footprints** have no schematic counterpart, so a board relying on
  one shows the split that is correct for the schematic. A KiCad schematic
  has no construct for a deliberate two-net join; the companion-schematic tie
  path is Eagle-only ([SHORTS.md](../checks/SHORTS.md)).
- Bus alias references (`{ALIAS}`) and group-member qualification are
  implemented and fixture-tested, but not validated against a real layout.
