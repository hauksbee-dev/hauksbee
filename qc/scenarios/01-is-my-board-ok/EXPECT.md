# What a real user should experience

Marco has a Watchy in his hand. He did not design it, he has never run this
tool, and the question he wants answered is "is this board OK?".

He runs:

```
hauksbee run watchy.kicad_pcb --check --plain
```

## What he should see

A report that opens by telling him what it could not fully answer. The SR2HARU
reset supervisor at U1 has a curated identity card, but that card deliberately
has no executable behaviour, so anything that depends on simulating it is
incomplete. The report says so up front rather than treating identity as a
model. Crucially it also tells him what is *not* affected: the copper checks
read the layout and do not care whether an IC is modelled.

Then one section per check, each with a verdict line he can read as a
sentence:

- Copper spacing: "Looks healthy". The board ships with its own KiCad project
  file, so the check runs under the designer's clearance rule, not a default.
  Copper is read straight off the layout, so this verdict owes nothing to
  device models and may claim health on its own.
- Connectivity and signal integrity: "INCONCLUSIVE", because U1 has no
  executable device behaviour. A clean bill from a check that never saw the
  part that matters would be vacuous, the exact false comfort this tool exists
  to refuse. The verdict counts the unmodelled part, names it, and says what
  unlocks a conclusive answer. The three SI heads-up notes underneath are still
  real, and the INCONCLUSIVE line must retain its "Plus 3 things worth a look"
  pointer rather than burying them.
- The closing verdict line says "inconclusive", not "clean", for the same
  reason. U1 can reach every net through the circuit, so no electrical check
  is judged around it and the answer is not even partly judged; the line adds,
  as information only and explicitly not a pass, how many wiring and value
  checks on the nets U1 does not touch found nothing.

Each finding underneath is three parts: what it is, why it matters, what to do.
He never has to ask "so is that bad?".

The report closes with what it did not check, in the plainest terms available:
these checks read the board, they do not run it, so nothing above can see a
rail that sags on inrush or a part that overheats under load. Then it names the
two commands that do run it.

## What he must never see

Anything from inside the program. Not `critical_parts_bound`, not
`serious_count`, not a struct name, not a stack trace, not a doubled article
from a badly assembled sentence. The plain surface exists for a person who does
not know the tool has a JSON layer, and one leaked identifier tells him he is
reading a debug dump rather than a report.

## Why the second step

The plain report tells him to run `--report` for the bind table. That sentence
is a promise about another command's output, so the scenario runs it and checks
the table is really there and really describes the whole bind: 60 of 67 parts
resolved, five of six critical active devices executable, and U1/SR2HARU still
open because its identity card has no behaviour. A hint that misdescribes what
it points at is worse than no hint: it sends a first-time user looking for
something that is not there.

A second existing board, Blinky, is the clean control. Its one critical active
device is executable, so its model-dependent sections may earn a clean verdict.
That control keeps "inconclusive" from becoming a blanket outcome while making
no claim that Watchy's unsupported supervisor is modelled. The distinct clean
verdict with informational SI notes is covered by the existing plain-report
unit test, which checks both the verdict's pointer and the retained observation;
Watchy checks the inconclusive composition end to end.
