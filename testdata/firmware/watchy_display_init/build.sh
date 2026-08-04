#!/usr/bin/env bash
# Build the reduced Watchy display-init firmware and merge it into flash.bin for
# the hauksbee ESP32 QEMU boot-coverage execution (docs/evidence/KNOWN_FAULTS_VALIDATION.md
# Watchy RES# row). Requires esp-idf v5.x (same install as esp32_blinky/build.sh).
#
#   . ~/esp/esp-idf/export.sh
#   ./build.sh
set -euo pipefail
cd "$(dirname "$0")"

: "${IDF_PATH:?source esp-idf export.sh first (. ~/esp/esp-idf/export.sh)}"

idf.py set-target esp32
idf.py build

cd build
esptool.py --chip esp32 merge_bin --fill-flash-size 4MB -o flash.bin @flash_args
cp flash.bin ../flash.bin
cp watchy_display_init.elf ../watchy_display_init.elf
cd ..
echo "Wrote flash.bin ($(du -h flash.bin | cut -f1)) and watchy_display_init.elf"
