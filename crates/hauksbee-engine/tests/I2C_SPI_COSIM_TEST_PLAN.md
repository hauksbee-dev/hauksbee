# I2C and SPI Co-Simulation Test Plan

This document describes the co-simulation test contracts for I2C and SPI sensor
integration through the hauksbee engine. Both families follow the same pattern:
a firmware image reads a peripheral over a hardware bus, makes a decision, and
drives a GPIO whose net voltage the test reads from the solved analog circuit.

---

## 1. Architecture

### 1.1 Test pattern

Each test:
1. Builds an engine from a KiCad board (inline `const BOARD` for AVR, a file for
   Renode/QEMU targets) that wires the MCU's output GPIO to a named net.
2. Attaches a software peripheral slave (`I2cBus` / `SpiBus`) via
   `engine.scheduler_mut().attach_i2c_bus` / `attach_spi_bus`.
3. Sets the peripheral's reading to a known value
   (e.g. `lm75.set_temp_c(40.0)` or `mcp3008.set_channel(0, 2.5)`).
4. Runs the co-sim for a fixed virtual time window.
5. Reads the "FLAG" net voltage from the final frame and asserts it is HIGH or
   LOW based on the expected threshold response.

### 1.2 Co-sim coupling mechanism

| Backend | Peripheral hook | Direction   | Status       |
|---------|----------------|-------------|--------------|
| AVR (simavr) | `on_i2c` CB via TWI IRQ | byte-level interception | WORKING |
| AVR (simavr) | `on_spi` CB via SPI IRQ | byte-level interception | WORKING |
| Renode (STM32) | `on_i2c` | not wired | RED |
| Renode (STM32) | `on_spi` | not wired | RED |
| QEMU (ESP32) | `on_i2c` | not wired | RED |
| QEMU (ESP32) | `on_spi` | not wired | RED |

The Renode and QEMU `on_i2c` / `on_spi` callbacks are documented no-ops in
`crates/hauksbee-mcu/src/renode/mod.rs` and `crates/hauksbee-mcu/src/qemu/mod.rs`.
All RED tests are expected to fail until the corresponding bridge is implemented
in `crates/hauksbee-mcu`.

### 1.3 Red-test design

Tests that target an unimplemented bridge are deliberately NOT `#[ignore]`d.
They run and produce a clear failure message that:
- Names the unimplemented path (`on_spi bridge is not yet wired`).
- States the symptom (`counts == 0 always, FLAG stays LOW`).
- Pinpoints the implementation site (`hauksbee-mcu/src/renode/mod.rs` line).

This ensures the test suite goes GREEN automatically once the bridge is wired,
without requiring any test modifications.

---

## 2. I2C Tests

### 2.1 `i2c_sensor_cosim.rs` (AVR) -- PASSING

**Backend:** `avr` (simavr, ATmega328P)  
**Firmware:** `testdata/firmware/i2c_thermostat/thermostat.hex`  
**Peripheral:** `Lm75` at address `0x48`, simulated via `I2cBus`  
**Threshold:** 30 C  
**Flag net:** `FLAG` (PB0, pad 14)

**Tests:**
- `firmware_drives_gpio_from_i2c_temperature`: below 30 C -> LOW, above 30 C -> HIGH.
- `gpio_follows_temperature_sweep`: sweeps [10, 25, 29, 31, 35, 50, 28, 15] C and
  asserts each frame tracks the threshold.

**Build command:**
```
make -C testdata/firmware/i2c_thermostat
```

---

## 3. SPI Tests

### 3.1 `spi_sensor_cosim_renode.rs` (STM32 / Renode) -- RED

**Backend:** `renode` (STM32F103C8T6, `#![cfg(feature = "renode")]`)  
**Firmware:** `testdata/firmware/stm32_spi_adc/spi_adc.elf`  
**Board:** `testdata/boards/stm32_spi_adc_demo.kicad_pcb`  
**Peripheral:** `Mcp3008` (Vref = 3.3 V) on `SpiBus`  
**Threshold:** counts >= 512 (Vin >= 1.65 V)  
**Flag net:** `FLAG` (PA8, with 10k pulldown)

**Firmware operation:**
- Initialises SPI1 master (PA4=NSS, PA5=SCK, PA6=MISO, PA7=MOSI), mode 0, fPCLK/16.
- Loop: reads MCP3008 channel 0 (3-byte transfer: `0x01, 0x80, 0x00`), decodes
  the 10-bit count from the response, drives PA8 HIGH/LOW, prints `adc:<n>` over
  USART1.
- Boot banner: `"spi adc ready\r\n"`.

**Build command:**
```
make -C testdata/firmware/stm32_spi_adc
```

**Tests:**
- `stm32_spi_adc_drives_flag_below_threshold`: 0.8 V input -> FLAG LOW.  
  _Currently: PASSES (bridge no-op, counts always 0, FLAG always LOW)._
- `stm32_spi_adc_drives_flag_above_threshold`: 2.5 V input -> FLAG HIGH.  
  _Currently: FAILS (bridge no-op, counts always 0, FLAG stays LOW)._
- `stm32_spi_adc_flag_follows_voltage_sweep`: sweep [0.5..3.0] V across the
  1.65 V threshold.  
  _Currently: FAILS for the 3 above-threshold cases._

**Goes GREEN when:** `RenodeBackend::on_spi` is wired to intercept SPDR writes
and return the registered `on_spi` callback's result as MISO.

---

### 3.2 `spi_sensor_cosim_qemu.rs` (ESP32 / QEMU) -- RED

**Backend:** `qemu` (ESP32, `#![cfg(feature = "qemu")]`)  
**Firmware:** `testdata/firmware/esp32_spi_adc/flash.bin`  
**ELF:** `testdata/firmware/esp32_spi_adc/esp32_spi_adc.elf`  
**Board:** `testdata/boards/esp32_spi_adc_demo.kicad_pcb`  
**Peripheral:** `Mcp3008` (Vref = 3.3 V) on `SpiBus`  
**Threshold:** counts >= 512 (Vin >= 1.65 V)  
**Flag net:** `FLAG` (GPIO4, pad 26, role "p04", with 10k pulldown)

**Firmware operation:**
- Publishes `HAUKSBEE_MAGIC` to RTC slow RAM mailbox (same pattern as blinky).
- Configures HSPI (SPI2) master: SCLK=GPIO14, MISO=GPIO12, MOSI=GPIO13, CS=GPIO15.
- Loop: reads MCP3008 channel 0 with `spi_device_transmit`, decodes the 10-bit
  count, drives GPIO4 HIGH/LOW, mirrors GPIO state to mailbox, prints
  `adc:<n>` over UART0.
- Boot banner: `"spi adc ready\r\n"`.

**Build command:**
```
cd testdata/firmware/esp32_spi_adc
. ~/esp/esp-idf/export.sh
./build.sh
```

**Tests:**
- `esp32_spi_adc_drives_flag_below_threshold`: 0.8 V input -> FLAG LOW.  
  _Currently: PASSES (bridge no-op, counts always 0, FLAG always LOW)._
- `esp32_spi_adc_drives_flag_above_threshold`: 2.5 V input -> FLAG HIGH.  
  _Currently: FAILS (bridge no-op, counts always 0, FLAG stays LOW)._
- `esp32_spi_adc_flag_follows_voltage_sweep`: sweep [0.5..3.0] V across the
  1.65 V threshold.  
  _Currently: FAILS for the 3 above-threshold cases._
- `esp32_spi_adc_uart_announces_ready`: boots and checks for `"spi adc ready"`.  
  _Currently: PASSES (UART bridge works independently of SPI bridge)._

**Goes GREEN when:** `QemuBackend::on_spi` is wired to intercept SPI peripheral
writes and return the registered callback's MISO byte via the QEMU control
channel (QMP or gdbstub memory poke into the SPI RX register).

---

## 4. What the bridge implementation must do

To make the Renode SPI tests go GREEN:
1. In `crates/hauksbee-mcu/src/renode/mod.rs`, store the `on_spi` callback
   (currently `_cb`).
2. After each `RunFor`, detect pending SPI transfers. The STM32F1 SPI RXNE bit
   lives at `sysbus.spi1` offset `0x08` bit 0. When set, read `DR` (offset
   `0x0C`) for the MOSI byte, call the callback, and poke the result back into
   `DR` as the MISO byte before the firmware's next `SPI_SR_RXNE` poll.
3. Alternatively, use Renode's Python hook API (`sysbus.spi1 AddPeripheralBackend`)
   to intercept at the peripheral level rather than the polling the register.

To make the QEMU SPI tests go GREEN:
1. In `crates/hauksbee-mcu/src/qemu/mod.rs`, store the `on_spi` callback.
2. After each `cont/stop` chunk, poll the ESP32 HSPI (`SPI2`) peripheral state
   via QMP `human-monitor-command` or a GDB memory read to detect completed
   transfers, extract the MOSI byte, call the callback, and write MISO back.
3. The HSPI status register is at `0x3FF64000` (SPI_RD_STATUS_REG). The MOSI
   data word lives at `SPI_W0_REG` (`0x3FF64080`). MISO is injected by writing
   the same register before the firmware's next SPI read polling.

---

## 5. Test run commands

```bash
# Compile only (must succeed, all features default-on):
cargo test -p hauksbee-engine --no-run

# AVR I2C (PASSING; needs thermostat.hex built):
cargo test -p hauksbee-engine --test i2c_sensor_cosim

# STM32 SPI / Renode (2 FAIL expected until bridge wired):
cargo test -p hauksbee-engine --test spi_sensor_cosim_renode

# ESP32 SPI / QEMU (2 FAIL expected until bridge wired):
cargo test -p hauksbee-engine --test spi_sensor_cosim_qemu -- --test-threads=1
```
