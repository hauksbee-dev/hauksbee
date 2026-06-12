/* I2C thermostat firmware for the galvani peripheral proof.
 *
 * Reads the temperature register of an LM75 (I2C address 0x48) over the
 * ATmega328P hardware TWI peripheral and drives a GPIO to indicate whether the
 * temperature is above a threshold:
 *   - PB0 (Arduino D8) HIGH  when T >= THRESHOLD_C  ("over temp")
 *   - PB0 LOW                when T <  THRESHOLD_C
 *
 * It also prints the raw temperature register over UART for debugging. The
 * galvani LM75 slave answers the read with real datasheet-encoded bytes
 * (0.125 C/LSB, left-justified, big-endian), so this is exercising the actual
 * register format, not a stub.
 *
 * ATmega328P @ 16 MHz. TWI at 100 kHz.
 */
#include <avr/io.h>
#include <util/delay.h>

#define LM75_ADDR 0x48
#define THRESHOLD_C 30          /* degrees C */
#define F_CPU_HZ 16000000UL
#define BAUD 9600

/* ---- UART (debug) ---- */
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
static void print_i16(int16_t v) {
    char buf[7];
    int8_t i = 0;
    uint16_t u;
    if (v < 0) {
        uart_tx('-');
        u = (uint16_t)(-v);
    } else {
        u = (uint16_t)v;
    }
    if (u == 0)
        buf[i++] = '0';
    while (u) {
        buf[i++] = '0' + (u % 10);
        u /= 10;
    }
    while (i)
        uart_tx((uint8_t)buf[--i]);
}

/* ---- TWI (I2C) master ---- */
static void twi_init(void) {
    /* SCL = F_CPU / (16 + 2*TWBR*prescaler). For 100 kHz at 16 MHz: TWBR=72. */
    TWSR = 0;
    TWBR = 72;
    TWCR = _BV(TWEN);
}
static void twi_start(void) {
    TWCR = _BV(TWINT) | _BV(TWSTA) | _BV(TWEN);
    while (!(TWCR & _BV(TWINT)))
        ;
}
static void twi_stop(void) {
    TWCR = _BV(TWINT) | _BV(TWSTO) | _BV(TWEN);
    _delay_us(20);
}
static void twi_write(uint8_t data) {
    TWDR = data;
    TWCR = _BV(TWINT) | _BV(TWEN);
    while (!(TWCR & _BV(TWINT)))
        ;
}
static uint8_t twi_read_ack(void) {
    TWCR = _BV(TWINT) | _BV(TWEN) | _BV(TWEA);
    while (!(TWCR & _BV(TWINT)))
        ;
    return TWDR;
}
static uint8_t twi_read_nack(void) {
    TWCR = _BV(TWINT) | _BV(TWEN);
    while (!(TWCR & _BV(TWINT)))
        ;
    return TWDR;
}

/* Read the LM75 temperature register (pointer 0x00), return the raw 16-bit
 * value (MSB:LSB). Decode: T_C = (raw >> 5) * 0.125. */
static int16_t lm75_read_raw(void) {
    uint8_t msb, lsb;
    /* Set pointer register to 0x00 (temperature). */
    twi_start();
    twi_write((LM75_ADDR << 1) | 0); /* write */
    twi_write(0x00);                 /* pointer = temp register */
    twi_stop();
    /* Repeated read of two bytes. */
    twi_start();
    twi_write((LM75_ADDR << 1) | 1); /* read */
    msb = twi_read_ack();
    lsb = twi_read_nack();
    twi_stop();
    return (int16_t)(((uint16_t)msb << 8) | lsb);
}

int main(void) {
    DDRB |= _BV(DDB0); /* PB0 output (the over-temp flag) */
    PORTB &= ~_BV(DDB0);
    uart_init();
    twi_init();

    for (;;) {
        int16_t raw = lm75_read_raw();
        /* raw is left-justified 0.125 C/LSB; integer degrees = raw >> 8. */
        int16_t temp_c = raw >> 8;
        if (temp_c >= THRESHOLD_C) {
            PORTB |= _BV(PORTB0);
        } else {
            PORTB &= ~_BV(PORTB0);
        }
        print_i16(temp_c);
        uart_tx('C');
        uart_tx('\r');
        uart_tx('\n');
        _delay_ms(5);
    }
}
