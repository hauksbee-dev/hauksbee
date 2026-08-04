# Build the clock-truth gate firmware, one ELF per Cortex-M part.
#
# Requires arm-none-eabi-gcc (brew install arm-none-eabi-gcc). The ELFs are
# committed so a machine without the toolchain still runs the gate; rebuild
# them here whenever tick.c changes and commit the result.
#
# What each image measures, and why the numbers in tick.c are silicon facts
# rather than emulator-tuned constants, is in tick.c's header comment.

CC = arm-none-eabi-gcc

CFLAGS = -mthumb -O2 -ffreestanding -nostdlib -Wall -Wextra -fno-builtin \
         -ffunction-sections -fdata-sections
LDFLAGS = -nostdlib -Wl,--gc-sections

SRCS = startup.c tick.c
TARGETS = stm32f103_tick.elf stm32f4_tick.elf nrf52840_tick.elf

all: $(TARGETS)

stm32f103_tick.elf: $(SRCS) stm32f103.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m3 -DPART_STM32F103 $(LDFLAGS) -T stm32f103.ld $(SRCS) -o $@

stm32f4_tick.elf: $(SRCS) stm32f4.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m4 -DPART_STM32F4 $(LDFLAGS) -T stm32f4.ld $(SRCS) -o $@

nrf52840_tick.elf: $(SRCS) nrf52840.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m4 -DPART_NRF52840 $(LDFLAGS) -T nrf52840.ld $(SRCS) -o $@

clean:
	rm -f $(TARGETS)

.PHONY: all clean
