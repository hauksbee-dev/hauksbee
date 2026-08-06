/* STM32F103 RCC readiness probe.
 *
 * PA5 rises only after HSERDY. PC13 rises only after PLLRDY, with the PLL fed
 * from HSE. A missing crystal must therefore leave both pins low forever; a
 * present crystal must raise them in order after the oscillator and PLL lock
 * delays. The firmware deliberately has no timeout: the test observes the
 * silicon contract directly instead of substituting a software fallback.
 */

#include <stdint.h>

#define REG(addr) (*(volatile uint32_t *)(addr))

#define RCC_BASE 0x40021000UL
#define RCC_CR REG(RCC_BASE + 0x00)
#define RCC_CFGR REG(RCC_BASE + 0x04)
#define RCC_APB2ENR REG(RCC_BASE + 0x18)

#define RCC_CR_HSEON (1U << 16)
#define RCC_CR_HSERDY (1U << 17)
#define RCC_CR_PLLON (1U << 24)
#define RCC_CR_PLLRDY (1U << 25)
#define RCC_CFGR_PLLSRC (1U << 16)
#define RCC_CFGR_PLLMUL9 (7U << 18)
#define RCC_APB2ENR_IOPAEN (1U << 2)
#define RCC_APB2ENR_IOPCEN (1U << 4)

#define GPIOA_BASE 0x40010800UL
#define GPIOC_BASE 0x40011000UL
#define GPIO_CRL(base) REG((base) + 0x00)
#define GPIO_CRH(base) REG((base) + 0x04)
#define GPIO_BSRR(base) REG((base) + 0x10)
#define OUTPUT_PP_2MHZ 0x2U

static void marker_gpio_init(void) {
    RCC_APB2ENR |= RCC_APB2ENR_IOPAEN | RCC_APB2ENR_IOPCEN;

    uint32_t crl = GPIO_CRL(GPIOA_BASE);
    crl &= ~(0xFU << (5U * 4U));
    crl |= OUTPUT_PP_2MHZ << (5U * 4U);
    GPIO_CRL(GPIOA_BASE) = crl;

    uint32_t crh = GPIO_CRH(GPIOC_BASE);
    crh &= ~(0xFU << ((13U - 8U) * 4U));
    crh |= OUTPUT_PP_2MHZ << ((13U - 8U) * 4U);
    GPIO_CRH(GPIOC_BASE) = crh;
}

int main(void) {
    marker_gpio_init();

    RCC_CR |= RCC_CR_HSEON;
    while (!(RCC_CR & RCC_CR_HSERDY))
        ;
    GPIO_BSRR(GPIOA_BASE) = 1U << 5;

    RCC_CFGR = (RCC_CFGR & ~((1U << 16) | (0xFU << 18)))
             | RCC_CFGR_PLLSRC | RCC_CFGR_PLLMUL9;
    RCC_CR |= RCC_CR_PLLON;
    while (!(RCC_CR & RCC_CR_PLLRDY))
        ;
    GPIO_BSRR(GPIOC_BASE) = 1U << 13;

    for (;;)
        ;
}
