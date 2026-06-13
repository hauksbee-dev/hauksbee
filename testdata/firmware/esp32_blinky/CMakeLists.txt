# Minimal esp-idf project for the hauksbee ESP32 QEMU co-sim demo.
# Build: idf.py set-target esp32 && idf.py build
# Then merge: see build.sh in this directory.
cmake_minimum_required(VERSION 3.16)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(esp32_blinky)
