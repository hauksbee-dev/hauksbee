#!/usr/bin/env bash
# Exercise a staged release binary through real firmware, not a Cargo harness.
set -euo pipefail

BIN="${1:-}"
MODE="${2:-default}"
[ -x "$BIN" ] || { echo "usage: $0 PATH_TO_HAUKSBEE [default|permissive]" >&2; exit 2; }
BUNDLE_ROOT="$(cd "$(dirname "$BIN")/.." && pwd)"
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
trap 'find "${ci_out:-}" -maxdepth 0 -type f -delete 2>/dev/null || true' EXIT
ci_bin="$(dirname "$BIN")/hauksbee-ci"
[ -x "$ci_bin" ] || { echo "staged bundle is missing hauksbee-ci" >&2; exit 1; }

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
  echo "release artifact runtime smoke: tracked AVR firmware reached both packaged front doors"
else
  # The Apache-only shape intentionally has no AVR backend. Exercise both
  # packaged front doors with a real solver/thermal board instead of letting
  # version and doctor probes stand in for runtime behavior.
  static_out="$(mktemp "${TMPDIR:-/tmp}/hauksbee-artifact-static.XXXXXX")"
  trap 'find "${static_out:-}" -maxdepth 0 -type f -delete 2>/dev/null || true' EXIT
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
  echo "release artifact runtime smoke: static solver reached both permissive packaged front doors"
fi
