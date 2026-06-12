# Schematic extraction (`.kicad_sch`)

Galvani can simulate a board from its **schematic** alone, before any layout
exists. This is what makes early-stage testing possible: you draw the circuit
in eeschema, and Galvani derives the same netlist eeschema would, binds models,
and runs the solver. No copper is required.

The code lives in `galvani-extract/src/schematic.rs` and produces the same
[`ExtractedBoard`] every other extractor produces, so the binder, solver, MCU
co-sim and stress monitor are unchanged. The only thing absent from a
schematic-derived board is geometry-dependent physics (copper parasitics, pad
positions); component positions for rendering come from the schematic symbol
placements instead.

## Why a layout is easy and a schematic is not

A `.kicad_pcb` hands us connectivity for free: every pad already carries its
net number. A schematic does not store nets at all. eeschema *computes* them
geometrically every time, and so must we. The derivation has four stages.

### 1. Geometric connectivity

Every symbol pin has a connection point in absolute schematic coordinates: the
pin's position in its library symbol, transformed by the symbol instance's
placement (translate, rotate, mirror). Wires, junctions, no-connects, labels
and sheet pins all live at absolute coordinates too. Anything sharing a
coordinate is electrically one node. We snap coordinates to a 0.001 mm grid
(far finer than KiCad's 1.27 mm placement grid, fine enough to absorb textual
rounding) and union-find every coincidence:

- a wire's two endpoints are unioned;
- a pin, a junction, a label endpoint touching a wire endpoint join that wire's
  component.

### 2. Coordinate transform (the subtle part)

Two conventions have to be exactly right or the netlist comes out
plausible-but-wrong:

- **Library symbols are drawn y-up; the schematic canvas is y-down.** The
  pin's local y is negated before placement. Skip this and every vertical
  two-pin part (resistor, capacitor, diode) silently swaps its two pins.
- **The placement angle rotates counter-clockwise on screen**, which on a
  y-down canvas is the matrix `(x cosθ + y sinθ, −x sinθ + y cosθ)`.

`mirror x` negates the local y, `mirror y` negates the local x, applied before
rotation, matching eeschema's transform order.

### 3. Named unification

Names then merge nodes that never touch geometrically:

- **Local labels** unify nets carrying the same text *within one sheet*.
- **Global labels** and **power symbols** unify across the whole design. A
  power symbol names its net after its `Value` (`GND`, `+5V`, `VPP`) — but only
  when its pin is `power_in`. A power symbol with a `power_out` pin is an ERC
  flag (`PWR_FLAG`): it marks a net as driven and must **not** name it, or
  every flagged net collapses into one.
- A symbol is treated as a power source when its library symbol has the
  `(power)` flag *or* its reference is the canonical `#PWR` prefix (some older
  library symbols, e.g. a hand-drawn `VPP`, omit the flag but are still power
  symbols).
- Net names are normalised the way KiCad does: a literal `/` inside a label is
  escaped as `{slash}`, so `VPP/MCLR` and `VPP{slash}MCLR` are the same net and
  must compare equal.

### 4. Hierarchy

Sub-sheets are resolved by file (the `Sheetfile` property), recursing into each
child. A child's `(hierarchical_label "X")` is the same net as the parent
`(sheet (pin "X"))` it sits behind; we record both sides keyed by the sheet
instance path and union them.

A **reused** sub-sheet (the same file instantiated on several pages) is the
reason symbols carry an `(instances (project .. (path "/uuid/uuid"
(reference "R201"))))` block. Each instantiation gets its own designators. We
expand every instance separately (its own geometry scope, its own references
resolved from the path), so `ampli_ht` instantiated twice becomes RV201… and
RV301… with identical topology — exactly KiCad's behaviour.

Multi-unit parts (a quad gate drawn as four symbols sharing one reference) are
folded into one component with the union of every unit's pins, deduping the
common unit-0 pins (VCC/GND) that appear on every gate.

### Net naming

Named nets keep their label/power name. Unnamed nets get KiCad's
`Net-(Ref-PadN)` form after their lowest-sorted member pin. (KiCad's exact
choice of which pin names an anonymous net depends on annotation order, so for
cross-validation we compare the *partition structure*, not the names.)

## Validation

The decisive test is cross-validation against the layout. For a corpus project
shipping both a `.kicad_sch` hierarchy and a `.kicad_pcb`, the netlist we
derive from the schematic must induce the **same partition** of component pins
into nets as the layout does. Net names and ids differ; the partition must be
identical over the pins the two share.

`tests/schematic.rs` runs this. Three real projects cross-validate **exactly**
(zero split nets, zero merged nets, full component-set agreement):

| Project | KiCad | Hierarchy | Result |
|---|---|---|---|
| `pic_programmer` | 10 (20260101) | 2 sheets | 63/63 components, exact partition |
| `ecc83-pp` | 9 (20250114) | 2 sheets | 15/15 components, exact partition |
| `complex_hierarchy` | 9 (20250114) | reused sub-sheet ×2 | 68/68 components, exact partition |

`complex_hierarchy` is the strongest case: the same sub-sheet instantiated
twice must expand to distinct designators yet identical topology, which
exercises per-instance reference resolution.

The `sch_diag` example is the tool behind these tests:

```
cargo run -p galvani-extract --example sch_diag -- \
    kicad-demos-src/demos/pic_programmer pic_programmer
# SCH: 63 comps, 111 nets, 34 multi-pin nets
# PCB: 63 comps, 111 nets, 34 multi-pin nets
# PCB nets that split in SCH: 0
# SCH nets that merge PCB nets: 0
```

`DETAIL=1` / `MERGE=1` print the offending nets when a board does not match.

## KiCad version coverage

The s-expression `.kicad_sch` format (KiCad 6 through 10) is supported. The
validated corpus spans version stamps 20250114 (KiCad 9) through 20260101
(KiCad 10); the format is stable across this range and the parser keys off
structure, not the version number.

**KiCad 5 legacy `.sch`** is a different, non-s-expression format (the
`stormduino` board is the corpus example). It is **not** parsed by this module.
That board's layout is still handled by the PCB extractor; only its legacy
schematic is out of scope. Adding the legacy parser is future work.

## Known limitations

- **Buses are not expanded.** Bus wires, bus entries and bus labels
  (`DATA[0..7]`) are parsed structurally but bus *membership* is not resolved,
  so a bus net appears split into its individual members rather than unified.
  This only ever *under*-connects (we never wrongly merge nets); the
  `interf_u`, `video` and `kit-dev-coldfire` corpus boards use buses heavily
  and consequently do not match exactly. `tests/schematic.rs` asserts that
  `interf_u` still never over-merges. Bus expansion is the main remaining work.
- **Net-tie footprints** (two pads tied only in copper) have no schematic
  counterpart; a board relying on them would show a split that is correct for
  the schematic. None of the exactly-validated projects use them.
- **Legacy KiCad 5 `.sch`** as above.
