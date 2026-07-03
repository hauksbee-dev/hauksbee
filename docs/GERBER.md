# Gerber + Pick-and-Place Reverse Extraction

Hauksbee normally extracts a board from native CAD (a `.kicad_pcb` carries every
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
   (R-tree-pruned union-find, with a shape/distance model that mirrors `drc.rs`
   — a parallel copy with matched numerics, since the gerber path needs ops the
   DRC didn't expose), plated holes stitch the layers, and each placed component
   claims the flashes nearest it as pads tagged with their net.

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
  outline** **or** a pad's copper genuinely **penetrates** the boundary. This is
  the single most important correctness rule in the module. It is exact **for the
  keyholed single-region pour** KiCad and Allegro emit (the antipad is part of
  the region winding, so even-odd puts a pocketed pad outside). It does *not*
  hold if a fab emits the pour as a dark region plus a *separate* clear (LPC)
  antipad — we skip clear polarity (see Limitations), so such a pad would read as
  inside the fill and could false-short. KiCad/Allegro don't do that; some tools
  can.
- **GND heuristic**: the largest pour-touching net is labelled `GND`. This is a
  **label only** (connectivity is unaffected), and it is a guess: a board with a
  power plane larger than its ground plane, or a split ground, can mislabel.
  Downstream code should not trust the `GND` *name* as ground-truth.

### Component binding

Each placed component sits at a known `(x, y)`. Every flash is assigned to the
*nearest* placed component whose footprint window contains it (flash-centric, so
a flash is never double-claimed and no component is starved). The window size is
inferred from the package name: chip codes (0402/0603/0805…), IC families
(SOIC/QFN/QFP/SOT…), and pin-header grids (`PinHeader_2x18_P2.54mm`). A package
name that matches no known token falls back to a flat 4.0 mm window, so a
blank or unrecognised package field degrades pad assignment for that part.
Coincident flashes at one location (a through-hole pad's F.Cu + B.Cu rings + its
drill discs) collapse to one pad. Synthesised drill discs are tagged `Via` and
never counted as component pads.

## Closed-loop validation (the honesty gate)

Before any real-world claim, the reconstruction is validated against ground
truth: take corpus KiCad boards, export their gerbers + drill + P&P with
`kicad-cli pcb export gerbers --no-x2 --no-netlist` (so the gerbers carry **no
net hints** — the reconstruction must rederive connectivity from copper geometry
alone, exactly as on a third-party board), reverse-extract, and compare to the
native KiCad extraction.

The metric is **net-partition equivalence over component pads**: net *names*
differ (we invent `NET_n`), so we match pads by board position (0.1 mm cells)
and check that every pair of matched pads is grouped the same way (same-net vs
different-net) in both extractions. 100% means the recovered electrical graph,
**over the pads we located**, is identical.

**Read the two columns together.** The partition % is computed *only over pads
the reconstruction located* (the "located" column). Native pads the
reconstruction did not place — bottom-side parts the P&P marks, pads the
footprint window missed — are excluded from the percentage, so a low located
count caps how much the % actually proves. The located fraction is therefore
gated alongside the partition %; both are asserted in
`tests/gerber_closedloop.rs`. Every row below is a corpus-gated regression test
(RP2040 is its own tight test; the rest are the sweep). The gated floor is in
parentheses where it is looser than the observed value, so you can see exactly
what the CI guarantees vs what a representative run produced.

| Board | Layers | Native nets / comps | Net partition (gate) | Located (gate) | Time |
|-------|--------|---------------------|----------------------|----------------|------|
| reform OLED | 2 | 14 / 14 | 100.0% (≥99) | 38/40 = 95% (≥85) | ~7 ms |
| LumenPnP ring-light | 2 | 14 / 36 | 100.0% (≥99) | 57/66 = 86% (≥80) | ~44 ms |
| Watchy | 2 | 85 / 86 | 100.0% (≥99) | 271/312 = 87% (≥80) | ~24 ms |
| RP2040 minimal | 2 | 53 / 34 | 99.6% (≥99) | 216/225 = 96% | ~0.2 s |
| reform trackball2 | 2 | 65 / 67 | 99.7% (≥99) | 200/241 = 83% (≥75) | ~0.4 s |
| Corne (crkbd) | 2 | 159 / 180 | 99.7% (≥99) | 476/950 = 50% (≥45) | ~0.4 s |
| Lily58 Pro V2 | 2 | 241 / 312 | 99.0% (≥98.5) | 716/1246 = 57% (≥50) | ~6 s |
| reform motherboard | 6 | 682 / 529 | 99.8% (≥99) | 2044/2276 = 90% (≥85) | ~15–41 s |

Over the located pads, net-partition agreement is 99.0–100% on every board: the
recovered electrical graph is essentially identical where we placed a pad. The
sub-100% rows are a handful of pads on a tiny isolated stub (e.g. one RP2040 GND
pad whose copper the export rounds just out of touch).

The **located fraction varies a lot** and is the honest weak spot. It is high
(83–96%) on most boards but only ~50% on the dense keyboards (Corne, Lily58),
where many pads are bottom-side or sit on footprints the package-name window
under-covers. Those *missing* pads are not scored by the partition %, so the
two columns must be read together: "99.7% on Corne" is "99.7% over the 50% of
pads we located", not "99.7% of the board".

Timings are a representative run on one laptop (Apple Silicon), not a guaranteed
bound; the reform motherboard varies 15–41 s with machine load.

**What the closed loop does and does not cover.** The ground-truth gerbers are
exported with `--no-protel-ext`, so they carry KiCad long layer names
(`*-F_Cu.gbr`). The sweep therefore validates the **connectivity engine** (the
union-find, pour rule, via stitching, pad assignment) on the strongest layer-
classification path. It does *not* stress the Protel-extension or Allegro
`.art`-digit layer inference, nor the gerber-format-drill reader — those paths
are exercised only by the uConsole, which has no ground truth to score against.
So "near-exact connectivity" is proven for the engine; the exotic-dialect
*ingestion* paths are proven only to parse and bind, not to a measured partition
accuracy. The "comps" column in the table is the *native* count, not how many
the reconstruction recovered (component recovery is materially lower than net
agreement — see Limitations).

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

Reverse-extracted (`cargo run --example gerber_report -- <dir>`; figures below
are a representative run — `tests/gerber_uconsole.rs` asserts floors, not these
exact values, since there is no ground truth to compare against):

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

A famous board with no CAD becomes a netlist hauksbee can analyse. Note there is
no native CAD to validate against here, so unlike the closed-loop boards these
numbers are *internal consistency* (it parsed, it bound, ground is the biggest
net), not a measured agreement. The closed-loop boards are what prove
correctness; the uConsole proves the pipeline runs end-to-end on a real
fab-only board in an unfamiliar dialect.

### What degrades without each input

- **No pick-and-place**: nets and geometry (DRC) still reconstruct from copper
  alone, but components cannot be bound — there is nothing to say which pads form
  which part. `from_gerber_dir` returns the nets with zero components. (The
  Inkplate 6 gerber set is exactly this case; see docs/record/FAMOUS_SWEEP.md Round 5.)
- **No BOM**: components still bind; their value/part-number is only the P&P
  `Val`/`Package` field rather than an enriched MPN.
- **No drill**: single-layer boards are fine; on multi-layer boards each layer's
  copper fragments into separate nets without via stitching.

## Per-net copper geometry (the trace-current surface)

The reconstruction surfaces, per reconstructed net, the copper geometry a
trace-current check needs: `ReconStats::net_copper` is a `Vec<GerberNetCopper>`
giving each net's narrowest drawn-track width (the series bottleneck), widest
width, track/region counts, and a `GerberCopperKind` (`Traces` / `Poured` /
`None`). A drawn track is a finite-width capsule (`width = 2*r`), so **copper
width is exact from the manufacturing files** (the one quantity gerbers give more
directly than a netlist). A net carrying any pour region is `Poured` and never
given a discrete width (a plane's true cross-section is not a segment width),
mirroring the native-CAD `trace_current` `Poured` exemption exactly. The probe is
`cargo run -p hauksbee-extract --example gerber_trace_current -- <dir>`.

This makes the IPC-2221 trace-current surface runnable on a gerber-only board.
Its reach is honest: it needs a *cited current attributed to a net*, and gerber
reconstruction recovers no net names or BOM-bound identity, so it runs but finds
nothing unless a current can be tied to a specific reconstructed net. And a board
whose fab draws traces as G36/G37 filled regions (some Altium exports, e.g. the
Inkplate 6) reads every net as `Poured`, so the check is inert there, the safe
failure direction (a `Poured` net is never flagged). See docs/record/FAMOUS_SWEEP.md Round 5.

## Excellon dialects

The drill reader handles the KiCad/decimal form and the **Altium dialect** the
Inkplate 6 ships: `;FILE_FORMAT=2:5` (integer:decimal), `INCH,LZ`,
`T<idx>F..S..C<dia>` tool defs (feed/speed before the diameter), **modal
single-axis coordinate lines** (`X..` keeps the last Y, `Y..` keeps the last X),
and `;TYPE=PLATED` / `;TYPE=NON_PLATED` sections (NPTH tools dropped from
connectivity). The Altium drill is named `<board>-RoundHoles.TXT` /
`-RectHoles.TXT` / `-SlotHoles.TXT`, recognised by the `holes` token. Before
these were handled the Inkplate drill parsed to zero holes; tests in
`gerber::excellon` and `gerber_inkplate.rs`.

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

- **The closed-loop % only scores located pads.** As the validation section
  stresses, net-partition agreement is computed over pads the reconstruction
  placed, which ranges from ~50% (dense keyboards) to ~96%. Pads we fail to
  locate (bottom-side parts, footprints the package-name window under-covers) are
  not counted against the percentage. The located fraction is gated separately so
  a regression that *loses* pads can't hide behind a high partition %, but the
  headline number is "agreement over what we found", not "of the whole board".
- **Footprint inference is approximate.** The pad-assignment window is inferred
  from the package-name string, with a flat 4.0 mm fallback for unrecognised
  names. This both *inflates* some pad counts (a stitching via inside the window
  is hard to tell from a real pad without the netlist, so a crystal may report
  its GND vias as extra pads — on the correct net, but over-counted) and *misses*
  pads (a part whose pads fall outside an under-sized window). Component-name
  match in the closed loop is therefore well below net agreement (e.g. Corne
  82/180); the recovered *connectivity* is what the simulator needs, and that is
  near-exact over located pads.
- **Aperture-macro coverage** is the common primitives (circle, center/vector
  line, outline, polygon) with `$n` substitution and basic arithmetic. Moiré and
  thermal primitives (fiducials/reliefs, not pads) are skipped; a macro using a
  variable expression we cannot evaluate falls back to a small disc so the flash
  still anchors a pad. Concave macros are over-approximated by their convex hull.
- **Pour fidelity**: a deliberately split pour drawn as one dark fill is read as
  connected within that fill. KiCad emits separate dark regions per island, so
  this is rarely wrong in practice; an exotic single-region split-plane could
  mis-merge.
- **Gerber-format drill diameter is partly guessed.** When a hole on a gerber
  drill film is flashed with a non-circular aperture, its barrel diameter falls
  back to 0.3 mm (a circular flash gives the true size). This feeds via stitching
  on exactly the multi-layer Allegro boards (uConsole) that have no ground truth,
  so it is an unverified assumption on that path.
- **Clear polarity (LPC)** is skipped for connectivity: a thermal relief or
  antipad clearing copper inside a pour does not disconnect a net the way it
  changes a rendered image, so treating the board as additive is correct for
  connectivity but means a net split *only* by a clear cut-out would be missed.
- **Inner-layer order is inferred from filename digits** (`gnd02` → stack index
  2). An Allegro-style plane named without a stack number collapses to a single
  default inner slot, so on a board that names its inner planes ambiguously the
  layer order — and therefore via stitching across the wrong pair — can be wrong.
  The mapping-file escape hatch (`copper:<index>`) overrides this when it matters.
- **GND label is a heuristic** (largest pour-touching net), so the `GND` *name*
  can be wrong on a power-plane-dominant or split-ground board. Connectivity is
  unaffected; only the label is a guess.
- **The bundled `.zip` reader flattens by file name.** Two entries with the same
  basename in different sub-folders of a job zip would overwrite each other. Job
  zips are flat in practice, but a deeply-nested archive is better extracted
  manually and pointed at as a directory.
- **kicad-cli round-trip**: KiCad-10-dev-format demo boards (pic_programmer,
  stickhub) cannot be exported by the installed kicad-cli 9.x to make ground
  truth, so they are skipped (not failed) in the closed loop.
