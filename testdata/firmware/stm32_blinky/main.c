/* Minimal bare-metal STM32F103 firmware for the hauksbee Renode co-sim demo.
 *
 * Exercises the same coupling paths as the AVR demo firmware:
 *   - GPIO out: PC13 (blue pill onboard LED) toggles every 100 ms.
 *   - GPIO out: PA5 driven HIGH at boot as a steady "alive" indicator that
 *     feeds the solved analog circuit (LED + resistor on the demo board).
 *   - UART:     USART1 at 115200 8N1. Prints "hello from stm32\r\n" once at
 *               boot, then on every received byte:
 *                 'v' -> prints the loop tick count as decimal + "\r\n"
 *                 'i' -> prints the ident string again
 *                 else -> echoes the byte back.
 *
 * No vendor SDK: direct register access only, so it builds with a bare
 * arm-none-eabi-gcc and a tiny linker script + vector table. Clocks run from
 * the reset-default 8 MHz HSI (no PLL bring-up needed), which is what
 * db/mcu/stm32f103.soc.toml declares and what the platform is now held to;
 * Renode models USART baud generation faithfully against the configured
 * peripheral clock, so the bytes come out regardless of exact wall timing.
 *
 * The 100 ms comes from SysTick, not from a counted busy-wait. See the comment
 * on delay_ms: the old busy-wait constant had been tuned against Renode's
 * default 100 MIPS rather than this part's ~8, so it made the emulator's wrong
 * clock look like a tidy 5 Hz blink. A SysTick delay is 100 ms on the bench and
 * 100 ms of virtual time here, with nobody guessing an instruction throughput.
 */

#include <stdint.h>

/* ---- Peripheral base addresses (STM32F103) ---- */
#define RCC_BASE    0x40021000UL
#define GPIOA_BASE  0x40010800UL
#define GPIOC_BASE  0x40011000UL
#define USART1_BASE 0x40013800UL

#define REG(addr) (*(volatile uint32_t *)(addr))

/* RCC */
#define RCC_APB2ENR REG(RCC_BASE + 0x18)
#define RCC_APB2ENR_IOPAEN  (1U << 2)
#define RCC_APB2ENR_IOPCEN  (1U << 4)
#define RCC_APB2ENR_USART1EN (1U << 14)

/* GPIO port: CRL (pins 0-7), CRH (pins 8-15), IDR, ODR, BSRR */
#define GPIO_CRL(base) REG((base) + 0x00)
#define GPIO_CRH(base) REG((base) + 0x04)
#define GPIO_ODR(base) REG((base) + 0x0C)
#define GPIO_BSRR(base) REG((base) + 0x10)

/* USART */
#define USART_SR(base)  REG((base) + 0x00)
#define USART_DR(base)  REG((base) + 0x04)
#define USART_BRR(base) REG((base) + 0x08)
#define USART_CR1(base) REG((base) + 0x0C)
#define USART_SR_RXNE (1U << 5)
#define USART_SR_TXE  (1U << 7)
#define USART_CR1_RE  (1U << 2)
#define USART_CR1_TE  (1U << 3)
#define USART_CR1_UE  (1U << 13)

/* Pin config nibble values for the CRL/CRH registers. */
#define CNF_OUTPUT_PP_50MHZ 0x3   /* general purpose push-pull, 50 MHz */
#define CNF_AF_PP_50MHZ     0xB   /* alternate function push-pull, 50 MHz */
#define CNF_INPUT_FLOATING  0x4   /* floating input */

/* The core clock this firmware runs on: the F103's reset default, HSI at 8 MHz
 * (RM0008 §7.2). No PLL is enabled below, so this is the truth on hardware, and
 * it is what db/mcu/stm32f103.soc.toml declares. SysTick and the USART baud
 * divisor are both derived from it, so there is one number to change if this
 * firmware ever brings up the PLL. */
#define SYSCLK_HZ 8000000UL

static void clock_init(void) {
    RCC_APB2ENR |= RCC_APB2ENR_IOPAEN | RCC_APB2ENR_IOPCEN
                 | RCC_APB2ENR_USART1EN;
}

static void gpio_init(void) {
    /* PC13: output push-pull (onboard LED). It lives in CRH (pin 13). */
    uint32_t crh = GPIO_CRH(GPIOC_BASE);
    crh &= ~(0xFU << ((13 - 8) * 4));
    crh |= (CNF_OUTPUT_PP_50MHZ << ((13 - 8) * 4));
    GPIO_CRH(GPIOC_BASE) = crh;

    /* PA5: output push-pull (alive indicator into the analog circuit). */
    uint32_t crl_a = GPIO_CRL(GPIOA_BASE);
    crl_a &= ~(0xFU << (5 * 4));
    crl_a |= (CNF_OUTPUT_PP_50MHZ << (5 * 4));
    GPIO_CRL(GPIOA_BASE) = crl_a;

    /* PA9: USART1 TX -> alternate function push-pull (CRH, pin 9). */
    /* PA10: USART1 RX -> floating input (CRH, pin 10). */
    uint32_t crh_a = GPIO_CRH(GPIOA_BASE);
    crh_a &= ~(0xFU << ((9 - 8) * 4));
    crh_a |= (CNF_AF_PP_50MHZ << ((9 - 8) * 4));
    crh_a &= ~(0xFU << ((10 - 8) * 4));
    crh_a |= (CNF_INPUT_FLOATING << ((10 - 8) * 4));
    GPIO_CRH(GPIOA_BASE) = crh_a;

    /* Drive PA5 HIGH so the analog LED net is energised from boot. */
    GPIO_BSRR(GPIOA_BASE) = (1U << 5);
}

static void uart_init(void) {
    /* 8 MHz HSI PCLK2 / 115200 ~= 69.4 -> BRR = 0x45 (mantissa 4, frac 5). */
    USART_BRR(USART1_BASE) = 0x45;
    USART_CR1(USART1_BASE) = USART_CR1_TE | USART_CR1_RE | USART_CR1_UE;
}

static void uart_tx(uint8_t c) {
    while (!(USART_SR(USART1_BASE) & USART_SR_TXE))
        ;
    USART_DR(USART1_BASE) = c;
}

static void uart_puts(const char *s) {
    while (*s)
        uart_tx((uint8_t)*s++);
}

static int uart_poll(uint8_t *out) {
    if (USART_SR(USART1_BASE) & USART_SR_RXNE) {
        *out = (uint8_t)(USART_DR(USART1_BASE) & 0xFF);
        return 1;
    }
    return 0;
}

static void print_u32(uint32_t v) {
    char buf[10];
    int i = 0;
    if (v == 0)
        buf[i++] = '0';
    while (v) {
        buf[i++] = (char)('0' + (v % 10));
        v /= 10;
    }
    while (i)
        uart_tx((uint8_t)buf[--i]);
}

/* SysTick delay, in milliseconds of REAL time.
 *
 * This used to be a busy-wait of 3,300,000 nops, with a comment explaining that
 * "Renode models the Cortex-M3 at roughly 100 MIPS on this platform" so that
 * many iterations came to about 100 ms. Both halves of that were the problem.
 * 100 MIPS was the EMULATOR's default rather than this part's ~8 MIPS at the
 * reset-default HSI, so the loop cost an eighth of the virtual time it costs on
 * a blue pill, and the constant had been tuned to make the wrong clock produce
 * a nice-looking blink rate. Once the platform declares the part's real 8 MHz
 * (db/mcu/stm32f103.soc.toml), the same loop takes about 1.65 s.
 *
 * SysTick counts the core clock, so 8000 counts is a millisecond on hardware
 * and a millisecond of virtual time here, and the blink rate no longer depends
 * on anyone guessing an instruction throughput. The demo keeps its ~5 Hz LED,
 * which the engine's 50-100 us analog chunks oversample comfortably, and now
 * keeps it for a reason that is true on the bench as well. */
#define STK_CTRL REG(0xE000E010UL)
#define STK_LOAD REG(0xE000E014UL)
#define STK_VAL REG(0xE000E018UL)

static void systick_init(void) {
    STK_LOAD = (SYSCLK_HZ / 1000UL) - 1UL; /* one millisecond */
    STK_VAL = 0;
    STK_CTRL = (1U << 2) | (1U << 0); /* CLKSOURCE = core clock, ENABLE */
}

static void delay_ms(uint32_t ms) {
    while (ms--)
        while (!(STK_CTRL & (1U << 16))) /* COUNTFLAG */
            ;
}

int main(void) {
    clock_init();
    systick_init();
    gpio_init();
    uart_init();

    uart_puts("hello from stm32\r\n");

    uint32_t ticks = 0;
    for (;;) {
        /* Toggle PC13 each loop pass, 100 ms apart: a ~5 Hz LED on hardware and
         * the same 5 Hz of virtual time in co-simulation. */
        GPIO_ODR(GPIOC_BASE) ^= (1U << 13);
        ticks++;

        uint8_t c;
        while (uart_poll(&c)) {
            if (c == 'v') {
                print_u32(ticks);
                uart_puts("\r\n");
            } else if (c == 'i') {
                uart_puts("hello from stm32\r\n");
            } else {
                uart_tx(c);
            }
        }

        delay_ms(100);
    }
}
