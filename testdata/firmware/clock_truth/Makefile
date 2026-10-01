# Build the clock-truth gate firmware, one ELF per part.
#
# The Cortex-M images require arm-none-eabi-gcc (brew install
# arm-none-eabi-gcc). The RISC-V FE310 image requires no separate cross-gcc:
# it builds with the Rust toolchain's own bare-metal target
# (`rustup target add riscv32imac-unknown-none-elf`), which links with
# rust-lld, so the machine that builds hauksbee can already build it. The ELFs
# are committed so a machine without the toolchains still runs the gate;
# rebuild here whenever a source changes and commit the result.
#
# What each image measures, and why the numbers in the sources are silicon
# facts rather than emulator-tuned constants, is in tick.c's header comment
# (SysTick parts) and fe310_tick.rs's header comment (the mtime oracle).

CC = arm-none-eabi-gcc
RUSTC = rustc

CFLAGS = -mthumb -O2 -ffreestanding -nostdlib -Wall -Wextra -fno-builtin \
         -ffunction-sections -fdata-sections
LDFLAGS = -nostdlib -Wl,--gc-sections

SRCS = startup.c tick.c
TARGETS = stm32f103_tick.elf stm32f4_tick.elf stm32g4_tick.elf stm32l0_tick.elf \
          stm32g4_fault.elf stm32g4_pull_mirror.elf \
          nrf52840_tick.elf fe310_tick.elf

all: $(TARGETS)

stm32f103_tick.elf: $(SRCS) stm32f103.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m3 -DPART_STM32F103 $(LDFLAGS) -T stm32f103.ld $(SRCS) -o $@

stm32f4_tick.elf: $(SRCS) stm32f4.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m4 -DPART_STM32F4 $(LDFLAGS) -T stm32f4.ld $(SRCS) -o $@

stm32g4_tick.elf: $(SRCS) stm32g4.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m4 -DPART_STM32G4 $(LDFLAGS) -T stm32g4.ld $(SRCS) -o $@

# Execution-evidence fixture: the G4 tick image that traps right after main()
# configures its pin, so the fault hook fires and no edge ever appears.
stm32g4_fault.elf: $(SRCS) stm32g4.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m4 -DPART_STM32G4 -DFAULT_AFTER_INIT $(LDFLAGS) -T stm32g4.ld $(SRCS) -o $@

# Internal-pull fixture: PA5 mirrors PB11, an undriven input with its pull-up on.
stm32g4_pull_mirror.elf: startup.c pull_mirror.c stm32g4.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m4 $(LDFLAGS) -T stm32g4.ld startup.c pull_mirror.c -o $@

stm32l0_tick.elf: $(SRCS) stm32l0.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m0plus -DPART_STM32L0 $(LDFLAGS) -T stm32l0.ld $(SRCS) -o $@

nrf52840_tick.elf: $(SRCS) nrf52840.ld
	$(CC) $(CFLAGS) -mcpu=cortex-m4 -DPART_NRF52840 $(LDFLAGS) -T nrf52840.ld $(SRCS) -o $@

fe310_tick.elf: fe310_tick.rs fe310.ld
	$(RUSTC) --edition 2021 --target riscv32imac-unknown-none-elf \
	    -C opt-level=2 -C panic=abort -C link-arg=-Tfe310.ld \
	    fe310_tick.rs -o $@

clean:
	rm -f $(TARGETS)

.PHONY: all clean
