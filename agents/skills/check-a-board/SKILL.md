---
name: check-a-board
description: Check a PCB or schematic end to end with Hauksbee - import it, see which parts bound to models, run the static checks, and report the verdict with its evidence. Use when asked to check, review, or "is this board OK to order" for a KiCad, Eagle, Altium, IPC-D-356, gerber, or Board-as-Code file.
---

# Check a board

The goal is a verdict you can defend: what was checked, what failed, and what
could not be judged. Parse the JSON and show the user the plain text; both
carry the same facts.

## 1. Confirm the tool and the input

```bash
hauksbee doctor                 # which co-sim backends this build has
hauksbee run --example blinky --check --plain   # smoke test, exit 0
```

Accepted inputs: `.kicad_pcb`, `.kicad_sch`, `.net`, `.brd`, `.PcbDoc`,
`.d356`, a gerber folder or zip, or `.board`. A missing file exits 1 with
`error: no board file at ...`.

## 2. See what bound

```bash
hauksbee run BOARD --report --plain          # bind table: every part and its model
hauksbee models coverage BOARD               # active devices: unresolved / identity-only / executable
```

An unbound active part is simulated as an open circuit. It is named in the
output, never guessed. Note the `critical_parts_bound` ratio (for example
`2/6`) before reading any verdict.

## 3. Run every static check

```bash
hauksbee run BOARD --check --json --strict > check.json; echo $?
hauksbee run BOARD --check --plain           # the same facts, for the human
```

`--check` runs bind coverage, DRC (shorts and clearance), the connectivity
lint, signal integrity, USB-C CC, and the MCU resource, strap-pin,
config-decode and contention checks. Without `--strict` the command exits 0
even with findings.

Exit codes for `hauksbee run` with `--strict`:

| exit | meaning |
|---|---|
| 0 | `verdict: "pass"` |
| 1 | input error: the board could not be read |
| 2 | `verdict: "fail"`: at least one gating finding (also a usage error) |
| 3 | `verdict: "invalid"`: nothing gates, but the run could not be judged (an unbound current-carrying or active part on a model-dependent surface) |

## 4. Read the JSON

Top-level keys to read first: `verdict`, `serious_count`, `actionable_count`,
`bind`, `findings`, `notes`, `triage`, `evidence`, `assumptions`.

- `verdict` is `pass`, `fail`, or `invalid`. `fail` outranks `invalid`.
- `findings[]`: each has `check`, `kind`, `severity`, `gating`, `actionable`,
  `refs`, `nets`, `message`, `plain` and `fix`. Only `gating: true` findings
  fail the strict gate.
- `bind.active_path_unresolved[]` names each open active IC and why.
- `notes[]` with `kind: "coverage"` start with `INCONCLUSIVE:` when parts
  that carry current have no model. A clean report with this note is not a
  clean bill.
- `triage` sorts findings into `do_not_order`, `inspect`, `checked_ok`, plus a
  `not_covered` sentence.
- `clearance_rule_source.source` is `defaulted` when no `.kicad_pro` was beside
  the board. Clearance findings then use 0.200 mm, not the user's netclass
  rules. Say so.

Full schema: `docs/analysis/JSON_OUTPUT.md`.

## 5. Report

Give the user, in this order:

1. The verdict and exit code.
2. Gating findings, each with refs, nets, and the tool's `fix` text.
3. What was not judged: open parts, default clearance rules, a KiCad format
   newer than the DRC parity range (reported as UNVALIDATED), and the fact
   that static checks do not power the board.
4. The next command: `hauksbee-ci init BOARD` for dynamic checks (see the
   `write-a-ci-spec` skill), or the `model-a-hardware-board` skill to close
   model gaps.

Do not call an `invalid` run a pass or a failure. Treat a finding on a board
the user knows works as a possible Hauksbee false positive and say so, rather
than explaining it away.

## Single reports

`--drc`, `--lint`, `--si`, `--resources`, and `--usb-c` each run one surface
and accept `--json`, `--plain`, and `--strict`. `--report` (the bind table)
and `--thermal` accept `--json`. `--junit FILE` and `--sarif FILE` write CI
artifacts for the selected checks. `--list-nets` prints net names, and
`--emit-netlist` prints connectivity.

## Over MCP

`analyze_board` with `board_path` returns the same analysis as one object:
`headline`, `serious`, `total`, `sections`, `bind`, `triage`, `nets`,
`supplies`, `model_coverage`, `evidence`. See `agents/AGENTS.md`.
