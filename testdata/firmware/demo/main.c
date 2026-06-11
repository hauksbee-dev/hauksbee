/* Demo firmware exercising every galvani co-sim coupling path:
 * - GPIO out: PB5 (Arduino D13 LED) toggles every 100ms
 * - ADC in:   channel 0 sampled continuously
 * - UART:     9600 baud; 'v' returns last ADC reading in millivolts,
 *             'i' returns ident string, anything else is echoed back
 * ATmega328P @ 16 MHz.
 */
#include <avr/io.h>
#include <util/delay.h>

#define F_CPU_HZ 16000000UL
#define BAUD 9600

static void uart_init(void) {
    uint16_t ubrr = (F_CPU_HZ / (16UL * BAUD)) - 1;
    UBRR0H = (uint8_t)(ubrr >> 8);
    UBRR0L = (uint8_t)ubrr;
    UCSR0B = _BV(RXEN0) | _BV(TXEN0);
    UCSR0C = _BV(UCSZ01) | _BV(UCSZ00);
}

static void uart_tx(uint8_t c) {
    while (!(UCSR0A & _BV(UDRE0)))
        ;
    UDR0 = c;
}

static void uart_puts(const char *s) {
    while (*s)
        uart_tx((uint8_t)*s++);
}

static uint8_t uart_poll(uint8_t *out) {
    if (UCSR0A & _BV(RXC0)) {
        *out = UDR0;
        return 1;
    }
    return 0;
}

static void adc_init(void) {
    ADMUX = _BV(REFS0); /* AVcc reference, channel 0 */
    ADCSRA = _BV(ADEN) | _BV(ADPS2) | _BV(ADPS1) | _BV(ADPS0);
}

static uint16_t adc_read(void) {
    ADCSRA |= _BV(ADSC);
    while (ADCSRA & _BV(ADSC))
        ;
    return ADC;
}

static void print_u16(uint16_t v) {
    char buf[6];
    int8_t i = 0;
    if (v == 0)
        buf[i++] = '0';
    while (v) {
        buf[i++] = '0' + (v % 10);
        v /= 10;
    }
    while (i)
        uart_tx((uint8_t)buf[--i]);
}

int main(void) {
    DDRB |= _BV(DDB5);
    uart_init();
    adc_init();
    uart_puts("galvani-demo v1\r\n");

    uint16_t last_mv = 0;
    uint8_t ticks = 0;
    for (;;) {
        /* ~10ms loop tick */
        _delay_ms(10);
        last_mv = (uint16_t)(((uint32_t)adc_read() * 5000UL) / 1023UL);
        if (++ticks >= 10) {
            ticks = 0;
            PORTB ^= _BV(PORTB5);
        }
        uint8_t c;
        while (uart_poll(&c)) {
            if (c == 'v') {
                print_u16(last_mv);
                uart_puts("mV\r\n");
            } else if (c == 'i') {
                uart_puts("galvani-demo v1\r\n");
            } else {
                uart_tx(c);
            }
        }
    }
}
