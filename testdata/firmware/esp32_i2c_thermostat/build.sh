#!/usr/bin/env bash
# Build the hauksbee ESP32 I2C thermostat firmware and produce the merged flash
# image (flash.bin) the QemuBackend boots.
#
# Requires esp-idf v5.x sourced in the shell environment:
#   . ~/esp/esp-idf/export.sh
#
# Then, from this directory:
#   ./build.sh
set -euo pipefail
cd "$(dirname "$0")"

: "${IDF_PATH:?source esp-idf export.sh first (. ~/esp/esp-idf/export.sh)}"

idf.py set-target esp32
idf.py build

cd build
# Merge 2nd-stage bootloader + partition table + app into a single 4 MB raw
# flash image (the 1st-stage ROM bootloader is baked into the QEMU binary).
esptool.py --chip esp32 merge_bin --fill-flash-size 4MB -o flash.bin @flash_args
cp flash.bin ../flash.bin
cp esp32_i2c_thermostat.elf ../esp32_i2c_thermostat.elf
cd ..

echo "Wrote flash.bin ($(du -h flash.bin | cut -f1)) and esp32_i2c_thermostat.elf"
