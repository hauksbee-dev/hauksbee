#!/usr/bin/env bash
# Shared helpers for the hauksbee distribution scripts.
#
# Sourced by install.sh, doctor.sh, ci.sh and bundle.sh. Keeps the colour /
# logging / path logic in one place so every script behaves the same way and
# stays CI-safe (colours auto-disable when stdout is not a TTY or NO_COLOR is
# set). This file is not meant to be executed directly.

# Resolve the hauksbee repo root from this file's location (scripts/ lives at
# the workspace root), so the scripts work no matter the caller's cwd.
_common_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HAUKSBEE_ROOT="$(cd "${_common_dir}/.." && pwd)"
export HAUKSBEE_ROOT

# Colours, but only when attached to a terminal and NO_COLOR is unset. CI logs
# stay clean. (Some colours are used only by scripts that source this file.)
# shellcheck disable=SC2034
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
  C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_RED=$'\033[31m'
  C_GREEN=$'\033[32m'; C_YELLOW=$'\033[33m'; C_BLUE=$'\033[34m'; C_DIM=$'\033[2m'
else
  C_RESET=''; C_BOLD=''; C_RED=''; C_GREEN=''; C_YELLOW=''; C_BLUE=''; C_DIM=''
fi

log()  { printf '%s\n' "${C_BLUE}==>${C_RESET} ${C_BOLD}$*${C_RESET}"; }
info() { printf '%s\n' "    $*"; }
ok()   { printf '%s\n' "${C_GREEN}  ok${C_RESET} $*"; }
warn() { printf '%s\n' "${C_YELLOW}warn${C_RESET} $*" >&2; }
err()  { printf '%s\n' "${C_RED} err${C_RESET} $*" >&2; }
die()  { err "$*"; exit 1; }

# True if a command is on PATH.
have() { command -v "$1" >/dev/null 2>&1; }

# The release binary directory inside the workspace.
hauksbee_target_bin() { printf '%s/target/release\n' "$HAUKSBEE_ROOT"; }

# Default install prefix: honour PREFIX, else ~/.local (no sudo), which is on
# PATH for most modern shells; fall back is documented in install.sh --help.
hauksbee_default_prefix() { printf '%s\n' "${PREFIX:-$HOME/.local}"; }
