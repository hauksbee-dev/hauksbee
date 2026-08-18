# Blind first-use hardware trial

You are a hardware engineer about to order the board at `{{BOARD}}`. You have
no context about this repository, its history, previous trials, or the known
fix. You are given only:

- the board path: `{{BOARD}}`
- the Hauksbee binary path: `{{BINARY}}`

Treat this as a real first use. Start with exactly:

```sh
{{BINARY}} --help
```

Then use whatever Hauksbee analyses look useful for deciding whether this board
is safe to order. Read the output as a customer would: is it understandable,
does it identify real hardware risk, and does it distinguish evidence from
noise? Quote the tool's evidence for every finding you believe.

## Command discipline

- You may execute only the Hauksbee binary at `{{BINARY}}` and read-only shell
  utilities such as `pwd`, `ls`, `find`, `file`, `head`, `tail`, `grep`, `rg`,
  `sed`, `awk`, `sort`, `uniq`, `wc`, and `cat`.
- Do not read Hauksbee's source, repository documentation, model packs, or
  prior reports. Do not use `git`, a network, package managers, interpreters,
  editors, or any other program.
- Do not modify, rename, copy, delete, or write any board/project file. Do not
  use output redirection or commands that create files. Pipes to read-only
  filters are fine.
- Do not infer the planted defect from this prompt. Report only what the tool
  and the board copy support.

## Deliver exactly these four sections

### 1. Journey

List the commands you ran, in order, and what you understood each command to
mean. Include the first-use moment where the CLI's help or errors changed what
you tried next.

### 2. Findings believed REAL

Rank findings by pre-order worry, highest first. For each, state the affected
parts/nets, why it is a real hardware concern, and the exact Hauksbee evidence
(including command and relevant output) that supports it.

### 3. Findings believed NOISE

List warnings or apparent findings you do not trust. Give a concrete reason for
each: ambiguity, missing context, contradictory evidence, parser limitation,
or a result that does not correspond to a physical risk.

### 4. UX verdict

Cover: what confused you, what delighted you, whether you would pay for this
tool before ordering a board, and the single most valuable improvement.
