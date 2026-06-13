# Altium `.PcbDoc` Ingest

Altium Designer is the dominant professional / enterprise / regulated-industry
EDA tool. A large, serious tier of hardware (medical, aerospace, industrial,
high-speed digital, satellites) is authored in Altium and never touches KiCad or
Eagle. Reading those designs natively brings that tier into hauksbee's bind +
DRC + lint + simulation pipeline.

Entry points:

- `ExtractedBoard::from_altium_pcb(bytes)` — connectivity (nets, components,
  netted pads), the same `ExtractedBoard` the KiCad / Eagle / IPC / gerber paths
  produce.
- `ExtractedBoard::altium_drc(bytes)` — the geometric short / clearance DRC over
  the board's copper (the binary twin of `ExtractedBoard::drc(text)`).
- `ExtractedBoard::from_auto_bytes(bytes)` — content sniff: an Altium `.PcbDoc`
  is auto-detected from the OLE2 magic (`D0 CF 11 E0`) plus the presence of
  Altium record streams, then dispatched. The CLI (`hauksbee run <board>`) reads
  the file as bytes and routes binary boards here automatically, exactly as the
  Eagle path is auto-detected from XML. There is no new CLI surface and no new
  flag: `.PcbDoc` works wherever `.kicad_pcb` / `.brd` does.

## The format

A `.PcbDoc` is a Microsoft OLE2 / Compound File Binary (CFB) container: a
filesystem-in-a-file of *storages* (directories) and *streams* (files). We open
it with the battle-tested [`cfb`](https://docs.rs/cfb) crate rather than
hand-rolling the FAT / DIFAT.

Each logical section is a sub-storage (`Nets6`, `Components6`, `Pads6`,
`Tracks6`, `Arcs6`, `Vias6`, `Polygons6`, ...) holding a `Data` stream (the
records) and a small `Header` stream (a record count, which we ignore). Older
Altium / Protel files drop the `6` suffix (`Nets`, `Pads`, `Components`); both
namings are tried.

Two record encodings live inside the `Data` streams:

- **Properties strings** (`Board6`, `Nets6`, `Components6`, `Polygons6`): a u32
  little-endian length (the top byte is a flag, masked off), then a
  NUL-terminated ASCII string `|KEY=VALUE|KEY=VALUE|...`. Keys are uppercased;
  coordinate values carry a `mil` suffix; a `%UTF8%`-prefixed twin key carries
  the UTF-8 form of footprint / library names.
- **Fixed binary records** (`Pads6`, `Vias6`, `Tracks6`, `Arcs6`): a 1-byte
  record-type marker, then one or more sub-records each prefixed with a u32
  length. Coordinates are signed `i32` in Altium internal units (1 unit =
  2.54 nm = 1/10000 mil, so `mm = unit * 2.54e-6`). Net and component references
  are u16 indices into `Nets6` / `Components6` (`0xFFFF` = none); the index is
  0-based, the net id we assign is `index + 1` so id 0 stays the "no net" bucket
  (matching the KiCad / Eagle convention).

## What is read

```
.PcbDoc (OLE2/CFB)
   │  cfb crate
   ├── Nets6/Data        ──▶ net names (index = primitive net field)
   ├── Components6/Data   ──▶ refdes (SOURCEDESIGNATOR), footprint (PATTERN),
   │                          library, placement (X/Y/ROTATION/LAYER), channel
   ├── Pads6/Data         ──▶ pad designator, layer, net, owning component,
   │                          position, size, shape  ──▶ pins + copper
   ├── Tracks6/Data       ──▶ copper segments (layer, net, endpoints, width)
   ├── Arcs6/Data         ──▶ copper arcs (centre, radius, angles, width)
   ├── Vias6/Data         ──▶ through-hole vias (net, position, diameter)
   ├── Texts6/Data        ──▶ component value/comment (best-effort)
   └── Polygons6/Data     ──▶ copper-pour outlines (layer, net, vertices)
          │ connectivity                       │ geometry
          ▼                                    ▼
   ExtractedBoard (nets, components, pads)   altium_drc → DrcReport
```

The connectivity extractor (`crates/hauksbee-extract/src/altium.rs`) builds the
nets, components, and netted pads. The DRC geometry extractor (the `altium_drc`
submodule in `drc.rs`) reads copper geometry per net and feeds it to
`sweep_buckets` — the exact same R-tree short / clearance engine the KiCad and
Eagle paths use, so there is one detection engine, not three.

Channel-replicated designs (the same `SOURCEDESIGNATOR` reused across identical
sub-blocks, e.g. three FLASH banks all called `C1`) are disambiguated by
appending the channel name from `SOURCEHIERARCHICALPATH` (`C1_FLASH2`), exactly
as KiCad's importer does, so every component has a unique reference for the
binder.

## Accuracy: closed-loop cross-validation against KiCad

KiCad 9 ships an independent Altium importer. Its bundled Python
(`pcbnew.PCB_IO_MGR.Load` with the `ALTIUM_DESIGNER` plugin) converts a
`.PcbDoc` to a `.kicad_pcb` headlessly. We convert each real corpus board,
extract BOTH the original (native Altium path) and the conversion (KiCad path),
and compare the **net partition** over shared `(refdes, pad)` pins: two pins
sharing a net in one extraction must share a net in the other. Net *names*
differ (KiCad renames), so the partition is compared, not the labels.

Result on the routable corpus boards:

| Board | Nets | Components | Netted pins | Partition agreement vs KiCad |
|-------|------|-----------|-------------|------------------------------|
| Cobra ESP32 dev board | 18 | 27 | 96 | **100%** (96 shared pins) |
| QFSAE dev kit | 21 | 24 | 61 | **100%** (61) |
| PiDP-11 IO expander | 30 | 23 | 95 | **100%** (95) |
| HERON CubeSat OBC | 62 | 70 | 281 | **100%** (279) |
| altium2kicad test-vias | 6 | 5 | 15 | **100%** (15) |
| EBAZ4205 Zynq FPGA | 392 | 565 | 1742 | connectivity matches; see limitations |

100% net-partition agreement against a wholly independent importer is strong
ground truth: the extraction is *correct*, not merely non-crashing. The DRC is
short-clean on every real board (they shipped, or nearly), with clearance
violations reported on the dense ones (e.g. the EBAZ4205 BGA fanout) as expected.

Test coverage:

- `tests/altium.rs` — synthetic in-memory `.PcbDoc` fixtures (built with `cfb`),
  exercising the properties decoder, the `Pads6` / `Tracks6` binary layouts,
  net / component index resolution, auto-detection, and a deliberate-short DRC.
- `tests/altium_corpus.rs` — the real-board sweep (extraction + short-clean DRC)
  and the KiCad cross-validation (corpus-gated; `HAUKSBEE_REQUIRE_CORPUS=1`).

Corpus boards, sources and licenses: `board-corpus/famous/SOURCES.md` (the
Altium section). The KiCad conversions used for cross-validation are committed
under `board-corpus/altium_xval/`.

## Records adapted from KiCad

The binary record layouts are ported field-by-field from KiCad's open-source
Altium importer (KiCad master tree), principally:

- `pcbnew/pcb_io/altium/altium_parser_pcb.cpp` — the `APAD6`, `AVIA6`,
  `ATRACK6`, `AARC6`, `ACOMPONENT6`, `ANET6`, `APOLYGON6` parsers and the
  `ALTIUM_LAYER` enum.
- `common/io/altium/altium_binary_parser.cpp` — `ReadProperties` (the
  pipe/equals decoder) and the stream reader primitives.
- `pcbnew/pcb_io/altium/altium_props_utils.cpp` — `ConvertToKicadUnit` (the unit
  factor).

Cross-checked against the `altium2kicad` project (thesourcerer8) and a Python
`olefile` prototype before porting. The `cfb` crate replaces KiCad's vendored
`CompoundFileReader`.

## A real bug chased to the binary (the Tarski discipline)

An early version reported 42 "shorts" on the EBAZ4205. Per `docs/BUG_HUNT.md`
the rule is: chase every short to the data before believing it. All 42 were on
`In2.Cu` and every one involved a copper-pour polygon. The board has split
power planes (10 solid pours of different nets — VCC, GND, VCCA, VCC-DDR — on one
inner layer), and foreign-net vias pass through each plane via antipad voids that
Altium carves in `Regions6`, which the extractor does not parse. So a via legally
sitting inside a foreign pour read as a short against the pour outline.

The fix is principled, not a per-board allowlist: a copper pour whose true fill
(with its antipads and thermal reliefs) is not modelled contributes **no edges**
to the short / clearance sweep (`push_zone_opts(..., edges = false)`). This is
the Altium analogue of the Eagle `filled = false` rule (Eagle `.brd` pours store
only the requested outline too). With it, the EBAZ4205 is short-clean and the
five cross-validated boards stay at 100% partition agreement.

## Honest limitations

- **Component value / comment is best-effort.** Altium stores the displayed
  value/comment as a `Texts6` record flagged `isComment`, but on most boards that
  text is a bound field placeholder (`.Comment` / `.Designator`) whose literal
  resolves through `WideStrings6`, which is not parsed. The refdes (from
  `SOURCEDESIGNATOR`), footprint (`PATTERN`) and full connectivity are solid; the
  value is often left empty. The binder works off footprint + connectivity
  regardless (see the `--report` output on the cobra board), so this does not
  affect bind / DRC / lint / sim.

- **Newest-format designators (WideStrings) on some boards.** A few newer Altium
  files (e.g. the EBAZ4205) store no `SOURCEDESIGNATOR` in `Components6` at all;
  the refdes lives in a `WideStrings6`-indexed `Texts6` designator label whose
  byte layout is version-specific. On those boards the component *references*
  come out blank. The **electrical model is unaffected** (the EBAZ4205 still
  extracts 392 nets and 1742 netted pins, and its DRC is short-clean) — only the
  human-facing labels are missing, which is why its KiCad cross-validation can
  not perform the label-keyed join. Resolving `WideStrings6` would lift this.

- **Copper-pour fill is not modelled.** Pour *outlines* (and therefore the pour's
  net) are read, but the filled copper with its antipads / thermal reliefs
  (`Regions6` / `ShapeBasedRegions6`) is not. Consequently a pour does not
  participate in short detection (see the bug note above). Pad-, track-, via- and
  arc-level shorts are detected normally.

- **ASCII `.pcbdoc` is not yet supported.** Altium also has a text variant whose
  files begin `|RECORD=Board|` (e.g. SimpleFOCMini). Only the binary OLE2 form is
  read today; the ASCII form is detected as "not a binary board" and currently
  falls through. (One ASCII sample is kept in the corpus for a future path.)

- **`.SchDoc` (schematic) is not read.** The PCB was the priority because it
  carries full net connectivity in one file (the layout alone fully describes the
  circuit, exactly like a `.kicad_pcb`). The Altium schematic (`.SchDoc`, also
  OLE2 but with different record streams: `FileHeader`, pin / wire / net-label
  records) would only be needed for the "simulate before there's a layout" path,
  and is deferred. For Altium projects the `.PcbDoc` is the complete source of
  truth for connectivity.

- **Allegro / OrCAD / other binary EDA are out of scope.** This module reads
  Altium `.PcbDoc` only. Cadence Allegro `.brd` (a different binary format) is
  still ingested only via its gerbers (`docs/GERBER.md`), not natively.

- **Record types not yet handled:** `Fills6` (copper fills), `Dimensions6`,
  `Rules6` (so the DRC uses the default 0.2 mm clearance, not the board's own
  rule), `ComponentBodies6` / `Models` (3D), `Classes6` (net classes). None of
  these change net connectivity; `Fills6` copper would refine the DRC on the rare
  boards that use large copper fills outside pours.
