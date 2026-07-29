#!/usr/bin/env bash
# test-install-mock.sh - prove the get-hauksbee.sh download/verify/install flow
# end to end WITHOUT a public release, by mocking GitHub's release endpoints
# locally. Lets us validate the install "links and all that" while the repo
# stays private.
#
# What it does:
#   1. Builds the real distributable bundle (scripts/bundle.sh) for THIS platform.
#   2. Lays out a static tree mirroring GitHub's exact URL paths:
#        <api>/releases/latest                         (tag_name JSON)
#        <releases>/download/<tag>/<asset>.tar.gz      (the real bundle)
#        <releases>/download/<tag>/<asset>.tar.gz.sha256
#        /raw/get-hauksbee.sh                          (for the curl|bash hop)
#   3. Serves it with `python3 -m http.server`.
#   4. Runs get-hauksbee.sh against the mock (via curl|bash, like the README) into
#      a throwaway prefix, with HAUKSBEE_API_BASE / HAUKSBEE_RELEASES_BASE pointed
#      at the mock.
#   5. Verifies the freshly-installed binary actually runs, then tears down.
#
# Usage: scripts/test-install-mock.sh [--port N] [--no-build] [--version vX.Y.Z]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
PORT=8091
BUILD_FLAG=""          # passed through to bundle.sh (e.g. --no-build)
TAG="v0.1.0"

while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="${2:?}"; shift 2 ;;
    --no-build) BUILD_FLAG="--no-build"; shift ;;
    --version) TAG="${2:?}"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; exit 0 ;;
    *) echo "unknown arg: $1" >&2; exit 1 ;;
  esac
done

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
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "==> Building bundle ($TARGET)"
bash "$HERE/bundle.sh" $BUILD_FLAG --version "$VERSION_BARE" --target "$TARGET" >/dev/null
TARBALL="$ROOT/dist/${ASSET}.tar.gz"
[ -f "$TARBALL" ] || { echo "FAIL: bundle not produced at $TARBALL" >&2; exit 1; }

echo "==> Laying out mock GitHub tree"
mkdir -p "$MOCK/repos/${REPO}/releases" \
         "$MOCK/${REPO}/releases/download/${TAG}" \
         "$MOCK/raw"
printf '{"tag_name":"%s","name":"hauksbee %s"}\n' "$TAG" "$TAG" \
  > "$MOCK/repos/${REPO}/releases/latest"
cp "$TARBALL" "$TARBALL.sha256" "$MOCK/${REPO}/releases/download/${TAG}/"
cp "$HERE/get-hauksbee.sh" "$MOCK/raw/get-hauksbee.sh"

echo "==> Serving mock at http://127.0.0.1:${PORT}"
( cd "$MOCK" && exec python3 -m http.server "$PORT" >/dev/null 2>&1 ) &
SRV_PID=$!
# Wait for the server to answer.
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:${PORT}/repos/${REPO}/releases/latest" >/dev/null 2>&1; then break; fi
  sleep 0.2
done

echo "==> Running the README curl|bash install against the mock"
export HAUKSBEE_API_BASE="http://127.0.0.1:${PORT}/repos/${REPO}"
export HAUKSBEE_RELEASES_BASE="http://127.0.0.1:${PORT}/${REPO}/releases/download"
curl -fsSL "http://127.0.0.1:${PORT}/raw/get-hauksbee.sh" | bash -s -- --prefix "$PREFIX"

echo "==> Verifying the installed binaries run"
"$PREFIX/bin/hauksbee" --version
[ -x "$PREFIX/bin/hauksbee-ci" ] || { echo "FAIL: hauksbee-ci not installed" >&2; exit 1; }
"$PREFIX/bin/hauksbee" run "$ROOT/crates/hauksbee-ci/examples/boards/blinky.kicad_pcb" --drc --plain >/dev/null

echo ""
echo "PASS: get-hauksbee.sh installed and ran from a mocked private release."
