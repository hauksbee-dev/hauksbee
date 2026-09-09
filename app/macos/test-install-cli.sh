#!/usr/bin/env bash
# Focused, host-independent test for install-cli.sh. It uses fake executable
# payloads, so it can run on CI without compiling the macOS app.
set -euo pipefail

command -v trash >/dev/null 2>&1 || {
  printf 'SKIP: trash is required for recoverable test cleanup\n'
  exit 0
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/hauksbee-cli-test.XXXXXX")"
cleanup() { trash "$TMP" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

APP="$TMP/Hauksbee.app"
SRC="$APP/Contents/Resources/bin"
mkdir -p "$SRC"
for binary in hauksbee hauksbee-ci hauksbee-mcp; do
  printf '#!/bin/sh\nprintf "%%s\\n" "%s"\n' "$binary-v1" > "$SRC/$binary"
  chmod 0755 "$SRC/$binary"
done

HELPER="$SRC/../install-cli.sh"
install -m 0755 "$ROOT/app/macos/install-cli.sh" "$HELPER"

PATH_BEFORE="$PATH"
OUTPUT="$("$HELPER" --prefix "$TMP/copy-prefix")"
[ "$PATH" = "$PATH_BEFORE" ] || { printf 'PATH changed\n' >&2; exit 1; }
case "$OUTPUT" in
  *"PATH was not changed"*) ;;
  *) printf 'helper did not report PATH policy\n' >&2; exit 1 ;;
esac
case "$OUTPUT" in
  *"export PATH="*":\$PATH"*) ;;
  *) printf 'helper did not print a usable PATH command\n' >&2; exit 1 ;;
esac
for binary in hauksbee hauksbee-ci hauksbee-mcp; do
  [ -f "$TMP/copy-prefix/bin/$binary" ]
  [ -x "$TMP/copy-prefix/bin/$binary" ]
  [ ! -L "$TMP/copy-prefix/bin/$binary" ]
done

# A later invocation refreshes a copied install.
printf '#!/bin/sh\nprintf "%%s\\n" "hauksbee-v2"\n' > "$SRC/hauksbee"
chmod 0755 "$SRC/hauksbee"
"$HELPER" --prefix "$TMP/copy-prefix" >/dev/null
[ "$("$TMP/copy-prefix/bin/hauksbee")" = "hauksbee-v2" ]

"$HELPER" --symlink --prefix "$TMP/link-prefix" >/dev/null
SOURCE_CANON="$(cd "$SRC" && pwd -P)"
for binary in hauksbee hauksbee-ci hauksbee-mcp; do
  [ -L "$TMP/link-prefix/bin/$binary" ]
  [ "$(readlink "$TMP/link-prefix/bin/$binary")" = "$SOURCE_CANON/$binary" ]
done

printf 'install-cli test: PASS\n'
