// pico-sdk firmware that reads two bytes from an I2C slave at 0x48 on i2c0 and
// prints them over UART0, so a host-side bridge slave can be proven end to end.
#include <stdio.h>
#include "pico/stdlib.h"
#include "hardware/i2c.h"

#define SDA_PIN 4
#define SCL_PIN 5
#define SLAVE_ADDR 0x48

int main(void) {
    stdio_init_all();
    printf("hauksbee rp2040 i2c: main reached\n");

    i2c_init(i2c0, 100 * 1000);
    gpio_set_function(SDA_PIN, GPIO_FUNC_I2C);
    gpio_set_function(SCL_PIN, GPIO_FUNC_I2C);
    gpio_pull_up(SDA_PIN);
    gpio_pull_up(SCL_PIN);

    for (int i = 0; i < 50; i++) {
        uint8_t reg = 0x00;
        int w = i2c_write_blocking(i2c0, SLAVE_ADDR, &reg, 1, true);
        uint8_t rx[2] = {0, 0};
        int r = i2c_read_blocking(i2c0, SLAVE_ADDR, rx, 2, false);
        printf("i2c w=%d r=%d bytes=%02X %02X\n", w, r, rx[0], rx[1]);
        sleep_ms(20);
    }
    printf("hauksbee rp2040 i2c: done\n");
    while (1) {
        tight_loop_contents();
    }
}
