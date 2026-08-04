// pico-sdk firmware that clocks two bytes out of spi0 and prints what came back
// on MISO, so a host-side SPI bridge slave can be proven end to end.
#include <stdio.h>
#include "pico/stdlib.h"
#include "hardware/spi.h"

#define SCK_PIN 18
#define MOSI_PIN 19
#define MISO_PIN 16
#define CS_PIN 17

int main(void) {
    stdio_init_all();
    printf("hauksbee rp2040 spi: main reached\n");

    spi_init(spi0, 1000 * 1000);
    gpio_set_function(SCK_PIN, GPIO_FUNC_SPI);
    gpio_set_function(MOSI_PIN, GPIO_FUNC_SPI);
    gpio_set_function(MISO_PIN, GPIO_FUNC_SPI);
    gpio_init(CS_PIN);
    gpio_set_dir(CS_PIN, GPIO_OUT);
    gpio_put(CS_PIN, 1);

    for (int i = 0; i < 50; i++) {
        uint8_t tx[2] = {0x9F, 0x00};
        uint8_t rx[2] = {0, 0};
        gpio_put(CS_PIN, 0);
        spi_write_read_blocking(spi0, tx, rx, 2);
        gpio_put(CS_PIN, 1);
        printf("spi bytes=%02X %02X\n", rx[0], rx[1]);
        sleep_ms(20);
    }
    printf("hauksbee rp2040 spi: done\n");
    while (1) {
        tight_loop_contents();
    }
}
