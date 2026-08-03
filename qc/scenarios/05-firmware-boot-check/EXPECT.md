# What a real user should experience

There is a 2N7002 on the board with no pull resistor on its gate. At reset the
GPIO driving that gate is Hi-Z, so the gate's power-up level is genuinely
undefined. No static check can settle this: a netlist cannot tell whether a
floating gate is a bug the firmware should fix or a state the designer accepted.
Running the firmware settles it.

## The passing side

Firmware A configures the pin as an output and drives it high. The report says
the net was driven to 3 V at 1.00 ms, inside the 20 ms deadline, and that the
boot window before that was clean. The *time* has to be in there. "In time" with
no number is unfalsifiable, and the margin is what tells the engineer whether
they are one millisecond or nineteen from a red build on the next firmware
change.

## The failing side

Firmware B never touches the pin. The report has to say four things:

- the net: `GATE_CTRL`
- the level it never reached: `>= 3 V`
- what happened instead: left Hi-Z for the whole run
- the range it was observed over, so the reader can see it was near zero
  rather than marginal

And then the why line names both plausible causes, in order of likelihood: the
`firmware = ...` line points at the wrong image, or the net is not a GPIO the
firmware drives. A never-driven net is much more often a misconfigured spec than
a firmware bug, and a report that only blames the firmware sends the reader to
the wrong file.

Exit 1. A red build, not a spec error and not a refusal.

## Both at once

A pipeline runs the whole spec directory in one invocation. Both specs must run
(the red one must not stop the green one), each gets its own verdict line, and
the process exits with the worse of the two. The severity order is printed with
the exit code so nobody has to look it up to interpret a mixed run.
