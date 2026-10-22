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
          nrf52840_tick.elf fe310_tick.elf $(PLL_TARGETS)

# CONFIGURED-CLOCK images (pll_tick.c): the firmware sets its own clock tree
# and then ticks from it, once with SysTick (HCLK) and once with TIM2 (the
# timer input clock). <part>_<clock>_<tick>.elf; the clock and tick choices are
# -D flags so one source covers the matrix. See pll_tick.c for the numbers.
PLL_CORTEX_M3 = stm32f103
PLL_CORTEX_M4 = stm32f4 stm32g4
PLL_CORTEX_M0 = stm32f072 stm32l0
PLL_TARGETS = \
    stm32f103_hsi_tim.elf stm32f103_pll_systick.elf stm32f103_pll_tim.elf \
    stm32f4_hsi_tim.elf stm32f4_pll_systick.elf stm32f4_pll_tim.elf \
    stm32f072_hsi_systick.elf stm32f072_pll_systick.elf stm32f072_pll_tim.elf \
    stm32l0_hsi_tim.elf stm32l0_msi6_systick.elf stm32l0_pll_systick.elf stm32l0_pll_tim.elf \
    stm32g4_hsi_tim.elf stm32g4_pll_systick.elf stm32g4_pll_tim.elf

# External-clock readiness probes (ready_probe.c): HSERDY then PLLRDY markers.
READY_TARGETS = stm32f4_ready_probe.elf stm32l0_ready_probe.elf stm32f072_ready_probe.elf stm32g4_ready_probe.elf

all: $(TARGETS) $(READY_TARGETS)

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

# stm32<part>_<clock>_<tick>.elf -> -mcpu from the part, -DPART_*, -DCLK_*, -DTICK_*.
PLL_MCPU_stm32f103 = cortex-m3
PLL_MCPU_stm32f4 = cortex-m4
PLL_MCPU_stm32g4 = cortex-m4
PLL_MCPU_stm32f072 = cortex-m0
PLL_MCPU_stm32l0 = cortex-m0plus
PLL_DEF_stm32f103 = PART_STM32F103
PLL_DEF_stm32f4 = PART_STM32F4
PLL_DEF_stm32g4 = PART_STM32G4
PLL_DEF_stm32f072 = PART_STM32F072
PLL_DEF_stm32l0 = PART_STM32L0
PLL_LD_stm32f103 = stm32f103.ld
PLL_LD_stm32f4 = stm32f4.ld
PLL_LD_stm32g4 = stm32g4.ld
PLL_LD_stm32f072 = stm32f072.ld
PLL_LD_stm32l0 = stm32l0.ld

define PLL_RULE
$(1)_$(2)_$(3).elf: pll_tick.c startup.c $$(PLL_LD_$(1))
	$$(CC) $$(CFLAGS) -mcpu=$$(PLL_MCPU_$(1)) -D$$(PLL_DEF_$(1)) $$(if $$(filter-out hsi,$(2)),-DCLK_$$(shell echo $(2) | tr a-z A-Z)) \
	    $$(if $$(filter tim,$(3)),-DTICK_TIM) $$(LDFLAGS) -T $$(PLL_LD_$(1)) startup.c pll_tick.c -o $$@
endef
$(foreach t,$(PLL_TARGETS),$(eval $(call PLL_RULE,$(word 1,$(subst _, ,$(t))),$(word 2,$(subst _, ,$(t))),$(patsubst %.elf,%,$(word 3,$(subst _, ,$(t)))))))

define READY_RULE
$(1)_ready_probe.elf: ready_probe.c startup.c $$(PLL_LD_$(1))
	$$(CC) $$(CFLAGS) -mcpu=$$(PLL_MCPU_$(1)) -D$$(PLL_DEF_$(1)) $$(LDFLAGS) -T $$(PLL_LD_$(1)) startup.c ready_probe.c -o $$@
endef
$(foreach p,stm32f4 stm32l0 stm32f072 stm32g4,$(eval $(call READY_RULE,$(p))))

clean:
	rm -f $(TARGETS) $(READY_TARGETS)

.PHONY: all clean

pll: $(PLL_TARGETS)

.PHONY: pll
