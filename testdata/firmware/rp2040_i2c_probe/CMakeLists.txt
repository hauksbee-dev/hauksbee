# pico-sdk I2C round-trip fixture for the RP2040 bus-slave proof. Reads two
# bytes from address 0x48 on i2c0 (GP4/GP5) and prints them, so a host-modelled
# slave can be verified in both directions. Build exactly as
# ../rp2040_blink_uart/CMakeLists.txt documents.
cmake_minimum_required(VERSION 3.13)
set(PICO_BOARD pico)
set(PICO_PLATFORM rp2040)
include(${PICO_SDK_PATH}/pico_sdk_init.cmake)
project(rp2040_i2c_probe C CXX ASM)
set(CMAKE_C_STANDARD 11)
pico_sdk_init()

add_executable(i2c_probe main.c)
target_link_libraries(i2c_probe pico_stdlib hardware_i2c)
pico_enable_stdio_uart(i2c_probe 1)
pico_enable_stdio_usb(i2c_probe 0)
