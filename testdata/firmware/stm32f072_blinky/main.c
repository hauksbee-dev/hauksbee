/* Minimal bare-metal STM32F072 firmware for the add-a-microcontroller
 * walkthrough (docs/extending/add-a-microcontroller.md, tier B).
 *
 * The STM32F0 family is a different GPIO and USART generation from the F103 the
 * stm32_blinky fixture targets, which is exactly why it is the worked example:
 *   - GPIO lives on AHB2 at 0x48000000 with an F4-style MODER/ODR layout
 *     (MODER at 0x00, ODR at 0x14), not the F1's CRL/CRH with ODR at 0x0C.
 *   - USART1 is the "new" peripheral generation (ISR at 0x1C, TDR at 0x28), not
 *     the F1's SR/DR pair at 0x00/0x04.
 * Reading either offset from the wrong family observes the wrong register
 * silently, which is the footgun the descriptor schema exists to keep in
 * reviewed data.
 *
 * Two builds come out of this one file, so the co-sim test can be two-sided:
 *   blinky.elf  drives PC6 (toggles) and PA5 (held HIGH), and prints on USART1.
 *   quiet.elf   (built with -DQUIET) configures nothing and drives nothing.
 * A test that only checks the first cannot tell a working GPIO bridge from one
 * that reports edges nobody drove.
 *
 * No vendor SDK: direct register access only, so it builds with a bare
 * arm-none-eabi-gcc, a small vector table and a short linker script.
 */

#include <stdint.h>

/* ---- Peripheral base addresses (STM32F072, RM0091) ---- */
#define RCC_BASE    0x40021000UL
#define GPIOA_BASE  0x48000000UL
#define GPIOC_BASE  0x48000800UL
#define USART1_BASE 0x40013800UL

#define REG(addr) (*(volatile uint32_t *)(addr))

/* RCC: GPIO ports hang off AHBENR on the F0, not APB2 as on the F1. */
#define RCC_AHBENR  REG(RCC_BASE + 0x14)
#define RCC_APB2ENR REG(RCC_BASE + 0x18)
#define RCC_AHBENR_IOPAEN (1U << 17)
#define RCC_AHBENR_IOPCEN (1U << 19)
#define RCC_APB2ENR_USART1EN (1U << 14)

/* GPIO port: MODER, OTYPER, OSPEEDR, PUPDR, IDR, ODR, BSRR, ... */
#define GPIO_MODER(base) REG((base) + 0x00)
#define GPIO_ODR(base)   REG((base) + 0x14)
#define GPIO_BSRR(base)  REG((base) + 0x18)
#define GPIO_AFRH(base)  REG((base) + 0x24)

#define MODER_OUTPUT 0x1U /* general-purpose output */
#define MODER_AF     0x2U /* alternate function */
#define MODER_ANALOG 0x3U /* analog input (what an ADC channel pin needs) */

/* ADC (RM0091 section 13). Renode's stock stm32f0.repl instantiates
 * Analog.STM32F0_ADC here with referenceVoltage 3.3. */
#define ADC_BASE 0x40012400UL
#define ADC_ISR    REG(ADC_BASE + 0x00)
#define ADC_CR     REG(ADC_BASE + 0x08)
#define ADC_CHSELR REG(ADC_BASE + 0x28)
#define ADC_DR     REG(ADC_BASE + 0x40)
#define ADC_ISR_ADRDY (1U << 0)
#define ADC_ISR_EOC   (1U << 2)
#define ADC_CR_ADEN    (1U << 0)
#define ADC_CR_ADSTART (1U << 2)
#define RCC_APB2ENR_ADCEN (1U << 9)

/* USART (F0 / F7 register generation) */
#define USART_CR1(base) REG((base) + 0x00)
#define USART_BRR(base) REG((base) + 0x0C)
#define USART_ISR(base) REG((base) + 0x1C)
#define USART_RDR(base) REG((base) + 0x24)
#define USART_TDR(base) REG((base) + 0x28)
#define USART_ISR_RXNE (1U << 5)
#define USART_ISR_TXE  (1U << 7)
#define USART_CR1_UE (1U << 0)
#define USART_CR1_RE (1U << 2)
#define USART_CR1_TE (1U << 3)

/* PC6 is the red LED on the STM32F072 Discovery. PA5 is driven HIGH from boot
 * as a steady "alive" level feeding the solved analog circuit. */
#define LED_PIN   6
#define ALIVE_PIN 5

#ifdef QUIET

/* The negative half of the two-sided proof: no clocks, no pin configuration, no
 * drives, no UART. Any edge or byte the co-sim reports from this image was
 * invented by the bridge, not driven by firmware. */
int main(void) {
    for (;;)
        __asm__ volatile("nop");
}

#else

static void moder_set(volatile uint32_t *moder, uint32_t pin, uint32_t mode) {
    uint32_t v = *moder;
    v &= ~(0x3U << (pin * 2));
    v |= mode << (pin * 2);
    *moder = v;
}

static void clock_init(void) {
    RCC_AHBENR |= RCC_AHBENR_IOPAEN | RCC_AHBENR_IOPCEN;
    RCC_APB2ENR |= RCC_APB2ENR_USART1EN;
}

static void gpio_init(void) {
    moder_set(&GPIO_MODER(GPIOC_BASE), LED_PIN, MODER_OUTPUT);
    moder_set(&GPIO_MODER(GPIOA_BASE), ALIVE_PIN, MODER_OUTPUT);

    /* PA9 = USART1_TX, PA10 = USART1_RX, both alternate function 1. */
    moder_set(&GPIO_MODER(GPIOA_BASE), 9, MODER_AF);
    moder_set(&GPIO_MODER(GPIOA_BASE), 10, MODER_AF);
    uint32_t afrh = GPIO_AFRH(GPIOA_BASE);
    afrh &= ~(0xFFU << ((9 - 8) * 4));
    afrh |= (1U << ((9 - 8) * 4)) | (1U << ((10 - 8) * 4));
    GPIO_AFRH(GPIOA_BASE) = afrh;

    GPIO_BSRR(GPIOA_BASE) = (1U << ALIVE_PIN);
}

static void uart_init(void) {
    /* 8 MHz HSI / 115200 = 69. The F0 USART BRR is a plain divisor: no
     * mantissa/fraction split, unlike the F1's. */
    USART_BRR(USART1_BASE) = 69;
    USART_CR1(USART1_BASE) = USART_CR1_TE | USART_CR1_RE | USART_CR1_UE;
}

static void uart_tx(uint8_t c) {
    while (!(USART_ISR(USART1_BASE) & USART_ISR_TXE))
        ;
    USART_TDR(USART1_BASE) = c;
}

static void uart_puts(const char *s) {
    while (*s)
        uart_tx((uint8_t)*s++);
}

static int uart_poll(uint8_t *out) {
    if (USART_ISR(USART1_BASE) & USART_ISR_RXNE) {
        *out = (uint8_t)(USART_RDR(USART1_BASE) & 0xFF);
        return 1;
    }
    return 0;
}

/* Hex, not decimal: the Cortex-M0 has no divide instruction, so `% 10` would
 * pull __aeabi_uidivmod out of libgcc and this firmware links against nothing. */
static void print_hex32(uint32_t v) {
    static const char digits[] = "0123456789abcdef";
    for (int shift = 28; shift >= 0; shift -= 4)
        uart_tx((uint8_t)digits[(v >> shift) & 0xF]);
}

/* Busy-wait in virtual time. Renode runs this Cortex-M0 platform fast enough
 * that a few hundred thousand iterations is tens of milliseconds of virtual
 * time: a slow blink the engine's 50 us analog chunks sample cleanly. */
static void delay_loop(volatile uint32_t n) {
    while (n--)
        __asm__ volatile("nop");
}

/* One 12-bit conversion on ADC_IN0 (PA0), RM0091 section 13.
 *
 * Bounded spins, not `while` loops: an emulated ADC model that never raises
 * ADRDY or EOC must make the firmware print a sentinel rather than hang the
 * co-sim, or a missing model reads as a stuck simulation instead of a missing
 * model. 0xffffffff is that sentinel. */
static uint32_t adc_read(uint32_t channel) {
    RCC_APB2ENR |= RCC_APB2ENR_ADCEN;
    moder_set(&GPIO_MODER(GPIOA_BASE), channel, MODER_ANALOG);

    ADC_CHSELR = 1U << channel;
    ADC_CR |= ADC_CR_ADEN;
    for (uint32_t spin = 0; spin < 100000; spin++)
        if (ADC_ISR & ADC_ISR_ADRDY)
            goto ready;
    return 0xffffffffU;

ready:
    ADC_CR |= ADC_CR_ADSTART;
    for (uint32_t spin = 0; spin < 100000; spin++)
        if (ADC_ISR & ADC_ISR_EOC)
            return ADC_DR;
    return 0xffffffffU;
}

int main(void) {
    clock_init();
    gpio_init();
    uart_init();

    uart_puts("hello from stm32f072\r\n");

    uint32_t ticks = 0;
    for (;;) {
        GPIO_ODR(GPIOC_BASE) ^= (1U << LED_PIN);
        ticks++;

        uint8_t c;
        while (uart_poll(&c)) {
            if (c == 'v') {
                print_hex32(ticks);
                uart_puts("\r\n");
            } else if (c == 'i') {
                uart_puts("hello from stm32f072\r\n");
            } else if (c == 'a' || c == 'b') {
                /* 'a' reads ADC_IN0 (PA0), 'b' reads ADC_IN3 (PA3), so a test
                 * can prove the channel argument selects a channel rather than
                 * every read returning one shared sample. */
                uint32_t ch = (c == 'a') ? 0u : 3u;
                uart_puts("adc");
                uart_tx((uint8_t)('0' + ch));
                uart_tx('=');
                print_hex32(adc_read(ch));
                uart_puts("\r\n");
            } else {
                uart_tx(c);
            }
        }

        delay_loop(200000);
    }
}

#endif /* QUIET */
