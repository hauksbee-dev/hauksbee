#!/usr/bin/env bash
# bundle.sh - build a versioned, distributable hauksbee binary bundle.
#
# Produces dist/hauksbee-<version>-<target>.tar.gz containing:
#   bin/hauksbee, bin/hauksbee-ci   the release binaries
#   db/                           the reference model database (the binaries
#                                 embed it; this copy is for the layered
#                                 ~/.hauksbee/models override mechanism and docs)
#   integrations/                 the GitHub Action, KiCad plugin, pre-commit hook
#   examples/                     the hauksbee-ci specs + boards (runnable demos)
#   scripts/                      install.sh / doctor.sh / ci.sh
#   VERSION, README-BUNDLE.txt    provenance + how to install
#
# Idempotent: re-running rebuilds the same artifact (overwriting it). The
# release.yml GitHub workflow calls this on a tag to produce the attached asset.
#
# Usage:
#   scripts/bundle.sh [--version V] [--target TRIPLE] [--out DIR] [--no-build] [--help]
#
# Options:
#   --version V    Override the version (default: workspace Cargo.toml version).
#   --target TRIPLE Override the target triple label (default: host os-arch).
#   --out DIR      Output directory (default: dist).
#   --no-build     Use existing target/release binaries; do not cargo build.
#   --no-default-features
#                  Pass --no-default-features to the cargo build (used by the
#                  release workflow for the GPL-free renode+qemu shape).
#   --features LIST Pass --features LIST to the cargo build (e.g. "renode,qemu").
#   --help         Show this help.
#
# Feature note: with NO feature flags, cargo builds the default feature set
# (avr + renode + qemu). The `avr` backend statically links libsimavr, which is
# GPL-3.0 — so a default bundle is GPL-encumbered. Pass
# `--no-default-features --features renode,qemu` for the MIT-clean shape (no
# libsimavr link; verified avr-free). See the release.yml header and
# docs/release-and-licensing.md for the licensing decision and the GPL guard.
set -euo pipefail
# shellcheck source=scripts/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; }

VERSION=""
TARGET=""
OUT="dist"
DO_BUILD=1
# Cargo feature selection for the build. Empty = default features (avr+renode+
# qemu, GPL-encumbered). The release workflow sets these for the MIT-clean shape.
NO_DEFAULT_FEATURES=0
FEATURES=""
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --target) TARGET="${2:?--target needs a value}"; shift 2 ;;
    --target=*) TARGET="${1#*=}"; shift ;;
    --out) OUT="${2:?--out needs a directory}"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    --no-build) DO_BUILD=0; shift ;;
    --no-default-features) NO_DEFAULT_FEATURES=1; shift ;;
    --features) FEATURES="${2:?--features needs a value}"; shift 2 ;;
    --features=*) FEATURES="${1#*=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument '$1' (try --help)" ;;
  esac
done

CARGO="${CARGO:-cargo}"

# Version: from the workspace Cargo.toml [workspace.package] version unless given.
if [ -z "$VERSION" ]; then
  VERSION="$(grep -m1 '^version' "$HAUKSBEE_ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
  [ -n "$VERSION" ] || die "could not read version from Cargo.toml; pass --version"
fi

# Target label: a stable os-arch slug for the asset name.
if [ -z "$TARGET" ]; then
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  TARGET="${os}-${arch}"
fi

SRC="$(hauksbee_target_bin)"
if [ "$DO_BUILD" -eq 1 ]; then
  have "$CARGO" || die "cargo not found. Install Rust or pass --no-build."
  # Assemble optional feature flags. Passed verbatim to `cargo build`.
  FEATURE_ARGS=()
  [ "$NO_DEFAULT_FEATURES" -eq 1 ] && FEATURE_ARGS+=(--no-default-features)
  [ -n "$FEATURES" ] && FEATURE_ARGS+=(--features "$FEATURES")
  if [ "${#FEATURE_ARGS[@]}" -gt 0 ]; then
    log "Building release binaries (cargo ${FEATURE_ARGS[*]})"
  else
    log "Building release binaries (default features: avr+renode+qemu, GPL-encumbered)"
  fi
  ( cd "$HAUKSBEE_ROOT" && "$CARGO" build --release -p hauksbee-engine -p hauksbee-ci "${FEATURE_ARGS[@]}" )
  log "Stripping release binaries"
  for bin in hauksbee hauksbee-ci; do
    strip "$SRC/$bin" 2>/dev/null || true
  done
fi
for bin in hauksbee hauksbee-ci; do
  [ -x "$SRC/$bin" ] || die "$SRC/$bin missing (build first, or drop --no-build)."
done

NAME="hauksbee-${VERSION}-${TARGET}"
OUT_ABS="$(mkdir -p "$OUT" && cd "$OUT" && pwd)"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/hauksbee-bundle.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
ROOTDIR="$STAGE/$NAME"
mkdir -p "$ROOTDIR/bin"

log "Staging $NAME"
install -m 0755 "$SRC/hauksbee"    "$ROOTDIR/bin/hauksbee"
install -m 0755 "$SRC/hauksbee-ci" "$ROOTDIR/bin/hauksbee-ci"

# Reference assets. Binaries embed the model db; this copy supports the layered
# ~/.hauksbee/models override and is handy documentation.
cp -R "$HAUKSBEE_ROOT/crates/hauksbee-models/db" "$ROOTDIR/db"
cp -R "$HAUKSBEE_ROOT/integrations" "$ROOTDIR/integrations"
cp -R "$HAUKSBEE_ROOT/scripts" "$ROOTDIR/scripts"
# Examples: ship the specs, boards and READMEs (skip any scratch dirs).
mkdir -p "$ROOTDIR/examples"
cp -R "$HAUKSBEE_ROOT/crates/hauksbee-ci/examples/." "$ROOTDIR/examples/ci-specs"
[ -d "$HAUKSBEE_ROOT/examples" ] && cp -R "$HAUKSBEE_ROOT/examples/." "$ROOTDIR/examples/"
# Drop python bytecode caches so the bundle is reproducible.
find "$ROOTDIR" -name '__pycache__' -type d -prune -exec rm -rf {} + 2>/dev/null || true
find "$ROOTDIR" -name '*.pyc' -delete 2>/dev/null || true

printf '%s\n' "$VERSION" > "$ROOTDIR/VERSION"
GIT_SHA="$(cd "$HAUKSBEE_ROOT" && git rev-parse --short HEAD 2>/dev/null || echo unknown)"

cat > "$ROOTDIR/README-BUNDLE.txt" <<EOF
hauksbee ${VERSION} (${TARGET})
built from git ${GIT_SHA}

A universal PCB emulator: bind a board, bring it to life, assert it in CI.

CONTENTS
  bin/hauksbee       the CLI (run / to-code / from-code / check-code)
  bin/hauksbee-ci    the headless CI runner (TOML specs, JUnit, GH annotations)
  db/               reference model database (the binaries already embed it;
                    drop extra parts here or in ~/.hauksbee/models to extend)
  examples/         runnable demos: board-as-code + hauksbee-ci specs + sessions
  integrations/     GitHub Action, KiCad plugin, pre-commit hook
  scripts/          install.sh, doctor.sh, ci.sh

INSTALL
  Copy the two binaries onto your PATH:
    install -m 0755 bin/hauksbee bin/hauksbee-ci /usr/local/bin/
  or run the bundled installer (no sudo, installs into ~/.local/bin):
    PREFIX=\$HOME/.local scripts/install.sh --no-build --symlink

VERIFY (no external files needed - the static checks run on the bundled board)
  hauksbee run examples/ci-specs/boards/blinky.kicad_pcb --report
  hauksbee run examples/ci-specs/boards/blinky.kicad_pcb --drc

The binaries are self-contained (model db compiled in). Optional firmware
backends (qemu, renode) are detected at runtime; run scripts/doctor.sh to see
what is present.

NOTE: the firmware-bearing hauksbee-ci specs in examples/ci-specs (blinky.toml,
the boot-coverage and brownout specs) reference firmware and netlists that live
in the hauksbee repo's testdata/ (too large to bundle). Run those from a repo
checkout. The bundled specs are the canonical, documented examples to copy.
EOF

log "Writing tarball"
TARBALL="$OUT_ABS/$NAME.tar.gz"
( cd "$STAGE" && tar -czf "$TARBALL" "$NAME" )

# A checksum next to the tarball, for release verification.
if have shasum; then
  ( cd "$OUT_ABS" && shasum -a 256 "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )
elif have sha256sum; then
  ( cd "$OUT_ABS" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256" )
fi

ok "Bundle: $TARBALL"
[ -f "$TARBALL.sha256" ] && ok "Checksum: $TARBALL.sha256"
info "size: $(du -h "$TARBALL" | cut -f1)"
