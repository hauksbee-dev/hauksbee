# RP2040 drive-strength fixtures. Needs arm-none-eabi-gcc; the ELFs are
# committed so a machine without it still runs the tests.
CC = arm-none-eabi-gcc
CFLAGS = -mthumb -mcpu=cortex-m0plus -O1 -ffreestanding -nostdlib -Wall -Wextra -fno-builtin
LDFLAGS = -nostdlib -T rp2040.ld
TARGETS = drive_2ma.elf drive_12ma.elf

# boot2.bin: arm-none-eabi-objcopy -O binary -j .boot2 ../rp2040_blink_uart/blink_uart.elf boot2.bin
all: $(TARGETS)
drive_2ma.elf: drive.c rp2040.ld boot2.bin
	$(CC) $(CFLAGS) -DDRIVE=0 drive.c $(LDFLAGS) -o $@
drive_12ma.elf: drive.c rp2040.ld boot2.bin
	$(CC) $(CFLAGS) -DDRIVE=3 drive.c $(LDFLAGS) -o $@
clean:
	rm -f $(TARGETS)
.PHONY: all clean
