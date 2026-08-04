# pico-sdk SPI fixture. Clocks two bytes out of spi0 and prints MISO. It is the
# reproduction for a KNOWN GAP, not a passing capability: the RP2040 SPI model
# hauksbee vendors bit-bangs onto GPIO pins instead of dispatching to a
# registered ISPIPeripheral, so a host-side SPI slave is never reached. See
# crates/hauksbee-mcu/tests/renode_rp2040_bus.rs. Build exactly as
# ../rp2040_blink_uart/CMakeLists.txt documents.
cmake_minimum_required(VERSION 3.13)
set(PICO_BOARD pico)
set(PICO_PLATFORM rp2040)
include(${PICO_SDK_PATH}/pico_sdk_init.cmake)
project(rp2040_spi_probe C CXX ASM)
set(CMAKE_C_STANDARD 11)
pico_sdk_init()

add_executable(spi_probe main.c)
target_link_libraries(spi_probe pico_stdlib hardware_spi)
pico_enable_stdio_uart(spi_probe 1)
pico_enable_stdio_usb(spi_probe 0)
