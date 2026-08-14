#!/usr/bin/env bash
# Build the no-mailbox GPIO fixture used by qemu_gpio_register_state.rs.
set -euo pipefail
cd "$(dirname "$0")"
: "${IDF_PATH:?source esp-idf export.sh first (. ~/esp/esp-idf/export.sh)}"
IDF_COMMIT="67c1de1eebe095d554d281952fde63c16ee2dca0" # ESP-IDF v5.4
actual_idf_commit="$(git -C "$IDF_PATH" rev-parse HEAD)"
[ "$actual_idf_commit" = "$IDF_COMMIT" ] || {
  echo "ESP-IDF source mismatch: expected $IDF_COMMIT, got $actual_idf_commit" >&2
  exit 1
}

idf.py set-target esp32
idf.py build
(
  cd build
  esptool.py --chip esp32 merge_bin --fill-flash-size 4MB \
    -o ../flash.bin @flash_args
  cp esp32_native_gpio.elf ../esp32_native_gpio.elf
)
echo "Wrote flash.bin and esp32_native_gpio.elf"
