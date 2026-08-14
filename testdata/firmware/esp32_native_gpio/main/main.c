/* Unmodified-style ESP-IDF GPIO fixture: deliberately NO Hauksbee mailbox.
 * GPIO2 stays high and GPIO4 toggles through the real ESP-IDF driver. A backend
 * can see these edges only through the emulated GPIO peripheral's register
 * state; reading RTC slow RAM at 0x5000_0000 must remain zero.
 */

#include <string.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "driver/gpio.h"
#include "driver/uart.h"

#define PIN_ALIVE GPIO_NUM_2
#define PIN_BLINK GPIO_NUM_4

static void say(const char *s)
{
    uart_write_bytes(UART_NUM_0, s, strlen(s));
}

void app_main(void)
{
    gpio_config_t io = {
        .pin_bit_mask = (1ULL << PIN_ALIVE) | (1ULL << PIN_BLINK),
        .mode = GPIO_MODE_OUTPUT,
        .pull_up_en = GPIO_PULLUP_DISABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    gpio_config(&io);
    gpio_set_level(PIN_ALIVE, 1);
    say("native gpio: no hauksbee mailbox\r\n");

    int level = 0;
    for (;;) {
        level = !level;
        gpio_set_level(PIN_BLINK, level);
        vTaskDelay(pdMS_TO_TICKS(100));
    }
}
