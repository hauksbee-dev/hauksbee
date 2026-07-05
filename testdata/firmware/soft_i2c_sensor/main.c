/* Soft-I2C read fixture for the synchronous input-responder proof
 * (dev-plan 05 §1.5).
 *
 * Reads a spec-driven MPU-6050 (docs/hunts/specs/mpu6050.toml, address 0x68)
 * over BIT-BANGED I2C: SCL/SDA toggled as plain GPIOs on PD2/PD3 —
 * deliberately NOT the ATmega328P's hardware TWI pins (PC4/PC5), so nothing
 * but the engine's soft-I2C responder can answer. Every ACK and every read
 * bit is sampled with a plain PIND read one instruction after the SCL rising
 * edge, which only works if the modeled slave's SDA level was pushed into the
 * input register synchronously.
 *
 * Master waveform: PUSH-PULL (the responder's stated subset — see
 * SoftI2cResponder). SDA is driven high/low as an output for address and
 * write bits, and switched to an input for the slave's ACK bits and read
 * bytes. DDR-only open-drain emulation would produce no PORT edges and is
 * documented as unsupported.
 *
 * Each loop performs the two classic pointered reads:
 *   1. WHO_AM_I (0x75), single byte, repeated-START framing -> expect 0x68
 *   2. TEMP_OUT (0x41), two-byte i16_be burst with a master ACK between
 * and reports "<A|n>W<who>T<hi><lo>\n" in hex over UART, where 'A' means all
 * address/pointer bytes were ACKed.
 *
 * ATmega328P @ 16 MHz, UART 9600 8N1.
 */
#include <avr/io.h>
#include <util/delay.h>

#define BAUD 9600
#define F_CPU_HZ 16000000UL

#define SCL_BIT PD2
#define SDA_BIT PD3

#define MPU_ADDR 0x68

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

/* ---- Soft I2C master, push-pull ---- */
static void sda_out(void) { DDRD |= _BV(SDA_BIT); }
static void sda_in(void) { DDRD &= ~_BV(SDA_BIT); }
static void sda_hi(void) { PORTD |= _BV(SDA_BIT); }
static void sda_lo(void) { PORTD &= ~_BV(SDA_BIT); }
static void scl_hi(void) { PORTD |= _BV(SCL_BIT); }
static void scl_lo(void) { PORTD &= ~_BV(SCL_BIT); }

static void i2c_start(void) {
    /* SDA falls while SCL is high. */
    sda_out();
    sda_hi();
    scl_hi();
    sda_lo();
    scl_lo();
}

static void i2c_stop(void) {
    /* SDA rises while SCL is high. */
    sda_out();
    sda_lo();
    scl_hi();
    sda_hi();
}

/* Write one byte, MSB first; returns 1 if the slave ACKed. */
static uint8_t i2c_write(uint8_t b) {
    sda_out();
    for (int8_t i = 7; i >= 0; i--) {
        if ((b >> i) & 1)
            sda_hi();
        else
            sda_lo();
        scl_hi();
        scl_lo();
    }
    /* ACK clock: release SDA to the slave, sample while SCL is high. */
    sda_in();
    scl_hi();
    uint8_t ack = !((PIND >> SDA_BIT) & 1);
    scl_lo();
    return ack;
}

/* Read one byte, MSB first, then send master ACK (more) / NACK (last). */
static uint8_t i2c_read(uint8_t ack) {
    uint8_t b = 0;
    sda_in();
    for (int8_t i = 7; i >= 0; i--) {
        scl_hi();
        b = (uint8_t)((b << 1) | ((PIND >> SDA_BIT) & 1));
        scl_lo();
    }
    sda_out();
    if (ack)
        sda_lo();
    else
        sda_hi();
    scl_hi();
    scl_lo();
    sda_in();
    return b;
}

/* Pointered register read with repeated-START framing. Returns 1 when every
 * address/pointer byte ACKed; the data lands in buf[0..n). */
static uint8_t mpu_read(uint8_t reg, uint8_t *buf, uint8_t n) {
    uint8_t ok = 1;
    i2c_start();
    ok &= i2c_write(MPU_ADDR << 1);
    ok &= i2c_write(reg);
    i2c_start(); /* repeated START */
    ok &= i2c_write((MPU_ADDR << 1) | 1);
    for (uint8_t i = 0; i < n; i++)
        buf[i] = i2c_read(i + 1 < n);
    i2c_stop();
    return ok;
}

int main(void) {
    uart_init();

    DDRD |= _BV(SCL_BIT);
    scl_hi();
    sda_out();
    sda_hi();

    for (;;) {
        uint8_t who = 0;
        uint8_t temp[2] = {0, 0};
        uint8_t ok = mpu_read(0x75, &who, 1);
        ok &= mpu_read(0x41, temp, 2);

        uart_tx(ok ? 'A' : 'n');
        uart_tx('W');
        print_hex8(who);
        uart_tx('T');
        print_hex8(temp[0]);
        print_hex8(temp[1]);
        uart_tx('\n');
        _delay_ms(5);
    }
}
