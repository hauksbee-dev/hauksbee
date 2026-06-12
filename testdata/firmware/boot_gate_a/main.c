/* boot-coverage demo, variant A (PASS).
 * The MOSFET gate is on PB0 (ATmega328P pad 14). At reset the GPIO is Hi-Z and
 * the gate floats (no pull on the board), so the boot-coverage assertion's
 * watched control net is undefined. This firmware promptly configures PB0 as an
 * output and drives it HIGH, so the gate is actively driven to a defined level
 * within the boot deadline. ATmega328P @ 16 MHz.
 */
#include <avr/io.h>
#include <util/delay.h>

int main(void) {
    /* Drive the gate promptly: DDB0 = output, PORTB0 = high. */
    DDRB |= _BV(DDB0);
    PORTB |= _BV(PORTB0);
    for (;;) {
        /* hold the gate driven */
        _delay_ms(100);
    }
    return 0;
}
