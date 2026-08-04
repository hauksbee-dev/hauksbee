#!/usr/bin/env bash
# build.sh - produce demo/embed-dist: the embeddable "try Hauksbee" widget plus
# the recorded assets it replays.
#
# Deliberately separate from every other build in the repo:
#   frontend/dist    the live app, served by the engine binary
#   demo/dist        hauksbee.dev's own demo page
#   demo/embed-dist  this: a widget a landing page drops in
#
# Output layout (copy the whole directory into the site's public/):
#   hauksbee-embed.js   the host-page module (import it, or use the data-attr)
#   iframe.html         the isolated shape
#   widget-*.js         the widget itself (lazy)
#   test.html           the harness the Playwright pass drives
#   favicon.svg         the app's wordmark asks for this at the site root too
#   sessions/           the recorded sessions and the interaction cache
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
EMBED="$ROOT/demo/embed"
OUT="$ROOT/demo/embed-dist"

if [ ! -f "$ROOT/demo/sessions/cache/index.json" ]; then
  echo "error: no recorded interaction cache (demo/sessions/cache/index.json missing)." >&2
  echo "       run scripts/capture-demo-sessions.sh first." >&2
  exit 1
fi

# demo/ has no node_modules; the app's dependencies (and this build's plugins)
# live in frontend's. Linked, not copied, and gitignored.
ln -sfn "$ROOT/frontend/node_modules" "$EMBED/node_modules"

echo "==> building the embed bundle"
cd "$EMBED"
bunx vite build --config "$EMBED/vite.config.ts"

echo "==> copying the recorded assets"
mkdir -p "$OUT/sessions"
# Sessions the cache index actually references, and the cache itself. The demo
# ladder may carry scenarios the widget does not offer; those are not shipped.
cp -R "$ROOT/demo/sessions/cache" "$OUT/sessions/cache"
cp "$ROOT/demo/sessions/manifest.json" "$OUT/sessions/manifest.json"
for board in $(bun -e '
  const idx = require(process.argv[1]);
  console.log(idx.boards.map(b => b.id).join(" "));
' "$ROOT/demo/sessions/cache/index.json"); do
  mkdir -p "$OUT/sessions/$board"
  cp -R "$ROOT/demo/sessions/$board/." "$OUT/sessions/$board/"
done

# The app's sidebar wordmark asks for /favicon.svg (root-absolute). Shipped here
# for the harness; the host site needs one at its own root (see demo/EMBED.md).
cp "$ROOT/frontend/public/favicon.svg" "$OUT/favicon.svg"
cp "$EMBED/test.html" "$OUT/test.html"

echo "==> demo/embed-dist ready:"
du -sh "$OUT" "$OUT/sessions"
echo
echo "    initial payload (what a host loads before any board):"
for f in "$OUT/hauksbee-embed.js" "$OUT/iframe.html"; do
  printf "      %-28s %6s raw  %6s gz\n" "$(basename "$f")" \
    "$(du -h "$f" | cut -f1)" "$(gzip -c "$f" | wc -c | awk '{printf "%.0fK", $1/1024}')"
done
for f in "$OUT"/chunks/*.js; do
  printf "      %-28s %6s raw  %6s gz\n" "$(basename "$f")" \
    "$(du -h "$f" | cut -f1)" "$(gzip -c "$f" | wc -c | awk '{printf "%.0fK", $1/1024}')"
done
