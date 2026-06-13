#!/usr/bin/env bash
# Build the hauksbee ESP32 QEMU co-sim demo firmware and produce the merged flash
# image (flash.bin) the QemuBackend boots.
#
# Requires esp-idf v5.x. One-time install (~3-5 GB, native macOS arm64 / Linux):
#   git clone --depth=1 --branch v5.4 https://github.com/espressif/esp-idf.git ~/esp/esp-idf
#   cd ~/esp/esp-idf && git submodule update --init --depth=1
#   ./install.sh esp32,esp32c3
#
# The Espressif QEMU binary itself is separate and does NOT need esp-idf; grab
# the prebuilt fork release (native macOS arm64 / Linux) and unpack it:
#   https://github.com/espressif/qemu/releases   ->  ~/.hauksbee-qemu-esp/qemu/
# (or `python $IDF_PATH/tools/idf_tools.py install qemu-xtensa qemu-riscv32`).
#
# Then, from this directory:
#   . ~/esp/esp-idf/export.sh
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
cp esp32_blinky.elf ../esp32_blinky.elf
cd ..
echo "Wrote flash.bin ($(du -h flash.bin | cut -f1)) and esp32_blinky.elf"
echo
echo "Smoke-test boot under Espressif QEMU:"
echo "  ~/.hauksbee-qemu-esp/qemu/bin/qemu-system-xtensa -nographic -machine esp32 \\"
echo "     -drive file=flash.bin,if=mtd,format=raw \\"
echo "     -global driver=timer.esp32.timg,property=wdt_disable,value=true"
