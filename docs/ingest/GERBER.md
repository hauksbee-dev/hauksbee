# Gerber + Pick-and-Place Reverse Extraction

Hauksbee normally extracts a board from native CAD, where every pad already
carries its net: a `.kicad_pcb` or `.brd` layout, a `.kicad_sch` hierarchy
([SCHEMATICS.md](SCHEMATICS.md)), or an Altium `.PcbDoc`
([ALTIUM.md](ALTIUM.md)). But a large tier of real
hardware ships only *manufacturing* files: RS-274X copper gerbers, an
Excellon (or gerber) drill, a pick-and-place CSV, sometimes a BOM, and no
CAD at all. The uConsole, Inkplate, and a long tail of famous boards live
here.

This module reconstructs an `ExtractedBoard` (the same nets + components +
pads the rest of the engine consumes) from those fab files alone, so bind,
DRC, lint, stress and simulation work on boards that otherwise could not be
ingested. The browser's per-object recovery status, honest missing-object
handling, and clickable reconstructed-net explanations are documented in
[Import diagnostics](IMPORT_DIAGNOSTICS.md).

Entry point: `ExtractedBoard::from_gerber(path)`, a job directory or a
`.zip`. The richer `gerber::from_gerber_dir` returns reconstruction stats
alongside the board.

## How it works

![How a gerber job directory is classified (with an optional mapping file overriding the guess), parsed file by file, and stitched back into a connected board by geometric union-find, with the BOM enriching the result](../assets/diagrams/gerber-reconstruction.svg)

1. **Classify** every file (recursing sub-dirs) by name into copper / drill /
   outline / ignore. A `layer_map.txt` or `*.map` file overrides the guess.
2. **Parse** copper layers into solid primitives (capsules and polygons in
   board mm), the drill into plated holes, the pick-and-place into
   placements.
3. **Reconstruct** connectivity geometrically: copper that touches is one
   net (R-tree-pruned union-find, with a shape/distance model that mirrors
   `drc.rs`, a parallel copy with matched numerics, since the gerber path
   needs ops the DRC did not expose), plated holes stitch the layers, and
   each placed component claims the flashes nearest it as pads tagged with
   their net.

## Parsing (what we read)

We adapt the battle-tested **`gerber_parser`** crate (RS-274X to the
`gerber-types` model) rather than hand-rolling the grammar. Aperture macros,
coordinate-format scaling, polarity and the deprecated codes are its
problem. The Excellon reader is hand-rolled (no mature Rust crate exists;
the format is a simple tool-table plus coordinates).

| Input | Reader | Notes |
|-------|--------|-------|
| RS-274X copper | `gerber_parser` + `rs274x` plotter | apertures (circle/rect/obround/poly/**macro**), draws (linear + arc), flashes, regions (G36/G37), polarity |
| Aperture macros (AM) | `macros` | circle / center-line / vector-line / outline / polygon primitives, `$n` variable substitution, a small `+ - x /` arithmetic evaluator; the solid area is the convex hull of the union |
| Excellon drill | `excellon` | tool table, metric/inch, leading/trailing-zero suppression, plated vs NPTH from the file's `TF.FileFunction` or the file name, `G85` canned slots, routed slots (`M15`/`G01`/`M16`), the X2 copper layer pair |
| Gerber-format drill | `rs274x` | some tools (Allegro) draw holes as flashes on a gerber film; flash centres become hole locations |
| Pick-and-place CSV | `placement::parse_pnp` | tolerant column matching: KiCad `Ref,Val,Package,PosX,PosY,Rot,Side`, JLCPCB `Designator,Mid X,Mid Y,Layer,Rotation`, Altium `Center-X(mm)…`; mm / mil / inch units |
| Allegro location file | `placement::parse_allegro_loc` | `smt_loc.txt`: `!`-delimited, mils, `mirror` flag for bottom side |
| BOM CSV | `placement::parse_bom` | `Designator/Comment/MPN` or `Reference(s)/Value`; one row may list many refs |

### RS-274X dialect normalisation

Real fab gerbers deviate from the textbook form in ways the upstream parser
rejects outright, which would silently drop a whole layer. Before parsing we
normalise two things (well-formed KiCad/JLCPCB gerbers pass through
untouched):

- **Multi-statement extended blocks**: one `%...%` may pack several
  statements, e.g. `%FSAX55Y55*MOIN*%`. We split each into its own `%...*%`.
- **FS without a zero-omission char**: Allegro writes `%FSAX55Y55*`
  (absolute, 5.5) with no leading `L`/`T`. Coordinates in these files are
  zero-padded to full width, so inserting `L` is exact.

## Layer-role inference

The fab package's own metadata is consulted before any filename guess. When a
package has no usable metadata, we recognise the common naming conventions and
retain an explicit mapping-file escape hatch:

- **KiCad long names**: `*-F_Cu.gbr`, `*-B_Cu.gbr`, `*-In1_Cu.gbr`.
- **Protel / Altium extensions**: `.GTL`/`.GBL` (top/bottom), `.G1L`/`.G2L`…
  (inner), `.GTP`/`.GBP` (paste), `.GTO`/`.GBO` (silk), `.GTS`/`.GBS`
  (mask), `.GKO`/`.GM1` (outline), `.TXT`/`.DRL` (drill).
- **Allegro `.art`**: role-named films `top` / `bottom` / `gnd02` / `pwr04`
  / `gnd05` (the digits are the stack position), plus the gerber-format
  drill.
- **Generic words**: a name containing `top`+`copper`, `bottom`+`cu`,
  `inner`, `signal`, and so on.
- **Job manifest** (`*.gbrjob`): the exporter's own file list. Each
  `FilesAttributes` entry names a file's role, and a copper film's
  `Copper,L<n>` entry is its physical stack position. When present this
  outranks every name-based rule above (the mapping file still outranks it).
- **Altium extension report** (`*.EXTREP`): a unique extension-to-description
  row classifies that film before its filename (`.GTL = Top Layer`, for
  example). If one extension is reused for several roles, as in named-output
  jobs where every film is `.gbr`, the report cannot identify an individual
  film. Hauksbee says that extension is ambiguous and falls back; it never lets
  the last report row win.
- **Altium layer-pairs export** (`*.LDP`): names each drill file, its
  plated/non-plated set, and the ordered copper layers it reaches. This is the
  span authority before filename inference, including when the drill file has
  an opaque name. Conflicting `.LDP` rows, duplicate package basenames, or a
  disagreement with the drill file's own explicit span refuse rather than
  stitch an invented barrel.
- **Mapping file** (`layer_map.txt` / `*.map`): `filename = copper:<index>`
  / `copper:bottom` / `drill` / `outline` / `ignore`, one per line.

Inner-layer indices stay provisional until we see the whole set, then
densify into a top-to-bottom `0..n` stack order.

## Connectivity reconstruction

Two pieces of copper that touch are the same conductor. Every primitive on a
layer is indexed in an `rstar` R*-tree (the same prune `drc.rs` uses for its
O(n) short sweep), and any pair whose signed copper gap is `<= eps` gets
unioned (disjoint-set). Connected components become the nets, named
`NET_n`.

- **Vias / through-holes**: a plated drill becomes a barrel on each copper
  layer it reaches and unions that layer's copper, stitching the stack. Which
  layers it reaches is a fact the files have to supply; see [Slots,
  castellations and layer spans](#slots-castellations-and-layer-spans).
- **Copper pours (the hard part)**: a pour ships as a *single keyholed
  outline* with antipads and thermal reliefs baked in (no clear-polarity
  cut-outs). Edge proximity to that weaving boundary would falsely short
  every net the pour surrounds; pure centre-containment would drop
  thermal-relief pads the pour laps onto. So a primitive joins a pour when a
  **sample point sits inside the filled outline** **or** a pad's copper
  genuinely **penetrates** the boundary. This is the single most important
  correctness rule in the module. It is exact **for the keyholed
  single-region pour** KiCad and Allegro emit (the antipad is part of the
  region winding, so even-odd puts a pocketed pad outside). It does *not*
  hold on its own if a fab emits the pour as a dark region plus *separate*
  clear (LPC) antipads, which is Altium's default; those voids are cut out
  of the pour first, see [Negative-drawn pours](#negative-drawn-pours-lpc).
- **GND heuristic**: the largest pour-touching net is labelled `GND`. This
  is a **label only** (connectivity is unaffected), and it is a guess: a
  board with a power plane larger than its ground plane, or a split ground,
  can mislabel. Downstream code should not trust the `GND` *name* as
  ground-truth.

### Slots, castellations and layer spans

Three drilled features carry connectivity that a plain "every hole is a
round through-hole" reader gets wrong. Each is recovered from what the files
state, and each has one case where the files do not state enough and the
reader refuses instead of guessing.

**Plated slots.** A slot is a hit with two endpoints, and its plated wall is
a conductor along its whole length, not just at the ends. Two forms are
read: the Excellon canned slot `X<a>Y<b>G85X<c>Y<d>`, and a routed slot,
where `M15` plunges the cutter, each `G01` cuts, and `M16` retracts. The
recovered barrel is a stadium of the tool's diameter swept along the path,
so a pad touching the middle of a slot is connected exactly like a pad at
its end. Both coordinate pairs of a `G85` record go through the same unit
and zero-suppression reader, because the file's inch-or-millimetre choice
applies to the far end as much as the near one.

Rapids matter as much as cuts: a `G00` move positions the cutter with it
raised, so it drills nothing, and a `G01` after `M16` is a move rather than
a cut. Motion codes are modal, so a run of cuts is written `G01` once and
then bare coordinate lines; with the cutter down those are cuts too. A file
with no `M15` in it is not in rout mode at all and keeps its previous
reading, so no existing job changes behaviour. A `G85` record is read before
any of this, because it carries both of its own endpoints and means the same
thing whichever mode the file is in.

A `G02`/`G03` arc cut is tessellated about its `I`/`J` centre. Each chord
lies inside its arc, so the stadium built on it falls short of the true wall
on the outside of the curve and reaches past it on the inside. The step is
therefore chosen per arc, from the radius, to hold that error under a micron
budget rather than at a fixed segment count: a fixed count leaves a 50 mm arc
bulging most of a millimetre off its own wall, which is enough to sweep in
copper the slot never touches. The segment count is capped so a hostile
radius cannot turn one line into unbounded work.

The residual is stated rather than waved away. No chord approximation has
zero error, so the effective contact tolerance against an arc wall is the
union's own epsilon plus the budget, about six microns instead of five.
Copper in that band, closer than six microns to touching the wall without
touching it, is joined by the approximation rather than by the board. Six
microns is a twentieth of the tightest clearance a board is designed to, so
this cannot bridge a gap anyone drew; it can only disagree about contacts
that are already too close to call.

An arc with no readable centre produces no geometry at all: planting a round
hit at the endpoint instead, which is what a fall-through to the plain
coordinate reader would do, invents a hole the file never described. On a
file with no `M15` at all, nothing is being cut, so an arc line moves the
position and records nothing.

The refusal is plating. An **unplated** slot is a mechanical cut with no
copper wall, and it connects nothing, however exactly it overlays two pads.
Plated-ness comes from the file's `TF.FileFunction` attribute or its name,
the same source the round holes use. `gerber_advanced_geometry.rs` carries
the pair: the same copper and the same slot path in two jobs, plated in one
and unplated in the other, and the unplated one must report the extra net.

A gerber-format drill film states its slots differently again: it draws the
finished **cutout**, so a slot arrives as one oblong or rectangular flash
rather than as a path. Both facts in that shape are recovered. The narrow
side is the tool diameter exactly, because a slot is machined by a bit of
that width, and the long axis is the path the bit swept, so the plated wall
is the whole stadium. Reducing the flash to one circle at its centre is what
makes a barrel miss copper the cutout plainly touches: a 4 mm by 1 mm slot
would reach 0.5 mm from its middle instead of the 2 mm it spans. The narrow
direction is measured over the outline's own edges rather than an
axis-aligned box, so a slot drawn at 45 degrees is a slot and not a round
hole of its diagonal's width.

A drawn path on such a film is a rout only when the film declares a rout,
slot or mill role in its own attributes. A suggestive file name is not
enough, and that distinction is the whole safeguard: any board whose project
name happens to contain "slot" would otherwise have its legend art promoted
to conductor. Where the name suggests a rout and the film does not declare
one, the draws are left as artwork and a reader note says what would recover
them.

Plating on a drill film comes from the film, not from a default. The film's
`TF.FileFunction`, its `%TA.AperFunction` drill functions
(`MechanicalDrill` says no copper; `ViaDrill`, `ComponentDrill` and
`CastellatedDrill` say the opposite), its file name, and the job's
plated/non-plated split are read in that order. A film that states plating in
none of them has its hits dropped and its name printed, because guessing
plated invents a net and guessing mechanical deletes one.

**Castellations.** A castellation is a plated half-hole on the board edge:
the outline cuts through the barrel, so the copper ring around it is cut
too. A reader that decides a hole belongs to a pad by testing whether the
hole sits inside a closed ring finds no owner here and drops the connection.
Hauksbee never asks that question. The barrel is copper, and it joins
whatever copper it touches, which is what a castellation physically is, so
its pad and its plated wall are one node with no special case. The count of
plated hits the outline cuts is reported as `ReconStats::n_castellations`,
so the claim is auditable on a real board rather than assumed.

The refusal is again plating: a mechanical edge slot cutting through a pad
is an outline feature and joins nothing, so it is neither counted nor
stitched.

**Blind and buried vias.** A via connects only the layers it spans. The span
comes first from the drill file's X2 `TF.FileFunction` layer pair
(`Plated,1,2,PTH` is layers 1 to 2, not the whole stack), or from the package's
Altium `.LDP` row when the file body is silent. Only when neither speaks does a
file name that encodes the pair supply it (`-L1-L2.drl`, or KiCad's
`-F_Cu-In1_Cu.drl`, resolved against the copper layers this job actually
carries). If the file body and `.LDP` disagree, the hit stitches no layer and
the report names the conflict. Where the copper films carry their own X2
attribute they are read too, because `%TF.FileFunction,Copper,L4,Bot*%` ties a
film to a position in the real stackup without relying on its name.

The refusal is the important part. Treating every drill as a through-hole
merges nets the real stackup keeps apart, which is a phantom short: the
reader inventing a connection nobody designed. Three cases refuse, and each
one is a case where a reading exists that would look fine and be wrong.

- **A declaration we cannot place.** Placing a pair means knowing which of
  the board's layers each of our copper films is, and that is not the same
  question as how many films we found. Films are numbered densely as they are
  classified, so on a job that is missing an inner layer the second film is
  index 1 while being the board's layer 4. Indexing a `1,2` pair straight
  into that numbering shorts the top of the board to the bottom of it, off a
  declaration that said the via stops two layers down.

  Three sources are read together, in this order.

  1. **The film's own X2 attribute.** `%TF.FileFunction,Copper,L4,Bot*%` is
     the film stating its position in the real stackup. When both ends of a
     drill's pair name a film that declared itself, the placement is exact
     whatever else is missing.
  2. **Full depth.** A pair running from layer 1 to the deepest layer
     anything in the job names is a hit through the whole board, and that
     stays true however many films we recognised.
  3. **A complete stack.** When nothing says the board has more layers than
     the films we found, the films are the stack and the 1-based pair indexes
     straight into it.

  Anything else refuses. The board's layer count for step 2 is the largest of
  the three readings: films classified, deepest layer a film declares, and
  deepest layer a drill declares. All three are needed. Taking the drill
  declarations alone is its own fabrication engine: a four-layer job whose
  only drill is a blind `Plated,1,2,PTH` would imply a two-layer board, that
  pair would look full-depth, and the hit would stitch all four layers, a
  phantom short built out of a perfectly correct declaration.

  Taking the films alone is the failure step 2 exists for, and it is not a
  corner case: KiCad names an inner layer's film after the user's label, so a
  six-layer board whose inner layers are called `GND.Cu` and `Power.Cu`
  exports films named `-GND_Cu.gbr` and `-Power_Cu.gbr`, which the filename
  inference does not place in the stack. The MNT Reform motherboard is that
  board, and refusing its `1,6` through-holes on the grounds that "layer 6 is
  not in our stack" cost it 2.5 points of net-partition agreement against
  KiCad before the rule was made exact. A job in that state now says so: a
  note reports how many layers the files describe against how many copper
  films were classified, so the missing copper is visible rather than
  silently absent.
- **Silence on a multi-span job.** A drill file that says nothing is read as
  a through-hole only where nothing else in the job says otherwise. Once a
  sibling declares a partial span, silence is ambiguous and the silent
  file's hits stitch nothing.
- **A name built out of layer names that do not resolve.** A file called
  `-F_Cu-In1_Cu.drl` on a job with no In1 film is telling us its hits are
  blind between two layers, one of which is missing. The layer words are
  found lexically, before any attempt to place them, precisely so that the
  missing one is noticed: resolving first and counting afterwards would read
  the name as mentioning a single layer and fall through to the through-hole
  default. For the same reason a positional reading of `L2` is only offered
  when nothing says the board is deeper than the films we have; on a gapped
  job the film at index 1 may be the board's layer 4, and handing it out
  under the name `L2` is how a blind via ends up joining the two outer
  layers.
- **A name that says blind or buried without saying which layers.** There is
  no reading of that, only a refusal.

Each refusal emits a reader note naming the file and saying what would
recover it. `ReconStats::refused_span_holes` counts the hits and
`ReconStats::notes` carries the text.

That direction is deliberate. A refused stitch under-reports connectivity:
conductors that meet only through those hits come back as separate nets, the
net count is an over-estimate, and the shortfall is stated out loud. A
guessed stitch fabricates connectivity, and nothing downstream can tell it
from a real one.

Measured across the 22 gerber jobs in the corpus, no real board triggers the
refusal: every one either carries a single through-hole drill or declares its
pairs. Slots are recovered on 20 of them (3 to 8 per board) and castellations
on one, the LumenPnP vacuum interposer, where all 6 plated hits sit on the
outline.

| Board | Feature | Recovered | Cross-check |
|-------|---------|-----------|-------------|
| LumenPnP vac interposer | 6 castellated half-holes | 6 of 6 hits on the outline; 6 nets, one per castellation | 100.0% net partition and an exact net count against the native KiCad board (12 nets without the barrels) |
| Olimex ESP32-EVB Rev F | 7 `G85` slots, inch units | 7 slots, both endpoints | 99.7% net partition over 525 of 553 located pads against the native KiCad board |
| Olimex RP2040-PICO-PC | 8 `G85` slots | 8 slots | no ground truth run |
| Watchy, ZSWatch mainboard | 4 slots each | 4 slots | Watchy already gated at 99.0% partition |

Both cross-checked rows are gated in
`advanced_geometry_boards_match_kicad` in
`crates/hauksbee-extract/tests/gerber_closedloop.rs`. The synthesized fixtures,
positive and lookalike-negative for each class, are in
`crates/hauksbee-extract/tests/gerber_advanced_geometry.rs`.

### Component binding

Each placed component sits at a known `(x, y)`. Every flash is assigned to
the *nearest* placed component whose footprint window contains it
(flash-centric, so a flash is never double-claimed and no component starves).
The window size is inferred from the package name: chip codes
(0402/0603/0805…), IC families (SOIC/QFN/QFP/SOT…), and pin-header grids
(`PinHeader_2x18_P2.54mm`). A package name that matches no known token falls
back to a flat 4.0 mm window, so a blank or unrecognised package field
degrades pad assignment for that part. Coincident flashes at one location (a
through-hole pad's F.Cu + B.Cu rings + its drill discs) collapse to one pad.
Synthesised drill discs are tagged `Via` and never counted as component pads.

## Closed-loop validation against native CAD

Before any real-world claim, we validate the reconstruction against ground
truth: take corpus KiCad boards, export their gerbers + drill + P&P with
`kicad-cli pcb export gerbers --no-x2 --no-netlist` (so the gerbers carry
**no net hints**; the reconstruction must rederive connectivity from copper
geometry alone, exactly as on a third-party board), reverse-extract, and
compare to the native KiCad extraction.

The metric is **net-partition equivalence over component pads**: net
*names* differ (we invent `NET_n`), so we match pads by board position (0.1
mm cells) and check that every pair of matched pads groups the same way
(same-net vs different-net) in both extractions. 100% means the recovered
electrical graph, **over the pads we located**, is identical.

**Read the two columns together.** The partition % is computed *only over
pads the reconstruction located* (the "located" column). Native pads the
reconstruction did not place, bottom-side parts the P&P marks, and pads the
footprint window missed are excluded from the percentage, so a low located
count caps how much the % actually proves. The located fraction is
therefore gated alongside the partition %; both are asserted in
`crates/hauksbee-extract/tests/gerber_closedloop.rs`.

Every number below is a **gate floor**, read straight out of that test's board
table. Reproduce them with:

```bash
HAUKSBEE_REQUIRE_CORPUS=1 cargo test -p hauksbee-extract \
    --test gerber_closedloop -- --nocapture
```

| Board | Native nets / comps | Net-partition floor | Located-pad floor | Last measured |
|-------|---------------------|---------------------|-------------------|---------------|
| reform OLED | 13 / 13 | 99.0% | 85% | 100.0% over 38/38 |
| Watchy | 84 / 84 | 99.0% | 80% | 99.7% over 262/276 = 95% |
| LumenPnP ring-light | 13 / 24 | 99.0% | 80% | 100.0% over 63/63 |
| reform trackball2 | 64 / 65 | 99.0% | 75% | 100.0% over 200/220 = 91% |
| Corne (crkbd cherry) | 158 / 178 | 99.0% | 70% | 100.0% over 482/648 = 74% |
| Lily58 Pro V2 | 240 / 310 | 98.5% | 78% | 100.0% over 687/848 = 81% |
| reform motherboard | 681 / 522 | 99.0% | 78% | 100.0% over 1729/2184 = 79% |

RP2040 minimal has its own tighter test (`rp2040_minimal_exact_nets`, floor
99.0% over more than 150 located pads) rather than a row here. The current
KiCad 9.0.3 run recovered 100.0% over 216 of 217 pads.

The SparkFun RP2040 Thing Plus panel supplies a second, exporter-independent
oracle: its Eagle `.brd` and already-published `.GTL`/`.GBL` sit in the same
upstream production folder. `gerber_native_partition.rs` derives placement
probes from the native layout, matches the physical pad centres, and refuses
any reconstructed Gerber net that contains pads from more than one native
Eagle net. The current run covered 2,208 shared pads across 1,236 reconstructed
nets with zero false merges. The package has no drill file, so this is
deliberately a one-sided over-connection gate: missing barrels can split a
native net, and this test does not relabel that known input absence as a clear-
polarity defect.

Which rows a given run actually exercises depends on your corpus. Every board
is fetched by `scripts/fetch-corpus.sh`, but a board the fetch has not
delivered, or one the installed `kicad-cli` cannot load to make ground truth,
is skipped with a printed note rather than failing. `HAUKSBEE_REQUIRE_CORPUS=1`
turns "nothing ran at all" into a failure. The "Last measured" column is what a
full run produced against kicad-cli 9.0.3, the release the floors are
calibrated to (`CALIBRATED_KICAD_CLI` in the test); the test prints both the
calibrated and the running version so a floor breach can be attributed to the
side that moved.

The corpus resolver accepts both the hand-built `famous/<id>/…` layout and the
flat `<id>/…` layout written by `scripts/fetch-corpus.sh`. A gate that cannot
resolve its required inputs says so instead of reporting an empty success.

Layout tolerance at the corpus root is not enough on its own, because two corpora
can also disagree about the path WITHIN a board. The Corne row is the case: the
hand-built corpus holds `crkbd/pcbs/corne-cherry.kicad_pcb`, flattened by hand,
while upstream nests each switch variant a level deeper at
`crkbd/pcbs/corne-cherry/hotswap/corne-cherry.kicad_pcb`. Only the flat path was
named, so on any fetched corpus this row matched nothing and the sweep counted the
shortfall as a skip. Rows that can differ list every candidate path, and
`corpus.toml` pins the one the fetch lands in the entry's `expect` list so a fetch
that stops landing it fails at the fetch rather than turning into a quiet gap here.

Over the located pads, net-partition agreement runs 99.7% to 100.0% on every
board that has ground truth: the recovered electrical graph is essentially
identical where we placed a pad. The sub-100% rows are a handful of pads on tiny
isolated stubs whose copper the gerber export rounds just out of touch.

The **located fraction varies a lot** and is the honest weak spot. It runs
82% to 100% on the reform and Watchy boards but drops to 74% on Corne and 81%
on Lily58, where many pads sit bottom-side or on footprints the package-name
window under-covers. Those *missing* pads are not scored by the partition %, so
read the two columns together: "99.7% on the reform motherboard" means "99.7%
over the 82% of pads we located", not "99.7% of the board".

**What the closed loop does and does not cover.** The ground-truth gerbers
are exported with `--no-protel-ext`, so they carry KiCad long layer names
(`*-F_Cu.gbr`). The sweep therefore validates the **connectivity engine**
(the union-find, pour rule, via stitching, pad assignment) on the strongest
layer-classification path. It does *not* stress the Protel-extension or
Allegro `.art`-digit layer inference, nor the gerber-format-drill reader:
only the uConsole exercises those paths, and it has no ground truth to
score against. So "near-exact connectivity" is proven for the engine; the
exotic-dialect *ingestion* paths are proven only to parse and bind, not to
a measured partition accuracy. The "comps" column in the table is the
*native* count, not how many the reconstruction recovered (component
recovery runs materially lower than net agreement, see Limitations).

### The boards that ship in this repo

The sweep above needs the corpus, which needs a fetch, so it does not run on a
bare clone. `shipped_boards_survive_gerbers` covers the fourteen boards that
ship here, and it runs wherever `kicad-cli` is installed.

It gates two different things, and reading them as one would mislead you.

**Every board must round-trip.** `kicad-cli` loads it, exports gerbers, and the
reconstruction reads them back without error. This is not a formality: it
caught six demo boards that KiCad itself could not open. They carried
Lisp-style `;` comment lines inside the s-expression, which the KiCad format
does not have, so `kicad-cli` answered "Failed to load board". Our own parser
tolerates them, which is why the gap survived until something outside hauksbee
was asked to read the same file. The prose moved to `<board>.notes.md`
sidecars.

**Only routed boards are judged on accuracy.** Most boards here are
pad-and-netlist fixtures with zero segments, vias and zones: their connectivity
lives in the file rather than in copper. A gerber carries copper and nothing
else, so on an unrouted board there is physically nothing to trace, and the
reconstruction can only infer from pad overlap. Those boards land anywhere from
about 40% to 100% net-partition (the smallest two-net fixtures happen to hit
100%; a five-part unrouted board reads 41.8%), and that number measures the
fixture rather than the extractor. None of them carries a floor, deliberately.

Watchy is the one shipped board with a real layout (685 segments, 114 vias, 6
zones), and it is the one carrying a floor:

| Board | Copper | Net-partition floor | Located-pad floor | Last measured |
|-------|--------|---------------------|-------------------|---------------|
| watchy (bundled) | 685 seg / 114 vias / 6 zones | 99.0% | 90% | 100.0% over 262/276 = 95% |

Measured against kicad-cli 9.0.3, the same release the corpus floors are
calibrated to.

Every closed-loop disagreement was treated as our bug and chased to the
primitive rather than tolerated. Two mattered: a Y-axis sign convention (KiCad
pcb Y-down vs gerber Y-up) and the pour-containment rule above, which moved the
ring-light board from 35% to 100%.

## Real-world demo: the uConsole mainboard

The ClockworkPi uConsole mainboard (CPI 3.14 Mainboard V5) comes from
`clockworkpi/uConsole` (`PCB/CPI_3.14_Mainboard_V5_Gerber.7z`), recorded in
`corpus.toml` as board id `uconsole_gerber`. It has **no native CAD** and ships
in Allegro `.art` format, a different gerber dialect, role-named layers, a
gerber-format drill, and an Allegro `!`-delimited pick-and-place in mils.

**This board is optional, and normally absent.** Its licence is unconfirmed
(GPL-v3 claimed in a forum thread, not asserted in the repository), so
`corpus.toml` marks it `license_confirmed = false` and `scripts/fetch-corpus.sh`
skips it unless you pass `--include-unconfirmed` and decide for yourself. On top
There is a second obstacle even with `--include-unconfirmed`: ClockworkPi publishes
the films only inside a `.7z`, which the fetch cannot open, so the entry's `expect`
path names the archive and unpacking it is a manual step. Treat the figures below
as a recorded run on a maintainer's corpus who did that unpacking, not as something
a clone reproduces. `gerber_uconsole.rs` prints `NOT RUN  uConsole mainboard: not in
the default fetch` and never passes quietly.

Reverse-extracted (`cargo run --example gerber_report -- <dir>`; the test
asserts loose floors, not these exact values, since there is no ground truth to
compare against):

```
copper layers:       5   (top, gnd02, pwr04, gnd05, bottom)
plated holes:        1095
nets reconstructed:  342
components placed:    223
flashes:             6449 total, 3762 assigned to components, 2687 unassigned (vias/test)
GND net detected:    true   (1158 pads; the dominant ground)
components bound:    217/223 (97%) to >= 1 net
extraction time:     ~238 ms
```

A famous board with no CAD becomes a netlist hauksbee can analyse. Note that
there is no native CAD to validate against here, so unlike the closed-loop
boards these numbers show *internal consistency* (it parsed, it bound,
ground is the biggest net), not a measured agreement. The closed-loop boards
prove correctness; the uConsole proves the pipeline runs end-to-end on a
real fab-only board in an unfamiliar dialect.

### What degrades without each input

- **No pick-and-place**: nets and geometry (DRC) still reconstruct from
  copper alone, but components cannot be bound; there is nothing to say
  which pads form which part. `from_gerber_dir` returns the nets with zero
  components.
- **No BOM**: components still bind; their value/part-number is only the
  P&P `Val`/`Package` field rather than an enriched MPN.
- **No drill**: single-layer boards are fine; on multi-layer boards each
  layer's copper fragments into separate nets without via stitching.

## Per-net copper geometry (the trace-current surface)

The reconstruction surfaces, per reconstructed net, the copper geometry a
trace-current check needs: `ReconStats::net_copper` is a
`Vec<GerberNetCopper>` giving each net's narrowest drawn-track width (the
series bottleneck), widest width, track/region counts, and a
`GerberCopperKind` (`Traces` / `Poured` / `None`). A drawn track is a
finite-width capsule (`width = 2*r`), so **copper width is exact from the
manufacturing files** (the one quantity gerbers give more directly than a
netlist). A net carrying any pour region is `Poured` and never given a
discrete width (a plane's true cross-section is not a segment width),
mirroring the native-CAD `trace_current` `Poured` exemption exactly. The
probe is `cargo run -p hauksbee-extract --example gerber_trace_current --
<dir>`.

This makes the IPC-2221 trace-current surface runnable on a gerber-only
board. Its reach stays honest: it needs a *cited current attributed to a
net*, and gerber reconstruction recovers no net names or BOM-bound
identity, so it runs but finds nothing unless a current can be tied to a
specific reconstructed net. And a board whose fab draws its traces as
G36/G37 filled regions rather than with draw apertures reads those nets as
`Poured`, so the check goes inert on them, the safe failure direction (a
`Poured` net is never flagged). This is the safe failure direction for
trace-current analysis.

## Negative-drawn pours (LPC)

> **Split planes are reconstructed from the painted image.** A clear gap across
> a negative plane is unioned with the rest of its uninterrupted clear pass and
> subtracted from the earlier copper. If that difference produces two filled
> polygons, they become two region primitives and two conductors. The same rule
> covers a gap that terminates inside a concave pour, an annular antipad, and a
> clear operation over a track or pad. Only clear geometry refused by the
> admission rules below remains conservatively over-connected.

Altium plots a plane *negatively*: one `G36/G37` dark region covering the
whole board, then `%LPC*%` and a few hundred clear regions, one per
clearance, antipad and thermal gap, then `%LPD*%` for whatever islands are
added back. The voids are not decoration. They are the only thing that makes
the film anything other than a solid sheet of copper.

Reading the darks and discarding the clears therefore produced a board-sized
slab on every signal layer. The failure was originally isolated from an Altium
STM32 CAN board and the Inkplate 6 export, but those external designs are
discovery inputs rather than retained native-partition oracles in this
repository. The committed claim is therefore bounded to the distilled
negative-plane fixtures and their exact copper probes; no unavailable external
board's final net count is presented as release evidence.

Admitted clear images are replayed as the Gerber painter operation they are.
Every uninterrupted clear pass is unioned first, so two overlapping images
erase their overlap instead of XOR-painting it back, and that union is
subtracted from every track, flash and region painted before it. Copper painted
after the pass is untouched. Each connected polygon in the Boolean remainder is
emitted as a separate primitive. This handles clear-over-track and
clear-over-pad operations, a gap severing a concave pour, partial intersection
with a pour boundary, and an annular clear's isolated centre without guessing
from cross-statement contour nesting. `%SR%` carries both dark and clear painter
operations into every repeat.

One high-density topology has an equivalent bounded fast path: exactly one
convex plane followed only by non-overlapping interior clear images. Signed
coverage plus indexed island extraction is exact under those preconditions and
does not clone a cut plane containing millions of contour points. The 10,000
annular CI regression exercises this route; overlap, concavity, boundary
crossing, interleaved copper, or any additional primitive takes the general
Boolean path.

What that argument rests on is the void's own **geometry** never being larger
than the void the film cleared, so **only exactly-reproduced geometry may
erase copper**. Several aperture images are deliberate over-approximations: a
macro flash becomes the convex hull of its primitives (a fixed disc when it
cannot be evaluated at all), a draw whose aperture declares no width takes a
0.1 mm hairline, and a circular draw or a circular region boundary flattens
into inscribed chords, which stay inside a convex stretch of a boundary but
cut across the copper on a concave one. An aperture block (`%AB`) resolves to
nothing at all, this plotter not implementing blocks. And the object
transforms `%LS` / `%LR` / `%LM` are not applied, so a `2x1` rectangle under
`%LS0.5*%` really clears `1x0.5`. Nor is `%IPNEG*%`, where the whole image is
complemented so the clear objects are the copper. Each is correct while it only
*adds* copper,
since a flash that claims a little too much never invents a gap, and
destructive the moment it subtracts. Those clears are refused outright.

Standard apertures are polygonized *inscribed*, so they under-remove; the one
exception is a holed aperture's rim, which for a clear flash bounds the copper
island the void leaves standing and is therefore **circumscribed** so the
island reads slightly wide rather than slightly short, and refused when the
hole is so nearly as wide as its aperture that the circumscribed rim escapes
the outer boundary.

Nesting depth is NOT used across clear images from a whole film. Those images
can overlap and their contours can cross; the clear-pass union resolves that
topology directly. The earlier depth reconstruction promoted 11,890 of 12,000
overlapping antipads into phantom copper and restored the board-sized sheet.

Where nesting depth IS valid is inside one `G36`/`G37` statement, whose
contours do not cross, so a clear region is split into its connected pieces at
the moment it is banked, each piece's outer boundary first and its holes after.
Containment there is judged from a constructed interior point, never a vertex:
`point_in_polygon` is half-open, so a hole whose loop starts at a point it
shares with its outer, which boolean-op CAM emits routinely, answered inside or
outside by drawing orientation alone. The pieces of one clear statement enter
the same Boolean pass together: cutting a ring's outer while dropping the piece
that restores its island would erase copper the film kept. That statement may
legally carry several disjoint islands in any order, and
treating everything after the first contour as a hole is a guess about draw
order: a second disjoint void in one statement was cancelled outright, and an
annular void drawn hole-first had its cleared ring promoted to copper.

A region counts as clear only when the polarity is clear at **both** `G36` and
`G37`. Which end decides is a reading of when the region object is created, and
requiring both means an ambiguous film is painted rather than subtracted, so a
wrong reading over-connects instead of fabricating a break. A region that flips
polarity mid-way is painted, never dropped.

The remaining limits are the admission refusals above: a clear outline that is
not conservative enough to subtract safely, negative image polarity, unapplied
transforms, unsupported aperture blocks, and arc-bearing clear regions or
strokes. Accepted straight-edged and standard-aperture clears no longer have
track/pad, concavity, boundary-crossing, or multi-pour exceptions.

Gated by `crates/hauksbee-extract/tests/gerber_negative_pour.rs`: a distilled
pour with two ringed pads must reconstruct to three conductors, and its
lookalike, whose right-hand pad the voids leave bridged, to two.

## Excellon dialects

The drill reader handles the KiCad/decimal form and the **Altium dialect**
the Inkplate 6 ships: `;FILE_FORMAT=2:5` (integer:decimal), `INCH,LZ`,
`T<idx>F..S..C<dia>` tool defs (feed/speed before the diameter), **modal
single-axis coordinate lines** (`X..` keeps the last Y, `Y..` keeps the last
X), and `;TYPE=PLATED` / `;TYPE=NON_PLATED` sections (NPTH tools dropped
from connectivity). The Altium drill is named `<board>-RoundHoles.TXT` /
`-RectHoles.TXT` / `-SlotHoles.TXT`, recognised by the `holes` token. Before
we handled these, the Inkplate drill parsed to zero holes; tests live in
`gerber::excellon` and `gerber_inkplate.rs`.

## Performance

The connectivity trace is fully spatial-indexed (R-tree per layer for the
touch sweep; R-tree-pruned region lookups). Pour containment over
board-spanning plane layers (tens of thousands of vertices, tested against
every primitive) is accelerated by `PolyGrid`: a scanline-rasterised
inside/outside/boundary grid built once per large pour, making each query
O(1) outside a thin boundary band. Multi-contour Boolean remainders and the
qualified dense-plane fast path use it too. The grid also answers "is
there any pour boundary inside this pad's bounds", which keeps the exact
poly-distance penetration test off a hot path that a board-sized pour would
otherwise reach for every pad on the layer. Its scanline classifier sweeps an
active-edge table, so it holds one copy of each edge rather than one per row
the edge spans, and its time is linear in the rows those edges sweep (two or
three per edge of an antipad's outline). A layer's gridded pours share a cell
budget rather than each taking the resolution ceiling.

The retained bounded stress regression replays 10,000 non-overlapping annular
antipads in one convex plane and requires one plane plus all 10,000 isolated
islands.

Two-layer boards extract in tens to hundreds of milliseconds. The 6-layer
reform motherboard (about 75k draws and four 35k-vertex plane pours)
extracts in ~15-40 s depending on machine load. The dominant remaining cost
is the all-pairs touch sweep on the densest signal layers.

## Honest limitations

- **The closed-loop % only scores located pads.** As the validation section
  stresses, net-partition agreement is computed over pads the
  reconstruction placed, which ranges from ~74% (dense keyboards) to 100%.
  Pads we fail to locate (bottom-side parts, footprints the package-name
  window under-covers) are not counted against the percentage. The located
  fraction is gated separately so a regression that *loses* pads cannot
  hide behind a high partition %, but the headline number is "agreement
  over what we found", not "of the whole board".

  This reaches users rather than living only here.
  `ReconStats::coverage_notes()` reports the pad accounting whenever any flash
  went unmatched ("N of M aperture flashes (P%) were matched to a placed
  component; K were not"), notes that not every flash is a component pad (via
  lands, fiducials and test points are flashed too) so the unmatched count is an
  upper bound on missing pads, states that an unmatched flash still joins the
  copper net it touches but carries no pin, so every component-level figure
  including any closed-loop percentage scores only the matched ones, and names
  the pick-and-place upload that would place the ones that are pads.
  The gerber readers return it through `ExtractedBoard::from_gerber_with_stats`,
  and `NormalizedBoard::notes` carries it into the evidence map alongside the
  ODB++ and IPC-2581 reader notes, so every surface prints it. A job with every
  pad located gets no note.
- **Footprint inference is approximate.** The pad-assignment window is
  inferred from the package-name string, with a flat 4.0 mm fallback for
  unrecognised names. This both *inflates* some pad counts (a stitching via
  inside the window is hard to tell from a real pad without the netlist, so
  a crystal may report its GND vias as extra pads, on the correct net, but
  over-counted) and *misses* pads (a part whose pads fall outside an
  under-sized window). Component-name match in the closed loop therefore
  runs well below net agreement (Watchy recovers 60 of 84 component names at
  100.0% net partition; the reform motherboard, 438 of 522); the recovered
  *connectivity* is what the simulator needs, and that stays near-exact
  over located pads.
- **Aperture-macro coverage** is the common primitives (circle,
  center/vector line, outline, polygon) with `$n` substitution and basic
  arithmetic. Moire and thermal primitives (fiducials/reliefs, not pads)
  are skipped; a macro using a variable expression we cannot evaluate falls
  back to a small disc so the flash still anchors a pad. Concave macros are
  over-approximated by their convex hull.
- **Pour fidelity**: a deliberately split pour drawn as one dark fill reads
  as connected within that fill. KiCad emits separate dark regions per
  island, so this is rarely wrong in practice; an exotic single-region
  split-plane could mis-merge.
- **An Allegro-style `drill-1-6.art` name is not read as a layer pair.** A
  bare number pair in a file name is too easily a revision or a part number,
  so only an `L`-marked or KiCad layer-named pair counts. A gerber-format
  drill film that states its span in a `TF.FileFunction` attribute is read
  like any other; one that only encodes it in a bare-number name falls back
  to the ordinary silent-drill reading.
- **Edge contacts are not a recognised class.** Gold fingers and card-edge
  contacts are ordinary copper on the outline, so they reconstruct as
  whatever copper they touch, with no separate treatment and no claim that
  they mate with anything off the board. The corpus carries no board with
  them, so there is nothing measured here either way.
- **The castellation count needs an outline film.** A job that ships no
  `Edge.Cuts` / `.GKO` outline reports zero castellations regardless of what
  it has. Connectivity is unaffected, since the barrel joins its pad without
  reference to the outline; only the count goes quiet.
- **A drill film cannot always say whether its holes are plated.** The
  geometry on such a film is recovered in full (see the slots section), but
  plating is the difference between a conductor and a hole in the board and
  it is not a geometric fact. Four sources are read: the film's
  `TF.FileFunction`, its `%TA.AperFunction` drill functions, its file name,
  and the job splitting plated from non-plated across two files. A film with
  none of those is dropped rather than guessed either way, counted in
  `ReconStats::refused_plating_files` and named in a note. Guessing plated
  invents a net; guessing mechanical deletes one; there is no safe default,
  only a visible refusal.
- **Clear polarity (LPC)** is cut, not skipped. See [Negative-drawn
  pours](#negative-drawn-pours-lpc). Admitted geometry is clipped at a pour edge
  and cuts earlier tracks and pads too. Geometry that cannot be reproduced
  conservatively enough for subtraction (such as an unapplied transform,
  unsupported aperture block, or arc-bearing clear region/stroke) is refused and
  leaves copper standing rather than fabricating an open.
- **Inner-layer order: exporter metadata is read before filenames.** A
  `.gbrjob` copper entry places its film in the stack; a usable `.EXTREP`
  extension row supplies the same role when no job entry exists. Only without
  either does order fall back to filename digits (`gnd02` maps to stack index
  2), where a plane named without a number collapses to a default inner slot
  and via stitching can land on the wrong pair. The mapping-file escape hatch
  (`copper:<index>`) deliberately overrides all package metadata.
- **An inner film named only by its user label needs the job file (or the
  mapping file) to be classified as copper.** KiCad exports `In1.Cu` renamed
  to `GND.Cu` as `-GND_Cu.gbr`; the `.gbrjob`, when present, names it
  `Copper,L3,Inr` and it classifies and orders exactly. A job that ships
  neither the manifest nor a mapping file loses the layer: the MNT Reform
  motherboard is a six-layer board that reconstructs from two films this
  way. It still scores 99.7% net partition, because its routing is dominated
  by the outer layers and its through-holes stitch what remains, but that
  number is over the pads we located on the layers we saw. The drill set's
  declared layer count is compared against the films classified and the
  shortfall is reported as a note, so the gap is at least visible; the fix
  is the exporter's `.gbrjob` or a `layer_map.txt` naming those films.
- **GND label is a heuristic** (largest pour-touching net), so the `GND`
  *name* can be wrong on a power-plane-dominant or split-ground board.
  Connectivity is unaffected; only the label is a guess.
- **The bundled `.zip` reader flattens by file name.** Two entries with the
  same basename in different sub-folders of a job zip would overwrite each
  other. Job zips are flat in practice, but a deeply-nested archive is
  better extracted manually and pointed at as a directory.
- **kicad-cli round-trip**: the closed loop needs `kicad-cli` to make ground
  truth, and the floors are calibrated against 9.0.3. A board a newer or older
  CLI cannot load (KiCad-10-format demos such as `pic_programmer` and
  `stickhub` are the ones we hit) is skipped rather than failed, with a printed
  note naming it. The gate prints both the calibrated and the running version on
  every run, so if a floor breaks you can tell whether the extractor regressed
  or the exporter moved the ground truth. The right response to the latter is to
  re-measure and record the new version, never to shave the floor.
