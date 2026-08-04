# Build the AVR watchdog firmware pair.
#
# Requires avr-gcc (brew install avr-gcc). The ELFs are committed so a machine
# without the toolchain still runs the gate; rebuild them here whenever either
# source changes and commit the result.
#
# The pair is the point: `wdt.elf` starves its watchdog, `nowdt.elf` is the same
# firmware with the one arming line removed. See wdt.c's header comment.

CC = avr-gcc
MCU = atmega328p
F_CPU = 16000000UL

CFLAGS = -mmcu=$(MCU) -DF_CPU=$(F_CPU) -Os -Wall -Wextra

TARGETS = wdt.elf nowdt.elf

all: $(TARGETS)

wdt.elf: wdt.c
	$(CC) $(CFLAGS) $< -o $@

nowdt.elf: nowdt.c
	$(CC) $(CFLAGS) $< -o $@

clean:
	rm -f $(TARGETS)

.PHONY: all clean
