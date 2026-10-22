# Build the hauksbee STM32F103 ADC rate probe firmware.
#
# Requires arm-none-eabi-gcc (brew install arm-none-eabi-gcc).
# Produces adc_rate.elf, which Renode loads with `sysbus LoadELF`.

CC      = arm-none-eabi-gcc
OBJCOPY = arm-none-eabi-objcopy

CFLAGS  = -mcpu=cortex-m3 -mthumb -O2 -ffreestanding -nostdlib \
          -Wall -Wextra -fno-builtin -ffunction-sections -fdata-sections
LDFLAGS = -T stm32f103.ld -nostdlib -Wl,--gc-sections

SRCS = startup.c main.c
TARGET = adc_rate

all: $(TARGET).elf $(TARGET).bin

$(TARGET).elf: $(SRCS) stm32f103.ld
	$(CC) $(CFLAGS) $(LDFLAGS) $(SRCS) -o $@

$(TARGET).bin: $(TARGET).elf
	$(OBJCOPY) -O binary $< $@

clean:
	rm -f $(TARGET).elf $(TARGET).bin

.PHONY: all clean
