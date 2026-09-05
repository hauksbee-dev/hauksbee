# BOM and pick-and-place ingest

A layout gives a footprint and a value string; neither names a part. The BOM
and the pick-and-place file carry real identity (manufacturer part numbers)
and are reconciled with the layout before binding.

```sh
hauksbee run board.kicad_pcb --bom bom.csv --placement positions.csv --report [--json]
hauksbee run board.kicad_pcb --bom bom.csv --bom-column 'reference=Customer Reference' --report
```

```toml
# hauksbee-ci spec (paths relative to the spec)
board = "hardware/board.kicad_pcb"
bom = "hardware/bom.csv"
bom_columns = ["reference=Customer Reference"]
placement = "hardware/positions.csv"
variant = "hardware/prototype.variant.toml"
```

`--bom-column ROLE=HEADER` (repeatable) accepts the roles `reference`,
`value`, `mpn`, `manufacturer`, `quantity`, `footprint`, `populate`,
`distributor_part`. `hauksbee-ci check` reconciles the same inputs. Every JSON
result carries an `inputs[]` inventory for the board, BOM and placement file
(hash, contributions, ignored fields, identity changes); `--list-nets --json`
stays a bare array.

## Dialects

Both readers detect the dialect from content, never the extension: KiCad
grouped and ungrouped BOMs, KiCad position files (csv and ascii), Altium BOM
and pick-and-place (csv and fixed-width), Eagle partlists (per part and
grouped), LCSC/EasyEDA and JLCPCB assembly BOMs, CPL files, Digi-Key exports,
and hand-maintained spreadsheets; comma-, semicolon- or tab-separated; UTF-8
or Windows-1252. Banner lines before the header are skipped, fixed-width
columns are sliced from blank character positions, and reference lists split
on commas, semicolons or spaces.

## Column mapping

Every mapping carries a tier: **certain** (`Designator`, `Reference(s)`,
`MPN`, `Manufacturer Part Number`, `Quantity`, `Footprint`, `DNP`, `Value`;
`Part`/`Device` inside an Eagle partlist), **likely** (`Val`, `Ref`, `Parts`,
`Package`, `LibRef`, `PartNumber`; `Comment` in an Altium/LCSC/JLCPCB export),
or **guess** (`Part`, `Component`, `Description`, `Customer Reference`,
`Status`). A guessed reference column is promoted when every cell parses as
designators.

Non-interactive contract: a run proceeds when every column it uses reached
`likely` or better and records the mapping, or refuses with **exit 3** naming
the ambiguous column and the `--bom-column` that settles it. A guessed
non-reference column is left unmapped and named.

```
bom.csv (kicad_grouped_bom, sha256 5296073e)
  reference <- "Reference(s)" (certain)
  value <- "Value" (certain)
  mpn <- "MPN" (certain)
  contributed: part identity: 60 reference designators over 22 rows, 1 of them carrying a manufacturer part number
  ignored:     column "Datasheet": no analysis reads it
```

## Refusals

Each names the file, the problem and one next action:

- not a bill of materials (no designator column beside a value/MPN/footprint/quantity column);
- a purchase list (designator column present but empty);
- no column confidently the reference;
- two columns equally entitled to one role (`MPN` beside `Manufacturer Part Number`; two manufacturer columns; `Value` beside `VALUE` with differing cells). Distributor order-code ties are not refusals;
- a repeated identity across two BOM rows, or a repeated designator in a placement file;
- a multiline quoted CSV record (re-export);
- a position file that places nothing;
- fewer than half the BOM's designators on the board (a BOM for a different board);
- a placement whose shared coordinates disagree beyond 0.01 mm, side disagrees, or rotation disagrees beyond 0.1 degrees, or with no comparable shared position.

## Precedence

1. The layout decides whenever its value resolves the part.
2. A BOM/placement manufacturer part number decides only where the layout
   resolved nothing.
3. Two files naming different parts for one designator refuse (different
   device kinds, or different models of the same kind).
4. A BOM or placement `Value` column only fills an empty layout value (every
   part on an Altium `.PcbDoc`, for example); it never outranks the layout's.
5. A magnitude disagreement (`10k` vs `4k7`) is reported; the layout's number
   is used.

Distributor order codes (LCSC `C1525`, Digi-Key `311-15LRCT-ND`) are never
identity. Part-number compatibility is exact alphanumeric identity, plus the
ordering suffixes `-AU` (Atmel/Microchip), `-7-F` (Diodes Inc) and `,215`
(Nexperia) when the BOM names that vendor. A shared prefix is not identity.

Contradiction detectors: two files resolving two parts; two dimensions (`10k`
vs `100nF`); a designator against a stated dimension (`C4` = `10 uH`); a
passive value on a semiconductor; two explicit manufacturers; package family
or electrical-pad count against the footprint's numbered pads. Contradictions
are gathered before anything is applied, so a refused BOM leaves the board as
the layout described it.

## Mismatches that are reported, not fatal

| Case | Result |
|---|---|
| BOM designator not on the board | named (mechanical parts, panels) |
| board part absent from the BOM | named (test points, fiducials) |
| BOM quantity disagrees with its designator list | named; the list wins |
| board part the placement file does not place | named |
| layout DNP vs BOM `populate=yes` | `--fit REF` advice, never applied |
| no layout DNP vs BOM `DNP=yes` | `--no-fit REF` advice |

The BOM's populate column is advice only; the DNP policy ([DNP.md](DNP.md))
and the `variant` artifact are the single place fitting is decided. A
successful placement reconciliation records how many positions, rotations and
sides were compared and whether the Y axis was mirrored or the origin offset
(a frame is inferred only from three or more comparable positions).

## Limits

- A reference range (`R1-R4`) is not expanded.
- An MPN unlocks a bind only when some model matches it.
- The BOM cannot correct a layout value.
- The gerber-only path has its own tolerant BOM reader ([GERBER.md](GERBER.md)).
