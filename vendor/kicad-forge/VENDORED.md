# Vendored: kicad-forge forge-* crates

This directory is a vendored copy of three crates from the sibling `kicad-forge`
repository. They are vendored so that a fresh clone of this repo builds with no
external sibling checkout and no network access.

## Why vendored (and not a git dependency)

The workspace previously declared these as path dependencies on
`../kicad-forge/crates/*`, a sibling repo that exists only on the author's
machine. Cargo loads every workspace member manifest during `cargo metadata`,
so a clone without that sibling failed before any build could start, even for
crates that never touch board-as-code. This was the number-one adoption blocker.

A git dependency was rejected: the exact forge sources hauksbee builds against
live on an unpushed local branch of kicad-forge and are not reachable from any
public ref, so a `rev`-pinned git dep could not fetch them on a clean clone.
Vendoring captures the exact sources, builds offline, and is deterministic.

## Source

- Repository: https://github.com/ETM-Code/kicad-forge.git
- Commit: `d3f1a18293387e721dddaa2f93af8c34d0eb3691`
  (branch `feat/decompile-board-example`)
- License: Apache-2.0 (relicensed with this workspace; the upstream sibling repo is the same author)

## What was and was not copied

Copied, per crate: `Cargo.toml` and the full `src/` tree. Not copied: `tests/`,
`examples/`, and their fixtures (hauksbee links the libraries, not the test or
example targets). The `[[test]]` targets were removed from
`crates/forge-model/Cargo.toml` accordingly. Nothing else was edited.

External crate dependencies: only `thiserror` (crates.io), used by
`forge-model`. The three crates otherwise depend on each other and std.

## Update procedure

When the upstream forge crates change and hauksbee needs the new version:

1. In a kicad-forge checkout, note the commit: `git -C <kicad-forge> rev-parse HEAD`.
2. From this repo root, re-copy the sources:
   ```sh
   SRC=<path-to-kicad-forge>
   for c in forge-sexpr forge-model forge-codegen; do
     rm -rf "vendor/kicad-forge/crates/$c/src"
     cp -R "$SRC/crates/$c/src" "vendor/kicad-forge/crates/$c/src"
     cp "$SRC/crates/$c/Cargo.toml" "vendor/kicad-forge/crates/$c/Cargo.toml"
   done
   ```
3. Re-strip the `[[test]]` targets from `crates/forge-model/Cargo.toml` (they
   reference `tests/` files that are not vendored).
4. Update the commit hash in this file.
5. Verify: `cargo build -p hauksbee-engine && cargo test -p hauksbee-extract`.

## Local forge development

To develop against a live kicad-forge checkout instead of this vendored copy,
see the commented path override in the root `Cargo.toml` under
`[workspace.dependencies]`.
