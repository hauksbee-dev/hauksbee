# What a real user should experience

Priya's board resets when the radio transmits. She suspects the rail, but the
static checks all pass, because a rail that sags under load is invisible to
anything that only reads copper.

She writes the load into a spec: a 0.5 A USB port, 20 mA idle, a 490 mA burst
for 10 ms, and one assertion saying the 5 V rail must not dip below 4.85 V while
that burst runs.

## The failing run

```
hauksbee-ci run rail.toml
```

Three things have to be in the failure, and all three are load-bearing:

1. **The bound it checked.** `+5V window: min >= 4.85 V [burst]` - the rail, the
   floor, and which scenario was running when it looked.
2. **The value it observed.** `min=4.755V < required 4.85V`. Without the measured
   number the report is an opinion.
3. **A why line.** `+5V sagged to 4.755 V in the window, 0.095 V below your
   4.85 V floor`. Not a restatement: it does the subtraction, so she knows she is
   95 mV short rather than an order of magnitude out.

Exit code 1, and the summary says RED. A red is a real answer, so it must not
be confused with the exit code for a broken spec (2) or an untrustworthy run
(3).

## The fix

95 mV short on a 0.5 A port drawing 0.49 A is the port's output impedance, not
the board's. She changes one line, `5v0.5a` to `5v1.5a`, and reruns.

Green, `min=4.918V`. The number moved up, by about the amount a lower source
impedance predicts. That correspondence is what makes the tool trustworthy: the
fix she reasoned about and the number the tool reports agree.

## Asking past the port's rating

The last step asks the same question with a 600 mA burst on a 500 mA port. A
brownout is the correct answer and a red is expected: that is a real budget
overrun and the tool should say so.

What is not acceptable is a *negative* steady-state voltage on a positive rail.
This board has no protection circuit to latch off and no inductance in the
supply path, so nothing in it can drive the rail below ground. A number like
`min=-0.300V` in that situation is the model reporting something that cannot
happen, in the exact check the tool advertises as the reason to use it, and it
tells the engineer nothing about how far over budget they are: it is the same
answer for one percent over as for a dead short.

A negative excursion *is* physical in a different situation, and
`docs/checks/TRANSIENTS.md` records one: after a battery protection cutoff
latches, inductive current still flowing drives the rail below zero for a
moment. That case has a mechanism. This one does not, so the correspondence of
numbers between them is not a reason to loosen this assertion.

## What would make this scenario a lie

If the failing report gave only "RED" with no measured value, or gave a value
that did not move when the supply changed, the check would be a coin flip
wearing a report. The scenario asserts both numbers explicitly for that reason.
