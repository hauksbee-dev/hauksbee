# Gerber + pick-and-place reverse extraction

A board that ships only manufacturing files (RS-274X copper, an Excellon or
gerber drill, a pick-and-place CSV, maybe a BOM) is reconstructed into the
same `ExtractedBoard` every other reader produces, so bind, DRC, lint, stress
and simulation work on it.

```
hauksbee run fab/ --report            # a job directory or a .zip
hauksbee run fab.zip --drc --plain
```

Entry points: `ExtractedBoard::from_gerber(path)`,
`ExtractedBoard::from_gerber_with_stats`, `gerber::from_gerber_dir` (returns
`ReconStats`). Browser diagnostics: [IMPORT_DIAGNOSTICS.md](IMPORT_DIAGNOSTICS.md).

## Pipeline

1. **Classify** every file (recursing sub-directories) as copper / drill /
   outline / ignore.
2. **Parse** copper into solid primitives, the drill into plated holes, the
   pick-and-place into placements.
3. **Reconstruct** connectivity: copper that touches is one net (R-tree
   pruned union-find), plated holes stitch the layers, each placed component
   claims the flashes nearest it as pads. Nets are named `NET_n`; the largest
   pour-touching net is *labelled* `GND` (a label, not a fact).

## What is read

| Input | Notes |
|---|---|
| RS-274X copper | apertures (circle/rect/obround/polygon/macro), draws, flashes, regions (G36/G37), polarity, `%SR%`; X2 `%TA.AperFunction`, `%TO.P`, `%TO.N`, `%TO.C` |
| Aperture macros | circle, centre/vector line, outline, polygon; `$n` variables and `+ - x /` arithmetic; the solid area is the convex hull. Moire and thermal primitives are skipped |
| Excellon drill | tool table, metric/inch, zero suppression, `G85` slots, routed slots (`M15`/`G01`/`M16`, `G02`/`G03` arcs), Altium dialect (`;FILE_FORMAT`, `INCH,LZ`, modal single-axis lines, `;TYPE=PLATED`) |
| Gerber-format drill | flash centres as holes; oblong flashes as slots; draws as routs only when the film declares a rout role |
| Pick-and-place | KiCad `Ref,Val,Package,PosX,PosY,Rot,Side`, JLCPCB `Designator,Mid X,...`, Altium `Center-X(mm)...`; mm/mil/inch |
| Allegro `smt_loc.txt` | `!`-delimited, mils, `mirror` flag |
| BOM CSV | `Designator/Comment/MPN` or `Reference(s)/Value`; tolerant reader |

Dialect fixes applied before parsing: multi-statement `%...%` blocks are
split; an `FS` with no zero-omission character gets `L`.

## Layer roles

Consulted in this order (higher wins): a mapping file; the `*.gbrjob` manifest
(`FilesAttributes` roles, `Copper,L<n>` stack positions); an Altium `*.EXTREP`
(unique extension rows only); an Altium `*.LDP` for drill spans; then
filename conventions (KiCad `*-F_Cu.gbr`/`*-In1_Cu.gbr`, Protel
`.GTL`/`.GBL`/`.G1L`/`.GTP`/`.GTO`/`.GTS`/`.GKO`/`.TXT`/`.DRL`, Allegro
`top`/`bottom`/`gnd02`/`pwr04`, generic `top`+`copper`, `inner`, `signal`).

Mapping file (`layer_map.txt` or `*.map`), one line per file:
`filename = copper:<index> | copper:bottom | drill | outline | ignore`.

An inner film named only by its user label (`-GND_Cu.gbr`) needs the
`.gbrjob` or a mapping file to be classified; the shortfall against the
drill's declared layer count is reported as a note.

## Connectivity rules

- **Pours** arrive as one keyholed outline. A primitive joins a pour when a
  sample point is inside the filled outline or a pad's copper penetrates the
  boundary. Exact for KiCad/Allegro single-region pours.
- **Negative pours (`%LPC*%`)** (Altium's default) are cut, not skipped: each
  uninterrupted clear pass is unioned and subtracted from earlier copper, and
  every remaining polygon is its own conductor. Only exactly reproduced
  geometry may erase copper; a macro hull, a widthless draw, an aperture
  block, `%LS`/`%LR`/`%LM` transforms, `%IPNEG*%`, or an arc-bearing clear is
  refused and leaves copper standing (over-connects, never fabricates an
  open). A region is clear only when polarity is clear at both `G36` and `G37`.
- **Slots** are plated stadiums along the tool path; **castellations** join
  whatever copper the barrel touches (`ReconStats::n_castellations`, needs an
  outline film). An unplated slot connects nothing.
- **Blind/buried vias** stitch only the layers their span names: the drill's
  X2 `TF.FileFunction` pair, the `.LDP` row, or an `L`-marked / KiCad
  layer-named file name. A pair that cannot be placed in the stack, a silent
  drill beside a partial-span sibling, a name naming a missing layer, or
  "blind" with no layers refuses to stitch and reports
  (`ReconStats::refused_span_holes`, `notes`). Under-reporting connectivity is
  preferred to a phantom short.
- **Plating** comes from `TF.FileFunction`, `%TA.AperFunction` drill functions
  (`MechanicalDrill` vs `ViaDrill`/`ComponentDrill`/`CastellatedDrill`), the
  file name, or the job's plated/non-plated split; a film with none is dropped
  and named (`ReconStats::refused_plating_files`).
- **Component binding**: each flash goes to the nearest placed component
  whose footprint window contains it. On X2 films `%TO.P`/`%TO.N` bind
  pad-to-refdes-to-net exactly and `ViaPad` flashes classify as vias. On
  stripped films the window is inferred from the package name (chip codes, IC
  families, pin-header grids), with a 4.0 mm fallback.

## Per-net copper geometry

`ReconStats::net_copper` gives each net's narrowest and widest track width,
track/region counts and a `GerberCopperKind` (`Traces` / `Poured` / `None`),
so the IPC-2221 ampacity check runs on gerber-only boards (a `Poured` net is
never rated). Probe: `cargo run -p hauksbee-extract --example gerber_trace_current -- <dir>`;
report: `cargo run --example gerber_report -- <dir>`.

## Validation

Closed loop: export a KiCad board's gerbers with
`kicad-cli pcb export gerbers --no-x2 --no-netlist`, reconstruct, and compare
the net partition over located component pads against the native extraction.
Agreement runs 99.7-100 % over located pads; the located fraction (74-100 %)
is gated separately. `shipped_boards_survive_gerbers` round-trips every
bundled board wherever `kicad-cli` is installed; the bundled Watchy carries a
99 % partition / 90 % located floor. X2 binding is proven at 100 % name and
partition agreement against a same-batch netlist (`tests/gerber_x2.rs`).

## Coverage notes and limits

`ReconStats::coverage_notes()` reports unmatched flashes ("N of M aperture
flashes matched to a placed component") on every surface.

- Partition agreement is scored over located pads only.
- Footprint inference on stripped films is approximate: a stitching via
  inside the window can be counted as a pad, and an under-sized window misses
  pads. Re-export with X2 attributes or supply the native layout.
- Concave macros are over-approximated by their convex hull.
- A split pour drawn as one dark fill reads as connected.
- A bare number pair in a drill file name (`drill-1-6.art`) is not a layer pair.
- Edge contacts / gold fingers get no special treatment.
- The `GND` label is a heuristic.
- The `.zip` reader flattens by basename; extract nested archives manually.
- No pick-and-place: nets and DRC reconstruct, components cannot bind. No
  drill: multi-layer boards fragment per layer. No BOM: values come from the
  P&P `Val`/`Package` only.

Tests: `crates/hauksbee-extract/tests/gerber_closedloop.rs`,
`gerber_advanced_geometry.rs`, `gerber_negative_pour.rs`, `gerber_x2.rs`,
`gerber_gbrjob.rs`, `gerber_native_partition.rs`.
