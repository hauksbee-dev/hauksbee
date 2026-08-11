#!/usr/bin/env bash
# get-hauksbee.sh - download and install prebuilt hauksbee + hauksbee-ci binaries.
#
# Detects OS/arch, fetches the latest GitHub Release asset (or a pinned version),
# verifies the sha256 checksum, and installs hauksbee, hauksbee-ci and (when the
# release carries it) hauksbee-mcp to ~/.local/bin.
# Safe to re-run: an existing install is only overwritten once the checksum
# of the new download is verified.
#
# Usage:
#   ( export HAUKSBEE_GITHUB_TOKEN="$(secret-manager read hauksbee-read)"
#     printf 'header = "Authorization: Bearer %s"\n' "$HAUKSBEE_GITHUB_TOKEN" |
#       curl --config - -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.sh | bash )
#   With flags through the pipe:
#     printf ... | curl --config - -fsSL .../get-hauksbee.sh | bash -s -- --permissive
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
#   HAUKSBEE_GITHUB_TOKEN  Required for the private release repository. Use a
#                          fine-grained PAT or GitHub App installation token
#                          with Contents: read on hauksbee-dev/hauksbee.
#   GITHUB_TOKEN           Accepted as a CI-compatible fallback.
set -euo pipefail

REPO="hauksbee-dev/hauksbee"
# The API base is overridable so the installer can target a GitHub Enterprise
# host or the local contract server used by the regression test.
API_BASE="${HAUKSBEE_API_BASE:-https://api.github.com/repos/${REPO}}"
PRIVATE_TOKEN="${HAUKSBEE_GITHUB_TOKEN:-${GITHUB_TOKEN:-}}"

VERSION=""
PREFIX="${HOME}/.local"
# Shape: "" = the default download (avr included, GPL-3.0 binary),
# "-permissive" = the GPL-free download (no avr, Apache-2.0 binary).
SHAPE_SUFFIX=""

# Usage text lives in this heredoc, not in a sed over the script file: under
# `curl | bash` there is no script file on disk and BASH_SOURCE is unset, so
# reading the file would crash --help in exactly the mode the README documents.
usage() {
  cat <<'USAGE'
get-hauksbee.sh - download and install prebuilt hauksbee binaries.

Detects OS/arch, fetches the latest GitHub Release asset (or a pinned
version), verifies the sha256 checksum, and installs hauksbee, hauksbee-ci
and (when the release carries it) hauksbee-mcp to ~/.local/bin. Safe to
re-run: an existing install is only overwritten once the checksum of the new
download is verified.

Usage:
  ( export HAUKSBEE_GITHUB_TOKEN="$(secret-manager read hauksbee-read)"
    printf 'header = "Authorization: Bearer %s"\n' "$HAUKSBEE_GITHUB_TOKEN" |
      curl --config - -fsSL https://raw.githubusercontent.com/hauksbee-dev/hauksbee/main/scripts/get-hauksbee.sh | bash )
  With flags through the pipe:
    printf ... | curl --config - -fsSL .../get-hauksbee.sh | bash -s -- --permissive
  Or run locally:
    bash scripts/get-hauksbee.sh [--version v0.1.0] [--prefix ~/.local] [--permissive]

Options:
  --version TAG   Install a specific release tag (default: latest).
  --prefix DIR    Install binaries to DIR/bin (default: ~/.local).
  --permissive    Install the GPL-free build instead of the default one.
  --help          Show this help.

Which build you get:
  Default: the full build, AVR / ATmega co-simulation included. It statically
  links libsimavr, so THE BINARY is GPL-3.0 (hauksbee's source stays
  Apache-2.0). GPL-3.0 constrains redistributing the binary, not running it.
  --permissive: the same tool without the avr backend, so no GPL code is
  linked and the binary is Apache-2.0. Take it if you redistribute or embed
  hauksbee. It cannot do AVR co-sim; Renode and Espressif QEMU still work.
  Either way, LICENSE-BINARY.txt inside the tarball spells out the terms.

Environment:
  HAUKSBEE_GITHUB_TOKEN  Required. Fine-grained PAT or GitHub App installation
                         token with Contents: read on the private repository.
  GITHUB_TOKEN           Accepted as a CI-compatible fallback.

Dependencies:
  curl, CA certificates, Python 3, tar, and sha256sum or shasum.
USAGE
}

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
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1 (try --help)" >&2
      exit 1
      ;;
  esac
done

if [ -z "$PRIVATE_TOKEN" ]; then
  echo "HAUKSBEE_GITHUB_TOKEN is required to download from the private ${REPO} release repository." >&2
  echo "Use a fine-grained PAT or GitHub App installation token with Contents: read; do not put it in a URL." >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required to parse authenticated GitHub release metadata safely." >&2
  exit 1
fi

# Feed the authorization header through curl's config stdin. Keeping the token
# out of curl's argv prevents it appearing in process listings or shell traces.
curl_private() {
  printf 'header = "Authorization: Bearer %s"\n' "$PRIVATE_TOKEN" \
    | curl --config - "$@"
}

# ---------------------------------------------------------------------------
# Detect OS and architecture -> asset name suffix
# ---------------------------------------------------------------------------
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
  Linux)
    # The prebuilt Linux binaries are glibc builds. On musl systems (Alpine
    # and friends) they fail at load time with a misleading "not found" for
    # the binary itself, so refuse up front and point at the source build.
    if { ldd --version 2>&1 | grep -qi musl; } || ls /lib/ld-musl-* >/dev/null 2>&1; then
      echo "This system uses musl libc; the prebuilt binaries are glibc-linked and will not run." >&2
      echo "Build from source instead: https://github.com/${REPO}#quickstart" >&2
      exit 1
    fi
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
    # A shell running under Rosetta 2 reports x86_64 from `uname -m` even on
    # Apple Silicon. Installing the Intel build there works but runs
    # translated forever; detect the translation and take the native arm64
    # asset instead.
    if [ "${ARCH}" = "x86_64" ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
      echo "Rosetta 2 detected (this shell is x86_64-translated on Apple Silicon); installing the native arm64 build."
      ARCH="arm64"
    fi
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
# Resolve one release metadata document (latest or pinned). Asset bytes are
# subsequently fetched only through the API URLs in this authenticated response.
# ---------------------------------------------------------------------------
fetch_release() {
  curl_private -fsSL \
    -H 'Accept: application/vnd.github+json' \
    -H 'X-GitHub-Api-Version: 2022-11-28' \
    "$1"
}

release_tag() {
  python3 -c 'import json,sys; value=json.load(sys.stdin).get("tag_name"); value or sys.exit(1); print(value)'
}

release_asset_url() {
  local name="$1"
  python3 -c '
import json, sys
name = sys.argv[1]
matches = [asset.get("url") for asset in json.load(sys.stdin).get("assets", []) if asset.get("name") == name]
if len(matches) != 1 or not isinstance(matches[0], str):
    sys.exit(1)
print(matches[0])
' "$name"
}

if [ -z "${VERSION}" ]; then
  echo "Fetching latest release tag..."
  if ! RELEASE_JSON="$(fetch_release "${API_BASE}/releases/latest")" \
    || ! VERSION="$(printf '%s' "$RELEASE_JSON" | release_tag)"; then
    echo "Could not determine the latest release tag from the GitHub API." >&2
    echo "Check your network connection and https://www.githubstatus.com," >&2
    echo "or pass --version vX.Y.Z explicitly (releases are listed at" >&2
    echo "https://github.com/${REPO}/releases)." >&2
    exit 1
  fi
else
  if ! RELEASE_JSON="$(fetch_release "${API_BASE}/releases/tags/${VERSION}")"; then
    echo "Could not read release ${VERSION} from the authenticated GitHub API." >&2
    exit 1
  fi
  if ! RESOLVED_TAG="$(printf '%s' "$RELEASE_JSON" | release_tag)" \
    || [ "$RESOLVED_TAG" != "$VERSION" ]; then
    echo "The GitHub API response did not identify the requested release ${VERSION}." >&2
    exit 1
  fi
fi

# Strip a leading 'v' to match the asset naming convention used in bundle.sh
# (bundle.sh strips the 'v' from the version when naming the tarball).
VERSION_BARE="${VERSION#v}"
echo "Installing hauksbee ${VERSION} (${VERSION_BARE})"

# ---------------------------------------------------------------------------
# Resolve exact asset API URLs from the authenticated release response.
# ---------------------------------------------------------------------------
ASSET_NAME="hauksbee-${VERSION_BARE}-${ASSET_SUFFIX}${SHAPE_SUFFIX}"
TARBALL_NAME="${ASSET_NAME}.tar.gz"
CHECKSUM_NAME="${TARBALL_NAME}.sha256"
if ! TARBALL_URL="$(printf '%s' "$RELEASE_JSON" | release_asset_url "$TARBALL_NAME")" \
  || ! CHECKSUM_URL="$(printf '%s' "$RELEASE_JSON" | release_asset_url "$CHECKSUM_NAME")"; then
  echo "Release ${VERSION} does not contain exactly one ${TARBALL_NAME} and checksum asset." >&2
  exit 1
fi
for api_url in "$TARBALL_URL" "$CHECKSUM_URL"; do
  case "$api_url" in
    "${API_BASE}/releases/assets/"*) ;;
    *)
      echo "Refusing release asset URL outside the configured GitHub API: ${api_url}" >&2
      exit 1
      ;;
  esac
done

# ---------------------------------------------------------------------------
# Download to a temp directory; verify; then install
# ---------------------------------------------------------------------------
# Strip any trailing slash from TMPDIR first: mktemp would otherwise build a
# doubled-slash template (e.g. /var/tmp//get-hauksbee.XXXXXX).
TMPDIR_BASE="${TMPDIR:-/tmp}"
TMPDIR_BASE="${TMPDIR_BASE%/}"
TMPDIR_WORK="$(mktemp -d "${TMPDIR_BASE}/get-hauksbee.XXXXXX")"
trap 'rm -rf "${TMPDIR_WORK}"' EXIT
# An interrupt mid-staging must not leave .hauksbee.new.$$ files behind.
trap 'rm -rf "${TMPDIR_WORK}"; cleanup_staged 2>/dev/null || true; exit 130' INT TERM

TARBALL_PATH="${TMPDIR_WORK}/${TARBALL_NAME}"
CHECKSUM_PATH="${TMPDIR_WORK}/${CHECKSUM_NAME}"

# download_asset URL DEST WHAT: a curl wrapper that names the failing URL and
# what to check, instead of dying with a bare non-zero under `set -e`.
download_asset() {
  local url="$1" dest="$2" what="$3"
  if ! curl_private -fsSL --retry 3 --retry-delay 2 \
    -H 'Accept: application/octet-stream' -o "${dest}" "${url}"; then
    echo "Failed to download ${what}:" >&2
    echo "  ${url}" >&2
    echo "Check your network connection, and that the release ${VERSION} exists" >&2
    echo "with a ${ASSET_SUFFIX}${SHAPE_SUFFIX} asset: https://github.com/${REPO}/releases" >&2
    exit 1
  fi
}

echo "Downloading ${TARBALL_NAME}..."
download_asset "${TARBALL_URL}" "${TARBALL_PATH}" "the release tarball"

echo "Downloading checksum..."
download_asset "${CHECKSUM_URL}" "${CHECKSUM_PATH}" "the sha256 checksum"

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

# hauksbee-mcp ships in current bundles; older releases lack it. Install it
# when present, and say nothing scary when it is not.
BINARIES="hauksbee hauksbee-ci"
if [ -x "${BIN_DIR}/hauksbee-mcp" ]; then
  BINARIES="${BINARIES} hauksbee-mcp"
fi

# ---------------------------------------------------------------------------
# Install to PREFIX/bin
# ---------------------------------------------------------------------------
INSTALL_DIR="${PREFIX}/bin"
mkdir -p "${INSTALL_DIR}"

# Two phases so the install is all-or-nothing: stage every binary under a
# temp name INSIDE the final directory first (same filesystem, so the later
# mv is an atomic rename), then rename them into place. A failure mid-way
# through staging leaves the existing install untouched; it can never leave a
# mixed old/new pair.
STAGED=""
cleanup_staged() {
  for _s in ${STAGED}; do rm -f "${_s}"; done
}
for b in ${BINARIES}; do
  staged="${INSTALL_DIR}/.${b}.new.$$"
  if ! install -m 0755 "${BIN_DIR}/${b}" "${staged}"; then
    echo "Failed to stage ${b} into ${INSTALL_DIR}; existing install left untouched." >&2
    cleanup_staged
    exit 1
  fi
  STAGED="${STAGED} ${staged}"
done
for b in ${BINARIES}; do
  if ! mv -f "${INSTALL_DIR}/.${b}.new.$$" "${INSTALL_DIR}/${b}"; then
    echo "Failed to move ${b} into place in ${INSTALL_DIR}." >&2
    cleanup_staged
    exit 1
  fi
done

echo ""
echo "Installed:"
for b in ${BINARIES}; do
  echo "  ${INSTALL_DIR}/${b}"
done

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
  echo "  Release binaries are signed with a Developer ID identity and, from"
  echo "  launch onward, notarized, so macOS runs them without complaint. If a"
  echo "  pre-release or dev bundle is blocked on first run, remove the"
  echo "  quarantine attribute:"
  echo ""
  for b in ${BINARIES}; do
    echo "    xattr -d com.apple.quarantine \"${INSTALL_DIR}/${b}\""
  done
fi

if [ -n "${SHAPE_SUFFIX}" ]; then
  LICENCE_LINE="Apache-2.0 binary (permissive build: no avr backend, no libsimavr, no GPL code)."
else
  LICENCE_LINE="GPL-3.0 binary (includes the avr backend, which links GPL-3.0 libsimavr); hauksbee's source is Apache-2.0."
fi

# ---------------------------------------------------------------------------
# Post-install smoke test: an installed binary that cannot start is a failed
# install, not a success. The default (avr) shape links libelf dynamically,
# and minimal Linux images do not ship it.
# ---------------------------------------------------------------------------
if ! "${INSTALL_DIR}/hauksbee" --version >/dev/null 2>&1; then
  echo "" >&2
  echo "ERROR: installed, but ${INSTALL_DIR}/hauksbee cannot start on this system." >&2
  "${INSTALL_DIR}/hauksbee" --version 2>&1 | head -2 | sed 's/^/  /' >&2 || true
  smoke_out="$("${INSTALL_DIR}/hauksbee" --version 2>&1 || true)"
  if printf '%s' "${smoke_out}" | grep -q 'libelf'; then
    echo "  The default download needs the system libelf runtime:" >&2
    echo "    Debian/Ubuntu:  apt-get install libelf1" >&2
    echo "    Fedora/RHEL:    dnf install elfutils-libelf" >&2
    echo "  Or take the Apache-2.0 build, which has no such dependency:" >&2
    echo "    re-run this installer with:  --permissive" >&2
  fi
  exit 1
fi

echo ""
echo "hauksbee ${VERSION} installed. Run: hauksbee --help"
echo "Licence: ${LICENCE_LINE}"
