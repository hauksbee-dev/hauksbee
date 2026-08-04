/* ATmega328P watchdog firmware: arm the watchdog and deliberately starve it.
 *
 * WHAT THIS IS FOR
 * ---------------------------------------------------------------------------
 * `wdt_enable(WDTO_15MS)` with no `wdt_reset()` used to HANG the whole
 * co-simulator. simavr's `avr_reset` zeroes `avr->cycle`, and the backend's
 * step loop ran against an absolute cumulative cycle target that a rewound
 * counter can never reach, so the chunk in which the watchdog first fired never
 * returned. `crates/hauksbee-mcu/tests/avr_watchdog.rs` runs this image to a
 * fixed number of chunks and asserts every one of them comes back.
 *
 * Prints the raw MCUSR reset-source byte at boot (PORF, bit 0, is a power-on
 * reset; WDRF, bit 3, is a watchdog reset), so the UART record shows WHICH
 * reset each boot came from rather than just that a boot happened. PB5 toggles
 * every 5 ms, which is the "still running" signal: on silicon it stops at the
 * timeout and resumes after the reboot, forever.
 *
 * `nowdt.c` is this file with the one `wdt_enable` line removed. It is the
 * control: the same firmware, the same 16 MHz clock, the same toggling, and it
 * completed every chunk even while this one hung, which is what pins the hang
 * on the watchdog rather than on the harness.
 */

#define F_CPU 16000000UL
#include <avr/io.h>
#include <avr/wdt.h>
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
    /* Read and CLEAR the reset-source flags before doing anything else: MCUSR
     * accumulates them, so a stale WDRF would make the next power-on look like
     * a watchdog reboot. */
    unsigned char src = MCUSR;
    MCUSR = 0;

    DDRB |= (1 << 5);
    uart_init();
    puts_("BOOT mcusr=");
    hex(src);
    puts_("\r\n");

    wdt_enable(WDTO_15MS);

    for (;;) {
        PORTB ^= (1 << 5);
        _delay_ms(5);
    }
}
