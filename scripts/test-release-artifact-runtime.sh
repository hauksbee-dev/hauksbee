#!/usr/bin/env bash
# Exercise a staged release binary through real firmware, not a Cargo harness.
set -euo pipefail

BIN="${1:-}"
MODE="${2:-default}"
[ -x "$BIN" ] || { echo "usage: $0 PATH_TO_HAUKSBEE [default|permissive]" >&2; exit 2; }
BUNDLE_ROOT="$(cd "$(dirname "$BIN")/.." && pwd)"
ci_out=""
static_out=""
serve_tmp=""
serve_pid=""
cleanup_artifacts() {
  if [ -n "$serve_pid" ]; then
    kill "$serve_pid" 2>/dev/null || true
    wait "$serve_pid" 2>/dev/null || true
  fi
  for path in "$ci_out" "$static_out"; do
    [ -n "$path" ] && find "$path" -maxdepth 0 -type f -delete 2>/dev/null || true
  done
  [ -n "$serve_tmp" ] && find "$serve_tmp" -depth -delete 2>/dev/null || true
}
trap cleanup_artifacts EXIT
case "$MODE" in
  default|permissive) ;;
  *) echo "unknown release shape: $MODE" >&2; exit 2 ;;
esac

run_and_require_activity() {
  local board="$1" firmware="$2" seconds="$3" label="$4"
  local out
  out="$(mktemp "${TMPDIR:-/tmp}/hauksbee-artifact-${label}.XXXXXX")"
  trap 'find "${out:-}" -maxdepth 0 -type f -delete 2>/dev/null || true' RETURN
  "$BIN" run "$board" --firmware "$firmware" --headless --plain \
    --seconds "$seconds" >"$out" 2>&1
  if grep -En 'SKIP:|compiled out|not installed|not found|no firmware|zero net toggles|0 active nets' "$out"; then
    echo "release artifact $label did not prove a live firmware path" >&2
    cat "$out" >&2
    exit 1
  fi
  grep -En 'UART|toggle|active net|firmware' "$out" >/dev/null || {
    echo "release artifact $label emitted no observable firmware evidence" >&2
    cat "$out" >&2
    exit 1
  }
}

ci_out="$(mktemp "${TMPDIR:-/tmp}/hauksbee-artifact-ci.XXXXXX")"
ci_bin="$(dirname "$BIN")/hauksbee-ci"
[ -x "$ci_bin" ] || { echo "staged bundle is missing hauksbee-ci" >&2; exit 1; }

# Prove the archive's embedded web payload, not the checkout's frontend/dist.
# The release job necessarily still has the source tree present, so the
# explicit sentinel bypasses that higher-priority development path.
serve_tmp="$(mktemp -d "${TMPDIR:-/tmp}/hauksbee-artifact-serve.XXXXXX")"
serve_log="$serve_tmp/serve.log"
port=$((31000 + ($$ % 20000)))
(
  cd "$serve_tmp"
  HAUKSBEE_WEB_DIST=:embedded: "$BIN" serve --port "$port" --no-open >"$serve_log" 2>&1
) &
serve_pid=$!
startup=""
for _ in $(seq 1 100); do
  if ! kill -0 "$serve_pid" 2>/dev/null; then
    echo "release artifact embedded web server exited during startup" >&2
    cat "$serve_log" >&2
    exit 1
  fi
  startup="$(curl -fsS "http://127.0.0.1:$port/api/startup" 2>/dev/null || true)"
  [ -n "$startup" ] && break
  sleep 0.1
done
printf '%s' "$startup" | grep -E '"live":true' >/dev/null || {
  echo "release artifact embedded /api/startup did not become ready" >&2
  cat "$serve_log" >&2
  exit 1
}
index="$(curl -fsS "http://127.0.0.1:$port/")"
printf '%s' "$index" | grep -E '<div[^>]+id="root"' >/dev/null || {
  echo "release artifact did not serve its embedded frontend" >&2
  exit 1
}
kill "$serve_pid" 2>/dev/null || true
wait "$serve_pid" 2>/dev/null || true
serve_pid=""

if [ "$MODE" = default ]; then
  # The tracked boot-gate fixture exercises the actual builtin simavr path.
  run_and_require_activity \
    "$BUNDLE_ROOT/examples/ci-specs/boards/boot_gate.kicad_pcb" \
    "$BUNDLE_ROOT/examples/firmware/boot_gate.hex" 0.05 avr
  "$ci_bin" run "$BUNDLE_ROOT/examples/ci-specs/boot_gate_pass.toml" >"$ci_out" 2>&1
  grep -E '1/1 assertions passed|GREEN' "$ci_out" >/dev/null || {
    echo "release artifact hauksbee-ci did not pass the tracked boot-gate spec" >&2
    cat "$ci_out" >&2
    exit 1
  }
  echo "release artifact runtime smoke: AVR, CLI/CI, and embedded web front doors passed"
else
  # The permissive (BETA-LICENSE.txt) shape intentionally has no AVR backend. Exercise both
  # packaged front doors with a real solver/thermal board instead of letting
  # version and doctor probes stand in for runtime behavior.
  static_out="$(mktemp "${TMPDIR:-/tmp}/hauksbee-artifact-static.XXXXXX")"
  "$BIN" run "$BUNDLE_ROOT/examples/ci-specs/boards/power_resistor.kicad_pcb" \
    --headless --plain >"$static_out" 2>&1
  grep -E 'simulated|numerical qualification|thermal' "$static_out" >/dev/null || {
    echo "permissive release artifact did not exercise the static solver front door" >&2
    cat "$static_out" >&2
    exit 1
  }
  "$ci_bin" run "$BUNDLE_ROOT/examples/ci-specs/power_resistor_cool.toml" >"$ci_out" 2>&1
  grep -E '2/2 assertions passed|GREEN' "$ci_out" >/dev/null || {
    echo "permissive release artifact hauksbee-ci did not pass the tracked static spec" >&2
    cat "$ci_out" >&2
    exit 1
  }
  echo "release artifact runtime smoke: static solver, CLI/CI, and embedded web front doors passed"
fi
