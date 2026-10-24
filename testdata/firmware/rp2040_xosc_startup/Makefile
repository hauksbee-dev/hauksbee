# RP2040 crystal start-up fixtures. Needs arm-none-eabi-gcc; the ELFs are
# committed so a machine without it still runs the tests.
#   xosc_wait_1ms.elf   STARTUP.DELAY = 47   (~1 ms at 12 MHz, the pico-sdk value)
#   xosc_wait_64ms.elf  STARTUP.DELAY = 3000 (64 ms at 12 MHz)
CC = arm-none-eabi-gcc
CFLAGS = -mthumb -mcpu=cortex-m0plus -O1 -ffreestanding -nostdlib -Wall -Wextra -fno-builtin
LDFLAGS = -nostdlib -T rp2040.ld
TARGETS = xosc_wait_1ms.elf xosc_wait_64ms.elf

# boot2.bin and rp2040.ld: copied from ../rp2040_drive.
all: $(TARGETS)
xosc_wait_1ms.elf: xosc.c rp2040.ld boot2.bin
	$(CC) $(CFLAGS) -DDELAY=47 xosc.c $(LDFLAGS) -o $@
xosc_wait_64ms.elf: xosc.c rp2040.ld boot2.bin
	$(CC) $(CFLAGS) -DDELAY=3000 xosc.c $(LDFLAGS) -o $@
clean:
	rm -f $(TARGETS)
.PHONY: all clean
