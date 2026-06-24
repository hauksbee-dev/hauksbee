/* ESP32 SPI ADC firmware for the hauksbee QEMU SPI co-sim.
 *
 * Exercises the SPI coupling path on ESP32 QEMU:
 *   - SPI master: reads MCP3008 channel 0 via esp-idf SPI driver.
 *     HSPI bus (SPI2): SCLK=GPIO14, MISO=GPIO12, MOSI=GPIO13, CS=GPIO15.
 *   - Threshold: if the ADC count >= 512 (i.e. Vin >= Vref/2 = 1.65 V with
 *     a 3.3 V reference), drive GPIO26 (net "FLAG") HIGH; otherwise LOW.
 *   - GPIO observation mailbox: same RTC slow RAM mailbox pattern as the blinky
 *     firmware. The QEMU GPIO peripheral does not read back GPIO_OUT_REG, so
 *     the firmware mirrors its GPIO output word to hauksbee_gpio_out so the
 *     hauksbee QEMU backend can synthesise pin edges.
 *   - UART: prints "spi adc ready\r\n" at boot and "adc:<decimal>\r\n" after
 *     each conversion.
 *
 * MCP3008 3-byte SPI protocol (mode 0, MSB first):
 *   byte0 = 0x01 (start bit)
 *   byte1 = 0x80 (single-ended channel 0: SGL=1, CH2..CH0=000)
 *   byte2 = 0x00 (clock out the low 8 result bits)
 * Reply:
 *   byte1 low 2 bits = ADC[9:8]
 *   byte2 = ADC[7:0]
 */

#include <stdio.h>
#include <string.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "driver/spi_master.h"
#include "driver/gpio.h"
#include "driver/uart.h"

/* HSPI (SPI2) pin assignment */
#define PIN_SCLK   14
#define PIN_MISO   12
#define PIN_MOSI   13
#define PIN_CS     15

/* FLAG output pin. GPIO4 = pad 26 on ESP32-WROOM-32, the "p04" role in the
 * hauksbee model DB. This is the observable threshold output. */
#define PIN_FLAG   4

/* ADC threshold: counts >= 512 -> FLAG HIGH */
#define ADC_THRESHOLD 512

#define UART_PORT UART_NUM_0

/* GPIO observation/injection mailbox in RTC slow memory (same layout as
 * esp32_blinky firmware; the hauksbee QEMU backend reads this to synthesise
 * GPIO edges because ESP32 QEMU GPIO_OUT_REG does not read back correctly).
 *
 *   +0x00  hauksbee_gpio_out : mirror of GPIO_OUT_REG (firmware -> host)
 *   +0x04  hauksbee_gpio_in  : injected input word (host -> firmware)
 *   +0x08  hauksbee_magic    : 0x6A6C6E69 tag confirming mailbox-aware firmware
 */
#define HAUKSBEE_MAILBOX_BASE 0x50000000UL
#define HAUKSBEE_GPIO_OUT (*(volatile uint32_t *)(HAUKSBEE_MAILBOX_BASE + 0x00))
#define HAUKSBEE_GPIO_IN  (*(volatile uint32_t *)(HAUKSBEE_MAILBOX_BASE + 0x04))
#define HAUKSBEE_MAGIC    (*(volatile uint32_t *)(HAUKSBEE_MAILBOX_BASE + 0x08))
#define HAUKSBEE_MAGIC_VALUE 0x6A6C6E69UL

static uint32_t gpio_out_shadow;

static void mailbox_set(int pin, int level)
{
    if (level)
        gpio_out_shadow |= (1u << pin);
    else
        gpio_out_shadow &= ~(1u << pin);
    HAUKSBEE_GPIO_OUT = gpio_out_shadow;
}

static void uart_setup(void)
{
    const uart_config_t cfg = {
        .baud_rate = 115200,
        .data_bits = UART_DATA_8_BITS,
        .parity    = UART_PARITY_DISABLE,
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

static void uart_print_u32(uint32_t v)
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
    while (i) out[j++] = buf[--i];
    uart_write_bytes(UART_PORT, out, j);
}

void app_main(void)
{
    /* Publish mailbox tag */
    HAUKSBEE_MAGIC = HAUKSBEE_MAGIC_VALUE;
    gpio_out_shadow = 0;
    HAUKSBEE_GPIO_OUT = 0;

    /* Configure FLAG GPIO */
    gpio_config_t flag_io = {
        .pin_bit_mask = (1ULL << PIN_FLAG),
        .mode = GPIO_MODE_OUTPUT,
        .pull_up_en = GPIO_PULLUP_DISABLE,
        .pull_down_en = GPIO_PULLDOWN_DISABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    gpio_config(&flag_io);
    gpio_set_level(PIN_FLAG, 0);
    mailbox_set(PIN_FLAG, 0);

    /* Set up SPI bus (HSPI / SPI2) */
    spi_bus_config_t buscfg = {
        .mosi_io_num   = PIN_MOSI,
        .miso_io_num   = PIN_MISO,
        .sclk_io_num   = PIN_SCLK,
        .quadwp_io_num = -1,
        .quadhd_io_num = -1,
        .max_transfer_sz = 4,
    };
    spi_bus_initialize(SPI2_HOST, &buscfg, SPI_DMA_DISABLED);

    /* Attach MCP3008: mode 0, 1 MHz (well within the 3.6 MHz Vdd=5V limit) */
    spi_device_interface_config_t devcfg = {
        .clock_speed_hz = 1000000,
        .mode           = 0,
        .spics_io_num   = PIN_CS,
        .queue_size     = 1,
        .command_bits   = 0,
        .address_bits   = 0,
    };
    spi_device_handle_t spi;
    spi_bus_add_device(SPI2_HOST, &devcfg, &spi);

    uart_setup();
    uart_puts("spi adc ready\r\n");

    for (;;) {
        /* 3-byte MCP3008 transfer */
        uint8_t tx[3] = { 0x01, 0x80, 0x00 };
        uint8_t rx[3] = { 0x00, 0x00, 0x00 };
        spi_transaction_t t = {
            .length    = 24,
            .tx_buffer = tx,
            .rx_buffer = rx,
        };
        spi_device_transmit(spi, &t);

        uint16_t counts = (uint16_t)(((rx[1] & 0x03U) << 8) | rx[2]);

        uart_puts("adc:");
        uart_print_u32(counts);
        uart_puts("\r\n");

        int flag_level = (counts >= ADC_THRESHOLD) ? 1 : 0;
        gpio_set_level(PIN_FLAG, flag_level);
        mailbox_set(PIN_FLAG, flag_level);

        /* ~10 ms between conversions */
        vTaskDelay(pdMS_TO_TICKS(10));
    }
}
