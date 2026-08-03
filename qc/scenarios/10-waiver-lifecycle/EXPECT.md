# What a real user should experience

A check fired on Tomas's board and he has decided it is wrong: the pad geometry
in the file says two nets touch, the assembled panel measures open. He has two
bad options and one good one. The bad ones are living with a red build, and
switching the check off. Nobody switches off one rule; they drop the suite, and
then the tool stops catching the things it was right about.

The good option is overruling that one finding, with a reason and a date.

## What has to be true when a waiver is active

**Both gates agree.** He puts one `hauksbee-waivers.toml` beside the board. The
copper gate (`run --check --strict`), the single-check gates (`--drc --strict`)
and the assertion runner (`hauksbee-ci run`) all read it and all reach the same
verdict. If they disagreed, a green `--check` would imply a waiver that a
pipeline gate then ignores, and that is worse than having no waivers at all,
because it produces a green local run and a red CI run with no explanation.

**It is visible.** The waived findings get their own section, with the finding,
the reason, and the expiry date. A board carrying two overruled findings must
not look identical to a clean one, or the waiver file becomes invisible
infrastructure that nobody reads and nobody removes.

**The measurement survives.** `Tj(R1) peak 156.7C > ceiling 125C` stays on
screen next to `[WAIVED]`. A waiver overrules a finding; it does not delete the
number that produced it.

**The JUnit says skipped, not passed.** A CI provider renders the XML, and a
waived failure that reports as a pass is a lie told to the dashboard. Skipped,
carrying the reason, is the honest shape.

## What has to be true when it lapses

The date arrives and the finding comes back, on both gates, at the same exit
codes as before the waiver existed. And the report names the lapsed waiver:
`drc/short expired 2026-01-01: measured open on the assembled panel`. Somebody
else's build just went red for a reason that has nothing to do with their
commit, and they must be able to understand it from the output alone.

## What has to be refused

A waiver with no `reason` is refused: six months on, a waiver with no reason
cannot be told apart from a bug.

A waiver naming neither `nets` nor `refs` is refused: it would silence the rule
across the whole board, which is turning the check off with extra steps, and it
is the shortest form to write so it is the most tempting one.

Both refusals ignore the *file* and let everything gate, with a warning. Failing
open would mean a typo silently disables a check. Refusing to run at all would
punish the whole board for one bad line.
