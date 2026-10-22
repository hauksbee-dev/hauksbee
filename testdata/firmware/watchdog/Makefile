# Watchdog fixtures. Needs arm-none-eabi-gcc; the ELFs are committed so a
# machine without it still runs the tests. Startup code and linker scripts are
# the clock-truth gate's.
CC = arm-none-eabi-gcc
CT = ../clock_truth
CFLAGS = -mthumb -O1 -ffreestanding -nostdlib -Wall -Wextra -fno-builtin \
         -ffunction-sections -fdata-sections
LDFLAGS = -nostdlib -Wl,--gc-sections
TARGETS = nrf52840_wdt.elf nrf52840_wdt_fed.elf stm32f103_wdt.elf stm32f103_wdt_fed.elf

all: $(TARGETS)

nrf52840_wdt.elf: wdt.c $(CT)/startup.c $(CT)/nrf52840.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m4 -DPART_NRF52840 $(LDFLAGS) -T $(CT)/nrf52840.ld wdt.c $(CT)/startup.c -o $@
nrf52840_wdt_fed.elf: wdt.c $(CT)/startup.c $(CT)/nrf52840.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m4 -DPART_NRF52840 -DFEED $(LDFLAGS) -T $(CT)/nrf52840.ld wdt.c $(CT)/startup.c -o $@
stm32f103_wdt.elf: wdt.c $(CT)/startup.c $(CT)/stm32f103.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m3 -DPART_STM32F103 $(LDFLAGS) -T $(CT)/stm32f103.ld wdt.c $(CT)/startup.c -o $@
stm32f103_wdt_fed.elf: wdt.c $(CT)/startup.c $(CT)/stm32f103.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m3 -DPART_STM32F103 -DFEED $(LDFLAGS) -T $(CT)/stm32f103.ld wdt.c $(CT)/startup.c -o $@

clean:
	rm -f $(TARGETS)
.PHONY: all clean
