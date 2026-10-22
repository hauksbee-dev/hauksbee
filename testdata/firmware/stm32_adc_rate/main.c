/* Tiny STM32F103 ADC sampling probe for the hauksbee co-sim fault-injection benchmark.
 *
 * Samples ADC1 channel 0 (PA0) at SAMPLE_RATE_HZ, paced by SysTick, with a bounded wait for each conversion.
 * Every 10000 samples it toggles the PC13 heartbeat, so a bench engineer's scope on PC13 shows
 * SAMPLE_RATE_HZ / 20000 Hz (5 Hz at the 100 kHz default). It prints one banner on USART1 (PA9) at 115200 8N1.
 *
 * The chip cannot sample faster than ADC_MAX_SPS: at the reset-default 8 MHz HSI, PCLK2/2 = 4 MHz clocks the ADC,
 * and 1.5 + 12.5 = 14 cycles per conversion gives 4e6 / 14 = 285 kSPS (the part's own ceiling is 1 MSPS at a
 * 14 MHz ADCCLK, RM0008 / DS5319 Table 55). A SAMPLE_RATE_HZ above that period cannot be met: the loop runs
 * conversion-bound and the heartbeat is slower than the define promises.
 *
 * Direct register access only; builds with a bare arm-none-eabi-gcc (see the Makefile).
 */

#include <stdint.h>

#define REG(addr) (*(volatile uint32_t *)(addr))

#define RCC_BASE     0x40021000UL
#define GPIOA_BASE   0x40010800UL
#define GPIOC_BASE   0x40011000UL
#define USART1_BASE  0x40013800UL
#define ADC1_BASE    0x40012400UL

#define RCC_APB2ENR  REG(RCC_BASE + 0x18)
#define GPIO_CRL(b)  REG((b) + 0x00)
#define GPIO_CRH(b)  REG((b) + 0x04)
#define GPIO_ODR(b)  REG((b) + 0x0C)
#define USART_SR(b)  REG((b) + 0x00)
#define USART_DR(b)  REG((b) + 0x04)
#define USART_BRR(b) REG((b) + 0x08)
#define USART_CR1(b) REG((b) + 0x0C)
#define ADC_SR(b)    REG((b) + 0x00)
#define ADC_CR2(b)   REG((b) + 0x08)
#define ADC_SMPR2(b) REG((b) + 0x10)
#define ADC_SQR3(b)  REG((b) + 0x34)
#define ADC_DR(b)    REG((b) + 0x4C)

#define SYSCLK_HZ    8000000UL
#define ADC_MAX_SPS  285000UL   /* 4 MHz ADCCLK / 14 cycles at the 8 MHz reset clock */

/* The sampling rate this firmware asks for. */
#define SAMPLE_RATE_HZ 100000UL

#define STK_CTRL REG(0xE000E010UL)
#define STK_LOAD REG(0xE000E014UL)
#define STK_VAL  REG(0xE000E018UL)

static void uart_tx(uint8_t c) {
    while (!(USART_SR(USART1_BASE) & (1U << 7)))
        ;
    USART_DR(USART1_BASE) = c;
}

static void uart_puts(const char *s) {
    while (*s)
        uart_tx((uint8_t)*s++);
}

static void init(void) {
    RCC_APB2ENR |= (1U << 2) | (1U << 4) | (1U << 9) | (1U << 14); /* IOPA, IOPC, ADC1, USART1 */

    uint32_t crh = GPIO_CRH(GPIOC_BASE);
    crh &= ~(0xFU << ((13 - 8) * 4));
    crh |= (0x3U << ((13 - 8) * 4)); /* PC13 push-pull output */
    GPIO_CRH(GPIOC_BASE) = crh;

    uint32_t crh_a = GPIO_CRH(GPIOA_BASE);
    crh_a &= ~(0xFU << ((9 - 8) * 4));
    crh_a |= (0xBU << ((9 - 8) * 4)); /* PA9 USART1 TX, alternate push-pull */
    GPIO_CRH(GPIOA_BASE) = crh_a;

    /* PA0 stays analog input (reset default CNF=00 MODE=00). */
    USART_BRR(USART1_BASE) = 0x45; /* 8 MHz / 115200 */
    USART_CR1(USART1_BASE) = (1U << 3) | (1U << 13);

    ADC_SMPR2(ADC1_BASE) = 0; /* 1.5-cycle sample time on channel 0 */
    ADC_SQR3(ADC1_BASE) = 0;  /* sequence: channel 0 only */
    ADC_CR2(ADC1_BASE) = 1U;  /* ADON */
    ADC_CR2(ADC1_BASE) |= (7U << 17) | (1U << 20); /* EXTSEL = SWSTART, EXTTRIG */

    STK_LOAD = (SYSCLK_HZ / SAMPLE_RATE_HZ) - 1UL;
    STK_VAL = 0;
    STK_CTRL = (1U << 2) | (1U << 0);
}

int main(void) {
    init();
    uart_puts("adc rate probe\r\n");

    uint32_t n = 0;
    uint32_t last = 0;
    for (;;) {
        ADC_CR2(ADC1_BASE) |= (1U << 22); /* SWSTART */
        uint32_t guard = 40; /* a real conversion needs ~28 core cycles, a few loop passes */
        while (!(ADC_SR(ADC1_BASE) & (1U << 1)) && --guard) /* EOC, bounded */
            ;
        last = ADC_DR(ADC1_BASE);
        (void)last;
        if (++n % 10000U == 0U)
            GPIO_ODR(GPIOC_BASE) ^= (1U << 13);
        while (!(STK_CTRL & (1U << 16))) /* pace to SAMPLE_RATE_HZ */
            ;
    }
}
