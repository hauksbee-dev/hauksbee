#!/usr/bin/env bash
# install.sh - one command to build galvani and put it on your PATH.
#
# Builds the `galvani` and `galvani-ci` release binaries from this checkout and
# installs (copies) them into a bin directory on your PATH. Idempotent: re-runs
# rebuild only what cargo decides is stale and overwrite the installed copies.
#
# Usage:
#   scripts/install.sh [--prefix DIR] [--no-build] [--symlink] [--help]
#
# Options:
#   --prefix DIR   Install into DIR/bin (default: $PREFIX or ~/.local).
#   --no-build     Skip `cargo build`; install whatever is already in
#                  target/release (errors if the binaries are missing).
#   --symlink      Symlink the binaries instead of copying them, so a later
#                  rebuild is picked up without re-running install.
#   --help         Show this help.
#
# Environment:
#   PREFIX         Same as --prefix.
#   CARGO          cargo binary to use (default: cargo).
#
# After install, ensure the bin dir is on PATH, e.g.:
#   export PATH="$HOME/.local/bin:$PATH"
set -euo pipefail
# shellcheck source=scripts/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; }

PREFIX_ARG=""
DO_BUILD=1
USE_SYMLINK=0
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX_ARG="${2:?--prefix needs a directory}"; shift 2 ;;
    --prefix=*) PREFIX_ARG="${1#*=}"; shift ;;
    --no-build) DO_BUILD=0; shift ;;
    --symlink) USE_SYMLINK=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument '$1' (try --help)" ;;
  esac
done

CARGO="${CARGO:-cargo}"
PREFIX="${PREFIX_ARG:-$(galvani_default_prefix)}"
BINDIR="$PREFIX/bin"
SRC="$(galvani_target_bin)"

have "$CARGO" || die "cargo not found. Install Rust from https://rustup.rs, then re-run."

if [ "$DO_BUILD" -eq 1 ]; then
  log "Building galvani + galvani-ci (release)"
  ( cd "$GALVANI_ROOT" && "$CARGO" build --release -p galvani-engine -p galvani-ci )
else
  log "Skipping build (--no-build)"
fi

for bin in galvani galvani-ci; do
  [ -x "$SRC/$bin" ] || die "$SRC/$bin missing. Run without --no-build, or 'cargo build --release' first."
done

mkdir -p "$BINDIR"
log "Installing into $BINDIR"
for bin in galvani galvani-ci; do
  dest="$BINDIR/$bin"
  if [ "$USE_SYMLINK" -eq 1 ]; then
    ln -sf "$SRC/$bin" "$dest"
    ok "$bin -> $SRC/$bin (symlink)"
  else
    install -m 0755 "$SRC/$bin" "$dest"
    ok "$bin -> $dest"
  fi
done

printf '\n'
if printf '%s' ":$PATH:" | grep -q ":$BINDIR:"; then
  ok "$BINDIR is already on your PATH"
else
  warn "$BINDIR is not on your PATH. Add it:"
  info "  export PATH=\"$BINDIR:\$PATH\""
fi
printf '\n'
log "Done. Verify with:"
info "  galvani run $GALVANI_ROOT/crates/galvani-ci/examples/boards/blinky.kicad_pcb --report"
info "  galvani-ci run $GALVANI_ROOT/crates/galvani-ci/examples/blinky.toml"
