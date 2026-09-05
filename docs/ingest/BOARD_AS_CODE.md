# Board-as-Code

A `.board` file is an editable text form of a board's components, pads and
net assignments. Decompile a layout to it, edit it (a value, a pad's net, a
part), recompile it to a `.kicad_pcb`, or run the edit straight through
simulation. `.board` is also accepted wherever a board is (`hauksbee run`,
`hauksbee-ci` `board`, MCP `board_to_code`).

## Commands

```
hauksbee to-code <board> [--out FILE]
hauksbee from-code <code> [--out FILE] [--relayout | --incremental]
    [--route | --route-grid | --route-dsn FILE] [--route-strict]
    [--route-timeout SECS] [--route-passes N] [--freerouting-jar JAR] [--json]
hauksbee merge-ses <code> <ses> [--out FILE] [--route-strict] [--json]
hauksbee check-code <code> [--seconds N] [--destructive] [--ambient C] [--json]
```

| Command / flag | Effect |
|---|---|
| `to-code <board>` | decompile a text board (`.kicad_pcb`, `.kicad_sch`, `.net`, Eagle `.brd`, `.d356`) to stdout or `--out`. Routed copper is not carried; a note says so. A board with a DNP part, or a part whose identity extraction refused, is refused by name: the DSL has no field for either state |
| `from-code <code>` | recompile to a `.kicad_pcb` (stdout or `--out`). Bare, it emits each part at the `at X Y rot` the code carries |
| `--relayout` | re-place every part: group by function, tile the outline, force-directed relaxation, hard courtyard de-overlap |
| `--incremental` | keep parts whose placement matches the base (the same file), re-place the rest; only duplicate reference designators move |
| `--route` | route through freerouting (`--freerouting-jar`, `$FREEROUTING_JAR`, `tools/` up the tree, `~/.local/share/freerouting`); falls back to the grid A* if absent |
| `--route-grid` | force the in-tree single-layer grid A* (Manhattan paths, no vias, reports nets it cannot complete) |
| `--route-dsn FILE` | write a Specctra DSN and stop; the unrouted board is still emitted |
| `--route-strict` | exit non-zero on an open connection, a serious DRC finding, or a wrong-net endpoint |
| `--route-timeout SECS` (180) / `--route-passes N` (10) | freerouting wall-clock budget (then the grid fallback) and its `-mp` passes |
| `--json` | one object on stdout: `{nets_total, connections_routed, unrouted, segments, vias, seconds, engine, drc_serious, endpoint_net_violations}` (with `--route-dsn`: `{ok, dsn}`); requires `--out` and a routing flag |
| `merge-ses <code> <ses>` | recompile the board bare, merge the routed SES (coordinate scale auto-detected), run the same post-route audit (`engine: "merged-ses"`). A SES that merges zero copper is refused |
| `check-code <code>` | recompile, bind, run the stress monitor for `--seconds` (default 0.2) and print a fault report; exit 1 when a part is destroyed. `--destructive` lets parts be destroyed; `--ambient` sets the thermal ambient; `--json` gives `{ok, board, components, nets, resolved_fraction, unresolved, simulated_seconds, active_nets, faults}` |

```
hauksbee to-code crates/hauksbee-ci/examples/boards/watchy.kicad_pcb --out w.board
hauksbee from-code w.board --out w_rebuilt.kicad_pcb
hauksbee check-code w.board --seconds 0.05
# Board-as-Code check: w
#   82 components, 84 nets, 88% resolved, 0 active nets (...)
#   8 unresolved (simulated as OPEN; add models with --models-dir ...): C7 (TBD) ...
#   no faults: circuit is within ratings.

hauksbee from-code b.board --route-dsn b.dsn --out b_placed.kicad_pcb   # route on your own clock
java -jar freerouting.jar -de b.dsn -do b.ses -mp 10
hauksbee merge-ses b.board b.ses --out b_routed.kicad_pcb --route-strict
# routed: 9/9 connections, 0 unrouted (merged-ses); DRC: 0 serious, 0 total
```

`connections` counts closed rat-lines (a four-pad net is three). `merge-ses`
recompiles bare, so export the DSN from the placement you merge onto. Use the
freerouting 1.9.0 jar; 2.x stalls without writing a SES on a partially routed
board.

## The DSL

Line-oriented; `#` starts a comment; one statement per line.

```text
# Board-as-Code (hauksbee board DSL v1)
board version 20241229
board size 60 45

fn block_r_led {
    slot 0 lib "Resistor_SMD:R_0805_2012Metric" val "1k" pads 2
    slot 1 lib "LED_SMD:LED_0805_2012Metric" val "LED" pads 2
}

fn main {
    net "+5V"
    net "GND"
    net "LED1_A"
    pin J1 edge left
    lock U5
    space fn block_r_led 8

    instance block_r_led {
        comp R1 lib "Resistor_SMD:R_0805_2012Metric" val "1k" layer "F.Cu" at 172.82 66.04 rot 0 {
            pad "1" smd roundrect at -0.9375 0 size 0.975 1.4 layers [F.Cu F.Paste F.Mask] net "+5V"
            pad "2" smd roundrect at 0.9375 0 size 0.975 1.4 layers [F.Cu F.Paste F.Mask] net "LED1_A"
        }
        comp D1 lib "LED_SMD:LED_0805_2012Metric" val "LED" at 176.82 66.04 {
            pad "2" smd roundrect at -0.9375 0 size 0.975 1.4 layers [F.Cu F.Paste F.Mask] net "LED1_A"
            pad "1" smd roundrect at 0.9375 0 size 0.975 1.4 layers [F.Cu F.Paste F.Mask] net "GND"
        }
    }
}
```

| Statement | Form | Notes |
|---|---|---|
| header | `board version <N>` | optional integer tag; default `20241229` |
| outline | `board size <W> <H>` or `board outline <X0> <Y0> <X1> <Y1>` | optional; bounds the placer. Otherwise read from the source board's `Edge.Cuts` |
| block | `fn <name> { slot <i> lib "<lib_id>" val "<value>" pads <n> ... }` | top level; the shared slot layout of a repeated cluster |
| main | `fn main { ... }` | the board; exactly one |
| net | `net "<name>"` | declares a net and fixes id order; a pad may name an undeclared net, which is auto-declared |
| instance | `instance <block> { comp ... }` | one concrete instance of a block |
| component | `comp <REF> lib "<lib_id>" val "<value>" [layer "<layer>"] at <X> <Y> [rot <deg>] { ... }` | `layer` defaults `F.Cu`, `rot` 0; body holds `pad` lines and an optional `space <mm>` |
| pad | `pad "<num>" <kind> <shape> at <x> <y> size <w> <h> [drill <D>] layers [<L> ...] (net "<name>" \| nonet)` | `x`/`y` relative to the comp origin; `kind` is `smd`, `thru_hole`, `np_thru_hole` or `connect` (`thru_hole`/`np_thru_hole` need `drill`); `shape` is `rect`, `roundrect`, `circle`, `oval` or `trapezoid` (KiCad `custom` cannot be carried); `nonet` must be bare |
| clearance | `space <mm>` (in a comp) or `space fn <block> <mm>` (in `fn main`) | hard minimum clear distance for the placer |
| edge | `pin <REF> edge <left\|right\|top\|bottom>` | holds a part against an edge; the along-edge coordinate relaxes |
| lock | `lock <REF>` | freezes a part at its coordinates as a keep-out |

`kind` and `shape` are positional; a token outside the closed set is a
line-numbered error naming the valid values (`line 6: pad kind: expected
smd|thru_hole|np_thru_hole|connect, got \`banana\``).

## Round-trip limits

- The bar is **connectivity equivalence**: `board -> code -> board` preserves
  the component set and every net's wiring (up to renaming), and placement on
  a bare recompile. Routed copper, zones, silk and non-pad graphics are not
  carried.
- `to-code` refuses boards with DNP parts or refused identities.
- `--incremental`'s base is the input file itself; true old-vs-new placement
  is library-only (`forge_codegen::relayout(&mut prog, &base, &cfg)`).
- The grid A* router is single-layer, no vias, no rip-up.

Code: `vendor/kicad-forge/crates/forge-codegen/src/{dsl/,layout.rs,route_freerouting.rs}`,
`crates/hauksbee-engine/src/boardcode.rs`; editor: `editors/vscode-hauksbee-board/`.
