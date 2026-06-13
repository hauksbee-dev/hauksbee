# Board-as-Code examples

A PCB is a program that draws itself. galvani makes that program executable and
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
| `blinky.board` | The small ATmega328P demo board (`crates/galvani-ci/examples/boards/blinky.kicad_pcb`) decompiled to the DSL. 5 components, the smallest real example to read. |
| `stormduino.board` | A real corpus Arduino-class board (`board-corpus/stormduino`) decompiled. 51 components, 4 repeat-detected blocks. Shows how the decompiler factors repeated hardware into `fn` blocks. |

## The edit-then-recheck loop

Decompile, edit, recompile, and re-simulate any of these:

```bash
BIN=target/release/galvani   # or just `galvani` if installed

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

