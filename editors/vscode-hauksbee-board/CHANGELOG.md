# Changelog

## 0.2.0

- New: `Hauksbee: Run CI Spec` — shells out to `hauksbee-ci run --junit`,
  maps failed/INVALID assertions to Error diagnostics on their `[[assert]]`
  blocks (positional mapping; file-level fallback when counts disagree).
- New: `Hauksbee: Check Board File` — shells out to
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
- Unit tests for the CLI-output → diagnostic mapping against real captured
  CLI output (`bun test`).

## 0.1.0

- Syntax highlighting, language configuration, and folding for `.board`
  Board-as-Code files.
