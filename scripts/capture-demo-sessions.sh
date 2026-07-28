#!/usr/bin/env bash
# capture-demo-sessions.sh - record the hauksbee.dev demo's replay sessions.
#
# Builds the release engine, then runs the recorder against it once per
# board/scenario in the demo ladder. Real runs only: the recorder connects a
# websocket client to `hauksbee run <board> --serve` and writes what the wire
# carried to demo/sessions/. See frontend/capture/record-demo-sessions.ts for
# the ladder and the honesty rules.
#
# Usage: scripts/capture-demo-sessions.sh [scenario-id ...]
#   (no args records the full ladder; ids re-record just those scenarios)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "==> building the release engine (recordings must come from a current build)"
cargo build --release --manifest-path "$ROOT/Cargo.toml" --bin hauksbee

echo "==> recording demo sessions"
cd "$ROOT/frontend"
exec bun capture/record-demo-sessions.ts "$@"
