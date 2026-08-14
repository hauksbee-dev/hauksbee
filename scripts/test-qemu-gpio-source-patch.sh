#!/usr/bin/env bash
# Exact-source F11 acceptance: build/verify the reviewed QEMU patch, then prove
# ordinary ESP-IDF GPIO levels and direction without a Hauksbee RAM mailbox.
set -euo pipefail

if [ "$#" -ne 0 ]; then
  if [ "$#" -eq 1 ] && { [ "$1" = "--help" ] || [ "$1" = "-h" ]; }; then
    echo "Usage: scripts/test-qemu-gpio-source-patch.sh"
    echo "Build/verify the exact reviewed QEMU source patch and run its live F11 gate."
    exit 0
  fi
  echo "unexpected argument: $1 (try --help)" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"$ROOT/scripts/install-sims.sh" --qemu-patched-source

xtensa="$HOME/.hauksbee-qemu-esp-patched/qemu/bin/qemu-system-xtensa"
riscv32="$HOME/.hauksbee-qemu-esp-patched/qemu/bin/qemu-system-riscv32"
[ -x "$xtensa" ] && [ -x "$riscv32" ] || {
  echo "patched QEMU installer did not leave both expected binaries" >&2
  exit 1
}

cd "$ROOT"
HAUKSBEE_QEMU_XTENSA="$xtensa" \
HAUKSBEE_QEMU_RISCV32="$riscv32" \
HAUKSBEE_REQUIRE_PATCHED_QEMU=1 \
  cargo test --locked -p hauksbee-mcu --no-default-features --features qemu \
    --test qemu_gpio_register_state -- --nocapture --test-threads=1
