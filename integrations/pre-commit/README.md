# galvani-ci pre-commit hook (schematic-stage and layout-stage)

Run galvani-ci hardware checks before a commit lands. When a staged file is a
board that a checked-in spec targets, the matching check runs; if any assertion
is RED, the commit is blocked.

This is the most natural home for **schematic-stage** CI. KiCad's schematic
editor (eeschema) has no in-editor plugin API yet: the IPC API in KiCad 9 and 10
is implemented for the PCB editor only, and headless operation through
`kicad-cli` is a KiCad 11 feature. So the place to catch a schematic-level fault
before it ever reaches a layout is the commit, not eeschema. (It works for
`.kicad_pcb` boards too, identically.)

## How it works

`galvani_ci_precommit.py` reuses the pcbnew-free core from
`../kicad-plugin/galvani_ci_core.py`, which is file-type-agnostic: it only ever
handles the spec path and shells out to the `galvani-ci` binary, which in turn
loads a `.kicad_sch` by path (resolving the sheet hierarchy) or a `.kicad_pcb`
from content. The hook:

1. lists the files staged for commit,
2. finds every spec under the configured directories (default `ci/` and the repo
   root),
3. runs only the specs whose `board` resolves to a staged file, and
4. exits non-zero (blocking the commit) if any spec is RED.

## Install

With the [pre-commit](https://pre-commit.com) framework: copy the `repos:` entry
from `.pre-commit-config.yaml` here into your repo-root `.pre-commit-config.yaml`,
then:

```bash
pre-commit install
```

Or as a plain git hook:

```bash
ln -s ../../integrations/pre-commit/galvani_ci_precommit.py .git/hooks/pre-commit
```

Point the hook at your built binary if it is not on `PATH`:

```bash
cargo build --release -p galvani-ci
export GALVANI_CI_BIN="$PWD/target/release/galvani-ci"
```

Configure which directories are searched for specs with `GALVANI_CI_SPECS`
(colon-separated, default `ci:.`).

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
commit is recorded. See `crates/galvani-ci/examples/pic_programmer_schematic.toml`
for a complete, runnable example, and `docs/CI.md` (Schematic-stage CI) for the
agreement guarantee with the layout-stage check.

## Testing without git

The hook's logic lives in the shared core; its discovery and board-detection
helpers (`find_specs`, `spec_board`, `spec_targets_schematic`) are covered by
`../kicad-plugin/test_galvani_ci_core.py`. Run them with plain python:

```bash
python3 integrations/kicad-plugin/test_galvani_ci_core.py
```
