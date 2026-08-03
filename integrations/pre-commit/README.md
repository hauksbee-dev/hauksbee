# hauksbee pre-commit hooks

Block a commit when a staged board is broken. This repo exports two hooks to
the [pre-commit](https://pre-commit.com) framework (declared in
`.pre-commit-hooks.yaml` at the repo root):

- **`hauksbee-check`**: zero-config. Runs `hauksbee run <board> --check
  --strict` on every staged board file. No spec needed.
- **`hauksbee-ci`**: spec-driven. Discovers checked-in hauksbee-ci specs and
  runs the ones whose board is staged, full co-simulation included.

## Quick start (remote hook)

Add to the `.pre-commit-config.yaml` at your repo root, then `pre-commit
install`:

```yaml
repos:
  - repo: https://github.com/hauksbee-dev/hauksbee
    rev: v0.x.y
    hooks:
      - id: hauksbee-check
```

That is the whole setup, provided the `hauksbee` binary is on `PATH` (or
`HAUKSBEE_BIN` points at it). Every staged `.kicad_pcb`, `.kicad_sch`, `.net`,
`.brd`, `.d356`, `.PcbDoc`, or `.board` file is checked; gate-grade findings
block the commit. Exit codes pass through unchanged: `2` means findings, `3`
means the run was invalid for analysis (the analog solve aborted), and both
block. See `docs/ci/CI.md` (Exit codes) for the exact gating semantics of
`--check --strict`.

## The spec-driven hook: hauksbee-ci

When you want more than the default check union (supplies, stimuli, timed
assertions, MCU firmware co-simulation), write a hauksbee-ci spec and use the
`hauksbee-ci` hook instead of (or alongside) `hauksbee-check`:

```yaml
repos:
  - repo: https://github.com/hauksbee-dev/hauksbee
    rev: v0.x.y
    hooks:
      - id: hauksbee-ci
```

`hauksbee_ci_precommit.py` reuses the pcbnew-free core from
`../kicad-plugin/hauksbee_ci_core.py`, which is file-type-agnostic: it only ever
handles the spec path and shells out to the `hauksbee-ci` binary, which in turn
loads a `.kicad_sch` by path (resolving the sheet hierarchy) or a `.kicad_pcb`
from content. The hook:

1. lists the files staged for commit,
2. finds every spec under the configured directories (default `ci/` and the repo
   root),
3. runs only the specs whose `board` resolves to a staged file, and
4. exits non-zero (blocking the commit) if any spec is RED.

This is the most natural home for **schematic-stage** CI. KiCad's schematic
editor (eeschema) has no in-editor plugin API yet: the IPC API in KiCad 9 and 10
is implemented for the PCB editor only, and headless operation through
`kicad-cli` is a KiCad 11 feature. So the place to catch a schematic-level fault
before it ever reaches a layout is the commit, not eeschema. (It works for
`.kicad_pcb` boards too, identically.)

Configure which directories are searched for specs with `HAUKSBEE_CI_SPECS`
(colon-separated, default `ci:.`), and point at a binary that is not on `PATH`
with `HAUKSBEE_CI_BIN`.

## Local install (working inside this repo, or without the framework)

The `.pre-commit-config.yaml` in this directory shows the `repo: local` form:
copy its `repos:` entry into your repo-root `.pre-commit-config.yaml`, then:

```bash
pre-commit install
```

Or skip the framework entirely and use a plain git hook:

```bash
ln -s ../../integrations/pre-commit/hauksbee_ci_precommit.py .git/hooks/pre-commit
```

Point the hooks at your built binaries if they are not on `PATH`:

```bash
cargo build --release -p hauksbee-engine -p hauksbee-ci
export HAUKSBEE_BIN="$PWD/target/release/hauksbee"        # hauksbee-check
export HAUKSBEE_CI_BIN="$PWD/target/release/hauksbee-ci"  # hauksbee-ci
```

## A schematic-stage spec

Point a spec's `board` at the schematic root (not a sub-sheet):

```toml
name = "power-up sanity (schematic)"
board = "hardware/myboard.kicad_sch"   # the hierarchy root
duration_ms = 1

[[supply]]
net = "VCC"
kind = "ideal"
volts = 5.0

[[assert]]
kind = "voltage"
net = "VCC"
min = 4.99
max = 5.01

[[assert]]
kind = "no_faults"
```

Commit an edit to `myboard.kicad_sch` and the hook runs this check before the
commit is recorded. See `crates/hauksbee-ci/examples/pic_programmer_schematic.toml`
for a complete, runnable example, and `docs/ci/CI.md` (Schematic-stage CI) for the
agreement guarantee with the layout-stage check.

## Testing without git

The hooks' decision logic is covered by plain-python tests:

```bash
python3 integrations/pre-commit/test_hauksbee_check_precommit.py
python3 integrations/pre-commit/test_hauksbee_ci_precommit.py
python3 integrations/kicad-plugin/test_hauksbee_ci_core.py
```

The last one covers the shared core's discovery and board-detection helpers
(`find_specs`, `spec_board`, `spec_targets_schematic`).
