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

Most DNP footprints get placed eventually, so **hauksbee simulates DNP parts
as fitted**, with one exception: **near-zero-ohm links stay open**, because
fitting one changes the board's topology instead of adding a component to it.

A part counts as a link when it has two or fewer pins and any of:

- a resistance of 0.5 Ω or less (`0`, `0R`, `0R0`, `R000`, `0.1`)
- a ferrite bead (value starting `FB`, or `ferrite` in the value or footprint)
- a bridging footprint (`JUMPER`, `SOLDER_BRIDGE`, `NET_TIE`)

Every run prints its decision, so the choice is never silent:

```
do-not-populate: DNP parts are simulated as fitted (they are usually placed
eventually), except near-zero-ohm links, which stay open because fitting one
merges the nets it bridges
  fitted:    A101 (Arduino_Nano_v3.x), DNP, fitted by default
  left open: R7 (0R), DNP link (near 0 ohm), left open: fitting it merges nets
```

A board with no DNP parts prints nothing about DNP.

## Choosing something else

Per part, on the command line:

```bash
hauksbee run board.kicad_pcb --fit R7          # fit this one, even though it is a link
hauksbee run board.kicad_pcb --no-fit A101     # leave this one open
```

Whole-board policy:

```bash
hauksbee run board.kicad_pcb --honour-dnp      # leave every DNP part out, as a fab house builds it
hauksbee run board.kicad_pcb --fit-all-dnp     # fit every DNP part, links included
```

The same controls exist in a `hauksbee-ci` spec, so a pipeline records the
decision alongside the checks instead of depending on how someone invoked the
CLI:

```toml
board = "hardware/board.kicad_pcb"
firmware = "firmware/build/app.elf"

dnp = "fit-except-links"   # or "fit-all", or "honour"; this is the default
fit = ["R7"]               # fit these regardless of the policy
no_fit = ["A101"]          # leave these open regardless of the policy
```

Naming an unknown reference is an error. Naming the same part in both `fit`
and `no_fit` is also an error. A typo should fail loudly, not quietly change
nothing.

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

The component-level checks (converter topology, USB-C CC termination, crystal
and antenna signal integrity) do skip parts left open, because those checks
ask whether a part is doing its job, and a part that is not there does
nothing.
