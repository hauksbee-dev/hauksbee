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

# --- Firmware co-sim backend discovery --------------------------------------
#
# The co-sim backends (Espressif QEMU for ESP32, Renode for STM32/nRF52/RISC-V)
# are NOT satisfied by "a binary of that name on PATH": the engine requires the
# Espressif *fork* of qemu-system-xtensa (Homebrew's mainline has no `esp32`
# machine and is rejected), and it finds Renode under `~/renode-portable` even
# when `renode` is not on PATH. A `command -v` check would give both a false OK
# (mainline QEMU) and a false ABSENT (portable Renode).
#
# To make this script agree with the engine BY CONSTRUCTION, we ask the engine
# itself: `hauksbee doctor --backends` runs the SAME discovery the co-sim uses
# (crates/hauksbee-mcu/src/{qemu,renode}/process.rs) and prints, per backend,
# `NAME<TAB>STATUS<TAB>DETAIL`. We call that and mirror its verdict.
#
# When the binary is not built yet we fall back to a pure-shell mirror of the
# engine's search order (below), clearly labelled "unverified" so the user
# knows to build hauksbee for the authoritative answer.

TAB="$(printf '\t')"

# Resolve the authoritative hauksbee binary: an explicit override, else a fresh
# workspace build, else whatever is on PATH (may be a stale install).
resolve_hauksbee_bin() {
  if [ -n "${HAUKSBEE_BIN:-}" ] && [ -x "${HAUKSBEE_BIN}" ]; then
    printf '%s\n' "$HAUKSBEE_BIN"; return 0
  fi
  local built onpath
  built="$(hauksbee_target_bin)/hauksbee"
  onpath="$(command -v hauksbee 2>/dev/null || true)"
  if [ -x "$built" ]; then
    if [ -n "$onpath" ] && ! cmp -s "$built" "$onpath"; then
      warn "backend probe uses the checkout build $built; the installed $onpath differs (re-run scripts/install.sh to refresh it)."
    fi
    printf '%s\n' "$built"; return 0
  fi
  printf '%s\n' "$onpath"
}

# Mirror of the engine's Espressif-fork check (qemu/process.rs `is_esp_fork`):
# a candidate qemu-system-* is the fork ONLY if `-machine help` lists esp32.
_qemu_is_esp_fork() { "$1" -machine help </dev/null 2>/dev/null | grep -qi esp32; }

# Mirror of `find_qemu` (qemu/process.rs) discovery order for arch $1
# (xtensa|riscv32). Echoes the resolved fork binary path; empty + non-zero if
# none is found. Kept byte-for-byte in step with the engine's candidate list.
_find_qemu_fork() {
  local arch="$1" name envov val home c d onpath
  case "$arch" in
    xtensa)  name=qemu-system-xtensa;  envov=HAUKSBEE_QEMU_XTENSA ;;
    riscv32) name=qemu-system-riscv32; envov=HAUKSBEE_QEMU_RISCV32 ;;
    *) return 1 ;;
  esac
  # 1. explicit per-arch override (full path to the binary, taken as-is).
  eval "val=\${$envov:-}"
  if [ -n "$val" ]; then
    [ -e "$val" ] && { printf '%s\n' "$val"; return 0; }
    return 1
  fi
  home="${HOME:-}"
  local candidates=()
  # 2. generic dir override pointing at the fork's bin/.
  [ -n "${HAUKSBEE_QEMU_DIR:-}" ] && candidates+=("${HAUKSBEE_QEMU_DIR}/$name")
  if [ -n "$home" ]; then
    # 3. conventional unpacked location (current + legacy pre-rename name).
    candidates+=("$home/.hauksbee-qemu-esp/qemu/bin/$name")
    candidates+=("$home/.galvani-qemu-esp/qemu/bin/$name")
    # 4. esp-idf idf_tools install: ~/.espressif/tools/qemu-*/<ver>/qemu/bin/<name>.
    for d in "$home"/.espressif/tools/qemu-*/*/qemu/bin/"$name"; do
      [ -f "$d" ] && candidates+=("$d")
    done
  fi
  for c in "${candidates[@]:-}"; do
    [ -n "$c" ] || continue
    if [ -f "$c" ] && _qemu_is_esp_fork "$c"; then printf '%s\n' "$c"; return 0; fi
  done
  # 5. PATH, but only if it is the Espressif fork.
  onpath="$(command -v "$name" 2>/dev/null || true)"
  if [ -n "$onpath" ] && _qemu_is_esp_fork "$onpath"; then printf '%s\n' "$onpath"; return 0; fi
  return 1
}

# Mirror of `find_renode` (renode/process.rs) discovery order. Echoes the
# resolved Renode binary path; empty + non-zero if none is found.
_find_renode() {
  local home="${HOME:-}" onpath c
  if [ -n "${HAUKSBEE_RENODE:-}" ]; then
    [ -e "$HAUKSBEE_RENODE" ] && { printf '%s\n' "$HAUKSBEE_RENODE"; return 0; }
    return 1
  fi
  onpath="$(command -v renode 2>/dev/null || true)"
  [ -n "$onpath" ] && { printf '%s\n' "$onpath"; return 0; }
  if [ -n "$home" ]; then
    for c in "$home/renode-portable/Renode.app/Contents/MacOS/renode" \
             "$home/renode-portable/renode" \
             "$home/renode_portable/renode"; do
      [ -e "$c" ] && { printf '%s\n' "$c"; return 0; }
    done
  fi
  return 1
}

# Build the same NAME<TAB>STATUS<TAB>DETAIL table the binary emits, from the
# pure-shell mirror, for the fallback path.
shell_mirror_probe() {
  local p
  if p="$(_find_qemu_fork xtensa)"; then
    printf 'qemu-xtensa%sok%s%s\n' "$TAB" "$TAB" "$p"
  else
    printf 'qemu-xtensa%sabsent%sEspressif qemu-system-xtensa fork not found (set HAUKSBEE_QEMU_XTENSA, or unpack the fork to ~/.hauksbee-qemu-esp/qemu)\n' "$TAB" "$TAB"
  fi
  if p="$(_find_qemu_fork riscv32)"; then
    printf 'qemu-riscv32%sok%s%s\n' "$TAB" "$TAB" "$p"
  else
    printf 'qemu-riscv32%sabsent%sEspressif qemu-system-riscv32 fork not found (set HAUKSBEE_QEMU_RISCV32, or unpack the fork to ~/.hauksbee-qemu-esp/qemu)\n' "$TAB" "$TAB"
  fi
  if p="$(_find_renode)"; then
    printf 'renode%sok%s%s\n' "$TAB" "$TAB" "$p"
  else
    printf 'renode%sabsent%sRenode not found (set HAUKSBEE_RENODE, put renode on PATH, or extract the portable build to ~/renode-portable)\n' "$TAB" "$TAB"
  fi
}

# Look up one backend's status / detail from the probe table in $BACKEND_PROBE.
backend_status() { printf '%s\n' "$BACKEND_PROBE" | awk -F "$TAB" -v n="$1" '$1==n {print $2; exit}'; }
backend_detail() { printf '%s\n' "$BACKEND_PROBE" | awk -F "$TAB" -v n="$1" '$1==n {print $3; exit}'; }

# report_backend NAME UNLOCKS  - render one co-sim backend from the probe table,
# using the authoritative-or-mirrored status and the resolved path.
report_backend() {
  local name="$1" unlocks="$2" status detail
  status="$(backend_status "$name")"
  detail="$(backend_detail "$name")"
  if [ "$status" = ok ] || [ "$status" = builtin ]; then
    present=$((present + 1))
    [ "$QUIET" -eq 1 ] || ok "${C_BOLD}${name}${C_RESET}  ${C_DIM}${detail}${C_RESET}"
    [ "$QUIET" -eq 1 ] || info "     unlocks: ${unlocks}"
  else
    missing=$((missing + 1))
    printf '%s\n' "  ${C_YELLOW}absent${C_RESET} ${C_BOLD}${name}${C_RESET}"
    info "     would unlock: ${unlocks}"
    if [ -n "$detail" ]; then info "     ${C_DIM}${detail}${C_RESET}"; fi
  fi
}

# Resolve the probe up front: authoritative from the engine if we can, else the
# labelled shell mirror.
HAUKSBEE_BIN_RESOLVED="$(resolve_hauksbee_bin)"
BACKEND_PROBE=""
BACKEND_AUTHORITATIVE=0
if [ -n "$HAUKSBEE_BIN_RESOLVED" ] \
   && BACKEND_PROBE="$("$HAUKSBEE_BIN_RESOLVED" doctor --backends 2>/dev/null)" \
   && [ -n "$BACKEND_PROBE" ]; then
  BACKEND_AUTHORITATIVE=1
else
  BACKEND_PROBE="$(shell_mirror_probe)"
  BACKEND_AUTHORITATIVE=0
fi

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
report optional freerouting \
  "production autorouting of recompiled boards over Specctra DSN (board-as-code routing)"

printf '\n'
# Firmware co-sim backends. These are NOT plain PATH checks (see the discovery
# block above): the verdict comes from the engine's own resolver so it can never
# disagree with a real co-sim. A Homebrew mainline qemu-system-xtensa on PATH is
# correctly reported absent (no esp32 machine); a portable Renode under
# ~/renode-portable is correctly reported present.
if [ "$BACKEND_AUTHORITATIVE" -eq 1 ]; then
  [ "$QUIET" -eq 1 ] || log "Firmware co-sim backends ${C_DIM}(authoritative: asked ${HAUKSBEE_BIN_RESOLVED})${C_RESET}"
else
  [ "$QUIET" -eq 1 ] || log "Firmware co-sim backends ${C_YELLOW}(unverified: build hauksbee for authoritative checks)${C_RESET}"
fi
report_backend qemu-xtensa \
  "ESP32 / ESP32-S3 (Xtensa) firmware co-sim, e.g. the Watchy boot-coverage examples"
report_backend qemu-riscv32 \
  "ESP32-C3 (RISC-V) firmware co-sim backend"
report_backend renode \
  "STM32 / nRF52 / SiFive RISC-V (ARM Cortex-M and RISC-V) firmware co-sim backend"

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
