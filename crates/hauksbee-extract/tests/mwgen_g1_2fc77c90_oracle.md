# MWGEN-G1 pad-overlap oracle

Recorded on 2026-08-09 against
[`MR-DOS/MWGEN-G1`](https://github.com/MR-DOS/MWGEN-G1) commit
`2fc77c9068534ed38f275337907b41942ff4621d`, which is the revision `corpus.toml`
pins as `mwgen_g1`.

Input:

- `MWGEN-G1.kicad_pcb`
- SHA-256: `acc1e240f99f59e68d698c19bd01eb6c6f736bcfea63a6b6dfdc7fb884a96299`

KiCad command:

```text
/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli pcb drc \
  --format json \
  --output mwgen_drc.json \
  --severity-error \
  MWGEN-G1.kicad_pcb
```

KiCad 9.0.3 reported 1144 error-severity violations and 499 unconnected items.
The per-type tally is in `violation_counts_by_type` in the committed JSON, and
`shorting_items` is 6 of them. The bulk (503 `clearance`, 199 `silk_overlap`, 199
`hole_to_hole`, 113 `silk_over_copper`) is the same overcrowding read from other
directions and is not what this oracle is about.

## What is committed, and what is trimmed

`mwgen_g1_2fc77c90_kicad_9_0_3_shorts.json` carries the report's metadata
(`kicad_version`, `coordinate_units`, `date`, `source`), the full per-type
violation tally, the unconnected-item count, and the six `shorting_items`
violations **verbatim**, item descriptions, pad UUIDs and positions included.

The other 1138 violations are trimmed, which is a deviation from the
`openmower_62dd369` oracle beside this one (that report is committed unedited).
The reason is size: the unedited run is 1.3 MB of mostly silkscreen and
hole-spacing noise, and the trim is mechanical (`type == "shorting_items"`) with
the discarded content fully accounted for by the committed tally, so re-running
the command above reproduces it.

## What the test does with it

`known_faults.rs::mwgen_g1_pad_overlap_shorts_match_kicads_own_drc` parses this
file, derives KiCad's expected set of (net pair, layer, footprint pair) from the
`items[].description` strings, and requires hauksbee's shorts to be exactly that
set. So the cross-check is pinned in CI without CI needing KiCad installed, and a
change to either tool's reading of this board fails against a recorded oracle
rather than against a hand-typed list.

The gap magnitudes hauksbee measures (-0.150, -0.060 twice, -0.015 twice, and
-1e-6 mm) are asserted in the same test. KiCad's JSON does not report gaps for
`shorting_items`, so those numbers are hauksbee's own and are pinned as a
regression guard, not as agreement with KiCad.

## The design's own rules

`MWGEN-G1.kicad_pro` at the same revision sets
`board.design_settings.rule_severities.shorting_items` and `.clearance` to
`error`, and `board.design_settings.drc_exclusions` is present and empty. Those
severities are KiCad's defaults rather than a deliberate tightening; the point is
that the design neither loosened the rule that forbids these overlaps nor
excepted any instance of it. The test asserts both, so the claim cannot rot.
