#!/usr/bin/env bash
# install-sims.sh - install or verify the external MCU simulator backends.
#
# hauksbee's AVR co-sim links libsimavr from the system. simavr is GPL-3.0 and
# this repo is Apache-2.0, so simavr is NOT vendored — it is linked from the system by
# deliberate choice; `--avr` below builds and installs it for you. The Renode
# and Espressif QEMU backends require externally installed binaries. This script
# installs any of them into the exact locations hauksbee auto-discovers, from
# release versions PINNED in this file, verifying every download against a
# recorded sha256 and checking the result after install.
#
# Usage:
#   scripts/install-sims.sh [--renode-only | --qemu-only | --avr]
#                           [--prefix DIR] [--check] [--help]
#
# Flags:
#   (none)           Install both Renode and Espressif QEMU (AVR is opt-in via
#                    --avr because simavr is GPL-3.0; see below).
#   --renode-only    Install Renode only.
#   --qemu-only      Install Espressif QEMU only.
#   --avr            Install libsimavr (AVR co-sim) only. Installs libelf, then
#                    clones + builds + installs simavr from source into the
#                    prefix hauksbee-mcu's build.rs links against.
#   --prefix DIR     Install simavr under DIR instead of the platform default
#                    (/opt/homebrew on arm64 macOS, /usr/local elsewhere). Only
#                    affects --avr; pair with SIMAVR_INCLUDE_DIR/SIMAVR_LIB_DIR
#                    when building.
#   --check          Do NOT install anything. Report for each backend whether
#                    hauksbee will discover it and at which path. Exit 0 if
#                    all requested backends are discoverable, else 1.
#   --require-pinned Release-evidence mode: reverify the repository-pinned
#                    archives, extract fresh non-overlaid trees, and refuse
#                    ordinary installed binaries as provenance.
#   --emit-required-paths
#                    With --require-pinned, print the exact verified executable
#                    paths for the release integration runner to bind.
#   --help           Show this help.
#
# Install targets:
#   Renode    -> ~/renode-portable/Renode.app  (macOS)
#              -> ~/renode-portable/renode      (Linux)
#   Esp QEMU  -> ~/.espressif/tools/qemu-xtensa/<ver>/qemu/bin/
#              -> ~/.espressif/tools/qemu-riscv32/<ver>/qemu/bin/
#             (or via idf_tools.py if an ESP-IDF checkout is found)
#   simavr    -> <prefix>/lib/libsimavr.a  +  <prefix>/include/simavr/
#
# Environment overrides (these are hauksbee's env overrides, not just this
# script's): HAUKSBEE_RENODE, HAUKSBEE_QEMU_XTENSA, HAUKSBEE_QEMU_RISCV32.
# Required mode caches only archives under HAUKSBEE_SIM_CACHE_DIR (default:
# ~/.cache/hauksbee/simulator-archives) and uses RUNNER_TEMP for fresh trees.
#
# Idempotent: if a backend is already discoverable, the download is skipped.
# Safe: ordinary installs write to ~/renode-portable and ~/.espressif/tools;
# required runs use the cache plus a fresh runner-owned temporary root.
set -euo pipefail
# shellcheck source=scripts/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; }

# Beside this script, so a release bundle that ships scripts/ carries the hashes
# with the installer that needs them. Resolved here rather than inside
# verify_asset: at top level BASH_SOURCE unambiguously names this file.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PINNED_RENODE_CHECKSUMS="$SCRIPT_DIR/renode-checksums.txt"
PINNED_QEMU_CHECKSUMS="$SCRIPT_DIR/espressif-qemu-checksums.txt"
RENODE_CHECKSUMS="${RENODE_CHECKSUMS:-$PINNED_RENODE_CHECKSUMS}"
QEMU_CHECKSUMS="${QEMU_CHECKSUMS:-$PINNED_QEMU_CHECKSUMS}"
VERSIONS_FILE="$SCRIPT_DIR/required-simulator-versions.env"
PROVENANCE_HELPER="$SCRIPT_DIR/simulator-provenance.py"
[ -f "$VERSIONS_FILE" ] || die "required simulator version manifest missing: $VERSIONS_FILE"
# shellcheck source=scripts/required-simulator-versions.env
source "$VERSIONS_FILE"

# ── defaults ────────────────────────────────────────────────────────────────
DO_RENODE=1
DO_QEMU=1
DO_AVR=0          # opt-in: simavr is GPL-3.0, so it is not installed by default
CHECK_ONLY=0
REQUIRE_PINNED=0
EMIT_REQUIRED_PATHS=0
PREFIX_OVERRIDE=""

while [ $# -gt 0 ]; do
  case "$1" in
    --renode-only)  DO_QEMU=0; DO_AVR=0; shift ;;
    --qemu-only)    DO_RENODE=0; DO_AVR=0; shift ;;
    --avr|--avr-only) DO_RENODE=0; DO_QEMU=0; DO_AVR=1; shift ;;
    --prefix)       PREFIX_OVERRIDE="${2:?--prefix needs a directory}"; shift 2 ;;
    --prefix=*)     PREFIX_OVERRIDE="${1#*=}"; shift ;;
    --check)        CHECK_ONLY=1; shift ;;
    --require-pinned) REQUIRE_PINNED=1; shift ;;
    --emit-required-paths) EMIT_REQUIRED_PATHS=1; shift ;;
    -h|--help)      usage; exit 0 ;;
    *) die "unknown argument '$1' (try --help)" ;;
  esac
done

if [ "$EMIT_REQUIRED_PATHS" -eq 1 ] && [ "$REQUIRE_PINNED" -ne 1 ]; then
  die "--emit-required-paths requires --require-pinned"
fi
if [ "$REQUIRE_PINNED" -eq 1 ] \
    && { [ "$RENODE_CHECKSUMS" != "$PINNED_RENODE_CHECKSUMS" ] \
      || [ "$QEMU_CHECKSUMS" != "$PINNED_QEMU_CHECKSUMS" ]; }; then
  die "--require-pinned only trusts checksum manifests shipped beside install-sims.sh"
fi

# ── platform detection ───────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin) PLATFORM=darwin ;;
  Linux)  PLATFORM=linux ;;
  *)
    err "Unsupported OS: $OS"
    err "Windows users: see docs/cosim/SIMULATORS.md for manual install instructions."
    exit 1
    ;;
esac

case "$ARCH" in
  arm64|aarch64) ARCH_NORM=arm64 ;;
  x86_64|amd64)  ARCH_NORM=x86_64 ;;
  *)
    err "Unsupported architecture: $ARCH"
    exit 1
    ;;
esac

# ── pinned versions ──────────────────────────────────────────────────────────
# Renode and Espressif QEMU come from required-simulator-versions.env above.
# Bumping QEMU_VERSION also requires replacing the checksum manifest and keeping
# it in step with FALLBACK_TAG in crates/hauksbee-mcu/src/qemu/install.rs and
# ESP_QEMU_TAG in docker/Dockerfile.full.
# Pinned simavr release tag (buserror/simavr). Bumping this is a deliberate,
# reviewed change — see the licensing note in the header.
SIMAVR_TAG="v1.8"

# ── helpers ──────────────────────────────────────────────────────────────────

# Resolve the Renode release tag (e.g. "1.16.1"). PINNED, not resolved.
#
# This used to ask GitHub for `latest` at install time and interpolate the
# answer straight into the download URL, which meant nothing in this repository
# governed what actually got downloaded and executed, and there was no version
# to hold a checksum against. A `latest` that moves is a `latest` nobody
# reviewed. Bumping this is a deliberate change: update the version, download
# the assets, and record their hashes in renode-checksums.txt beside this
# script, the same discipline SIMAVR_TAG already gets.
resolve_renode_version() {
  printf '%s' "$RENODE_VERSION"
}

# Verify a downloaded asset against the pinned hash, and REFUSE if there is no
# hash for it.
#
# Failing closed matters here more than usual: this install is reachable from a
# browser button, and the macOS path strips the Gatekeeper quarantine flag from
# what it unpacks. Stripping the last OS-level guard from a download whose only
# provenance check was TLS is not a trade worth making silently, so an asset
# with no recorded hash stops the install and tells the user to install
# manually.
#
# $1 file  $2 asset name  $3 checksum file  $4 backend label  $5 pinned version
# $6 what to tell the user to do by hand instead.
verify_asset() {
  local file="$1" name="$2" sums="$3" what="$4" version="$5" manual="$6"
  [ -f "$sums" ] || die "no checksum file at $sums; refusing to install an unverified $what. $manual"
  local want
  want="$(awk -v n="$name" '$2 == n { print $1 }' "$sums" | head -1)"
  [ -n "$want" ] || die "no recorded checksum for $name (pinned version $version). Refusing to install it unverified: add its hash to $sums after checking it. $manual"
  local got
  if command -v shasum >/dev/null 2>&1; then
    got="$(shasum -a 256 "$file" | cut -d" " -f1)"
  elif command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$file" | cut -d" " -f1)"
  else
    die "neither shasum nor sha256sum is available, so the download cannot be verified. $manual"
  fi
  [ "$got" = "$want" ] || die "checksum mismatch for $name: expected $want, got $got. The download is not what we pinned; do not use it."
  log "$what: checksum verified"
}

verify_renode_asset() {
  verify_asset "$1" "$2" "$RENODE_CHECKSUMS" "Renode" "$RENODE_VERSION" \
    "Install it manually from renode.io."
}

verify_qemu_asset() {
  verify_asset "$1" "$2" "$QEMU_CHECKSUMS" "Espressif QEMU" "$QEMU_VERSION" \
    "Install it manually from https://github.com/espressif/qemu/releases/tag/${QEMU_VERSION}."
}

sha256_file() {
  local file="$1"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$file" | cut -d" " -f1
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$file" | cut -d" " -f1
  else
    return 1
  fi
}

# Resolve the Espressif QEMU release tag. PINNED, not resolved.
#
# This used to ask GitHub for `latest` at install time and interpolate the
# answer into the download URL, so what got downloaded and executed was decided
# by whatever upstream had tagged that morning, and no hash could be held
# against it. Same reasoning as resolve_renode_version: a `latest` that moves is
# a `latest` nobody reviewed.
resolve_qemu_tag() {
  printf '%s' "$QEMU_VERSION"
}

# Convert an espressif/qemu release tag (e.g. "esp-develop-9.2.2-20260417") to
# the version form used inside asset names and by idf_tools directory layout
# ("esp_develop_9.2.2_20260417").
qemu_tag_to_dir_ver() {
  printf '%s' "$1" | tr '-' '_'
}

# The release asset name for a tool+platform.
#
# This used to be resolved by listing the release's assets over the API, because
# upstream has changed both the version separator (hyphens -> underscores inside
# the version) and the compression (.tar.bz2 -> .tar.xz) across releases, so a
# name constructed against a FLOATING tag 404s and the "archive" we then extract
# is a GitHub error page. With the tag pinned the name is fixed and known good,
# and espressif-qemu-checksums.txt is the backstop: a name this builds that the
# release does not publish has no recorded hash, so the install stops rather
# than unpacking whatever came back.
qemu_asset_name() {
  local dir_ver="$1" tool_name="$2" os_arch_suffix="$3"
  printf '%s' "${tool_name}-softmmu-${dir_ver}-${os_arch_suffix}.tar.xz"
}

# ── AVR / simavr prefix + discovery ──────────────────────────────────────────
#
# hauksbee-mcu/build.rs links a SYSTEM libsimavr. Its default prefix is
# /opt/homebrew on arm64 macOS and /usr/local everywhere else; --prefix overrides
# it (pair with SIMAVR_INCLUDE_DIR / SIMAVR_LIB_DIR at build time). Keep this in
# lockstep with build.rs's default_prefix.
avr_prefix() {
  if [ -n "$PREFIX_OVERRIDE" ]; then printf '%s' "$PREFIX_OVERRIDE"; return 0; fi
  if [ "$PLATFORM" = darwin ] && [ "$ARCH_NORM" = arm64 ]; then
    printf '%s' "/opt/homebrew"
  else
    printf '%s' "/usr/local"
  fi
}

# Where Homebrew keeps libelf/zlib, so simavr's Makefile can find them while
# building — independent of where we install simavr itself (which --prefix may
# redirect to a scratch dir).
brew_prefix() {
  if [ "$ARCH_NORM" = arm64 ]; then printf '%s' "/opt/homebrew"; else printf '%s' "/usr/local"; fi
}

# Returns 0 and prints the prefix if a usable simavr is installed: BOTH the
# header build.rs includes and the static archive it links must exist. Mirrors
# the preflight in crates/hauksbee-mcu/build.rs.
find_avr_prefix() {
  local prefix; prefix="$(avr_prefix)"
  if [ -f "$prefix/include/simavr/sim_avr.h" ] && [ -f "$prefix/lib/libsimavr.a" ]; then
    printf '%s' "$prefix"
    return 0
  fi
  return 1
}

# ── Renode discovery (mirrors find_renode() in renode/process.rs) ────────────
#
# Returns 0 and prints the binary path to stdout if Renode is discoverable.
# Returns 1 if nothing is found (prints nothing).
find_renode_bin() {
  # 1. HAUKSBEE_RENODE env
  if [ -n "${HAUKSBEE_RENODE:-}" ]; then
    if [ -x "$HAUKSBEE_RENODE" ]; then
      printf '%s' "$HAUKSBEE_RENODE"
      return 0
    fi
    return 1
  fi

  # 2. `renode` on PATH
  if command -v renode >/dev/null 2>&1; then
    local path_renode
    path_renode="$(command -v renode)"
    printf '%s' "$path_renode"
    return 0
  fi

  # 3. Conventional portable locations
  local candidate
  for candidate in \
    "$HOME/renode-portable/Renode.app/Contents/MacOS/renode" \
    "$HOME/renode-portable/renode" \
    "$HOME/renode_portable/renode"; do
    if [ -x "$candidate" ]; then
      printf '%s' "$candidate"
      return 0
    fi
  done

  return 1
}

# ── Espressif QEMU discovery (mirrors find_qemu() in qemu/process.rs) ────────
#
# $1 = binary name (qemu-system-xtensa | qemu-system-riscv32)
# $2 = env-override var name (HAUKSBEE_QEMU_XTENSA | HAUKSBEE_QEMU_RISCV32)
# Returns 0 and prints path if the Espressif fork is discoverable, else 1.
find_qemu_bin() {
  local name="$1" envvar="$2"

  # 1. Per-arch env override
  local envval
  envval="${!envvar:-}"
  if [ -n "$envval" ]; then
    if [ -x "$envval" ] && is_esp_qemu_fork "$envval"; then
      printf '%s' "$envval"
      return 0
    fi
    return 1
  fi

  local candidates=()

  # 2. HAUKSBEE_QEMU_DIR generic dir
  if [ -n "${HAUKSBEE_QEMU_DIR:-}" ] && [ -x "$HAUKSBEE_QEMU_DIR/bin/$name" ]; then
    candidates+=("$HAUKSBEE_QEMU_DIR/bin/$name")
  fi

  # 3. Conventional unpacked location (current name first, legacy as fallback).
  candidates+=("$HOME/.hauksbee-qemu-esp/qemu/bin/$name")
  candidates+=("$HOME/.galvani-qemu-esp/qemu/bin/$name")

  # 4. esp-idf idf_tools install: glob ~/.espressif/tools/qemu-*/<ver>/qemu/bin/
  if [ -d "$HOME/.espressif/tools" ]; then
    for tool_dir in "$HOME/.espressif/tools"/qemu-*/; do
      [ -d "$tool_dir" ] || continue
      for ver_dir in "$tool_dir"*/; do
        [ -d "$ver_dir" ] || continue
        candidates+=("${ver_dir}qemu/bin/$name")
      done
    done
  fi

  for c in "${candidates[@]}"; do
    if [ -x "$c" ] && is_esp_qemu_fork "$c"; then
      printf '%s' "$c"
      return 0
    fi
  done

  # 5. PATH, only if it is the Espressif fork
  if command -v "$name" >/dev/null 2>&1; then
    local path_bin
    path_bin="$(command -v "$name")"
    if is_esp_qemu_fork "$path_bin"; then
      printf '%s' "$path_bin"
      return 0
    fi
  fi

  return 1
}

# Verify a qemu-system-* binary is the Espressif fork by checking for esp32 in
# its machine list. Mirrors is_esp_fork() in qemu/process.rs.
is_esp_qemu_fork() {
  local bin="$1"
  [ -x "$bin" ] || return 1
  "$bin" -machine help 2>/dev/null | grep -qi 'esp32'
}

# ── find ESP-IDF idf_tools.py ─────────────────────────────────────────────────
find_idf_tools_py() {
  # Check canonical locations in order.
  for candidate in \
    "${IDF_PATH:-__none__}/tools/idf_tools.py" \
    "$HOME/esp/esp-idf/tools/idf_tools.py" \
    "$HOME/.espressif/idf_tools.py"; do
    [ -f "$candidate" ] && printf '%s' "$candidate" && return 0
  done
  return 1
}

# ── temp dir management ───────────────────────────────────────────────────────
TMPDIR_CREATED=""
make_tmpdir() {
  TMPDIR_CREATED="$(mktemp -d)"
  printf '%s' "$TMPDIR_CREATED"
}
cleanup_tmpdir() {
  if [ -n "$TMPDIR_CREATED" ] && [ -d "$TMPDIR_CREATED" ]; then
    # rm is blocked in this environment; use trash if available, else leave the
    # temp files (they are in /tmp and will be reaped by the OS).
    if have trash; then
      trash "$TMPDIR_CREATED" 2>/dev/null || true
    else
      # Fall back to rm if it works; otherwise leave the temp dir.
      rm -rf "$TMPDIR_CREATED" 2>/dev/null || true
    fi
  fi
}
REQUIRED_MOUNTPOINT=""
cleanup_required_mount() {
  if [ -n "${REQUIRED_MOUNTPOINT:-}" ]; then
    if hdiutil detach "$REQUIRED_MOUNTPOINT" -quiet 2>/dev/null \
      || hdiutil detach "$REQUIRED_MOUNTPOINT" -force -quiet 2>/dev/null; then
      REQUIRED_MOUNTPOINT=""
    else
      warn "Could not detach mounted simulator image: $REQUIRED_MOUNTPOINT"
      return 1
    fi
  fi
}
cleanup_installer() {
  local original_status="${1:-$?}" cleanup_status=0
  cleanup_required_mount || cleanup_status=$?
  cleanup_tmpdir
  if [ "$original_status" -ne 0 ]; then
    return "$original_status"
  fi
  return "$cleanup_status"
}
handle_installer_exit() {
  local status=$?
  trap - EXIT
  cleanup_installer "$status"
  exit $?
}
trap handle_installer_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# Required integration evidence is rooted in the repository's checksum
# manifests, never in an installed binary or a sidecar that a local process can
# author.  Cache only the upstream archive, reverify it on every invocation,
# and extract into a new empty tree.  The caller binds Cargo to the exact paths
# emitted from that tree, so ordinary PATH discovery remains permissive without
# being usable as release evidence.
REQUIRED_RUN_ROOT=""
VERIFIED_ASSET_PATH=""
REQUIRED_RENODE_PATH=""
REQUIRED_QEMU_XTENSA_PATH=""
REQUIRED_QEMU_RISCV32_PATH=""

expected_asset_sha() {
  local name="$1" sums="$2"
  awk -v n="$name" '$2 == n { print $1; exit }' "$sums"
}

asset_matches_sha() {
  local file="$1" want="$2" got
  got="$(sha256_file "$file" 2>/dev/null || true)"
  [ -n "$got" ] && [ "$got" = "$want" ]
}

obtain_verified_asset() {
  local asset="$1" sums="$2" what="$3" version="$4" url="$5"
  local want cache_dir cached partial rejected archive_dir staged
  want="$(expected_asset_sha "$asset" "$sums")"
  [ -n "$want" ] || die "no recorded checksum for $asset (pinned version $version); refusing unverified $what"
  cache_dir="${HAUKSBEE_SIM_CACHE_DIR:-$HOME/.cache/hauksbee/simulator-archives}"
  mkdir -p "$cache_dir"
  cached="$cache_dir/${want}-${asset}"
  if [ -f "$cached" ] && ! asset_matches_sha "$cached" "$want"; then
    rejected="${cached}.rejected.$$"
    mv "$cached" "$rejected"
    warn "$what: moved corrupt cached archive to $rejected"
  fi
  if [ ! -f "$cached" ]; then
    partial="$(mktemp "$cache_dir/.${asset}.partial.XXXXXX")"
    if [ -n "${HAUKSBEE_SIM_ASSET_DIR:-}" ]; then
      [ -f "$HAUKSBEE_SIM_ASSET_DIR/$asset" ] \
        || die "pinned asset not found: $HAUKSBEE_SIM_ASSET_DIR/$asset"
      cp "$HAUKSBEE_SIM_ASSET_DIR/$asset" "$partial"
    else
      curl --fail --location --progress-bar --output "$partial" "$url" \
        || die "download failed for pinned asset: $url"
    fi
    verify_asset "$partial" "$asset" "$sums" "$what" "$version" "Restore the pinned archive."
    mv "$partial" "$cached"
  fi
  # Reverify even cache hits immediately before extraction.
  verify_asset "$cached" "$asset" "$sums" "$what" "$version" "Restore the pinned archive."
  # Snapshot and reverify the exact bytes beneath the private runner-owned
  # root. A concurrent cache replacement cannot change what validation and
  # extraction subsequently consume.
  archive_dir="$REQUIRED_RUN_ROOT/archives"
  mkdir -p "$archive_dir"
  partial="$(mktemp "$archive_dir/.${asset}.partial.XXXXXX")"
  cp "$cached" "$partial" || die "could not snapshot verified $what asset"
  verify_asset "$partial" "$asset" "$sums" "$what" "$version" "Restore the pinned archive."
  chmod 0400 "$partial"
  staged="$archive_dir/$asset"
  mv "$partial" "$staged"
  VERIFIED_ASSET_PATH="$staged"
}

validate_archive_members() {
  local archive="$1" strip_components="${2:-0}"
  [ -f "$PROVENANCE_HELPER" ] \
    || die "simulator provenance helper missing: $PROVENANCE_HELPER"
  have python3 || die "python3 is required to validate pinned simulator archives"
  python3 "$PROVENANCE_HELPER" archive --strip-components "$strip_components" "$archive" \
    || die "archive contains an unsafe path or link: $archive"
}

canonical_required_backend() {
  local candidate="$1" resolved
  resolved="$(python3 "$PROVENANCE_HELPER" path "$REQUIRED_RUN_ROOT" "$candidate")" \
    || die "required backend escapes or is not executable beneath $REQUIRED_RUN_ROOT: $candidate"
  printf '%s' "$resolved"
}

prepare_required_renode() {
  local asset url mountpoint bin ver
  case "$PLATFORM-$ARCH_NORM" in
    darwin-arm64) asset="renode-${RENODE_VERSION}-dotnet.osx-arm64-portable.dmg" ;;
    darwin-x86_64) asset="renode_${RENODE_VERSION}.dmg" ;;
    linux-arm64) asset="renode-${RENODE_VERSION}.linux-arm64-portable-dotnet.tar.gz" ;;
    linux-x86_64) asset="renode-${RENODE_VERSION}.linux-portable-dotnet.tar.gz" ;;
  esac
  url="https://github.com/renode/renode/releases/download/v${RENODE_VERSION}/${asset}"
  obtain_verified_asset "$asset" "$RENODE_CHECKSUMS" "Renode" "$RENODE_VERSION" "$url"
  if [ "$PLATFORM" = darwin ]; then
    mountpoint="$REQUIRED_RUN_ROOT/renode-mount"
    mkdir -p "$mountpoint" "$REQUIRED_RUN_ROOT/renode"
    REQUIRED_MOUNTPOINT="$mountpoint"
    hdiutil attach "$VERIFIED_ASSET_PATH" -mountpoint "$mountpoint" -nobrowse -quiet \
      || die "hdiutil attach failed for verified Renode asset"
    ditto "$mountpoint/Renode.app" "$REQUIRED_RUN_ROOT/renode/Renode.app" \
      || die "ditto copy failed"
    hdiutil detach "$mountpoint" -quiet || die "could not detach verified Renode asset"
    REQUIRED_MOUNTPOINT=""
    if [ "$ARCH_NORM" = x86_64 ]; then
      bin="$REQUIRED_RUN_ROOT/renode/Renode.app/Contents/MacOS/macos_run.command"
    else
      bin="$REQUIRED_RUN_ROOT/renode/Renode.app/Contents/MacOS/renode"
    fi
  else
    mkdir -p "$REQUIRED_RUN_ROOT/renode"
    validate_archive_members "$VERIFIED_ASSET_PATH" 1
    tar xzf "$VERIFIED_ASSET_PATH" -C "$REQUIRED_RUN_ROOT/renode" --strip-components=1 \
      || die "tar extraction failed for verified Renode asset"
    bin="$REQUIRED_RUN_ROOT/renode/renode"
  fi
  bin="$(canonical_required_backend "$bin")"
  ver="$("$bin" --version 2>/dev/null | head -1 || true)"
  case "$ver" in "Renode v${RENODE_VERSION}"|"Renode v${RENODE_VERSION}."*|"Renode, version ${RENODE_VERSION}"*) ;; *) die "verified Renode payload reports the wrong version: $ver" ;; esac
  REQUIRED_RENODE_PATH="$bin"
}

prepare_required_qemu() {
  local dir_ver suffix arch tool bin asset url dest ver expected
  dir_ver="$(qemu_tag_to_dir_ver "$QEMU_VERSION")"
  expected="$dir_ver"
  case "$PLATFORM-$ARCH_NORM" in
    darwin-arm64) suffix="aarch64-apple-darwin" ;;
    darwin-x86_64) suffix="x86_64-apple-darwin" ;;
    linux-arm64) suffix="aarch64-linux-gnu" ;;
    linux-x86_64) suffix="x86_64-linux-gnu" ;;
  esac
  for arch in xtensa riscv32; do
    tool="qemu-$arch"
    bin="qemu-system-$arch"
    asset="$(qemu_asset_name "$dir_ver" "$tool" "$suffix")"
    url="https://github.com/espressif/qemu/releases/download/${QEMU_VERSION}/${asset}"
    obtain_verified_asset "$asset" "$QEMU_CHECKSUMS" "Espressif QEMU" "$QEMU_VERSION" "$url"
    dest="$REQUIRED_RUN_ROOT/$tool"
    mkdir -p "$dest"
    validate_archive_members "$VERIFIED_ASSET_PATH"
    tar xf "$VERIFIED_ASSET_PATH" -C "$dest" \
      || die "tar extraction failed for verified $asset"
    bin="$dest/qemu/bin/$bin"
    bin="$(canonical_required_backend "$bin")"
    is_esp_qemu_fork "$bin" || die "verified $asset is not the Espressif QEMU fork"
    ver="$("$bin" --version 2>/dev/null | head -1 || true)"
    case "$ver" in *"(${expected})"*) ;; *) die "verified $asset reports the wrong version: $ver" ;; esac
    case "$arch" in
      xtensa) REQUIRED_QEMU_XTENSA_PATH="$bin" ;;
      riscv32) REQUIRED_QEMU_RISCV32_PATH="$bin" ;;
    esac
  done
}

prepare_required_backends() {
  local run_base
  if [ -n "${HAUKSBEE_REQUIRED_RUN_ROOT:-}" ]; then
    REQUIRED_RUN_ROOT="$HAUKSBEE_REQUIRED_RUN_ROOT"
    case "$REQUIRED_RUN_ROOT" in /*) ;; *) die "HAUKSBEE_REQUIRED_RUN_ROOT must be absolute" ;; esac
    [ -d "$REQUIRED_RUN_ROOT" ] || die "runner-owned required root does not exist: $REQUIRED_RUN_ROOT"
    [ -z "$(find "$REQUIRED_RUN_ROOT" -mindepth 1 -maxdepth 1 -print -quit)" ] \
      || die "runner-owned required root is not empty: $REQUIRED_RUN_ROOT"
  else
    run_base="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
    mkdir -p "$run_base"
    REQUIRED_RUN_ROOT="$(mktemp -d "$run_base/hauksbee-required-sims.XXXXXX")"
  fi
  [ "$DO_RENODE" -eq 0 ] || prepare_required_renode
  [ "$DO_QEMU" -eq 0 ] || prepare_required_qemu
}

# ── CHECK mode ───────────────────────────────────────────────────────────────
if [ "$CHECK_ONLY" -eq 1 ]; then
  if [ "$REQUIRE_PINNED" -eq 1 ]; then
    die "--check --require-pinned cannot authenticate an installed tree; run --require-pinned to reverify repository-pinned archives and prepare fresh backends"
  fi
  log "hauksbee simulator discovery check"
  printf '\n'

  all_ok=0
  REQUIRED_RENODE_PATH=""
  REQUIRED_QEMU_XTENSA_PATH=""
  REQUIRED_QEMU_RISCV32_PATH=""

  if [ "$DO_RENODE" -eq 1 ]; then
    log "Renode (STM32 / nRF52 / RP2040 / RISC-V backend)"
    info "  pinned release: $RENODE_VERSION  (installs come from this, hash-checked)"
    rnode_bin="$(find_renode_bin || true)"
    if [ -n "$rnode_bin" ]; then
      REQUIRED_RENODE_PATH="$rnode_bin"
      ok "FOUND  $rnode_bin"
      ver="$("$rnode_bin" --version 2>/dev/null | head -1 || echo "(version unknown)")"
      info "       $ver"
    else
      err "NOT FOUND"
      info "  Discovery order:"
      info "    1. \$HAUKSBEE_RENODE env var (currently: ${HAUKSBEE_RENODE:-unset})"
      info "    2. \`renode\` on PATH"
      info "    3. ~/renode-portable/Renode.app/Contents/MacOS/renode  (macOS)"
      info "    3. ~/renode-portable/renode  or  ~/renode_portable/renode  (Linux)"
      info "  Run: scripts/install-sims.sh --renode-only"
      all_ok=1
    fi
    printf '\n'
  fi

  if [ "$DO_QEMU" -eq 1 ]; then
    log "Espressif QEMU (ESP32 / ESP32-S3 / ESP32-C3 backend)"
    info "  pinned release: $QEMU_VERSION  (installs come from this, hash-checked)"
    for arch_info in "qemu-system-xtensa:HAUKSBEE_QEMU_XTENSA" "qemu-system-riscv32:HAUKSBEE_QEMU_RISCV32"; do
      name="${arch_info%%:*}"
      envvar="${arch_info##*:}"
      qemu_bin="$(find_qemu_bin "$name" "$envvar" || true)"
      if [ -n "$qemu_bin" ]; then
        case "$name" in
          qemu-system-xtensa) REQUIRED_QEMU_XTENSA_PATH="$qemu_bin" ;;
          qemu-system-riscv32) REQUIRED_QEMU_RISCV32_PATH="$qemu_bin" ;;
        esac
        ok "FOUND  $qemu_bin"
        ver="$("$qemu_bin" --version 2>/dev/null | head -1 || echo "(version unknown)")"
        info "       $ver"
        # Confirm it is the Espressif fork
        if is_esp_qemu_fork "$qemu_bin"; then
          ok "       Espressif fork confirmed (esp32 machine present)"
        else
          warn "       WARNING: binary found but esp32 machine NOT listed. This may be mainline QEMU, which cannot emulate ESP32."
          all_ok=1
        fi
      else
        err "NOT FOUND  $name"
        info "  Discovery order:"
        info "    1. \$$envvar env var"
        info "    2. \$HAUKSBEE_QEMU_DIR/bin/$name"
        info "    3. ~/.hauksbee-qemu-esp/qemu/bin/$name (legacy: ~/.galvani-qemu-esp/...)"
        info "    4. ~/.espressif/tools/qemu-*/<ver>/qemu/bin/$name  (idf_tools)"
        info "    5. \`$name\` on PATH (Espressif fork only; mainline rejected)"
        info "  Run: scripts/install-sims.sh --qemu-only"
        all_ok=1
      fi
      printf '\n'
    done
  fi

  if [ "$DO_AVR" -eq 1 ]; then
    log "AVR / simavr (ATmega / ATtiny co-sim, linked in-process)"
    avr_found="$(find_avr_prefix 2>/dev/null || true)"
    if [ -n "$avr_found" ]; then
      ok "FOUND  $avr_found"
      info "       header: $avr_found/include/simavr/sim_avr.h"
      info "       lib:    $avr_found/lib/libsimavr.a"
    else
      err "NOT FOUND under $(avr_prefix)"
      info "  Expected: $(avr_prefix)/include/simavr/sim_avr.h and $(avr_prefix)/lib/libsimavr.a"
      info "  Run: scripts/install-sims.sh --avr"
      all_ok=1
    fi
    printf '\n'
  fi

  if [ "$all_ok" -eq 0 ]; then
    log "All requested backends found. hauksbee will discover them."
    if [ "$EMIT_REQUIRED_PATHS" -eq 1 ]; then
      printf 'REQUIRED_BACKEND_PATH HAUKSBEE_RENODE=%s\n' "$REQUIRED_RENODE_PATH"
      printf 'REQUIRED_BACKEND_PATH HAUKSBEE_QEMU_XTENSA=%s\n' "$REQUIRED_QEMU_XTENSA_PATH"
      printf 'REQUIRED_BACKEND_PATH HAUKSBEE_QEMU_RISCV32=%s\n' "$REQUIRED_QEMU_RISCV32_PATH"
    fi
  else
    err "One or more backends not found. Run scripts/install-sims.sh to install."
  fi
  exit $all_ok
fi

# ── INSTALL mode ─────────────────────────────────────────────────────────────

# ── install Renode ────────────────────────────────────────────────────────────
install_renode() {
  log "Renode: checking for existing install..."

  rnode_bin="$(find_renode_bin 2>/dev/null || true)"
  if [ -n "$rnode_bin" ]; then
    ok "Already discoverable at $rnode_bin — skipping download."
    return 0
  fi

  log "Renode: selecting pinned release..."
  RENODE_VER="$(resolve_renode_version)"
  info "  version: $RENODE_VER"

  TMPDIR="$(make_tmpdir)"

  case "$PLATFORM" in
    darwin)
      if [ "$ARCH_NORM" = x86_64 ]; then
        ASSET="renode_${RENODE_VER}.dmg"
      else
        ASSET="renode-${RENODE_VER}-dotnet.osx-arm64-portable.dmg"
      fi
      DOWNLOAD_URL="https://github.com/renode/renode/releases/download/v${RENODE_VER}/${ASSET}"

      log "Renode: downloading $ASSET..."
      info "  from: $DOWNLOAD_URL"
      # --fail: without it a 404 or a captive-portal page is written to disk
      # and handed to hdiutil, which then fails with something unrelated to the
      # actual problem.
      curl --fail --location --progress-bar --output "$TMPDIR/$ASSET" "$DOWNLOAD_URL" \
        || die "Download failed. Check the URL or download manually: $DOWNLOAD_URL"
      verify_renode_asset "$TMPDIR/$ASSET" "$ASSET"

      MOUNTPOINT="$TMPDIR/renode_mnt"
      mkdir -p "$MOUNTPOINT"
      log "Renode: mounting DMG..."
      REQUIRED_MOUNTPOINT="$MOUNTPOINT"
      hdiutil attach "$TMPDIR/$ASSET" -mountpoint "$MOUNTPOINT" -nobrowse -quiet \
        || die "hdiutil attach failed. The DMG may be corrupt; try again."

      mkdir -p "$HOME/renode-portable"
      log "Renode: copying Renode.app to ~/renode-portable/ ..."
      info "  (using ditto to preserve bundle permissions and symlinks)"
      ditto "$MOUNTPOINT/Renode.app" "$HOME/renode-portable/Renode.app" \
        || { hdiutil detach "$MOUNTPOINT" -quiet 2>/dev/null || true; die "ditto copy failed."; }

      if hdiutil detach "$MOUNTPOINT" -quiet; then
        REQUIRED_MOUNTPOINT=""
      else
        warn "Could not detach DMG; retrying during installer cleanup."
      fi

      log "Renode: removing quarantine flag (Gatekeeper)..."
      xattr -dr com.apple.quarantine "$HOME/renode-portable/Renode.app" 2>/dev/null || true

      if [ "$ARCH_NORM" = x86_64 ]; then
        BIN="$HOME/renode-portable/Renode.app/Contents/MacOS/macos_run.command"
        ln -sf macos_run.command "$HOME/renode-portable/Renode.app/Contents/MacOS/renode"
      else
        BIN="$HOME/renode-portable/Renode.app/Contents/MacOS/renode"
      fi
      [ -x "$BIN" ] || die "Renode binary not found at $BIN after install."
      ;;

    linux)
      case "$ARCH_NORM" in
        arm64)  ASSET="renode-${RENODE_VER}.linux-arm64-portable-dotnet.tar.gz" ;;
        x86_64) ASSET="renode-${RENODE_VER}.linux-portable-dotnet.tar.gz" ;;
      esac
      DOWNLOAD_URL="https://github.com/renode/renode/releases/download/v${RENODE_VER}/${ASSET}"

      log "Renode: downloading $ASSET..."
      info "  from: $DOWNLOAD_URL"
      # --fail: without it a 404 or a captive-portal page is written to disk
      # and handed to hdiutil, which then fails with something unrelated to the
      # actual problem.
      curl --fail --location --progress-bar --output "$TMPDIR/$ASSET" "$DOWNLOAD_URL" \
        || die "Download failed. Check the URL or download manually: $DOWNLOAD_URL"
      verify_renode_asset "$TMPDIR/$ASSET" "$ASSET"

      mkdir -p "$HOME/renode-portable"
      log "Renode: extracting to ~/renode-portable/ ..."
      info "  (stripping top-level versioned directory)"
      tar xzf "$TMPDIR/$ASSET" -C "$HOME/renode-portable" --strip-components=1 \
        || die "tar extraction failed."

      BIN="$HOME/renode-portable/renode"
      [ -x "$BIN" ] || die "Renode binary not found at $BIN after extract."
      ;;
  esac

  log "Renode: verifying..."
  VER_OUT="$("$BIN" --version 2>/dev/null | head -1 || echo "(unknown)")"
  case "$VER_OUT" in
    "Renode v${RENODE_VERSION}"|"Renode v${RENODE_VERSION}."*|"Renode, version ${RENODE_VERSION}"*) ;;
    *) die "Renode installed from the pinned asset but reports the wrong version: $VER_OUT" ;;
  esac
  ok "Renode installed: $BIN"
  info "  $VER_OUT"
}

# ── install Espressif QEMU ────────────────────────────────────────────────────
install_qemu() {
  log "Espressif QEMU: checking for existing install..."

  xtensa_bin="$(find_qemu_bin "qemu-system-xtensa" "HAUKSBEE_QEMU_XTENSA" 2>/dev/null || true)"
  riscv_bin="$(find_qemu_bin "qemu-system-riscv32" "HAUKSBEE_QEMU_RISCV32" 2>/dev/null || true)"

  if [ -n "$xtensa_bin" ] && [ -n "$riscv_bin" ]; then
    ok "Both already discoverable:"
    info "  qemu-system-xtensa:  $xtensa_bin"
    info "  qemu-system-riscv32: $riscv_bin"
    ok "Skipping download."
    return 0
  fi

  # ── try idf_tools.py path first ──────────────────────────────────────────
  IDF_TOOLS_PY="$(find_idf_tools_py 2>/dev/null || true)"
  if [ -n "$IDF_TOOLS_PY" ] && [ "$REQUIRE_PINNED" -eq 0 ]; then
    log "Espressif QEMU: ESP-IDF found at $(dirname "$(dirname "$IDF_TOOLS_PY")")"
    info "  Installing via idf_tools.py (preferred; lands in ~/.espressif/tools/)"
    python3 "$IDF_TOOLS_PY" install qemu-xtensa qemu-riscv32 \
      || die "idf_tools.py install failed. See output above."
    ok "idf_tools.py install complete."

    # Verify
    xtensa_bin="$(find_qemu_bin "qemu-system-xtensa" "HAUKSBEE_QEMU_XTENSA" 2>/dev/null || true)"
    riscv_bin="$(find_qemu_bin "qemu-system-riscv32" "HAUKSBEE_QEMU_RISCV32" 2>/dev/null || true)"
    [ -n "$xtensa_bin" ] || die "qemu-system-xtensa not discoverable after idf_tools install."
    [ -n "$riscv_bin" ]  || die "qemu-system-riscv32 not discoverable after idf_tools install."
    ok "qemu-system-xtensa:  $xtensa_bin"
    ok "qemu-system-riscv32: $riscv_bin"
    return 0
  fi

  # ── direct download from espressif/qemu releases ─────────────────────────
  log "Espressif QEMU: no ESP-IDF found; using the pinned release..."
  QEMU_TAG="$(resolve_qemu_tag)"
  QEMU_DIR_VER="$(qemu_tag_to_dir_ver "$QEMU_TAG")"
  info "  tag: $QEMU_TAG  (dir ver: $QEMU_DIR_VER)"

  # Asset names: qemu-<arch>-softmmu-<dir-ver>-<os>-<arch>.tar.xz
  case "${PLATFORM}-${ARCH_NORM}" in
    darwin-arm64)  OS_ARCH_SUFFIX="aarch64-apple-darwin" ;;
    darwin-x86_64) OS_ARCH_SUFFIX="x86_64-apple-darwin" ;;
    linux-x86_64)  OS_ARCH_SUFFIX="x86_64-linux-gnu" ;;
    linux-arm64)   OS_ARCH_SUFFIX="aarch64-linux-gnu" ;;
  esac

  TMPDIR="$(make_tmpdir)"

  for arch_info in "xtensa:qemu-xtensa" "riscv32:qemu-riscv32"; do
    qemu_arch="${arch_info%%:*}"        # xtensa | riscv32
    tool_name="${arch_info##*:}"        # qemu-xtensa | qemu-riscv32
    bin_name="qemu-system-${qemu_arch}"

    # Already installed?
    existing="$(find_qemu_bin "$bin_name" "HAUKSBEE_QEMU_$(echo "$qemu_arch" | tr '[:lower:]' '[:upper:]' | tr '-' '_')" 2>/dev/null || true)"
    if [ -n "$existing" ]; then
      ok "$bin_name already discoverable at $existing — skipping."
      continue
    fi

    ASSET="$(qemu_asset_name "$QEMU_DIR_VER" "$tool_name" "$OS_ARCH_SUFFIX")"
    DOWNLOAD_URL="https://github.com/espressif/qemu/releases/download/${QEMU_TAG}/${ASSET}"

    log "Espressif QEMU: downloading $ASSET..."
    info "  from: $DOWNLOAD_URL"

    if ! curl --fail --location --progress-bar --output "$TMPDIR/$ASSET" "$DOWNLOAD_URL" 2>/dev/null; then
      warn "Automatic download failed for $ASSET"
      warn "Download it manually from the pinned release:"
      warn "  https://github.com/espressif/qemu/releases/tag/${QEMU_TAG}"
      warn "Pick the ${OS_ARCH_SUFFIX} asset for ${tool_name}, check its sha256 against"
      warn "  $QEMU_CHECKSUMS"
      warn "and extract it so that"
      warn "  ~/.espressif/tools/${tool_name}/${QEMU_DIR_VER}/qemu/bin/${bin_name}"
      warn "exists, then re-run with --check."
      continue
    fi
    verify_qemu_asset "$TMPDIR/$ASSET" "$ASSET"

    DEST_DIR="$HOME/.espressif/tools/${tool_name}/${QEMU_DIR_VER}/qemu"
    mkdir -p "$DEST_DIR"
    log "Espressif QEMU: extracting to $DEST_DIR ..."

    # The Espressif releases have a top-level 'qemu/' directory in the tarball.
    # We want ~/.espressif/tools/qemu-xtensa/<ver>/qemu/bin/..., so strip that
    # top-level 'qemu' component by extracting one level up and using --strip=1.
    # `tar xf` (no compression flag) sniffs bz2/xz/gz — the compression has
    # changed across upstream releases, so never hardcode it.
    tar xf "$TMPDIR/$ASSET" -C "$DEST_DIR" --strip-components=1 \
      || die "tar extraction failed for $ASSET."

    # Verify
    installed_bin="$DEST_DIR/bin/$bin_name"
    [ -x "$installed_bin" ] || die "$installed_bin not found after extraction."
    chmod +x "$installed_bin"

    # A per-arch failure must not abort the sibling arch's install (a die here
    # historically left riscv32 uninstalled whenever xtensa's check failed).
    # The common cause on a fresh mac is a missing Homebrew dylib, not a wrong
    # binary — surface the dyld error so the fix is obvious.
    if ! is_esp_qemu_fork "$installed_bin"; then
      warn "$bin_name installed but its esp32 machine check failed."
      warn "Likely a missing shared library. First error from the binary:"
      "$installed_bin" -machine help 2>&1 | head -2 | while IFS= read -r l; do warn "  $l"; done
      warn "If it names a /opt/homebrew library: brew install the package, then re-run --check."
      continue
    fi
    ok "$bin_name installed: $installed_bin"
  done

  # Final cross-check
  xtensa_bin="$(find_qemu_bin "qemu-system-xtensa"  "HAUKSBEE_QEMU_XTENSA"  2>/dev/null || true)"
  riscv_bin="$(find_qemu_bin  "qemu-system-riscv32" "HAUKSBEE_QEMU_RISCV32" 2>/dev/null || true)"
  [ -n "$xtensa_bin" ]  || die "qemu-system-xtensa not discoverable after install."
  [ -n "$riscv_bin" ]   || die "qemu-system-riscv32 not discoverable after install."
}

# ── install AVR / simavr ──────────────────────────────────────────────────────
#
# simavr is GPL-3.0 and deliberately not vendored (this repo is Apache-2.0); we build
# and install it from a pinned upstream tag into the prefix build.rs links
# against. This is the same recipe used to produce the working install on the
# reference machine.
install_avr() {
  local prefix; prefix="$(avr_prefix)"

  log "AVR / simavr: checking for existing install under $prefix ..."
  if find_avr_prefix >/dev/null 2>&1; then
    ok "Already installed at $prefix — skipping."
    info "  header: $prefix/include/simavr/sim_avr.h"
    info "  lib:    $prefix/lib/libsimavr.a"
    return 0
  fi

  # 1. libelf (simavr's ELF loader dependency). zlib ships with the OS.
  log "AVR / simavr: ensuring libelf is present..."
  case "$PLATFORM" in
    darwin)
      if have brew; then
        brew list libelf >/dev/null 2>&1 || brew install libelf \
          || die "brew install libelf failed. Install it, then re-run --avr."
        ok "libelf present (Homebrew)."
      else
        warn "Homebrew not found. Install libelf yourself, then re-run --avr."
      fi
      ;;
    linux)
      info "  simavr needs libelf, zlib and a C toolchain. If the build below"
      info "  fails on a missing header, install them:"
      info "    Debian/Ubuntu: sudo apt-get install libelf-dev zlib1g-dev build-essential"
      info "    Fedora:        sudo dnf install elfutils-libelf-devel zlib-devel make gcc"
      have cc || have gcc || warn "no C compiler on PATH; the simavr build will fail without one."
      ;;
  esac

  # 2. clone + build + install simavr at the pinned tag.
  have git  || die "git not found; needed to clone simavr."
  have make || die "make not found; needed to build simavr."
  TMPDIR="$(make_tmpdir)"
  log "AVR / simavr: cloning buserror/simavr @ $SIMAVR_TAG ..."
  info "  into: $TMPDIR/simavr"
  git clone --depth 1 --branch "$SIMAVR_TAG" https://github.com/buserror/simavr "$TMPDIR/simavr" \
    || die "git clone of simavr failed (tag $SIMAVR_TAG). Clone it manually and 'make install-simavr'."

  # simavr's install prefix is controlled by DESTDIR (its Makefile sets
  # PREFIX = ${DESTDIR}); headers land in <DESTDIR>/include/simavr, the archive
  # in <DESTDIR>/lib. On macOS point HOMEBREW_PREFIX at the real Homebrew tree so
  # its Makefile finds libelf, regardless of where we install simavr.
  local make_args=(RELEASE=1 DESTDIR="$prefix")
  if [ "$PLATFORM" = darwin ]; then
    make_args+=(HOMEBREW_PREFIX="$(brew_prefix)")
  fi

  log "AVR / simavr: building + installing into $prefix ..."
  info "  make -C $TMPDIR/simavr install-simavr ${make_args[*]}"
  if [ ! -w "$prefix" ] && [ ! -w "$(dirname "$prefix")" ]; then
    warn "  $prefix is not writable by this user; the install may need sudo."
  fi
  make -C "$TMPDIR/simavr" install-simavr "${make_args[@]}" \
    || die "simavr build/install failed. Check the output above (missing libelf/zlib or a C compiler are the usual causes)."

  # 3. verify against the exact paths build.rs will look for.
  [ -f "$prefix/include/simavr/sim_avr.h" ] \
    || die "simavr headers not found at $prefix/include/simavr/sim_avr.h after install."
  [ -f "$prefix/lib/libsimavr.a" ] \
    || die "libsimavr.a not found at $prefix/lib/libsimavr.a after install."
  ok "simavr installed under $prefix"
  info "  header: $prefix/include/simavr/sim_avr.h"
  info "  lib:    $prefix/lib/libsimavr.a"
  if [ -n "$PREFIX_OVERRIDE" ]; then
    info "  Non-default prefix: build with"
    info "    SIMAVR_INCLUDE_DIR=$prefix/include SIMAVR_LIB_DIR=$prefix/lib cargo build -p hauksbee-mcu"
  fi
}

# ── main ─────────────────────────────────────────────────────────────────────
printf '\n'
log "hauksbee simulator installer  (OS: $OS  arch: $ARCH_NORM)"
printf '\n'

if [ "$REQUIRE_PINNED" -eq 1 ]; then
  prepare_required_backends
elif [ "$DO_RENODE" -eq 1 ]; then
  install_renode
  printf '\n'
fi

if [ "$REQUIRE_PINNED" -eq 0 ] && [ "$DO_QEMU" -eq 1 ]; then
  install_qemu
  printf '\n'
fi

if [ "$REQUIRE_PINNED" -eq 0 ] && [ "$DO_AVR" -eq 1 ]; then
  install_avr
  printf '\n'
fi

# ── summary ───────────────────────────────────────────────────────────────────
log "Summary"

summary_ok=1

if [ "$DO_RENODE" -eq 1 ]; then
  if [ "$REQUIRE_PINNED" -eq 1 ]; then rnode_bin="$REQUIRED_RENODE_PATH"; else rnode_bin="$(find_renode_bin 2>/dev/null || true)"; fi
  if [ -n "$rnode_bin" ]; then
    ok "Renode   $rnode_bin"
  else
    err "Renode   NOT FOUND (install step may have failed; see output above)"
    summary_ok=0
  fi
fi

if [ "$DO_QEMU" -eq 1 ]; then
  if [ "$REQUIRE_PINNED" -eq 1 ]; then
    xtensa_bin="$REQUIRED_QEMU_XTENSA_PATH"; riscv_bin="$REQUIRED_QEMU_RISCV32_PATH"
  else
    xtensa_bin="$(find_qemu_bin "qemu-system-xtensa"  "HAUKSBEE_QEMU_XTENSA"  2>/dev/null || true)"
    riscv_bin="$(find_qemu_bin  "qemu-system-riscv32" "HAUKSBEE_QEMU_RISCV32" 2>/dev/null || true)"
  fi
  if [ -n "$xtensa_bin" ]; then
    ok "QEMU (xtensa)   $xtensa_bin"
  else
    err "QEMU (xtensa)   NOT FOUND"
    summary_ok=0
  fi
  if [ -n "$riscv_bin" ]; then
    ok "QEMU (riscv32)  $riscv_bin"
  else
    err "QEMU (riscv32)  NOT FOUND"
    summary_ok=0
  fi
fi

if [ "$DO_AVR" -eq 1 ]; then
  avr_found="$(find_avr_prefix 2>/dev/null || true)"
  if [ -n "$avr_found" ]; then
    ok "simavr   $avr_found/lib/libsimavr.a"
  else
    err "simavr   NOT FOUND (install step may have failed; see output above)"
    summary_ok=0
  fi
fi

printf '\n'
if [ "$summary_ok" -eq 1 ]; then
  if [ "$EMIT_REQUIRED_PATHS" -eq 1 ]; then
    [ "$DO_RENODE" -eq 0 ] || printf 'REQUIRED_BACKEND_PATH HAUKSBEE_RENODE=%s\n' "$REQUIRED_RENODE_PATH"
    [ "$DO_QEMU" -eq 0 ] || printf 'REQUIRED_BACKEND_PATH HAUKSBEE_QEMU_XTENSA=%s\n' "$REQUIRED_QEMU_XTENSA_PATH"
    [ "$DO_QEMU" -eq 0 ] || printf 'REQUIRED_BACKEND_PATH HAUKSBEE_QEMU_RISCV32=%s\n' "$REQUIRED_QEMU_RISCV32_PATH"
  fi
  log "Done. Verify with: scripts/install-sims.sh --check"
else
  die "One or more backends did not install. See output above."
fi
