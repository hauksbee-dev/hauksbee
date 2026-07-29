# Vendored: the kicad-forge forge-* crates

This directory holds three crates that make up `kicad-forge`, the KiCad file
layer hauksbee reads and writes boards through. They are vendored rather than
depended on, so a fresh clone of this repo builds with no sibling checkout and
no network access.

`kicad-forge` has no public repository. Hauksbee is where it is used, and this
directory is its source of record: edit the crates here, and the update
procedure below is history rather than a live import path.

## Why vendored (and not a dependency)

The workspace once declared these as path dependencies on
`../kicad-forge/crates/*`, a sibling repo that existed only on the author's
machine. Cargo loads every workspace member manifest during `cargo metadata`,
so a clone without that sibling failed before any build could start, even for
crates that never touch board-as-code. This was the number-one adoption
blocker.

A git dependency does not solve it either. The exact forge sources hauksbee
builds against live on an unpushed branch and are not reachable from any
public ref, so a `rev`-pinned git dependency could not fetch them on a clean
clone. Vendoring captures the exact sources, builds offline, and is
deterministic.

## Provenance

- Imported from commit `d3f1a18293387e721dddaa2f93af8c34d0eb3691`
  (branch `feat/decompile-board-example`) of the private kicad-forge checkout.
- License: Apache-2.0, matching this workspace. The upstream is the same
  author, so the relicense needed no third-party permission.

Copied, per crate: `Cargo.toml` and the full `src/` tree. Not copied:
`tests/`, `examples/`, and their fixtures, since hauksbee links the libraries
rather than the test or example targets. The `[[test]]` targets were removed
from `crates/forge-model/Cargo.toml` accordingly. Nothing else was edited.

External crate dependencies: only `thiserror` (crates.io), used by
`forge-model`. The three crates otherwise depend on each other and std.

## Re-importing from a kicad-forge checkout

Kept for the maintainer, who still has one. Everyone else edits the crates
here directly.

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
3. Re-strip the `[[test]]` targets from `crates/forge-model/Cargo.toml`. They
   reference `tests/` files that are not vendored.
4. Update the commit hash above.
5. Verify: `cargo build -p hauksbee-engine && cargo test -p hauksbee-extract`.

Re-importing overwrites any edit made here, so a change that should last needs
to go into both places, or into this directory alone once the checkout is
retired.

## Local forge development

To develop against a live kicad-forge checkout instead of this vendored copy,
see the commented path override in the root `Cargo.toml` under
`[workspace.dependencies]`.
