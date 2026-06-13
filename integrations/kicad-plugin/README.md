# galvani-ci KiCad plugin

A pcbnew action plugin that runs `galvani-ci` on the board you have open in
KiCad and shows the pass/fail results in a dialog: does the rail come up, does
the UART say hello, does the LED blink. It is deliberately thin, it shells out
to the `galvani-ci` binary and parses the JUnit XML, so all the simulation
lives in the Rust runner.

## Which editor?

This is a **pcbnew** (PCB editor) plugin. KiCad's schematic editor (eeschema)
has no equivalent plugin API yet: the new IPC plugin API in KiCad 9 and 10 is
implemented for the PCB editor only, schematic-editor support is future work,
and headless operation via `kicad-cli` arrives in KiCad 11. We do not ship a
fake eeschema button.

For **schematic-stage** CI (a spec whose `board` is a `.kicad_sch`), use the
pre-commit hook in `../pre-commit` or the `galvani-ci` CLI; those are the natural
gates for a schematic-level check. You can also run a schematic-stage spec from
*this* pcbnew plugin while a project's PCB is open, because spec discovery offers
every `*.toml` next to the board (and in a sibling `ci/`), including ones that
target the project's schematic. The shared core (`galvani_ci_core.py`) is
file-type-agnostic, so when eeschema gains an API the entry point drops in beside
this one. See `docs/CI.md` (Schematic-stage CI).

## Prerequisites

You need a `galvani-ci` binary. The plugin finds one without any setup in most
cases, and only offers to compile as a last resort.

It looks for the binary in this order:

1. an explicit path passed in code,
2. the `GALVANI_CI_BIN` environment variable,
3. your `PATH`,
4. a prebuilt release bundle (`bin/galvani-ci` next to a `galvani` checkout, or
   `~/.galvani/bin/galvani-ci`) or a local `target/release/galvani-ci`.

So the easiest path is to **download a prebuilt release** (see the repo's
Releases, produced by `.github/workflows/release.yml`) and unpack it, or run
`scripts/install.sh` once. If none of the above is found, the plugin asks
whether to build it with cargo (the explicit, opt-in fallback):

```bash
# Either: a prebuilt release tarball (no compiler needed)
tar -xzf galvani-<version>-<os>-<arch>.tar.gz
PREFIX=$HOME/.local ./galvani-*/scripts/install.sh --no-build --symlink

# Or: build from source once
cargo build --release -p galvani-ci   # binary at target/release/galvani-ci
ln -s "$PWD/target/release/galvani-ci" /usr/local/bin/galvani-ci
```

2. Keep at least one galvani-ci spec next to your board, or in a sibling `ci/`
   directory (e.g. `ci/power-up.toml`).

## Install

KiCad loads action plugins from a per-user plugins directory. Find yours via
KiCad's menu: **Tools -> External Plugins -> Open Plugin Directory**, or use the
default for your OS:

- Linux: `~/.local/share/kicad/<version>/scripting/plugins/`
- macOS: `~/Documents/KiCad/<version>/scripting/plugins/`
- Windows: `%USERPROFILE%\Documents\KiCad\<version>\scripting\plugins\`

Copy (or symlink) this whole directory in as a package named `galvani_ci`:

```bash
PLUGINS=~/.local/share/kicad/8.0/scripting/plugins   # adjust for your OS/version
ln -s "$PWD/integrations/kicad-plugin" "$PLUGINS/galvani_ci"
```

Then in the PCB editor: **Tools -> External Plugins -> Refresh Plugins**. A
"galvani-ci: run hardware check" entry appears (and a toolbar button).

## Use

1. Open and save your board in the PCB editor.
2. Click **galvani-ci: run hardware check** (or pick it from External Plugins).
3. If more than one spec is found, choose one. The plugin runs galvani-ci from
   the board's directory (so relative spec paths resolve) and shows a dialog
   with the verdict and each assertion's pass/fail detail.

## Testing without KiCad

The pcbnew/wx wrapper (`galvani_ci_action.py`) is thin; all the logic lives in
`galvani_ci_core.py`, which imports neither pcbnew nor wx. Run its tests with
plain python:

```bash
python3 integrations/kicad-plugin/test_galvani_ci_core.py
# or: python3 -m pytest integrations/kicad-plugin/test_galvani_ci_core.py
```

To smoke-test the full shell-out + JUnit-parse path against the real binary:

```bash
python3 - <<'PY'
import sys, os
sys.path.insert(0, "integrations/kicad-plugin")
import galvani_ci_core as core
run = core.run_ci(
    os.path.abspath("crates/galvani-ci/examples/tarski_brownout.toml"),
    binary=os.path.abspath("target/release/galvani-ci"),
)
print(core.format_report(run))
PY
```

## Manual test inside KiCad (cannot be automated here)

1. Open `board-corpus/stormduino/stormduino Rev2.kicad_pcb` (or your board).
2. Put a spec at `ci/power-up.toml` next to it (see
   `crates/galvani-ci/examples/blinky.toml` for the format).
3. Run the plugin; confirm the dialog shows the assertions and a GREEN/RED
   headline. Break the spec's voltage threshold and confirm it turns RED.

## Files

- `galvani_ci_core.py` - pcbnew-free, file-type-agnostic logic: find binary,
  discover specs (`find_specs`), read a spec's board / detect schematic-stage
  (`spec_board`, `spec_targets_schematic`), build command, run, parse JUnit,
  format report. Hardened XML parsing (rejects DOCTYPE/entities). Shared with
  the pre-commit hook in `../pre-commit`.
- `galvani_ci_action.py` - the `pcbnew.ActionPlugin` + wx results dialog.
- `__init__.py` - registers the plugin on import.
- `test_galvani_ci_core.py` - core unit tests (no pcbnew/wx needed).
