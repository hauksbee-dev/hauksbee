# KiCad 10 keyhole-antipad oracle

Recorded 2026-08-07 with:

```text
/Applications/KiCad10/KiCad.app/Contents/MacOS/kicad-cli --version
10.0.5
```

## Real-board control

Board:
`board-corpus/famous/hunt/vendettafc/VESC/VENDETTAESC.kicad_pcb`
(format `20260206`, SHA-256
`d844e3dfaf4262e6975b6cc83d7d249cd07302a9c85c3c4f64f2f8cb5ef97815`).

Before and after restoring pad containment, Hauksbee reported:

```text
clearance_mm=0.2 primitives=142645 shorts=67 clearance_violations=14526
short item-kind histogram: {"Pad-Pad": 63, "Pad-Track": 4}
```

There are no Zone-Pad findings, so restoring the pad pass did not reopen the
former 1,668-keyhole false-positive epidemic.

The native command was:

```text
"/Applications/KiCad10/KiCad.app/Contents/MacOS/kicad-cli" pcb drc \
  --format json \
  --output "crates/hauksbee-extract/tests/vendettaesc_kicad_10_0_5_drc.json" \
  "/Users/hauksbee-user/Tarski/Tarski-Repos/board-corpus/famous/hunt/vendettafc/VESC/VENDETTAESC.kicad_pcb"
```

Its result was:

```text
Found 357 violations
Found 0 unconnected items
Saved DRC Report to crates/hauksbee-extract/tests/vendettaesc_kicad_10_0_5_drc.json
```

The JSON contains 60 `shorting_items`, all PTH-PTH pairs on U12, two
`clearance` findings, and no Zone-Pad finding. Its SHA-256 is
`ee36c89462aba5ef4eaca2a5b15f7fd8e731b013f0378d8e86db878f5ff901da`.
The remaining 67-versus-60 short-count difference is outside this bounded
Zone-Pad remediation, so format `20260206` retains its exact-parity warning.

## Focused positive and negative controls

`fixtures/kicad_10_keyhole_antipad.kicad_pcb` is a format-`20260206` fixture
containing:

- `ANTIPAD_OK`, isolated by a doubled-back keyhole void; and
- `PAD_ONLY_SHORT`, covered by a solid different-net filled polygon.

The fixture SHA-256 is
`348adfe3594357cf8e264d8c8fa8d3fe22f73efd1498c6bd6b2294336d674cbf`.
Before the implementation change, Hauksbee incorrectly returned:

```text
clearance_mm=0.2 primitives=18 shorts=0 clearance_violations=0
```

The native command was:

```text
"/Applications/KiCad10/KiCad.app/Contents/MacOS/kicad-cli" pcb drc \
  --format json \
  --output "crates/hauksbee-extract/tests/kicad_10_keyhole_antipad_10_0_5_drc.json" \
  "crates/hauksbee-extract/tests/fixtures/kicad_10_keyhole_antipad.kicad_pcb"
```

Its result was:

```text
Found 7 violations
Found 0 unconnected items
Saved DRC Report to crates/hauksbee-extract/tests/kicad_10_keyhole_antipad_10_0_5_drc.json
```

The one copper finding is:

```text
clearance: Clearance violation (zone clearance 0.5000 mm; actual 0.0000 mm)
  Pad 1 [PAD_ONLY_SHORT] of PAD_SHORT on F.Cu
  Zone [GND] on F.Cu, priority 0
```

There is no copper finding for `ANTIPAD_OK`. The JSON SHA-256 is
`af5fb2d3b7a9e8f2dea6899866a74c0aabd91d80268bdf7689f65876852d992b`.

After the implementation change, Hauksbee returns:

```text
clearance_mm=0.2 primitives=18 shorts=1 clearance_violations=0
short item-kind histogram: {"Zone-Pad": 1}
Zone-Pad GND<->PAD_ONLY_SHORT F.Cu gap=-0.2000 @(15.0,5.0) owners[|PAD_SHORT]
```
