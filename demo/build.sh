#!/usr/bin/env bash
# build.sh - produce demo/dist: the VITE_DEMO=1 build of the frontend plus
# the recorded sessions, ready for `wrangler dev` locally or, on launch day,
# the `demo` phase of scripts/make-public.sh (`wrangler deploy`).
#
# Deliberately separate from `bun run build`: the live app's bundle lives in
# frontend/dist and is served by the engine binary; this build must never
# overwrite it.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEMO="$ROOT/demo"

if [ ! -f "$DEMO/sessions/manifest.json" ]; then
  echo "error: no recorded sessions (demo/sessions/manifest.json missing)." >&2
  echo "       run scripts/capture-demo-sessions.sh first." >&2
  exit 1
fi

echo "==> type-checking and building the demo bundle (VITE_DEMO=1)"
cd "$ROOT/frontend"
bunx tsc -b
VITE_DEMO=1 bunx vite build --outDir "$DEMO/dist" --emptyOutDir

echo "==> copying recorded sessions into the bundle"
cp -R "$DEMO/sessions" "$DEMO/dist/sessions"

# The live app's public assets ride along via vite's publicDir, but the demo
# never fetches them: samples/ and boards/ feed the upload drop zone (an
# install CTA here) and boards3d/ only backs boards this ladder doesn't ship.
# ~17 MB of dead weight in the deploy otherwise.
rm -rf "$DEMO/dist/samples" "$DEMO/dist/boards" "$DEMO/dist/boards3d"

echo "==> demo/dist ready:"
du -sh "$DEMO/dist" "$DEMO/dist/sessions"
