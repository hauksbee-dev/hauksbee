# Blind first-use trials

This directory is the repeatable harness for a zero-context hardware engineer
trying Hauksbee on an unseen, real upstream board. The harness pins both sides
of a known change: the agent sees the PRE-FIX bytes, while the FIX ref and
planted defect are printed only after setup for the operator's evidence ledger.

## Run one trial

From the repository root, prepare a board:

```sh
bash qc/blind_trials/run.sh doorbell
```

The script:

1. reads the board registry;
2. shallow-clones the upstream repository into `qc/blind_trials/work/<id>/` (or
   reuses that clone), checks out the pinned PRE-FIX ref, and verifies `HEAD`;
3. copies the board and its KiCad project siblings into
   `work/<id>/board/`—all `.kicad_sch`, `.kicad_pro`, `.kicad_prl`,
   `fp-lib-table`, and `sym-lib-table` files in the board's directory;
4. strips copied `*.md`, `*.txt`, `*.csv`, and `*.xlsx` engineering-note/BOM
   files and prints the exact removal list; and
5. prints the absolute board-copy path and a command that renders
   [`prompt.md`](prompt.md) with the board and binary paths. It never invokes an
   agent.

Run the printed prompt-rendering command, give its result to a fresh blind
agent, and set `HAUKSBEE_BIN` if the binary is not at
`target/release/hauksbee`. The agent must receive only the rendered prompt and
the two paths. Do not show it the final separator section containing the defect
or fix ref.

## Persist the evidence

Reports belong in the committed evidence trail, not in ignored `work/`:

```text
qc/results/blind-trials/<id>-<date>.md
```

Use an ISO date such as `2026-08-18`, and commit the report with the rest of
the reviewed evidence. Preserve the agent's four sections from `prompt.md`:
Journey, Findings believed REAL, Findings believed NOISE, and UX verdict. Add
the exact binary identity, PRE-FIX ref, board-copy path, and raw command/output
snippets needed to reproduce the report. The FIX ref and planted defect are
operator-side annotations, not input to the blind agent.

## Blindness and board selection rules

Every trial uses a **fresh agent** with no knowledge of prior rounds, prior
reports, or prior fixes. Otherwise the result grades the improvement curve
instead of the artifact the first-time user receives.

The standing exclusion list is strict: never use a board already listed in
the repository's `corpus.toml`, and never reuse a board that has appeared in a
previous blind trial. Exclude the same upstream design even if you change its
local filename or choose a nearby revision. Add a new registry entry only
after checking both exclusions and recording the exact upstream refs.

The three seeded entries are deliberately fixed regression trials. Their
known defects remain hidden from the agent; the later FIX ref is there to make
the evidence trail falsifiable.
