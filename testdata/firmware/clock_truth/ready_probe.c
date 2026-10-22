/* External-clock readiness probe for the MODER-style STM32 families.
 *
 * PA5 rises only after HSERDY. PA6 rises only after PLLRDY, with the PLL fed
 * from HSE. A board without a crystal must leave both pins low forever (the
 * firmware has no timeout, like the HAL's bring-up loop it stands in for); a
 * board with one must raise them in order, after the oscillator startup time
 * and the PLL lock time. The firmware never switches SYSCLK, so only the
 * ready flags are exercised.
 *
 * Register layouts, from the reference manuals (page numbers are printed
 * pages; manuals are in target/launch/wp1-datasheets):
 *
 *   PART_STM32F4   RM0090 Rev 22: RCC_CR +0x00 (HSEON 16, HSERDY 17, PLLON 24,
 *                  PLLRDY 25; pp.162-163), RCC_PLLCFGR +0x04 (PLLSRC bit 22,
 *                  PLLM 5:0, PLLN 14:6; pp.164-165), RCC_AHB1ENR +0x30 (GPIOAEN
 *                  bit 0), GPIOA 0x40020000, BSRR +0x18.
 *   PART_STM32L0   RM0377 Rev 10: RCC_CR +0x00 (same bits; pp.181-183),
 *                  RCC_CFGR +0x0C (PLLSRC bit 16, PLLMUL 21:18, PLLDIV 23:22;
 *                  pp.185-186), RCC_IOPENR +0x2C (IOPAEN bit 0), GPIOA
 *                  0x50000000, BSRR +0x18.
 *   PART_STM32F072 RM0091 Rev 10: RCC_CR +0x00 (same bits; pp.110-111),
 *                  RCC_CFGR +0x04 (PLLSRC 16:15 = 10 HSE/PREDIV, PLLMUL
 *                  21:18; pp.111-114), RCC_AHBENR +0x14 (IOPAEN bit 17), GPIOA
 *                  0x48000000, BSRR +0x18.
 *   PART_STM32G4   RM0440 Rev 9: RCC_CR +0x00 (same bits; p.287), RCC_PLLCFGR
 *                  +0x0C (PLLSRC 1:0 = 11 HSE, PLLM 7:4, PLLN 14:8; pp.291-293),
 *                  RCC_AHB2ENR +0x4C (GPIOAEN bit 0), GPIOA 0x48000000,
 *                  BSRR +0x18.
 */

#include <stdint.h>

#define REG(a) (*(volatile uint32_t *)(a))

#define CR_HSEON (1UL << 16)
#define CR_HSERDY (1UL << 17)
#define CR_PLLON (1UL << 24)
#define CR_PLLRDY (1UL << 25)

#if defined(PART_STM32F4)
#define RCC_CR REG(0x40023800UL)
#define GPIOA 0x40020000UL
static void gpio_clock_on(void) { REG(0x40023830UL) |= 1UL; }
static void pll_config_hse(void) {
    /* PLLSRC = HSE, PLLM = 8, PLLN = 336, PLLP = /2 */
    REG(0x40023804UL) = (1UL << 22) | 8UL | (336UL << 6);
}
#elif defined(PART_STM32L0)
#define RCC_CR REG(0x40021000UL)
#define GPIOA 0x50000000UL
static void gpio_clock_on(void) { REG(0x4002102CUL) |= 1UL; }
static void pll_config_hse(void) {
    /* PLLSRC = HSE, PLLMUL x4, PLLDIV /2 */
    REG(0x4002100CUL) = (1UL << 16) | (1UL << 18) | (1UL << 22);
}
#elif defined(PART_STM32F072)
#define RCC_CR REG(0x40021000UL)
#define GPIOA 0x48000000UL
static void gpio_clock_on(void) { REG(0x40021014UL) |= (1UL << 17); }
static void pll_config_hse(void) {
    /* PLLSRC[1:0] = 10 HSE/PREDIV, PLLMUL x6 */
    REG(0x40021004UL) = (2UL << 15) | (4UL << 18);
}
#elif defined(PART_STM32G4)
#define RCC_CR REG(0x40021000UL)
#define GPIOA 0x48000000UL
static void gpio_clock_on(void) { REG(0x4002104CUL) |= 1UL; }
static void pll_config_hse(void) {
    /* PLLSRC = 11 HSE, PLLM = /2, PLLN = x24 */
    REG(0x4002100CUL) = 3UL | (1UL << 4) | (24UL << 8);
}
#else
#error "define a PART_ macro"
#endif

#define GPIO_MODER REG(GPIOA + 0x00UL)
#define GPIO_BSRR REG(GPIOA + 0x18UL)

int main(void) {
    gpio_clock_on();
    GPIO_MODER = (GPIO_MODER & ~((3UL << 10) | (3UL << 12))) | (1UL << 10) | (1UL << 12);

    RCC_CR |= CR_HSEON;
    while (!(RCC_CR & CR_HSERDY)) {
    }
    GPIO_BSRR = 1UL << 5;

    pll_config_hse();
    RCC_CR |= CR_PLLON;
    while (!(RCC_CR & CR_PLLRDY)) {
    }
    GPIO_BSRR = 1UL << 6;

    for (;;) {
    }
}
