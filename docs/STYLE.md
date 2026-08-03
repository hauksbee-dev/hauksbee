# Style contract

Every surface hauksbee has (CLI text, plain-language explanations, JSON fields,
spec keys, docs) is read by someone deciding whether to trust a board. This page
is the contract those surfaces are held to. It is short on purpose: a rule you
cannot recite is a rule nobody applies in review.

## 1. The error template

Every error a user can hit says three things, in this order:

1. **What hauksbee could not do.** In the user's terms, not the code's.
2. **The specific input that caused it.** The path, the net name, the refdes,
   the spec key: whatever they typed or drew.
3. **One concrete next action.** A command to run, a file to fix, a flag to add.
   Exactly one, the most likely one. A list of five possibilities is a way of
   saying we do not know.

**Never print an internal identifier the user did not type.** No struct names,
no enum variants, no crate paths, no `NodeId(37)`. If the only handle you have on
something is internal, the message is not finished. This is the difference
between a message that closes the loop and one that sends someone to the source.

Good:

```
error: unrecognized board format; tried altium, eagle, kicad-netlist,
kicad-schematic, kicad-pcb, ipc-d356
```

It names the failure, and every format it tried is a thing the user can check
their file against.

## 2. The verdict lexicon

One word per concept, on every surface. Synonyms make two surfaces look like two
tools.

| Concept | The words | Never |
|---|---|---|
| A run | GREEN or RED | passed/failed, ok/broken, success |
| An assertion | passes or fails | holds/violated, met/unmet, green/red |
| A finding's severity | `serious`, `warning`, `note`, `info` | critical, major, minor, error, issue |
| A binding | `exact`, `family`, `guessed`, `unresolved` | matched, partial, approximate, unknown |
| A refusal | invalid for analysis | error, failure, inconclusive, N/A |

Prose may group everything below `serious` as **advisory**, and that is the only
sanctioned collective term. A refusal is never a failure: exit 3 and
`status: "invalid_for_analysis"` mean the run declined to vouch for itself, which
is an answer.

The same word must mean the same thing in the terminal, in `--json`, in the web
UI, and in these docs. If you introduce a concept, add a row here before you ship
the surface.

## 3. Tables

- Box-drawing characters (`┌ ─ ┬ │ ├ └`) in terminal output. Markdown tables in
  docs.
- **Deterministic sort.** Two runs on the same board produce byte-identical
  tables. Sort by something stable and say what it is: never by hash-map
  iteration order, and never by anything derived from wall-clock time or
  parallel completion order.
- One header row, no nested headers, no merged cells. A column whose meaning
  needs a footnote needs a better name.
- Numbers right-aligned in their column, units in the header or attached to
  every value, never both.

## 4. Naming

- **User-facing spec vocabulary is `snake_case`**: assertion kinds, supply
  kinds, spec keys, JSON field names. `boot_coverage`, `no_faults`, `max_temp`,
  `duration_ms`, `rail_window`. The one historical exception,
  `kind = "boot-coverage"`, is a deprecated alias folded onto `boot_coverage`
  before matching. Do not add a second exception.
- **CLI flags are `kebab-case`**: `--list-nets`, `--fail-on-findings`,
  `--models-dir`, `--ac-node`.
- A flag and the spec key that does the same job should be recognisably the same
  word in the two conventions.
- Refdes and net names are echoed exactly as the board file spells them,
  including case. They are the user's names, not ours.

## 5. The honesty clause

**A result you cannot stand behind is stated as such in the same breath as the
result, not in a footnote.** Where a number might be wrong, the surface that
prints the number also prints why, and does it inline.

Every such statement names two things:

- **The blast radius.** Which nets, which parts, which analysis. "Analog, AC and
  thermal results on U6's nets are not trustworthy" is a blast radius. "Results
  may be inaccurate" is not.
- **The remedy.** The thing the user does to make the caveat go away. Add a
  model, install a backend, supply the missing value.

The failure mode this guards against is a plausible number with no marker on it.
A tool that quietly rounds a coverage hole into a verdict gets switched off the
first time someone catches it, and a switched-off tool catches nothing.

## 6. The repetition rule

**A fact appears once per report, at its highest-value position.** If the bottom
line says two active ICs are unresolved, the section above it does not repeat the
count, and the table does not summarise itself. Restating a fact reads as
padding, and it trains readers to skim past the one place it mattered.

Highest-value position means: where the reader is making the decision the fact
bears on. A caveat about U6 belongs next to the number it undermines, not in a
notes block at the end. A remedy belongs where the reader has just learned they
need it.

The exception is the machine surface. `--json` carries every fact as a field
whether or not the human rendering repeated it, because a consumer cannot infer
a field from prose.

## 7. Mechanics

- No em dashes. Use a comma, a colon, a semicolon, or two sentences.
- The tree uses British spelling in prose (`analyse`, `behavioural`,
  `notarised`, `licence` as the noun). Match it. Identifiers keep whatever the
  code already uses.
- Comments explain **why**. The what is in the code.
- No references to review rounds, audits, or feedback inside an artifact. A doc
  reads as if it were written right the first time.

---

Reviewers: [`../CONTRIBUTING.md`](../CONTRIBUTING.md) lists what a change has to
clear. This page is the surface half of it.
