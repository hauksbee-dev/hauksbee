/* I2C thermostat firmware for the hauksbee ESP32 QEMU co-sim peripheral proof.
 *
 * Mirrors the AVR i2c_thermostat firmware contract on an ESP32:
 *   - Reads the LM75 temperature register (I2C address 0x48) over the ESP32
 *     hardware I2C0 peripheral (GPIO21=SDA, GPIO22=SCL, the default IDF pins).
 *   - Drives GPIO5 HIGH when T >= 30 C, LOW otherwise (the "FLAG" output).
 *   - Prints the integer temperature over UART0 at 115200 8N1 for debug.
 *
 * Also writes the GPIO observation mailbox (see esp32_blinky/main.c) so the
 * hauksbee QEMU backend can observe pin state without peripheral register
 * read-back issues.
 *
 * Uses esp-idf I2C master driver. Build with:
 *   idf.py set-target esp32 && idf.py build && ./build.sh
 */

#include <stdio.h>
#include <string.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "driver/i2c.h"
#include "driver/gpio.h"

/* I2C configuration */
#define I2C_MASTER_PORT   I2C_NUM_0
#define I2C_SDA_PIN       21
#define I2C_SCL_PIN       22
#define I2C_FREQ_HZ       100000

/* LM75 */
#define LM75_ADDR         0x48
#define THRESHOLD_C       30

/* GPIO FLAG pin (over-temp indicator) */
#define PIN_FLAG          5

/* GPIO observation mailbox (same layout as esp32_blinky).
 * RTC slow memory 0x5000_0000, uncached, fixed address. */
#define HAUKSBEE_MAILBOX_BASE  0x50000000UL
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

static void i2c_master_init(void)
{
    i2c_config_t conf = {
        .mode             = I2C_MODE_MASTER,
        .sda_io_num       = I2C_SDA_PIN,
        .scl_io_num       = I2C_SCL_PIN,
        .sda_pullup_en    = GPIO_PULLUP_ENABLE,
        .scl_pullup_en    = GPIO_PULLUP_ENABLE,
        .master.clk_speed = I2C_FREQ_HZ,
    };
    i2c_param_config(I2C_MASTER_PORT, &conf);
    i2c_driver_install(I2C_MASTER_PORT, conf.mode, 0, 0, 0);
}

/* Read 2 bytes from LM75 temperature register (pointer 0x00).
 * Returns 0 on success, non-zero on I2C error. */
static int lm75_read(int16_t *raw_out)
{
    /* Write pointer register = 0x00 */
    i2c_cmd_handle_t cmd = i2c_cmd_link_create();
    i2c_master_start(cmd);
    i2c_master_write_byte(cmd, (LM75_ADDR << 1) | I2C_MASTER_WRITE, true);
    i2c_master_write_byte(cmd, 0x00, true);
    i2c_master_stop(cmd);
    esp_err_t ret = i2c_master_cmd_begin(I2C_MASTER_PORT, cmd, pdMS_TO_TICKS(100));
    i2c_cmd_link_delete(cmd);
    if (ret != ESP_OK) return (int)ret;

    /* Read 2 bytes */
    uint8_t buf[2] = {0, 0};
    cmd = i2c_cmd_link_create();
    i2c_master_start(cmd);
    i2c_master_write_byte(cmd, (LM75_ADDR << 1) | I2C_MASTER_READ, true);
    i2c_master_read_byte(cmd, &buf[0], I2C_MASTER_ACK);
    i2c_master_read_byte(cmd, &buf[1], I2C_MASTER_NACK);
    i2c_master_stop(cmd);
    ret = i2c_master_cmd_begin(I2C_MASTER_PORT, cmd, pdMS_TO_TICKS(100));
    i2c_cmd_link_delete(cmd);
    if (ret != ESP_OK) return (int)ret;

    *raw_out = (int16_t)(((uint16_t)buf[0] << 8) | buf[1]);
    return 0;
}

void app_main(void)
{
    /* Configure FLAG GPIO as output */
    gpio_config_t io = {
        .pin_bit_mask    = (1ULL << PIN_FLAG),
        .mode            = GPIO_MODE_OUTPUT,
        .pull_up_en      = GPIO_PULLUP_DISABLE,
        .pull_down_en    = GPIO_PULLDOWN_DISABLE,
        .intr_type       = GPIO_INTR_DISABLE,
    };
    gpio_config(&io);

    /* Publish mailbox magic */
    HAUKSBEE_MAGIC    = HAUKSBEE_MAGIC_VALUE;
    gpio_out_shadow   = 0;
    HAUKSBEE_GPIO_OUT = 0;

    i2c_master_init();
    printf("esp32 i2c thermostat ready\r\n");

    for (;;) {
        int16_t raw = 0;
        int err = lm75_read(&raw);
        if (err == 0) {
            /* LM75A: raw is 11-bit left-justified, 0.125 C/LSB.
             * Integer degrees = raw >> 8 (sign-extends correctly). */
            int32_t temp_c = (int32_t)(raw >> 8);

            if (temp_c >= THRESHOLD_C) {
                gpio_set_level(PIN_FLAG, 1);
                mailbox_set(PIN_FLAG, 1);
            } else {
                gpio_set_level(PIN_FLAG, 0);
                mailbox_set(PIN_FLAG, 0);
            }

            printf("%dC\r\n", (int)temp_c);
        } else {
            printf("i2c_err %d\r\n", err);
        }

        /* ~50 ms poll interval */
        vTaskDelay(pdMS_TO_TICKS(50));
    }
}
