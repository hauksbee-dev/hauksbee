# galvani-ci KiCad plugin

A pcbnew action plugin that runs `galvani-ci` on the board you have open in
KiCad and shows the pass/fail results in a dialog: does the rail come up, does
the UART say hello, does the LED blink. It is deliberately thin, it shells out
to the `galvani-ci` binary and parses the JUnit XML, so all the simulation
lives in the Rust runner.

## Prerequisites

1. Build the runner and put it on your PATH (or note its path):

   ```bash
   cargo build --release -p galvani-ci
   # binary at target/release/galvani-ci
   ```

   The plugin finds the binary via, in order: an explicit path, the
   `GALVANI_CI_BIN` environment variable, then your `PATH`. The simplest setup
   is to copy or symlink `galvani-ci` somewhere on PATH:

   ```bash
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

- `galvani_ci_core.py` - pcbnew-free logic: find binary, build command, run,
  parse JUnit, format report. Hardened XML parsing (rejects DOCTYPE/entities).
- `galvani_ci_action.py` - the `pcbnew.ActionPlugin` + wx results dialog.
- `__init__.py` - registers the plugin on import.
- `test_galvani_ci_core.py` - core unit tests (no pcbnew/wx needed).
