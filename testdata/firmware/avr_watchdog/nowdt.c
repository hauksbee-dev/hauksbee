/* The control for `wdt.c`: byte-for-byte the same firmware with the one
 * `wdt_enable(WDTO_15MS)` line left out.
 *
 * Its job is to make the watchdog test two-sided. A harness that runs 40 chunks
 * and sees 40 chunks come back proves nothing on its own; it has to fail on one
 * image and pass on the other. This image completed all 40 chunks even while
 * `wdt.c` hung in its third, which is what pins the hang on the watchdog reset
 * path rather than on the harness, the UART, or the delay loop.
 */

#define F_CPU 16000000UL
#include <avr/io.h>
#include <util/delay.h>

static void uart_init(void) {
    /* 115200 8N1 at 16 MHz with U2X0: UBRR = 8. */
    UBRR0H = 0;
    UBRR0L = 8;
    UCSR0A = (1 << U2X0);
    UCSR0B = (1 << TXEN0);
    UCSR0C = (1 << UCSZ01) | (1 << UCSZ00);
}

static void tx(char c) {
    while (!(UCSR0A & (1 << UDRE0)))
        ;
    UDR0 = c;
}

static void puts_(const char *s) {
    while (*s)
        tx(*s++);
}

static void hex(unsigned char v) {
    const char *d = "0123456789abcdef";
    tx(d[v >> 4]);
    tx(d[v & 0xF]);
}

int main(void) {
    unsigned char src = MCUSR;
    MCUSR = 0;

    DDRB |= (1 << 5);
    uart_init();
    puts_("BOOT mcusr=");
    hex(src);
    puts_("\r\n");

    /* No watchdog: the control half of the pair. */

    for (;;) {
        PORTB ^= (1 << 5);
        _delay_ms(5);
    }
}
