#!/usr/bin/env bash
# get-hauksbee.sh - download and install prebuilt hauksbee + hauksbee-ci binaries.
#
# Detects OS/arch, fetches the latest GitHub Release asset (or a pinned version),
# verifies the sha256 checksum, and extracts the two binaries to ~/.local/bin.
# Safe to re-run: an existing install is only overwritten once the checksum
# of the new download is verified.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/ETM-Code/hauksbee/main/scripts/get-hauksbee.sh | bash
#   With flags through the pipe:
#     curl -fsSL .../get-hauksbee.sh | bash -s -- --permissive
#   Or run locally:
#     bash scripts/get-hauksbee.sh [--version v0.1.0] [--prefix ~/.local] [--permissive]
#
# Options:
#   --version TAG   Install a specific release tag (default: latest).
#   --prefix DIR    Install binaries to DIR/bin (default: ~/.local).
#   --permissive    Install the GPL-free build instead of the default one.
#   --help          Show this help.
#
# Which build you get:
#   Default: the full build, AVR / ATmega co-simulation included. It statically
#   links libsimavr, so THE BINARY is GPL-3.0 (hauksbee's source stays
#   Apache-2.0). GPL-3.0 constrains redistributing the binary, not running it.
#   --permissive: the same tool without the avr backend, so no GPL code is
#   linked and the binary is Apache-2.0. Take it if you redistribute or embed
#   hauksbee. It cannot do AVR co-sim; Renode and Espressif QEMU still work.
#   Either way, LICENSE-BINARY.txt inside the tarball spells out the terms.
#
# Environment:
#   GITHUB_TOKEN    Optional. Set to avoid GitHub API rate limits (60 req/hr
#                   unauthed vs 5000 authed). The CI token works:
#                   export GITHUB_TOKEN="$GITHUB_TOKEN"
set -euo pipefail

REPO="ETM-Code/hauksbee"
# Base URLs are overridable so the installer can target a GitHub Enterprise host,
# a self-hosted mirror, or a local mock (used to test the whole download/verify/
# install flow while the repo is still private). Defaults are public GitHub.
API_BASE="${HAUKSBEE_API_BASE:-https://api.github.com/repos/${REPO}}"
RELEASES_BASE="${HAUKSBEE_RELEASES_BASE:-https://github.com/${REPO}/releases/download}"

VERSION=""
PREFIX="${HOME}/.local"
# Shape: "" = the default download (avr included, GPL-3.0 binary),
# "-permissive" = the GPL-free download (no avr, Apache-2.0 binary).
SHAPE_SUFFIX=""

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------
while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      VERSION="${2:?--version requires a tag value (e.g. v0.1.0)}"
      shift 2
      ;;
    --version=*)
      VERSION="${1#*=}"
      shift
      ;;
    --prefix)
      PREFIX="${2:?--prefix requires a directory}"
      shift 2
      ;;
    --prefix=*)
      PREFIX="${1#*=}"
      shift
      ;;
    --permissive)
      SHAPE_SUFFIX="-permissive"
      shift
      ;;
    -h|--help)
      sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'
      exit 0
      ;;
    *)
      echo "Unknown argument: $1 (try --help)" >&2
      exit 1
      ;;
  esac
done

# ---------------------------------------------------------------------------
# Detect OS and architecture -> asset name suffix
# ---------------------------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
  Linux)
    # The release matrix (release.yml) builds all four promised targets natively:
    # linux-x86_64, linux-aarch64, darwin-arm64, darwin-x86_64.
    case "${ARCH}" in
      x86_64)          ASSET_SUFFIX="linux-x86_64" ;;
      aarch64|arm64)   ASSET_SUFFIX="linux-aarch64" ;;
      *)
        echo "No prebuilt hauksbee binary for Linux/${ARCH} (prebuilt: Linux x86_64/aarch64, macOS arm64/x86_64)." >&2
        echo "Build from source: https://github.com/${REPO}#quickstart" >&2
        exit 1
        ;;
    esac
    ;;
  Darwin)
    case "${ARCH}" in
      arm64)   ASSET_SUFFIX="darwin-arm64" ;;
      x86_64)  ASSET_SUFFIX="darwin-x86_64" ;;
      *)
        echo "No prebuilt hauksbee binary for macOS/${ARCH} (prebuilt: macOS arm64/x86_64, Linux x86_64/aarch64)." >&2
        echo "Build from source: https://github.com/${REPO}#quickstart" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    echo "Unsupported OS: ${OS}" >&2
    echo "Build from source: https://github.com/${REPO}#quickstart" >&2
    exit 1
    ;;
esac

echo "Detected platform: ${OS}/${ARCH} -> asset suffix: ${ASSET_SUFFIX}"

# ---------------------------------------------------------------------------
# Resolve the release tag (latest or pinned)
# ---------------------------------------------------------------------------
resolve_latest_tag() {
  local url="${API_BASE}/releases/latest"
  local auth_header=""
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    auth_header="Authorization: Bearer ${GITHUB_TOKEN}"
  fi

  local response
  if [ -n "${auth_header}" ]; then
    response="$(curl -fsSL -H "${auth_header}" "${url}")"
  else
    response="$(curl -fsSL "${url}")"
  fi

  # Extract the tag_name field. Use grep + sed rather than jq (may not be installed).
  printf '%s' "${response}" | grep '"tag_name"' | head -1 \
    | sed -E 's/.*"tag_name"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/'
}

if [ -z "${VERSION}" ]; then
  echo "Fetching latest release tag..."
  VERSION="$(resolve_latest_tag)"
  if [ -z "${VERSION}" ]; then
    echo "Could not determine the latest release tag from GitHub API." >&2
    echo "Pass --version vX.Y.Z explicitly, or check https://github.com/${REPO}/releases" >&2
    exit 1
  fi
fi

# Strip a leading 'v' to match the asset naming convention used in bundle.sh
# (bundle.sh strips the 'v' from the version when naming the tarball).
VERSION_BARE="${VERSION#v}"
echo "Installing hauksbee ${VERSION} (${VERSION_BARE})"

# ---------------------------------------------------------------------------
# Construct asset URLs
# ---------------------------------------------------------------------------
ASSET_NAME="hauksbee-${VERSION_BARE}-${ASSET_SUFFIX}${SHAPE_SUFFIX}"
TARBALL_NAME="${ASSET_NAME}.tar.gz"
CHECKSUM_NAME="${TARBALL_NAME}.sha256"
TARBALL_URL="${RELEASES_BASE}/${VERSION}/${TARBALL_NAME}"
CHECKSUM_URL="${RELEASES_BASE}/${VERSION}/${CHECKSUM_NAME}"

# ---------------------------------------------------------------------------
# Download to a temp directory; verify; then install
# ---------------------------------------------------------------------------
TMPDIR_WORK="$(mktemp -d "${TMPDIR:-/tmp}/get-hauksbee.XXXXXX")"
trap 'rm -rf "${TMPDIR_WORK}"' EXIT

TARBALL_PATH="${TMPDIR_WORK}/${TARBALL_NAME}"
CHECKSUM_PATH="${TMPDIR_WORK}/${CHECKSUM_NAME}"

echo "Downloading ${TARBALL_NAME}..."
curl -fsSL --retry 3 --retry-delay 2 -o "${TARBALL_PATH}" "${TARBALL_URL}"

echo "Downloading checksum..."
curl -fsSL --retry 3 --retry-delay 2 -o "${CHECKSUM_PATH}" "${CHECKSUM_URL}"

# ---------------------------------------------------------------------------
# Verify sha256 checksum
# ---------------------------------------------------------------------------
echo "Verifying checksum..."
# The .sha256 file uses the same basename as the tarball (produced by shasum -a
# 256 or sha256sum in bundle.sh). Change into TMPDIR so the relative path in
# the checksum file matches.
(
  cd "${TMPDIR_WORK}"
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 --check "${CHECKSUM_NAME}"
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum --check "${CHECKSUM_NAME}"
  else
    # Refuse to install an unverified download. macOS 10.13+ ships shasum and
    # every supported Linux has sha256sum (coreutils), so this is effectively
    # unreachable in normal use, but a missing tool must abort, not silently
    # install a possibly-tampered binary.
    echo "ERROR: neither shasum nor sha256sum found; cannot verify the download." >&2
    echo "Install coreutils (Linux) or use macOS 10.13+; then re-run. Aborting." >&2
    exit 1
  fi
)

# ---------------------------------------------------------------------------
# Extract binaries
# ---------------------------------------------------------------------------
echo "Extracting binaries..."
tar -xzf "${TARBALL_PATH}" -C "${TMPDIR_WORK}"

BIN_DIR="${TMPDIR_WORK}/${ASSET_NAME}/bin"
if [ ! -x "${BIN_DIR}/hauksbee" ] || [ ! -x "${BIN_DIR}/hauksbee-ci" ]; then
  echo "Unexpected tarball layout: bin/hauksbee or bin/hauksbee-ci not found." >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# Install to PREFIX/bin
# ---------------------------------------------------------------------------
INSTALL_DIR="${PREFIX}/bin"
mkdir -p "${INSTALL_DIR}"

install -m 0755 "${BIN_DIR}/hauksbee"    "${INSTALL_DIR}/hauksbee"
install -m 0755 "${BIN_DIR}/hauksbee-ci" "${INSTALL_DIR}/hauksbee-ci"

echo ""
echo "Installed:"
echo "  ${INSTALL_DIR}/hauksbee"
echo "  ${INSTALL_DIR}/hauksbee-ci"

# ---------------------------------------------------------------------------
# PATH hint
# ---------------------------------------------------------------------------
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*)
    ;;  # already on PATH, no hint needed
  *)
    echo ""
    echo "Add ${INSTALL_DIR} to your PATH to use the binaries:"
    echo ""
    echo "  # bash"
    echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.bashrc && source ~/.bashrc"
    echo ""
    echo "  # zsh"
    echo "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.zshrc && source ~/.zshrc"
    ;;
esac

# ---------------------------------------------------------------------------
# macOS Gatekeeper note
# ---------------------------------------------------------------------------
if [ "${OS}" = "Darwin" ]; then
  echo ""
  echo "macOS Gatekeeper note:"
  echo "  The binaries are not notarized. If macOS blocks them on first run,"
  echo "  remove the quarantine attribute:"
  echo ""
  echo "    xattr -d com.apple.quarantine \"${INSTALL_DIR}/hauksbee\""
  echo "    xattr -d com.apple.quarantine \"${INSTALL_DIR}/hauksbee-ci\""
  echo ""
  echo "  Or, to clear both at once:"
  echo "    xattr -d com.apple.quarantine \"${INSTALL_DIR}/hauksbee\" \"${INSTALL_DIR}/hauksbee-ci\""
fi

if [ -n "${SHAPE_SUFFIX}" ]; then
  LICENCE_LINE="Apache-2.0 binary (permissive build: no avr backend, no libsimavr, no GPL code)."
else
  LICENCE_LINE="GPL-3.0 binary (includes the avr backend, which links GPL-3.0 libsimavr); hauksbee's source is Apache-2.0."
fi

echo ""
echo "hauksbee ${VERSION} installed. Run: hauksbee --help"
echo "Licence: ${LICENCE_LINE}"
