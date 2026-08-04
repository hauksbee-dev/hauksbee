# pico-sdk blink + UART demo for the RP2040 co-sim smoke.
#
#   cmake -B build -G Ninja -DPICO_SDK_PATH=<path to pico-sdk 2.1.1> .
#   ninja -C build
#   arm-none-eabi-strip --strip-debug build/blink_uart.elf
#   cp build/blink_uart.elf .
#
# The checked-in blink_uart.elf was built exactly that way with pico-sdk 2.1.1
# and the Arm GNU toolchain 14.2.rel1 (arm-none-eabi). It is committed because
# the test must run on a machine with no pico-sdk and no Arm toolchain; only
# --strip-debug is applied, so the symbol table Renode reads is intact.
#
# Note that Homebrew's arm-none-eabi-gcc will NOT build this: that formula ships
# no newlib, so the link fails on `cannot read spec file 'nosys.specs'`. Use
# Arm's own toolchain release, or any arm-none-eabi that bundles newlib.
cmake_minimum_required(VERSION 3.13)
set(PICO_BOARD pico)
set(PICO_PLATFORM rp2040)
include(${PICO_SDK_PATH}/pico_sdk_init.cmake)
project(rp2040_blink_uart C CXX ASM)
set(CMAKE_C_STANDARD 11)
pico_sdk_init()

add_executable(blink_uart main.c)
target_link_libraries(blink_uart pico_stdlib)
pico_enable_stdio_uart(blink_uart 1)
pico_enable_stdio_usb(blink_uart 0)
