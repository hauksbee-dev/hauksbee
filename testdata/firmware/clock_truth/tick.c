/* Clock-truth gate firmware: one image per Cortex-M part whose only job is to
 * put a KNOWN REAL-SILICON PERIOD on a GPIO pin.
 *
 * WHY THIS EXISTS
 * ---------------------------------------------------------------------------
 * A co-simulated MCU's virtual time is only worth something if a firmware
 * delay costs the same virtual time it would cost on the part. Four Renode
 * platforms used to run 6.5x to 9x fast, because the stock .repl declared a
 * core clock (`nvic systickFrequency`, `cpu PerformanceInMips`) that had
 * nothing to do with the part the descriptor claimed. Nothing complained,
 * because `frequency_hz` cancels out of the engine's own bookkeeping. This
 * firmware is the measuring stick: `crates/hauksbee-mcu/tests/clock_truth.rs`
 * boots it and asserts the ratio is 1.00 within 5%.
 *
 * WHAT MAKES THE PERIOD A SILICON FACT
 * ---------------------------------------------------------------------------
 * SysTick counts the CORE CLOCK (CLKSOURCE = 1), and each part is left on its
 * RESET-DEFAULT core clock: no PLL is enabled, no clock mux is touched, no
 * vendor SDK runs. So `CORE_HZ` below is the datasheet reset default, and
 * `TICK_HZ` ticks of SysTick is exactly one millisecond of real time on real
 * silicon, for any speed grade of the part. `HALF_PERIOD_MS` milliseconds
 * between edges is therefore a hardware quantity, not a tuned-to-the-emulator
 * one: on a bench a scope would read HALF_PERIOD_MS between edges.
 *
 * THE ALIASING TRAP (read this before changing HALF_PERIOD_MS)
 * ---------------------------------------------------------------------------
 * The engine observes the pin by polling the output-data register once per
 * chunk, so a half-period at or below the chunk width aliases: an earlier
 * revision of this measurement read a PERFECT 100 edges from a 9x-fast
 * STM32F103 because 5 ms chunks could not resolve its 2.22 ms half-period, and
 * nearly shipped "F103 timing is exact". HALF_PERIOD_MS is 100 ms so that the
 * gate's 1 ms chunk is 100x finer than the silicon half-period, and still 10x
 * finer than the half-period of a sim running 10x fast. Any change here has to
 * keep that second margin, not just the first.
 *
 * No UART, no interrupts, no vendor headers: the fewer moving parts, the fewer
 * ways for a clock error to hide.
 */

#include <stdint.h>

#define REG(a) (*(volatile uint32_t *)(a))

/* Cortex-M SysTick, identical on every part here. */
#define STK_CTRL REG(0xE000E010UL)
#define STK_LOAD REG(0xE000E014UL)
#define STK_VAL REG(0xE000E018UL)
#define STK_COUNTFLAG (1UL << 16)
#define STK_CLKSOURCE_CORE (1UL << 2)
#define STK_ENABLE (1UL << 0)

/* Half-period in milliseconds of REAL time. See the aliasing note above. */
#define HALF_PERIOD_MS 100UL

#if defined(PART_STM32F103)
/* STM32F103C8 reset default: HSI, 8 MHz, no PLL (RM0008 §7.2 — HSI is the
 * system clock after reset). db/mcu/stm32f103.soc.toml declares 8 MHz. */
#define CORE_HZ 8000000UL
static void pin_init(void) {
    REG(0x40021018UL) |= (1UL << 4); /* RCC_APB2ENR: IOPCEN */
    /* GPIOC_CRH MODE13/CNF13 = 0b0011: general-purpose push-pull, 50 MHz. */
    REG(0x40011004UL) = (REG(0x40011004UL) & ~(0xFUL << 20)) | (0x3UL << 20);
}
static void pin_toggle(void) { REG(0x4001100CUL) ^= (1UL << 13); } /* GPIOC_ODR */

#elif defined(PART_STM32F4)
/* STM32F407 reset default: HSI, 16 MHz, no PLL (RM0090 §6.2 — HSI is the
 * system clock after reset). db/mcu/stm32f4_discovery.soc.toml declares
 * 16 MHz. PD12 is the Discovery board's green LED. */
#define CORE_HZ 16000000UL
static void pin_init(void) {
    REG(0x40023830UL) |= (1UL << 3); /* RCC_AHB1ENR: GPIODEN */
    /* GPIOD_MODER pin 12 = 0b01: general-purpose output. */
    REG(0x40020C00UL) = (REG(0x40020C00UL) & ~(0x3UL << 24)) | (0x1UL << 24);
}
static void pin_toggle(void) { REG(0x40020C14UL) ^= (1UL << 12); } /* GPIOD_ODR */

#elif defined(PART_NRF52840)
/* nRF52840: the HFCLK is 64 MHz and cannot be anything else — the M4 core
 * clock is fixed at 64 MHz whether the source is the internal oscillator or
 * the crystal (nRF52840 PS v1.7 §5.3), so there is no "unconfigured" rate to
 * distinguish. db/mcu/nrf52840.soc.toml declares 64 MHz. P0.13 is the DK's
 * button/LED pin and the pin the audit firmware used. */
#define CORE_HZ 64000000UL
static void pin_init(void) { REG(0x50000514UL) |= (1UL << 13); } /* P0.DIR */
static void pin_toggle(void) { REG(0x50000504UL) ^= (1UL << 13); } /* P0.OUT */

#else
#error "define one of PART_STM32F103 / PART_STM32F4 / PART_NRF52840"
#endif

int main(void) {
    pin_init();

    /* One millisecond of SysTick at the part's reset-default core clock. */
    STK_LOAD = (CORE_HZ / 1000UL) - 1UL;
    STK_VAL = 0;
    STK_CTRL = STK_CLKSOURCE_CORE | STK_ENABLE;

    for (;;) {
        for (uint32_t i = 0; i < HALF_PERIOD_MS; i++) {
            while (!(STK_CTRL & STK_COUNTFLAG)) {
            }
        }
        pin_toggle();
    }
}
