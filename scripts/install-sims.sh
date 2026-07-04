#!/usr/bin/env bash
# install-sims.sh - install or verify the external MCU simulator backends.
#
# hauksbee's AVR co-sim is built in (libsimavr, no install needed). The Renode
# and Espressif QEMU backends require externally installed binaries. This script
# installs them into the exact locations hauksbee auto-discovers, resolving
# versions from the GitHub API and verifying the binaries after install.
#
# Usage:
#   scripts/install-sims.sh [--renode-only | --qemu-only] [--check] [--help]
#
# Flags:
#   (none)           Install both Renode and Espressif QEMU.
#   --renode-only    Install Renode only.
#   --qemu-only      Install Espressif QEMU only.
#   --check          Do NOT install anything. Report for each backend whether
#                    hauksbee will discover it and at which path. Exit 0 if
#                    both requested backends are discoverable, else 1.
#   --help           Show this help.
#
# Install targets:
#   Renode    -> ~/renode-portable/Renode.app  (macOS)
#              -> ~/renode-portable/renode      (Linux)
#   Esp QEMU  -> ~/.espressif/tools/qemu-xtensa/<ver>/qemu/bin/
#              -> ~/.espressif/tools/qemu-riscv32/<ver>/qemu/bin/
#             (or via idf_tools.py if an ESP-IDF checkout is found)
#
# Environment overrides (these are hauksbee's env overrides, not just this
# script's): HAUKSBEE_RENODE, HAUKSBEE_QEMU_XTENSA, HAUKSBEE_QEMU_RISCV32.
#
# Idempotent: if a backend is already discoverable, the download is skipped.
# Safe: writes only to ~/renode-portable and ~/.espressif/tools. Never uses
# rm -rf on user paths.
set -euo pipefail
# shellcheck source=scripts/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; }

# ── defaults ────────────────────────────────────────────────────────────────
DO_RENODE=1
DO_QEMU=1
CHECK_ONLY=0

while [ $# -gt 0 ]; do
  case "$1" in
    --renode-only)  DO_QEMU=0; shift ;;
    --qemu-only)    DO_RENODE=0; shift ;;
    --check)        CHECK_ONLY=1; shift ;;
    -h|--help)      usage; exit 0 ;;
    *) die "unknown argument '$1' (try --help)" ;;
  esac
done

# ── platform detection ───────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin) PLATFORM=darwin ;;
  Linux)  PLATFORM=linux ;;
  *)
    err "Unsupported OS: $OS"
    err "Windows users: see docs/SIMULATORS.md for manual install instructions."
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

# ── fallback pinned versions (used if GitHub API is unreachable) ─────────────
RENODE_FALLBACK_VER="1.16.1"
QEMU_FALLBACK_VER="esp_develop_9.0.0_20240606"

# ── helpers ──────────────────────────────────────────────────────────────────

# Fetch JSON from GitHub API (unauthenticated; may be rate-limited on shared IPs).
# Returns non-zero if curl fails.
github_api() {
  local url="$1"
  curl --silent --fail --location \
    -H "Accept: application/vnd.github+json" \
    -H "X-GitHub-Api-Version: 2022-11-28" \
    "$url"
}

# Resolve the latest Renode release tag (e.g. "1.16.1"), stripping any leading "v".
resolve_renode_version() {
  local tag
  if tag="$(github_api "https://api.github.com/repos/renode/renode/releases/latest" \
             2>/dev/null | grep '"tag_name"' | sed 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/')"; then
    [ -n "$tag" ] && printf '%s' "$tag" && return 0
  fi
  warn "GitHub API unreachable; using pinned Renode version $RENODE_FALLBACK_VER"
  printf '%s' "$RENODE_FALLBACK_VER"
}

# Resolve the latest Espressif QEMU release tag (e.g. "esp-develop-9.0.0-20240606").
resolve_qemu_tag() {
  local tag
  if tag="$(github_api "https://api.github.com/repos/espressif/qemu/releases/latest" \
             2>/dev/null | grep '"tag_name"' | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"; then
    [ -n "$tag" ] && printf '%s' "$tag" && return 0
  fi
  warn "GitHub API unreachable; using pinned QEMU tag $QEMU_FALLBACK_VER"
  printf '%s' "$QEMU_FALLBACK_VER"
}

# Resolve the actual release asset name for a tool+platform by listing the
# release's assets, instead of constructing the name. Upstream has changed
# both the version separator (hyphens -> underscores inside the version) and
# the compression (.tar.bz2 -> .tar.xz) across releases; a constructed name
# 404s and the "archive" we then extract is a GitHub error page. Matching
# against the published asset list survives both kinds of rename.
resolve_qemu_asset() {
  local tag="$1" tool_name="$2" os_arch_suffix="$3" asset
  asset="$(github_api "https://api.github.com/repos/espressif/qemu/releases/tags/${tag}" \
            2>/dev/null \
          | grep '"name"' \
          | sed 's/.*"name": *"\([^"]*\)".*/\1/' \
          | grep "^${tool_name}-softmmu-.*-${os_arch_suffix}\.tar\.\(bz2\|xz\|gz\)$" \
          | head -1)"
  [ -n "$asset" ] && printf '%s' "$asset"
}

# Convert an espressif/qemu release tag (e.g. "esp-develop-9.0.0-20240606") to
# the directory-name form used by idf_tools ("esp_develop_9.0.0_20240606").
qemu_tag_to_dir_ver() {
  printf '%s' "$1" | tr '-' '_' | sed 's/^esp_/esp_/'
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
    printf '%s' "$(command -v renode)"
    return 0
  fi

  # 3. Conventional portable locations
  if [ -x "$HOME/renode-portable/Renode.app/Contents/MacOS/renode" ]; then
    printf '%s' "$HOME/renode-portable/Renode.app/Contents/MacOS/renode"
    return 0
  fi
  if [ -x "$HOME/renode-portable/renode" ]; then
    printf '%s' "$HOME/renode-portable/renode"
    return 0
  fi
  if [ -x "$HOME/renode_portable/renode" ]; then
    printf '%s' "$HOME/renode_portable/renode"
    return 0
  fi

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
    if [ -x "$envval" ]; then
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
  candidates+=("$HOME/.galvani-qemu-esp/qemu/bin/$name")
  candidates+=("$HOME/.hauksbee-qemu-esp/qemu/bin/$name")

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
trap cleanup_tmpdir EXIT

# ── CHECK mode ───────────────────────────────────────────────────────────────
if [ "$CHECK_ONLY" -eq 1 ]; then
  log "hauksbee simulator discovery check"
  printf '\n'

  all_ok=0

  if [ "$DO_RENODE" -eq 1 ]; then
    log "Renode (STM32 / nRF52 / RP2040 / RISC-V backend)"
    rnode_bin="$(find_renode_bin 2>/dev/null || true)"
    if [ -n "$rnode_bin" ]; then
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
    for arch_info in "qemu-system-xtensa:HAUKSBEE_QEMU_XTENSA" "qemu-system-riscv32:HAUKSBEE_QEMU_RISCV32"; do
      name="${arch_info%%:*}"
      envvar="${arch_info##*:}"
      qemu_bin="$(find_qemu_bin "$name" "$envvar" 2>/dev/null || true)"
      if [ -n "$qemu_bin" ]; then
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
        info "    3. ~/.galvani-qemu-esp/qemu/bin/$name (legacy: ~/.hauksbee-qemu-esp/...)"
        info "    4. ~/.espressif/tools/qemu-*/<ver>/qemu/bin/$name  (idf_tools)"
        info "    5. \`$name\` on PATH (Espressif fork only; mainline rejected)"
        info "  Run: scripts/install-sims.sh --qemu-only"
        all_ok=1
      fi
      printf '\n'
    done
  fi

  if [ "$all_ok" -eq 0 ]; then
    log "All requested backends found. hauksbee will discover them."
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

  log "Renode: resolving latest release..."
  RENODE_VER="$(resolve_renode_version)"
  info "  version: $RENODE_VER"

  TMPDIR="$(make_tmpdir)"

  case "$PLATFORM" in
    darwin)
      ASSET="renode-${RENODE_VER}-dotnet.osx-${ARCH_NORM}-portable.dmg"
      DOWNLOAD_URL="https://github.com/renode/renode/releases/download/v${RENODE_VER}/${ASSET}"

      log "Renode: downloading $ASSET..."
      info "  from: $DOWNLOAD_URL"
      curl --location --progress-bar --output "$TMPDIR/$ASSET" "$DOWNLOAD_URL" \
        || die "Download failed. Check the URL or download manually: $DOWNLOAD_URL"

      MOUNTPOINT="$TMPDIR/renode_mnt"
      mkdir -p "$MOUNTPOINT"
      log "Renode: mounting DMG..."
      hdiutil attach "$TMPDIR/$ASSET" -mountpoint "$MOUNTPOINT" -nobrowse -quiet \
        || die "hdiutil attach failed. The DMG may be corrupt; try again."

      mkdir -p "$HOME/renode-portable"
      log "Renode: copying Renode.app to ~/renode-portable/ ..."
      info "  (using ditto to preserve bundle permissions and symlinks)"
      ditto "$MOUNTPOINT/Renode.app" "$HOME/renode-portable/Renode.app" \
        || { hdiutil detach "$MOUNTPOINT" -quiet 2>/dev/null || true; die "ditto copy failed."; }

      hdiutil detach "$MOUNTPOINT" -quiet \
        || warn "Could not detach DMG; you can detach it manually."

      log "Renode: removing quarantine flag (Gatekeeper)..."
      xattr -dr com.apple.quarantine "$HOME/renode-portable/Renode.app" 2>/dev/null || true

      BIN="$HOME/renode-portable/Renode.app/Contents/MacOS/renode"
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
      curl --location --progress-bar --output "$TMPDIR/$ASSET" "$DOWNLOAD_URL" \
        || die "Download failed. Check the URL or download manually: $DOWNLOAD_URL"

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
  if [ -n "$IDF_TOOLS_PY" ]; then
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
  log "Espressif QEMU: no ESP-IDF found; resolving latest release from GitHub..."
  QEMU_TAG="$(resolve_qemu_tag)"
  QEMU_DIR_VER="$(qemu_tag_to_dir_ver "$QEMU_TAG")"
  info "  tag: $QEMU_TAG  (dir ver: $QEMU_DIR_VER)"

  # Asset names: qemu-<arch>-softmmu-<tag>-<os>-<arch>.tar.bz2
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

    # Prefer the asset name the release actually publishes (see
    # resolve_qemu_asset); the constructed form is a fallback for when the
    # API is unreachable, and carries the historical naming.
    ASSET="$(resolve_qemu_asset "$QEMU_TAG" "$tool_name" "$OS_ARCH_SUFFIX")"
    if [ -z "$ASSET" ]; then
      warn "Could not list release assets; falling back to constructed name."
      ASSET="${tool_name}-softmmu-${QEMU_TAG}-${OS_ARCH_SUFFIX}.tar.bz2"
    fi
    DOWNLOAD_URL="https://github.com/espressif/qemu/releases/download/${QEMU_TAG}/${ASSET}"

    log "Espressif QEMU: downloading $ASSET..."
    info "  from: $DOWNLOAD_URL"

    if ! curl --fail --location --progress-bar --output "$TMPDIR/$ASSET" "$DOWNLOAD_URL" 2>/dev/null; then
      warn "Automatic download failed for $ASSET"
      warn "Asset naming conventions change between releases. Download manually:"
      warn "  https://github.com/espressif/qemu/releases/tag/${QEMU_TAG}"
      warn "Pick the ${OS_ARCH_SUFFIX} asset for ${tool_name} and extract it so that"
      warn "  ~/.espressif/tools/${tool_name}/${QEMU_DIR_VER}/qemu/bin/${bin_name}"
      warn "exists, then re-run with --check."
      continue
    fi

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

# ── main ─────────────────────────────────────────────────────────────────────
printf '\n'
log "hauksbee simulator installer  (OS: $OS  arch: $ARCH_NORM)"
printf '\n'

if [ "$DO_RENODE" -eq 1 ]; then
  install_renode
  printf '\n'
fi

if [ "$DO_QEMU" -eq 1 ]; then
  install_qemu
  printf '\n'
fi

# ── summary ───────────────────────────────────────────────────────────────────
log "Summary"

summary_ok=1

if [ "$DO_RENODE" -eq 1 ]; then
  rnode_bin="$(find_renode_bin 2>/dev/null || true)"
  if [ -n "$rnode_bin" ]; then
    ok "Renode   $rnode_bin"
  else
    err "Renode   NOT FOUND (install step may have failed; see output above)"
    summary_ok=0
  fi
fi

if [ "$DO_QEMU" -eq 1 ]; then
  xtensa_bin="$(find_qemu_bin "qemu-system-xtensa"  "HAUKSBEE_QEMU_XTENSA"  2>/dev/null || true)"
  riscv_bin="$(find_qemu_bin  "qemu-system-riscv32" "HAUKSBEE_QEMU_RISCV32" 2>/dev/null || true)"
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

printf '\n'
if [ "$summary_ok" -eq 1 ]; then
  log "Done. Verify with: scripts/install-sims.sh --check"
else
  die "One or more backends did not install. See output above."
fi
