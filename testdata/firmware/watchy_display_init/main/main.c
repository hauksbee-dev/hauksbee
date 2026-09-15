/* Watchy e-paper display-init firmware for the hauksbee boot-coverage execution.
 *
 * This is a DELIBERATELY REDUCED stand-in for the full Watchy Arduino firmware,
 * built to flip the Watchy display-RES# boot-coverage validation row
 * (docs/evidence/CORPUS.md) from MISSED-but-decidable to executed. The
 * full Watchy firmware (sqfmi/Watchy, Arduino + GxEPD2) does the same thing this
 * does for the reset line, but pulls in WiFi/BT/RTC/SPI display stacks that the
 * Espressif QEMU fork does not model and that do not mirror GPIO to the RAM
 * mailbox the backend needs to observe a pin (see docs/cosim/MCU.md). So this firmware
 * reproduces ONLY the load-bearing operation the documented fault is about, on
 * the same GPIO the real board uses, and maintains the mailbox so the engine can
 * watch it:
 *
 *   - DISPLAY_RES (GPIO9 on the Watchy ESP32-PICO-D4, board net "RES", U1 pad 28
 *     verified from the corpus board file) is configured as an output and driven
 *     HIGH at boot, deasserting the e-paper reset and HOLDING it high. The
 *     reporter of Watchy issue #14 states: "RES# of the display is supposed to be
 *     left connected to HIGH. If it is not left in that state the Display
 *     hardware will not fully enter deep sleep." This firmware is the "left in
 *     that state" behaviour: it brings RES high promptly and never releases it.
 *   - The other display control lines the Watchy uses are configured the same
 *     way for fidelity: DC=GPIO10, CS=GPIO5 (idle high), BUSY=GPIO19 (input).
 *   - UART0 prints a banner so the boot is observable even without GPIO mailbox.
 *   - The RAM mailbox (0x5000_0000) is maintained exactly as the esp32_blinky
 *     demo does, so the hauksbee QEMU backend synthesises the GPIO9 rising edge
 *     and the boot-coverage assertion can time it.
 *
 * What this firmware does NOT claim: it is not the full Watchy stack, it does not
 * run GxEPD2, and it does not prove the real firmware drives RES at the same
 * time. It proves the decidable thing the validation row was waiting on: that
 * the boot-coverage mechanism, on the real Watchy board file under the real
 * ESP32 QEMU backend, observes a firmware that brings the e-paper reset to its
 * required HIGH state in time. The verdict is labelled as a reduced validation.
 */

#include <stdio.h>
#include <string.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "driver/gpio.h"
#include "driver/uart.h"

/* Watchy ESP32-PICO-D4 display GPIOs (sqfmi/Watchy src/Watchy.h; cross-checked
 * against the corpus board file: net RES = U1 pad 28 = GPIO9). */
#define PIN_DISPLAY_RES 9  /* e-paper RES#: must be driven HIGH and held */
#define PIN_DISPLAY_DC 10  /* data/command */
#define PIN_DISPLAY_CS 5   /* SPI chip-select, idle high */
#define PIN_DISPLAY_BUSY 19 /* e-paper BUSY (input) */

#define UART_PORT UART_NUM_0

/* GPIO observation mailbox, identical layout to the esp32_blinky demo. */
#define HAUKSBEE_MAILBOX_BASE 0x50000000UL
#define HAUKSBEE_GPIO_OUT (*(volatile uint32_t *)(HAUKSBEE_MAILBOX_BASE + 0x00))
#define HAUKSBEE_GPIO_IN  (*(volatile uint32_t *)(HAUKSBEE_MAILBOX_BASE + 0x04))
#define HAUKSBEE_MAGIC    (*(volatile uint32_t *)(HAUKSBEE_MAILBOX_BASE + 0x08))
#define HAUKSBEE_MAGIC_VALUE 0x6A6C6E69UL

static uint32_t gpio_out_shadow;

static void mailbox_set(int pin, int level)
{
    if (level) {
        gpio_out_shadow |= (1u << pin);
    } else {
        gpio_out_shadow &= ~(1u << pin);
    }
    HAUKSBEE_GPIO_OUT = gpio_out_shadow;
}

static void uart_setup(void)
{
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

void app_main(void)
{
    /* Outputs: RES, DC, CS. BUSY is an input. */
    gpio_config_t out = {
        .pin_bit_mask = (1ULL << PIN_DISPLAY_RES) | (1ULL << PIN_DISPLAY_DC) |
                        (1ULL << PIN_DISPLAY_CS),
        .mode = GPIO_MODE_OUTPUT,
        .pull_up_en = GPIO_PULLUP_DISABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    gpio_config(&out);
    gpio_config_t in = {
        .pin_bit_mask = (1ULL << PIN_DISPLAY_BUSY),
        .mode = GPIO_MODE_INPUT,
        .pull_up_en = GPIO_PULLUP_DISABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    gpio_config(&in);

    /* Publish the mailbox tag so the backend trusts the GPIO mirror. */
    HAUKSBEE_MAGIC = HAUKSBEE_MAGIC_VALUE;
    gpio_out_shadow = 0;
    HAUKSBEE_GPIO_OUT = 0;

    /* Bring the e-paper reset HIGH and hold it: the documented required state.
     * CS idle high, DC low (command phase default). The real GxEPD2 init pulses
     * RES low-high-low-high to reset the panel then leaves it HIGH; the
     * load-bearing end state for the #14 fault is "left HIGH", which is what we
     * assert reaches and holds. */
    gpio_set_level(PIN_DISPLAY_CS, 1);
    mailbox_set(PIN_DISPLAY_CS, 1);
    gpio_set_level(PIN_DISPLAY_DC, 0);
    mailbox_set(PIN_DISPLAY_DC, 0);
    gpio_set_level(PIN_DISPLAY_RES, 1);
    mailbox_set(PIN_DISPLAY_RES, 1);

    uart_setup();
    uart_puts("watchy display-init: RES high\r\n");

    /* Hold the state forever; the boot-coverage assertion samples the RES net. */
    for (;;) {
        vTaskDelay(pdMS_TO_TICKS(200));
    }
}
