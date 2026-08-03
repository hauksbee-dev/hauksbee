# What a real user should experience

Sam has a board where R1 is a 30 ohm 2512 across a 5 V rail. Somebody in
purchasing wants to know whether a 10 ohm part from the same reel would do,
because there are two thousand of them in stock.

Sam does not want to edit the board file to find out. A board file edit is a
change he then has to remember to undo, and if he forgets, the design of record
is now wrong.

## What he does instead

He copies his spec, adds four lines, and runs it:

```toml
[[override]]
ref = "R1"
value = "10"
```

## What he should see

Same board, same two assertions, opposite verdict.

On the board's own value: `Tj(R1) peak 91.7C (<= 125C)`, no stress faults,
GREEN.

With the swap: `Tj(R1) peak 225.0C`, and `R1 overpower 2.500 > 1.000`. RED,
exit 1.

The numbers are the point. 5 V across 30 ohm is 0.83 W and 5 V across 10 ohm is
2.5 W, and the report says 2.500. If the override had been parsed and quietly
dropped, the run would still have reported 91.7 C and GREEN, and Sam would have
ordered two thousand resistors that burn. So the assertion is on the value, not
on the verdict: a green-to-red flip alone would not prove the override reached
the circuit.

## The other direction

He tries a 100 ohm part: 0.25 W, 45 C, green. no_faults has to flip both ways.
A check that only ever goes red on a swap is a check that is not reading the
swap.

## The JSON

The same run through `--json` reports `exit_code: 1`, `passed: false`, and
`run_valid: true`. That last one matters: the run is a trustworthy red, not a
refusal. A pipeline reading the JSON has to be able to tell those apart, and
the human report and the JSON must never disagree about which one happened.
