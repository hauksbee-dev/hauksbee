# Build the hauksbee STM32F072 walkthrough firmware.
#
# Requires arm-none-eabi-gcc (brew install arm-none-eabi-gcc).
# Produces two ELFs from one source, which is what makes the co-sim test
# two-sided: blinky.elf drives pins and prints, quiet.elf drives nothing.

CC      = arm-none-eabi-gcc
OBJCOPY = arm-none-eabi-objcopy

CFLAGS  = -mcpu=cortex-m0 -mthumb -O2 -ffreestanding -nostdlib \
          -Wall -Wextra -fno-builtin -ffunction-sections -fdata-sections
LDFLAGS = -T stm32f072.ld -nostdlib -Wl,--gc-sections

SRCS = startup.c main.c

all: blinky.elf quiet.elf

blinky.elf: $(SRCS) stm32f072.ld
	$(CC) $(CFLAGS) $(LDFLAGS) $(SRCS) -o $@

quiet.elf: $(SRCS) stm32f072.ld
	$(CC) $(CFLAGS) -DQUIET $(LDFLAGS) $(SRCS) -o $@

clean:
	rm -f blinky.elf quiet.elf

.PHONY: all clean
