# Changelog

## 0.3.0

Full editing support for `hauksbee-ci` spec TOML, none of which needs a binary.

- New: **completion** for table headers, for the keys valid inside the table the
  cursor is in, and for every closed vocabulary in the format (assertion kinds,
  supply kinds, USB profiles, battery chemistries, peripheral types, stimulus
  waveforms, DNP modes, distributions, ensemble modes). Required keys sort first;
  each item carries the field's Rust doc comment as its documentation.
- New: **quick fixes** for the diagnostics that already name their own answer:
  rename to a did-you-mean suggestion, switch to any token in a vocabulary, add
  a missing required key (using the loader's own worked value where it gives
  one), add the bound an assertion needs (filtered to the keys its `kind`
  reads), and correct a toggle `tolerance` written as a percentage.
- New: key completions are **snippets**: the cursor lands on the value, and a
  closed vocabulary opens as a choice list. Table headers scaffold their required
  keys in Rust field order.
- New: the completion list is **ordered by the discriminant you wrote**: the
  fields that `kind` / `type` reads first, in the order you fill them in, the
  rest marked as belonging to another kind. Covers `[[assert]]`, `[[supply]]`
  and `[[peripheral]]`.
- New: **cross-reference completion** for `scenario`, `profile`, `id`, `ref` and
  `part`, from the blocks declared in the same spec.
- Loader failures that are about the machine rather than the spec (a build
  without the `avr` / `qemu` / `renode` feature, a missing simulator) are
  reported as Information and say so, instead of blaming the file.
- New: `board = "…"` completes from the board files in your workspace.
- New: **hovers** showing a field's type, numeric bounds, default, vocabulary
  and doc comment. Hovering `kind = "boot-coverage"` says that the canonical
  spelling is `boot_coverage` and that the alias keeps working.
- New: **board-net completion** inside `net = "…"` and its siblings, from
  `hauksbee run <board> --list-nets --json`, cached per board and invalidated by
  mtime. Silent when unavailable.
- New: **linting**, in two layers. Always on and binary-free: unknown keys with
  a did-you-mean, missing required keys, wrong types, values outside a closed
  vocabulary, out-of-range and non-finite numbers, plus the conditional rules
  `Spec::validate` owns (a `bench`/`wall`/`ideal` supply with no `volts`, a
  `usb` leg with no profile, a `battery` with no chemistry, `min > max`, a
  toggle `tolerance` written as a percentage, an assertion scoped to a
  `[[scenario]]` that does not exist, a `vcd_sink` with a singular `net`,
  `[ensemble]` with nothing to sample, an AC assertion with no `[ac]` block).
  On save: the real `hauksbee-ci` loader's own message, positioned in the buffer,
  including TOML parse errors with column and caret width, serde's
  unknown-field rejection, and unknown nets with their suggestions.
- New: `Hauksbee: Validate CI Spec (loader)` command, and a quiet `✓ spec ok`
  status-bar item for a spec that loads clean.
- New settings: `hauksbee.spec.loaderCheck`, `hauksbee.spec.loaderTimeoutMs`.
- Workspace trust: everything that runs a binary (the loader lint layer, net
  listing, and both run commands) now requires a trusted workspace, and
  `hauksbee.path` / `hauksbee.ciPath` are machine-scoped so a checked-in
  `.vscode/settings.json` cannot choose the executable. Completion, hovers and
  the schema lint keep working untrusted, since they only read text.
- Binary discovery now matches the pre-commit hook and the KiCad plugin:
  setting, then `HAUKSBEE_BIN` / `HAUKSBEE_CI_BIN`, then PATH, then a local
  `target/release` (then `target/debug`) build.
- Spec files are detected by content, confirmed against the parse tree, so a
  `Cargo.toml` and a document that merely QUOTES a spec inside a multi-line
  string are both left alone. The Even Better TOML schema association stays
  narrow (`hauksbee*.toml`, `*.hauksbee.toml`) because it can only match on the
  filename, and a blanket `ci/*.toml` rule would hand somebody's
  `ci/renovate.toml` to a third-party validator.
- The spec schema is now **generated** from the Rust `Spec` type rather than
  hand-written, and the `boot_coverage` enum accepts both the canonical and the
  legacy `boot-coverage` spelling. Doc comments and numeric bounds were filled
  in across `SupplySpec`, `NetDrive`, `Override`, `Assertion`,
  `TimelineEventSpec`, `InlineProfile`, `InlineSegment` and `CapOverride`, so
  the hover text and range checks cover the whole format.
- The TOML reader also rejects what `toml-rs` rejects, so a file the loader
  cannot parse never reads as clean: duplicate keys and tables, redefinitions,
  leading zeros, misplaced underscores, date-times, raw control characters,
  out-of-range escapes. A UTF-8 BOM and CRLF are accepted, as they should be.
- Tests: 169 unit tests (no VS Code, no binaries) plus a 10-test headless VS Code
  suite (`bun run test:e2e`) that reads the diagnostics, completions, hovers and
  applied quick fixes VS Code actually produces. Two of them are structural: a
  loader-parity sweep that generates a violating value for every bounded numeric
  field and every closed vocabulary in the schema (under all fourteen assertion
  kinds) and requires the extension's error-or-not verdict to match the real
  binary; and a repository-wide detection sweep over every checked-in `.toml`.

## 0.2.0

- New: `Hauksbee: Run CI Spec`: shells out to `hauksbee-ci run --junit`,
  maps failed/INVALID assertions to Error diagnostics on their `[[assert]]`
  blocks (positional mapping; file-level fallback when counts disagree).
- New: `Hauksbee: Check Board File`: shells out to
  `hauksbee run <board> --check --json`, maps lint/SI/USB-C findings and DRC
  shorts/clearance groups to diagnostics (ref/net line location on `.board`
  files).
- New: status-bar pass/fail + finding count for the most recent run; click to
  re-run (`Hauksbee: Re-run Last Check`).
- New: JSON Schema for hauksbee-ci spec TOML files, contributed to Even
  Better TOML via `tomlValidation` (hand-written from `spec.rs`; validated
  against all bundled example specs).
- New settings: `hauksbee.path`, `hauksbee.ciPath` (binary locations; PATH
  fallback with a clear install-pointer error when absent).
- Unit tests for the CLI-output to diagnostic mapping against real captured
  CLI output (`bun test`).

## 0.1.0

- Syntax highlighting, language configuration, and folding for `.board`
  Board-as-Code files.
