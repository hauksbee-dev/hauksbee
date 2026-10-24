# Do-not-populate parts

A DNP ("do not populate") marking means the assembler does not place that part.
The pads are on the board. The component is not.

Designers use the flag for two opposite things, and that is the whole
difficulty:

1. **"Not on the assembly BOM, but it will be there."** A socketed module (an
   Arduino Nano, an ESP32 carrier) bought separately and plugged into headers,
   a footprint stuffed by hand later, or a part fitted at rework. Analyzing the
   board without it answers a question nobody asked.
2. **"This link is deliberately open."** A 0R bridge between two ground
   planes, a solder jumper that selects a mode, or a config strap. Fitting one
   of these merges nets the designer split on purpose. The tool would then
   report one ground plane on a board that has two.

## What hauksbee does by default

**hauksbee leaves every part the source says is not built out of the circuit
and the checks.** That is a part marked DNP (KiCad 7/8/9 `dnp`, Eagle
`populate="no"`, a netlist `dnp` property), a part kept off the board
(`exclude_from_board`, `on_board no`) that has connections, and a part whose
value field says so (`DNP`, `DNF`, `NC`, `do not populate`, `not fitted`, in any
case, as a whole word). A second regulator marked DNP no longer fights the
fitted one for the rail, and a DNP load no longer appears as an overstressed
part.

**"Excluded from the BOM" does not mean "not fitted".** Card-edge fingers, test
pads, net ties, copper-only keys, mounting holes and logos are on the board but
are not bought, so KiCad's `exclude_from_bom` / `(in_bom no)` leaves a part
fitted. Such a part still counts as not on the bill of materials for the value
and ordering checks; it only stays in the circuit. Mark a part DNP (or put `DNP`
in its value field) to leave it out.

If you do want a DNP part simulated, say so:

- `--fit R7` (or `fit` in a spec) fits one named part.
- `--fit-dnp` fits every DNP part except near-zero-ohm links (a 0R bridge,
  a ferrite, a net tie, a jumper), which stay open because fitting one merges
  the nets it bridges.
- `--fit-all-dnp` fits every DNP part, links included.

The link rule used by `--fit-dnp`: a part with two or fewer pins and any of:

- a resistance of 0.5 Ω or less (`0`, `0R`, `0R0`, `R000`, `0.1`)
- a ferrite bead: value starting `FB`, or `ferrite` anywhere in the value **or**
  the footprint
- a bridging **value**: `JUMPER`, `SOLDER_BRIDGE`, `SOLDERBRIDGE`, `NET_TIE` or
  `NETTIE`, case-insensitively

The bridging test reads the component's *value* only; ferrite is the one kind
matched against the footprint as well.

Every run prints its decision, so the choice is never silent:

```
do-not-populate: DNP parts are left out of the simulation, matching the board file
  left open: A101 (Arduino_Nano_v3.x), DNP, left open
  left open: R7 (0R), DNP, left open
```

A board with no DNP parts prints nothing about DNP.

One exception to "leave it out" under the default: a plug-in module (a compute
module, SoM or dev board, told by an `A<n>` or `MOD` designator, a module or
SODIMM symbol or footprint, or a `SoM` value, with six or more connected pads) that
is marked DNP because it is bought separately, and whose connector or socket is
fitted. The socket carries at least half of the module's nets, so the module is
treated as plugged in and printed as `fitted:    A101 (...), DNP plug-in module on a
fitted socket, treated as plugged in (--no-fit to leave it out)`. A module whose
socket is itself DNP, or that no fitted connector shares nets with, stays open, and
`--no-fit A101` leaves it out whatever its socket.

## Choosing something else

Per part, on the command line:

```bash
hauksbee run board.kicad_pcb --fit A101        # fit this one although it is DNP
hauksbee run board.kicad_pcb --no-fit R7       # leave this one open regardless
```

Whole-board policy:

```bash
hauksbee run board.kicad_pcb --honour-dnp      # leave every DNP part out (the default)
hauksbee run board.kicad_pcb --fit-dnp         # fit every DNP part except near-zero-ohm links
hauksbee run board.kicad_pcb --fit-all-dnp     # fit every DNP part, links included
```

The same controls exist in a `hauksbee-ci` spec, so a pipeline records the
decision alongside the checks instead of depending on how someone invoked the
CLI:

```toml
board = "hardware/board.kicad_pcb"
firmware = "firmware/build/app.elf"

dnp = "honour"   # the default; or "fit-except-links", or "fit-all"
fit = ["R7"]               # fit these regardless of the policy
no_fit = ["A101"]          # leave these open regardless of layout DNP state

# A spec needs at least one assertion; without an [[assert]] block
# `hauksbee-ci run` refuses:
#   invalid spec: spec has no [[assert]] blocks: a check with no assertions
#   always passes vacuously
[[assert]]
kind = "no_faults"
```

Naming an unknown reference is an error. Naming the same part in both `fit`
and `no_fit` is also an error. A typo should fail loudly, not quietly change
nothing.

### Checked-in assembly variants

When one superset layout has several fitted assemblies, put the population
decision in its own small TOML artifact instead of treating the BOM's populate
column as executable policy:

```toml
# hardware/prototype.variant.toml
name = "prototype without sensor"
fit = ["R7"]
no_fit = ["U4"]
```

Then reference it from the CI spec:

```toml
board = "hardware/board.kicad_pcb"
variant = "hardware/prototype.variant.toml"
duration_ms = 1

[[assert]]
kind = "no_faults"
```

The variant's `fit` and `no_fit` lists merge with any lists written directly
in the spec. `fit` can place a layout-DNP part. `no_fit` can leave out an
ordinary part when the layout describes a superset assembly. Unknown refs,
duplicates within one artifact, and contradictions refuse. Identical decisions
in the spec and variant deduplicate; opposite decisions refuse. The exact
variant bytes and the named decision are separate entries in the CI evidence
inventory and immutable run manifest; a purchasing spreadsheet never overrides
them implicitly. CI refuses a selection which leaves every board component
open: `no_faults` on an empty circuit would be a vacuous pass, not evidence
about the design. It also refuses a simulated peripheral whose `ref` names a
component this variant leaves open, even when the peripheral declares its net
explicitly; routing information cannot make an absent physical device present.

## Firmware on a board with no processor

If the DNP policy leaves the board with no processor and the run asks for
firmware, hauksbee exits 3 (invalid for analysis) instead of reporting a pass.
Nothing executes on a board with no processor, so every "the firmware must ..."
assertion would pass without ever being tested, and a green result would mean
less than nothing. The error names the part and both ways out:

```
cannot run firmware: this board bound zero processors. A101 (Arduino_Nano_v3.x)
is marked DNP in the board file, so it was not simulated. If the module is
really fitted (socketed modules are often marked DNP because they are bought
separately), re-run with --fit A101. If it is really absent, drop --firmware and
analyse the board without it.
```

## What DNP never affects

Copper is copper. The geometric DRC reads the layout directly and never
consults the DNP flag, so a DNP footprint's pads, and the traces that run to
them, are still checked for clearance and shorts. Only the component is
absent.

The component-level checks do skip parts left open, because those checks ask
whether a part is doing its job, and a part that is not there does nothing. That
covers more than the obvious ones: converter topology, USB-C CC termination,
crystal and antenna signal integrity, trace ampacity, supply ripple, boot straps
and boot-mode transistors, bus contention, device decode, and MCU pin coverage
all consult the flag and pass over a part the policy left absent.
