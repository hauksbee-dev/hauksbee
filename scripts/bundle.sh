#!/usr/bin/env bash
# bundle.sh - build a versioned, distributable hauksbee binary bundle.
#
# Produces dist/<base>.tar.gz containing:
#   bin/hauksbee, bin/hauksbee-ci, bin/hauksbee-mcp   the release binaries
#   db/                           the reference model database (the binaries
#                                 embed it; this copy is for the layered
#                                 ~/.hauksbee/models override mechanism and docs)
#   integrations/                 the GitHub Action, KiCad plugin, pre-commit hook
#   examples/                     the hauksbee-ci specs + boards (runnable demos)
#   scripts/                      install.sh / doctor.sh / ci.sh /
#                                 install-sims.sh / common.sh /
#                                 simavr-payload-provenance.sh /
#                                 simulator-provenance.py
#   LICENSE, NOTICE               hauksbee's own Apache-2.0 terms + attribution
#   LICENSE-BINARY.txt            what THIS binary is licensed under, and why
#   VERSION, README-BUNDLE.txt    provenance + how to install
#
# Idempotent: re-running rebuilds the same artifact (overwriting it). The
# release.yml GitHub workflow calls this on a tag to produce the attached asset.
#
# Usage:
#   scripts/bundle.sh [--shape default|permissive] [--version V] [--target TRIPLE]
#                     [--out DIR] [--no-build] [--help]
#
# Options:
#   --shape SHAPE  Which of the two shipped shapes to build (default: default).
#                    default     avr + renode + qemu. Statically links libsimavr,
#                                so the BINARY is GPL-3.0. Asset base name:
#                                hauksbee-<version>-<target>
#                    permissive  --no-default-features --features renode,qemu.
#                                No avr, no libsimavr, binary stays Apache-2.0.
#                                Asset base name:
#                                hauksbee-<version>-<target>-permissive
#   --version V    Override the version (default: workspace Cargo.toml version).
#   --target TRIPLE Override the target triple label (default: host os-arch).
#   --out DIR      Output directory (default: dist).
#   --no-build     Use existing target/release binaries; do not cargo build.
#   --no-default-features
#                  Pass --no-default-features to the cargo build. Overrides what
#                  --shape would have chosen; the asset name still follows --shape.
#   --features LIST Pass --features LIST to the cargo build (e.g. "renode,qemu").
#                  Same override semantics as --no-default-features.
#   --help         Show this help.
#
# Licensing note: the `avr` backend statically links libsimavr, which is GPL-3.0,
# so the default shape's binary is a GPL-3.0 combined work even though hauksbee's
# source stays Apache-2.0. That is a deliberate, labelled choice: AVR/ATmega is
# the biggest slice of the audience and the download it needs must exist. The
# permissive shape is the same tool minus the avr backend, for redistributors and
# embedders who cannot take GPL code. Both ship on every release, each carrying a
# LICENSE-BINARY.txt that says which it is. See the release.yml header and
# docs/about/release-and-licensing.md for the decision and the GPL guard that
# keeps the permissive shape honest.
#
# Web UI: on a build (not --no-build) this first builds frontend/dist and then
# appends the `serve,embed-web` features, so the resulting binary includes the
# web front door and embeds its UI; `hauksbee serve` works from a bare install
# even for the permissive no-default-features shape. embed-web only adds
# rust-embed (permissive), so it has no bearing on the GPL guard. Needs bun or
# npm on PATH;
# without a JS toolchain and no existing dist/, the bundle builds without a UI.
#
# macOS signing: on a Darwin host, when HAUKSBEE_SIGN_IDENTITY is set, each
# staged binary is codesigned (hardened runtime + timestamp) and verified; the
# notary env vars build-app.sh documents (HAUKSBEE_NOTARY_PROFILE, or the
# APPLE_ID/TEAM_ID/PASSWORD triple) additionally notarise the signed binaries.
# Without the vars the bundle ships unsigned, with a printed note. See
# app/macos/SIGNING.md.
set -euo pipefail
# shellcheck source=scripts/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; }

VERSION=""
TARGET=""
OUT="dist"
DO_BUILD=1
SHAPE="default"
# Cargo feature selection for the build. Normally derived from --shape below;
# an explicit --no-default-features/--features wins (build-app.sh forwards them).
NO_DEFAULT_FEATURES=0
FEATURES=""
FEATURES_EXPLICIT=0
while [ $# -gt 0 ]; do
  case "$1" in
    --shape) SHAPE="${2:?--shape needs default or permissive}"; shift 2 ;;
    --shape=*) SHAPE="${1#*=}"; shift ;;
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --version=*) VERSION="${1#*=}"; shift ;;
    --target) TARGET="${2:?--target needs a value}"; shift 2 ;;
    --target=*) TARGET="${1#*=}"; shift ;;
    --out) OUT="${2:?--out needs a directory}"; shift 2 ;;
    --out=*) OUT="${1#*=}"; shift ;;
    --no-build) DO_BUILD=0; shift ;;
    --no-default-features) NO_DEFAULT_FEATURES=1; FEATURES_EXPLICIT=1; shift ;;
    --features) FEATURES="${2:?--features needs a value}"; FEATURES_EXPLICIT=1; shift 2 ;;
    --features=*) FEATURES="${1#*=}"; FEATURES_EXPLICIT=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument '$1' (try --help)" ;;
  esac
done

# ── shape -> feature flags + asset name suffix ───────────────────────────────
# The beta ships one shape: `permissive`. `default` adds the avr backend and is
# therefore a GPL-3.0 binary; `permissive` drops it and stays Apache-2.0.
case "$SHAPE" in
  default)
    NAME_SUFFIX=""
    ;;
  permissive)
    NAME_SUFFIX="-permissive"
    if [ "$FEATURES_EXPLICIT" -eq 0 ]; then
      NO_DEFAULT_FEATURES=1
      FEATURES="renode,qemu"
    fi
    ;;
  *) die "unknown --shape '$SHAPE' (expected 'default' or 'permissive')" ;;
esac

CARGO="${CARGO:-cargo}"
HAUKSBEE_RELEASE_COMMIT="${HAUKSBEE_RELEASE_COMMIT:-$(git -C "$HAUKSBEE_ROOT" rev-parse HEAD 2>/dev/null || true)}"
export HAUKSBEE_RELEASE_COMMIT
[[ "$HAUKSBEE_RELEASE_COMMIT" =~ ^[0-9a-f]{40}$ ]] \
  || die "release bundle needs one exact HAUKSBEE_RELEASE_COMMIT to pin generated private workflows"
if [ -n "$(git -C "$HAUKSBEE_ROOT" status --porcelain --untracked-files=normal --)" ]; then
  die "refusing to bundle a dirty source tree: commit or stash every tracked/untracked source change so the embedded commit identifies the bytes being packaged"
fi
export HAUKSBEE_SOURCE_COMMIT="$HAUKSBEE_RELEASE_COMMIT"

# Resolve the immutable simavr revision before Cargo runs. build.rs embeds this
# value into every AVR-capable binary; discovering it only after the build would
# guarantee that an ordinary local bundle builds an unattested binary and then
# correctly refuses to package it.
SIMAVR_COMMIT="$(sed -nE 's/^SIMAVR_COMMIT="([0-9a-f]+)"$/\1/p' \
  "$HAUKSBEE_ROOT/scripts/install-sims.sh" | head -1)"
[[ "$SIMAVR_COMMIT" =~ ^[0-9a-f]{40}$ ]] \
  || die "scripts/install-sims.sh does not name one immutable SIMAVR_COMMIT"

if [ "$SHAPE" = default ] && [ "$DO_BUILD" -eq 1 ]; then
  export SIMAVR_COMMIT

  # Bind Cargo to one installation carrying the same immutable marker. Honour
  # an explicit pair first, then the CI/release prefix, then the installer's
  # conventional platform prefix. A half-specified or unmarked install is not
  # corresponding-source evidence and must fail before compilation.
  if { [ -n "${SIMAVR_INCLUDE_DIR:-}" ] && [ -z "${SIMAVR_LIB_DIR:-}" ]; } \
      || { [ -z "${SIMAVR_INCLUDE_DIR:-}" ] && [ -n "${SIMAVR_LIB_DIR:-}" ]; }; then
    die "set both SIMAVR_INCLUDE_DIR and SIMAVR_LIB_DIR, or neither"
  fi
  if [ -n "${SIMAVR_INCLUDE_DIR:-}" ]; then
    simavr_prefix="$(dirname "$SIMAVR_INCLUDE_DIR")"
    [ "$(dirname "$SIMAVR_LIB_DIR")" = "$simavr_prefix" ] \
      || die "SIMAVR_INCLUDE_DIR and SIMAVR_LIB_DIR must belong to one prefix"
  else
    pinned_prefix="$HOME/.hauksbee-simavr/$SIMAVR_COMMIT"
    if [ -f "$pinned_prefix/.hauksbee-simavr-commit" ]; then
      simavr_prefix="$pinned_prefix"
    elif [ "$(uname -s)" = Darwin ] && [ "$(uname -m)" = arm64 ]; then
      simavr_prefix="/opt/homebrew"
    else
      simavr_prefix="/usr/local"
    fi
    export SIMAVR_INCLUDE_DIR="$simavr_prefix/include"
    export SIMAVR_LIB_DIR="$simavr_prefix/lib"
  fi
  [ -f "$SIMAVR_INCLUDE_DIR/simavr/sim_avr.h" ] \
    || die "pinned simavr header missing at $SIMAVR_INCLUDE_DIR/simavr/sim_avr.h; run scripts/install-sims.sh --avr --prefix '$HOME/.hauksbee-simavr/$SIMAVR_COMMIT'"
  [ -f "$SIMAVR_LIB_DIR/libsimavr.a" ] \
    || die "pinned libsimavr.a missing at $SIMAVR_LIB_DIR/libsimavr.a; run scripts/install-sims.sh --avr --prefix '$HOME/.hauksbee-simavr/$SIMAVR_COMMIT'"
  [ "$(cat "$simavr_prefix/.hauksbee-simavr-commit" 2>/dev/null || true)" = "$SIMAVR_COMMIT" ] \
    || die "simavr at $simavr_prefix is not marked as immutable commit $SIMAVR_COMMIT; refusing to build an unattested default bundle"
  "$HAUKSBEE_ROOT/scripts/simavr-payload-provenance.sh" verify "$simavr_prefix" \
    || die "simavr payload bytes under $simavr_prefix do not match their recorded provenance"
fi

# Version: from the workspace Cargo.toml [workspace.package] version unless given.
if [ -z "$VERSION" ]; then
  VERSION="$(grep -m1 '^version' "$HAUKSBEE_ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
  [ -n "$VERSION" ] || die "could not read version from Cargo.toml; pass --version"
fi
export HAUKSBEE_RELEASE_TAG="v$VERSION"

# Target label: a stable os-arch slug for the asset name.
if [ -z "$TARGET" ]; then
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  TARGET="${os}-${arch}"
fi

SRC="$(hauksbee_target_bin)"
if [ "$DO_BUILD" -eq 1 ]; then
  have "$CARGO" || die "cargo not found. Install Rust or pass --no-build."

  # Build the web front door so the release bundle self-contains the UI. The
  # serve + embed-web (appended below) compile the front door and frontend/dist
  # INTO the binary, so `hauksbee serve` works from a bare installed binary
  # with no checkout.
  # Release bytes must come from the checked-in Bun lock. Gated on DO_BUILD
  # (the --no-build path ships the already-built binaries as-is and never
  # rebuilds the frontend).
  if have bun; then
    log "Building web front door (frontend/dist via bun)"
    ( cd "$HAUKSBEE_ROOT/frontend" && bun install --frozen-lockfile --silent && bun run build )
  else
    die "bun not found; a release bundle must rebuild frontend/dist from the checked-in bun.lock"
  fi

  # Self-contain the web app: append serve,embed-web so the built binary serves
  # the UI without a checkout. A release bundle always wants this. rust-embed needs
  # frontend/dist to exist at COMPILE time, so only enable it when dist is
  # actually present; a missing dist would otherwise hard-fail the build. Append
  # (never replace) so it composes onto whatever features were requested, e.g.
  # the permissive shape's `renode,qemu` -> `renode,qemu,serve,embed-web`.
  if [ -d "$HAUKSBEE_ROOT/frontend/dist" ]; then
    if [ -n "$FEATURES" ]; then FEATURES="$FEATURES,serve,embed-web"; else FEATURES="serve,embed-web"; fi
    log "Self-contained web UI: serve + embed-web enabled (features: ${FEATURES})"
  else
    warn "frontend/dist not found; building WITHOUT embed-web."
    warn "The bare binary will have no web UI until it is built from a checkout."
  fi

  # Assemble optional feature flags. Passed verbatim to `cargo build`.
  FEATURE_ARGS=()
  [ "$NO_DEFAULT_FEATURES" -eq 1 ] && FEATURE_ARGS+=(--no-default-features)
  [ -n "$FEATURES" ] && FEATURE_ARGS+=(--features "$FEATURES")
  if [ "${#FEATURE_ARGS[@]}" -gt 0 ]; then
    log "Building release binaries, shape '$SHAPE' (cargo ${FEATURE_ARGS[*]})"
  else
    log "Building release binaries, shape '$SHAPE' (default features: avr+renode+qemu)"
  fi
  if [ "$SHAPE" = default ]; then
    info "  the avr backend links libsimavr (GPL-3.0): this binary will be GPL-3.0"
  fi
  # `${arr[@]+...}` guards empty-array expansion under `set -u` on bash 3.2
  # (the macOS default), where a bare `"${arr[@]}"` on an empty array errors.
  ( cd "$HAUKSBEE_ROOT" && "$CARGO" build --locked --release -p hauksbee-engine -p hauksbee-ci -p hauksbee-mcp ${FEATURE_ARGS[@]+"${FEATURE_ARGS[@]}"} )

  log "Stripping release binaries"
  for bin in hauksbee hauksbee-ci hauksbee-mcp; do
    strip "$SRC/$bin" 2>/dev/null || true
  done
fi
for bin in hauksbee hauksbee-ci hauksbee-mcp; do
  [ -x "$SRC/$bin" ] || die "$SRC/$bin missing (build first, or drop --no-build)."
done

# ── prove the shape before packaging it ──────────────────────────────────────
# Each shape makes a licensing claim, and a claim nobody checks is a rumour.
#   default    MUST have the avr backend compiled in. If it does not, the
#              download labelled "AVR included, GPL-3.0" is neither, and the
#              biggest slice of the audience silently gets a tool that cannot
#              do the thing they came for.
#   permissive MUST NOT. If it does, GPL code ships under an Apache-2.0 label,
#              which is the serious direction of wrong.
#
# This runs on the --no-build path too, and must: --no-build packages whatever
# binaries happen to sit in target/release, which is exactly the case where the
# label and the binary are most likely to disagree.
#
# Ask the binary, not its symbol table. `nm` cannot answer this here: the
# workspace sets `strip = true` in [profile.release], so rustc strips at link
# time and a release binary of EITHER shape shows zero simavr symbols. That
# was measured, and it is why this check is behavioural. `hauksbee doctor`
# reports the backends the build can actually reach, and its avr line reads
# `builtin` only when libsimavr really linked in.
#
# hauksbee-ci has no doctor subcommand, and needs none: both binaries come out
# of a single cargo invocation with the identical feature set, so they cannot
# disagree about `avr`.
shape_doctor="$("$SRC/hauksbee" doctor 2>&1 || true)"
avr_line="$(printf '%s\n' "$shape_doctor" | grep -E '^avr[[:space:]]' || true)"
[ -n "$avr_line" ] || die "shape check could not read an avr line from \`hauksbee doctor\`. Refusing to package a bundle whose licensing claim is unverified. Doctor said:
$shape_doctor"
log "Shape check: $avr_line"
case "$SHAPE" in
  default)
    printf '%s' "$avr_line" | grep -qE '^avr[[:space:]]+builtin' \
      || die "shape 'default' has a binary with the avr backend NOT compiled in (doctor says: $avr_line). This bundle would be labelled GPL-3.0 and AVR-capable while being neither. Install libsimavr with scripts/install-sims.sh --avr, or point SIMAVR_INCLUDE_DIR/SIMAVR_LIB_DIR at an existing install."
    ;;
  permissive)
    printf '%s' "$avr_line" | grep -qE '^avr[[:space:]]+disabled' \
      || die "shape 'permissive' has a binary with the avr backend compiled IN (doctor says: $avr_line). GPL libsimavr would ship under an Apache-2.0 label. Fix the feature graph (docs/about/release-and-licensing.md section 2); never weaken this check."
    ;;
esac

NAME="hauksbee-${VERSION}-${TARGET}${NAME_SUFFIX}"
OUT_ABS="$(mkdir -p "$OUT" && cd "$OUT" && pwd)"
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/hauksbee-bundle.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT
ROOTDIR="$STAGE/$NAME"
mkdir -p "$ROOTDIR/bin"

log "Staging $NAME"
install -m 0755 "$SRC/hauksbee"     "$ROOTDIR/bin/hauksbee"
install -m 0755 "$SRC/hauksbee-ci"  "$ROOTDIR/bin/hauksbee-ci"
install -m 0755 "$SRC/hauksbee-mcp" "$ROOTDIR/bin/hauksbee-mcp"

# ── darwin codesigning + notarisation (same env contract as build-app.sh) ────
# The STAGED copies are signed, never the target/release originals, so the
# tarball's signatures cannot be invalidated by a later rebuild. Signing is
# optional and env-driven exactly like app/macos/build-app.sh: unsigned local
# builds keep working, release builds set the vars. Non-darwin targets are
# untouched.
#
# Notarisation, when credentials are present, submits a zip of the signed
# binaries via `notarytool submit --wait`. One difference from the app: a bare
# Mach-O cannot carry a stapled ticket (stapler wants a bundle / dmg / pkg),
# so there is nothing to staple here. The ticket lives on Apple's servers and
# Gatekeeper looks it up ONLINE the first time a quarantined binary runs. See
# app/macos/SIGNING.md, "Tarball binaries".
notarize_staged_binaries() {
  # Credential convention copied from build-app.sh: a notarytool keychain
  # profile, or the apple-id + team-id + app-specific-password triple.
  NOTARY_ARGS=()
  if [ -n "${HAUKSBEE_NOTARY_PROFILE:-}" ]; then
    NOTARY_ARGS=(--keychain-profile "$HAUKSBEE_NOTARY_PROFILE")
  elif [ -n "${HAUKSBEE_NOTARY_APPLE_ID:-}" ] && [ -n "${HAUKSBEE_NOTARY_TEAM_ID:-}" ] && [ -n "${HAUKSBEE_NOTARY_PASSWORD:-}" ]; then
    NOTARY_ARGS=(--apple-id "$HAUKSBEE_NOTARY_APPLE_ID" \
                 --team-id "$HAUKSBEE_NOTARY_TEAM_ID" \
                 --password "$HAUKSBEE_NOTARY_PASSWORD")
  fi
  if [ "${#NOTARY_ARGS[@]}" -eq 0 ]; then
    if [ "${HAUKSBEE_REQUIRE_NOTARIZATION:-0}" = "1" ]; then
      die "Darwin release shape requires notary credentials (set HAUKSBEE_NOTARY_PROFILE or the Apple ID credential triple)"
    fi
    info "No notary credentials set; binaries are signed but NOT notarised."
    info "Set HAUKSBEE_NOTARY_PROFILE (or the APPLE_ID/TEAM_ID/PASSWORD triple) to notarise."
    return 0
  fi
  log "Notarising the signed binaries (notarytool submit --wait)"
  NOTARY_ZIP="$STAGE/notary-bins.zip"
  ( cd "$ROOTDIR" && ditto -c -k bin "$NOTARY_ZIP" )
  xcrun notarytool submit "$NOTARY_ZIP" ${NOTARY_ARGS[@]+"${NOTARY_ARGS[@]}"} --wait
  ok "Notarised. Bare binaries take no staple; Gatekeeper checks the ticket online."
}

if [ "$(uname -s)" = "Darwin" ]; then
  if [ -n "${HAUKSBEE_SIGN_IDENTITY:-}" ]; then
    log "Codesigning staged binaries with: $HAUKSBEE_SIGN_IDENTITY"
    for bin in hauksbee hauksbee-ci hauksbee-mcp; do
      codesign --force --options runtime --timestamp \
        --sign "$HAUKSBEE_SIGN_IDENTITY" "$ROOTDIR/bin/$bin"
      codesign --verify --strict --verbose=2 "$ROOTDIR/bin/$bin"
    done
    ok "codesign --verify --strict: passed for hauksbee, hauksbee-ci, hauksbee-mcp"
    notarize_staged_binaries
  else
    if [ "${HAUKSBEE_REQUIRE_NOTARIZATION:-0}" = "1" ]; then
      die "Darwin release shape requires HAUKSBEE_SIGN_IDENTITY before notarisation"
    fi
    info "HAUKSBEE_SIGN_IDENTITY not set; darwin binaries ship UNSIGNED."
    info "Release builds must set it to a 'Developer ID Application' identity (see app/macos/SIGNING.md)."
  fi
fi

# Reference assets. Binaries embed the model db; this copy supports the layered
# ~/.hauksbee/models override and is handy documentation.
cp -R "$HAUKSBEE_ROOT/crates/hauksbee-models/db" "$ROOTDIR/db"
cp -R "$HAUKSBEE_ROOT/integrations" "$ROOTDIR/integrations"
# Ship only the scripts a bundle USER needs (the ones README-BUNDLE names,
# plus the helpers they source). The rest of scripts/ is maintainer tooling
# (release mirroring, benchmarking, demo capture) that has no business in a
# user tarball.
mkdir -p "$ROOTDIR/scripts"
for s in install.sh doctor.sh ci.sh install-sims.sh common.sh simavr-payload-provenance.sh simulator-provenance.py; do
  install -m 0755 "$HAUKSBEE_ROOT/scripts/$s" "$ROOTDIR/scripts/$s"
done
for s in required-simulator-versions.env renode-checksums.txt espressif-qemu-checksums.txt; do
  install -m 0644 "$HAUKSBEE_ROOT/scripts/$s" "$ROOTDIR/scripts/$s"
done
mkdir -p "$ROOTDIR/scripts/qemu-patches"
install -m 0644 \
  "$HAUKSBEE_ROOT/scripts/qemu-patches/esp32-gpio-register-state.patch" \
  "$ROOTDIR/scripts/qemu-patches/esp32-gpio-register-state.patch"
# Examples: ship the specs, boards and READMEs (skip any scratch dirs).
mkdir -p "$ROOTDIR/examples"
cp -R "$HAUKSBEE_ROOT/crates/hauksbee-ci/examples/." "$ROOTDIR/examples/ci-specs"
[ -d "$HAUKSBEE_ROOT/examples" ] && cp -R "$HAUKSBEE_ROOT/examples/." "$ROOTDIR/examples/"
# The AVR boot-gate demo belongs only in a bundle whose binary carries the avr
# backend. The permissive shape has none, so staging the fixture and rewriting
# the spec there would ship a headline example that cannot run. Leave both out
# instead; scripts/bundle-windows.ps1 already drops the spec from its zip.
if [ "$SHAPE" = default ]; then
  mkdir -p "$ROOTDIR/examples/firmware"
  install -m 0644 "$HAUKSBEE_ROOT/testdata/firmware/boot_gate_a/boot_gate.hex" \
    "$ROOTDIR/examples/firmware/boot_gate.hex"
  # The checkout-relative tracked spec points at testdata/. The bundle retains a
  # package-local copy so its documented example and runtime gate are self-contained.
  sed 's#firmware = "../../../testdata/firmware/boot_gate_a/boot_gate.hex"#firmware = "../firmware/boot_gate.hex"#' \
    "$HAUKSBEE_ROOT/crates/hauksbee-ci/examples/boot_gate_pass.toml" \
    > "$ROOTDIR/examples/ci-specs/boot_gate_pass.toml"
  perl -pi -e 's#hauksbee-ci run crates/hauksbee-ci/examples/boot_gate_pass\.toml#hauksbee-ci run examples/ci-specs/boot_gate_pass.toml#' \
    "$ROOTDIR/examples/ci-specs/boot_gate_pass.toml"
else
  # The checkout-relative spec rode in with the `cp -R` of the whole examples
  # directory above. Its firmware path escapes the tarball and no avr backend
  # exists to run it, so drop it rather than ship a spec that fails on arrival.
  rm -f "$ROOTDIR/examples/ci-specs/boot_gate_pass.toml"
fi
# Drop python bytecode caches so the bundle is reproducible.
find "$ROOTDIR" -name '__pycache__' -type d -prune -exec rm -rf {} + 2>/dev/null || true
find "$ROOTDIR" -name '*.pyc' -delete 2>/dev/null || true

# ── flagship-board gate ──────────────────────────────────────────────────────
# The flagship board is private and never enters a distributed source tree,
# but its hauksbee-ci example specs
# live in crates/hauksbee-ci/examples/ and would otherwise ride into every
# bundle with the rest of the examples. The three known specs are dropped by
# name; anything ELSE named after the board, anywhere in the staged tree, is a
# hard failure rather than a silent drop, because a new board-named file
# appearing in the staging set means the enumeration is stale and a human has
# to look.
rm -f "$ROOTDIR/examples/ci-specs/tarski_brownout.toml" \
      "$ROOTDIR/examples/ci-specs/tarski_brownout_repaired.toml" \
      "$ROOTDIR/examples/ci-specs/tarski_stage0_powerup.toml"

# ── corpus-dependent example specs ───────────────────────────────────────────
# These three specs point their `board =` at ../../../../board-corpus/, which
# only exists in a repo checkout (a large symlinked corpus that is not bundled).
# Staged into a tarball they are broken on arrival: the relative path escapes
# the bundle and resolves to nothing. Drop them at staging, same as the
# flagship specs above; they remain runnable from a checkout, which is the only
# place they ever worked.
rm -f "$ROOTDIR/examples/ci-specs/watchy_v15_display_res.toml" \
      "$ROOTDIR/examples/ci-specs/watchy_v15_display_res_undriven.toml" \
      "$ROOTDIR/examples/ci-specs/pic_programmer_schematic.toml" \
      "$ROOTDIR/examples/ci-specs/olimex_wifi_burst_transient.toml"
BOARD_LEAK="$(find "$ROOTDIR" -iname '*tarski*' -print 2>/dev/null || true)"
[ -z "$BOARD_LEAK" ] || die "flagship board data staged into the bundle; refusing to package:
$BOARD_LEAK
Remove it from the staging inputs (and extend the drop list above if it is a new legitimate path)."

printf '%s\n' "$VERSION" > "$ROOTDIR/VERSION"
GIT_SHA="$(cd "$HAUKSBEE_ROOT" && git rev-parse HEAD 2>/dev/null || echo unknown)"
GIT_SHA_SHORT="${GIT_SHA:0:7}"

# hauksbee's own terms travel with every redistribution (Apache-2.0 §4).
cp "$HAUKSBEE_ROOT/LICENSE" "$ROOTDIR/LICENSE"
cp "$HAUKSBEE_ROOT/NOTICE"  "$ROOTDIR/NOTICE"
[ -s "$HAUKSBEE_ROOT/licenses/evalexpr-MIT.txt" ] \
  || die "licenses/evalexpr-MIT.txt missing; every binary must retain evalexpr's MIT notice"
cp "$HAUKSBEE_ROOT/licenses/evalexpr-MIT.txt" "$ROOTDIR/LICENSE-EVALEXPR-MIT.txt"

# ── LICENSE-BINARY.txt: what THIS build is licensed under ────────────────────
#
# The two shapes have genuinely different answers, and a download that does not
# say which one it is puts the obligation-guessing on the person installing it.
# The immutable simavr commit was resolved before the build so build.rs and the
# corresponding-source notice are necessarily bound to the same revision.
if [ "$SHAPE" = default ]; then
  doctor_avr="$("$ROOTDIR/bin/hauksbee" doctor --backends | awk -F '\t' '$1 == "avr" { print; exit }')"
  [[ "$doctor_avr" == *"source commit $SIMAVR_COMMIT"* ]] \
    || die "default binaries do not attest simavr commit $SIMAVR_COMMIT (doctor says: ${doctor_avr:-no avr row}); refusing to write a false corresponding-source notice"
fi
PERMISSIVE_NAME="hauksbee-${VERSION}-${TARGET}-permissive.tar.gz"
DEFAULT_NAME="hauksbee-${VERSION}-${TARGET}.tar.gz"

if [ "$SHAPE" = default ]; then
  # GPL-3.0 §4 wants a copy of the licence alongside the binary, not just a
  # URL. The text is vendored in the repo (licenses/gpl-3.0.txt) so packaging
  # never depends on the network; a missing copy is a broken checkout, not a
  # downgrade to a link.
  GPL_NOTE="  Full GPL-3.0 text: LICENSE-GPL-3.0.txt, next to this file."
  [ -s "$HAUKSBEE_ROOT/licenses/gpl-3.0.txt" ] \
    || die "licenses/gpl-3.0.txt missing; the default shape must enclose the GPL-3.0 text (GPL-3.0 section 4). Restore it from https://www.gnu.org/licenses/gpl-3.0.txt"
  cp "$HAUKSBEE_ROOT/licenses/gpl-3.0.txt" "$ROOTDIR/LICENSE-GPL-3.0.txt"

  cat > "$ROOTDIR/LICENSE-BINARY.txt" <<EOF
hauksbee ${VERSION} (${TARGET}) - licence of THIS binary
===========================================================

THE BINARIES IN bin/ ARE LICENSED TO YOU UNDER GPL-3.0.

hauksbee's own source code is Apache-2.0 (see LICENSE and NOTICE in this
bundle). This build enables the optional \`avr\` co-simulation backend, which
STATICALLY LINKS libsimavr (GPL-3.0). Statically linking GPL-3.0 code makes the
resulting executable a combined work, so these binaries are distributed under
GPL-3.0. The source licence does not change; the binary licence does.

WHAT THIS MEANS FOR YOU
  Running it, on any board, for any purpose, commercial included: no
  obligations at all. GPL-3.0 constrains distribution, not use.
  Redistributing these binaries, or shipping them inside a product: GPL-3.0
  applies to you, including the obligation to offer the corresponding source.

CORRESPONDING SOURCE (GPL-3.0 section 6)
  Complete, checksummed Corresponding Source for Hauksbee, every locked Cargo
  dependency compiled into these binaries, and simavr is the same-release asset:
    hauksbee-${VERSION}-source.tar.gz
  hauksbee (Apache-2.0), at the exact commit this was built from:
    https://github.com/hauksbee-dev/hauksbee
    commit ${GIT_SHA}
  simavr (GPL-3.0), at the immutable commit this build links:
    https://github.com/buserror/simavr
    commit ${SIMAVR_COMMIT}
  The build recipe, both scripts included in this bundle:
    scripts/install-sims.sh --avr     builds and installs libsimavr
    scripts/bundle.sh --shape default rebuilds this exact bundle
  Confirm for yourself that the avr backend really is linked in:
    bin/hauksbee doctor      the avr line reads \`builtin\`
${GPL_NOTE}

IF YOU CANNOT TAKE GPL CODE
  Take ${PERMISSIVE_NAME}
  from the same release. It is Apache-2.0: the same tool built without the avr
  backend, so no libsimavr is linked and no GPL code is present. It cannot do
  AVR / ATmega co-simulation; Renode (STM32, nRF52, RP2040, RISC-V) and the
  Espressif QEMU fork (ESP32 family) are still there, and both run as separate
  processes reached over TCP, which imposes no link-time licence obligation.
EOF
else
  cat > "$ROOTDIR/LICENSE-BINARY.txt" <<EOF
hauksbee ${VERSION} (${TARGET}, permissive) - licence of THIS binary
=======================================================================

THE BINARIES IN bin/ ARE LICENSED TO YOU UNDER APACHE-2.0.

Built with \`--no-default-features --features renode,qemu\`: no \`avr\` backend,
no libsimavr, no GPL code linked or contained. hauksbee's own terms are in
LICENSE, and NOTICE is the attribution that must travel with any
redistribution (Apache-2.0 section 4). This is the shape to embed, repackage,
or ship inside a product.

Verified, not asserted, and you can re-run the verification yourself:

    bin/hauksbee doctor

The avr line must read \`disabled  not in this build\`. That answer is decided at
compile time, so it cannot be true of a binary that linked libsimavr. The build
refuses to package this shape unless it reads that way, and the release
workflow re-checks the extracted tarball before publishing. The build also
points simavr discovery at a nonexistent path, so a feature-graph regression
that dragged \`avr\` back in would abort before any GPL code could be linked.

WHAT IS NOT IN THIS BUILD
  AVR / ATmega co-simulation. Renode (STM32, nRF52, RP2040, RISC-V) and the
  Espressif QEMU fork (ESP32 family) are present and unaffected; they run as
  separate processes reached over TCP, so they impose no link-time obligation.

  If you need AVR, either take ${DEFAULT_NAME}
  from the same release (a GPL-3.0 binary, labelled as such), or build from
  source and run \`scripts/install-sims.sh --avr\`, which builds simavr on your
  own machine, making the combination yours rather than something anyone
  distributed to you.

Source: https://github.com/hauksbee-dev/hauksbee  commit ${GIT_SHA}
EOF
fi

if [ "$SHAPE" = default ]; then
  SHAPE_LINE="shape: default (avr + renode + qemu). This BINARY is GPL-3.0 because
the avr backend statically links libsimavr. Source stays Apache-2.0.
Read LICENSE-BINARY.txt; the -permissive download is the GPL-free one."
else
  SHAPE_LINE="shape: permissive (renode + qemu, no avr, no libsimavr). This binary
is Apache-2.0 and carries no GPL code. Read LICENSE-BINARY.txt;
the download without the -permissive suffix adds AVR co-sim and is GPL-3.0."
fi

# The examples note has to match what was actually staged: the permissive shape
# packages no AVR fixture and no boot_gate_pass.toml, so promising a runnable
# self-contained example there would be a lie the user discovers on first run.
if [ "$SHAPE" = default ]; then
  EXAMPLES_NOTE="NOTE: boot_gate_pass.toml and its tiny AVR fixture are package-local and
self-contained. Other firmware-bearing specs (blinky, boot_gate_fail, and the
lm75 thermostat variants) still reference firmware in the repository's large
testdata tree; run those from a checkout. They remain canonical examples to copy."
else
  EXAMPLES_NOTE="NOTE: this download has no avr backend, so its AVR examples are absent rather
than broken: boot_gate_pass.toml and its fixture are not packaged here. To run
the AVR specs (boot_gate_pass, boot_gate_fail, blinky, and the lm75 thermostat
variants), take a repository checkout, build simavr with
scripts/install-sims.sh --avr, and build hauksbee from source. They remain
canonical examples to copy."
fi

cat > "$ROOTDIR/README-BUNDLE.txt" <<EOF
hauksbee ${VERSION} (${TARGET})
built from git ${GIT_SHA_SHORT}
${SHAPE_LINE}

A universal PCB emulator: bind a board, bring it to life, assert it in CI.

CONTENTS
  bin/hauksbee       the CLI (run / to-code / from-code / check-code)
  bin/hauksbee-ci    the headless CI runner (TOML specs, JUnit, GH annotations)
  bin/hauksbee-mcp   the MCP server (hauksbee as structured tools over stdio)
  db/               reference model database (the binaries already embed it;
                    drop extra parts here or in ~/.hauksbee/models to extend)
  examples/         runnable demos: board-as-code + hauksbee-ci specs + sessions
  integrations/     GitHub Action, KiCad plugin, pre-commit hook
  scripts/          install.sh, doctor.sh, ci.sh
  LICENSE, NOTICE   hauksbee's Apache-2.0 source terms and attribution
  LICENSE-BINARY.txt what this particular download is licensed under, and why

INSTALL
  Copy the binaries onto your PATH:
    install -m 0755 bin/hauksbee bin/hauksbee-ci bin/hauksbee-mcp /usr/local/bin/
  or run the bundled installer (no sudo, installs into ~/.local/bin):
    PREFIX=\$HOME/.local scripts/install.sh --no-build --symlink

VERIFY (no external files needed - the static checks run on the bundled board)
  hauksbee run examples/ci-specs/boards/watchy.kicad_pcb --report
  hauksbee run examples/ci-specs/boards/watchy.kicad_pcb --drc
  hauksbee-ci run examples/ci-specs/watchy.toml

The binaries are self-contained (model db compiled in). Optional firmware
backends (qemu, renode) are detected at runtime; run scripts/doctor.sh to see
what is present.

${EXAMPLES_NOTE}
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
