# Gerber + Pick-and-Place Reverse Extraction

Galvani normally extracts a board from native CAD (a `.kicad_pcb` carries every
pad's net; see `docs/SCHEMATICS.md`). But a large tier of real hardware ships
only *manufacturing* files: RS-274X copper gerbers, an Excellon (or gerber)
drill, a pick-and-place CSV, sometimes a BOM, and no CAD at all. The uConsole,
Inkplate, and a long tail of famous boards live here.

This module reconstructs an `ExtractedBoard` (the same nets + components + pads
the rest of the engine consumes) from those fab files alone, so bind, DRC, lint,
stress and simulation work on boards that otherwise could not be ingested.

Entry point: `ExtractedBoard::from_gerber(path)` — a job directory or a `.zip`.
The richer `gerber::from_gerber_dir` returns reconstruction stats alongside the
board.

## How it works

```
job dir ──classify──▶ copper layers, drill, pick-and-place, (BOM)
   │                        │
   │                   parse each
   │        ┌───────────────┼────────────────┬─────────────┐
   ▼        ▼               ▼                ▼             ▼
 mapping  RS-274X        Excellon /        P&P CSV /      BOM CSV
 file     copper →       gerber drill →    Allegro loc →  (enrich)
 (opt)    primitives     plated holes      placements
                    │          │               │
                    ▼          ▼               ▼
           ┌──────────────────────────────────────────┐
           │  connect: R-tree union-find connectivity  │
           │   • copper that touches = one conductor   │
           │   • plated holes stitch layers (vias/PTH) │
           │   • pour membership = containment         │
           │   • components claim nearby flashes       │
           └──────────────────────────────────────────┘
                              ▼
                        ExtractedBoard
```

1. **Classify** every file (recursing sub-dirs) by name into copper / drill /
   outline / ignore. A `layer_map.txt` or `*.map` file overrides the guess.
2. **Parse** copper layers into solid primitives (capsules and polygons in board
   mm), the drill into plated holes, the pick-and-place into placements.
3. **Reconstruct** connectivity geometrically: copper that touches is one net
   (R-tree-pruned union-find, reusing the `drc.rs` shape model), plated holes
   stitch the layers, and each placed component claims the flashes nearest it as
   pads tagged with their net.

## Parsing (what we read)

We adapt the battle-tested **`gerber_parser`** crate (RS-274X → the
`gerber-types` model) rather than hand-rolling the grammar; aperture macros,
coordinate-format scaling, polarity and the deprecated codes are its problem.
The Excellon reader is hand-rolled (no mature Rust crate exists; the format is a
simple tool-table + coordinates).

| Input | Reader | Notes |
|-------|--------|-------|
| RS-274X copper | `gerber_parser` + `rs274x` plotter | apertures (circle/rect/obround/poly/**macro**), draws (linear + arc), flashes, regions (G36/G37), polarity |
| Aperture macros (AM) | `macros` | circle / center-line / vector-line / outline / polygon primitives, `$n` variable substitution, a small `+ - x /` arithmetic evaluator; the solid area is the convex hull of the union |
| Excellon drill | `excellon` | tool table, metric/inch, leading/trailing-zero suppression, plated vs NPTH from the file's `TF.FileFunction` or the file name |
| Gerber-format drill | `rs274x` | some tools (Allegro) draw holes as flashes on a gerber film; flash centres become hole locations |
| Pick-and-place CSV | `placement::parse_pnp` | tolerant column matching: KiCad `Ref,Val,Package,PosX,PosY,Rot,Side`, JLCPCB `Designator,Mid X,Mid Y,Layer,Rotation`, Altium `Center-X(mm)…`; mm / mil / inch units |
| Allegro location file | `placement::parse_allegro_loc` | `smt_loc.txt`: `!`-delimited, mils, `mirror` flag for bottom side |
| BOM CSV | `placement::parse_bom` | `Designator/Comment/MPN` or `Reference(s)/Value`; one row may list many refs |

### RS-274X dialect normalisation

Real fab gerbers deviate from the textbook form in ways the upstream parser
rejects outright — which would silently drop a whole layer. Before parsing we
normalise two things (well-formed KiCad/JLCPCB gerbers pass through untouched):

- **Multi-statement extended blocks**: one `%...%` may pack several statements,
  e.g. `%FSAX55Y55*MOIN*%`. We split each into its own `%...*%`.
- **FS without a zero-omission char**: Allegro writes `%FSAX55Y55*` (absolute,
  5.5) with no leading `L`/`T`. Coordinates in these files are zero-padded to
  full width, so inserting `L` is exact.

## Layer-role inference

Filenames are the only clue to what each gerber is. We recognise the common
conventions and fall back to an explicit mapping file:

- **KiCad long names**: `*-F_Cu.gbr`, `*-B_Cu.gbr`, `*-In1_Cu.gbr`.
- **Protel / Altium extensions**: `.GTL`/`.GBL` (top/bottom), `.G1L`/`.G2L`…
  (inner), `.GTP`/`.GBP` (paste), `.GTO`/`.GBO` (silk), `.GTS`/`.GBS` (mask),
  `.GKO`/`.GM1` (outline), `.TXT`/`.DRL` (drill).
- **Allegro `.art`**: role-named films `top` / `bottom` / `gnd02` / `pwr04` /
  `gnd05` (the digits are the stack position), plus the gerber-format drill.
- **Generic words**: a name containing `top`+`copper`, `bottom`+`cu`, `inner`,
  `signal`, etc.
- **Mapping file** (`layer_map.txt` / `*.map`): `filename = copper:<index>` /
  `copper:bottom` / `drill` / `outline` / `ignore`, one per line.

Inner-layer indices are provisional until the whole set is seen, then densified
into a top-to-bottom `0..n` stack order.

## Connectivity reconstruction

Two pieces of copper that touch are the same conductor. Every primitive on a
layer is indexed in an `rstar` R*-tree (the same prune `drc.rs` uses for its
O(n) short sweep) and any pair whose signed copper gap is `<= eps` is unioned
(disjoint-set). Connected components are the nets, named `NET_n`.

- **Vias / through-holes**: each plated drill becomes a disc on every copper
  layer and unions those layers' copper, stitching the stack.
- **Copper pours (the hard part)**: a pour ships as a *single keyholed outline*
  with antipads and thermal reliefs baked in (no clear-polarity cut-outs). Edge
  proximity to that weaving boundary would falsely short every net the pour
  surrounds; pure centre-containment would drop thermal-relief pads the pour laps
  onto. So a primitive joins a pour when a **sample point is inside the filled
  outline** (the even-odd test correctly puts antipad-pocketed pads *outside*)
  **or** a pad's copper genuinely **penetrates** the boundary. This is the
  single most important correctness rule in the module.
- **GND heuristic**: the largest pour-touching net is labelled `GND` (a label
  only; connectivity is unaffected).

### Component binding

Each placed component sits at a known `(x, y)`. Every flash is assigned to the
*nearest* placed component whose footprint window contains it (flash-centric, so
a flash is never double-claimed and no component is starved). The window size is
inferred from the package name: chip codes (0402/0603/0805…), IC families
(SOIC/QFN/QFP/SOT…), and pin-header grids (`PinHeader_2x18_P2.54mm`). Coincident
flashes at one location (a through-hole pad's F.Cu + B.Cu rings + its drill
discs) collapse to one pad. Synthesised drill discs are tagged `Via` and never
counted as component pads.

## Closed-loop validation (the honesty gate)

Before any real-world claim, the reconstruction is validated against ground
truth: take corpus KiCad boards, export their gerbers + drill + P&P with
`kicad-cli pcb export gerbers --no-x2 --no-netlist` (so the gerbers carry **no
net hints** — the reconstruction must rederive connectivity from copper geometry
alone, exactly as on a third-party board), reverse-extract, and compare to the
native KiCad extraction.

The metric is **net-partition equivalence over component pads**: net *names*
differ (we invent `NET_n`), so we match pads by board position and check that
every pair of matched pads is grouped the same way (same-net vs different-net) in
both extractions. 100% means the recovered electrical graph is identical.

| Board | Layers | Native nets / comps | Net partition | Pads located | Time |
|-------|--------|---------------------|---------------|--------------|------|
| reform OLED | 2 | 14 / 14 | **100.0%** | 38/40 | 7 ms |
| LumenPnP ring-light | 2 | 14 / 36 | **100.0%** | 57/66 | 44 ms |
| Watchy | 2 | 85 / 86 | **100.0%** | 271/312 | 24 ms |
| RP2040 minimal | 2 | 53 / 34 | 99.6% | 216/225 | 0.2 s |
| reform trackball2 | 2 | 65 / 67 | 99.7% | 200/241 | 0.4 s |
| Corne (crkbd) | 2 | 159 / 180 | 99.7% | 476/950 | 0.4 s |
| Lily58 Pro V2 | 2 | 241 / 312 | 99.0% | 716/1246 | 6 s |
| reform motherboard | 6 | 682 / 529 | **99.8%** | 2044/2276 | 15–41 s |

Net-partition agreement is **99.0–100%** across every board. The
sub-100% boards are a handful of pads on a tiny isolated stub (e.g. one RP2040
GND pad whose copper the export rounds just out of touch) — the electrical graph
is otherwise identical. These run as corpus-gated regression tests
(`tests/gerber_closedloop.rs`, RP2040 gated at 99% as the tight gate; the rest a
sweep floor).

Per the Tarski meta-lesson, every closed-loop disagreement was treated as our
bug and chased to the primitive. The two that mattered: a Y-axis sign convention
(KiCad pcb Y-down vs gerber Y-up) and the pour-containment rule above (which
moved ring-light from 35% to 100%).

## Real-world demo: the uConsole mainboard

`board-corpus/famous/uconsole_gerber/` holds the ClockworkPi uConsole mainboard
(CPI 3.14 Mainboard V5), fetched from `clockworkpi/uConsole`
(`PCB/CPI_3.14_Mainboard_V5_Gerber.7z`). It has **no native CAD** and ships in
Allegro `.art` format — a different gerber dialect, role-named layers, a
gerber-format drill, and an Allegro `!`-delimited pick-and-place in mils. License
is **unconfirmed** (GPL-v3 claimed in forum, not asserted in-repo; see
`SOURCES.md`).

Reverse-extracted (`cargo run --example gerber_report -- <dir>`):

```
copper layers:       5   (top, gnd02, pwr04, gnd05, bottom)
plated holes:        1095
nets reconstructed:  342
components placed:    223
flashes:             6449 total, 3762 assigned to components, 2687 unassigned (vias/test)
GND net detected:    true   (1158 pads — the dominant ground)
components bound:    217/223 (97%) to >= 1 net
extraction time:     ~238 ms
```

A famous board with no CAD becomes a netlist galvani can analyse. This is a
corpus-gated regression test (`tests/gerber_uconsole.rs`).

### What degrades without each input

- **No pick-and-place**: nets and geometry (DRC) still reconstruct from copper
  alone, but components cannot be bound — there is nothing to say which pads form
  which part. `from_gerber_dir` returns the nets with zero components.
- **No BOM**: components still bind; their value/part-number is only the P&P
  `Val`/`Package` field rather than an enriched MPN.
- **No drill**: single-layer boards are fine; on multi-layer boards each layer's
  copper fragments into separate nets without via stitching.

## Performance

The connectivity trace is fully spatial-indexed (R-tree per layer for the touch
sweep; R-tree-pruned region lookups). Pour containment over board-spanning plane
layers (tens of thousands of vertices, tested against every primitive) is
accelerated by `PolyGrid`: a scanline-rasterised inside/outside/boundary grid
built once per large pour, making each query O(1) outside a thin boundary band.

Two-layer boards extract in tens to hundreds of milliseconds. The 6-layer reform
motherboard (≈75k draws and four 35k-vertex plane pours) extracts in ~15–40 s
depending on machine load. The dominant remaining cost is the all-pairs touch
sweep on the densest signal layers.

## Honest limitations

- **Footprint inference inflates some pad counts.** Net partition is near-exact,
  but a *stitching via* sitting inside a component's footprint window is hard to
  distinguish from a real pad without the netlist, so some components report a
  few extra pads (e.g. a crystal's GND vias). The pads are on the correct net;
  only the per-component pad *count* is over.
- **Aperture-macro coverage** is the common primitives (circle, center/vector
  line, outline, polygon) with `$n` substitution and basic arithmetic. Moiré and
  thermal primitives (fiducials/reliefs, not pads) are skipped; a macro using a
  variable expression we cannot evaluate falls back to a small disc so the flash
  still anchors a pad. Concave macros are over-approximated by their convex hull.
- **Pour fidelity**: a deliberately split pour drawn as one dark fill is read as
  connected within that fill. KiCad emits separate dark regions per island, so
  this is rarely wrong in practice; an exotic single-region split-plane could
  mis-merge.
- **Clear polarity (LPC)** is skipped for connectivity: a thermal relief or
  antipad clearing copper inside a pour does not disconnect a net the way it
  changes a rendered image, so treating the board as additive is correct for
  connectivity but means a net split *only* by a clear cut-out would be missed.
- **Component-name match** in the closed loop is lower than net agreement (e.g.
  Corne 82/180) partly from the via over-count above and partly because the
  comparison is strict on pad count; the recovered *connectivity* is what the
  simulator needs, and that is near-exact.
- **kicad-cli round-trip**: KiCad-10-dev-format demo boards (pic_programmer,
  stickhub) cannot be exported by the installed kicad-cli 9.x to make ground
  truth, so they are skipped (not failed) in the closed loop.
