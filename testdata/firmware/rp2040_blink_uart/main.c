// Stock pico-sdk firmware: the SDK runtime brings the chip up, then main()
// prints over UART0 and toggles GP25 (the Pico's on-board LED pin).
#include <stdio.h>
#include "pico/stdlib.h"

#define LED_PIN 25

int main(void) {
    stdio_init_all();
    printf("hauksbee rp2040: main reached\n");

    gpio_init(LED_PIN);
    gpio_set_dir(LED_PIN, GPIO_OUT);

    for (int i = 0; i < 200; i++) {
        gpio_put(LED_PIN, 1);
        printf("led on %d\n", i);
        sleep_ms(20);
        gpio_put(LED_PIN, 0);
        printf("led off %d\n", i);
        sleep_ms(20);
    }
    printf("hauksbee rp2040: done\n");
    while (1) {
        tight_loop_contents();
    }
}
