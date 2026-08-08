# Board-as-Code

A PCB is a program that draws itself. Hauksbee makes that program *executable*
and *editable*: you can decompile a real `.kicad_pcb` into readable code, edit
the code (change a part value, fix a wiring swap, add a component), recompile
it back into a coherent board, and run the edit straight through simulation to
see whether your fix actually worked.

This closes a loop that did not exist before. `kicad-forge` already decompiled
a board into a readable, repeat-detected program, but that text dropped net
assignments and was never re-executable; the rebuild step read an in-memory
analysis struct, not source. The Board-as-Code DSL is the missing executable
layer: it carries full pad-level connectivity and round-trips.

## The loop

![The Board-as-Code loop: a .kicad_pcb decompiles to editable board.board text, edits to values, wiring, parts and layout hints recompile through from-code into a connectivity-equal board, and check-code runs the same code through extract, bind, co-simulation, the stress monitor and a report](../assets/diagrams/board-as-code-roundtrip.svg)

Four CLI verbs, alongside the existing `hauksbee run`:

| command | what it does |
|---|---|
| `hauksbee to-code <board>` | decompile a board into Board-as-Code text |
| `hauksbee from-code <code>` | recompile code back into a `.kicad_pcb`. Bare, it emits the coordinates the code carries; `--relayout` / `--incremental` re-place, `--route` / `--route-grid` route |
| `hauksbee merge-ses <code> <ses>` | merge an externally routed Specctra SES back onto the board the code recompiles to |
| `hauksbee check-code <code>` | recompile, bind, simulate with the stress monitor, print a fault report |

`from-code --route` runs the router inside the command, on hauksbee's clock.
`--route-dsn <file>` is the other half: it writes the Specctra DSN and stops,
so you can route it with anything Specctra-capable for as long as you like,
then bring the result back with `merge-ses`. The SES then becomes a cacheable,
diffable artifact: keep it and re-merge at will, instead of re-routing on every
build.

## The DSL

A line-oriented, AI- and human-editable format. Repeated hardware blocks the
decompiler found (a synapse instanced 90 times, a neuron a dozen) become `fn`
blocks. `main` declares the nets and instantiates the blocks, with every
component carrying its concrete pads and per-pad net assignments:

```text
# Board-as-Code (hauksbee board DSL v1)
board version 20241229

# block block_2c_r_0805_2012metric_led_0805_2012metric: 2 slot(s), 3 instance(s)
fn block_2c_r_0805_2012metric_led_0805_2012metric {
    slot 0 lib "Resistor_SMD:R_0805_2012Metric" val "1k" pads 2
    slot 1 lib "LED_SMD:LED_0805_2012Metric" val "LED" pads 2
}

fn main {
    net "+5V"
    net "GND"
    net "LED1_A"

    instance block_2c_r_0805_2012metric_led_0805_2012metric {
        comp R1 lib "Resistor_SMD:R_0805_2012Metric" val "1k" layer "F.Cu" at 172.82 66.04 rot 0 {
            pad "1" smd roundrect at -0.9375 0 size 0.975 1.4 layers [F.Cu F.Paste F.Mask] net "+5V"
            pad "2" smd roundrect at 0.9375 0 size 0.975 1.4 layers [F.Cu F.Paste F.Mask] net "LED1_A"
        }
        comp D1 lib "LED_SMD:LED_0805_2012Metric" val "LED" layer "F.Cu" at 176.82 66.04 rot 0 {
            pad "2" smd roundrect at -0.9375 0 size 0.975 1.4 layers [F.Cu F.Paste F.Mask] net "LED1_A"
            pad "1" smd roundrect at 0.9375 0 size 0.975 1.4 layers [F.Cu F.Paste F.Mask] net "GND"
        }
    }
    # ... two more instances of the same block, on LED2_A and LED3_A
}
```

That excerpt is a loadable file on its own, elision comment included:
`hauksbee check-code` on it reports `2 components, 3 nets, 100% resolved`.

The bar for the recompile is **connectivity equivalence**, not
byte-exactness: `board -> code -> board` preserves the component set and
every net's wiring (up to net renaming). On a clean round-trip it also
preserves placement.

That claim applies only when the DSL can preserve the input's assembly and
identity evidence. The current component statement has no DNP or ambiguous-
identity field, so `to-code` refuses a board containing a DNP component or a
component whose physical identity was refused during extraction. It names the
component and the lost evidence. This is deliberate: emitting code would make a
DNP link fitted, or turn an unknown identity into a precise simulated part, on
the next `from-code`/`check-code` pass. Resolve the source identity or assembly
variant before converting; Hauksbee never launders either state through the
editable format.

Two things back that claim. In-repo, `crates/hauksbee-engine/tests/boardcode_run.rs`
round-trips a board through `to-code` then `from-code` and asserts the
recompiled text binds the *same* IR device set as the original
(`board_as_code_run_matches_kicad_pcb`), and does the same for a netlist with no
layout at all (`netlist_to_board_preserves_components_and_nets`). Second, you can
run the round-trip yourself on the bundled Watchy board and check the net
partition:

Each `to-code` reminds you on stderr that it is carrying components and nets
only, and that the routed copper of the input is not in the output. That is the
point of the exercise: the *connectivity* has to survive a trip through a
representation that never held the geometry.

```bash
hauksbee to-code crates/hauksbee-ci/examples/boards/watchy.kicad_pcb --out w.board
hauksbee from-code w.board --out w_rebuilt.kicad_pcb
hauksbee to-code w_rebuilt.kicad_pcb --out w2.board
# w.board and w2.board carry the same 267 `(ref, pad)` keys and partition them
# into the same 84 nets.
```

(267, not 276 pad entries: a few switches and test points expose two physical
pads under one pad number, and those collapse to one key. One key carries no net
at all, an unconnected pad, leaving 266 that the partition actually covers.)

`forge-codegen` also exposes `compare_connectivity` (in `rebuild.rs`) for
programmatic use, though nothing in the test suite calls it today: the shipped
gate is the bind-equivalence pair above.

## Authoring a board from scratch

You do not have to start from a decompiled board. The DSL is small enough to
hand-write. [`examples/board-as-code/starter.board`](../../examples/board-as-code/starter.board)
is a complete, hand-authored three-part board, a 2-pin header driving an LED
through a series resistor, written from nothing and richly commented. Build
one up the same way:

1. **Header + `fn main`.** A file is one `fn main { ... }` block that holds the
   whole board, usually preceded by `board version <N>` (N is any integer tag).
   The header is optional: omit it and the version defaults to `20241229`.
2. **Declare the nets.** List every net with `net "<name>"` before you wire a
   pad to it. This is documentation, not a contract: the recompiler
   auto-declares any net it first meets on a pad, so a pad wired to a net you
   forgot to list still lands on that net rather than erroring. Declaring nets
   up front fixes their id order and gives a reader the board's net list in one
   place, which is why the decompiler always emits them.
3. **Add components.** Each `comp` has a reference, a footprint
   `lib "<lib_id>"`, a `val`, a `layer`, and an `at X Y rot` placement, then
   a `{ ... }` body of pads. A footprint `lib_id` is a KiCad `Library:Footprint`
   name (e.g. `Resistor_SMD:R_0805_2012Metric`), the same strings KiCad's
   footprint chooser shows. The recompiler passes them straight to the
   `.kicad_pcb`.
4. **Wire the pads.** Each `pad` carries its number, kind, shape, position
   (relative to the comp origin), size, copper `layers`, and the
   `net "<name>"` it connects to (or `nonet` for an unconnected pad). A
   `thru_hole` / `np_thru_hole` pad also needs a `drill <D>`.
5. **Check it.** `hauksbee check-code starter.board` recompiles, binds, and
   simulates it, the fastest way to see "100% resolved" and a clean report
   (or find out which pad you mis-wired).

```bash
hauksbee check-code examples/board-as-code/starter.board --seconds 0.01
# Board-as-Code check: starter
#   3 components, 3 nets, 100% resolved, 0 active nets (nothing toggles without
#   firmware; `hauksbee run <board> --firmware <f> --headless` exercises it)
#   simulated 0.010s
#   no faults: circuit is within ratings.
```

## Statement & field reference

One statement per line, inside `fn main` (or an `fn <block>` you instantiate).
`[...]` marks an optional field.

| statement | form | notes |
|---|---|---|
| board header | `board version <N>` | optional; `N` is an integer tag, default `20241229` |
| board outline | `board size <W> <H>` or `board outline <X0> <Y0> <X1> <Y1>` | optional; constrains re-layout |
| net | `net "<name>"` | declares the net and fixes its id order; a pad may also name a net that was never declared, and the recompiler auto-declares it |
| block | `fn <name> { slot <i> lib "<lib_id>" val "<value>" pads <n> ... }` | the shared slot layout of a repeated cluster, at file top level |
| instance | `instance <block> { comp ... }` | one concrete instance of a block, inside `fn main`; the decompiler emits one per detected cluster instance (55 of them in the bundled Watchy board) |
| component | `comp <REF> lib "<lib_id>" val "<value>" [layer "<layer>"] at <X> <Y> [rot <deg>] {` | `layer` defaults `F.Cu`, `rot` defaults `0`; body ends with `}`. Legal at the top of `fn main` or inside an `instance` block |
| pad | `pad "<num>" <kind> <shape> at <x> <y> size <w> <h> [drill <D>] layers [<L> ...] (net "<name>" \| nonet)` | `x`/`y` are relative to the comp origin |
| clearance | `space <mm>` (in a comp body) or `space fn <block> <mm>` (in `fn main`) | hard minimum clear distance for the placer |
| edge constraint | `pin <REF> edge <left\|right\|top\|bottom>` | holds a part against a board edge |
| lock | `lock <REF>` | freezes a part at its coordinates as a keep-out |

The token sets the parser enforces (closed sets, validated at parse time):

- **pad `kind`:** exactly `smd`, `thru_hole`, `np_thru_hole`, `connect` (a
  `thru_hole` / `np_thru_hole` pad needs a `drill`).
- **pad `shape`:** exactly `rect`, `roundrect`, `circle`, `oval`, `trapezoid`
  (the KiCad pad shapes minus `custom`, which the DSL cannot carry).
- **`layers`:** KiCad layer names in `[ ]`, e.g. `[F.Cu F.Paste F.Mask]` for
  a top SMD pad, `[F.Cu B.Cu]` for a through-hole pad.

`kind` and `shape` are positional, not optional, and any token outside the
closed set is a line-numbered parse error naming the offender and the valid
values (``line 6: pad kind: expected smd|thru_hole|np_thru_hole|connect, got
`banana` ``). Omitting `shape` fails the same way: the token in the shape slot
would be `at`, which is not a valid shape.

## Worked example: decompile and recompile

Against the Watchy board that ships in this repository, so this runs on a bare
clone:

```bash
BOARD=crates/hauksbee-ci/examples/boards/watchy.kicad_pcb

# 1. decompile to code
hauksbee to-code "$BOARD" --out w.board

# 2. recompile back to a board (connectivity-equal to the original)
hauksbee from-code w.board --out w_rebuilt.kicad_pcb

# 3. run the recompiled board through the bind + co-sim + stress loop
hauksbee check-code w.board --seconds 0.05
```

`check-code` prints:

```text
Board-as-Code check: w
  82 components, 84 nets, 88% resolved, 0 active nets (nothing toggles without firmware; `hauksbee run <board> --firmware <f> --headless` exercises it)
  8 unresolved (simulated as OPEN; add models with --models-dir, see hauksbee models --help):
    - C7 (TBD)
    - C9 (TBD)
    - R12 (TBD)
    - Y1 (32.768KHz)
    - Y2 (40MHz)
    - L1 (TBD)
    - M1 (Vibration_Motor)
    - AE1 (Antenna_Chip)
  simulated 0.050s
  no faults: circuit is within ratings.
```

`examples/board-as-code/blinky.board` is the same loop with no corpus at all:
`hauksbee check-code examples/board-as-code/blinky.board --seconds 0.01`
reports `5 components, 5 nets, 100% resolved` and a clean report.

## Worked example: a fix, expressed as a code edit

The headline case is the Tarski inhibitory-synapse miswire. The inhibitory
cells cross the dual-NPN's base and collector connections: pin 5 (B2) is
wired to the weight-switch common instead of pin 3 (C2). Enabling the weight
then slams the base toward the rail through the switch's 6 ohm
on-resistance.

Expressed as a Board-as-Code edit, the repair is swapping the net names on
IC3906's pad 5 and pad 3:

```rust
let mut prog = Program::parse(&code)?;
let ic = prog.comp_mut("IC3906").unwrap();
let b2 = ic.pads.iter().position(|p| p.number == "5").unwrap();
let c2 = ic.pads.iter().position(|p| p.number == "3").unwrap();
let tmp = ic.pads[b2].net.clone();
ic.pads[b2].net = ic.pads[c2].net.clone();
ic.pads[c2].net = tmp;
let repaired = prog.emit();
```

Recompiling and re-simulating both versions shows the edit changed the
physics:

| version | base/sink current | stress faults |
|---|---|---|
| as-wired (code unchanged) | ~689 mA | overcurrent + overpower on IC3906, pin overcurrent on the switch |
| repaired (code edit) | ~0.42 µA | none |

This is the integration test `hauksbee-engine` `boardcode_miswire`
(`code_edit_repairs_the_miswire`). The fault count strictly drops as a direct
consequence of the one-line wiring edit.

## Logical re-layout

`from-code --relayout` recompiles the code to a board arranged by function:
components group by the cluster/function they belong to, the groups tile the
board outline so each function occupies its own region, then a
force-directed relaxation pulls net-connected parts together while a hard
de-overlap pass guarantees no two courtyards (plus their clearances)
intersect. The relaxation excludes global power/ground rails from the
attraction, so they do not collapse the whole board into one blob.

The placer respects four real constraints, so the output is something a
human would accept rather than a scramble:

* **Board outline.** The outline is read from the source board's
  `Edge.Cuts` geometry on decompile (and re-emitted as `Edge.Cuts` lines on
  recompile), or set in the DSL with `board size W H` /
  `board outline X0 Y0 X1 Y1`. Every component, courtyard included, stays
  inside it; nothing is placed off-board.
* **Courtyards, not points.** Overlap is computed from each footprint's
  rotation-aware pad bounding box, so large parts (a DIP, a connector)
  genuinely reserve their area instead of being treated as a point.
* **Hard clearances.** `space` / `space fn` are enforced as minimum clear
  distances by the de-overlap pass, not soft hints: a test point with
  `space 3` actually gets 3 mm of clear room around it.
* **User position constraints.** `pin <ref> edge <left|right|top|bottom>`
  holds a component against a board edge (the edge-normal coordinate is
  fixed, the along-edge coordinate relaxes), and `lock <ref>` freezes a
  component at its exact coordinates and makes it a fixed keep-out for
  everything else.

### Constraints in the DSL

```text
board size 60 45                 # constrain the board to 60 x 45 mm

fn main {
    pin J1 edge left             # hold connector J1 against the left edge
    pin J2 edge right
    lock U5                      # never move the MCU; keep its placement
    space fn block_synapse 8     # every synapse instance gets 8 mm clear
    # ... the nets and comps
}
```

### Distance fields

Components and whole functions can carry a `space` clearance field that the
placer enforces as a hard minimum clear distance:

```text
comp TP1 lib "TestPoint:TestPoint_Pad_D1" val "TP" layer "F.Cu" at 0 0 rot 0 {
    space 5                       # keep 5 mm clear around this test point
    pad "1" smd circle at 0 0 size 1 1 layers [F.Cu] net "PROBE"
}

fn main {
    space fn block_synapse 8      # every synapse instance gets 8 mm breathing room
    # ... the rest of the board
}
```

### Before / after

Full re-layout of a 51-part Arduino-style board, exported with
`kicad-cli pcb export svg` (`assets/storm_before.svg`,
`assets/storm_after.svg`). These three figures were rendered from a board that
is not redistributable, so they are illustrations rather than something a clone
can regenerate; the runnable equivalents are the transcripts above.

| before | after |
|---|---|
| ![before](../assets/storm_before.png) | ![after](../assets/storm_after.png) |

Left is the original placement; right is the function-grouped re-layout,
with every part inside the board outline, courtyards non-overlapping, and
clearances respected. Connectivity stays identical (the recompiled board
passes the same connectivity check); only the placement changed.

Regenerate both, the routed board, and the incremental diff with one
command:

```bash
scripts/board_as_code_assets.sh
```

## Keeping the placement: bare `from-code`, and `--incremental`

For a fix workflow you almost always want no re-placement at all, and that is
what **bare `from-code` already does**: it emits each component at exactly the
`at X Y rot` the code carries. Edit one resistor's value or one pad's net, and
the recompiled board differs from the original in that one thing. No flag
needed.

`--incremental` adds one narrow behaviour on top: it compares each component
against a base index keyed by reference, keeps every component whose placement
and rotation match the base, and re-places the rest into free space near their
net neighbours. At the CLI the base is the same file you passed in, so the only
components that can miss the index are ones whose **reference designator
collides with another component's**. Everything else is byte-identical to the
bare run.

```bash
hauksbee from-code w.board --incremental --out w_patched.kicad_pcb
# re-layout: 0 groups, 2 moved, 84 kept
```

The `2 moved` on the unedited Watchy board above is exactly that: Watchy ships
two components called `TP4` and two called `TP5`, so the first of each pair
loses its slot in the reference-keyed index and gets re-placed. A board with
unique references reports `0 moved` and produces the same bytes as bare
`from-code`. So reach for `--incremental` when you are patching a board with
duplicate designators and want them separated; otherwise leave it off.

The re-placement engine itself takes a distinct base program
(`forge_codegen::relayout(&mut prog, &base, &cfg)` in `layout.rs`), so a caller
that holds the pre-edit program can get true old-versus-new incremental
placement. The CLI does not expose that second input today.

### Before / after / diff

The visualisation below shows one textual edit (move a single resistor)
recompiled: the original board, the recompiled board, and a diff that
highlights the moved part in orange (with a dashed ghost at its old
position and an arrow to the new one), new parts in green, and the
untouched parts in grey.

![incremental recompile before/after/diff](../assets/incremental_recompile.png)

The middle and diff panels are identical to the original except for that
one part. Regenerate with `scripts/board_as_code_assets.sh`
(renderer: `scripts/make_incremental_viz.py`).

## Routing

Routing is a hard, well-solved problem, so the production path hands the
placed board to **freerouting** (the standard open-source autorouter, Java)
over the Specctra DSN/SES interchange format rather than growing a bespoke
router.

```bash
# route with freerouting (the default when it is installed)
hauksbee from-code examples/board-as-code/blinky.board --relayout --route \
    --out blinky_routed.kicad_pcb
# re-layout: 3 groups, 5 moved, 0 kept
# routing: freerouting handoff (DSN -> freerouting -> SES); DSN at freerouting-work/board.dsn
# merged 27 segments, 0 vias in 35.4s (freerouting-1.9.0)
# routed: 9/9 connections, 0 unrouted (freerouting-1.9.0); endpoint-net violations: 0
# DRC: 0 serious, 0 total
```

Read the last two lines together. `connections` counts real routed connections
(rat-lines closed), not nets: a four-pad net is three connections, so
`9/9 connections` on a five-part board is a fully routed board. The DRC line is
the same internal short/clearance sweep `hauksbee run --drc` runs, over the
board that just came out of the router, and `--route-strict` turns any open
connection, serious DRC finding, or wrong-net endpoint into a non-zero exit.

If you would rather route on your own clock, split the run in two:

```bash
hauksbee from-code examples/board-as-code/blinky.board \
    --route-dsn b.dsn --out b_placed.kicad_pcb
# wrote routing DSN to b.dsn
# ... the unrouted board is still written to --out

java -jar freerouting.jar -de b.dsn -do b.ses -mp 10   # any Specctra router, any budget

hauksbee merge-ses examples/board-as-code/blinky.board b.ses --out b_routed.kicad_pcb
# merged 21 segments, 0 vias from b.ses (merged-ses)
# routed: 9/9 connections, 0 unrouted (merged-ses); endpoint-net violations: 0
# DRC: 0 serious, 0 total
```

`merge-ses` recompiles the board from the same source and re-runs the same
post-route audit, so the merged result is judged exactly as the in-process route
is. One caveat: it recompiles *bare*, so if you exported the DSN after
`--relayout` or `--incremental`, the pads land somewhere else and the merge
scores badly. Export the DSN from the same placement you intend to merge onto.

The hand-off, in `forge-codegen`'s `route_freerouting` module:

1. **DSN export** (`write_dsn`): serialise the placed board to Specctra DSN,
   the board boundary (from `Edge.Cuts`), one image per footprint, a
   padstack per distinct pad/via geometry, the net list, and a default
   width/clearance rule.
2. **Invoke freerouting headless** (`run_freerouting`): spawn
   `java -jar freerouting -de board.dsn -do board.ses -mp <passes>` as a
   child process, poll it, and **kill it if it exceeds a wall-clock
   budget** (autorouting a large board can otherwise run for minutes).
3. **SES import** (`parse_ses` + `merge_ses_into_pcb`): read the routed
   wires and vias back and write them onto the board as copper segments and
   vias on the correct nets.

![the same board routed by freerouting](../assets/storm_routed.png)

### Installing freerouting

Download a release jar from
[github.com/freerouting/freerouting](https://github.com/freerouting/freerouting/releases)
and either point `FREEROUTING_JAR` at it or drop it in a `tools/` directory
up the tree (the engine auto-discovers it). A JRE (`java`) must be on
`PATH`.

**Use the 1.9.0 jar** (`freerouting-1.9.0.jar`). Its headless batch mode
reliably writes the SES and exits even on a partially-routed board. The 2.x
line (tested with 2.2.4) parses and routes correctly but **stalls without
writing the SES unless the board is 100% routed**, which is unworkable for
a batch handoff. The engine prefers a 1.x jar when both are present and
passes `-da` only to 2.x. (The DSN/SES writer was validated against both;
the 10x output-resolution quirk of 1.9.0's SES is handled by detecting the
coordinate scale empirically.)

### Grid A* fallback

When freerouting (or a JRE) is absent, `--route` transparently falls back to
the in-tree grid A* router (`route_grid`); `--route-grid` forces it. It
connects each net's pads with Manhattan paths on a single layer, treats
component bodies as keep-outs, and **reports any net it cannot complete
rather than silently dropping it**. Its honest limitations: one layer, no
vias, no rip-up-and-retry, and it bails on boards whose bounding grid would
exceed a few million cells. It is enough to prove a placement is routable in
the small and to visualise tracks; it is not a production autorouter. Use
freerouting for real boards.

## Where the code lives

| piece | path |
|---|---|
| executable DSL (emit / parse / build) | `vendor/kicad-forge/crates/forge-codegen/src/dsl/` |
| re-layout + incremental + grid router | `vendor/kicad-forge/crates/forge-codegen/src/layout.rs` |
| freerouting handoff (DSN / invoke / SES) | `vendor/kicad-forge/crates/forge-codegen/src/route_freerouting.rs` |
| connectivity-only board comparison | `vendor/kicad-forge/crates/forge-codegen/src/rebuild.rs` (`compare_connectivity`) |
| KiCad library-parity DRC test | `vendor/kicad-forge/crates/forge-codegen/tests/kicad_drc_parity.rs` |
| edit -> simulate loop | `crates/hauksbee-engine/src/boardcode.rs` |
| CLI subcommands (`to-code` / `from-code` / `merge-ses` / `check-code`) | `crates/hauksbee-engine/src/commands/boardcode.rs` |
| round-trip bind-equivalence tests | `crates/hauksbee-engine/tests/boardcode_run.rs` |
| CLI-level tests (DSN export, SES merge, JSON shapes) | `crates/hauksbee-engine/tests/cli_boardcode.rs` |
| miswire edit -> simulate demo | `crates/hauksbee-engine/tests/boardcode_miswire.rs` |
| re-runnable asset/export script | `scripts/board_as_code_assets.sh`, `scripts/make_incremental_viz.py` |
