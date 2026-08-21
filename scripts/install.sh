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
  # ships the current UI. `scripts/install.sh` promises the complete
  # product, including `hauksbee serve`; succeeding with no web front door (or
  # embedding a stale dist after a failed rebuild) makes the first run fail
  # after an apparently successful install, so the source installer is
  # deliberately fail-closed here.
  if have bun; then
    log "Building web front door (frontend/dist via bun)"
    ( cd "$HAUKSBEE_ROOT/frontend" && bun install --frozen-lockfile --silent && bun run build ) \
      || die "Frontend build via bun failed; no binaries were installed. Fix the frontend build and re-run."
  elif have npm; then
    log "Building web front door (frontend/dist via npm)"
    ( cd "$HAUKSBEE_ROOT/frontend" && npm ci --silent && npm run build ) \
      || die "Frontend build via npm failed; no binaries were installed. Fix the frontend build and re-run."
  else
    die "bun/npm not found, so the web front door cannot be built. Install bun (https://bun.sh) and re-run; no binaries were installed."
  fi

  # Embed the web UI into the binary (same mechanism as bundle.sh): rust-embed
  # needs frontend/dist to exist at COMPILE time, so gate on it being present,
  # whether from the build above or an earlier one.
  [ -d "$HAUKSBEE_ROOT/frontend/dist" ] \
    || die "frontend build reported success but frontend/dist is missing; refusing a web-less install."
  SIMAVR_COMMIT="$(sed -nE 's/^SIMAVR_COMMIT="([0-9a-f]+)"$/\1/p' "$HAUKSBEE_ROOT/scripts/install-sims.sh" | head -1)"
  [[ "$SIMAVR_COMMIT" =~ ^[0-9a-f]{40}$ ]] \
    || die "scripts/install-sims.sh does not name one immutable simavr commit"
  _simavr_prefix="$(dirname "$(dirname "$_simavr_lib")")"
  [ "$(cat "$_simavr_prefix/.hauksbee-simavr-commit" 2>/dev/null || true)" = "$SIMAVR_COMMIT" ] \
    || die "simavr at $_simavr_prefix is not the pinned install from scripts/install-sims.sh --avr; refusing an unattested source build"
  "$HAUKSBEE_ROOT/scripts/simavr-payload-provenance.sh" verify "$_simavr_prefix" \
    || die "simavr payload bytes under $_simavr_prefix do not match their recorded provenance"
  export SIMAVR_COMMIT
  EMBED_ARGS=(--features "serve,embed-web")
  log "Building hauksbee + hauksbee-ci + hauksbee-mcp (release, serve + embed-web)"
  # `${arr[@]+...}` guards empty-array expansion under `set -u` on bash 3.2
  # (the macOS default), where a bare `"${arr[@]}"` on an empty array errors.
  ( cd "$HAUKSBEE_ROOT" && "$CARGO" build --locked --release -p hauksbee-engine -p hauksbee-ci -p hauksbee-mcp ${EMBED_ARGS[@]+"${EMBED_ARGS[@]}"} )
  # A checkout can also contain a stale release-bundle `bin/` directory. The
  # build above is authoritative for a source install, so re-resolve the Cargo
  # target after it completes rather than retaining an earlier bundle fallback.
  SRC="$(hauksbee_target_bin)"
else
  log "Skipping build (--no-build)"
fi

for bin in hauksbee hauksbee-ci hauksbee-mcp; do
  [ -x "$SRC/$bin" ] || die "$SRC/$bin missing. Run without --no-build, or 'cargo build --release' first."
done

mkdir -p "$BINDIR"
log "Installing into $BINDIR"
install_lock="$BINDIR/.hauksbee-source-install.lock"
lock_token="$$-$RANDOM-$RANDOM"
stage="$BINDIR/.hauksbee-source-install-stage-$$-$RANDOM"
backup="$BINDIR/.hauksbee-source-install-backup-$$-$RANDOM"
install_committed=0
lock_owned=0
cleanup_install() {
  if [ "$install_committed" -eq 0 ]; then
    for rollback_bin in hauksbee hauksbee-ci hauksbee-mcp; do
      if [ -e "$backup/.installing-$rollback_bin" ]; then
        find "$BINDIR/$rollback_bin" -maxdepth 0 \( -type f -o -type l \) -delete 2>/dev/null || true
      fi
      if [ -e "$backup/$rollback_bin" ] || [ -L "$backup/$rollback_bin" ]; then
        find "$BINDIR/$rollback_bin" -maxdepth 0 \( -type f -o -type l \) -delete 2>/dev/null || true
        mv "$backup/$rollback_bin" "$BINDIR/$rollback_bin" 2>/dev/null || true
      fi
    done
  fi
  find "$stage" "$backup" -depth -delete 2>/dev/null || true
  if [ "$lock_owned" -eq 1 ] && [ "$(sed -n '2p' "$install_lock" 2>/dev/null || true)" = "$lock_token" ]; then
    find "$install_lock" -maxdepth 0 -type f -delete 2>/dev/null || true
  fi
  lock_owned=0
}
trap cleanup_install EXIT INT TERM

recover_source_install() {
  local stale_lock="$1" old_stage old_backup recovery_bin
  old_stage="$(sed -n '3p' "$stale_lock" 2>/dev/null || true)"
  old_backup="$(sed -n '4p' "$stale_lock" 2>/dev/null || true)"
  case "$old_stage" in "$BINDIR/.hauksbee-source-install-stage-"*) ;; *) die "stale source-install lock has an unsafe stage path; inspect $install_lock" ;; esac
  case "$old_backup" in "$BINDIR/.hauksbee-source-install-backup-"*) ;; *) die "stale source-install lock has an unsafe backup path; inspect $install_lock" ;; esac
  if [ ! -e "$old_backup/.committed" ]; then
    for recovery_bin in hauksbee hauksbee-ci hauksbee-mcp; do
      if [ -e "$old_backup/.installing-$recovery_bin" ]; then
        find "$BINDIR/$recovery_bin" -maxdepth 0 \( -type f -o -type l \) -delete 2>/dev/null || true
      fi
      if [ -e "$old_backup/$recovery_bin" ] || [ -L "$old_backup/$recovery_bin" ]; then
        find "$BINDIR/$recovery_bin" -maxdepth 0 \( -type f -o -type l \) -delete 2>/dev/null || true
        mv "$old_backup/$recovery_bin" "$BINDIR/$recovery_bin"
      fi
    done
  fi
  find "$old_stage" "$old_backup" -depth -delete 2>/dev/null || true
  find "$stale_lock" -maxdepth 0 -type f -delete 2>/dev/null || true
}

for _lock_attempt in 1 2; do
  lock_candidate="$BINDIR/.hauksbee-source-install.lock.candidate-$lock_token"
  printf '%s\n%s\n%s\n%s\n' "$$" "$lock_token" "$stage" "$backup" > "$lock_candidate"
  chmod 600 "$lock_candidate"
  if ln "$lock_candidate" "$install_lock" 2>/dev/null; then
    find "$lock_candidate" -maxdepth 0 -type f -delete 2>/dev/null || true
    lock_owned=1
    break
  fi
  find "$lock_candidate" -maxdepth 0 -type f -delete 2>/dev/null || true
  lock_owner="$(sed -n '1p' "$install_lock" 2>/dev/null || true)"
  if ! [[ "$lock_owner" =~ ^[0-9]+$ ]]; then
    die "source-install lock has no valid owner; inspect $install_lock"
  fi
  if kill -0 "$lock_owner" 2>/dev/null || ps -p "$lock_owner" -o pid= 2>/dev/null | grep -Eq '[0-9]'; then
    die "another source install (pid $lock_owner) is updating $BINDIR; wait and retry"
  fi
  stale_lock="$BINDIR/.hauksbee-source-install.lock.stale-$lock_token-$_lock_attempt"
  if ! mv "$install_lock" "$stale_lock" 2>/dev/null; then
    continue
  fi
  recover_source_install "$stale_lock"
done
[ "$lock_owned" -eq 1 ] || die "could not acquire source-install lock $install_lock"
mkdir "$stage" "$backup"

# Prepare all three destinations before changing any live command.
for bin in hauksbee hauksbee-ci hauksbee-mcp; do
  if [ "$USE_SYMLINK" -eq 1 ]; then
    ln -s "$SRC/$bin" "$stage/$bin"
  else
    install -m 0755 "$SRC/$bin" "$stage/$bin"
  fi
done
for bin in hauksbee hauksbee-ci hauksbee-mcp; do
  dest="$BINDIR/$bin"
  if [ -e "$dest" ] || [ -L "$dest" ]; then
    [ -f "$dest" ] || [ -L "$dest" ] || die "$dest is not a file or symlink; refusing to replace it"
    mv "$dest" "$backup/$bin"
  fi
done
for bin in hauksbee hauksbee-ci hauksbee-mcp; do
  dest="$BINDIR/$bin"
  : > "$backup/.installing-$bin"
  mv "$stage/$bin" "$dest"
  if [ "$USE_SYMLINK" -eq 1 ]; then
    ok "$bin -> $SRC/$bin (symlink)"
  else
    ok "$bin -> $dest"
  fi
done
: > "$backup/.committed"
install_committed=1
cleanup_install
trap - EXIT INT TERM

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
