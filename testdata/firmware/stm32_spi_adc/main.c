/* Bare-metal STM32F103 SPI ADC firmware for the hauksbee Renode SPI co-sim.
 *
 * Exercises the SPI coupling path:
 *   - SPI1 master: reads MCP3008 channel 0 via hardware SPI (PA5=SCK, PA6=MISO,
 *     PA7=MOSI, PA4=NSS driven manually as a GPIO).
 *   - Threshold: if the ADC reading >= 512 (half of 1023 full-scale, i.e.
 *     >= Vref/2 = 1.65 V with a 3.3 V reference), drive PA8 HIGH; else LOW.
 *   - PA8 -> net "FLAG": the observable net the co-sim test reads.
 *   - UART: USART1 at 115200 sends "spi adc ready\r\n" at boot and
 *     "adc:<decimal>\r\n" after each conversion loop.
 *
 * MCP3008 3-byte SPI protocol (SPI mode 0,0, MSB first):
 *   byte0 = 0x01 (start bit)
 *   byte1 = 0x80 (single-ended, channel 0: SGL=1, D2..D0=000)
 *   byte2 = 0x00 (clocks out the low 8 bits)
 * Reply (MISO):
 *   byte0: don't-care
 *   byte1: bits[1:0] = ADC bits[9:8]
 *   byte2: ADC bits[7:0]
 *
 * No vendor SDK: direct register access only.
 */

#include <stdint.h>

/* Peripheral base addresses */
#define RCC_BASE    0x40021000UL
#define GPIOA_BASE  0x40010800UL
#define USART1_BASE 0x40013800UL
#define SPI1_BASE   0x40013000UL

#define REG(addr) (*(volatile uint32_t *)(addr))

/* RCC */
#define RCC_APB2ENR REG(RCC_BASE + 0x18)
#define RCC_APB2ENR_IOPAEN   (1U << 2)
#define RCC_APB2ENR_USART1EN (1U << 14)
#define RCC_APB2ENR_SPI1EN   (1U << 12)

/* GPIO */
#define GPIO_CRL(base)  REG((base) + 0x00)
#define GPIO_CRH(base)  REG((base) + 0x04)
#define GPIO_ODR(base)  REG((base) + 0x0C)
#define GPIO_BSRR(base) REG((base) + 0x10)
#define GPIO_BRR(base)  REG((base) + 0x14)

/* USART */
#define USART_SR(base)  REG((base) + 0x00)
#define USART_DR(base)  REG((base) + 0x04)
#define USART_BRR(base) REG((base) + 0x08)
#define USART_CR1(base) REG((base) + 0x0C)
#define USART_SR_TXE  (1U << 7)
#define USART_CR1_TE  (1U << 3)
#define USART_CR1_UE  (1U << 13)

/* SPI */
#define SPI_CR1(base)  REG((base) + 0x00)
#define SPI_SR(base)   REG((base) + 0x08)
#define SPI_DR(base)   REG((base) + 0x0C)
#define SPI_SR_TXE  (1U << 1)
#define SPI_SR_RXNE (1U << 0)
#define SPI_SR_BSY  (1U << 7)
#define SPI_CR1_MSTR  (1U << 2)
#define SPI_CR1_SPE   (1U << 6)
#define SPI_CR1_SSI   (1U << 8)
#define SPI_CR1_SSM   (1U << 9)
/* BR[2:0] = 0b011 -> fPCLK/16 (plenty for MCP3008 at 8 MHz PCLK2) */
#define SPI_CR1_BR_DIV16 (3U << 3)

/* Pin config nibbles for CRL/CRH */
#define CNF_OUTPUT_PP_50MHZ 0x3 /* general-purpose push-pull 50 MHz */
#define CNF_AF_PP_50MHZ     0xB /* alternate-function push-pull 50 MHz */
#define CNF_INPUT_FLOATING  0x4 /* floating input */

/* ADC conversion threshold: counts >= 512 -> FLAG HIGH */
#define ADC_THRESHOLD 512U

/* NSS = PA4, driven manually (software NSS). */
#define NSS_LOW()  GPIO_BRR(GPIOA_BASE)  = (1U << 4)
#define NSS_HIGH() GPIO_BSRR(GPIOA_BASE) = (1U << 4)

static void clock_init(void)
{
    RCC_APB2ENR |= RCC_APB2ENR_IOPAEN | RCC_APB2ENR_USART1EN | RCC_APB2ENR_SPI1EN;
}

static void gpio_init(void)
{
    uint32_t crl = GPIO_CRL(GPIOA_BASE);

    /* PA4: NSS (software, output push-pull) */
    crl &= ~(0xFU << (4 * 4));
    crl |=  (CNF_OUTPUT_PP_50MHZ << (4 * 4));

    /* PA5: SCK (alternate function push-pull) */
    crl &= ~(0xFU << (5 * 4));
    crl |=  (CNF_AF_PP_50MHZ << (5 * 4));

    /* PA6: MISO (floating input) */
    crl &= ~(0xFU << (6 * 4));
    crl |=  (CNF_INPUT_FLOATING << (6 * 4));

    /* PA7: MOSI (alternate function push-pull) */
    crl &= ~(0xFU << (7 * 4));
    crl |=  (CNF_AF_PP_50MHZ << (7 * 4));

    GPIO_CRL(GPIOA_BASE) = crl;

    /* PA8: FLAG (output push-pull, in CRH) */
    uint32_t crh = GPIO_CRH(GPIOA_BASE);
    crh &= ~(0xFU << ((8 - 8) * 4));
    crh |=  (CNF_OUTPUT_PP_50MHZ << ((8 - 8) * 4));

    /* PA9: USART1 TX (alternate function push-pull, in CRH) */
    crh &= ~(0xFU << ((9 - 8) * 4));
    crh |=  (CNF_AF_PP_50MHZ << ((9 - 8) * 4));

    GPIO_CRH(GPIOA_BASE) = crh;

    /* Deassert NSS at startup */
    NSS_HIGH();
    /* FLAG starts LOW */
    GPIO_BRR(GPIOA_BASE) = (1U << 8);
}

static void spi_init(void)
{
    /* SPI1 master, software NSS, SPI mode 0 (CPOL=0 CPHA=0), fPCLK/16, 8-bit */
    SPI_CR1(SPI1_BASE) = SPI_CR1_MSTR | SPI_CR1_SSM | SPI_CR1_SSI
                       | SPI_CR1_BR_DIV16 | SPI_CR1_SPE;
}

static void uart_init(void)
{
    /* 8 MHz HSI PCLK2 / 115200 ~= 69.4 -> BRR = 0x45 */
    USART_BRR(USART1_BASE) = 0x45;
    USART_CR1(USART1_BASE) = USART_CR1_TE | USART_CR1_UE;
}

static void uart_tx(uint8_t c)
{
    while (!(USART_SR(USART1_BASE) & USART_SR_TXE))
        ;
    USART_DR(USART1_BASE) = c;
}

static void uart_puts(const char *s)
{
    while (*s)
        uart_tx((uint8_t)*s++);
}

static void uart_print_u32(uint32_t v)
{
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

/* Transfer one byte over SPI1 (blocking). */
static uint8_t spi_byte(uint8_t out)
{
    while (!(SPI_SR(SPI1_BASE) & SPI_SR_TXE))
        ;
    SPI_DR(SPI1_BASE) = out;
    while (!(SPI_SR(SPI1_BASE) & SPI_SR_RXNE))
        ;
    return (uint8_t)(SPI_DR(SPI1_BASE) & 0xFF);
}

/* Read MCP3008 channel 0 (single-ended). Returns 10-bit count (0..1023). */
static uint16_t mcp3008_read_ch0(void)
{
    NSS_LOW();

    /* Three-byte transfer: start, config, result clock */
    (void)spi_byte(0x01);           /* start bit */
    uint8_t hi = spi_byte(0x80);    /* SGL=1, CH2..CH0=000 */
    uint8_t lo = spi_byte(0x00);    /* clock out low 8 bits */

    /* Drain BSY */
    while (SPI_SR(SPI1_BASE) & SPI_SR_BSY)
        ;

    NSS_HIGH();

    return (uint16_t)(((hi & 0x03U) << 8) | lo);
}

static void delay_loop(volatile uint32_t n)
{
    while (n--)
        __asm__ volatile("nop");
}

int main(void)
{
    clock_init();
    gpio_init();
    spi_init();
    uart_init();

    uart_puts("spi adc ready\r\n");

    for (;;) {
        uint16_t counts = mcp3008_read_ch0();

        uart_puts("adc:");
        uart_print_u32(counts);
        uart_puts("\r\n");

        /* Drive FLAG (PA8) based on threshold */
        if (counts >= ADC_THRESHOLD) {
            GPIO_BSRR(GPIOA_BASE) = (1U << 8); /* set PA8 HIGH */
        } else {
            GPIO_BRR(GPIOA_BASE) = (1U << 8);  /* set PA8 LOW */
        }

        /* ~10 ms between conversions in virtual time */
        delay_loop(330000);
    }
}
