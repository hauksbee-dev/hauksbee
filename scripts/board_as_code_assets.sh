#!/usr/bin/env bash
#
# Regenerate every Board-as-Code documentation asset, end to end, from the
# corpus boards. Re-runnable: it builds the CLI, decompiles, re-lays-out,
# routes with freerouting, runs an incremental edit, and exports SVG/PNG into
# galvani/docs/assets/.
#
# Requirements:
#   - kicad-cli (KiCad 8/9): for SVG export. Override with KICAD_CLI=...
#   - rsvg-convert (librsvg) or ImageMagick `convert`: SVG -> PNG.
#   - java + a freerouting jar: for the real routing pass. Point FREEROUTING_JAR
#     at the jar, or drop it in <repo>/tools/. Without it the script still emits
#     placement/relayout assets and uses the in-tree grid A* fallback for
#     routing. freerouting 1.9.0 is recommended (its headless batch reliably
#     writes the SES; 2.x stalls unless the board is 100% routed).
#
# Usage:
#   galvani/scripts/board_as_code_assets.sh
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GALVANI_DIR="$(cd "$HERE/.." && pwd)"
REPO_ROOT="$(cd "$GALVANI_DIR/.." && pwd)"
ASSETS="$GALVANI_DIR/docs/assets"
CORPUS="${BOARD_CORPUS:-$REPO_ROOT/board-corpus}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

KICAD_CLI="${KICAD_CLI:-/Applications/KiCad/KiCad.app/Contents/MacOS/kicad-cli}"
if ! command -v "$KICAD_CLI" >/dev/null 2>&1; then
  if command -v kicad-cli >/dev/null 2>&1; then KICAD_CLI="kicad-cli"; fi
fi

# Auto-discover a freerouting jar in <repo>/tools if FREEROUTING_JAR is unset.
if [ -z "${FREEROUTING_JAR:-}" ]; then
  CAND="$(ls "$REPO_ROOT"/tools/freerouting-1.*.jar 2>/dev/null | head -1 || true)"
  [ -z "$CAND" ] && CAND="$(ls "$REPO_ROOT"/tools/freerouting-*.jar 2>/dev/null | head -1 || true)"
  [ -n "$CAND" ] && export FREEROUTING_JAR="$CAND"
fi

mkdir -p "$ASSETS"

echo "==> building galvani CLI"
( cd "$GALVANI_DIR" && cargo build -p galvani-engine )
BIN="$GALVANI_DIR/target/debug/galvani"

svg() { # svg <in.kicad_pcb> <out.svg>
  "$KICAD_CLI" pcb export svg --layers "F.Cu,B.Cu,F.SilkS,Edge.Cuts" \
    --output "$2" "$1" >/dev/null
}
png() { # png <in.svg> <out.png> [width]
  local w="${3:-1400}"
  if command -v rsvg-convert >/dev/null 2>&1; then
    rsvg-convert -w "$w" "$1" -o "$2"
  elif command -v convert >/dev/null 2>&1; then
    convert -density 200 "$1" "$2"
  else
    echo "   (no SVG->PNG converter; leaving $1 only)"
  fi
}

BOARD="$CORPUS/stormduino/stormduino Rev2.kicad_pcb"

echo "==> stormduino: decompile to Board-as-Code"
"$BIN" to-code "$BOARD" --out "$WORK/storm.board"

echo "==> stormduino: BEFORE (recompiled, original placement)"
"$BIN" from-code "$WORK/storm.board" --out "$WORK/storm_before.kicad_pcb"
svg "$WORK/storm_before.kicad_pcb" "$ASSETS/storm_before.svg"
png "$ASSETS/storm_before.svg" "$ASSETS/storm_before.png"

echo "==> stormduino: AFTER (full re-layout, function-grouped, on-board)"
"$BIN" from-code "$WORK/storm.board" --relayout --out "$WORK/storm_after.kicad_pcb"
svg "$WORK/storm_after.kicad_pcb" "$ASSETS/storm_after.svg"
png "$ASSETS/storm_after.svg" "$ASSETS/storm_after.png"

echo "==> stormduino: ROUTED (re-layout + freerouting handoff)"
"$BIN" from-code "$WORK/storm.board" --relayout --route \
  --out "$WORK/storm_routed.kicad_pcb"
svg "$WORK/storm_routed.kicad_pcb" "$ASSETS/storm_routed.svg"
png "$ASSETS/storm_routed.svg" "$ASSETS/storm_routed.png" 2400

echo "==> incremental edit visualisation (pic_programmer: unique refs => clean diff)"
PIC="$CORPUS/kicad-demos-src/demos/pic_programmer/pic_programmer.kicad_pcb"
if [ -f "$PIC" ]; then
  "$BIN" to-code "$PIC" --out "$WORK/pic.board"
  INC_BOARD="$WORK/pic.board"
else
  echo "   (pic_programmer not in corpus; falling back to stormduino)"
  INC_BOARD="$WORK/storm.board"
fi
# matplotlib is needed for the highlighted diff; uv provides it on demand.
if command -v uv >/dev/null 2>&1; then
  uv run --with matplotlib python3 "$HERE/make_incremental_viz.py" \
    --bin "$BIN" --board "$INC_BOARD" --kicad-cli "$KICAD_CLI" \
    --assets "$ASSETS" --work "$WORK"
else
  python3 "$HERE/make_incremental_viz.py" \
    --bin "$BIN" --board "$INC_BOARD" --kicad-cli "$KICAD_CLI" \
    --assets "$ASSETS" --work "$WORK"
fi

echo "==> done. Assets in $ASSETS:"
ls -1 "$ASSETS"
