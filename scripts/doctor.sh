#!/usr/bin/env bash
# doctor.sh - check the environment and report what each tool unlocks.
#
# hauksbee's core (extract, bind, solve, the static checks, board-as-code, the
# AVR co-sim via the built-in simavr) needs nothing but the binaries. The extra
# tools below unlock specific backends and exports; this reports what is present
# and what each missing one would add, so you know exactly what to install for
# the flow you want.
#
# Usage:
#   scripts/doctor.sh [--quiet] [--help]
#
# Options:
#   --quiet   Print only the summary line and missing-tool hints.
#   --help    Show this help.
#
# Exit code is always 0: a missing optional tool is information, not a failure.
# (Set HAUKSBEE_DOCTOR_STRICT=1 to exit non-zero if a REQUIRED tool is missing.)
set -euo pipefail
# shellcheck source=scripts/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//; $d'; }

QUIET=0
while [ $# -gt 0 ]; do
  case "$1" in
    --quiet) QUIET=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown argument '$1' (try --help)" ;;
  esac
done

present=0
missing=0
required_missing=0

# report ROLE NAME UNLOCKS [ENV_HINT]  - ROLE is "required" or "optional".
report() {
  local role="$1" name="$2" unlocks="$3" envhint="${4:-}"
  local path
  if path="$(command -v "$name" 2>/dev/null)"; then
    present=$((present + 1))
    [ "$QUIET" -eq 1 ] || ok "${C_BOLD}${name}${C_RESET}  ${C_DIM}${path}${C_RESET}"
    [ "$QUIET" -eq 1 ] || info "     unlocks: ${unlocks}"
  else
    missing=$((missing + 1))
    local tag="${C_YELLOW}absent${C_RESET}"
    if [ "$role" = required ]; then
      required_missing=$((required_missing + 1))
      tag="${C_RED}MISSING (required)${C_RESET}"
    fi
    printf '%s\n' "  ${tag} ${C_BOLD}${name}${C_RESET}"
    info "     would unlock: ${unlocks}"
    if [ -n "$envhint" ]; then info "     ${C_DIM}${envhint}${C_RESET}"; fi
  fi
}

log "hauksbee environment doctor"
printf '\n'

[ "$QUIET" -eq 1 ] || log "Required (the toolchain that builds hauksbee)"
report required cargo "build + install the hauksbee / hauksbee-ci binaries (https://rustup.rs)"
report required rustc "the Rust compiler cargo drives"

# The installed binaries themselves (present once you have run install.sh).
for bin in hauksbee hauksbee-ci; do
  if have "$bin"; then
    present=$((present + 1))
    [ "$QUIET" -eq 1 ] || ok "${C_BOLD}${bin}${C_RESET}  ${C_DIM}$(command -v "$bin")${C_RESET}  (installed)"
  else
    [ "$QUIET" -eq 1 ] || info "  ${C_DIM}${bin} not yet on PATH (run scripts/install.sh)${C_RESET}"
  fi
done

printf '\n'
[ "$QUIET" -eq 1 ] || log "Optional (each unlocks a backend or export)"

report optional kicad-cli \
  "SVG/Gerber export of re-laid-out boards (board-as-code --relayout visuals); KiCad 11 headless schematic ops"
report optional simavr \
  "(also built in) external AVR backend cross-check for ATmega/ATtiny firmware co-sim"
report optional qemu-system-xtensa \
  "ESP32 / ESP32-S3 (Xtensa) firmware co-sim, e.g. the Watchy boot-coverage examples" \
  "set HAUKSBEE_QEMU_XTENSA=/path/to/qemu-system-xtensa if not on PATH"
report optional qemu-system-riscv32 \
  "ESP32-C3 (RISC-V) firmware co-sim backend" \
  "set HAUKSBEE_QEMU_RISCV32 if not on PATH"
report optional renode \
  "STM32 / nRF52 / SiFive RISC-V (ARM Cortex-M and RISC-V) firmware co-sim backend" \
  "set HAUKSBEE_RENODE if not on PATH"
report optional freerouting \
  "production autorouting of recompiled boards over Specctra DSN (board-as-code routing)"

printf '\n'
if [ "$missing" -eq 0 ]; then
  log "${C_GREEN}All tools present.${C_RESET} ${present} found."
else
  log "${present} present, ${missing} optional/absent. Core hauksbee (static checks, board-as-code, built-in AVR co-sim) works now."
fi

if [ "$required_missing" -gt 0 ] && [ "${HAUKSBEE_DOCTOR_STRICT:-0}" = "1" ]; then
  die "a required tool is missing (HAUKSBEE_DOCTOR_STRICT=1)"
fi
exit 0
