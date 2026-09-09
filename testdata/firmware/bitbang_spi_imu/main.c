/* Bit-banged SPI read fixture for the synchronous input-responder proof
 * (dev-plan 05 §1.5).
 *
 * Reads a spec-driven ICM-42605 IMU over
 * SOFTWARE SPI: SCLK/MOSI/CS toggled as plain GPIOs and MISO sampled with a
 * plain PIND read — deliberately NOT the ATmega328P's hardware SPI pins
 * (PB2..PB5), so nothing but the engine's bit-banged SPI responder can answer.
 * The read closes inside the firmware's own clock loop: each PIND sample lands
 * one instruction after the SCLK rising edge, which only works if the modeled
 * slave's MISO bit was pushed into the input register synchronously.
 *
 * Wiring (DIP-28 numbering, see the co-sim test's board):
 *   PD4 = CS_n   (pad 6)
 *   PD5 = SCLK   (pad 11)
 *   PD6 = MOSI   (pad 12)
 *   PD7 = MISO   (pad 13, input)
 *
 * SPI mode 0, MSB first — the responder's stated subset. Each loop:
 *   1. WHO_AM_I (0x75) single read           -> expect 0x42
 *   2. GYRO_CONFIG1 (0x4F) burst read of two -> expect 0x06 0x06
 *      (the second byte exercises the register auto-increment hop to 0x50)
 * and reports "W<who>G<b0><b1>\n" in hex over UART.
 *
 * ATmega328P @ 16 MHz, UART 9600 8N1.
 */
#include <avr/io.h>
#include <util/delay.h>

#define BAUD 9600
#define F_CPU_HZ 16000000UL

#define CS_BIT PD4
#define SCLK_BIT PD5
#define MOSI_BIT PD6
#define MISO_BIT PD7

/* ---- UART (report) ---- */
static void uart_init(void) {
    uint16_t ubrr = (F_CPU_HZ / (16UL * BAUD)) - 1;
    UBRR0H = (uint8_t)(ubrr >> 8);
    UBRR0L = (uint8_t)ubrr;
    UCSR0B = _BV(TXEN0);
    UCSR0C = _BV(UCSZ01) | _BV(UCSZ00);
}
static void uart_tx(uint8_t c) {
    while (!(UCSR0A & _BV(UDRE0)))
        ;
    UDR0 = c;
}
static void print_hex8(uint8_t v) {
    static const char hex[] = "0123456789ABCDEF";
    uart_tx(hex[v >> 4]);
    uart_tx(hex[v & 0x0F]);
}

/* ---- Software SPI, mode 0, MSB first ---- */
static uint8_t spi_xfer(uint8_t out) {
    uint8_t in = 0;
    for (int8_t i = 7; i >= 0; i--) {
        if ((out >> i) & 1)
            PORTD |= _BV(MOSI_BIT);
        else
            PORTD &= ~_BV(MOSI_BIT);
        PORTD |= _BV(SCLK_BIT); /* rising edge: slave samples MOSI */
        /* Sample MISO immediately after the rising edge — the bit the modeled
         * slave presented on the PREVIOUS falling edge (or at CS assert). */
        in = (uint8_t)((in << 1) | ((PIND >> MISO_BIT) & 1));
        PORTD &= ~_BV(SCLK_BIT); /* falling edge: slave shifts the next bit */
    }
    return in;
}

static void cs_low(void) { PORTD &= ~_BV(CS_BIT); }
static void cs_high(void) { PORTD |= _BV(CS_BIT); }

int main(void) {
    uart_init();

    DDRD |= _BV(CS_BIT) | _BV(SCLK_BIT) | _BV(MOSI_BIT);
    DDRD &= ~_BV(MISO_BIT);
    cs_high();               /* CS_n idles high */
    PORTD &= ~_BV(SCLK_BIT); /* mode 0: clock idles low */

    for (;;) {
        /* WHO_AM_I: read bit (0x80) | register 0x75. */
        cs_low();
        spi_xfer(0x80 | 0x75);
        uint8_t who = spi_xfer(0x00);
        cs_high();

        /* GYRO_CONFIG1 (0x4F) burst of two: byte 2 auto-increments to 0x50. */
        cs_low();
        spi_xfer(0x80 | 0x4F);
        uint8_t g0 = spi_xfer(0x00);
        uint8_t g1 = spi_xfer(0x00);
        cs_high();

        uart_tx('W');
        print_hex8(who);
        uart_tx('G');
        print_hex8(g0);
        print_hex8(g1);
        uart_tx('\n');
        _delay_ms(5);
    }
}
