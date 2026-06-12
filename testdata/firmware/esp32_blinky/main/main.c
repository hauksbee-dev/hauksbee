/* Minimal ESP32 firmware for the galvani QEMU co-sim demo.
 *
 * Exercises the same coupling paths the STM32/AVR demo firmwares do, so the
 * QEMU backend is proven against the identical co-sim contract:
 *
 *   - GPIO out: GPIO2 driven HIGH at boot as a steady "alive" indicator that
 *     feeds the solved analog circuit (LED + resistor on the demo board). This
 *     is the analog current path the MNA solver computes.
 *   - GPIO out: GPIO4 toggles at ~5 Hz (the blink), observable through the
 *     solved circuit as a logic-level toggle on its net.
 *   - UART:     UART0 at 115200 8N1. Prints "hello from esp32\r\n" once at boot,
 *               then on every received byte:
 *                 'v' -> prints the loop tick count as decimal + "\r\n"
 *                 'i' -> prints the ident string again
 *                 else -> echoes the byte back.
 *
 * It uses the esp-idf gpio + uart drivers (battle-tested) for the real GPIO and
 * UART activity.
 *
 * GPIO observation mailbox: the Espressif QEMU esp32 GPIO peripheral model does
 * not implement read-back of GPIO_OUT_REG (a host read of 0x3FF44004 returns 0
 * regardless of the driven level; verified empirically). RAM, by contrast, reads
 * back exactly over the QEMU control channel. So the firmware mirrors its GPIO
 * output word to a fixed, reserved RAM mailbox after every change, and the
 * galvani QEMU backend reads THAT word (same bit layout as GPIO_OUT_REG) to
 * synthesise pin edges. The GPIO writes themselves are real driver calls; the
 * mailbox is only the observation path the emulator's gpio model lacks. The
 * mailbox address is published in the firmware's ELF symbol `galvani_gpio_out`
 * and pinned by the backend's GALVANI_ESP32_GPIO_MAILBOX constant.
 *
 * Likewise GPIO input: the backend pokes the mailbox-adjacent `galvani_gpio_in`
 * word, and the firmware reads it where it would read GPIO_IN_REG.
 */

#include <stdio.h>
#include <string.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "driver/gpio.h"
#include "driver/uart.h"

#define PIN_ALIVE 2 /* GPIO2: steady HIGH -> LED current path */
#define PIN_BLINK 4 /* GPIO4: ~5 Hz toggle */

#define UART_PORT UART_NUM_0

/* GPIO observation/injection mailbox in RTC slow memory (0x5000_0000, 8 KiB,
 * uncached, fixed address, untouched by a minimal app). The galvani QEMU backend
 * reads/writes these exact addresses (RAM reads/writes round-trip over the QEMU
 * control channel; the GPIO peripheral registers do not). Layout:
 *   +0x00  galvani_gpio_out : mirror of GPIO_OUT_REG (firmware -> host)
 *   +0x04  galvani_gpio_in  : injected input word (host -> firmware)
 *   +0x08  galvani_magic    : 0x6A6C6E69 ("galv" tag) so the backend can confirm
 *                             the firmware is mailbox-aware before trusting it. */
#define GALVANI_MAILBOX_BASE 0x50000000UL
#define GALVANI_GPIO_OUT (*(volatile uint32_t *)(GALVANI_MAILBOX_BASE + 0x00))
#define GALVANI_GPIO_IN  (*(volatile uint32_t *)(GALVANI_MAILBOX_BASE + 0x04))
#define GALVANI_MAGIC    (*(volatile uint32_t *)(GALVANI_MAILBOX_BASE + 0x08))
#define GALVANI_MAGIC_VALUE 0x6A6C6E69UL

static uint32_t gpio_out_shadow;

static void mailbox_set(int pin, int level)
{
    if (level) {
        gpio_out_shadow |= (1u << pin);
    } else {
        gpio_out_shadow &= ~(1u << pin);
    }
    GALVANI_GPIO_OUT = gpio_out_shadow;
}

static void uart_setup(void)
{
    /* UART0 is already initialised as the console by the bootloader; install a
     * small driver so we can read RX bytes back. 115200 8N1 is the default. */
    const uart_config_t cfg = {
        .baud_rate = 115200,
        .data_bits = UART_DATA_8_BITS,
        .parity = UART_PARITY_DISABLE,
        .stop_bits = UART_STOP_BITS_1,
        .flow_ctrl = UART_HW_FLOWCTRL_DISABLE,
        .source_clk = UART_SCLK_DEFAULT,
    };
    uart_driver_install(UART_PORT, 256, 0, 0, NULL, 0);
    uart_param_config(UART_PORT, &cfg);
}

static void uart_puts(const char *s)
{
    uart_write_bytes(UART_PORT, s, strlen(s));
}

static void print_u32(uint32_t v)
{
    char buf[12];
    int i = 0;
    if (v == 0) {
        buf[i++] = '0';
    }
    while (v) {
        buf[i++] = (char)('0' + (v % 10));
        v /= 10;
    }
    char out[12];
    int j = 0;
    while (i) {
        out[j++] = buf[--i];
    }
    uart_write_bytes(UART_PORT, out, j);
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

    /* Publish the mailbox tag so the backend knows this firmware mirrors GPIO. */
    GALVANI_MAGIC = GALVANI_MAGIC_VALUE;
    gpio_out_shadow = 0;
    GALVANI_GPIO_OUT = 0;

    /* Drive GPIO2 HIGH so the analog LED net is energised from boot. */
    gpio_set_level(PIN_ALIVE, 1);
    mailbox_set(PIN_ALIVE, 1);

    uart_setup();
    uart_puts("hello from esp32\r\n");

    uint32_t ticks = 0;
    int level = 0;
    uint8_t rx;
    for (;;) {
        level = !level;
        gpio_set_level(PIN_BLINK, level);
        mailbox_set(PIN_BLINK, level);
        ticks++;

        /* Drain UART RX (non-blocking) and respond, mirroring the STM32 demo. */
        while (uart_read_bytes(UART_PORT, &rx, 1, 0) == 1) {
            if (rx == 'v') {
                print_u32(ticks);
                uart_puts("\r\n");
            } else if (rx == 'i') {
                uart_puts("hello from esp32\r\n");
            } else {
                uart_write_bytes(UART_PORT, &rx, 1);
            }
        }

        /* ~100 ms of virtual time -> ~5 Hz blink, oversampled by the engine's
         * analog chunks just like the STM32 demo. */
        vTaskDelay(pdMS_TO_TICKS(100));
    }
}
