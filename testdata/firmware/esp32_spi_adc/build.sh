#!/usr/bin/env bash
# Build the hauksbee ESP32 SPI ADC co-sim firmware and produce the merged flash
# image (flash.bin) the QemuBackend boots.
#
# Requires esp-idf v5.x sourced:
#   . ~/esp/esp-idf/export.sh
# Then from this directory:
#   ./build.sh
set -euo pipefail
cd "$(dirname "$0")"

: "${IDF_PATH:?source esp-idf export.sh first (. ~/esp/esp-idf/export.sh)}"

idf.py set-target esp32
idf.py build

cd build
# Merge 2nd-stage bootloader + partition table + app into a 4 MB raw flash image.
esptool.py --chip esp32 merge_bin --fill-flash-size 4MB -o flash.bin @flash_args
cp flash.bin ../flash.bin
cp esp32_spi_adc.elf ../esp32_spi_adc.elf
cd ..
echo "Wrote flash.bin ($(du -h flash.bin | cut -f1)) and esp32_spi_adc.elf"
echo
echo "Smoke-test boot under Espressif QEMU:"
echo "  ~/.hauksbee-qemu-esp/qemu/bin/qemu-system-xtensa -nographic -machine esp32 \\"
echo "     -drive file=flash.bin,if=mtd,format=raw \\"
echo "     -global driver=timer.esp32.timg,property=wdt_disable,value=true"
