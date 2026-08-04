// pico-sdk firmware that reads ADC channel 0 (GP26) and prints the raw 12-bit
// count, so an engine-injected analog voltage can be proven to reach firmware.
#include <stdio.h>
#include "pico/stdlib.h"
#include "hardware/adc.h"

int main(void) {
    stdio_init_all();
    printf("hauksbee rp2040 adc: main reached\n");

    adc_init();
    adc_gpio_init(26);
    adc_select_input(0);

    for (int i = 0; i < 200; i++) {
        uint16_t raw = adc_read();
        printf("adc count=%u\n", raw);
        sleep_ms(20);
    }
    printf("hauksbee rp2040 adc: done\n");
    while (1) {
        tight_loop_contents();
    }
}
