/* Clock-truth gate, CONFIGURED-CLOCK variants.
 *
 * tick.c measures a part on its reset-default clock. This file measures the
 * same silicon fact AFTER the firmware has configured the clock tree, which is
 * where a fixed-rate emulator is most wrong: an STM32F4 Discovery firmware that
 * brings the PLL to 168 MHz sees SysTick run 10.5x slow on a platform pinned to
 * the 16 MHz reset default, and a bare-metal STM32F103 TIMx time base ran 9x
 * fast on a platform whose timers were pinned to 72 MHz.
 *
 * WHAT MAKES THE PERIOD A SILICON FACT
 * ---------------------------------------------------------------------------
 * Every image derives its tick from the clock it configured, using only
 * numbers in the part's reference manual:
 *
 *   - SYSTICK images (default): CORE_HZ is HCLK as the manual's clock-tree
 *     arithmetic gives it, SysTick counts HCLK (CLKSOURCE = 1), and
 *     CORE_HZ / 1000 ticks are exactly 1 ms of real time on silicon.
 *   - TIMER images (-DTICK_TIM): TIM2 counts TIM_HZ, the timer input clock the
 *     manual defines (x1 when the APB prescaler is 1, x2 otherwise), and a
 *     prescaler of TIM_HZ / 10000 - 1 makes the counter tick at exactly 10 kHz.
 *
 * Either way HALF_PERIOD_MS = 100 ms between edges is a bench quantity: a scope
 * on the pin reads 100 ms. Each image is built WITHOUT any vendor SDK and
 * touches no pin until its clock is switched, so a firmware that stalls in a
 * ready-flag loop shows no edge at all, which the gate reports as a 0.0 ratio.
 *
 * PART / CLOCK MATRIX (all PLLs run from the internal HSI so no crystal is
 * required; the numbers come from the reference manuals cited at each block):
 *
 *   PART_STM32F103  HSI/2 x16 = 64 MHz           RM0008 s7.3.2 (CFGR), s7.2
 *   PART_STM32F4    HSI /16 x336 /2 = 168 MHz    RM0090 s6.3.2 (PLLCFGR)
 *   PART_STM32F072  HSI/2 x12 = 48 MHz           RM0091 CFGR PLLMUL
 *   PART_STM32L0    HSI16 x4 /2 = 32 MHz         RM0377 CFGR PLLMUL/PLLDIV
 *   PART_STM32G4    HSI16 /4 x64 /2 = 128 MHz    RM0440 PLLCFGR
 */

#include <stdint.h>

#define REG(a) (*(volatile uint32_t *)(a))

#define STK_CTRL REG(0xE000E010UL)
#define STK_LOAD REG(0xE000E014UL)
#define STK_VAL REG(0xE000E018UL)
#define STK_COUNTFLAG (1UL << 16)

#define HALF_PERIOD_MS 100UL

/* General-purpose timer 2 register offsets (identical across these parts). */
#define TIM2_BASE 0x40000000UL
#define TIM_CR1 REG(TIM2_BASE + 0x00)
#define TIM_SR REG(TIM2_BASE + 0x10)
#define TIM_EGR REG(TIM2_BASE + 0x14)
#define TIM_PSC REG(TIM2_BASE + 0x28)
#define TIM_ARR REG(TIM2_BASE + 0x2C)

#if defined(PART_STM32F103)
/* RM0008: RCC base 0x40021000, CR +0, CFGR +4, APB1ENR +0x1C, APB2ENR +0x18. */
#define RCC REG(0x40021000UL)
#define RCC_CFGR REG(0x40021004UL)
#define RCC_APB1ENR REG(0x4002101CUL)
#define FLASH_ACR REG(0x40022000UL)
static void pin_init(void) {
    REG(0x40021018UL) |= (1UL << 4); /* APB2ENR IOPCEN */
    REG(0x40011004UL) = (REG(0x40011004UL) & ~(0xFUL << 20)) | (0x3UL << 20);
}
static void pin_toggle(void) { REG(0x4001100CUL) ^= (1UL << 13); }
#ifdef CLK_PLL
/* CFGR: PLLSRC (bit16) = 0 -> HSI/2 = 4 MHz; PLLMUL (21:18) = 0b1110 -> x16 =
 * 64 MHz; PPRE1 (10:8) = 0b100 -> /2. SYSCLK 64 MHz, HCLK 64 MHz, PCLK1 32 MHz,
 * TIM2CLK = 2 x PCLK1 = 64 MHz (APB prescaler != 1). Flash needs 2 wait states
 * above 48 MHz (FLASH_ACR LATENCY = 2). */
#define CORE_HZ 64000000UL
#define TIM_HZ 64000000UL
static void clock_init(void) {
    FLASH_ACR = (FLASH_ACR & ~7UL) | 2UL;
    RCC_CFGR = (RCC_CFGR & ~((1UL << 16) | (0xFUL << 18) | (7UL << 8))) |
               (0xEUL << 18) | (4UL << 8);
    RCC |= (1UL << 24); /* PLLON */
    while (!(RCC & (1UL << 25))) {
    }
    RCC_CFGR = (RCC_CFGR & ~3UL) | 2UL; /* SW = PLL */
    while (((RCC_CFGR >> 2) & 3UL) != 2UL) {
    }
}
#else
#define CORE_HZ 8000000UL
#define TIM_HZ 8000000UL
static void clock_init(void) {}
#endif
__attribute__((unused)) static void tim_clock_on(void) { RCC_APB1ENR |= 1UL; }

#elif defined(PART_STM32F4)
/* RM0090 s6.3: RCC base 0x40023800, CR +0, PLLCFGR +4, CFGR +8, APB1ENR +0x40. */
#define RCC REG(0x40023800UL)
#define RCC_PLLCFGR REG(0x40023804UL)
#define RCC_CFGR REG(0x40023808UL)
#define RCC_APB1ENR REG(0x40023840UL)
#define FLASH_ACR REG(0x40023C00UL)
static void pin_init(void) {
    REG(0x40023830UL) |= (1UL << 3); /* AHB1ENR GPIODEN */
    REG(0x40020C00UL) = (REG(0x40020C00UL) & ~(0x3UL << 24)) | (0x1UL << 24);
}
static void pin_toggle(void) { REG(0x40020C14UL) ^= (1UL << 12); }
#ifdef CLK_PLL
/* PLLCFGR: PLLSRC (22) = 0 HSI, PLLM (5:0) = 16 -> 1 MHz VCO input, PLLN (14:6)
 * = 336 -> 336 MHz VCO, PLLP (17:16) = 0b00 -> /2 -> 168 MHz, PLLQ (27:24) = 7.
 * CFGR: HPRE (7:4) = 0 /1; PPRE1 (12:10) = 0b101 /4 -> PCLK1 42 MHz; PPRE2
 * (15:13) = 0b100 /2 -> PCLK2 84 MHz. TIM2 is on APB1 with a prescaler != 1 so
 * TIM2CLK = 2 x 42 = 84 MHz. 168 MHz needs 5 flash wait states. */
#define CORE_HZ 168000000UL
#define TIM_HZ 84000000UL
static void clock_init(void) {
    FLASH_ACR = (FLASH_ACR & ~0xFUL) | 5UL;
    RCC_PLLCFGR = (16UL << 0) | (336UL << 6) | (0UL << 16) | (0UL << 22) | (7UL << 24);
    RCC_CFGR = (RCC_CFGR & ~((7UL << 10) | (7UL << 13))) | (5UL << 10) | (4UL << 13);
    RCC |= (1UL << 24); /* PLLON */
    while (!(RCC & (1UL << 25))) {
    }
    RCC_CFGR = (RCC_CFGR & ~3UL) | 2UL; /* SW = PLL */
    while (((RCC_CFGR >> 2) & 3UL) != 2UL) {
    }
}
#else
#define CORE_HZ 16000000UL
#define TIM_HZ 16000000UL
static void clock_init(void) {}
#endif
__attribute__((unused)) static void tim_clock_on(void) { RCC_APB1ENR |= 1UL; }

#elif defined(PART_STM32F072)
/* RM0091: RCC base 0x40021000, CR +0, CFGR +4, AHBENR +0x14, APB1ENR +0x1C.
 * PC6 is the walkthrough blinky's pin. */
#define RCC REG(0x40021000UL)
#define RCC_CFGR REG(0x40021004UL)
#define RCC_AHBENR REG(0x40021014UL)
#define RCC_APB1ENR REG(0x4002101CUL)
#define FLASH_ACR REG(0x40022000UL)
static void pin_init(void) {
    RCC_AHBENR |= (1UL << 19); /* IOPCEN */
    REG(0x48000800UL) = (REG(0x48000800UL) & ~(0x3UL << 12)) | (0x1UL << 12);
}
static void pin_toggle(void) { REG(0x48000814UL) ^= (1UL << 6); }
#ifdef CLK_PLL
/* CFGR: PLLSRC (16:15) = 0b00 HSI/2 = 4 MHz; PLLMUL (21:18) = 0b1010 -> x12 =
 * 48 MHz; PPRE (10:8) = 0b100 -> /2 so PCLK = 24 MHz and TIM2CLK = 2 x PCLK =
 * 48 MHz. One flash wait state above 24 MHz. */
#define CORE_HZ 48000000UL
#define TIM_HZ 48000000UL
static void clock_init(void) {
    FLASH_ACR = (FLASH_ACR & ~7UL) | 1UL;
    RCC_CFGR = (RCC_CFGR & ~((3UL << 15) | (0xFUL << 18) | (7UL << 8))) |
               (0xAUL << 18) | (4UL << 8);
    RCC |= (1UL << 24);
    while (!(RCC & (1UL << 25))) {
    }
    RCC_CFGR = (RCC_CFGR & ~3UL) | 2UL;
    while (((RCC_CFGR >> 2) & 3UL) != 2UL) {
    }
}
#else
#define CORE_HZ 8000000UL
#define TIM_HZ 8000000UL
static void clock_init(void) {}
#endif
__attribute__((unused)) static void tim_clock_on(void) { RCC_APB1ENR |= 1UL; }

#elif defined(PART_STM32L0)
/* RM0377: RCC base 0x40021000, CR +0, ICSCR +4, CFGR +0xC, IOPENR +0x2C,
 * APB1ENR +0x38. PA5 is the Nucleo-L0 LED. */
#define RCC REG(0x40021000UL)
#define RCC_ICSCR REG(0x40021004UL)
#define RCC_CFGR REG(0x4002100CUL)
#define RCC_APB1ENR REG(0x40021038UL)
#define FLASH_ACR REG(0x40022000UL)
static void pin_init(void) {
    REG(0x4002102CUL) |= 1UL; /* IOPAEN */
    REG(0x50000000UL) = (REG(0x50000000UL) & ~(0x3UL << 10)) | (0x1UL << 10);
}
static void pin_toggle(void) { REG(0x50000014UL) ^= (1UL << 5); }
#if defined(CLK_PLL)
/* CR: HSI16ON (bit 0), HSI16RDYF (bit 2). CFGR: PLLSRC (16) = 0 HSI16, PLLMUL
 * (21:18) = 0b0001 -> x4 = 64 MHz VCO, PLLDIV (23:22) = 0b01 -> /2 = 32 MHz;
 * SW (1:0) = 0b11 PLL. One flash wait state above 16 MHz. APB1 prescaler 1 so
 * TIM2CLK = PCLK1 = 32 MHz. */
#define CORE_HZ 32000000UL
#define TIM_HZ 32000000UL
static void clock_init(void) {
    FLASH_ACR |= 1UL;
    RCC |= 1UL;
    while (!(RCC & (1UL << 2))) {
    }
    RCC_CFGR = (RCC_CFGR & ~((1UL << 16) | (0xFUL << 18) | (3UL << 22))) |
               (1UL << 18) | (1UL << 22);
    RCC |= (1UL << 24);
    while (!(RCC & (1UL << 25))) {
    }
    RCC_CFGR = (RCC_CFGR & ~3UL) | 3UL;
    while (((RCC_CFGR >> 2) & 3UL) != 3UL) {
    }
}
#elif defined(CLK_MSI6)
/* ICSCR MSIRANGE (15:13) = 0b110 -> 4.194 MHz (RM0377 ICSCR table). No clock
 * switch: the MSI is already the system clock, only its range changes. */
#define CORE_HZ 4194000UL
#define TIM_HZ 4194000UL
static void clock_init(void) {
    RCC_ICSCR = (RCC_ICSCR & ~(7UL << 13)) | (6UL << 13);
}
#else
#define CORE_HZ 2097000UL
#define TIM_HZ 2097000UL
static void clock_init(void) {}
#endif
__attribute__((unused)) static void tim_clock_on(void) { RCC_APB1ENR |= 1UL; }

#elif defined(PART_STM32G4)
/* RM0440: RCC base 0x40021000, CR +0, CFGR +8, PLLCFGR +0xC, AHB2ENR +0x4C,
 * APB1ENR1 +0x58. PA5 is the Nucleo-G474RE LED. */
#define RCC REG(0x40021000UL)
#define RCC_CFGR REG(0x40021008UL)
#define RCC_PLLCFGR REG(0x4002100CUL)
#define RCC_APB1ENR1 REG(0x40021058UL)
#define FLASH_ACR REG(0x40022000UL)
static void pin_init(void) {
    REG(0x4002104CUL) |= 1UL;
    REG(0x48000000UL) = (REG(0x48000000UL) & ~(0x3UL << 10)) | (0x1UL << 10);
}
static void pin_toggle(void) { REG(0x48000014UL) ^= (1UL << 5); }
#ifdef CLK_PLL
/* PLLCFGR: PLLSRC (1:0) = 0b10 HSI16, PLLM (7:4) = 3 -> /4 = 4 MHz, PLLN
 * (14:8) = 64 -> 256 MHz VCO, PLLREN (24) = 1, PLLR (26:25) = 0b00 -> /2 =
 * 128 MHz (below the 150 MHz non-boost ceiling). PPRE1 (10:8) = 0b100 -> /2 so
 * PCLK1 = 64 MHz, TIM2CLK = 2 x 64 = 128 MHz. Four flash wait states. */
#define CORE_HZ 128000000UL
#define TIM_HZ 128000000UL
static void clock_init(void) {
    FLASH_ACR = (FLASH_ACR & ~0xFUL) | 4UL;
    RCC |= (1UL << 8); /* HSION */
    while (!(RCC & (1UL << 10))) {
    }
    RCC_PLLCFGR = 2UL | (3UL << 4) | (64UL << 8) | (1UL << 24);
    RCC_CFGR = (RCC_CFGR & ~(7UL << 8)) | (4UL << 8);
    RCC |= (1UL << 24);
    while (!(RCC & (1UL << 25))) {
    }
    RCC_CFGR = (RCC_CFGR & ~3UL) | 3UL;
    while (((RCC_CFGR >> 2) & 3UL) != 3UL) {
    }
}
#else
#define CORE_HZ 16000000UL
#define TIM_HZ 16000000UL
static void clock_init(void) {}
#endif
__attribute__((unused)) static void tim_clock_on(void) { RCC_APB1ENR1 |= 1UL; }

#else
#error "define PART_STM32F103 / PART_STM32F4 / PART_STM32F072 / PART_STM32L0 / PART_STM32G4"
#endif

int main(void) {
    pin_init();
    clock_init();

#if defined(TICK_TIM)
    /* TIM2 counts TIM_HZ / (PSC + 1) = 10 kHz (PSC is a 16-bit register, so a
     * 1 kHz tick would overflow it from TIM_HZ >= 65.5 MHz); the counter counts
     * 0..ARR, ARR + 1 ticks per update event, so ARR = 10 * HALF_PERIOD_MS - 1
     * makes the update event (UIF, SR bit 0) arrive every HALF_PERIOD_MS
     * milliseconds. DIER is left at its reset value (0): the update FLAG still
     * sets on silicon, which is what a polled time base relies on. */
    tim_clock_on();
    TIM_PSC = (TIM_HZ / 10000UL) - 1UL;
    TIM_ARR = (10UL * HALF_PERIOD_MS) - 1UL;
    TIM_EGR = 1UL; /* load PSC/ARR now */
    TIM_SR = 0;
    TIM_CR1 = 1UL; /* CEN */
    for (;;) {
        while (!(TIM_SR & 1UL)) {
        }
        TIM_SR = 0;
        pin_toggle();
    }
#else
    STK_LOAD = (CORE_HZ / 1000UL) - 1UL;
    STK_VAL = 0;
    STK_CTRL = (1UL << 2) | 1UL; /* CLKSOURCE = HCLK, ENABLE */
    for (;;) {
        for (uint32_t i = 0; i < HALF_PERIOD_MS; i++) {
            while (!(STK_CTRL & STK_COUNTFLAG)) {
            }
        }
        pin_toggle();
    }
#endif
}
