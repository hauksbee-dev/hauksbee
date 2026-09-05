# Eagle `.brd` ingest

```bash
hauksbee run my_board.brd --report --plain   # which parts were modelled
hauksbee run my_board.brd --drc --plain      # shorts and clearance from the copper
hauksbee run my_board.brd --lint --plain     # design lint
hauksbee to-code my_board.brd                # decompile to Board-as-Code
```

No flag and no conversion: `hauksbee run` sniffs the content, so a `.brd`
works wherever a `.kicad_pcb` does, including as the `board` key of a
`hauksbee-ci` spec.

## Accepted input

- **Eagle 6 or later** (XML `.brd`), as saved.
- **Fusion 360 Electronics**: export or save an Eagle `.brd`. Native Fusion
  project files are not read.
- **Pre-Eagle-6 binary `.brd`**: not read. Re-save once in Eagle 6+ or Fusion.
  Recognised by the drawing header bytes (`0x10 0x00` / `0x10 0x80`) and
  refused with a message naming the re-save; exit 1.
- **Cadence Allegro `.brd`** is an unrelated binary format; use its gerbers
  ([GERBER.md](GERBER.md)).
- A **Git LFS pointer** is recognised and refused with `git lfs pull` advice.

## What the reader extracts

- **Nets** from `<signals>` (XML entities decoded).
- **Components** from `<elements>`: reference, value, library, package,
  placement, rotation, side, DNP flag (`POPULATE="no"`).
- **Pins** by resolving each element's package from `<packages>`; packages are
  keyed per library, so two libraries' `0805` stay distinct.
- **Copper geometry per net** for the DRC: wires, vias, pads, SMDs, polygons,
  rectangles, circles on layers 1 (top) .. 16 (bottom). It feeds the same
  R-tree short/clearance engine the KiCad path uses; the board's own
  `<designrules>` clearance is applied ([SHORTS.md](../checks/SHORTS.md)).

Everything downstream (binding, checks, solve, co-sim) is format-blind.

## What it does not extract

- **Eagle schematics are not a netlist source.** The `.brd` fully describes
  the circuit. One thing is read from a companion `.sch`: **net ties declared
  through supply symbols** (a star ground drawn by placing one net's supply
  symbol on another), which a `.brd` cannot express. Pass `--schematic <FILE>`
  or leave `<name>.sch` beside `<name>.brd`. It adds context to a matching
  short; it never downgrades it. See
  [SHORTS.md](../checks/SHORTS.md#declared-ties-read-from-an-eagle-schematic).
- **Pin electrical roles** are absent from the `.brd` and come out blank.
- **Copper pour fill** is not stored in the file; see the pour caveat in
  [SHORTS.md](../checks/SHORTS.md).

## Bind rates

Passives bind from their values; ICs bind when the model library knows the
part, and the rest are reported unresolved and simulated open. Copper checks
are unaffected by bind coverage. Close a gap with one model TOML file
([MODELS.md](../models/MODELS.md)).
