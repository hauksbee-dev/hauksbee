# Altium `.PcbDoc` Ingest

Altium Designer is the dominant professional / enterprise / regulated-industry
EDA tool. A large, serious tier of hardware (medical, aerospace, industrial,
high-speed digital, satellites) is authored in Altium and never touches KiCad
or Eagle. Reading those designs natively brings that tier into hauksbee's bind
+ DRC + lint + simulation pipeline.

## If you use Altium

The whole path, with no conversion step and no flag:

```bash
hauksbee run MyBoard.PcbDoc --report --plain   # which parts were modelled
hauksbee run MyBoard.PcbDoc --drc --plain      # shorts and clearance from the copper
hauksbee run MyBoard.PcbDoc --lint --plain     # design lint in plain language
```

`hauksbee run` sniffs the file content, so a `.PcbDoc` works wherever a
`.kicad_pcb` does, including as the `board` key of a `hauksbee-ci` spec.

**Which file.** The `.PcbDoc` from your Altium project, as is. It carries the
full net connectivity in one file, so it is the complete source of truth and you
do not need the rest of the project.

**Binary, not ASCII Protel.** hauksbee reads the binary OLE2 `.PcbDoc` that
Altium Designer writes. There is also a text variant whose file begins
`|RECORD=Board|`, which is what tools such as EasyEDA emit when they claim an
Altium export, and hauksbee does not read it. If you have one of those, re-save
it from Altium Designer as a normal binary `.PcbDoc`.

**Git LFS pointers.** Large `.PcbDoc` files are commonly stored in Git LFS. A
fresh clone without `git lfs pull` gives you a few hundred bytes of text instead
of the board. Run `git lfs pull` first.

Neither case falls through to a generic "cannot read this" error. Both are
recognised for what they actually are and told apart, because "your file is a
stub the clone never downloaded" and "your file is the wrong Altium dialect"
need different actions from you:

```
$ hauksbee run board.PcbDoc --report
error: 'board.PcbDoc': this is a Git LFS pointer, not the board file itself: the repository stores the real file in Git LFS and it was never downloaded. Run `git lfs install && git lfs pull` in the repository, then retry with the real file
```

The ASCII-Protel case names itself the same way, says it is what EasyEDA
produces, and tells you to re-save from Altium Designer as a binary `.PcbDoc`.
Only a file that matches none of the readers at all gets the generic message,
which lists every format hauksbee does read. All three exit 1: a hard input
error, distinct from a report that ran and found something (see the
[exit-code contract](../ci/CI.md#exit-codes-the-pipeline-contract)).

**What to expect from bind coverage.** Altium keeps the displayed component
value as a bound field that resolves through a string table hauksbee does not
parse yet, so on many boards the `Value` column comes out blank and those parts
report as unresolved rather than being silently guessed. Refdes, footprint, and
connectivity are solid, so **the copper checks (DRC, netlint, signal integrity)
are unaffected**; it is the analog, AC, thermal, and firmware results on those
specific nets that a blank value limits, and the report's bottom line says which.
Passives whose value you need are worth adding as models, one small TOML file
each and no recompile
([`../extending/add-an-analog-part.md`](../extending/add-an-analog-part.md)). The
full statement is under "Honest limitations" below.

**No Altium project at all, or an unreadable one?** Gerbers plus a
pick-and-place file are the universal fallback, and Altium exports both.
hauksbee reverse-extracts the board from copper geometry alone
([`GERBER.md`](GERBER.md)).

The rest of this document is the format internals: how a `.PcbDoc` is parsed,
what is cross-validated against KiCad, and where the limits are.

Entry points:

- `ExtractedBoard::from_altium_pcb(bytes)` reads connectivity (nets,
  components, netted pads) into the same `ExtractedBoard` the KiCad / Eagle /
  IPC / gerber paths produce.
- `ExtractedBoard::altium_drc(bytes)` runs the geometric short / clearance DRC
  over the board's copper: the binary twin of `ExtractedBoard::drc(text)`.
- `ExtractedBoard::from_auto_bytes(bytes)` performs a content sniff. It
  auto-detects an Altium `.PcbDoc` from the OLE2 magic (`D0 CF 11 E0`) plus
  the presence of Altium record streams, then dispatches accordingly. The CLI
  (`hauksbee run <board>`) reads the file as bytes and routes binary boards
  here automatically, exactly as it auto-detects the Eagle path from XML.
  There is no new CLI surface and no new flag: `.PcbDoc` works wherever
  `.kicad_pcb` / `.brd` does.

## The format

A `.PcbDoc` is a Microsoft OLE2 / Compound File Binary (CFB) container: a
filesystem-in-a-file of *storages* (directories) and *streams* (files). We
open it with the battle-tested [`cfb`](https://docs.rs/cfb) crate rather than
hand-rolling the FAT / DIFAT.

Each logical section is a sub-storage (`Nets6`, `Components6`, `Pads6`,
`Tracks6`, `Arcs6`, `Vias6`, `Polygons6`, and so on) holding a `Data` stream
(the records) and a small `Header` stream (a record count, which we ignore).
Older Altium / Protel files drop the `6` suffix (`Nets`, `Pads`,
`Components`). We try both namings.

Two record encodings live inside the `Data` streams:

- **Properties strings** (`Board6`, `Nets6`, `Components6`, `Polygons6`): a
  u32 little-endian length (the top byte is a flag, masked off), then a
  NUL-terminated ASCII string `|KEY=VALUE|KEY=VALUE|...`. Keys stay
  uppercase. Coordinate values carry a `mil` suffix. A `%UTF8%`-prefixed twin
  key carries the UTF-8 form of footprint / library names.
- **Fixed binary records** (`Pads6`, `Vias6`, `Tracks6`, `Arcs6`): a 1-byte
  record-type marker, then one or more sub-records, each prefixed with a u32
  length. Coordinates are signed `i32` in Altium internal units (1 unit =
  2.54 nm = 1/10000 mil, so `mm = unit * 2.54e-6`). Net and component
  references are u16 indices into `Nets6` / `Components6` (`0xFFFF` means
  none). The index is 0-based; we assign the net id as `index + 1` so id 0
  stays the "no net" bucket, matching the KiCad / Eagle convention.

## What is read

![Which streams inside a .PcbDoc file are read, and how they split into board connectivity and DRC geometry](../assets/diagrams/altium-streams.svg)

The connectivity extractor (`crates/hauksbee-extract/src/altium.rs`) builds
the nets, components, and netted pads. The DRC geometry extractor (the
`altium_drc` submodule in `drc.rs`) reads copper geometry per net and feeds it
to `sweep_buckets`, the exact same R-tree short / clearance engine the KiCad
and Eagle paths use. There is one detection engine, not three.

Channel-replicated designs (the same `SOURCEDESIGNATOR` reused across
identical sub-blocks, e.g. three FLASH banks all called `C1`) are
disambiguated by appending the channel name from `SOURCEHIERARCHICALPATH`
(`C1_FLASH2`), exactly as KiCad's importer does, so every component carries a
unique reference for the binder.

## Accuracy: closed-loop cross-validation against KiCad

KiCad 9 ships an independent Altium importer. Its bundled Python
(`pcbnew.PCB_IO_MGR.Load` with the `ALTIUM_DESIGNER` plugin) converts a
`.PcbDoc` to a `.kicad_pcb` headlessly. We convert each real corpus board,
extract BOTH the original (native Altium path) and the conversion (KiCad
path), and compare the **net partition** over shared `(refdes, pad)` pins:
two pins sharing a net in one extraction must share a net in the other. Net
*names* differ (KiCad renames them), so we compare the partition, not the
labels.

Result on the routable corpus boards:

| Board | Nets | Components | Netted pins | Partition agreement vs KiCad |
|-------|------|-----------|-------------|------------------------------|
| Cobra ESP32 dev board | 18 | 27 | 96 | **100%** (96 shared pins) |
| QFSAE dev kit | 21 | 24 | 61 | **100%** (61) |
| PiDP-11 IO expander | 30 | 23 | 95 | **100%** (95) |
| HERON CubeSat OBC | 62 | 70 | 281 | **100%** (279) |
| altium2kicad test-vias | 6 | 5 | 15 | **100%** (15) |
| EBAZ4205 Zynq FPGA | 392 | 565 | 1742 | **not cross-validated**: extracts 392 nets and 1742 netted pins and is short-clean, but the join cannot be made (see limitations) |

100% net-partition agreement against a wholly independent importer is strong
ground truth: the extraction is *correct*, not merely non-crashing. The DRC
is short-clean on every real board (they shipped, or nearly), with clearance
violations reported on the dense ones (e.g. the EBAZ4205 BGA fanout) as
expected.

**How that table was produced, and what a clone can rerun.** The five
cross-validated rows come from a run on the maintainers' corpus. The
cross-validation needs two things a clone does not have: the Altium source
boards, and a KiCad conversion of each. Neither is in the public fetch manifest.
The Altium board family is not listed in `corpus.toml` at all, and
`altium_xval/`, the conversion set, appears there only in the local-only
section, deliberately absent because its licence could not be established. So
**these five rows are not reproducible from a clone**, and nothing in the public
test suite depends on them: `altium_corpus.rs` skips with a printed note naming
exactly which state it is in ("no corpus at all" versus "corpus present but the
Altium family is not in it"), and `HAUKSBEE_REQUIRE_CORPUS=1` turns either skip
into a failure for a run that is supposed to have them.

What a clone *can* run is the synthetic layer, and that is where the format
contract is pinned:

- `crates/hauksbee-extract/tests/altium.rs` exercises synthetic in-memory
  `.PcbDoc` fixtures (built with `cfb`): the properties decoder, the `Pads6` /
  `Tracks6` binary layouts, net / component index resolution, auto-detection,
  and a deliberate-short DRC. No corpus needed.
- `crates/hauksbee-extract/tests/altium_corpus.rs` runs the real-board sweep
  (extraction + short-clean DRC) and the KiCad cross-validation. Corpus-gated.

Board provenance and licences live in `corpus.toml` at the repository root.

## Records adapted from KiCad

We port the binary record layouts field-by-field from KiCad's open-source
Altium importer (KiCad master tree), principally:

- `pcbnew/pcb_io/altium/altium_parser_pcb.cpp`, the `APAD6`, `AVIA6`,
  `ATRACK6`, `AARC6`, `ACOMPONENT6`, `ANET6`, `APOLYGON6` parsers and the
  `ALTIUM_LAYER` enum.
- `common/io/altium/altium_binary_parser.cpp`, `ReadProperties` (the
  pipe/equals decoder) and the stream reader primitives.
- `pcbnew/pcb_io/altium/altium_props_utils.cpp`, `ConvertToKicadUnit` (the
  unit factor).

We cross-checked against the `altium2kicad` project (thesourcerer8) and a
Python `olefile` prototype before porting. The `cfb` crate replaces KiCad's
vendored `CompoundFileReader`.

## A real bug chased to the binary

An early version reported 42 "shorts" on the EBAZ4205. A short is a claim about
the copper, so the rule is to chase every one to the data before believing it
(see [docs/evidence/BUG_HUNT.md](../evidence/BUG_HUNT.md) for the same discipline
applied across the checks). All 42 sat on `In2.Cu`, and every one involved a
copper-pour polygon. The board has split power planes (10 solid pours of
different nets
(VCC, GND, VCCA, VCC-DDR) on one inner layer), and foreign-net vias pass
through each plane through antipad voids that Altium carves in `Regions6`,
which the extractor does not parse. So a via legally sitting inside a
foreign pour read as a short against the pour outline.

The fix is principled, not a per-board allowlist: a copper pour whose true
fill (with its antipads and thermal reliefs) is not modelled contributes
**no edges** to the short / clearance sweep (`push_zone_opts(..., edges =
false)`). This is the Altium analogue of the Eagle `filled = false` rule
(Eagle `.brd` pours also store only the requested outline). With this fix,
the EBAZ4205 is short-clean and the five cross-validated boards stay at 100%
partition agreement.

## Honest limitations

- **Component value / comment is best-effort.** Altium stores the displayed
  value/comment as a `Texts6` record flagged `isComment`, but on most boards
  that text is a bound field placeholder (`.Comment` / `.Designator`) whose
  literal resolves through `WideStrings6`, which we do not parse. The refdes
  (from `SOURCEDESIGNATOR`), footprint (`PATTERN`) and full connectivity are
  solid. The value is often left empty. The binder works off footprint +
  connectivity regardless, so this does not affect bind / DRC / lint / sim.
  `hauksbee run <board.PcbDoc> --report` is where you see it: parts bind and
  resolve with their `Value` column blank.

- **Newest-format designators (WideStrings) on some boards.** A few newer
  Altium files (e.g. the EBAZ4205) store no `SOURCEDESIGNATOR` in
  `Components6` at all. The refdes lives in a `WideStrings6`-indexed
  `Texts6` designator label whose byte layout is version-specific. On those
  boards the component *references* come out blank. The **electrical model
  is unaffected** (the EBAZ4205 still extracts 392 nets and 1742 netted
  pins, and its DRC is short-clean); only the human-facing labels are
  missing, which is why its KiCad cross-validation cannot perform the
  label-keyed join. Resolving `WideStrings6` would lift this limit.

- **Copper-pour fill is not modelled.** Pour *outlines* (and therefore the
  pour's net) are read, but the filled copper with its antipads / thermal
  reliefs (`Regions6` / `ShapeBasedRegions6`) is not. Consequently a pour
  does not participate in short detection (see the bug note above). Pad-,
  track-, via- and arc-level shorts are detected normally.

- **ASCII `.pcbdoc` is not yet supported.** Altium also has a text variant
  whose files begin `|RECORD=Board|` (e.g. SimpleFOCMini). We read only the
  binary OLE2 form today. hauksbee detects the ASCII form as "not a binary
  board" and currently falls through. An ASCII sample is kept in the
  maintainers' corpus against a future path; it is not in the public fetch.

- **`.SchDoc` (schematic) is not read.** The PCB was the priority because it
  carries full net connectivity in one file (the layout alone fully
  describes the circuit, exactly like a `.kicad_pcb`). The Altium schematic
  (`.SchDoc`, also OLE2 but with different record streams: `FileHeader`,
  pin / wire / net-label records) would only be needed for the "simulate
  before there's a layout" path, and we defer it. For Altium projects the
  `.PcbDoc` is the complete source of truth for connectivity.

- **Allegro / OrCAD / other binary EDA are out of scope.** This module reads
  Altium `.PcbDoc` only. Cadence Allegro `.brd` (a different binary format)
  is still ingested only through its gerbers (`docs/ingest/GERBER.md`), not
  natively.

- **Record types not yet handled:** `Fills6` (copper fills), `Dimensions6`,
  `Rules6` (so the DRC uses the default 0.2 mm clearance, not the board's own
  rule), `ComponentBodies6` / `Models` (3D), `Classes6` (net classes). None
  of these change net connectivity. `Fills6` copper would refine the DRC on
  the rare boards that use large copper fills outside pours.
