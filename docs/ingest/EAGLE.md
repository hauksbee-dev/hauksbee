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

- **Eagle schematics (`.sch`)** are not read. The layout alone fully describes
  the circuit, so this is a gap rather than a blocker: hauksbee's schematic
  path covers KiCad `.kicad_sch` only. See
  [`SCHEMATICS.md`](SCHEMATICS.md).
- **Board design rules** from the `.brd` are not applied to the DRC, which uses
  its default clearance rather than the board's own rule. Findings are reported
  against that default.
- **Pin electrical roles** (input, output, power, passive) are not present in
  the `.brd`, so they come out blank. Binding works off value, footprint, and
  connectivity, which is what the checks need.
- **Pre-Eagle-6 binary `.brd`** is not read; nor is Cadence Allegro's
  unrelated `.brd`, which is a different binary format entirely and is ingested
  only through its gerbers.

If hauksbee cannot read the file it says so and names every format it does read,
rather than guessing:

```
$ hauksbee run mystery.brd --report
error: 'mystery.brd': unrecognized board format: hauksbee reads a KiCad board, schematic or netlist, an Eagle board, an Altium .PcbDoc (binary or ASCII), an IPC-D-356 netlist, or a folder or zip of gerbers
```

That is what a binary pre-Eagle-6 `.brd` looks like. A Git-LFS pointer does
*not*: it is detected as itself, because the fix is specific and worth naming:

```
$ hauksbee run board.brd --report
error: 'board.brd': this is a Git LFS pointer, not the board file itself: the repository stores the real file in Git LFS and it was never downloaded. Run `git lfs install && git lfs pull` in the repository, then retry with the real file
```

So if your `.brd` is a few hundred bytes of text starting `version
https://git-lfs.github.com/spec/v1`, run `git lfs pull` first. Both messages
exit 1, a hard input error rather than a finding.

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
