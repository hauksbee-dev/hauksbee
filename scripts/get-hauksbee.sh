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
#   ( set -o pipefail
#     set +x
#     export HAUKSBEE_GITHUB_TOKEN="$(secret-manager read hauksbee-read)"
#     export HAUKSBEE_INSTALLER_COMMIT=REPLACE_WITH_RELEASE_COMMIT_SHA
#     export HAUKSBEE_INSTALLER_VERSION=REPLACE_WITH_RELEASE_TAG
#     printf 'header = "Authorization: Bearer %s"\n' "$HAUKSBEE_GITHUB_TOKEN" |
#       curl -q --config - -fsSL "https://api.github.com/repos/hauksbee-dev/hauksbee/contents/scripts/get-hauksbee.sh?ref=$HAUKSBEE_INSTALLER_COMMIT" |
#       python3 -c 'import base64,json,sys; sys.stdout.write(base64.b64decode(json.load(sys.stdin)["content"]).decode())' | bash -s -- --version "$HAUKSBEE_INSTALLER_VERSION" )
#   With flags through the pipe:
#     printf ... | curl -q --config - -fsSL .../get-hauksbee.sh | bash -s -- --permissive
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
# Credentials must never become xtrace output. This intentionally overrides an
# inherited `SHELLOPTS=xtrace` or an explicit `bash -x` before reading them.
set +x
set -euo pipefail

REPO="hauksbee-dev/hauksbee"
# The API base is overridable so the installer can target a GitHub Enterprise
# host or the local contract server used by the regression test.
API_BASE="${HAUKSBEE_API_BASE:-https://api.github.com/repos/${REPO}}"
PRIVATE_TOKEN="${HAUKSBEE_GITHUB_TOKEN:-${GITHUB_TOKEN:-}}"
# The credential authorizes API reads only. Keep the captured value shell-local
# and prevent every downloaded binary/child process from inheriting it.
unset HAUKSBEE_GITHUB_TOKEN GITHUB_TOKEN GH_TOKEN

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
  ( set -o pipefail
    set +x
    export HAUKSBEE_GITHUB_TOKEN="$(secret-manager read hauksbee-read)"
    export HAUKSBEE_INSTALLER_COMMIT=REPLACE_WITH_RELEASE_COMMIT_SHA
    export HAUKSBEE_INSTALLER_VERSION=REPLACE_WITH_RELEASE_TAG
    printf 'header = "Authorization: Bearer %s"\n' "$HAUKSBEE_GITHUB_TOKEN" |
      curl -q --config - -fsSL "https://api.github.com/repos/hauksbee-dev/hauksbee/contents/scripts/get-hauksbee.sh?ref=$HAUKSBEE_INSTALLER_COMMIT" |
      python3 -c 'import base64,json,sys; sys.stdout.write(base64.b64decode(json.load(sys.stdin)["content"]).decode())' | bash -s -- --version "$HAUKSBEE_INSTALLER_VERSION" )
  With flags through the pipe:
    printf ... | curl -q --config - -fsSL .../get-hauksbee.sh | bash -s -- --permissive
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
if ! command -v gh >/dev/null 2>&1; then
  echo "GitHub CLI (gh) is required to verify signed release provenance." >&2
  exit 1
fi

# Feed the authorization header through curl's config stdin. Keeping the token
# out of curl's argv prevents it appearing in process listings or shell traces.
curl_private() {
  printf 'header = "Authorization: Bearer %s"\n' "$PRIVATE_TOKEN" \
    | curl -q --config - "$@"
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
    -H 'X-GitHub-Api-Version: 2026-03-10' \
    "$1"
}

release_tag() {
  python3 -c 'import json,sys; value=json.load(sys.stdin).get("tag_name"); value or sys.exit(1); print(value)'
}

release_is_immutable() {
  python3 -c 'import json,sys; sys.exit(0 if json.load(sys.stdin).get("immutable") is True else 1)'
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
if ! printf '%s' "$RELEASE_JSON" | release_is_immutable; then
  echo "Release ${VERSION} is not immutable; refusing replaceable private assets." >&2
  exit 1
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
TXN_DIR=""
INSTALL_COMMITTED=1
cleanup_on_exit() {
  if [ -n "$TXN_DIR" ] && [ "$INSTALL_COMMITTED" -eq 0 ]; then
    rollback_install 2>/dev/null || true
  fi
  find "$TMPDIR_WORK" -depth -mindepth 1 -delete 2>/dev/null || true
  rmdir "$TMPDIR_WORK" 2>/dev/null || true
  if [ -n "${INSTALL_LOCK:-}" ]; then
    release_install_lock 2>/dev/null || true
  fi
}
trap cleanup_on_exit EXIT
trap 'exit 130' INT TERM

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

# The checksum catches corruption, while the repository's immutable-release
# attestation binds the archive bytes to the protected tag and release.
RELEASE_SHA="$(GH_TOKEN="$PRIVATE_TOKEN" gh api "repos/${REPO}/commits/${VERSION}" --jq .sha)"
[[ "$RELEASE_SHA" =~ ^[0-9a-f]{40}$ ]] || {
  echo "Could not resolve ${VERSION} to one immutable source commit." >&2
  exit 1
}
if [ -n "${HAUKSBEE_INSTALLER_COMMIT:-}" ] \
  && [ "$HAUKSBEE_INSTALLER_COMMIT" != "$RELEASE_SHA" ]; then
  echo "Installer commit $HAUKSBEE_INSTALLER_COMMIT does not match release $VERSION ($RELEASE_SHA)." >&2
  exit 1
fi
GH_TOKEN="$PRIVATE_TOKEN" gh release verify-asset "$VERSION" "$TARBALL_PATH" \
  --repo "$REPO" >/dev/null
GH_TOKEN="$PRIVATE_TOKEN" gh release verify-asset "$VERSION" "$CHECKSUM_PATH" \
  --repo "$REPO" >/dev/null

validate_release_archive() {
  local archive="$1" member line kind
  while IFS= read -r member; do
    case "$member" in
      /*|\\*|[A-Za-z]:*|../*|*/../*|*/..) echo "Unsafe archive path: $member" >&2; return 1 ;;
    esac
  done < <(tar -tzf "$archive")
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    kind="${line:0:1}"
    case "$kind" in -|d) ;; *) echo "Unsafe archive member type: $line" >&2; return 1 ;; esac
  done < <(tar -tvzf "$archive")
}
validate_release_archive "$TARBALL_PATH" || {
  echo "Release archive contains a traversal, link, or special-file member; refusing extraction." >&2
  exit 1
}

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

# Prove the authenticated archive can actually start and identifies the one
# immutable release before touching any live destination. This keeps a healthy
# existing installation continuously available when an asset is malformed,
# unloadable, or internally mixed despite otherwise valid release metadata.
for b in ${BINARIES}; do
  actual_version="$("${BIN_DIR}/$b" --version 2>&1 || true)"
  expected_version="$b ${VERSION#v} (git $RELEASE_SHA)"
  if [ "$actual_version" != "$expected_version" ]; then
    echo "ERROR: staged $b reports '$actual_version', expected '$expected_version'; existing install left untouched." >&2
    exit 1
  fi
done

# ---------------------------------------------------------------------------
# Install to PREFIX/bin
# ---------------------------------------------------------------------------
INSTALL_DIR="${PREFIX}/bin"
mkdir -p "${INSTALL_DIR}"

# Serialize installers and recover one interrupted transaction before staging
# another. SIGKILL/power loss cannot run traps, so the durable journal is part
# of the startup protocol, not merely EXIT cleanup.
INSTALL_LOCK="${INSTALL_DIR}/.hauksbee-install.lock"
LOCK_OWNED=0
LOCK_TOKEN="$$-${RANDOM:-0}-$(date +%s)"
acquire_install_lock() {
  candidate_lock="${INSTALL_LOCK}.candidate-${LOCK_TOKEN}"
  printf '%s\n%s\n' "$$" "$LOCK_TOKEN" > "$candidate_lock"
  while :; do
    # A hard link publishes the already-complete owner record atomically. The
    # candidate lives beside the lock, so link(2) cannot cross filesystems.
    if ln "$candidate_lock" "$INSTALL_LOCK" 2>/dev/null; then
      find "$candidate_lock" -maxdepth 0 -type f -delete
      LOCK_OWNED=1
      return
    fi
    if [ ! -e "$INSTALL_LOCK" ]; then
      find "$candidate_lock" -maxdepth 0 -type f -delete 2>/dev/null || true
      echo "This install prefix cannot atomically acquire $INSTALL_LOCK; refusing instead of spinning or replacing without ownership." >&2
      exit 1
    fi
    owner="$(sed -n '1p' "$INSTALL_LOCK" 2>/dev/null || true)"
    if ! [[ "$owner" =~ ^[0-9]+$ ]]; then
      find "$candidate_lock" -maxdepth 0 -type f -delete 2>/dev/null || true
      echo "Install lock $INSTALL_LOCK has no valid owner PID; refusing ambiguous recovery." >&2
      exit 1
    fi
    # kill -0 can fail with EPERM for a live process owned by another user.
    # ps supplies the independent existence check; unknown remains owned.
    if kill -0 "$owner" 2>/dev/null \
        || ps -p "$owner" -o pid= 2>/dev/null | grep -Eq '[0-9]'; then
      find "$candidate_lock" -maxdepth 0 -type f -delete 2>/dev/null || true
      echo "Another hauksbee installer (pid $owner) owns $INSTALL_LOCK; refusing concurrent replacement." >&2
      exit 1
    fi
    find "$candidate_lock" -maxdepth 0 -type f -delete 2>/dev/null || true
    echo "Install lock owner pid $owner is absent, but automatic stale-lock reclamation is unsafe under concurrent installers." >&2
    echo "Inspect $INSTALL_LOCK and remove that one file explicitly before retrying." >&2
    exit 1
  done
}
release_install_lock() {
  [ "$LOCK_OWNED" -eq 1 ] || return 0
  [ "$(sed -n '2p' "$INSTALL_LOCK" 2>/dev/null || true)" = "$LOCK_TOKEN" ] || {
    echo "Install lock ownership changed unexpectedly; refusing to delete it." >&2
    LOCK_OWNED=0
    return 1
  }
  find "$INSTALL_LOCK" -maxdepth 0 -type f -delete 2>/dev/null || true
  LOCK_OWNED=0
}
recover_transaction() {
  journal="$1"
  if [ -e "$journal/committed" ]; then
    find "$journal" -depth -mindepth 1 -delete 2>/dev/null || true
    rmdir "$journal" 2>/dev/null || true
    return
  fi
  journal_binaries="$(find "$journal" -maxdepth 1 \( -type f -o -type l \) \( -name 'old-*' -o -name 'installing-*' \) -print \
    | sed -E 's#^.*/(old|installing)-##' | sort -u)"
  for b in ${journal_binaries}; do
    if [ -e "$journal/old-$b" ] || [ -L "$journal/old-$b" ]; then
      find "$INSTALL_DIR/$b" -maxdepth 0 \( -type f -o -type l \) -delete 2>/dev/null || true
      mv -f "$journal/old-$b" "$INSTALL_DIR/$b"
    elif [ -e "$journal/installing-$b" ] || [ -L "$journal/installing-$b" ]; then
      find "$INSTALL_DIR/$b" -maxdepth 0 \( -type f -o -type l \) -delete 2>/dev/null || true
    fi
  done
  find "$journal" -depth -mindepth 1 -delete 2>/dev/null || true
  rmdir "$journal" 2>/dev/null || true
}
acquire_install_lock
orphan_journals="$(find "$INSTALL_DIR" -maxdepth 1 -type d -name '.hauksbee-install.*' -print)"
orphan_count="$(printf '%s\n' "$orphan_journals" | sed '/^$/d' | wc -l | tr -d ' ')"
[ "$orphan_count" -le 1 ] || {
  echo "Multiple interrupted install journals exist; refusing ambiguous recovery." >&2
  release_install_lock
  exit 1
}
if [ "$orphan_count" -eq 1 ]; then
  recover_transaction "$orphan_journals"
fi
if [ "${HAUKSBEE_TEST_EXIT_AFTER_RECOVERY:-}" = 1 ]; then
  exit 75
fi

# One recoverable transaction directory lives on the destination filesystem.
# Old binaries remain there until every replacement starts successfully.
TXN_DIR="$(mktemp -d "${INSTALL_DIR}/.hauksbee-install.XXXXXX")"
INSTALL_COMMITTED=0
rollback_install() {
  recover_transaction "$TXN_DIR"
}
for b in ${BINARIES}; do
  if ! install -m 0755 "${BIN_DIR}/${b}" "$TXN_DIR/new-$b"; then
    echo "Failed to stage ${b} into ${INSTALL_DIR}; existing install left untouched." >&2
    exit 1
  fi
done
for b in ${BINARIES}; do
  if [ -e "$INSTALL_DIR/$b" ]; then
    mv -f "$INSTALL_DIR/$b" "$TXN_DIR/old-$b"
  fi
  # Record intent before the rename. If a signal lands after mv(1) succeeds
  # but before the shell runs another command, rollback still knows that a
  # fresh destination may need to be removed.
  : > "$TXN_DIR/installing-$b"
  if ! mv -f "$TXN_DIR/new-$b" "$INSTALL_DIR/$b"; then
    echo "Failed to move ${b} into place in ${INSTALL_DIR}." >&2
    rollback_install
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
# Post-install defense in depth: re-run the same probes from their final paths
# before committing the recovery journal. The preflight above preserves the old
# installation for malformed assets; this catches destination-filesystem or
# rename damage. Current release builds link libelf statically; retaining the
# final-path probes also catches older/private assets and other loader failures.
# ---------------------------------------------------------------------------
for b in ${BINARIES}; do
  actual_version="$("${INSTALL_DIR}/$b" --version 2>&1 || true)"
  expected_version="$b ${VERSION#v} (git $RELEASE_SHA)"
  if [ "$actual_version" != "$expected_version" ]; then
    echo "ERROR: installed $b reports '$actual_version', expected '$expected_version'." >&2
    exit 1
  fi
done
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

# Persist commit state before discarding backups. Recovery treats a committed
# journal as cleanup-only, so SIGKILL/power loss during deletion can never
# remove a verified live binary or attempt an impossible partial rollback.
: > "$TXN_DIR/committed"
INSTALL_COMMITTED=1
find "$TXN_DIR" -depth -mindepth 1 -delete
rmdir "$TXN_DIR"
TXN_DIR=""
release_install_lock

echo ""
echo "hauksbee ${VERSION} installed. Run: hauksbee --help"
echo "Licence: ${LICENCE_LINE}"
