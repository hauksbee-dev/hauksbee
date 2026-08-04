# pico-sdk ADC fixture for the RP2040 injection proof. Reads ADC channel 0
# (GP26) and prints the raw 12-bit count, so an engine-injected voltage can be
# checked against the code the firmware actually sees. Build exactly as
# ../rp2040_blink_uart/CMakeLists.txt documents.
cmake_minimum_required(VERSION 3.13)
set(PICO_BOARD pico)
set(PICO_PLATFORM rp2040)
include(${PICO_SDK_PATH}/pico_sdk_init.cmake)
project(rp2040_adc_probe C CXX ASM)
set(CMAKE_C_STANDARD 11)
pico_sdk_init()

add_executable(adc_probe main.c)
target_link_libraries(adc_probe pico_stdlib hardware_adc)
pico_enable_stdio_uart(adc_probe 1)
pico_enable_stdio_usb(adc_probe 0)
