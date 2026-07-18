# Board-as-Code examples

A PCB is a program that draws itself. hauksbee makes that program executable and
editable: decompile a real `.kicad_pcb` into readable text, edit the text
(change a value, fix a wiring swap, add a part), recompile it back into a
coherent board, and run the edit straight through simulation to see whether the
fix worked. See [`docs/BOARD_AS_CODE.md`](../../docs/BOARD_AS_CODE.md) for the
full DSL reference.

The loop, in three CLI verbs:

```
.kicad_pcb ──to-code──▶ board.board  (editable text)
                              │  edit: value / wiring / add-remove parts
                              ▼
                        from-code ──▶ .kicad_pcb'   (connectivity-equal)
                              │
                          check-code ──▶ extract ▸ bind ▸ co-sim ▸ stress ▸ report
```

## Files here

| File | What it is |
|---|---|
| `starter.board` | A **hand-authored** three-part board (a 2-pin header → resistor → LED), written from scratch, not decompiled — the smallest thing to copy when authoring your own. Richly commented; `check-code` it directly. See the [authoring-from-scratch walkthrough](../../docs/BOARD_AS_CODE.md#authoring-a-board-from-scratch). |
| `blinky.board` | The small ATmega328P demo board (`crates/hauksbee-ci/examples/boards/blinky.kicad_pcb`) decompiled to the DSL. 5 components, the smallest real *decompiled* example to read. |
| `stormduino.board` | A real corpus Arduino-class board (`board-corpus/stormduino`) decompiled. 51 components, 4 repeat-detected blocks. Shows how the decompiler factors repeated hardware into `fn` blocks. |
| `tarski_miswire_repair.rs` (in `crates/hauksbee-engine/examples/`) | The headline demo: the Tarski inhibitory-synapse miswire, repaired as a code edit, run through simulation. See below. |

## The edit-then-recheck loop

Decompile, edit, recompile, and re-simulate any of these:

```bash
BIN=target/release/hauksbee   # or just `hauksbee` if installed

# 1. decompile a board to editable text
$BIN to-code "board-corpus/stormduino/stormduino Rev2.kicad_pcb" --out storm.board

# 2. edit storm.board (change an R value, swap a net, add a comp)...

# 3. recompile - --incremental keeps the settled placement, re-placing only
#    the parts you changed
$BIN from-code storm.board --incremental --out storm_patched.kicad_pcb

# 4. run the rebuilt board through bind + co-sim + the stress monitor
$BIN check-code storm.board --seconds 0.05
```

`check-code` prints a fault report; a clean board ends with
`no faults: circuit is within ratings.` A real captured run of exactly this
loop is in [`../sessions/09_board_as_code_loop.txt`](../sessions/09_board_as_code_loop.txt).

## The Tarski miswire, repaired as a code edit (the headline)

The inhibitory synapse cells cross the dual-NPN's base and collector: IC3906
pad 5 (B2) is wired to the weight-switch common instead of pad 3 (C2). Enabling
the weight then slams the base toward the rail through the switch's 6 ohm
on-resistance and pulls destruction-scale current.

The repair is one edit in the DSL: swap the nets on IC3906 pad 5 and pad 3.
The runnable demo extracts the real cell from the Tarski netlist, applies that
edit in code, recompiles, and re-simulates both versions:

```bash
cargo run --release -p hauksbee-engine --example tarski_miswire_repair
```

Expected output (deterministic):

```
as-wired (code unchanged): V(COM)=0.865 V, I~689.2 mA
  faults: ANALOG_SWITCH3905_s1 PinOvercurrent, IC3906_q2 Overcurrent, IC3906_q2 Overpower

repaired (code edit):      V(COM)=5.0000 V, I~0.424 uA
  faults: none

The one-line net swap on IC3906 pad 5/3 took the cell from 3 fault(s) to 0 fault(s).
```

The one-line wiring edit takes the cell from ~689 mA of destruction-scale base
current (3 stress faults) to ~0.42 µA of controlled sink (0 faults). This is
the integration test `boardcode_miswire::code_edit_repairs_the_miswire`, lifted
into a runnable example. It is corpus-gated: it needs
`testdata/tarski_inputsystem.net`, and prints a notice and exits cleanly if that
file is absent.
