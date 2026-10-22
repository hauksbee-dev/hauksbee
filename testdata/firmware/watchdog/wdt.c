/* Watchdog fixture: a boot pulse on one GPIO pin, then an armed watchdog that
 * is never fed (or, with -DFEED, fed forever).
 *
 * Every boot of the core drives the pin high for a short busy loop and then
 * low, so the number of rising edges on the pin is the number of times the
 * core booted. The pulse is 3 ms of SysTick time so the engine's polling
 * (once per chunk of 1 ms) cannot miss it. A starved 50 ms watchdog reboots the
 * core about every 53 ms; the FEED control boots once and never again. The timeout comes from the part's
 * documented clock: nRF52840 WDT on its 32768 Hz clock (PS v1.7, WDT chapter:
 * timeout = (CRV + 1) / 32768 s); STM32 IWDG on LSI through /4 (RM0008 s.19:
 * timeout = RL * 4 / f_LSI). No vendor SDK, no interrupts.
 */
#include <stdint.h>

#define REG(a) (*(volatile uint32_t *)(a))

#if defined(PART_NRF52840)
#define CORE_HZ 64000000UL /* nRF52840 PS v1.7 s.5.3: HFCLK fixed at 64 MHz */
static void pin_init(void) { REG(0x50000514UL) |= (1UL << 13); } /* P0.DIR */
static void pin_set(int high) {
    if (high) REG(0x50000508UL) = (1UL << 13); /* P0.OUTSET */
    else REG(0x5000050CUL) = (1UL << 13);      /* P0.OUTCLR */
}
#define WDT_BASE 0x40010000UL
static void wdt_start(void) {
    REG(WDT_BASE + 0x504) = 1638; /* CRV: (1638+1)/32768 = 50.02 ms */
    REG(WDT_BASE + 0x508) = 1;    /* RREN: reload request 0 enabled */
    REG(WDT_BASE + 0x000) = 1;    /* TASKS_START */
}
#if defined(FEED)
static void wdt_feed(void) { REG(WDT_BASE + 0x600) = 0x6E524635UL; }
#endif

#elif defined(PART_STM32F103)
#define CORE_HZ 8000000UL /* RM0008 s.7.2: HSI 8 MHz is SYSCLK after reset */
static void pin_init(void) {
    REG(0x40021018UL) |= (1UL << 4); /* RCC_APB2ENR: IOPCEN */
    REG(0x40011004UL) = (REG(0x40011004UL) & ~(0xFUL << 20)) | (0x3UL << 20); /* PC13 push-pull */
}
static void pin_set(int high) {
    if (high) REG(0x40011010UL) = (1UL << 13);   /* GPIOC_BSRR set */
    else REG(0x40011010UL) = (1UL << (13 + 16)); /* GPIOC_BSRR reset */
}
#define IWDG_BASE 0x40003000UL
static void wdt_start(void) {
    REG(IWDG_BASE + 0x00) = 0xCCCC; /* KR: start */
    REG(IWDG_BASE + 0x00) = 0x5555; /* KR: unlock PR/RLR */
    REG(IWDG_BASE + 0x04) = 0;      /* PR: /4 -> 10 kHz from 40 kHz LSI */
    REG(IWDG_BASE + 0x08) = 500;    /* RLR: 500 * 100 us = 50 ms */
    REG(IWDG_BASE + 0x00) = 0xAAAA; /* KR: reload */
}
#if defined(FEED)
static void wdt_feed(void) { REG(IWDG_BASE + 0x00) = 0xAAAA; }
#endif

#else
#error "define PART_NRF52840 or PART_STM32F103"
#endif

/* Cortex-M SysTick on the core clock: wait `ms` milliseconds of real time. */
static void delay_ms(uint32_t ms) {
    REG(0xE000E014UL) = (CORE_HZ / 1000UL) - 1UL; /* STK_LOAD: 1 ms */
    REG(0xE000E018UL) = 0;                        /* STK_VAL */
    REG(0xE000E010UL) = (1UL << 2) | 1UL;         /* STK_CTRL: core clock, enable */
    while (ms--) {
        while (!(REG(0xE000E010UL) & (1UL << 16))) {
        }
    }
    REG(0xE000E010UL) = 0;
}

int main(void) {
    pin_init();
    pin_set(1);
    delay_ms(3);
    pin_set(0);
    wdt_start();
    for (;;) {
#if defined(FEED)
        wdt_feed();
#endif
    }
}
