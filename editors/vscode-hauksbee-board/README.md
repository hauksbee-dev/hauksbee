# Hauksbee — VS Code Extension

Hauksbee hardware checks inside VS Code. A **thin shell-out client** (no
language server — LSP for the `.board` DSL is deferred until the DSL
stabilises): it runs the `hauksbee` / `hauksbee-ci` binaries you already have
and renders their results as native diagnostics.

## Features

- **Syntax highlighting** for hauksbee Board-as-Code `.board` files
  (comments, `fn`/`instance` blocks, `comp`/`slot`/`pad`/`net` statements,
  layer lists, placement constraints).
- **`Hauksbee: Run CI Spec`** — runs `hauksbee-ci run <spec> --junit …` on the
  current spec TOML (or a picker) and maps every failed assertion to an Error
  diagnostic **on its `[[assert]]` block** in the spec.
- **`Hauksbee: Check Board File`** — runs
  `hauksbee run <board> --check --json` on the current `.board` / `.kicad_pcb`
  / `.kicad_sch` / `.net` / `.brd` / `.d356` file and maps the findings
  (lint + signal integrity + USB-C + DRC) to diagnostics. On `.board` files,
  findings are located at the offending `comp <ref>` / `net "…"` line.
- **Status bar** — pass/fail and finding count for the most recent run; click
  it (or run `Hauksbee: Re-run Last Check`) to re-run.
- **Spec TOML schema** — a JSON Schema for hauksbee-ci spec files
  (`schemas/hauksbee-ci-spec.schema.json`), wired up via the
  [Even Better TOML](https://marketplace.visualstudio.com/items?itemName=tamasfe.even-better-toml)
  extension for completion + validation while you edit a spec.

## Requirements

The binaries are **not** bundled. Build them from the
[hauksbee](https://github.com/hauksbee-dev/hauksbee) repo:

```sh
cargo build --release -p hauksbee-engine -p hauksbee-ci
```

then either put `hauksbee` and `hauksbee-ci` on your PATH, or point the
settings at them:

| Setting           | Meaning                                    |
| ----------------- | ------------------------------------------ |
| `hauksbee.path`   | Path to the `hauksbee` engine binary       |
| `hauksbee.ciPath` | Path to the `hauksbee-ci` spec runner      |

If a binary is missing you get one clear error notification with this pointer.

## How results are mapped (the contract)

Machine formats consumed (as of hauksbee-ci 0.1 / hauksbee engine 0.1):

- `hauksbee run --check --json` emits a structured report with a `findings`
  array and a `drc` section. **`hauksbee-ci` has no `--json` flag** — its
  stable machine format is JUnit XML (`--junit`), which this extension writes
  to a temp file and parses. (`hauksbee check-code` has no `--json` either;
  the board command uses `run --check --json`, which accepts `.board` files.)

Severity mapping:

| Source finding                            | VS Code severity |
| ----------------------------------------- | ---------------- |
| engine `severity: "serious"`              | Error            |
| engine `severity: "warning"` / `"medium"` | Warning          |
| engine `severity: "note"` / `"info"`      | Information      |
| DRC copper short                          | Error (Information when the report carries a `version_warning`: unvalidated board format, shorts may be phantom) |
| DRC clearance-below-rule group            | Warning          |
| DRC at-limit group                        | Information      |
| hauksbee-ci assertion FAIL (`<failure>`)  | Error            |
| hauksbee-ci assertion INVALID (`<error>`, analog co-sim did not converge) | Error (it gates CI with exit 3; anything softer would misrepresent the gate) |
| hauksbee-ci spec/usage error (exit 2)     | Error at file level (stderr text) |

Line numbers: the CLI output carries **no source line numbers**, so the
extension reconstructs them honestly:

- Spec TOML: assertions are evaluated and reported **in spec order**, so
  testcase *N* is attached to the *N*-th `[[assert]]` block. If the block
  count and testcase count disagree (stale buffer), diagnostics fall back to
  file level rather than guess.
- `.board`: the first `comp <ref>` line for the finding's first ref, else the
  `net "…"` declaration for its first net, else file level.
- Other board formats (`.kicad_pcb` etc.): file level.

Diagnostics from the previous run are replaced on every re-run.

## Spec TOML schema

`schemas/hauksbee-ci-spec.schema.json` is **hand-written** from
`crates/hauksbee-ci/src/spec.rs` and `scenarios.rs` (schemars is not a
workspace dependency, so there is no derived schema); it mirrors serde's
`deny_unknown_fields` with `additionalProperties: false` and has been
validated against all bundled example specs plus the `hauksbee-ci init`
scaffold. If you change `spec.rs`, update the schema.

The association is contributed via Even Better TOML's `tomlValidation`
contribution point for files matching `hauksbee*.toml` / `*.hauksbee.toml`.
For a spec with any other name, add one of:

```toml
#:schema ./path/to/hauksbee-ci-spec.schema.json
```

at the top of the file, or in your settings:

```jsonc
"evenBetterToml.schema.associations": {
  "ci/.*\\.toml$": "<path to>/schemas/hauksbee-ci-spec.schema.json"
}
```

## Install

### From a .vsix (current route — not yet on the marketplace)

```sh
cd editors/vscode-hauksbee-board
bun install
bun run build
bunx @vscode/vsce package
code --install-extension hauksbee-board-0.2.0.vsix
```

### Development / symlink (grammar-only live edits)

```sh
ln -s "$(pwd)/editors/vscode-hauksbee-board" \
    ~/.vscode/extensions/hauksbee-dev.hauksbee-board-dev
```

## Publishing to the marketplace (maintainers)

Publishing needs a human-owned account; it is deliberately not automated yet.

1. Create (once) an Azure DevOps organisation and a
   [marketplace publisher](https://marketplace.visualstudio.com/manage) named
   `hauksbee-dev` (must match `publisher` in package.json).
2. Create a Personal Access Token with the **Marketplace → Manage** scope.
3. `bunx @vscode/vsce login hauksbee-dev` (paste the PAT).
4. `bun run build && bunx @vscode/vsce publish` (or
   `bunx @vscode/vsce publish --packagePath hauksbee-board-0.2.0.vsix`).
5. Tag the release; later, wire this into the release-automation CI job
   (dev-plan 07 §6) with the PAT as a repo secret.

## Manual verification checklist

Automated coverage: the CLI-output → diagnostic mapping is unit-tested against
real captured CLI output (`bun test`, `test/fixtures/`). The following need a
human with VS Code (no `code` CLI was available where this was packaged):

- [ ] Install the .vsix; open `crates/hauksbee-ci/examples/boot_gate_fail.toml`;
      run **Hauksbee: Run CI Spec**; expect one Error on the `[[assert]]`
      block (line 25) and a red `0/1 assertions passed - RED` status-bar item.
- [ ] Open `boot_gate_pass.toml`; run the same; expect no diagnostics and a
      green `2/2 assertions passed - GREEN` status bar.
- [ ] Open `examples/board-as-code/stormduino.board`; run
      **Hauksbee: Check Board File**; expect 11 findings in Problems, the
      `R9 is a R designator…` warning on the `comp R9` line (line 92).
- [ ] Click the status-bar item; the last check re-runs and diagnostics
      refresh (edit the file first to see them clear/reappear).
- [ ] With Even Better TOML installed, open a `hauksbee*.toml` spec; typing an
      unknown top-level key or a bad `kind` is flagged; hovering fields shows
      the schema descriptions.
- [ ] Clear `hauksbee.path`/`hauksbee.ciPath` with the binaries off PATH; both
      commands produce the install-pointer error notification.

## Board DSL keyword inventory (grammar reference, confirmed from the parser)

Top-level structural: `board`, `fn`, `instance`, `comp`, `slot`, `net`, `pad`,
`space`, `pin`, `lock`. Board sub-directives: `version`, `outline`, `size`.
Inline property keys: `lib`, `val`, `pads`, `layer`, `at`, `rot`, `size`,
`drill`, `layers`, `nonet`, `edge`. Pad kinds: `smd`, `thru_hole`,
`np_thru_hole`, `connect`. Pad shapes: `rect`, `roundrect`, `oval`, `circle`,
`trapezoid`, `custom`.
