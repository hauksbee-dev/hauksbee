# Contributing

## Build and test

```bash
scripts/install-sims.sh --avr     # libsimavr for the in-process AVR backend
cargo build --workspace
cargo test --workspace
cd frontend && bun install && bun run build && bun test
```

Renode and Espressif QEMU are optional external backends; tests that need
them are not part of the default suite.

## Layout

- `crates/hauksbee-ir`: circuit intermediate representation and evidence types.
- `crates/hauksbee-extract`: board, fab, schematic, BOM and placement readers.
- `crates/hauksbee-models`: device model database and resolution.
- `crates/hauksbee-solve`: DC, transient and AC solvers.
- `crates/hauksbee-mcu`: MCU emulator backends (AVR, Renode, QEMU).
- `crates/hauksbee-engine`: binder, checks, co-sim scheduler, reports, CLI.
- `crates/hauksbee-ci`: CI spec loader, assertions, reports.
- `crates/hauksbee-server` + `frontend/`: local web UI.
- `crates/hauksbee-mcp`: MCP tools for agents.

## Rules

- `cargo fmt` and `cargo clippy --workspace --all-targets -- -D warnings` must pass.
- A test must prove behaviour: numbers, verdicts, exit codes, JSON fields.
  Do not add tests that assert message wording or documentation text.
- A missing model or an unsupported input is reported as a refusal, never
  turned into a green result.
