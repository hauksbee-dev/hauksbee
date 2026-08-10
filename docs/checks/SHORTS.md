# Copper short / clearance detection, and simulating shorts

Hauksbee simulates from a real layout, so two pieces of copper that touch while
belonging to different nets are an electrical fact the simulation must know
about: a solder bridge, an overlapping pad, a pour eating into a track. This
document covers how those are found from geometry (`hauksbee-extract`), and how a
detected short is then applied to the live circuit so the simulation shows what
the board actually does with the short present (`hauksbee-engine`).

## Pipeline

![How a copper short travels through Hauksbee: DRC on the .kicad_pcb produces a DrcReport of shorts and clearance violations, the scheduler bridges the shorted nets with a few-milliohm resistor, and the transient solve and stress monitor raise a short fault event on the frontend fault channel](../assets/diagrams/short-detection.svg)

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
| SMD / THT pad, rect / roundrect | Polygon (+ corner radius for roundrect) | roundrect inset by the corner radius, radius carried so the rounded copper is not overstated |
| SMD / THT pad, trapezoid       | Polygon | `rect_delta` honoured (KiCad corner formula, delta clamped the way KiCad clamps it) |
| SMD / THT pad, custom          | Anchor shape + every primitive | anchor circle/stadium or rect over `size`, plus all `gr_poly`/`poly` outlines, stroked lines, arcs (covering flattening), circles (filled disc or covering ring) and rects |
| Filled zone (`zone` → `filled_polygon`) | Polygon (boundary edges + containment) | the actual fill copper, with antipads / thermal reliefs |

Pad outlines are transformed into the board frame correctly for rotated
footprints: KiCad writes a pad's `(at x y rot)` rotation as the pad's
*absolute* board-frame orientation (the footprint rotation already folded
in), so the outline is rotated by that angle alone while the pad *position*
is rotated by the footprint frame. Getting this wrong collapses a rotated
QFP's pads on top of one another and manufactures thousands of false shorts.
The synthetic and corpus tests guard it.

### Net handling

Net references are resolved across all the KiCad encodings the corpus contains:
`(net N "name")` (KiCad ≤9), name-only `(net "name")` (KiCad 10), and the older
`(net N)` + sibling `(net_name "name")` on zones. Two net classes are treated as
carrying no connectivity and are never reported as a short:

- net 0 (KiCad's empty / "no net" bucket), and
- `unconnected-(...)` placeholder nets (one per floating pad).

Footprint ownership alone never suppresses a finding. Different-net pads in an
ordinary fuse, connector, resistor, or IC are checked exactly like copper from
different footprints. Deliberate contacts are exempted only by the explicit,
format-specific rules under "Deliberate ties exempted locally" below.

### Spatial index and the sweep

Primitives are bucketed per copper layer and indexed in an
[`rstar`](https://docs.rs/rstar) R\*-tree on their (width-inflated) bounding
boxes. Each primitive queries the tree for neighbors whose boxes fall within
the clearance window, so the distance test runs only on genuinely-close pairs
instead of the O(n²) all-pairs blow-up. For a candidate pair on different nets we
compute the signed copper-edge gap (capsule-capsule, capsule-polygon, and
polygon-polygon distance, with proper segment-crossing tests so a track passing
straight *through* a pad is caught even when no endpoint or vertex is near):

- gap `<= 0` → the copper intersects: a **short** (`ViolationKind::Short`).
- `0 < gap < clearance - CLEARANCE_TOLERANCE_MM` → not touching but genuinely
  closer than the rule allows: a **clearance violation**
  (`ViolationKind::Clearance`), a lower-severity near-short risk.
- `clearance - tolerance <= gap < clearance` → a gap sitting *at* the rule (or a
  few microns under it): routing-to-rule, **not reported**. See the tolerance
  note below.

#### Clearance tolerance (the boundary-noise fix)

A gap reported as `clearance - epsilon` is overwhelmingly a routing-to-rule
artifact, not a defect: KiCad lets the router lay copper exactly at the design
rule, and the nm grid plus our arc/capsule flattening (chord error a few
microns) leaves the *measured* gap a hair under the nominal rule. Reporting
those produced **137 spurious clearance notes on bms-c1 and 66 on the PD-sink
board**, which drowned the real findings. So a small tolerance
(`CLEARANCE_TOLERANCE_MM = 0.005 mm`, 5 um, well under any real copper clearance
yet above the geometry's own rounding noise) raises the floor for the soft
clearance band: a positive gap is a clearance violation only when it falls more
than the tolerance below the rule. **Shorts (gap <= 0, real copper overlap) are
unaffected**. The tolerance only relaxes the soft clearance band, never a true
intersection.

Validated: bms-c1 drops from 137 spurious notes to **0** ("no shorts or
clearance violations"). The PD-sink board drops from 66 to **4** genuinely
sub-rule gaps (0.15 / 0.18 mm under the 0.2 mm rule) that were previously
buried. The DRC corpus + fixture tests (including the at-rule,
sub-micron-under-rule, and genuinely-sub-rule cases) stay green. True shorts
still fire.

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

## Eagle `.brd` detection (`hauksbee-extract/src/drc.rs`, `eagle_drc_from_text`)

The most famous open-hardware boards (Arduino Uno, five Adafruit, four SparkFun)
are Eagle, not KiCad, so they used to get `n/a` for DRC. The Eagle path closes
that gap: it reads copper *geometry* per net out of the `.brd` XML and feeds it
to the same `sweep_buckets` engine the KiCad path uses. There is one detection
/ classification core. Only the front-end geometry reader differs.
`ExtractedBoard::drc(text)` dispatches on content (`(kicad_pcb` vs `<eagle`), so
`hauksbee run <board.brd> --drc` works.

Geometry is kept in Eagle's native frame (millimeters, y-up). The DRC is
self-consistent, so the y orientation never matters. Only relative positions
matter.
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
| Board circle (`<circle>` on copper) | Annulus (covering capsule ring), or a solid disc when `width` is 0 | the stroke band is the copper; the interior is bare board (Eagle fills only zero-width circles) |
| Signal polygon / pour (`<polygon>`) | settings parsed; a same-rank overlap with another signal's pour is shorted, at a measured 4-of-6 accuracy; fill **excluded from the pour-to-copper test** | see the honesty caveat below |

Package copper is placed with the full element transform: position rotated by the
element's `rot` (CCW, y-up). A mirrored (`MR`) element negates local X and uses
the mirrored rotation sense. This is regression-tested for both `MR90` and
`MR0`; the latter is what keeps the RP2040 Thing Plus micro-SD pads on their
actual bottom-side coordinates. The pad's own axis is reflected through that
same transform. A real asymmetric `shape="offset"` through-hole fixture checks
both `MR0` and `MR180`, including the side where copper must *not* appear; a
round or symmetric long pad cannot detect that direction error.

### Rule source

The clearance is read from the board's embedded `<designrules>` rather than
assumed: the tightest of the copper-to-copper spacing rules (`mdWireWire`,
`mdWirePad`, `mdWireVia`, `mdPadPad`, `mdPadVia`, `mdViaVia`, `mdSmdPad`,
`mdSmdVia`, `mdSmdSmd`), parsed with their unit suffix (`mil` / `mm` / `mic` /
`inch`). The `0.2 mm` default is used only when no design rules are present.
Reporting against the board's own (often tighter) rule avoids manufacturing
clearance noise on densely-routed boards.

The same rules block supplies `psElongationLong` and `psElongationOffset`.
EAGLE defines these as the percentage of pad diameter added along the pad axis;
using the 100% default unconditionally overstated the Arduino Uno's 50%-elongated
header pads and manufactured an SDA/SCL collision.

### Honest polygon (copper pour) fidelity caveat

A `.brd` stores a signal polygon's **requested outline** plus its pour settings
(`isolate`, `rank`, `thermals`, `orphans`, `pour`), all of which are parsed;
only the *computed* fill polygon is absent, because Eagle re-derives it on every
ratsnest / CAM run. That derivation is what keeps the fill out of the
pour-to-copper short test: Eagle carves max(`isolate`, the applicable
design-rule / net-class clearance) around every foreign-net wire, pad and via
(an `isolate` below the rules distance is ignored), thermal spokes only remove
same-net copper, and orphan removal only deletes fill pockets. Every setting
keeps or widens gaps, so a correctly derived fill cannot short or crowd foreign
copper in the same file, while treating the drawn outline as solid copper would
turn every trace the fill legitimately carves around into a false short. The
fill itself is never reconstructed and the isolate distance never numerically
re-verified; the settings are parsed, drive this reasoning, and travel verbatim on
a pour finding's `Item::owner` field, which the report does not render (see below). Pour-to-copper pairs are therefore *not checked*
(rather than checked and found clean), and same-rank pours that approach
without ring overlap are not distance-checked either, since the fill extent
near the boundary depends on those settings.

One pour-to-pour construct **is** checked: two overlapping same-rank pours of
different signals get no arbitration from their rank, and the overlap is reported.

That rule over-reports, and by how much is measured rather than estimated. The
emonTx revision family ships copper gerbers beside its `.brd`, so its six
layer-instances can be scored by asking whether any filled region of a layer
contains vias of both nets: the rule is right about four, flagging the three layers
where the nets really do share copper and over-reporting two top layers where the
outlines overlap and nothing bridges them
([`../evidence/KNOWN_FAULTS_VALIDATION.md`](../evidence/KNOWN_FAULTS_VALIDATION.md)).
Narrowing it by `isolate` was implemented and reverted: `isolate` is necessary but
not sufficient for two pours to merge, so the narrowing fixed one over-report and
silenced a layer where a trace genuinely joins the nets. Removing the over-reports
needs Eagle's fill reconstructed, which is not implemented
([`../about/LIMITATIONS.md`](../about/LIMITATIONS.md)).

The pour settings ride along on the finding's `Item::owner` field, which
`drc_probe` prints and the tests assert, but no user surface shows it: `--drc`
renders through `DrcStructured::render` (nets, layer, gap, location), `--drc
--plain` through `plain_drc_structured` (nets, layer, location), and `--drc --json`
serialises `DrcShort`, which has no owner field. So a reader of the report sees
neither what the overlap was made of nor that this class over-reports.

### Deliberate ties exempted locally

Ordinary components do not create DRC exemptions: two different-net pads owned
by the same resistor, IC, or connector still report, and an A/B component never
waives an unrelated A/B copper collision elsewhere.

The recogniser is deliberately format-specific; a reference, value, library, or
footprint name by itself never waives copper:

- **KiCad:** `(net_tie_pad_groups "1, 2" "3, 4")` is authoritative, and each
  quoted group remains electrically separate. `(attr net_tie)` with no group
  list forms one all-pad group for the older native form. House footprint names
  work because names are not the discriminator. Two tightly bounded legacy
  forms remain: a dedicated `0R_...` footprint *and* an independently zero-ohm
  value, and old EAGLE imports whose footprint has a `TIED` token *and* whose
  value explicitly identifies a pair such as `Closed(1-2)`. The latter exempts
  only that pair, never every pad in the footprint.
- **EAGLE:** `.brd` has no native net-tie Component Type field. The DRC therefore
  accepts only exact, real-board library/package conventions: Arduino's
  `library="jumper"`, `package="SJ"`, and SparkFun's
  `SparkFun-Jumpers` closed-trace packages. A generic package/value named
  `JUMPER` is checked even when both fields match. New vendor conventions require
  a real-board fixture. The two-field dedicated-0R rule is the only zero-ohm
  exception. A tie drawn as a **supply symbol** rather than a footprint is not
  expressible in the `.brd` at all, and is read from the companion `.sch`
  instead: see [Declared ties read from the schematic](#declared-ties-read-from-the-schematic)
  below, which reclassifies rather than exempts.
- **Altium binary `.PcbDoc`:** only the native Components record
  `COMPONENTTYPE=Net Tie` or `Net Tie (In BOM)` is accepted. `PATTERN`, library,
  reference, and inferred 0R names are not substitutes. Component ownership
  uses the same channel-aware canonical reference as extraction, so repeated
  `NT1` designators in two channels cannot share an exemption. If the native
  field is absent, the DRC abstains from exempting it. The Protel ASCII reader
  currently extracts connectivity but has no geometric DRC path.

Every accepted group is still owner-, layer-, net-group-, and contact-local.
The reported contact point must land on that group's own copper. A legal A/B
contact at one tie can never suppress another A/B collision elsewhere.

### Declared ties read from the schematic

The exemptions above are all **footprint** properties, and they all **waive** the
finding: the copper never appears in the report. Eagle has a second way to declare
a deliberate join that no footprint carries, and it needs the opposite treatment.

A star ground in Eagle is drawn by placing one net's **supply symbol** on another
net. `emonTx V3.4.5.sch` puts an `AGND` supply symbol (`AGND7`) on a segment of
net `GND` alongside a `GND` one (`SUPPLY6`); the board then routes the two
together on both layers. An Eagle `.brd` records no net ties of any kind, so the
DRC, handed only the `.brd`, sees copper between two differently named nets and
cannot tell this from a solder bridge.

**The companion input.** Supply the schematic with `--schematic <FILE>`, or leave
it beside the board under the board's own name (`<project>.brd` / `<project>.sch`)
and it is found automatically, the same sibling convention the `.kicad_pro`
clearance lookup uses. The parser (`hauksbee-extract/src/eagle_sch.rs`) reads one
thing and nothing else: which named nets the schematic declares deliberately tied.
It is not a schematic extractor, because the `.brd` already carries the netlist.
A supply symbol is recognised by Eagle's own marker, a library symbol whose pin
has `direction="sup"`, with the **pin's name** being the net it imposes; a tie is
claimed only when such a symbol sits on a net whose name differs from its own.
An ordinary component bridging two nets declares nothing.

**Reclassified, not deleted.** A covered finding keeps its net pair, layer,
location and measured gap, and gains the declaration. Its severity drops from
`serious` to `note`, it stops failing `--strict`
(`DrcReport::undeclared_shorts` is the gating set, `shorts` stays the full one),
and every surface states both halves: that the nets are joined in copper, and
that the schematic declares the tie, naming the symbols and the net. Deleting the
finding would hide that GND and AGND share copper, which is a fact about the board
the user is entitled to whether or not it was intended.

**No schematic, no silence.** With none supplied, the finding stays `serious` and
names the schematic as the unlocking upload, on the finding's own `fix` text and
as a report-level note. And a schematic that declares nothing qualifies nothing:
matching is by net-name pair against an actual declaration, so supplying a
schematic can never be a route to silence a short. emonTx V3.4.0, the revision
before the tie was drawn, declares none and its contacts stay serious even with
its own schematic supplied. That is the guard the reverted `isolate` narrowing
failed, and it is a test, not an intention.

**Eagle only, and enforced.** A `.kicad_pcb` declares its ties in the layout the
DRC already has and a `.kicad_sch` has no construct for a deliberate two-net join
to read (`docs/ingest/SCHEMATICS.md`); Altium carries the native
`COMPONENTTYPE=Net Tie` field. A KiCad or Altium board is therefore never asked
for a schematic and carries no hint, and no companion is looked for beside it. An
explicit `--schematic` on a non-Eagle board is REFUSED rather than ignored:
accepting it would let a schematic describing a different design reclassify that
board's shorts on a net-name coincidence, and ignoring it would leave the user
reading an unqualified serious short believing their schematic had been read.
Likewise a path that is not an Eagle `.sch` (a `.brd`, a KiCad legacy `.sch`, a
pre-Eagle-6 binary drawing) is refused with the reason.

The schematic enters the run's input inventory as
`ArtifactRole::Schematic` / `ArtifactKind::EagleSchematic` with its SHA-256 and
what it contributed, so a reclassified finding traces to the file that
reclassified it; its connectivity is recorded as deliberately ignored. The DRC
evidence map for a reclassified short cites both artifacts, because its assertion
text quotes the declaration.

The `sch_ties` example is the diagnostic behind these tests, and reproduces the
revision boundary the evidence file records:

```
cargo run -p hauksbee-extract --example sch_ties -- "<board>.brd" "<board>.sch"
# declared ties: 1
#   GND <-> AGND: AGND7 wired to SUPPLY6 in net GND
# shorts: 2
# qualified: 2
# still gating: 0
```

Run against emonTx V3.4.0 it prints `declared ties: 0` and `still gating: 2`,
which is the same board family three revisions earlier and the reason the
false-negative guard is a test rather than an intention.

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

The R-tree sweep plus the zone edge-indexing keeps even very large boards
tractable. The s-expression parse dominates the wall time on the biggest files,
so time tracks file size more closely than primitive count: the largest board
below takes roughly 10 to 16 s, the corpus's biggest fetchable KiCad file
(`mnt_reform/reform2-motherboard25`, 16 MB, 398,079 primitives, clean) about
4.8 s, and a mid-size board like `pic_programmer` about 0.2 s end to end through
the CLI.

| Board | Size | Copper primitives | Shorts | Clearance |
|-------|------|-------------------|--------|-----------|
| jetson-agx-thor-baseboard | 85 MB | 600,007 | 0 | 25,226 |
| vme-wren | 69 MB | 1,032,732 | 0 | 46,154 |
| video | 5.8 MB | 135,031 | 0 | 0 |
| tinytapeout-demo | 4.5 MB | 86,626 | 0 | 347 |
| pic_programmer | 0.6 MB | 11,087 | 0 | 0 |

The current required-corpus gate scans 64 parseable KiCad boards and 2,300,130
copper primitives. It reports no unaccounted true shorts; the explicitly
documented, expiring exceptions remain visible in `tests/drc_corpus.rs` rather
than being hidden in the detector. Clearance violations remain on tightly
routed boards and are expected.

### Eagle famous-board sweep (release build, warm, the board's own rule)

All ten famous Eagle boards report **zero true shorts**, each judged against
its own embedded design-rule clearance:

| Board | Rule | Copper primitives | Shorts | Clearance |
|-------|------|-------------------|--------|-----------|
| Arduino Uno R3 (official) | 0.2032 mm | 1,403 | 0 | 0 |
| Adafruit Circuit Playground Express | 0.1778 mm | 1,252 | 0 | 1 |
| Adafruit Feather M0 Basic | 0.2032 mm | 873 | 0 | 0 |
| Adafruit Metro M4 Express | 0.2032 mm | 1,424 | 0 | 0 |
| Adafruit QT Py | 0.1524 mm | 637 | 0 | 0 |
| Adafruit Trinket M0 | 0.2032 mm | 369 | 0 | 0 |
| SparkFun RedBoard | 0.2032 mm | 797 | 0 | 0 |
| SparkFun Pro Micro | 0.1016 mm | 834 | 0 | 0 |
| SparkFun Thing Plus SAMD51 | 0.127 mm | 906 | 0 | 0 |
| SparkFun Thing Plus RP2040 | 0.1016 mm | 1,179 | 0 | 0 |

The RP2040 Thing Plus is the regression guard for the Eagle mirror transform in
`drc.rs`: get the mirrored-element handedness wrong and its `MR0` micro-SD socket
J6 lands ~23 mm off, dropping pads onto the V_USB/EN bottom traces and reporting
5 false shorts. It must stay short-clean.

The two residual clearance violations in the current sweep (one each on the
Circuit Playground Express and Metro M4 Express) are genuine sub-rule near-miss
reports on densely routed copper, not shorts. Every board here sweeps in a
fraction of a second, dominated by the XML parse
(the Eagle reader streams the file twice: once for copper geometry, once for the
`contactref` net map).

### A documented corpus finding

An earlier sweep hid contacts on several Olimex ESP32-EVB revisions behind a
blanket same-owner waiver. Re-investigation found the surviving case in a
dedicated `0R_0603` footprint: an auxiliary same-number copper pad locally
touches the opposite terminal. It is now handled by the explicit, owner- and
location-scoped copper-link rule above. Ordinary same-footprint pads are never
waived. The corpus test (`tests/drc_corpus.rs`) documents this evidence.

## Tests

- `hauksbee-extract/tests/drc.rs`: 40 synthetic fixtures, one per geometry kind
  (segment-segment, segment-pad, pad-pad, via-zone, via-spans-layers) plus
  clearance-only, cross-layer non-shorts, ordinary same-footprint shorts,
  native house-name net ties, distinct multi-group and cross-group cases,
  name-only negatives, locally scoped 0R and legacy closed-pair cases, the
  clearance-override classification, the at-rule / micron-under-rule /
  genuinely-sub-rule boundary cases, the per-netclass and diff-pair clearance
  rules (including `.kicad_pro` assignments and a malformed class), blind and
  buried via spans on a 4-layer stack, and a mask-only pad carrying no copper.
- `hauksbee-extract/tests/drc_corpus.rs`: the corpus sweep asserting zero true
  shorts across the parseable boards (skipped gracefully if the corpus is
  absent).
- `hauksbee-extract/tests/eagle_drc.rs`: 28 synthetic minimal `.brd` fixtures, one
  per Eagle geometry kind (wire-wire short, wire-smd, smd-smd, via-wire,
  via-spans-layers, octagon pad, curved wire) plus the clearance-only, no-rule
  fallback, cross-layer non-short, ordinary same-owner shorts, locally scoped
  dual-field jumper abutment, the single-field negative, remote same-net-pair
  collisions, board-derived long pad elongation, mirrored package placement,
  and asymmetric offset-pad direction under `MR0`/`MR180`, via-restring
  derivation, format dispatch, `POPULATE="no"` mapping to DNP, same-named
  packages in different libraries staying distinct, an element with a missing
  package, and the design-rule clearance respected / overridden cases.
- `hauksbee-extract/tests/altium.rs`: synthetic binary Components/Pads/Tracks
  records covering both native net-tie Component Types, PATTERN-only abstention,
  local scope, and repeated-channel raw-designator isolation.
- `hauksbee-extract/tests/eagle_drc_corpus.rs`: the famous-Eagle sweep over all
  ten boards (Arduino Uno, five Adafruit, four SparkFun), asserting zero true
  shorts and recording per-board clearance counts, rule, primitive count and
  timing (skipped gracefully if the corpus is absent).
- `hauksbee-engine/tests/shorts.rs`: end-to-end, detect a copper short from a
  layout, apply it, and assert a `short` fault is raised and the bridged nets
  are pulled together. Also the what-if `short_nets` rail-to-ground case
  raising an overpower fault on the series resistor, and a clean board
  applying nothing.

## Limitations

- **Zone-versus-pad overlaps are suppressed as a class, and the run says so.**
  A pad overlapping a different-net pour is nearly always the antipad carve:
  KiCad always clears a pad out of a different-net fill, and KiCad 10 draws that
  carve with keyhole slits whose boundary edges run through the pad interior,
  producing negative gaps (the 1,668-short epidemic on one ESC). So a
  Zone↔Pad *overlap* is dropped wholesale. Real pour incursions by a track, via
  or arc are unaffected, and a positive sub-clearance Zone↔Pad gap is still kept
  as a normal clearance note.

  Dropping a whole finding class silently would make "no shorts found" and "no
  shorts found, having declined to look at a category" render identically.
  `DrcReport::zone_pad_overlaps_suppressed` counts the suppressed **primitive
  pairs** (a zone boundary is indexed edge by edge, so one carve around one pad
  contributes several, making the number an upper bound rather than a pad count,
  which the disclosure states). It is an `Option`, because `Some(0)` ("measured,
  nothing suppressed") and `None` ("this payload predates the accounting, so it
  was never measured") are different claims and a serde default of zero would
  turn the second into the first. `None` yields its own note saying so. It is
  `DrcReport::suppression_note()` renders the disclosure, which all four DRC
  surfaces print ahead of any clean claim (the text report as `NOT CHECKED:`, the
  plain report as a heads-up, JSON as `suppression_note`, and the TUI's copper
  pane as an info-severity `class_not_checked` finding). Note this is a different
  four from the "four batch report surfaces" the co-sim docs count: hauksbee-ci
  runs no DRC at all, and the web front door does not read the field. Boards where nothing was suppressed get no note. To audit
  the suppressed class, run KiCad's own DRC.

- **KiCad 10 and newer: exact native-DRC parity remains unvalidated.** The
  `20260206` name-only net encoding and baked keyhole-antipad contours are now
  handled. A format-20260206 fixture checked with kicad-cli 10.0.5 keeps the pad
  inside a real keyhole antipad silent while reporting a pad under solid
  different-net fill; the same oracle reports no Zone↔Pad violations on the
  VENDETTA ESC that formerly produced 1,668 phantom shorts.

  The version warning remains because the complete finding set is not yet exact
  KiCad parity (VENDETTA reports 67 Hauksbee shorts versus 60 native
  `shorting_items`), and KiCad 10 keeps project clearance rules in the sibling
  `.kicad_pro` rather than the board text consumed by this API. CI gates
  therefore still do not fail on a format at or above `20260000`. Cross-check
  with KiCad 10's own DRC. The constant is
  `FIRST_UNVALIDATED_PCB_VERSION` in `crates/hauksbee-extract/src/drc.rs`.
- **Zone fill fidelity.** Detection uses the `filled_polygon` copper KiCad
  computed and stored in the file. Boards with no stored fill (older formats, or
  freshly-edited unfilled zones) fall back to the drawn outline for clearance
  only and are excluded from the containment short-test, so a short *into the
  interior* of an unfilled pour is not detected (its boundary is still checked).
  Re-running the board through KiCad's zone fill restores full coverage.
- **Arc flattening.** Arc tracks are approximated by 8 straight capsule links.
  The chord error is sub-micron for typical track radii, but a pathologically
  large arc could under-report a grazing clearance by a few microns.
- **Roundrect pads** are represented as an inset rectangle plus a corner
  radius. This is conservative for overlap and tight for clearance to within
  the corner radius. Custom pads stamp their anchor shape and every drawn
  primitive (all polygons, lines, arcs, circles and rectangles); trapezoid
  pads honor `rect_delta`.
- **Eagle signal pours.** The computed fill is not in the `.brd`, so
  pour-to-copper pairs are not checked (the fidelity caveat above explains
  why a fill Eagle derives from the stored settings would not violate, and
  why the outline must not stand in for it); overlapping same-rank pours of
  different signals are reported as shorts, which over-reports at a measured
  rate (4 of 6 scored instances right). Wires, vias and
  each other are fully covered. KiCad pours, which do carry the computed
  fill, are covered.
- **Eagle multilayer.** The Eagle reader spans through-hole pads and vias
  over a two-layer (`1`/`16`) copper stack, which matches the entire
  famous-board corpus. A genuinely multilayer Eagle `.brd` with inner-layer
  copper would need its inner layers added to that stack.
- **Eagle curve flattening.** Wire `curve` arcs are flattened into 8 capsule
  links, with the circumcircle centre chosen so the sweep lands on the stated
  endpoint. The chord error is sub-micron for typical radii. Drawn copper
  circles, pad-primitive arcs, and pour-outline curves instead use
  sagitta-bounded *covering* flattening (error at most 2.5 µm), biased
  outward so a grazing short is never lost to chord sag; the flip side is
  that a true air gap smaller than ~2.5 µm can read as touching and be
  reported a short. That is a deliberate FN-averse trade at a scale no
  fabricator can produce as intentional spacing.
- **Eagle pour boundary strokes.** The same-rank pour overlap short keys on
  the polygons' vertex rings. Two same-rank pours whose rings miss each other
  by less than their drawn boundary stroke widths are not flagged; pinning
  whether Eagle's own DRC treats a stroke graze as overlap needs an
  Eagle-generated oracle board, which the corpus does not yet carry.
- **Eagle single-clearance model.** The Eagle DRC resolves ONE clearance (the
  tightest copper-gating `md*` rule) rather than the per-item-kind matrix
  (`mdWireWire` vs `mdPadPad`, ...); net-class values and matrix cells are
  floored at that single rule. A board whose `md*` values diverge is checked
  against the tightest of them everywhere, the long-standing
  no-manufactured-noise choice of this path.
- **Altium octagonal pads.** Modeled with the 0.25 corner-cut ratio KiCad's
  Altium importer uses (the reference implementation this module's record
  layouts are ported from). If Altium's native render were the regular
  octagon (cut ≈ 0.293), up to ~4% of the shorter pad side of corner copper
  is overstated; an Altium-generated oracle board would pin the true cut.
- The bridge model is a fixed small resistance. It does not model the
  bridge's own current-dependent fusing. Destructive-mode faulting still
  applies to the parts the short over-drives.
