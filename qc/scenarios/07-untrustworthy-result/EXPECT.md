# What a real user should experience

Every simulator can produce a number. The interesting question is whether it
produces one when it should not.

Ravi asks for an AC sweep of a divider whose only source is DC. There is no AC
stimulus anywhere in the deck, so the small-signal drive is zero and every point
of the sweep would come back as zero. A table of zeros is not an answer. It is
worse than no answer, because it looks like data and it plots.

## What should happen

Exit 3, and nothing on stdout that could be mistaken for a result.

The refusal has three jobs:

1. **Name what was refused.** "AC analysis refused".
2. **Say why the number would have been wrong.** The drive is identically zero,
   so the response would be a meaningless all-zeros table. Without this, the
   user's next move is to argue with the tool.
3. **Say what to change.** Add `AC 1` to the driving source. A refusal with no
   route out of it is a dead end.

## Why exit 3 and not 1

Exit 1 means "the thing you checked is bad". Exit 3 means "I cannot tell you
whether the thing you checked is bad". A pipeline that conflates them either
blocks a good change or, much worse, lets a red build through as an
"unstable" one. Three distinct codes exist so the pipeline can distinguish
answers from non-answers, and the only way to keep that distinction real is to
assert it.

## Why the second and third steps exist

A tool that refused everything would also pass step one. So the same sweep runs
against a deck that *does* carry an AC source, and has to come back with real
CSV data at exit 0. The refusal is discriminating, not a blanket bail-out.

And the third step separates exit 3 from exit 2: `--format both` with nowhere to
write is a usage error, not an untrustworthy result. Both are non-zero, both are
refusals in plain speech, and they must not share a code.

## A note on the assertion runner

`hauksbee-ci` reaches exit 3 when the analog solve fails a chunk inside an
assertion's window. That state is not constructible from the spec surface with
the bundled fixtures, and deliberately so: forcing it needs two ideal voltage
sources on one node, which no KiCad file and no `[[supply]]` block can express.
The engine's own tests force it at the scheduler boundary instead
(`crates/hauksbee-engine/tests/cosim_failed_chunk.rs`,
`crates/hauksbee-ci/tests/analog_invalid.rs`). So this scenario exercises the
exit-3 contract on the surface where a user can actually reach it, which is
`hauksbee sim`.
