# CLAUDE.md

Read [`AGENTS.md`](AGENTS.md) first: it is the canonical guide for driving
hauksbee as an agent (commands, JSON shapes, exit codes, the spec contract).
This file adds only what is specific to working ON this repository.

## Build and test

```bash
scripts/install.sh                        # frontend + both binaries onto PATH
cargo build --workspace                   # engine, ci, solve, ir, extract, models, mcu, server
cargo test -p hauksbee-engine --lib       # fastest broad signal (~400 tests)
cargo test -p hauksbee-ci                 # spec/assertion/runner suites
cd frontend && bun run build              # the web app (bun, not npm)
```

- `frontend/dist` is a gitignored build artifact; `hauksbee serve` serves it
  from the checkout. Rebuild it after touching `frontend/src` or you will be
  served stale behavior.
- The ngspice cross-check and corpus suites skip when their oracle/corpus is
  absent; `HAUKSBEE_REQUIRE_NGSPICE=1` / `HAUKSBEE_REQUIRE_CORPUS=1` turn a
  skip into a failure.

## Repository gates

- Every non-test module needs a row in `docs/teach/inventory.toml`; the
  doc-coverage test (`cargo test -p hauksbee-ci --test doc_coverage`) fails
  the build otherwise. Classify new modules deliberately (P0/P1 need a
  substantive `//!` header).
- The SPICE compatibility statement is drift-tested: after changing supported
  or refused cards, regenerate with
  `UPDATE_COMPAT=1 cargo test -p hauksbee-ir --test compat_drift`.
- Honesty is a hard invariant: never let a model field, coverage hole, or
  substitution be silently dropped. If a result cannot be vouched for, refuse
  loudly (exit 3 pattern) rather than emitting a plausible number.
- Bit-identity discipline: default-valued new model fields must leave every
  existing deck's solver output byte-identical.

## Layout in one breath

`crates/hauksbee-extract` (board files to netlist) -> `hauksbee-models`
(device models) -> `hauksbee-ir` (circuit IR + SPICE loader) ->
`hauksbee-solve` (MNA + Newton solver) -> `hauksbee-mcu` (emulator backends)
-> `hauksbee-engine` (binds it all; the `hauksbee` binary) with
`hauksbee-ci` (the spec runner) and `hauksbee-server` (HTTP/WS) beside it.
`frontend/` is the React web app. `docs/START_HERE.md` maps the docs.

This is the DEVELOPMENT repo (full docs, evidence trail, dev plans). The
public mirror ships a curated subset; do not assume dev-only trees
(`docs/record/`, `docs/hunts/`, `docs/dev-plans/`, `docs/how-and-why/`,
`docs/teach/`, `docs/learn/`) exist in a public checkout.
