#!/usr/bin/env bash
# install.sh - one command to build hauksbee and put it on your PATH.
#
# Builds the `hauksbee`, `hauksbee-ci` and `hauksbee-mcp` release binaries from
# this checkout and installs (copies) them into a bin directory on your PATH.
# Idempotent: a re-run rebuilds only what cargo decides is stale and overwrites
# the installed copies.
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
PREFIX="${PREFIX_ARG:-$(hauksbee_default_prefix)}"
BINDIR="$PREFIX/bin"
SRC="$(hauksbee_target_bin)"

# Release-bundle layout: the extracted tarball carries prebuilt binaries in
# bin/ and no target/ at all (README-BUNDLE.txt points users at
# `scripts/install.sh --no-build --symlink`). Fall back to that layout when
# target/release has no binaries, so the documented command works with no
# Rust toolchain present. A checkout with built binaries is untouched.
if [ ! -x "$SRC/hauksbee" ] && [ -x "$HAUKSBEE_ROOT/bin/hauksbee" ]; then
  SRC="$HAUKSBEE_ROOT/bin"
fi

if [ "$DO_BUILD" -eq 1 ]; then
  have "$CARGO" || die "cargo not found. Install Rust from https://rustup.rs, then re-run (or pass --no-build to install prebuilt binaries)."

  # Preflight the default-features build BEFORE the multi-minute cargo run.
  # The `avr` backend (on by default) runs bindgen (needs clang/libclang) and
  # statically links libsimavr; missing either used to surface as a build.rs
  # panic four minutes in. Mirror hauksbee-mcu/build.rs's discovery: explicit
  # SIMAVR_LIB_DIR override, else <prefix>/lib for /opt/homebrew (Apple
  # Silicon) or /usr/local (everything else).
  if ! have clang; then
    die "clang not found, and the default build needs it (bindgen over the simavr headers).
    Install it (Xcode CLT on macOS: xcode-select --install; Debian/Ubuntu: apt install clang libclang-dev),
    or build without the avr backend: cargo build --release --no-default-features --features renode,qemu"
  fi
  _simavr_prefix="/usr/local"
  [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ] && _simavr_prefix="/opt/homebrew"
  _simavr_lib="${SIMAVR_LIB_DIR:-$_simavr_prefix/lib}/libsimavr.a"
  if [ ! -f "$_simavr_lib" ]; then
    die "libsimavr.a not found (looked for $_simavr_lib), and the default build links it.
    Either install it first:   scripts/install-sims.sh --avr
    or point at an existing install:   SIMAVR_INCLUDE_DIR=<prefix>/include SIMAVR_LIB_DIR=<prefix>/lib scripts/install.sh
    or build without the avr backend:  cargo build --release --no-default-features --features renode,qemu"
  fi

  # Build the web front door bundle first. `hauksbee serve` serves
  # frontend/dist/, which is gitignored (a build artifact), so a fresh `git
  # pull` + install would otherwise keep serving a stale bundle: the classic
  # "I rebuilt but the page is old" trap. Rebuild it here so an install always
  # ships the current UI. Skipped (with a warning) if no JS toolchain is
  # present or the JS build fails; `serve` then falls back to any existing
  # dist/ or the bundled note. A broken frontend toolchain must not take the
  # whole install down with it.
  if have bun; then
    log "Building web front door (frontend/dist via bun)"
    ( cd "$HAUKSBEE_ROOT/frontend" && bun install --silent && bun run build ) \
      || warn "Frontend build via bun FAILED; continuing without a fresh web UI."
  elif have npm; then
    log "Building web front door (frontend/dist via npm)"
    ( cd "$HAUKSBEE_ROOT/frontend" && npm install --silent && npm run build ) \
      || warn "Frontend build via npm FAILED; continuing without a fresh web UI."
  else
    warn "No bun/npm found; skipping the frontend build."
    warn "Install bun (https://bun.sh) and re-run to refresh the web UI."
  fi

  # Embed the web UI into the binary (same mechanism as bundle.sh): rust-embed
  # needs frontend/dist to exist at COMPILE time, so gate on it being present,
  # whether from the build above or an earlier one.
  EMBED_ARGS=()
  if [ -d "$HAUKSBEE_ROOT/frontend/dist" ]; then
    EMBED_ARGS=(--features embed-web)
    log "Building hauksbee + hauksbee-ci + hauksbee-mcp (release, embed-web)"
  else
    warn "frontend/dist is missing, so the binaries are built WITHOUT the web UI:"
    warn "  \`hauksbee serve\` will have no web front door."
    warn "  Fix: install bun (https://bun.sh), then re-run scripts/install.sh."
    log "Building hauksbee + hauksbee-ci + hauksbee-mcp (release)"
  fi
  # `${arr[@]+...}` guards empty-array expansion under `set -u` on bash 3.2
  # (the macOS default), where a bare `"${arr[@]}"` on an empty array errors.
  ( cd "$HAUKSBEE_ROOT" && "$CARGO" build --release -p hauksbee-engine -p hauksbee-ci -p hauksbee-mcp ${EMBED_ARGS[@]+"${EMBED_ARGS[@]}"} )
else
  log "Skipping build (--no-build)"
fi

for bin in hauksbee hauksbee-ci hauksbee-mcp; do
  [ -x "$SRC/$bin" ] || die "$SRC/$bin missing. Run without --no-build, or 'cargo build --release' first."
done

mkdir -p "$BINDIR"
log "Installing into $BINDIR"
for bin in hauksbee hauksbee-ci hauksbee-mcp; do
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
case ":${PATH}:" in *":${BINDIR}:"*) PATH_HAS_BINDIR=1 ;; *) PATH_HAS_BINDIR=0 ;; esac
if [ "${PATH_HAS_BINDIR}" = 1 ]; then
  ok "$BINDIR is already on your PATH"
else
  warn "$BINDIR is not on your PATH. Add it:"
  info "  export PATH=\"$BINDIR:\$PATH\""
fi
printf '\n'
# Layout-aware closing line: "frontend/dist" is checkout-speak that means
# nothing to a bundle user, whose web UI is embedded in the binary.
if [ "$DO_BUILD" -eq 1 ]; then
  ok "Everything is current: web UI (frontend/dist) + hauksbee + hauksbee-ci + hauksbee-mcp."
  info "  Re-run this script any time (e.g. after 'git pull') to rebuild them all."
else
  ok "Installed: hauksbee + hauksbee-ci + hauksbee-mcp (the web UI is embedded; \`hauksbee serve\` opens it)."
fi
printf '\n'
log "Done. Verify with:"
# The example paths differ per layout: a checkout keeps them under crates/,
# a release bundle ships them under examples/ci-specs.
# Point at the Watchy, not the minimal blinky board: the first thing someone
# runs after installing should meet a board that was actually fabricated.
if [ -f "$HAUKSBEE_ROOT/crates/hauksbee-ci/examples/boards/watchy.kicad_pcb" ]; then
  info "  hauksbee run $HAUKSBEE_ROOT/crates/hauksbee-ci/examples/boards/watchy.kicad_pcb --report"
  info "  hauksbee-ci run $HAUKSBEE_ROOT/crates/hauksbee-ci/examples/watchy.toml"
else
  info "  hauksbee run $HAUKSBEE_ROOT/examples/ci-specs/boards/watchy.kicad_pcb --report"
fi
