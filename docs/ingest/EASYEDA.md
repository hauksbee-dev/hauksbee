# EasyEDA Ingest

EasyEDA (and its JLCEDA edition) is the editor most hobby and small-run boards are drawn in, ordered straight from JLCPCB. Hauksbee reads its board files natively: drop the file and the parts, pads and nets come out the same way they do from a KiCad board.

## If you use EasyEDA

```bash
hauksbee run MyBoard.json --report --plain     # Standard edition export
hauksbee run PCB1.epcb --report --plain        # Pro edition board
hauksbee run MyProject.epro --report --plain   # Pro edition whole project
```

Content decides, not the extension, so a renamed or pasted file still reads, and `hauksbee-ci` accepts the same files as its `board` key.

**Which file.**

| Edition | What to export | File |
|---|---|---|
| Standard (6.x) | open the PCB, File, Export, EasyEDA source | `.json` |
| Pro (2.x) | save the board, or File, Save As, project | `.epcb`, or the whole `.epro` |

Open the board in the PCB editor before exporting. A schematic, a symbol or a footprint exports to the same file type and is refused by name ("this EasyEDA file is a schematic, not a PCB board").

A `.epro` is a zip of the project. If it holds several boards, the first (in path order) is read and the others are named in the report; they are never merged. Pass one on its own to check it.

## What is read

| Construct | Standard | Pro |
|---|---|---|
| Placed parts: designator, side, position, rotation | yes | yes |
| Pads and the net each is on, with the pad number | yes | yes (`PAD_NET` records) |
| Nets, including nets named only by copper | yes | yes |
| Part package, and attributes (Supplier Part, Manufacturer, ...) | yes | partly (see below) |
| Part value | yes, when the file carries one | no (kept in the project library) |
| Copper tracks, arcs, vias, pours, solid regions | counted | counted |

**Copper is counted, not measured.** The report says how many track segments (and their total length), arcs, vias and pours the board has, and on which layers. Clearance DRC and trace-geometry SI are recorded as not run, with the same wording every other connectivity-only input gets, because they need a layout format that carries checkable copper (KiCad, Eagle, Altium). If you need spacing checked, run hauksbee on the KiCad, Eagle or Altium layout too, or on the Gerbers.

## What is not read, and how you are told

Nothing is dropped silently. Every record the reader does not understand is named in the report with a count:

```
Found in the file and NOT read, so nothing in them was checked: 1 x TEXT on a copper layer.
Read past as carrying nothing electrical: 4 x board-outline graphics; 2 x non-plated mounting holes; 10 x silkscreen graphics; 3 x text labels.
```

"NOT read" is a coverage gap. "Read past" is positive accounting: records that carry nothing a check could use (silkscreen, outlines, labels, computed pour fill), listed so you can see they were looked at.

Other limits the report states in its own words:

* **Values.** EasyEDA keeps a part's value with the schematic symbol. A Standard board shows one only when the author set the footprint to display it; a Pro board never carries it. Such parts are marked unresolved and the value-dependent checks do not guess one. Add a BOM (`--bom`) to give them identities.
* **Pro footprints.** A Pro `.epcb` holds pad geometry and footprint names in the project's libraries, not in the layout. Without them only connectivity and placement are read, and pad positions are absent. The report says how many parts this affects.
* **Parts with no pads.** A Pro layout states a part's pads only through its `PAD_NET` records, so a part with none is read as board artwork (a logo, a fiducial) and listed as such.
* **Pro v3 log files** (`DOCHEAD` followed by `||`-joined records) are refused by name with the way out (export Gerber, ODB++ or IPC-2581, or save as `.epro`). They are not guessed at.
* **Pro dimensions** are in mils, Standard in 10 mil units; both are converted to millimetres at the reader.

## Hostile and damaged files

A truncated download, a hand-edited number, a million-character name and a deeply nested JSON bomb each get an actionable sentence that names the cause and the fix ("ends in the middle of the JSON: the file looks truncated ... Re-export the board from EasyEDA"), never a panic and never a half-read board passed off as whole. A coordinate that is not a finite number (`NaN`, `inf`, `1e999`) stops the read. Zip members are read under an inflation budget. Names that reach the report are cut to a bounded length.

## Provenance and the reuse decision

Written from scratch against EasyEDA's own published Standard format documentation and real exports; no code is copied. The EasyEDA Pro file spec is all-rights-reserved and was not used: that edition was read from real files and checked against the connectivity the files state. The full adopt/borrow/reject record (including why KiCad's EasyEDA importer, `easyeda2kicad`, `JLC2KiCad_lib` and `LC2KiCad` were not used) is in the module docs of `crates/hauksbee-extract/src/easyeda/mod.rs`.

Test fixtures are real public MIT-licensed boards with pinned sources, hashes and licence texts: `crates/hauksbee-extract/tests/fixtures/easyeda/README.md`. That README also states what is not covered: no public Pro `.epro` archive was found, so the archive path is tested with a zip built in the test from a real `.epcb` and a hand-written `project.json`; the `project.json` layout is an inference. There is no KiCad export of these boards upstream, so equivalence is checked against a known answer derived independently from the raw files, not against a converter.

## Entry points

* Reader: `hauksbee_extract::easyeda::{extract, extract_archive}` and `ExtractedBoard::{from_easyeda, from_easyeda_archive}`.
* Registry: `EasyedaReader` ("easyeda") and `EasyedaArchiveReader` ("easyeda-archive").
* Accepted-format list: `ACCEPTED_FORMATS` in `hauksbee-bind`'s `board_input.rs` (the one list the CLI, the web app's drop error and the file picker all read).
