# Copper short / clearance detection, and simulating shorts

Hauksbee simulates from a real layout, so two pieces of copper that touch while
belonging to different nets are an electrical fact the simulation must know
about: a solder bridge, an overlapping pad, a pour eating into a track. This
document covers how those are found from geometry (`hauksbee-extract`), and how a
detected short is then applied to the live circuit so the simulation shows what
the board actually does with the short present (`hauksbee-engine`).

## Pipeline

```
.kicad_pcb ──drc::run_drc──▶ DrcReport (shorts + clearance violations)
                                   │
                                   ▼
              Scheduler::apply_drc_shorts / short_nets
                                   │  bridge shorted nets with a few-mΩ resistor
                                   ▼
              transient solve ──▶ StressMonitor ──▶ FaultEvent{kind:"short", ...}
                                   │
                                   ▼
                         frontend fault channel (no UI change)
```

## Detection (`hauksbee-extract/src/drc.rs`)

### Geometry kinds covered

Every conductive primitive on a copper layer is reduced to one of three solid
shapes, all in board millimetres:

| Source primitive                | Modelled as | Notes |
|---------------------------------|-------------|-------|
| Track segment (`segment`)       | Capsule (a width-aware "stadium") | width-aware: distance subtracts both half-widths |
| Arc track (`arc`)               | Capsule chain | flattened to 8 links through start/mid/end via the circumcircle |
| Via (`via`)                     | Disc on every copper layer it spans | layer span read from `(layers ...)`, else all copper |
| Through-hole pad (`*.Cu`)       | Disc / shape on every copper layer | `*.Cu` / `F&B.Cu` expanded to the declared copper stack |
| SMD / THT pad, circle          | Disc | radius = half the larger size dimension |
| SMD / THT pad, oval            | Capsule (stadium) | segment along the long axis, radius = half the short axis |
| SMD / THT pad, rect / roundrect / trapezoid | Polygon (+ corner radius for roundrect) | roundrect inset by the corner radius, radius carried so the rounded copper is not overstated |
| SMD / THT pad, custom          | Polygon | first `gr_poly` outline, else the bounding rect |
| Filled zone (`zone` → `filled_polygon`) | Polygon (boundary edges + containment) | the actual fill copper, with antipads / thermal reliefs |

Pad outlines are transformed into the board frame correctly for rotated
footprints: KiCad writes a pad's `(at x y rot)` rotation as the pad's *absolute*
board-frame orientation (the footprint rotation already folded in), so the
outline is rotated by that angle alone while the pad *position* is rotated by the
footprint frame. Getting this wrong collapses a rotated QFP's pads on top of one
another and manufactures thousands of false shorts; the synthetic and corpus
tests guard it.

### Net handling

Net references are resolved across all the KiCad encodings the corpus contains:
`(net N "name")` (KiCad ≤9), name-only `(net "name")` (KiCad 10), and the older
`(net N)` + sibling `(net_name "name")` on zones. Two net classes are treated as
carrying no connectivity and are never reported as a short:

- net 0 (KiCad's empty / "no net" bucket), and
- `unconnected-(...)` placeholder nets (one per floating pad).

Two pads of the *same* footprint are also skipped: some footprints place
different-net pads deliberately abutting (fuse clips, jumper bridges,
edge-connector fingers), which KiCad does not treat as a board short.

### Spatial index and the sweep

Primitives are bucketed per copper layer and indexed in an
[`rstar`](https://docs.rs/rstar) R\*-tree on their (width-inflated) bounding
boxes. Each primitive queries the tree for neighbours whose boxes fall within
the clearance window, so the distance test runs only on genuinely-close pairs
instead of the O(n²) all-pairs blow-up. For a candidate pair on different nets we
compute the signed copper-edge gap (capsule-capsule, capsule-polygon, and
polygon-polygon distance, with proper segment-crossing tests so a track passing
straight *through* a pad is caught even when no endpoint or vertex is near):

- gap `<= 0` → the copper intersects: a **short** (`ViolationKind::Short`).
- `0 < gap < clearance` → not touching but closer than the rule allows: a
  **clearance violation** (`ViolationKind::Clearance`), a lower-severity
  near-short risk.

Filled zones are handled specially for performance: a single GND pour can carry
thousands of boundary vertices and a board-spanning bounding box, which would
make every primitive test against it. Instead each zone boundary *edge* is
indexed as its own zero-width capsule (so the R-tree prunes the distance sweep),
and the whole polygon is kept aside only for a cheap point-in-polygon
*containment* pass (a different-net via/track/pad fully inside the pour is a
short even if it never crosses the boundary). Outline-only zones (pre-2017
boards with no computed `filled_polygon`) keep their drawn boundary for
clearance checks but are excluded from the containment test, because the solid
outline has no antipads or thermal reliefs and would falsely engulf every
other-net pad inside the pour.

### Clearance rule

The clearance is read from the board's design rules when present
(`(setup (rules (min_clearance N)))`, or `(setup (min_clearance|clearance N))`),
else the sane default `DEFAULT_CLEARANCE_MM = 0.2 mm`. `run_drc` also accepts an
explicit override.

### Output

`DrcReport` carries every finding (nets involved by id and name, copper layer,
representative `(x, y)` location, signed gap, and the two involved items with
their kind and owning component), plus `shorted_net_pairs()` for the engine. The
CLI surfaces it: `hauksbee run <board> --drc` prints the table and exits, and
`ExtractedBoard::drc(text)` is the library entry point, alongside the existing
`lint()`.

## Eagle `.brd` detection (`hauksbee-extract/src/drc.rs`, `eagle_drc`)

The most famous open-hardware boards (Arduino Uno, five Adafruit, two SparkFun)
are Eagle, not KiCad, so they used to get `n/a` for DRC. The Eagle path closes
that gap: it reads copper *geometry* per net out of the `.brd` XML and feeds it
to the same `sweep_buckets` engine the KiCad path uses. There is one detection /
classification core; only the front-end geometry reader differs.
`ExtractedBoard::drc(text)` dispatches on content (`(kicad_pcb` vs `<eagle`), so
`hauksbee run <board.brd> --drc` works.

Geometry is kept in Eagle's native frame (millimetres, y-up). The DRC is
self-consistent, so the y orientation never matters; only relative positions do.
Layer model: copper layers are numbered, 1 = Top (`F.Cu`), 16 = Bottom (`B.Cu`),
2..15 inner. A mirrored element (`rot="MR<deg>"`) is flipped onto the opposite
face, swapping its side-specific copper 1↔16.

### Geometry kinds covered

| Source primitive | Modelled as | Notes |
|------------------|-------------|-------|
| Signal wire (`<wire>`) | Capsule (width-aware "stadium") | distance subtracts both half-widths; a `curve="<deg>"` attribute is flattened into 8 capsule links |
| Via (`<via>`) | Disc on every copper layer it spans | `extent="1-16"` → all copper; outer diameter taken from `diameter` if present, else derived from the drill and the board's `rvViaOuter` / `rl*ViaOuter` restring rule; `shape="octagon"` modelled as an octagon polygon |
| Through-hole pad (`<pad>` in a package) | shape on every copper layer | `round`→disc, `square`→rect, `octagon`→octagon polygon, `long`→stadium capsule, `offset`→capsule offset to one end |
| SMD pad (`<smd>` in a package) | Rect polygon (+ corner radius) | single layer (1 by default, flipped by mirror); `roundness` (0..100 %) carried as a corner-radius inflation on an inset rect, like KiCad roundrect; `rot` honoured |
| Board rectangle (`<rectangle>` on copper) | Rect polygon | rotated by `rot` |
| Board circle (`<circle>` on copper) | Disc | conservative solid disc of the outer radius (`radius + width/2`) |
| Signal polygon / pour (`<polygon>`) | **excluded from the short test** | see the honesty caveat below |

Package copper is placed with the full element transform: position rotated by the
element's `rot` (CCW, y-up), and a mirrored (`MR`) element reflected across its
local X axis (flip Y) before the rotation. (Verified against the QT Py's SOIC8:
only flip-Y puts `MR90`-placed pad 1 at its real coordinate; the older flip-X
convention scrambled every mirrored package and manufactured thousands of false
shorts.)

### Rule source

The clearance is read from the board's embedded `<designrules>` rather than
assumed: the tightest of the copper-to-copper spacing rules (`mdWireWire`,
`mdWirePad`, `mdWireVia`, `mdPadPad`, `mdPadVia`, `mdViaVia`, `mdSmdPad`,
`mdSmdVia`, `mdSmdSmd`), parsed with their unit suffix (`mil` / `mm` / `mic` /
`inch`). The `0.2 mm` default is used only when no design rules are present.
Reporting against the board's own (often tighter) rule avoids manufacturing
clearance noise on densely-routed boards.

### Honest polygon (copper pour) fidelity caveat

A `.brd` stores only a signal polygon's **requested outline**, not the copper
Eagle actually pours. The real fill carves an `isolate` antipad gap around every
foreign-net wire / pad / via inside the outline, and arbitrates overlapping pours
by `rank`. Neither the antipads nor the rank are in the file. Treating the drawn
outline as solid copper turns every trace that legitimately crosses into a pour
(and every foreign pad the pour isolates around) into a false short, so the pour
is **excluded from the short / clearance test entirely** rather than reported
dishonestly. The outline is still parsed (with its per-vertex curves) so the
limitation is explicit, not silent. Checking pour-to-copper shorts honestly would
need the board re-poured in Eagle and the computed polygons (with antipads)
exported; that data is not in the source `.brd`.

### Deliberate ties exempted

Two nets that both land a pad in the *same* component are deliberately tied there
(a solder jumper `SJ` / `SMT-JUMPER`, a star-ground point, a 0-ohm / ferrite
bridge). The component places their pads a hair apart by design and the traces
feeding it abut, which the EDA does not flag. So copper of two nets sharing a
footprint is exempt. This generalises the KiCad pad-vs-pad same-owner rule to the
track-vs-pad and track-vs-track forms Eagle's jumpers produce (Eagle traces carry
no owner of their own, so the exemption keys on shared net membership in a
footprint). On the Uno this correctly clears `GND`↔`UGND`, joined through the
`GROUND` SJ jumper.

## Simulation (`hauksbee-engine/src/shorts.rs`)

A detected (or hypothetical) short is applied by bridging the two nets' circuit
nodes with a small resistor (`BRIDGE_OHMS = 5 mΩ`, the resistance of a real
solder blob, small enough to drag the nets together hard, large enough to keep
the MNA matrix well-conditioned). After stamping, the scheduler rebuilds the MNA
layout and resizes its state buffers, then every subsequent transient solve
carries current across the bridge and the existing **stress monitor** sees the
fallout (rails collapsing, series parts driven over their power/current
ratings).

Each applied bridge is itself surfaced as a `FaultEvent` of kind `short` through
the same channel as the rating-based faults, so the frontend highlights it with
no UI change. Two entry points on `HauksbeeEngine` / `Scheduler`:

- `apply_drc_shorts(&report)`: apply every true overlap a `DrcReport` found
  (clearance-only violations are not applied), and the convenience
  `from_board_file_with_drc_shorts(...)` that detects and applies in one call.
- `short_nets(net_a, net_b)`: the what-if API, short an arbitrary pair of nets
  on demand (a solder-bridge scenario), including shorting a live net to GND.

The CLI exposes `--apply-shorts` to bridge every detected short before a run.

## Performance (measured on board-corpus, release build, warm)

The R-tree sweep plus the zone edge-indexing keeps even very large boards to a
few seconds; the s-expression parse dominates the wall time on the biggest
files.

| Board | Size | Copper primitives | Shorts | Clearance | Time |
|-------|------|-------------------|--------|-----------|------|
| jetson-agx-thor-baseboard | 85 MB | 573,619 | 0 | 25,226 | ~2.6 s |
| vme-wren | 69 MB | 988,848 | 0 | 46,154 | ~3.4 s |
| video | 5.8 MB | 133,415 | 0 | ~34 | ~0.4 s |
| tinytapeout-demo | 4.5 MB | 85,816 | 0 | 347 | ~0.5 s |
| pic_programmer | 0.6 MB | 11,087 | 0 | 0 | ~20 ms |

A full sweep of the corpus (54 boards, ~50 parse successfully; one,
RoyalBlue54L-Feather, is malformed at the s-expression level and rejected
upstream by `forge-sexpr`) reports **zero true shorts**, correct, since these
are all shipped, working boards. Clearance violations remain on tightly-routed
boards and are expected.

### Eagle famous-board sweep (release build, warm; the board's own rule)

All ten famous Eagle boards report **zero true shorts**, each judged against
its own embedded design-rule clearance. The first eight carry the measured
copper/clearance/time figures below (observed on a release build, warm; the
last two were added later as regression guards and are asserted short-clean by
the corpus test without re-measuring the timings):

| Board | Rule | Copper primitives | Shorts | Clearance | Time |
|-------|------|-------------------|--------|-----------|------|
| Arduino Uno R3 (official) | 0.2032 mm | 1,407 | 0 | 0 | ~13 ms |
| Adafruit Circuit Playground Express | 0.1778 mm | 1,252 | 0 | 1 | ~36 ms |
| Adafruit Feather M0 Basic | 0.2032 mm | 873 | 0 | 0 | ~7 ms |
| Adafruit Metro M4 Express | 0.2032 mm | 1,430 | 0 | 0 | ~48 ms |
| Adafruit QT Py | 0.1524 mm | 637 | 0 | 0 | ~10 ms |
| Adafruit Trinket M0 | 0.2032 mm | 369 | 0 | 0 | ~12 ms |
| SparkFun RedBoard | 0.2032 mm | 851 | 0 | 1 | ~32 ms |
| SparkFun Pro Micro | 0.1016 mm | 868 | 0 | 0 | ~7 ms |
| SparkFun Thing Plus SAMD51 | board rule | — | 0 | — | — |
| SparkFun Thing Plus RP2040 | board rule | — | 0 | — | — |

The last two were the round-3 additions: the RP2040 Thing Plus in particular is
the regression guard for the Eagle mirror-transform fix in `drc.rs` (its MR0
micro-SD socket J6 used to drop pads ~23 mm onto the V_USB/EN bottom traces and
report 5 false shorts), so it must stay short-clean.

The two residual clearance violations (one on Circuit Playground Express, one on
RedBoard) are genuine sub-rule near-misses on densely-routed copper, not shorts.
Time is dominated by the XML parse (the Eagle reader streams the file twice: once
for copper geometry, once for the `contactref` net map).

### A documented corpus finding

An earlier sweep surfaced 2 "shorts" on several Olimex ESP32-EVB revisions
(REV-A..D, L). Investigated: they were different-net pads placed deliberately
*abutting inside one footprint* (a fuse-clip footprint and a capacitor
footprint). That is the footprint author's intent, not a board short, and KiCad
does not flag intra-footprint copper. The detector handles it with a principled
rule (pads sharing a footprint owner are skipped) rather than a per-board
allowlist. The corpus test (`tests/drc_corpus.rs`) documents this.

## Tests

- `hauksbee-extract/tests/drc.rs`: 11 synthetic fixtures, one per geometry kind
  (segment-segment, segment-pad, pad-pad, via-zone, via-spans-layers) plus
  clearance-only, cross-layer non-shorts, same-footprint abutment, and the
  clearance-override classification.
- `hauksbee-extract/tests/drc_corpus.rs`: the corpus sweep asserting zero true
  shorts across the parseable boards (skipped gracefully if the corpus is
  absent).
- `hauksbee-extract/tests/eagle_drc.rs`: 16 synthetic minimal `.brd` fixtures, one
  per Eagle geometry kind (wire-wire short, wire-smd, smd-smd, via-wire,
  via-spans-layers, octagon pad, curved wire) plus the clearance-only, no-rule
  fallback, cross-layer non-short, shared-footprint (jumper) abutment, mirrored
  package placed on the bottom, via-restring derivation, and the design-rule
  clearance respected / overridden cases.
- `hauksbee-extract/tests/eagle_drc_corpus.rs`: the famous-Eagle sweep over all
  eight boards (Arduino Uno, five Adafruit, two SparkFun), asserting zero true
  shorts and recording per-board clearance counts, rule, primitive count and
  timing (skipped gracefully if the corpus is absent).
- `hauksbee-engine/tests/shorts.rs`: end-to-end, detect a copper short from a
  layout, apply it, and assert a `short` fault is raised and the bridged nets are
  pulled together; the what-if `short_nets` rail-to-ground case raising an
  overpower fault on the series resistor; and a clean board applying nothing.

## Limitations

- **Zone fill fidelity.** Detection uses the `filled_polygon` copper KiCad
  computed and stored in the file. Boards with no stored fill (older formats, or
  freshly-edited unfilled zones) fall back to the drawn outline for clearance
  only and are excluded from the containment short-test, so a short *into the
  interior* of an unfilled pour is not detected (its boundary is still checked).
  Re-running the board through KiCad's zone fill restores full coverage.
- **Arc flattening.** Arc tracks are approximated by 8 straight capsule links;
  the chord error is sub-micron for typical track radii but a pathologically
  large arc could under-report a grazing clearance by a few microns.
- **Roundrect / custom pads** are approximated (roundrect as an inset rectangle
  plus a corner radius; custom pads by their first polygon primitive or bounding
  rect). This is conservative for overlap and tight for clearance to within the
  corner radius.
- **Eagle signal pours.** A `.brd` stores only a pour's requested outline, not
  the poured copper with its `isolate` antipads or `rank` arbitration, so pours
  are excluded from the Eagle short / clearance test entirely (see the fidelity
  caveat above). A short *into* a pour is therefore not detected on Eagle boards;
  wires, vias and pads against each other are fully covered. KiCad pours, which
  do carry the computed fill, are covered.
- **Eagle multilayer.** The Eagle reader spans through-hole pads and vias over a
  two-layer (`1`/`16`) copper stack, which matches the entire famous-board
  corpus. A genuinely multilayer Eagle `.brd` with inner-layer copper would need
  its inner layers added to that stack.
- **Eagle curve flattening.** Wire `curve` arcs and per-vertex polygon curves are
  flattened into 8 capsule links, with the circumcircle centre chosen so the
  sweep lands on the stated endpoint; the chord error is sub-micron for typical
  radii.
- The bridge model is a fixed small resistance; it does not model the bridge's
  own current-dependent fusing. Destructive-mode faulting still applies to the
  parts the short over-drives.
