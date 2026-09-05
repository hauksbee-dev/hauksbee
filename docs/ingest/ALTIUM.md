# Altium `.PcbDoc` ingest

```bash
hauksbee run MyBoard.PcbDoc --report --plain
hauksbee run MyBoard.PcbDoc --drc --plain
hauksbee run MyBoard.PcbDoc --lint --plain
```

No flag and no conversion: content sniffing (OLE2 magic `D0 CF 11 E0` plus
Altium record streams) selects the reader, so a `.PcbDoc` works wherever a
`.kicad_pcb` does, including as a spec's `board`.

## Accepted input

- **Binary `.PcbDoc`** (Altium Designer): connectivity and copper geometry.
- **ASCII `Protel_Advanced_PCB`** (EasyEDA and converters): connectivity only,
  no track/pour geometry, so no geometric DRC.
- A **Git LFS pointer** is refused with `git lfs pull` advice; an ASCII
  pipe-record file whose `KIND` is not `Protel_Advanced_PCB` is refused as an
  unsupported Protel export. Input failures exit 1.
- `.SchDoc` is not read; the `.PcbDoc` carries full connectivity.
- Cadence Allegro `.brd` is a different format; use its gerbers
  ([GERBER.md](GERBER.md)).

## What is read

Nets (`Nets6`), components (`Components6`), pads (`Pads6`), and copper
geometry (`Tracks6`, `Arcs6`, `Vias6`, `Polygons6` outlines) into the same
`ExtractedBoard` and R-tree DRC engine the KiCad and Eagle paths use. Older
files without the `6` suffix are tried too. Coordinates are Altium internal
units (1 unit = 2.54 nm).

Component identity: `UNIQUEID` (fallback `SOURCEUNIQUEID`) plus the
normalised `SOURCEHIERARCHICALPATH` is authoritative. A designator reused
across replicated channels gets the path appended (`C1@A/FLASH2`). Without
authoritative IDs, repeated records merge only when they share an
identically-netted pad and no pad disagrees; the merge carries
`reference_ambiguous`, and conflicting records stay distinct under
`@record-<id>` names with `duplicate_reference_conflict`. The binder leaves
conflicted or ambiguous identities open. Missing designators become stable
`UNK<record>` identities. Net ties are recognised only from the native
`COMPONENTTYPE=Net Tie` / `Net Tie (In BOM)` field.

## Limitations

- **Component value is best-effort.** The displayed value is usually a bound
  field resolved through `WideStrings6`, which is not parsed, so on many
  boards `Value` is blank and the part reports unresolved. Copper DRC and
  connectivity lint are unaffected; analog, AC, thermal and firmware results
  on those parts are limited. Supply values through a BOM
  ([BOM.md](BOM.md)) or a model entry ([MODELS.md](../models/MODELS.md)).
- **Newest-format designators**: some files store no `SOURCEDESIGNATOR`, so
  references become `UNK<record>` placeholders.
- **Copper-pour fill is not modelled.** Pour outlines (and nets) are read, but
  the filled copper with antipads (`Regions6`, `ShapeBasedRegions6`) is not,
  so a pour contributes no edges to the short sweep. Pad, track, via and arc
  shorts are detected normally.
- **Not handled**: `Fills6`, `Dimensions6`, `Rules6` (the DRC uses the 0.2 mm
  default clearance), `ComponentBodies6`/`Models`, `Classes6`. None change
  connectivity.
- Octagonal pads use KiCad's 0.25 corner-cut ratio.

Entry points: `ExtractedBoard::from_altium_pcb(bytes)`,
`ExtractedBoard::from_protel_ascii(text)`, `ExtractedBoard::altium_drc(bytes)`,
`ExtractedBoard::from_auto_bytes(bytes)`. Record layouts are ported from
KiCad's Altium importer, read through the `cfb` crate. Synthetic fixtures:
`crates/hauksbee-extract/tests/altium.rs`, `protel_ascii.rs`.
