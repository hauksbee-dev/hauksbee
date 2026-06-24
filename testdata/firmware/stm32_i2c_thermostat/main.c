/* I2C thermostat firmware for the hauksbee Renode co-sim peripheral proof.
 *
 * Mirrors the AVR i2c_thermostat firmware contract on an STM32F103:
 *   - Reads the LM75 temperature register (I2C address 0x48) over the STM32F103
 *     hardware I2C1 peripheral (PB6=SCL, PB7=SDA).
 *   - Drives PA8 HIGH when T >= 30 C, LOW otherwise (the "FLAG" output).
 *   - Prints the integer temperature over USART1 at 115200 8N1 for debug.
 *
 * Also mirrors the GPIO mailbox from the ESP32 blinky firmware so the hauksbee
 * QEMU/Renode backend can observe the pin state without peripheral register
 * read-back issues. The mailbox sits in SRAM at a known symbol.
 *
 * No vendor SDK: direct register access only. Builds with arm-none-eabi-gcc and
 * the same minimal linker script + startup used by stm32_blinky.
 *
 * STM32F103C8T6 at 8 MHz HSI (no PLL). I2C1 at ~100 kHz.
 */

#include <stdint.h>

/* ---- Peripheral base addresses ---- */
#define RCC_BASE     0x40021000UL
#define GPIOA_BASE   0x40010800UL
#define GPIOB_BASE   0x40010C00UL
#define I2C1_BASE    0x40005400UL
#define USART1_BASE  0x40013800UL

#define REG(addr) (*(volatile uint32_t *)(addr))

/* RCC */
#define RCC_APB1ENR  REG(RCC_BASE + 0x1C)
#define RCC_APB2ENR  REG(RCC_BASE + 0x18)
#define RCC_APB1ENR_I2C1EN    (1U << 21)
#define RCC_APB2ENR_IOPAEN    (1U << 2)
#define RCC_APB2ENR_IOPBEN    (1U << 3)
#define RCC_APB2ENR_USART1EN  (1U << 14)

/* GPIO registers */
#define GPIO_CRL(base)  REG((base) + 0x00)
#define GPIO_CRH(base)  REG((base) + 0x04)
#define GPIO_IDR(base)  REG((base) + 0x08)
#define GPIO_ODR(base)  REG((base) + 0x0C)
#define GPIO_BSRR(base) REG((base) + 0x10)
#define GPIO_BRR(base)  REG((base) + 0x14)

/* Pin config nibble values */
#define CNF_OUTPUT_PP_50MHZ  0x3U  /* push-pull output, 50 MHz */
#define CNF_OUTPUT_OD_50MHZ  0x7U  /* open-drain output, 50 MHz (I2C) */
#define CNF_AF_OD_50MHZ      0xFU  /* alternate function open-drain, 50 MHz */
#define CNF_INPUT_FLOATING   0x4U  /* floating input */
#define CNF_AF_PP_50MHZ      0xBU  /* alternate function push-pull, 50 MHz */

/* I2C registers */
#define I2C_CR1(base)   REG((base) + 0x00)
#define I2C_CR2(base)   REG((base) + 0x04)
#define I2C_OAR1(base)  REG((base) + 0x08)
#define I2C_DR(base)    REG((base) + 0x10)
#define I2C_SR1(base)   REG((base) + 0x14)
#define I2C_SR2(base)   REG((base) + 0x18)
#define I2C_CCR(base)   REG((base) + 0x1C)
#define I2C_TRISE(base) REG((base) + 0x20)

/* I2C_CR1 bits */
#define I2C_CR1_PE      (1U << 0)   /* peripheral enable */
#define I2C_CR1_START   (1U << 8)   /* generate START */
#define I2C_CR1_STOP    (1U << 9)   /* generate STOP */
#define I2C_CR1_ACK     (1U << 10)  /* ACK enable */
#define I2C_CR1_SWRST   (1U << 15)  /* software reset */

/* I2C_SR1 bits */
#define I2C_SR1_SB      (1U << 0)   /* start bit sent */
#define I2C_SR1_ADDR    (1U << 1)   /* address sent/matched */
#define I2C_SR1_BTF     (1U << 2)   /* byte transfer finished */
#define I2C_SR1_RXNE    (1U << 6)   /* RX not empty */
#define I2C_SR1_TXE     (1U << 7)   /* TX empty */
#define I2C_SR1_AF      (1U << 10)  /* ACK failure */
#define I2C_SR1_BUSY    (1U << 1)   /* from SR2 */

/* USART */
#define USART_SR(base)   REG((base) + 0x00)
#define USART_DR(base)   REG((base) + 0x04)
#define USART_BRR(base)  REG((base) + 0x08)
#define USART_CR1(base)  REG((base) + 0x0C)
#define USART_SR_TXE     (1U << 7)
#define USART_CR1_TE     (1U << 3)
#define USART_CR1_UE     (1U << 13)

/* LM75 I2C address */
#define LM75_ADDR 0x48U
/* Over-temperature threshold (Celsius, integer comparison) */
#define THRESHOLD_C 30

/* GPIO observation mailbox (mirrors ESP32 blinky pattern).
 * Sits at a fixed SRAM symbol so the Renode backend can read it.
 * Layout: [0] = output pin-state word (bit 8 = PA8 level). */
volatile uint32_t hauksbee_gpio_out __attribute__((section(".bss"))) = 0;
volatile uint32_t hauksbee_magic    __attribute__((section(".bss"))) = 0;
#define HAUKSBEE_MAGIC_VALUE 0x6A6C6E69UL

/* ---- Crude busy-wait timeout counter ---- */
#define TIMEOUT_LIMIT 100000U

/* ---- Clock init ---- */
static void clock_init(void) {
    RCC_APB2ENR |= RCC_APB2ENR_IOPAEN | RCC_APB2ENR_IOPBEN | RCC_APB2ENR_USART1EN;
    RCC_APB1ENR |= RCC_APB1ENR_I2C1EN;
}

/* ---- GPIO init ---- */
static void gpio_init(void) {
    /* PA8: output push-pull (FLAG net - over-temp indicator). In CRH (pin 8). */
    uint32_t crh_a = GPIO_CRH(GPIOA_BASE);
    crh_a &= ~(0xFU << ((8 - 8) * 4));
    crh_a |= (CNF_OUTPUT_PP_50MHZ << ((8 - 8) * 4));
    GPIO_CRH(GPIOA_BASE) = crh_a;
    /* Start LOW */
    GPIO_BRR(GPIOA_BASE) = (1U << 8);

    /* PA9: USART1 TX -> AF push-pull. PA10: USART1 RX -> floating input. */
    uint32_t crh_a2 = GPIO_CRH(GPIOA_BASE);
    crh_a2 &= ~(0xFU << ((9 - 8) * 4));
    crh_a2 |= (CNF_AF_PP_50MHZ << ((9 - 8) * 4));
    crh_a2 &= ~(0xFU << ((10 - 8) * 4));
    crh_a2 |= (CNF_INPUT_FLOATING << ((10 - 8) * 4));
    GPIO_CRH(GPIOA_BASE) = crh_a2;

    /* PB6: I2C1_SCL, PB7: I2C1_SDA -> AF open-drain. In CRL. */
    uint32_t crl_b = GPIO_CRL(GPIOB_BASE);
    crl_b &= ~(0xFU << (6 * 4));
    crl_b |= (CNF_AF_OD_50MHZ << (6 * 4));
    crl_b &= ~(0xFU << (7 * 4));
    crl_b |= (CNF_AF_OD_50MHZ << (7 * 4));
    GPIO_CRL(GPIOB_BASE) = crl_b;
    /* Set PB6/PB7 high (open-drain idle state) */
    GPIO_BSRR(GPIOB_BASE) = (1U << 6) | (1U << 7);
}

/* ---- USART1 init ---- */
static void uart_init(void) {
    /* 8 MHz HSI PCLK2 / 115200 ~= 69.4 -> BRR mantissa 4, frac 5 = 0x45 */
    USART_BRR(USART1_BASE) = 0x45;
    USART_CR1(USART1_BASE) = USART_CR1_TE | USART_CR1_UE;
}

static void uart_tx(uint8_t c) {
    while (!(USART_SR(USART1_BASE) & USART_SR_TXE))
        ;
    USART_DR(USART1_BASE) = c;
}

static void uart_puts(const char *s) {
    while (*s) uart_tx((uint8_t)*s++);
}

static void print_i32(int32_t v) {
    char buf[12];
    int8_t i = 0;
    uint32_t u;
    if (v < 0) { uart_tx('-'); u = (uint32_t)(-v); }
    else { u = (uint32_t)v; }
    if (u == 0) { uart_tx('0'); return; }
    while (u) { buf[i++] = (char)('0' + (u % 10)); u /= 10; }
    while (i) uart_tx((uint8_t)buf[--i]);
}

/* ---- I2C1 init at ~100 kHz with 8 MHz PCLK1 ---- */
static void i2c_init(void) {
    /* Software reset to clear any stuck state */
    I2C_CR1(I2C1_BASE) = I2C_CR1_SWRST;
    I2C_CR1(I2C1_BASE) = 0;

    /* FREQ = PCLK1 in MHz = 8 */
    I2C_CR2(I2C1_BASE) = 8U;

    /* CCR for 100 kHz Sm: CCR = PCLK1 / (2 * 100 kHz) = 8e6 / 200e3 = 40 */
    I2C_CCR(I2C1_BASE) = 40U;

    /* TRISE = (1000 ns / 125 ns) + 1 = 9 (for 8 MHz) */
    I2C_TRISE(I2C1_BASE) = 9U;

    /* Own address 0 (master-only), 7-bit addressing */
    I2C_OAR1(I2C1_BASE) = 0x4000U; /* bit 14 must be 1 per datasheet */

    /* Enable I2C1 */
    I2C_CR1(I2C1_BASE) = I2C_CR1_PE;
}

/* Wait for SR1 bit with timeout; returns 0 on timeout */
static int i2c_wait(uint32_t bit) {
    uint32_t t = TIMEOUT_LIMIT;
    while (!(I2C_SR1(I2C1_BASE) & bit)) {
        if (!--t) return 0;
    }
    return 1;
}

/* Generate START; returns 0 on failure */
static int i2c_start(void) {
    /* Enable ACK, generate START */
    I2C_CR1(I2C1_BASE) |= I2C_CR1_ACK | I2C_CR1_START;
    return i2c_wait(I2C_SR1_SB);
}

/* Send 7-bit address + R/W bit; returns 0 on failure or NACK */
static int i2c_addr(uint8_t addr7, int read) {
    I2C_DR(I2C1_BASE) = (uint32_t)((addr7 << 1) | (read ? 1U : 0U));
    if (!i2c_wait(I2C_SR1_ADDR)) return 0;
    if (I2C_SR1(I2C1_BASE) & I2C_SR1_AF) {
        I2C_SR1(I2C1_BASE) &= ~I2C_SR1_AF; /* clear ACK fail */
        return 0;
    }
    /* Clear ADDR by reading SR1 then SR2 */
    (void)I2C_SR1(I2C1_BASE);
    (void)I2C_SR2(I2C1_BASE);
    return 1;
}

/* Write one data byte; returns 0 on failure */
static int i2c_write_byte(uint8_t data) {
    if (!i2c_wait(I2C_SR1_TXE)) return 0;
    I2C_DR(I2C1_BASE) = data;
    return i2c_wait(I2C_SR1_BTF);
}

/* Generate STOP */
static void i2c_stop(void) {
    I2C_CR1(I2C1_BASE) |= I2C_CR1_STOP;
    /* Brief busy-wait for bus to return to idle */
    uint32_t t = TIMEOUT_LIMIT;
    while (I2C_SR2(I2C1_BASE) & (1U << 1) /* BUSY */ && --t)
        ;
}

/* Read two bytes from I2C slave (for LM75 temperature register).
 * Uses the "2-byte receive" sequence from the STM32F10x reference manual
 * (RM0008 section 26.3.3 "Master receiver"): disable ACK before reading the
 * second-to-last byte so the peripheral NACKs automatically after the 2nd byte.
 */
static int i2c_read2(uint8_t *msb, uint8_t *lsb) {
    /* Prepare: set ACK, then set POS (NACK the second byte automatically) */
    I2C_CR1(I2C1_BASE) |= I2C_CR1_ACK;
    /* For 2-byte receive: clear ACK before ADDR is cleared */
    /* See RM0008 errata / application note AN2824 for exact sequence */
    /* Simplified approach: wait for RXNE after each byte, clear ACK before 2nd */
    uint32_t t = TIMEOUT_LIMIT;
    while (!(I2C_SR1(I2C1_BASE) & (1U << 6 /* RXNE */)) && --t)
        ;
    if (!t) return 0;
    /* Before reading MSB byte, disable ACK so NACK is sent after next byte */
    I2C_CR1(I2C1_BASE) &= ~I2C_CR1_ACK;
    /* Generate STOP after next byte */
    I2C_CR1(I2C1_BASE) |= I2C_CR1_STOP;
    *msb = (uint8_t)I2C_DR(I2C1_BASE);

    /* Wait for LSB */
    t = TIMEOUT_LIMIT;
    while (!(I2C_SR1(I2C1_BASE) & (1U << 6 /* RXNE */)) && --t)
        ;
    if (!t) return 0;
    *lsb = (uint8_t)I2C_DR(I2C1_BASE);
    return 1;
}

/* Read the LM75 temperature register (16-bit big-endian). Returns raw value. */
static int lm75_read(int16_t *raw_out) {
    /* Phase 1: write pointer register 0x00 (temperature) */
    if (!i2c_start()) return 0;
    if (!i2c_addr(LM75_ADDR, 0)) { i2c_stop(); return 0; }
    if (!i2c_write_byte(0x00)) { i2c_stop(); return 0; }
    i2c_stop();

    /* Phase 2: read 2 bytes */
    if (!i2c_start()) return 0;
    if (!i2c_addr(LM75_ADDR, 1)) { i2c_stop(); return 0; }

    uint8_t msb = 0, lsb = 0;
    if (!i2c_read2(&msb, &lsb)) { i2c_stop(); return 0; }
    /* STOP was already generated inside i2c_read2 */

    *raw_out = (int16_t)(((uint16_t)msb << 8) | lsb);
    return 1;
}

static void flag_set(int high) {
    if (high) {
        GPIO_BSRR(GPIOA_BASE) = (1U << 8);
        hauksbee_gpio_out |= (1U << 8);
    } else {
        GPIO_BRR(GPIOA_BASE) = (1U << 8);
        hauksbee_gpio_out &= ~(1U << 8);
    }
}

int main(void) {
    clock_init();
    gpio_init();
    uart_init();
    i2c_init();

    /* Publish mailbox magic so the backend knows this firmware mirrors GPIO. */
    hauksbee_magic = HAUKSBEE_MAGIC_VALUE;
    hauksbee_gpio_out = 0;

    uart_puts("stm32 i2c thermostat ready\r\n");

    for (;;) {
        int16_t raw = 0;
        if (lm75_read(&raw)) {
            /* LM75A: raw is 11-bit left-justified in 16 bits, 0.125 C/LSB.
             * Integer degrees C = raw >> 8  (drops fractional bits).
             * Positive and negative values are both correct with this shift
             * because the value is sign-extended. */
            int32_t temp_c = (int32_t)(raw >> 8);

            if (temp_c >= THRESHOLD_C) {
                flag_set(1);
            } else {
                flag_set(0);
            }

            print_i32(temp_c);
            uart_puts("C\r\n");
        } else {
            /* I2C read failed: keep previous flag state, print error */
            uart_puts("i2c_err\r\n");
        }

        /* Small delay between polls: ~1 ms of virtual time at 8 MHz HSI.
         * The Renode STM32F103 model runs at ~MIPS-class virtual time;
         * this loop gives the engine a chance to complete the analog chunk
         * and re-enter the MCU run before the next I2C transaction. */
        volatile uint32_t d = 8000;
        while (d--) __asm__ volatile("nop");
    }
}
