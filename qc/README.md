# Scenario QC

Ten simulated engineering sessions, run end to end against a real build.

Every other suite in this repository checks a unit, a function, or a report in
isolation. This one checks what a person experiences: the wording of a verdict,
the exit code a pipeline reads, whether a diagnosis names the number it measured,
whether an error message leaves you somewhere you can act from. Those are the
things that can be individually correct and collectively unusable, and nothing
else here looks at them.

```
qc/run.sh                          # every scenario, against target/release
qc/run.sh --scenario 04            # just the ones matching "04"
qc/run.sh --list                   # what exists
qc/run.sh --bin-dir /path/to/bins  # a build somewhere else
```

Nothing is built for you. The suite asserts on the behaviour of a specific build,
and silently rebuilding would hide which build was measured.

## What a scenario is

A directory under `scenarios/` holding two files.

**`scenario.toml`** is the executable part: who is running this and why, what
counts as success, and then the steps. A step either does something to the
working directory (`copy`, `write`, `patch`, `uncomment`, `truncate`) or runs a
command and asserts on the result: the exit code, substrings that must appear,
substrings that must not, regexes either way, JSON keys and values, JSON against
the schema this repo ships, XML well-formedness, and a wall-clock ceiling.

**`EXPECT.md`** is the part a human reads: what the session should feel like, and
why each assertion is there rather than a weaker one. When a scenario fails and
the fix is not obvious, EXPECT.md is where the intent lives.

Each scenario runs in its own fresh working directory under
`results/<timestamp>/work/`, so a scenario that edits a board edits its own copy.

## The scenarios

| id | session |
| --- | --- |
| 01 | Is my board OK? A first run, in plain language, with no internals leaking |
| 02 | Why does the rail sag? A failing rail window, diagnosed, then fixed |
| 03 | Can I swap this part? A spec override, and the numbers that prove it landed |
| 04 | Gate my repo. Scaffold to green, then a board that moves on without the spec |
| 05 | Firmware boot check. Two firmware images, one board, opposite verdicts |
| 06 | The spec is wrong, not the board. Five typos, five exit-2s |
| 07 | Untrustworthy result. An analysis refused at exit 3 rather than faked |
| 08 | Export and reproduce. JSON against the schema, JUnit as real XML, twice the same |
| 09 | Hostile input. Six bad files, six human messages, no panics |
| 10 | Waiver lifecycle. Overruled on both gates, visible, then lapsed |

## Timing

Every step carries a 30-second ceiling by default and fails if it overruns, even
when its assertions hold. These are sub-second flows; a thirty-second one is a
regression that no assertion here is looking at directly.

## Output

The table goes to the terminal. `results/<timestamp>/report.md` carries the same
table plus every step's command, exit code, duration, stdout and stderr, with the
working directory and the repo root substituted out so two runs on two machines
diff cleanly. The process exits non-zero if any scenario failed.

## When behaviour changes on purpose

Change the scenario in the same commit. That is the whole discipline: a scenario
that has been loosened to make a red run green has stopped being a check. If the
new wording is better, pin the new wording, and say so in the commit message.
