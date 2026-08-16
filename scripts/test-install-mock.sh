#!/usr/bin/env bash
# test-install-mock.sh - prove the get-hauksbee.sh download/verify/install flow
# end to end WITHOUT touching a live release, by mocking GitHub's release
# endpoints locally. Covers the tokenless public default and the optional
# with-token path.
#
# What it does:
#   1. Builds the real distributable bundle (scripts/bundle.sh) for THIS platform.
#   2. Lays out a static tree mirroring GitHub's exact URL paths:
#        <api>/releases/latest                         (tag + asset API URLs)
#        <api>/releases/assets/<id>                    (bundle/checksum bytes)
#        /raw/get-hauksbee.sh                          (for the curl|bash hop)
#   3. Serves it with `python3 -m http.server`.
#   4. Runs get-hauksbee.sh against the mock (via curl|bash, like the README) into
#      a throwaway prefix, with HAUKSBEE_API_BASE pointed at the mock.
#   5. Verifies the freshly-installed binaries actually run.
#   6. Negative test: serves a CORRUPTED tarball next to the genuine checksum
#      and asserts the installer refuses it and installs nothing.
#   Then tears down.
#
# Prerequisites: cargo (bundle.sh builds the release binaries unless
# --no-build), python3 (the mock HTTP server), curl, tar, shasum/sha256sum.
#
# Usage: scripts/test-install-mock.sh [--port N] [--no-build] [--version vX.Y.Z]
#                                     [--shape permissive]
#
#   --shape permissive   ALSO build the permissive bundle, publish it in the
#                        mock, and run a second install with --permissive into
#                        its own prefix. The default flow is unchanged.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
PORT=8091
BUILD_FLAG=""          # passed through to bundle.sh (e.g. --no-build)
TAG="v0.1.0"

SHAPE="default"
while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="${2:?}"; shift 2 ;;
    --no-build) BUILD_FLAG="--no-build"; shift ;;
    --version) TAG="${2:?}"; shift 2 ;;
    --shape) SHAPE="${2:?--shape needs default or permissive}"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done
case "$SHAPE" in default|permissive) ;; *) echo "unknown --shape: $SHAPE" >&2; exit 1 ;; esac

VERSION_BARE="${TAG#v}"
REPO="hauksbee-dev/hauksbee"
OS="$(uname -s)"; ARCH="$(uname -m)"
TARGET="$(echo "$OS" | tr '[:upper:]' '[:lower:]')-${ARCH}"
ASSET="hauksbee-${VERSION_BARE}-${TARGET}"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/hbinstall-mock.XXXXXX")"
PREFIX="$WORK/prefix"
MOCK="$WORK/ghmock"
SRV_PID=""
cleanup() {
  [ -n "$SRV_PID" ] && kill "$SRV_PID" 2>/dev/null || true
  find "$WORK" -depth -mindepth 1 -delete 2>/dev/null || true
  rmdir "$WORK" 2>/dev/null || true
}
trap cleanup EXIT

echo "==> Building bundle ($TARGET)"
bash "$HERE/bundle.sh" $BUILD_FLAG --version "$VERSION_BARE" --target "$TARGET" >/dev/null
TARBALL="$ROOT/dist/${ASSET}.tar.gz"
[ -f "$TARBALL" ] || { echo "FAIL: bundle not produced at $TARBALL" >&2; exit 1; }

if [ "$SHAPE" = permissive ]; then
  echo "==> Building permissive bundle ($TARGET)"
  bash "$HERE/bundle.sh" $BUILD_FLAG --shape permissive \
    --version "$VERSION_BARE" --target "$TARGET" >/dev/null
  TARBALL_PERM="$ROOT/dist/${ASSET}-permissive.tar.gz"
  [ -f "$TARBALL_PERM" ] || { echo "FAIL: permissive bundle not produced at $TARBALL_PERM" >&2; exit 1; }
fi

echo "==> Laying out mock GitHub tree"
# The mock mirrors the real API contract the installer depends on: an
# immutable release document whose assets carry GitHub's server-side sha256
# digests, plus the /commits/<tag> endpoint that resolves the tag to the one
# source commit the staged binaries must attest.
RELEASE_SHA="$(git -C "$ROOT" rev-parse HEAD)"
sha256_of() {
  if command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}';
  else sha256sum "$1" | awk '{print $1}'; fi
}
mkdir -p "$MOCK/repos/${REPO}/releases/assets" \
         "$MOCK/repos/${REPO}/commits" \
         "$MOCK/raw"
printf '{"sha":"%s"}\n' "$RELEASE_SHA" > "$MOCK/repos/${REPO}/commits/${TAG}"
{
  printf '{"tag_name":"%s","immutable":true,"assets":[' "$TAG"
  printf '{"name":"%s.tar.gz","digest":"sha256:%s","url":"http://127.0.0.1:%s/repos/%s/releases/assets/101"}' \
    "$ASSET" "$(sha256_of "$TARBALL")" "$PORT" "$REPO"
  printf ',{"name":"%s.tar.gz.sha256","digest":"sha256:%s","url":"http://127.0.0.1:%s/repos/%s/releases/assets/102"}' \
    "$ASSET" "$(sha256_of "$TARBALL.sha256")" "$PORT" "$REPO"
  if [ "$SHAPE" = permissive ]; then
    printf ',{"name":"%s-permissive.tar.gz","digest":"sha256:%s","url":"http://127.0.0.1:%s/repos/%s/releases/assets/103"}' \
      "$ASSET" "$(sha256_of "$TARBALL_PERM")" "$PORT" "$REPO"
    printf ',{"name":"%s-permissive.tar.gz.sha256","digest":"sha256:%s","url":"http://127.0.0.1:%s/repos/%s/releases/assets/104"}' \
      "$ASSET" "$(sha256_of "$TARBALL_PERM.sha256")" "$PORT" "$REPO"
  fi
  printf ']}\n'
} > "$MOCK/repos/${REPO}/releases/latest"
cp "$TARBALL" "$MOCK/repos/${REPO}/releases/assets/101"
cp "$TARBALL.sha256" "$MOCK/repos/${REPO}/releases/assets/102"
if [ "$SHAPE" = permissive ]; then
  cp "$TARBALL_PERM" "$MOCK/repos/${REPO}/releases/assets/103"
  cp "$TARBALL_PERM.sha256" "$MOCK/repos/${REPO}/releases/assets/104"
fi
cp "$HERE/get-hauksbee.sh" "$MOCK/raw/get-hauksbee.sh"

# The negative-test tree: the SAME release document (genuine digests and
# checksum), but the served tarball bytes are corrupted afterwards, the way a
# tampered or truncated download would arrive. Served under bad/ so the URL
# bases select it.
echo "==> Laying out the corrupted mock tree (negative test)"
mkdir -p "$MOCK/bad/repos/${REPO}/releases/assets" \
         "$MOCK/bad/repos/${REPO}/commits"
printf '{"sha":"%s"}\n' "$RELEASE_SHA" > "$MOCK/bad/repos/${REPO}/commits/${TAG}"
printf '{"tag_name":"%s","immutable":true,"assets":[{"name":"%s.tar.gz","digest":"sha256:%s","url":"http://127.0.0.1:%s/bad/repos/%s/releases/assets/201"},{"name":"%s.tar.gz.sha256","digest":"sha256:%s","url":"http://127.0.0.1:%s/bad/repos/%s/releases/assets/202"}]}\n' \
  "$TAG" "$ASSET" "$(sha256_of "$TARBALL")" "$PORT" "$REPO" \
  "$ASSET" "$(sha256_of "$TARBALL.sha256")" "$PORT" "$REPO" \
  > "$MOCK/bad/repos/${REPO}/releases/latest"
cp "$TARBALL.sha256" "$MOCK/bad/repos/${REPO}/releases/assets/202"
cp "$TARBALL" "$MOCK/bad/repos/${REPO}/releases/assets/201"
printf 'corrupt' >> "$MOCK/bad/repos/${REPO}/releases/assets/201"

echo "==> Serving mock at http://127.0.0.1:${PORT}"
( cd "$MOCK" && exec python3 -m http.server "$PORT" >/dev/null 2>&1 ) &
SRV_PID=$!
# Wait for the server to answer.
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:${PORT}/repos/${REPO}/releases/latest" >/dev/null 2>&1; then break; fi
  sleep 0.2
done

echo "==> Running the README curl|bash install against the mock (tokenless, the public default)"
export HAUKSBEE_API_BASE="http://127.0.0.1:${PORT}/repos/${REPO}"
unset HAUKSBEE_GITHUB_TOKEN GITHUB_TOKEN GH_TOKEN 2>/dev/null || true
curl -fsSL "http://127.0.0.1:${PORT}/raw/get-hauksbee.sh" | bash -s -- --prefix "$PREFIX"

echo "==> Running the install again WITH a token (rate-limit / private-mirror path)"
PREFIX_TOKEN="$WORK/prefix-token"
HAUKSBEE_GITHUB_TOKEN="mock-optional-token" \
  bash "$HERE/get-hauksbee.sh" --prefix "$PREFIX_TOKEN"
"$PREFIX_TOKEN/bin/hauksbee" --version

echo "==> Verifying the installed binaries run"
"$PREFIX/bin/hauksbee" --version
[ -x "$PREFIX/bin/hauksbee-ci" ] || { echo "FAIL: hauksbee-ci not installed" >&2; exit 1; }
[ -x "$PREFIX/bin/hauksbee-mcp" ] || { echo "FAIL: hauksbee-mcp not installed" >&2; exit 1; }
"$PREFIX/bin/hauksbee" run "$ROOT/crates/hauksbee-ci/examples/boards/blinky.kicad_pcb" --drc --plain >/dev/null

if [ "$SHAPE" = permissive ]; then
  echo "==> Running the install again with --permissive"
  PREFIX_PERM="$WORK/prefix-permissive"
  curl -fsSL "http://127.0.0.1:${PORT}/raw/get-hauksbee.sh" \
    | bash -s -- --prefix "$PREFIX_PERM" --permissive
  echo "==> Verifying the permissive install"
  "$PREFIX_PERM/bin/hauksbee" --version
  [ -x "$PREFIX_PERM/bin/hauksbee-ci" ]  || { echo "FAIL: permissive hauksbee-ci not installed" >&2; exit 1; }
  [ -x "$PREFIX_PERM/bin/hauksbee-mcp" ] || { echo "FAIL: permissive hauksbee-mcp not installed" >&2; exit 1; }
  # The permissive binary must have the avr backend compiled out. Capture the
  # doctor output first: doctor exits non-zero when external backends are
  # absent, and under pipefail that would fail the pipeline even on a match.
  perm_doctor="$("$PREFIX_PERM/bin/hauksbee" doctor 2>&1 || true)"
  printf '%s' "$perm_doctor" | grep -qE '^avr[[:space:]]+disabled' \
    || { echo "FAIL: permissive install does not report avr as disabled" >&2; exit 1; }
fi

echo "==> Negative test: corrupted tarball must be refused, nothing installed"
PREFIX_BAD="$WORK/prefix-bad"
if HAUKSBEE_API_BASE="http://127.0.0.1:${PORT}/bad/repos/${REPO}" \
   bash "$HERE/get-hauksbee.sh" --prefix "$PREFIX_BAD" >/dev/null 2>&1; then
  echo "FAIL: the installer accepted a tarball whose sha256 does not match" >&2
  exit 1
fi
for b in hauksbee hauksbee-ci hauksbee-mcp; do
  if [ -e "$PREFIX_BAD/bin/$b" ]; then
    echo "FAIL: corrupted download still installed $b" >&2
    exit 1
  fi
done
echo "    refused, and nothing was installed. Good."

echo ""
echo "PASS: get-hauksbee.sh installed and ran from a mocked public release."
