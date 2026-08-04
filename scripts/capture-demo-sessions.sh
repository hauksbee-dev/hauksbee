#!/usr/bin/env bash
# capture-demo-sessions.sh - record everything the browser demos replay.
#
# Two recorders, both against the REAL engine, both writing demo/sessions/:
#
#   1. the sim timeline, per board/scenario (frontend/capture/record-demo-sessions.ts)
#      A websocket client attached to `hauksbee run <board> --serve` writes every
#      server message with the sim time it arrived at. This is what the live
#      surface replays.
#
#   2. the interaction cache, per board (demo/capture/record-embed-cache.ts)
#      A real `hauksbee serve` answers the requests the app's other surfaces
#      make: the report (with and without firmware), one checks run per rule
#      (again both ways), and the curated spec presets including a deliberately
#      red one per board. This is what the embeddable widget answers from, and
#      an interaction with no recording behind it is not offered.
#
# Real runs only. No mock frames, no hand-written verdicts: see the honesty
# rules at the top of each recorder.
#
# Usage:
#   scripts/capture-demo-sessions.sh                 everything
#   scripts/capture-demo-sessions.sh --sessions ...  only the sim timelines
#   scripts/capture-demo-sessions.sh --cache ...     only the interaction cache
#   scripts/capture-demo-sessions.sh <scenario-id>   re-record those scenarios
#                                                    (and the caches of the
#                                                    boards they belong to)
#
# Env:
#   HB_SESSION_BUDGET_BYTES   size budget per recorded session (default 10 MB).
#                             The widget lazy-loads a session per board over
#                             someone else's landing page, so the demo ladder is
#                             recorded at 2.2 MB; over-budget recordings are
#                             cadence-thinned and say so in the manifest.
#   HB_CHECK_TIMEOUT_MS       give up on a single checks run after this long.
#   HAUKSBEE_QEMU_XTENSA      Espressif qemu-system-xtensa, for the Watchy co-sim.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
: "${HB_SESSION_BUDGET_BYTES:=2200000}"
export HB_SESSION_BUDGET_BYTES

WHAT=all
ARGS=()
for arg in "$@"; do
  case "$arg" in
    --sessions) WHAT=sessions ;;
    --cache) WHAT=cache ;;
    *) ARGS+=("$arg") ;;
  esac
done

echo "==> building the release engine (recordings must come from a current build)"
cargo build --release --manifest-path "$ROOT/Cargo.toml" --bins

if [ "$WHAT" = all ] || [ "$WHAT" = sessions ]; then
  echo
  echo "==> recording sim sessions (budget ${HB_SESSION_BUDGET_BYTES} bytes each)"
  (cd "$ROOT/frontend" && bun capture/record-demo-sessions.ts "${ARGS[@]+"${ARGS[@]}"}")
fi

if [ "$WHAT" = all ] || [ "$WHAT" = cache ]; then
  echo
  echo "==> recording the interaction cache"
  # Scenario ids are "<board>-<scenario>"; the cache is per board, so a partial
  # session re-record maps onto the boards those scenarios belong to.
  BOARDS=()
  for id in "${ARGS[@]+"${ARGS[@]}"}"; do BOARDS+=("${id%%-*}"); done
  (cd "$ROOT" && bun demo/capture/record-embed-cache.ts "${BOARDS[@]+"${BOARDS[@]}"}")
fi

echo
echo "==> recorded assets:"
du -sh "$ROOT/demo/sessions"
echo "    (rebuild the widget with demo/embed/build.sh, then verify with"
echo "     bun demo/embed/test/embed-e2e.ts)"
