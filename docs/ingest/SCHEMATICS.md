# Schematic extraction (`.kicad_sch`)

Hauksbee can simulate a board from its **schematic** alone, before any layout
exists. This makes early-stage testing possible: you draw the circuit in
eeschema, and Hauksbee derives the same netlist eeschema would, binds models,
and runs the solver. No copper is required.

The code lives in `hauksbee-extract/src/schematic.rs` and produces the same
[`ExtractedBoard`] every other extractor produces, so the binder, solver, MCU
co-sim and stress monitor stay unchanged. The only thing absent from a
schematic-derived board is geometry-dependent physics (copper parasitics, pad
positions). Component positions for rendering come from the schematic symbol
placements instead.

## Why a layout is easy and a schematic is not

A `.kicad_pcb` hands us connectivity for free: every pad already carries its
net number. A schematic does not store nets at all. eeschema *computes* them
geometrically every time, and so must we. The derivation runs in stages:
geometry, then names, then hierarchy, then wire incidence and implicit power,
with bus expansion layered on top.

### 1. Geometric connectivity

Every symbol pin has a connection point in absolute schematic coordinates:
the pin's position in its library symbol, transformed by the symbol
instance's placement (translate, rotate, mirror). Wires, junctions,
no-connects, labels and sheet pins all live at absolute coordinates too.
Anything sharing a coordinate is electrically one node. We snap coordinates
to a 0.001 mm grid (far finer than KiCad's 1.27 mm placement grid, fine
enough to absorb textual rounding) and union-find every coincidence:

- a wire's two endpoints are unioned;
- a pin, a junction, or a label endpoint touching a wire endpoint joins that
  wire's component.

### 2. Coordinate transform (the subtle part)

Two conventions have to be exactly right, or the netlist comes out
plausible-but-wrong:

- **Library symbols are drawn y-up; the schematic canvas is y-down.** We
  negate the pin's local y before placement. Skip this, and every vertical
  two-pin part (resistor, capacitor, diode) silently swaps its two pins.
- **The placement angle rotates counter-clockwise on screen**, which on a
  y-down canvas is the matrix `(x cosθ + y sinθ, −x sinθ + y cosθ)`.

`mirror x` negates y, `mirror y` negates x, applied *after* rotation in the
placed frame, matching eeschema's transform order (rotate the symbol, then
flip it). The order matters only when a symbol is both rotated and mirrored:
apply the mirror first and the two pins of such a part swap (e.g. a resistor
placed at rot 90 + mirror x), which again yields a plausible-but-wrong
netlist.

### 3. Named unification

Names then merge nodes that never touch geometrically:

- **Local labels** unify nets carrying the same text *within one sheet*.
- **Global labels** and **power symbols** unify across the whole design. A
  power symbol names its net after its `Value` (`GND`, `+5V`, `VPP`), but
  only when its pin is `power_in`. A power symbol with a `power_out` pin is
  an ERC flag (`PWR_FLAG`): it marks a net as driven and must **not** name
  it, or every flagged net collapses into one.
- A symbol counts as a power source when its library symbol carries the
  `(power)` flag *or* its reference uses the canonical `#PWR` prefix (some
  older library symbols, e.g. a hand-drawn `VPP`, omit the flag but are
  still power symbols).
- Net names are normalised the way KiCad does: a literal `/` inside a label
  is escaped as `{slash}`, so `VPP/MCLR` and `VPP{slash}MCLR` are the same
  net and must compare equal.

### 4. Hierarchy

We resolve sub-sheets by file (the `Sheetfile` property), recursing into each
child. A child's `(hierarchical_label "X")` is the same net as the parent
`(sheet (pin "X"))` it sits behind. We record both sides keyed by the sheet
instance path and union them.

A **reused** sub-sheet (the same file instantiated on several pages) is the
reason symbols carry an `(instances (project .. (path "/uuid/uuid"
(reference "R201"))))` block. Each instantiation gets its own designators.
We expand every instance separately (its own geometry scope, its own
references resolved from the path), so `ampli_ht` instantiated twice becomes
RV201… and RV301… with identical topology, exactly matching KiCad's
behaviour.

Multi-unit parts (a quad gate drawn as four symbols sharing one reference)
fold into one component with the union of every unit's pins, deduping the
common unit-0 pins (VCC/GND) that appear on every gate.

### 5. Wire incidence (anything on a wire is on the net)

KiCad joins anything lying *along* a wire, not only at its two endpoints: a
net label or a pin placed mid-span on a wire is electrically on that wire.
This matters: in real boards almost every net label sits mid-span (eeschema
drops the label a grid step inside the wire), so without an incidence pass
nearly every label would float free of its wire and the netlist would
shatter into single-pin fragments. After the geometric pass we union every
anchored point that lies strictly inside a wire segment into that segment.
Bus segments are skipped (see below).

### 6. Implicit power pins

A logic chip's GND/VCC pins are usually drawn as *hidden* `power_in` pins
with no wire. KiCad auto-connects each such pin to the global power net named
after the pin's own name (`GND`, `VCC`, `+3.3V`). We do the same: a hidden
`power_in` pin on an ordinary device imposes its pin name as a global net.
Visible `power_in` pins are wired normally and are not auto-named (two
chips' visible supply pins sharing a pin name must not merge through the
name alone).

### Buses

A bus is a thick wire that carries several member nets at once. Crucially, a
bus is **electrically cosmetic**: members travel by *name*, not through the
bus geometry. KiCad will not let you connect a pin to a bus without a label,
and a wire entering a bus through a 45-degree *bus entry* carries its own
member label (`D0`, `PC-A7`). So on a single sheet, bus members unify exactly
like ordinary local labels, and the bus, its entries and its bus label add no
connectivity of their own. We therefore record bus segments and entries but
give them no union-find edges; if they carried edges, every member landing
on the bus would short into one net.

Bus *labels* and *pins* are still parsed and expanded into their members:

- **Vector**: `D[0..7]` → `D0`, `D1`, … `D7` (ascending or descending; the
  prefix keeps any punctuation, so `IRQ-[1..7]` → `IRQ-1` … `IRQ-7`).
- **Group**: `USB{DP DM}` → `USB.DP`, `USB.DM` (named group members are
  qualified with the prefix and a dot); an anonymous group `{A B[0..1]}` →
  `A`, `B0`, `B1`. Vector tokens inside a group expand in place.

Expansion matters at two places where connectivity actually crosses:

- **Hierarchical sheet pins.** A bus passing through a
  `(sheet (pin "ADDR[0..7]"))` connects member-wise to the child's
  `(hierarchical_label "ADDR[0..7]")`: each member `ADDR0`…`ADDR7` crosses
  the boundary on its own, never the literal `ADDR[0..7]`.
- **A bus crossing a sheet pin under a different name.** A bus labelled
  `DQ[0..31]` can feed a sheet pin named `DPC[0..31]`; KiCad maps the
  members *positionally by index* (`DQ7` ↔ `DPC7`). We resolve this by
  finding the bus the pin sits on (a flood over the sheet's bus segments to
  the bus's label) and pairing the bus's member `i` with the pin's member
  `i`.

Bus aliases (`(bus_alias "NAME" (members …))`) are parsed, recorded per
sheet, and **expanded when referenced**: a group bus `MEM{ADDR}` whose
`ADDR` token is a bus alias expands to the alias's member list (each member
itself expanded, so an alias member can be a vector like `A[7..0]`),
qualified by the group prefix.

### Net naming

Named nets keep their label/power name. Unnamed nets get KiCad's
`Net-(Ref-PadN)` form after their lowest-sorted member pin. (KiCad's exact
choice of which pin names an anonymous net depends on annotation order, so
for cross-validation we compare the *partition structure*, not the names.)

## Validation

The decisive test is cross-validation against the layout. For a corpus
project shipping both a `.kicad_sch` hierarchy and a `.kicad_pcb`, the
netlist we derive from the schematic must induce the **same partition** of
component pins into nets as the layout does. Net names and ids differ; the
partition must match exactly over the pins the two share.

`tests/schematic.rs` runs this. Six real projects cross-validate **exactly**
(zero split nets, zero merged nets, full component-set agreement over the
shared pins; the PCB may carry mounting holes / fiducials the schematic
never models):

| Project | KiCad | Hierarchy | What it exercises | Result |
|---|---|---|---|---|
| `pic_programmer` | 10 (20260101) | 2 sheets | baseline hierarchy | 63/63 comps, 34 multi-pin nets |
| `ecc83-pp` | 9 (20250114) | 2 sheets | tube preamp | 15/15 comps, 9 multi-pin nets |
| `complex_hierarchy` | 9 (20250114) | reused sub-sheet ×2 | per-instance references | 68/68 comps, 50 multi-pin nets |
| `interf_u` | 9 (20250114) | single sheet | vector buses, bus entries, hidden power pins | 24 comps, 110 multi-pin nets |
| `kit-dev-coldfire` | 9 (20250114) | multi-sheet | bus sheet pins, parent-side named parts, rot+mirror placement | 160/160 comps, 209 multi-pin nets |
| `video` | 9 (20250114) | multi-sheet | buses across sheet pins, incl. cross-named (`DQ` to `DPC`) positional mapping | 189/189 comps, 371 multi-pin nets |

`complex_hierarchy` is the strongest hierarchy case: the same sub-sheet
instantiated twice must expand to distinct designators yet identical
topology. `video` is the strongest bus case: a 32-bit data bus crosses a
sheet boundary under a different name and must map member-wise by index.

The `sch_diag` example is the tool behind these tests:

```
cargo run -p hauksbee-extract --example sch_diag -- \
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
(KiCad 10). The format stays stable across this range, and the parser keys
off structure, not the version number.

**KiCad 5 legacy `.sch`** is a different, non-s-expression format (the
`stormduino` board is the corpus example). This module does **not** parse
it. That board's layout is still handled by the PCB extractor; only its
legacy schematic is out of scope. Adding the legacy parser is future work.

## Known limitations

- **Bus aliases referenced as `{ALIAS}` are expanded.** A `(bus_alias …)`
  definition is recorded per sheet and substituted for its member list when
  a group bus references it (`MEM{ADDR}`). The `bus_alias_top` /
  `bus_alias_child` fixture pair (a `MEM{ADDR}` reference crossing a sheet
  boundary) covers this end-to-end, along with expander unit tests. No
  *corpus* board uses an alias reference, so the corpus cross-validations do
  not exercise this path; the synthetic fixture does, and would fail if the
  alias were left unexpanded.
- **Group-bus member qualification** follows KiCad's `PREFIX.member` form
  for named groups. The corpus exercises only vector buses, so unit tests on
  the expander and the `bus_alias_*` fixtures (`MEM{ADDR}` -> `MEM.A1`,
  `MEM.A0`) cover group qualification, not a corpus board.
- **Net-tie footprints** (two pads tied only in copper) have no schematic
  counterpart; a board relying on them would show a split that is correct
  for the schematic. None of the exactly-validated projects use them.
- **Legacy KiCad 5 `.sch`** as above.
