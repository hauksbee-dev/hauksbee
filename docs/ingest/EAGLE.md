# Eagle `.brd` Ingest

Autodesk Eagle authored most of the hobby and maker hardware that exists:
Arduino, Adafruit, and SparkFun shipped their reference designs as `.brd`
files, and a large amount of small-company hardware is still maintained in
Eagle or in Fusion 360 Electronics, which is Eagle's successor. Those boards go
straight into hauksbee.

```bash
hauksbee run my_board.brd --report --plain   # which parts were modelled
hauksbee run my_board.brd --drc --plain      # shorts and clearance from the copper
hauksbee run my_board.brd --lint --plain     # design lint in plain language
hauksbee to-code my_board.brd                # decompile to editable Board-as-Code
```

There is no flag and no conversion step. `hauksbee run` sniffs the file
content, so a `.brd` works wherever a `.kicad_pcb` does, including as the
`board` key of a `hauksbee-ci` spec.

## Where your `.brd` comes from

- **Eagle 6 or later**: the `.brd` in your project directory, as is. Eagle 6
  moved to an XML file format, and that is what hauksbee reads.
- **Fusion 360 Electronics**: Fusion's electronics workspace grew out of Eagle
  and can write the Eagle board format. Save or export a `.brd` and point
  hauksbee at that. Fusion's own native project files are not read.
- **Anything older than Eagle 6**: the pre-6 `.brd` is a binary format, and
  hauksbee does not read it. Open it once in Eagle 6+ or Fusion and re-save,
  which converts it to XML.
- **No CAD at all**: take the gerber route below.

## What the reader extracts

Connectivity in an Eagle `.brd` is explicit rather than inferred: `<signals>`
lists every net with a `<contactref>` per connected pad. hauksbee reads:

- **Nets**, from `<signals>`, with XML entities in net names decoded.
- **Components**, from `<elements>`: reference, value, library and package
  name, placement, rotation, board side, and the do-not-populate flag.
- **Pins**, by resolving each element's package from `<packages>` and placing
  its pads and SMDs at absolute coordinates, with each pad's net attached.
  Packages are keyed per library, so two embedded libraries that each define a
  package named `0805` stay distinct.
- **Copper geometry per net**, separately, for the DRC: wires, vias, pads,
  SMDs, polygons, rectangles, and circles, on numbered copper layers (1 = top,
  16 = bottom, 2 to 15 inner). That geometry feeds the same R-tree short and
  clearance engine the KiCad path uses. There is one detection engine, not one
  per format.

Everything downstream is format-blind from that point: binding, the static
checks, the analog solve, and firmware co-simulation all work off the extracted
board, so the Eagle path gets the same treatment a KiCad board gets.

## What it does not extract

- **Eagle schematics (`.sch`) are not a netlist source.** The layout alone fully
  describes the circuit, so nothing is bound or simulated from the schematic;
  hauksbee's schematic-derived netlist path covers KiCad `.kicad_sch` only. See
  [`SCHEMATICS.md`](SCHEMATICS.md). One thing IS read from an Eagle `.sch` when it
  is supplied, because the `.brd` cannot express it: the **net ties the schematic
  declares** through Eagle's supply-symbol construct. A `.brd` records no net ties
  at all, so without the schematic a deliberate star ground is reported as a
  serious short. Pass `--schematic <FILE>`, or leave the `.sch` beside the board
  under the board's own name and it is found automatically. See
  [`../checks/SHORTS.md`](../checks/SHORTS.md), "Declared ties read from the
  schematic".
- **Board design rules** from the `.brd` are not applied to the DRC, which uses
  its default clearance rather than the board's own rule. Findings are reported
  against that default.
- **Pin electrical roles** (input, output, power, passive) are not present in
  the `.brd`, so they come out blank. Binding works off value, footprint, and
  connectivity, which is what the checks need.
- **Pre-Eagle-6 binary `.brd`** is not read; nor is Cadence Allegro's
  unrelated `.brd`, which is a different binary format entirely and is ingested
  only through its gerbers.

A pre-Eagle-6 `.brd` is recognised as itself rather than lumped in with files
hauksbee has never heard of, because the difference matters: this design becomes
readable after one re-save, and reciting the accepted-format list would send you
looking for a tool that does not exist.

The recognition is the drawing record's first two bytes, `0x10 0x00` for the
Eagle 4.x/5.x layout and `0x10 0x80` for 3.x, checked on the file's RAW bytes
before anything decodes them. Ahead of everything else, deliberately: the text
readers work from a lossy UTF-8 decode that destroys the header, and the
IPC-D-356 reader claims any input carrying a line that starts `317`, which
binary board records sometimes do. A binary Eagle board that tripped that used
to come back as a *report*, with parts invented out of binary noise.

```
$ hauksbee run braids_v50.brd --report
error: 'braids_v50.brd': this is an Eagle drawing in the pre-Eagle-6 BINARY format, which hauksbee does not read. Eagle 6 moved the .brd and .sch formats to XML, and the XML form is what hauksbee reads: open this file once in Eagle 6 or later, or in Fusion 360 Electronics, re-save it, and retry with the re-saved file. Anything that opens the pre-6 binary format and writes Eagle XML or a KiCad board will do; failing that, the design's gerbers are the other way in. See https://docs.hauksbee.dev/docs/ingest/eagle
```

A file that is no board format at all gets the accepted-format list instead,
because for that file the list is the answer:

```
$ hauksbee run mystery.brd --report
error: 'mystery.brd': unrecognized board format: hauksbee reads a KiCad board, schematic or netlist, an Eagle board, an Altium .PcbDoc (binary or ASCII), an IPC-D-356 netlist, or a folder or zip of gerbers
```

Both refusals are held to their word on real files rather than on a fixture.
`corpus.toml` fetches the 35 Mutable Instruments Eurorack modules
(`eurorack_binary_eagle`) purely for this: they are Eagle 5 and earlier, so their
`.brd` and `.sch` are both binary, and the pre-Eagle-6 message above is the only
correct output for all 70 of those drawings.
The entry carries the axis `unreadable-by-design` and is counted as a refusal,
never as board coverage. A corpus sweep that reported 35 more boards because these
landed would be reporting files it could not open.

Three of the boards (Braids, Clouds and Blinds) are the entry's declared inputs and
so are the three the release browser gate drops through a real Chromium journey. It
requires exactly what the CLI does and a little more: a refusal that names the
re-save, no JSON export offered, no parts/nets inventory, no live-simulation action,
and a way to try another file.

The Eagle XML side of the corpus spans 6.4 to 9.6.2 across 20 layouts and 16
schematics: the Adafruit and SparkFun boards, the official Arduino Uno Rev3 release,
and the SparkFun MicroMod processor boards.

A Git-LFS pointer is detected as itself on the same reasoning, because its fix is
specific too:

```
$ hauksbee run board.brd --report
error: 'board.brd': this is a Git LFS pointer, not the board file itself: the repository stores the real file in Git LFS and it was never downloaded. Run `git lfs install && git lfs pull` in the repository, then retry with the real file
```

So if your `.brd` is a few hundred bytes of text starting `version
https://git-lfs.github.com/spec/v1`, run `git lfs pull` first. Every one of these
messages exits 1, a hard input error rather than a finding.

## What to expect from bind rates

Eagle stores a displayed value per element, and passives carry real values, so
resistors and capacitors bind from their values. ICs bind when hauksbee has a
model for the part; the rest report as unresolved and are simulated as open
rather than silently guessed. Measured on public Eagle boards from the corpus
(`scripts/fetch-corpus.sh`), with no user model directory:

| Board | Parts resolved |
|---|---|
| Arduino UNO Rev3e | 27 of 54 (50%) |
| SparkFun RedBoard | 37 of 45 (82%) |
| Adafruit Feather M0 Basic rev C | 19 of 28 (68%) |
| Adafruit QT Py | 12 of 23 (52%) |

Copper checks are unaffected by bind coverage: all four boards return a clean
`--drc`. An unresolved part only limits the analog, AC, thermal, and firmware
results on its own nets, and the report's bottom line says exactly which. To
close the gap, add the parts you care about: one small TOML file each, no
recompile ([`../extending/add-an-analog-part.md`](../extending/add-an-analog-part.md)).

## The gerber fallback

If the design predates Eagle 6, lives in a tool nobody exports from, or you
only ever received fab output, hauksbee can reverse-extract the board from
gerbers plus a pick-and-place file. That path works for every EDA tool that can
produce fab output, which is all of them. It recovers connectivity from copper
geometry alone, and it is honest about what geometry cannot tell it. See
[`GERBER.md`](GERBER.md).
