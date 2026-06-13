# Reduced Watchy display-init firmware for the hauksbee boot-coverage execution.
# Build: idf.py set-target esp32 && idf.py build  (then merge: see build.sh).
cmake_minimum_required(VERSION 3.16)
include($ENV{IDF_PATH}/tools/cmake/project.cmake)
project(watchy_display_init)
