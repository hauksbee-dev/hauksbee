# Copper short / clearance detection, and simulating shorts

```
hauksbee run <board> --drc [--plain] [--json] [--strict] [--oracle]
hauksbee run <board> --drc --apply-shorts --firmware fw.hex --headless   # bridge every short, then simulate
```

Detection lives in `crates/hauksbee-extract/src/drc.rs`; simulation of a
detected short in `crates/hauksbee-bind/src/shorts.rs`. Output shape:
[JSON_OUTPUT.md](../analysis/JSON_OUTPUT.md#finding).

## Detection

Every copper primitive is reduced to a solid shape in board millimetres:

| KiCad primitive | Modelled as |
|---|---|
| track `segment` / `arc` | capsule / chain of 8 capsules |
| `via`, through-hole pad | disc (or pad shape) on every copper layer spanned |
| circle / oval pad | disc / capsule |
| rect / roundrect / trapezoid pad | polygon (roundrect inset by its corner radius; `rect_delta` honoured) |
| custom pad | anchor shape plus every primitive |
| filled zone | `filled_polygon` boundary edges plus containment |

Pad rotation is the pad's absolute board-frame angle. Net references are read
in every KiCad encoding (`(net N "name")`, name-only KiCad 10, zone
`net_name`). Net 0 and `unconnected-(...)` nets never short.

Primitives are indexed per layer in an R*-tree and each queries its
neighbours within the clearance window. For a pair on different nets:

- gap <= 0: a **short** (`serious`);
- `0 < gap < clearance - 0.005 mm`: a **clearance violation** (grouped by
  net pair, layer and root cause; never gates);
- within 5 um of the rule: routing-to-rule, not reported.

Zone edges are indexed individually; the polygon is kept for a
point-in-polygon containment pass (a foreign via or track fully inside a pour
is a short). Outline-only zones (no stored `filled_polygon`) are checked at
their boundary only. Zone-vs-pad *overlaps* are dropped as a class (they are
nearly always the antipad carve); the count is disclosed as `NOT CHECKED:` in
text, a heads-up in `--plain`, and `suppression_note` in JSON.

Clearance: the board's `(setup (rules (min_clearance N)))` or `(setup
(min_clearance|clearance N))`, per-netclass and diff-pair rules from the
sibling `.kicad_pro`, else 0.2 mm.

### Eagle `.brd`

Copper geometry per net from the XML (wires, vias, pads, SMDs, rectangles,
circles, curved wires, mirrored elements with side swap) feeds the same sweep.
The clearance is the tightest of the board's `md*` design rules (`mdWireWire`,
`mdPadPad`, `mdSmdSmd`, ...), else 0.2 mm; pad elongation comes from
`psElongationLong`/`psElongationOffset`. Through-hole pads and vias span a
two-layer (`1`/`16`) stack. A pour's fill is not stored in the file, so
pour-to-copper pairs are **not checked**; two overlapping same-rank pours of
different signals are reported as a short (over-reports on some boards).

### Altium `.PcbDoc`

`Tracks6`, `Arcs6`, `Vias6`, `Pads6` feed the same sweep; pour fill
(`Regions6`) is not modelled and contributes no edges. The ASCII Protel
reader has no DRC path ([ALTIUM.md](../ingest/ALTIUM.md)).

## Deliberate ties

Footprint ownership never suppresses a finding: different-net pads of one
resistor, IC or connector are checked like any copper. Only these
format-specific declarations exempt a contact, and only at that group's own
copper:

- **KiCad**: `(net_tie_pad_groups "1, 2" "3, 4")`, or `(attr net_tie)` for
  one all-pad group. Two bounded legacy forms: a dedicated `0R_...` footprint
  with a zero-ohm value, and old Eagle imports whose footprint has a `TIED`
  token and whose value names a pair such as `Closed(1-2)`.
- **Eagle**: Arduino's `library="jumper"` / `package="SJ"` and SparkFun's
  `SparkFun-Jumpers` closed-trace packages; the dedicated-0R rule. A generic
  `JUMPER` package/value is checked.
- **Altium**: only the native `COMPONENTTYPE=Net Tie` / `Net Tie (In BOM)`.

### Declared ties read from an Eagle schematic

An Eagle star ground is drawn by placing one net's supply symbol on another
net; the `.brd` records no tie. Pass `--schematic <FILE>` or leave
`<name>.sch` beside `<name>.brd` (in `hauksbee serve`, the optional schematic
companion input). A supply symbol is recognised only when its library symbol
has exactly one pin with `direction="sup"`, one gate, and no packaged device.

The declaration is **context, not authorisation**: a matching short keeps its
nets, layer, location, gap, `serious` severity and `--strict` gate, and gains
the declaration text (`AGND7 wired to SUPPLY6 in net GND`). A schematic that
declares nothing adds nothing; a missing one leaves the finding unchanged. The
companion must share the board's basename and physical reference/value and
pad/net incidence, or it is refused. `--schematic` on a non-Eagle board is
refused (KiCad and Altium declare ties in the layout). The schematic enters
the input inventory with `ArtifactRole::Schematic`.

Diagnostic: `cargo run -p hauksbee-extract --example sch_ties -- <board>.brd <board>.sch`.

## Simulating a short

A detected or hypothetical short is applied by bridging the two nets with
`BRIDGE_OHMS = 5 mOhm`; the scheduler rebuilds the MNA layout and the stress
monitor sees the fallout. Each bridge is a `FaultEvent` of kind `short`.
API: `apply_drc_shorts(&report)`, `from_board_file_with_drc_shorts(...)`,
`short_nets(net_a, net_b)`. CLI: `--apply-shorts`.

## Limits

- Zone-vs-pad overlaps are suppressed as a class (disclosed, see above); audit
  them with `--oracle`.
- KiCad 10+ boards keep a `version_warning`; their shorts are demoted and do
  not gate.
- Boards with no stored zone fill are checked at the pour boundary only.
- Arcs are flattened to 8 links (sub-micron chord error); Eagle circles and
  pour curves use outward-biased covering flattening (<= 2.5 um), so an air
  gap under ~2.5 um can read as touching.
- Roundrect pads are an inset rectangle plus corner radius.
- Eagle: inner copper layers are not spanned; a single clearance is used
  rather than the per-kind `md*` matrix; same-rank pour rings that miss by
  less than their stroke width are not flagged.
- Altium octagonal pads use the 0.25 corner-cut ratio.
- The bridge is a fixed resistance and does not fuse.

Tests: `crates/hauksbee-extract/tests/drc.rs`, `eagle_drc.rs`, `altium.rs`;
`crates/hauksbee-engine/tests/shorts.rs`.
