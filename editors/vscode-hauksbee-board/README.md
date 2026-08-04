# Hauksbee VS Code Extension

Hauksbee hardware checks inside VS Code. Writing a `hauksbee-ci` spec should
feel like writing code with a compiler watching, not like editing a config file
and finding out in CI. So there are two halves:

- **Spec TOML editing**: completion, hovers and linting, all generated from
  the Rust `Spec` type. Works with **no binaries installed**.
- **Running checks**: a **thin shell-out client** (no language server; LSP for
  the `.board` DSL is deferred until the DSL stabilises) that runs the
  `hauksbee` / `hauksbee-ci` binaries you already have and renders their
  results as native diagnostics.

## Editing a hauksbee-ci spec

Open any spec TOML and you get:

- **Completion** that finishes the job rather than the word:
  - **Table headers** (`[[assert]]`, `[[supply]]`, `[ac]`,
    `[[peripheral.event]]`, …) come with their **required keys scaffolded** in
    Rust field order, so choosing `[[supply]]` leaves you a block to fill in
    rather than a header and three field names to remember.
  - **Keys** are snippets: the cursor lands on the value, and a closed
    vocabulary opens as a **choice list**, so `kind` in a `[[supply]]` becomes a
    pick from `ideal | bench | wall | usb | battery` instead of a token to spell
    from memory. Required keys sort first.
  - **The list is ordered by the discriminant you wrote.** An `[[assert]]` has
    thirty-odd fields and any one `kind` reads about five: those five come first,
    in the order you fill them in, and the rest are marked "not read by a
    `voltage` assertion". Same for a `[[supply]]`'s `kind` (a `usb` leg leads
    with `usb`, a `battery` with `chemistry`) and a `[[peripheral]]`'s `type`. The
    discriminant is found from below the cursor too, since people often write it
    last.
  - **Cross-references** complete from the document: an assertion's `scenario`
    from the `[[scenario]]` ids you declared, its `id` from your `[[peripheral]]`
    and `[[sensor]]` blocks, a scenario's `profile` from your inline
    `[[profile]]` blocks. The lint layer computes those same sets to reject a bad
    one, so offering them costs nothing.
  - **Every closed vocabulary** in the format: assertion kinds, supply kinds,
    USB profiles, battery chemistries, peripheral types, stimulus waveforms, DNP
    modes, sampling distributions, ensemble modes.
  - **`board = "…"`** offers the board files actually in your workspace,
    nearest first, as paths relative to the spec.
- **Hovers** carrying each field's Rust doc comment, its type, its numeric
  bounds and its default. `tolerance` tells you it is a fraction in (0, 1];
  `kind = "bench"` tells you it needs an explicit `volts`.
- **Board-net completion** inside `net = "…"`, `supply_net`, `cs_net`, `nets`
  and friends, from the actual board your spec points at (via
  `hauksbee run <board> --list-nets`, cached and invalidated by mtime). Fails
  silently when the engine binary or the board is not available.
- **Linting**, in two layers:
  0. Anything the loader reports that is about the **machine** rather than the
     spec (a hauksbee built without the `avr` feature, a simulator that is not
     installed) is Information, and says so. No edit to the file would fix it.
  1. **Always on, no binary needed.** Unknown keys (with a did-you-mean),
     missing required keys, wrong types, values outside a closed vocabulary,
     out-of-range numbers, non-finite numbers, and the conditional rules the
     loader owns: a `bench`/`wall`/`ideal` supply with no `volts`, a `usb` leg
     with no profile, a `battery` with no chemistry, `min > max`, a toggle
     `tolerance` written as a percentage, an assertion scoped to a
     `[[scenario]]` that does not exist, a `vcd_sink` with a singular `net`,
     `[ensemble]` with nothing to sample, an AC assertion with no `[ac]` block.
  2. **The real loader, on save.** `hauksbee-ci`'s own error text, positioned in
     your buffer: the exact message CI will print, on the exact line. This layer
     runs only when layer 1 is already clean, only in a trusted workspace, and
     only if the binary is found.

- **Quick fixes**, because every one of those messages already names its own
  answer. `did you mean 'duration_ms'?` becomes **Change to `duration_ms`**; a
  bad `kind` offers the correction first and then the whole vocabulary; a `bench`
  supply with no `volts` offers **Add `volts = 3.3`**, the loader's own worked
  example, inserted in the right block; a `voltage` assertion with no bound
  offers `min` and `max` and not the twenty keys a voltage assertion ignores;
  `tolerance = 25` offers **Change to 0.25**. Nothing is offered when the message
  names no answer, because a fix that guesses a number you then have to check is
  worse than no fix.

  Layer 1 distinguishes what CI will reject from what it merely dislikes. The
  schema documents some constraints that the loader does not check at load time:
  a peripheral's I2C `address`, an EEPROM `size`, a scenario's `start_ms`, a
  capacitor override's ESR/ESL, and `peripheral.waveform` (which the *run*
  rejects, later). Several more are checked only under one assertion kind: a
  `tolerance` outside (0, 1] is an error on a `toggle` and ignored elsewhere,
  the `model_coverage` fractions likewise. Those are **warnings**, because calling them errors would
  claim a build will fail at load when it will not. Everything reported as an
  **error** is something the loader itself rejects.

  `src/spec/parity.test.ts` holds that line mechanically rather than by hand: it
  walks the schema, generates a violating value for every bounded numeric field,
  a bogus token for every closed vocabulary, and the values the Rust integer type
  decides rather than the bound, then runs the real binary on each one and
  requires the extension's error-or-not verdict to match. Assertion fields are
  swept under all fourteen assertion kinds, because several of Rust's checks live
  inside a single `match kind` arm.

A clean spec shows a quiet `✓ spec ok` in the status bar. Its tooltip says which
layers actually ran: "schema lint clean" when only layer 1 has, "this spec loads
clean" once the binary has confirmed it. Click it (or run **Hauksbee: Validate CI
Spec**) to check against the binary on demand.

Files are detected by **content**, not by name: a `.toml` with both a `board =`
key and an `[[assert]]` table is a spec, wherever it lives. Your `Cargo.toml`
is left completely alone. A file named `hauksbee*.toml` / `*.hauksbee.toml`, or
any `.toml` under a `ci/` directory, gets completion and hovers while it is
still being written but is not linted until it looks like a spec.

### Even Better TOML

This extension does not ship a TOML grammar, so install
[Even Better TOML](https://marketplace.visualstudio.com/items?itemName=tamasfe.even-better-toml)
for syntax highlighting and formatting. The two compose rather than compete:

- The completion, hover and lint above are contributed on `**/*.toml` directly,
  so they work whether or not Even Better TOML is installed.
- `schemas/hauksbee-ci-spec.schema.json` is *also* registered through Even
  Better TOML's `tomlValidation` contribution point, which adds taplo's own
  schema validation on top. That binding can only match on the filename, so it
  is deliberately narrow: `hauksbee*.toml` and `*.hauksbee.toml` only. A blanket
  `ci/*.toml` rule would hand somebody's `ci/renovate.toml` to a third-party
  extension and produce a wall of errors that looks like ours. For a spec with
  any other name, add a schema directive at the top of the file:

  ```toml
  #:schema ./path/to/hauksbee-ci-spec.schema.json
  ```

  or an association in your settings:

  ```jsonc
  "evenBetterToml.schema.associations": {
    "hardware/.*\\.toml$": "<path to>/schemas/hauksbee-ci-spec.schema.json"
  }
  ```

## Running checks

- **`Hauksbee: Run CI Spec`**: runs `hauksbee-ci run <spec> --junit …` on the
  current spec TOML (or a picker) and maps every failed assertion to an Error
  diagnostic **on its `[[assert]]` block** in the spec.
- **`Hauksbee: Check Board File`**: runs
  `hauksbee run <board> --check --json` on the current `.board` / `.kicad_pcb`
  / `.kicad_sch` / `.net` / `.brd` / `.d356` file and maps the findings
  (lint + signal integrity + USB-C + DRC) to diagnostics. On `.board` files,
  findings are located at the offending `comp <ref>` / `net "…"` line.
- **`Hauksbee: Validate CI Spec`**: forces the loader lint layer on the current
  spec, whatever `hauksbee.spec.loaderCheck` says.
- **Syntax highlighting** for hauksbee Board-as-Code `.board` files
  (comments, `fn`/`instance` blocks, `comp`/`slot`/`pad`/`net` statements,
  layer lists, placement constraints).
- **Status bar**: pass/fail and finding count for the most recent run; click
  it (or run `Hauksbee: Re-run Last Check`) to re-run.

## Requirements

The binaries are **not** bundled, and the editing features above do not need
them. Build them from the
[hauksbee](https://github.com/hauksbee-dev/hauksbee) repo:

```sh
cargo build --release -p hauksbee-engine -p hauksbee-ci
```

### How a binary is found

Same order as the documented pre-commit hook and the KiCad plugin, so a machine
set up for one integration is set up for all of them:

1. the explicit setting: `hauksbee.path` / `hauksbee.ciPath`
2. the environment: `HAUKSBEE_BIN` / `HAUKSBEE_CI_BIN`
3. `PATH`
4. a local cargo build: `<workspace>/target/release/<name>`, then
   `target/debug/<name>`

Step 4 means the extension works inside the hauksbee repo itself, and for
anyone who ran `cargo build --release` once without touching PATH. Note the
consequence of step 3 preceding step 4: an older `hauksbee-ci` installed on
PATH wins over a fresh local build, which is one reason the always-on lint
layer does not delegate everything to the binary.

| Setting                            | Meaning                                                     |
| ---------------------------------- | ----------------------------------------------------------- |
| `hauksbee.path`                    | Path to the `hauksbee` engine binary                        |
| `hauksbee.ciPath`                  | Path to the `hauksbee-ci` spec runner                       |
| `hauksbee.spec.loaderCheck`        | `onSave` (default) / `onCommand` / `off`                     |
| `hauksbee.spec.loaderTimeoutMs`    | How long to give the loader before assuming the spec loaded |

Note what `onSave` costs. `hauksbee-ci run` has no load-only mode, so a spec
that loads cleanly goes on into a co-simulation, and one that finishes inside
the timeout has genuinely run, artifacts and all. Specs that declare an output
(`vcd_path`) are therefore excluded from the automatic layer, and checked only
by the explicit command. A `hauksbee-ci check <spec>` subcommand upstream would
remove the whole heuristic.

If a binary is missing, the commands produce one clear error notification with
this pointer; the editing features degrade quietly rather than nagging.

### Workspace trust

Completion, hovers and the schema lint are pure text analysis and work in an
untrusted workspace. Anything that **runs** a binary does not: the loader lint
layer, board-net listing, and both run commands stay off until the workspace is
trusted. `hauksbee.path` and `hauksbee.ciPath` are machine-scoped for the same
reason, so a checked-in `.vscode/settings.json` cannot decide which executable
gets run on your machine.

Note what trusting a workspace grants, though: discovery step 4 above runs
`<workspace>/target/release/hauksbee-ci` if it finds one, so in a trusted
workspace the binary executed on open and save can come from the repository
tree itself. That is the usual trust bargain, and it is what makes the extension
work in a hauksbee checkout with no setup, but it is worth knowing before
trusting a repository you did not write.

## How results are mapped (the contract)

Machine formats consumed (as of hauksbee-ci 0.1 / hauksbee engine 0.1):

- `hauksbee run --check --json` emits a structured report with a `findings`
  array and a `drc` section. (`hauksbee check-code` has no `--json`; the board
  command uses `run --check --json`, which accepts `.board` files.)
- `hauksbee-ci run --junit <out.xml>` writes JUnit XML with one `<testcase>` per
  assertion, in spec order. That is the format the **Run CI Spec** command
  consumes, because it is the one that reports per-assertion outcomes.
  `hauksbee-ci run --json` also exists (one JSON object per spec, NDJSON for a
  multi-spec run); the spec lint layer runs the same binary and reads its
  **stderr**, which is where a spec/board rejection lands.
- `hauksbee run <board> --list-nets --json` emits a JSON array of net names.
  That feeds board-net completion.

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

`schemas/hauksbee-ci-spec.schema.json` is **generated** from the Rust `Spec`
type in `crates/hauksbee-ci/src/spec.rs` (plus `scenarios.rs`) via schemars.
The serde derives and the doc comments *are* the schema: `deny_unknown_fields`
becomes `additionalProperties: false`, each field's doc comment becomes its
`description` and therefore its hover text, and the `#[schemars(extend("enum" =
[…]))]` lists mirror exactly what the loader's `validate` accepts.

Do not hand-edit it. After changing `spec.rs`, regenerate:

```sh
UPDATE_SPEC_SCHEMA=1 cargo test -p hauksbee-ci --test schema_drift
```

`crates/hauksbee-ci/tests/schema_drift.rs` fails the build when the checked-in
file and the types disagree, so the editor experience cannot silently drift from
the format.

Everything in the extension reads that one file: completion vocabularies, hover
text, and the structural half of the lint. A new field on `Spec` becomes a new
completion with documentation the moment the schema is regenerated, with no
TypeScript change.

## Install

### From a .vsix (current route: not yet on the marketplace)

```sh
cd editors/vscode-hauksbee-board
bun install
bun run build
bunx @vscode/vsce package
code --install-extension hauksbee-board-0.3.0.vsix
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
2. Create a Personal Access Token with the **Marketplace / Manage** scope.
3. `bunx @vscode/vsce login hauksbee-dev` (paste the PAT).
4. `bun run build && bunx @vscode/vsce publish` (or
   `bunx @vscode/vsce publish --packagePath hauksbee-board-0.3.0.vsix`).
5. Tag the release; later, wire this into the release-automation CI job
   (dev-plan 07 §6) with the PAT as a repo secret.

## Tests

```sh
bun test src/        # unit: 100+ tests, no VS Code, no binaries required
bun run test:e2e     # headless VS Code, downloads one into .vscode-test/
```

`bun test src/` covers the pure layers against real captured artefacts:

- the position-aware TOML reader (values *and* spans)
- the spec lint, checked both ways: every bad fixture flagged with the right
  message on the right line, and every spec under
  `crates/hauksbee-ci/examples/` reported clean
- completion and hover output for keys, values, tables and board nets
- `hauksbee-ci` stderr fixtures mapped back to ranges, captured from the binary
- binary discovery order
- the CLI-output to diagnostic mapping for the run commands (`test/fixtures/`)
- **loader parity** (`src/spec/parity.test.ts`): the cross-field lint layer is a
  second implementation of `Spec::validate`, so every fixture the extension
  rejects is also fed to the real `hauksbee-ci` binary, which must reject it too
  (exit 2), and the valid fixture must survive. If the loader's rules move, this
  test fails instead of the extension quietly lying. It skips when no
  `target/{release,debug}/hauksbee-ci` exists.

`bun run test:e2e` proves the wiring inside a real VS Code: providers registered
on plain `.toml` documents, diagnostics reaching the Problems panel with the
right codes, messages, severities and lines, a diagnostic clearing as you edit,
`Cargo.toml` left untouched, completion and hover results as a user would see
them, board nets fetched from the engine, and a genuine loader shell-out (a
structurally perfect spec whose board is missing, which only the binary can
detect).

Still worth a human eye, since no automated check covers appearance:

- [ ] the `✓ spec ok` status-bar item and its red counterpart
- [ ] completion and hover *rendering* (markdown, sort order, icons)
- [ ] the run commands' notifications with the binaries off PATH

## Board DSL keyword inventory (grammar reference, confirmed from the parser)

Top-level structural: `board`, `fn`, `instance`, `comp`, `slot`, `net`, `pad`,
`space`, `pin`, `lock`. Board sub-directives: `version`, `outline`, `size`.
Inline property keys: `lib`, `val`, `pads`, `layer`, `at`, `rot`, `size`,
`drill`, `layers`, `nonet`, `edge`. Pad kinds: `smd`, `thru_hole`,
`np_thru_hole`, `connect`. Pad shapes: `rect`, `roundrect`, `oval`, `circle`,
`trapezoid`, `custom`.
