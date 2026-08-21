#!/usr/bin/env bash
# Install the command-line tools shipped inside Hauksbee.app.
#
# This helper is deliberately user-invoked: it never edits PATH, shell startup
# files, /usr/local, or invokes sudo. Copies are the default so an app update
# or relocation does not invalidate an installed command.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: install-cli.sh [--app PATH] [--prefix DIR] [--copy|--symlink]

Install hauksbee, hauksbee-ci, and hauksbee-mcp from Hauksbee.app into
DIR/bin. The default prefix is ~/Library/Application Support/Hauksbee.
No PATH or shell startup file is changed. --symlink is opt-in and follows the
app's current location; the default is to copy the binaries.
USAGE
}

APP=""
PREFIX="${HOME}/Library/Application Support/Hauksbee"
MODE=copy
while [ $# -gt 0 ]; do
  case "$1" in
    --app) APP="${2:?--app needs a path}"; shift 2 ;;
    --app=*) APP="${1#*=}"; shift ;;
    --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
    --prefix=*) PREFIX="${1#*=}"; shift ;;
    --copy) MODE=copy; shift ;;
    --symlink) MODE=symlink; shift ;;
    -h|--help) usage; exit 0 ;;
    *) printf 'error: unknown argument %s\n\n' "$1" >&2; usage >&2; exit 2 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)"
if [ -z "$APP" ]; then
  # Installed helper: Contents/Resources/install-cli.sh -> enclosing .app.
  APP="$(cd "$SCRIPT_DIR/../.." && pwd -P)"
else
  APP="$(cd "$APP" && pwd -P)"
fi
case "$APP" in
  *.app) ;;
  *) printf 'error: app path must end in .app: %s\n' "$APP" >&2; exit 2 ;;
esac

SRC_DIR="$APP/Contents/Resources/bin"
[ -d "$SRC_DIR" ] || { printf 'error: missing app CLI directory: %s\n' "$SRC_DIR" >&2; exit 1; }

case "$PREFIX" in
  /*) ;;
  *) printf 'error: --prefix must be an absolute path: %s\n' "$PREFIX" >&2; exit 2 ;;
esac
PREFIX="$(mkdir -p "$PREFIX" && cd "$PREFIX" && pwd -P)"
DEST_DIR="$PREFIX/bin"
case "$DEST_DIR/" in
  "$APP"/*) printf 'error: refusing to install inside the app bundle\n' >&2; exit 2 ;;
esac

command -v install >/dev/null 2>&1 || { printf 'error: install command not found\n' >&2; exit 1; }

BINARIES=(hauksbee hauksbee-ci hauksbee-mcp)
for binary in "${BINARIES[@]}"; do
  source="$SRC_DIR/$binary"
  [ -f "$source" ] && [ ! -L "$source" ] && [ -x "$source" ] \
    || { printf 'error: missing executable: %s\n' "$source" >&2; exit 1; }
done

mkdir -p "$DEST_DIR"
for binary in "${BINARIES[@]}"; do
  [ -d "$DEST_DIR/$binary" ] \
    && { printf 'error: refusing to replace directory: %s\n' "$DEST_DIR/$binary" >&2; exit 2; }
done
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/hauksbee-cli.XXXXXX")"
cleanup() {
  # The stage is in the user's temporary directory, never in the app or
  # install prefix. Use the optional `trash` utility when available; rmdir is
  # sufficient after a successful move and avoids requiring extra software.
  if [ -d "$STAGE" ]; then
    if command -v trash >/dev/null 2>&1; then
      trash "$STAGE" >/dev/null 2>&1 || true
    else
      rmdir "$STAGE" >/dev/null 2>&1 || true
    fi
  fi
}
trap cleanup EXIT INT TERM

for binary in "${BINARIES[@]}"; do
  source="$SRC_DIR/$binary"
  if [ "$MODE" = copy ]; then
    install -m 0755 "$source" "$STAGE/$binary"
  else
    ln -s "$source" "$STAGE/$binary"
  fi
done
for binary in "${BINARIES[@]}"; do
  # Replacing the directory entry avoids following an old destination symlink.
  mv -f "$STAGE/$binary" "$DEST_DIR/$binary"
done

trap - EXIT INT TERM
cleanup

printf 'Installed Hauksbee CLI (%s):\n' "$MODE"
for binary in "${BINARIES[@]}"; do
  printf '  %s\n' "$DEST_DIR/$binary"
done
printf '\nPATH was not changed. To use these commands in this shell:\n'
printf "  export PATH=%q:\$PATH\n" "$DEST_DIR"
printf '\nCopies are independent of app relocation; rerun this helper after an app update.\n'
if [ "$MODE" = symlink ]; then
  printf 'Symlink mode follows this app path and will break if the app moves or is replaced.\n'
fi
