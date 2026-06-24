# Build the hauksbee STM32F103 I2C thermostat firmware.
#
# Requires arm-none-eabi-gcc (brew install arm-none-eabi-gcc on macOS).
# Produces thermostat.elf, which Renode loads with `sysbus LoadELF`.
#
# The firmware reads an LM75 at I2C address 0x48 (I2C1, PB6/PB7) and drives
# PA8 HIGH when temperature >= 30 C. The mailbox symbol `hauksbee_gpio_out`
# in .bss lets the Renode backend read the pin state.

CC      = arm-none-eabi-gcc
CFLAGS  = -mcpu=cortex-m3 -mthumb -O2 -ffreestanding -nostdlib \
          -Wall -Wextra -fno-builtin -ffunction-sections -fdata-sections
LDFLAGS = -T stm32f103.ld -nostdlib -Wl,--gc-sections

SRCS    = startup.c main.c
TARGET  = thermostat

all: $(TARGET).elf

$(TARGET).elf: $(SRCS) stm32f103.ld
	$(CC) $(CFLAGS) $(LDFLAGS) $(SRCS) -o $@

clean:
	rm -f $(TARGET).elf

.PHONY: all clean
