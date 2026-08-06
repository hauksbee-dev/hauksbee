# Build the STM32F103 external-clock readiness probe.

CC = arm-none-eabi-gcc
CFLAGS = -mcpu=cortex-m3 -mthumb -O2 -ffreestanding -nostdlib \
	-Wall -Wextra -fno-builtin -ffunction-sections -fdata-sections
LDFLAGS = -T ../stm32_blinky/stm32f103.ld -nostdlib -Wl,--gc-sections

TARGET = clock_ready
SRCS = ../stm32_blinky/startup.c main.c

all: $(TARGET).elf

$(TARGET).elf: $(SRCS) ../stm32_blinky/stm32f103.ld
	$(CC) $(CFLAGS) $(LDFLAGS) $(SRCS) -o $@

.PHONY: all
