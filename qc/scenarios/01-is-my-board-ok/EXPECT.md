# What a real user should experience

Marco has a Watchy in his hand. He did not design it, he has never run this
tool, and the question he wants answered is "is this board OK?".

He runs:

```
hauksbee run watchy.kicad_pcb --check --plain
```

## What he should see

A report that opens by telling him what it could not fully answer. Two of the
ICs on the board have no device model, so anything that depends on simulating
them is incomplete, and the report says so in the first sentence rather than
burying it. Crucially it also tells him what is *not* affected: the copper
checks read the layout and do not care whether an IC is modelled.

Then one section per check, each with a verdict line he can read as a
sentence:

- Copper spacing: "Looks healthy". The board ships with its own KiCad project
  file, so the check runs under the designer's clearance rule, not a default.
- Connectivity: "Looks healthy" when it is.
- Signal integrity: no failures, but three things worth a look.

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
the table is really there and really describes the whole bind. A hint that
misdescribes what it points at is worse than no hint: it sends a first-time
user looking for something that is not there.
