# The negative half of the RP2040 two-sided co-sim proof. Same pico-sdk runtime
# and same UART chatter as ../rp2040_blink_uart, but main() never touches a GPIO,
# so a toggle assertion on GP25 must FAIL against this image. Build exactly as
# ../rp2040_blink_uart/CMakeLists.txt documents.
cmake_minimum_required(VERSION 3.13)
set(PICO_BOARD pico)
set(PICO_PLATFORM rp2040)
include(${PICO_SDK_PATH}/pico_sdk_init.cmake)
project(rp2040_quiet C CXX ASM)
set(CMAKE_C_STANDARD 11)
pico_sdk_init()

add_executable(quiet main.c)
target_link_libraries(quiet pico_stdlib)
pico_enable_stdio_uart(quiet 1)
pico_enable_stdio_usb(quiet 0)
