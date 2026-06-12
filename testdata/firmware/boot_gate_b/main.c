/* boot-coverage demo, variant B (FAIL).
 * Identical board, but this firmware never configures PB0: the MOSFET gate is
 * left floating (Hi-Z) for the whole run. The boot-coverage assertion must FAIL,
 * naming the control net that was never driven. ATmega328P @ 16 MHz.
 */
#include <avr/io.h>
#include <util/delay.h>

int main(void) {
    /* Blink PB5 (an unrelated LED pin) so the firmware is clearly alive, but
     * deliberately never touch PB0 (the gate). */
    DDRB |= _BV(DDB5);
    for (;;) {
        PORTB ^= _BV(PORTB5);
        _delay_ms(100);
    }
    return 0;
}
