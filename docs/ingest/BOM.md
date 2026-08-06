# BOM and pick-and-place ingest

A layout gives a footprint and a value string. Neither of those names a part. A
footprint is a pad pattern thousands of devices share. A value string is a human
label whose meaning is convention: `10k` is a resistance, `AO3400A` is a part
number, `DNP` is an instruction, and `~` is KiCad's way of writing nothing.

So a board whose layout says `10k` binds fine, and a board whose layout says
nothing useful binds unresolved, even when the BOM sitting beside it names the
exact manufacturer part number. The BOM and the pick-and-place file are the two
artifacts that carry real identity, and every real project ships them.

Two rules shape everything below:

1. **Autodetect the dialect, then ask rather than guess.** There is no BOM
   format. There are the shapes each CAD tool exports, the shapes the assembly
   houses accept, the shapes the distributors' cart pages produce, and the
   spreadsheet somebody maintains by hand.
2. **A confident wrong answer is worse than a refusal.** A mis-bound part makes
   the report authoritative about a device that is not on the board, and every
   number downstream inherits that. Erroring is fine. Mis-binding is not.

The executable path is direct:

```sh
hauksbee run board.kicad_pcb --bom bom.csv --placement positions.csv --report
hauksbee run board.kicad_pcb --bom bom.csv --placement positions.csv --report --json
```

An ambiguous BOM header can be confirmed explicitly:

```sh
hauksbee run board.kicad_pcb --bom bom.csv \
  --bom-column 'reference=Customer Reference' --report
```

Both artifacts are reconciled before binding. A refusal leaves the board
unchanged. `--report --json` includes a typed `inputs[]` inventory for the
board, BOM and placement file, including hashes, contributions, ignored fields
and identity changes.

## What is read

Both readers detect the dialect from the file's content, never from its
extension. That is not fussiness: KiCad writes csv into a file called `.pos`,
Altium writes fixed-width text into a file called `.csv`, and tab-separated
exports routinely claim to be comma-separated.

The counts are from a survey of 664 real BOM and pick-and-place files gathered
from public hardware projects on GitHub. They are counts of what the survey
found, not a random sample of the world: the search deliberately went looking
for each shape, so the numbers say "this shape is common enough to find dozens
of" rather than "this share of all BOMs".

| Shape | Files |
|---|---|
| Altium Pick and Place (csv and fixed-width `.txt`) | 81 |
| CPL, the generic shape JLCPCB and PCBWay accept | 80 |
| Eagle partlist, per part | 58 |
| KiCad grouped BOM | 53 |
| LCSC / EasyEDA assembly BOM | 50 |
| JLCPCB assembly BOM | 43 |
| Altium BOM | 40 |
| KiCad position file, ascii | 36 |
| KiCad position file, csv | 34 |
| KiCad ungrouped BOM | 31 |
| Hand-maintained spreadsheet BOM | 31 |
| Eagle partlist, grouped by value | 17 |
| Digi-Key BOM export | 11 |

565 of the 664 read. The other 99 are refused, and what they are is as
interesting as what read; see [Refusals](#refusals).

Three details in that table cost real work and are worth naming, because a
reader that misses any one of them silently returns wrong numbers:

- **The banner.** A KiCad grouped BOM spends six lines on the source schematic,
  the date, the generator script and a component count before its header row.
  An Altium pick-and-place spends up to thirteen on the project path and the
  units. Both readers scan for the header rather than assuming line one.
- **Fixed-width columns are sliced from the data, not the header.** Eagle and
  Altium left-align their columns; a KiCad position file right-aligns its three
  numeric columns, so a number begins several characters left of its own header
  and slicing from the header's offset reads `2550` out of `74.2550`. The
  boundaries therefore come from the character positions that are blank in the
  header and in every data row, which is exactly what makes the file readable by
  eye and is alignment-agnostic.
- **Reference lists separate on anything.** KiCad's grouped export writes
  `"C1, C2, C3"`, its ungrouped export and the Digi-Key shape write `C1 C2 C3`,
  a hand-maintained sheet writes `"C3, C2, C1, "` with a trailing separator, and
  a semicolon-delimited European export uses semicolons for its fields. Both
  separators are real, so both are accepted.

## Column mapping, and what "confident" means

Detecting a column is not a yes-or-no matter, so every mapping carries a tier,
and the tier is what decides whether a run may act unasked.

| Tier | Means | Examples |
|---|---|---|
| certain | The header is an unambiguous name for the role, or the dialect defines it | `Designator`, `Reference(s)`, `MPN`, `Manufacturer Part Number`, `Quantity`, `Footprint`, `DNP`, `Value`; `Part` and `Device` inside an Eagle partlist |
| likely | The header means the role in a recognised dialect, or is a widely used abbreviation, but is not self-explanatory alone | `Val`, `Ref`, `Parts`, `Package`, `LibRef`, `Quantity Per PCB`, `PartNumber`; `Comment` inside an Altium, LCSC or JLCPCB export |
| guess | The header only plausibly means the role | `Part`, `Component`, `Description`, `Customer Reference`, `Status`, `Assembly Class` |

A header literally called `MPN` is not a guess. A header called `Part` is.

Two mechanisms make this workable rather than merely strict.

**The dialect sharpens the ambiguous cases.** `Comment` is the value column in an
Altium, LCSC or JLCPCB export and a free-text note anywhere else, so it is
`likely` in the first three and a `guess` in a spreadsheet. `Part` is the Eagle
partlist's reference column and a guess anywhere else.

**A guessed reference column can be confirmed by its own content.** A Digi-Key
BOM-manager export keeps its designators under `Customer Reference`, a header
that means nothing on its own. But if every cell in that column splits into
tokens shaped like reference designators, one to four letters then digits, short,
then it IS the reference column, and that is evidence rather than a guess. The
check is discriminating: a distributor order code (`296-1566-5-ND`) starts with a
digit, a packaging cell (`Cut Tape`) has no digit, a description is too long. Ten
of the surveyed Digi-Key files still fail it, because their `Customer Reference`
column really does mix designators with free text like `PH Crimp Connectors`.
Those refuse.

Every run records the mapping it used, so the choice is never invisible:

```
bom.csv (kicad_grouped_bom, sha256 5296073e)
  reference <- "Reference(s)" (certain)
  value <- "Value" (certain)
  mpn <- "MPN" (certain)
  quantity <- "Qty" (certain)
  footprint <- "Footprint" (certain)
  contributed: part identity: 60 reference designators over 22 rows, 1 of them carrying a manufacturer part number
  ignored:     column "Item": no analysis reads it
  ignored:     column "Datasheet": no analysis reads it
```

### The non-interactive contract

A run with nobody watching it is the main consumer, since this feeds CI, and it
must never block on a question. So it does exactly one of two things:

- proceeds on a mapping where every column it uses reached `likely` or better,
  and records that mapping; or
- refuses with **exit 3**, naming the ambiguous column and the flag that settles
  it.

Exit 3 is "invalid for analysis", the same code a diverged co-simulation
produces. A BOM that cannot be mapped is not a failed assertion (exit 1) and not
a usage error (exit 2): it is an input that cannot be read truthfully, which is
what 3 means. See [`ci/CI.md`](../ci/CI.md).

A guess-tier column for any role other than the reference is not a refusal. It is
left unmapped and named in the report, because leaving the layout's own value in
charge is the status quo, and the status quo beats a wrong answer.

An interactive caller has the same information: the refusal text names the
column, and the mapping record names every column that was left unmapped and the
flag that would map it.

## Refusals

Each of these states what is wrong, which file, and one concrete next action,
because a refusal whose message does not say what to do is a dead end.

**Not a bill of materials at all** (51 of the 664). Most are the non-hardware
CSVs the survey's search dragged in: Reddit comment dumps, prompt libraries, an
HL7 field table. Refusing them is the property this exists for.

```
prompts.csv does not read as a bill of materials: no row in its first 40 lines
has a reference-designator column beside a value, part-number, footprint or
quantity column. hauksbee reads KiCad, Altium, Eagle, LCSC/JLCPCB, Digi-Key and
hand-maintained spreadsheet BOMs, comma-, semicolon- or tab-separated. If this
really is a BOM, name its reference column explicitly with
`--bom-column reference=Designator`
```

**A purchase list rather than a BOM** (13). A distributor cart export carries
real identity and no designators at all, so nothing in it can be attached to a
part.

```
Bom.csv has a "Reference Designator" column but it is empty on all 9 rows, so
nothing in this file can be attached to a part on the board. A distributor cart
export (Digi-Key, Mouser) usually looks like this: it is a purchase list, not a
BOM. Re-export it with the reference-designator field filled in, or point
hauksbee at the BOM your CAD tool wrote
```

**No column that is confidently the reference** (10).

```
bom-digikey.csv has no column hauksbee is confident is the reference designator.
The closest is "Customer Reference", which is a guess, and a guess here attaches
every part number in the file to the wrong part. Confirm it with
`--bom-column reference=Customer Reference`, or name the right column the same
way. The columns in the file are: ...
```

**Two columns equally entitled to one role** (1). A file carrying both `MPN` and
`Manufacturer Part Number` says nothing about which is authoritative, and picking
the first would bind whichever the exporter happened to write first. A tie
between two columns nothing reads for identity, two distributor order codes or
two manufacturer-name columns, is not a refusal: the first is taken and the rest
recorded as ignored. Two columns spelled the same way (`Value` beside `VALUE`)
are accepted only when every row carries equivalent cells. If the cells differ,
the file refuses: column order is not an authority rule.

Rows are subject to the same uniqueness rule. A designator appearing in two BOM
rows, or twice in one placement file, refuses with both source lines. A
multiline quoted CSV record is refused with its first line and re-export
guidance; it is never split into two apparent parts. UTF-8 and Windows-1252
spreadsheet exports are decoded explicitly.

**A position file that places nothing** (24). A side-specific export does this
when that side of the board is empty. It is refused rather than read as an empty
success, because reconciling a board against it would report every part on the
board as unplaced, which is a confident wrong answer about the assembly.

## What the BOM changes about binding

An MPN from a BOM lets a part bind that the layout's value string could not.

```
  U9 identified from bom.csv as "MCP4728": unresolved -> exact
```

Every such bind is attributed to the file that supplied it, per part, so a reader
can tell which parts were identified from which artifact.

### The precedence

1. **The layout decides whenever it can.** If the layout's value resolves the
   part exactly or by family, that reading stands. The layout is the file the
   netlist itself came from, so it is the description of the circuit; a BOM
   describes a purchase, and it goes stale between revisions in a way the layout
   cannot.
2. **A part number decides only where the layout could not.** Where the layout
   resolves nothing, an artifact's manufacturer part number settles it. This is
   the whole gain, and it takes nothing away, because the layout said nothing.
3. **Two files naming different parts for one designator is refused.** Not
   merged, not averaged. See below.
4. **A BOM's value column never outranks the layout's.** It is the same kind of
   claim. It only fills a hole: a part whose layout value is empty, which is
   every part on an Altium `.PcbDoc`, since Altium keeps values in the schematic.
   A pick-and-place file's `Val` column works the same way and is never treated
   as a part number.
5. **A magnitude disagreement inside one part is reported, not acted on.** The
   layout says `10k` and the BOM says `4k7`: same device, different number. The
   layout's number is used and the disagreement gets a line, because a number
   that changed between revisions is worth saying and is not worth refusing a run
   over.

A distributor order code is never identity. An LCSC `C1525` or a Digi-Key
`311-15LRCT-ND` carries a distributor prefix, is not a manufacturer part number,
and matching a model against one is how a part binds to the wrong device. Those
columns are read, recorded, and dropped before they reach the binder.

### Contradictions are refused

A refdes the BOM calls a 10k resistor and the layout calls a MOSFET is real
evidence that the two files describe different revisions of the board.

```
bom.csv and the board disagree about what 1 of the same parts ARE, which means
they are different revisions of the board rather than two views of one: Q5 is
"BSS138" (nmos) on the layout and "10k", a passive value, in the BOM. Anything
computed from the pair would describe a board that does not exist. Use the BOM
that was exported from this layout, or drop it and analyse the layout alone
```

Six independent detectors find these, because each alone misses cases the others
catch:

- **Two files, two parts.** The layout resolves a model and the BOM's part number
  resolves a different one. Different device kinds is the obvious case; a
  different model of the same kind is the same problem in a quieter voice, two
  processors or two regulators, and gets the same refusal, because a run that
  silently swaps the simulated chip would evaluate every firmware assertion
  against a core the board does not have.
- **Two dimensions.** `10k` against `100nF` on one designator is ohms against
  farads. A bare magnitude states no unit, so the designator supplies it: `10k`
  on an `R` is ohms and on a `C` is farads, which is the convention every value
  string relies on.
- **A designator against a stated dimension.** A `C4` the BOM calls `10 uH`.
- **A passive value on a semiconductor.** The case this feature exists for: the
  layout resolves `Q5` as a MOSFET and the BOM's value is a bare magnitude. A
  transistor's value is never one.
- **Two explicit manufacturers.** A BOM manufacturer conflicts only with a
  manufacturer property the layout actually carries. A missing property is
  unknown, never guessed.
- **Package identity.** Recognized package families and explicit pin counts are
  compared against the layout footprint and its actual pad count. Free-form
  package prose that cannot be normalized is not promoted to certainty.

Part-number compatibility is deliberately narrow. Exact alphanumeric identity
is accepted, as are the documented ordering suffix forms `-AU`, `-7-F` and
`,215`. A shared prefix is not identity: `TPS62130` and `TPS62135`, or
`ATmega328P` and `ATmega328PB`, remain different parts.

Contradictions are gathered before anything is applied, so a refused BOM leaves
the board exactly as the layout described it.

A BOM whose designators mostly are not on the board is the same problem stated
differently, and gets its own refusal:

```
bom.csv names 10 reference designators and only 0 of them are on this board, so
it is a BOM for a different board. Check which file goes with which layout, then
retry
```

At least half of the BOM's distinct designators must be on the board. Mechanical
parts and panel extras are ordinary, but they cannot outnumber the actual board
population. Ten matches in a hundred is refused, not accepted at a boundary.

A pick-and-place file gets the same treatment on its own terms. It states where
each part sits, the layout states the same thing, and any shared coordinate
disagreement beyond 0.01 mm, side disagreement, or rotation disagreement beyond
0.1 degrees refuses before identity is applied. Angles compare modulo 360. The
file also refuses when no shared designator has a comparable layout position,
or when at least half of its placements are not on the board.
The tolerance is deliberately far tighter than any real placement difference:
every writer surveyed emits four decimal places or better, so the only thing it
absorbs is rounding, and moving a part by a tenth of a millimetre between
revisions is a change worth noticing.

A missing or unrecognized side remains `unknown`; it is recorded and omitted
from the side comparison, never read as top. A missing rotation likewise
remains absent rather than becoming zero degrees.

### The mismatch cases

| Case | What happens |
|---|---|
| A refdes in the BOM that is not on the board | Reported by name. Ordinary in small numbers: a BOM covers mechanical parts with no footprint, a panel, or a variant. If fewer than half of the BOM designators match, it is a different board and refuses |
| A board part absent from the BOM | Reported by name, never fatal. A BOM legitimately omits test points and fiducials. Worth saying because the BOM was the artifact that could have identified them |
| A BOM row's quantity disagreeing with the number of designators the same row lists | Reported. The list wins: it is the enumerated fact and the quantity is a number derived from it |
| A part on the board that the pick-and-place file does not place | Reported. Ordinary: only the SMD side gets placed, and KiCad excludes test points and artwork placeholders from position files |
| An explicit layout DNP marker against a BOM `populate=yes` | Reported, and turned into `--fit` advice. Never applied. See below |
| No explicit layout DNP marker against a BOM `DNP=yes` | `--no-fit` advice without a fabricated disagreement: the layout state is unspecified, not explicitly fitted |

A quantity that disagrees with the number of placements is not a separate case.
It is a designator missing from one side or the other, which the first two rows
already report by name.

### Do-not-populate stays one decision

hauksbee already has a DNP policy, and it is the single place the question "is
this part fitted?" is decided; see [`DNP.md`](DNP.md). A second mechanism
quietly overriding it from a purchasing spreadsheet is exactly the kind of hidden
second opinion that policy exists to prevent.

So the BOM's populate column becomes advice:

```
  R1 is do-not-populate on the layout and populated in bom.csv; the
  do-not-populate policy decided it, not the BOM
  the BOM says these DNP parts are populated: R1. Re-run with --fit R1 to
  honour it
```

The advice feeds straight into the same `--fit` / `--no-fit` lists the policy
already takes, so honouring the BOM is one flag and is recorded in the run's own
DNP report rather than in a second place. The half worth having is the other
direction: a BOM saying a part is not populated where the layout does not mark
it DNP is information the board file does not carry at all. In that case
`dnp=false` means unspecified, not an explicit fitted claim, so the run gives
`--no-fit` advice without inventing a conflict.

## Provenance

Every read records what the artifact contributed, what was ignored and why, and
the SHA-256 of the exact bytes. The BOM is the artifact most likely to be edited
between runs, so pinning it is the difference between a reproducible identity
claim and a guess about which revision was used.

```
bom.csv (lcsc_bom, sha256 38141a87)
  reference <- "Designator" (certain)
  value <- "Comment" (likely)
  footprint <- "Footprint" (certain)
  distributor_part <- "LCSC" (certain)
  contributed: part identity: 21 reference designators over 14 rows, 0 of them carrying a manufacturer part number
  ignored:     distributor order codes: a distributor code is not a manufacturer part number, so matching a model against one would bind the wrong device
```

The shape is deliberately minimal, and its field names match the typed
provenance the evidence work defines, so it is absorbed by that rather than
becoming a second vocabulary for the same idea.

## Honest limitations

- **The CLI surface is live; the `hauksbee-ci` spec surface is not.** `run`
  accepts `--bom`, repeatable `--bom-column`, and `--placement`. The same typed
  fields are not yet accepted in a `hauksbee-ci` project specification.
- **A reference range is not expanded.** A BOM that writes `R1-R4` in one cell
  yields one designator called `R1-R4`, which then reports as not on the board.
  No file in the survey used the form; a BOM that does will say so loudly rather
  than quietly cover three fewer parts than it looks like.
- **A part number is only as good as the model library.** An MPN unlocks a bind
  only when some model matches it. An MPN for a device nothing models leaves the
  part exactly as unresolved as before, which is the honest outcome.
- **The BOM cannot correct a value.** By precedence rule 5 a magnitude
  disagreement is reported and the layout's number is used. If the BOM is the
  current one and the layout is stale, the report says so and the fix is to
  update the layout, not to pass a flag.
- **A variant BOM is not modelled.** An assembly variant that populates a
  different set of parts under one layout reads as a BOM disagreeing with the
  layout about which parts are fitted, and produces populate advice rather than a
  variant.
- **Multiline CSV cells require a re-export.** They are refused explicitly
  rather than misparsed. Single-line quoted cells, including embedded
  delimiters and grouped reference lists, remain supported.
- **The gerber-only path has its own reader.** When the fab package is the whole
  design there is no layout to reconcile against, so guessing is the best answer
  available and refusing would mean refusing the board. That path keeps its
  tolerant reader; see [`GERBER.md`](GERBER.md).
