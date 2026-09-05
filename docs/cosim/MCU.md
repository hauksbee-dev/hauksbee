# MCU co-simulation backends

Hauksbee boots firmware on an emulated MCU in lockstep with the solved analog
circuit. Every backend implements one trait (`hauksbee-mcu::Mcu`,
`crates/hauksbee-mcu/src/traits.rs`), so the scheduler does not care which
emulator sits behind it.

```
hauksbee run <board> --firmware <img> --headless --seconds N [--chunk-us US] [--plain|--json] [--strict] [--strict-boot]
```

## The `Mcu` trait

```
load_firmware(path)          load one .elf / .hex
run_micros(us)               advance firmware by us microseconds
set_digital_in(pin, hi)      drive an input pin
set_analog_in(ch, volts)     inject an ADC voltage
on_pin_change(cb)            callback per GPIO output edge (PinId{port,bit}, level, cycle stamp)
on_input_responder(cb)       synchronous responder: per cycle-stamped edge, returns input pins before the next instruction
on_input_responder_batch(cb) same, but all pins of one port write arrive as one atomic batch
uart_write(bytes) / on_uart(cb)
on_i2c(cb) / on_spi(cb)      intercept bus bytes, return the slave's reply
on_spi_controller(name, cb)  same, routed to one named controller
```

Status accessors: `state`, `frequency`, `reset` (AVR only), `cycle_exact`,
`pins_configured_output`, `drive_direction_observable`, `uart_rx_overflow`,
`uart_rx_pending`, `set_active_ports`, `watchdog_limitation`,
`watchdog_resets`, `timing_limitation`. `PinId { port: char, bit: u8 }`: AVR
ports `B/C/D`; STM32 `A..G` bits 0-15; nRF52 and RISC-V ports `'0'`/`'1'`
bits 0-31. Board pads map to pins through the model db's pin roles (`pc13`,
`pa5`, `pb5_sck`).

The synchronous responder lets a bit-banged readback (a `74HC165` chain read
inside a tight `digitalRead`/`digitalWrite` loop) resolve per edge on AVR;
Renode and QEMU push state once per chunk.

## Backends

| Feature | Backend | Parts | Mechanism | Links |
|---|---|---|---|---|
| `avr` | `AvrMcu` | ATmega328P (any simavr part by name; only the ATmega328P port map is auto-registered) | in-process libsimavr | libsimavr (GPL-3.0) |
| `renode` | `RenodeBackend` | STM32F072/F103/F4, nRF52840, SiFive FE310, RP2040 | headless Renode over Monitor TCP + UART socket | none |
| `qemu` | `QemuBackend` | ESP32, ESP32-S3, ESP32-C3 | Espressif QEMU over QMP + gdbstub + UART socket | none |

All three features are on by default; `--no-default-features --features
renode,qemu` is GPL-free. Install and discovery: [SIMULATORS.md](SIMULATORS.md).

### Parts are data

Each Renode/QEMU part is a `crates/hauksbee-mcu/db/mcu/<part>.soc.toml`
descriptor (platform, CPU path, per-port register offsets, UART/I2C/SPI
controller names, ISA, ADC injection recipes, `[soc.clock_control]`),
embedded in the binary and overridable from `$HAUKSBEE_MCU_DIR` or
`~/.config/hauksbee/mcu/`. A board's part reaches a backend through a
`[[models]] kind = "mcu"` routing entry whose `params.backend` is
`renode:<part>` or `qemu:<part>`. See
[add-an-mcu-variant.md](../extending/add-an-mcu-variant.md).

### Coupling coverage

| Coupling | AVR | Renode | QEMU (ESP32 family) |
|---|---|---|---|
| GPIO out | per-edge IRQ | ODR poll per chunk | real MMIO on the patched build; RAM mailbox on the prebuilt |
| GPIO in | yes | `OnGPIO` | firmware mailbox (gdbstub `M` write) |
| UART | yes | socket terminal | serial socket |
| ADC inject | exact | only where a descriptor maps channels (F072 inputs 0-7, RP2040 inputs 0-3); elsewhere DROPPED and reported | RAM-mailbox count slots |
| I2C slave | TWI decode | C# bridge on named controllers (F103 `i2c1`, F4 `i2c1`, nRF52840 `twi0`/`twi1`, RP2040 `i2c0`/`i2c1`) | RAM-mailbox cells, plus the machine's tmp105 |
| SPI slave | yes | named controllers (F103 `spi1`, F4 `spi2`/`spi3`, nRF52840 `spi2`); RP2040 none | RAM-mailbox cells |
| Drive direction | DDR hooks | dir-mapped platforms (F072/F4 MODER, F103 CRL/CRH, nRF52840 DIR, RP2040 `GPIO_OE`); FE310 direction-blind | patched build only |

A slave on a controller-less platform is recorded UNEXERCISED on every report
surface and fails a CI `peripheral` assertion. Every hole rides `--json`
(`cosim.adc_dropped`, `unexercised_buses`, `watchdog_limitations`,
`timing_limitations`, `spi_framing`, ...) and every hauksbee-ci format
([JSON_OUTPUT.md](../analysis/JSON_OUTPUT.md#cosim)). The live `hauksbee
serve` websocket is a scope, not a report; direction-blind backends list
unobservable nets in `SimFrame.unobserved_drive_nets`.

### Clock fidelity

Ratio of simulated-time rate to the part's rate; `1.00x` means `sleep_ms(20)`
costs 20 ms of virtual time.

| Backend | Ratio | How it is held |
|---|---|---|
| `simavr:atmega328p` | 1.00x, cycle-exact | by construction |
| `renode:rp2040` | 1.00x | stock pico-sdk timing |
| `renode:stm32f103`, `stm32f4_discovery`, `nrf52840` | 1.00x | `tests/clock_truth.rs` (SysTick-timed half-period) |
| `renode:sifive_fe310` | 1.00x on `mtime` (32768 Hz) | same gate, two-sided |
| `renode:stm32f072` | unmeasured | declared 8 MHz; timing claims qualified |
| `qemu:esp32/-s3/-c3` | ~0.94x, host-load dependent | `tests/qemu_clock_truth.rs`; wall-paced (icount breaks esp32 boot) |

The loader refuses a Renode descriptor whose platform clock declarations
disagree with `frequency_hz`. Gated paths are timer-paced delays (SysTick,
`mtime`); raw instruction busy-waits ride `PerformanceInMips`. The F103's
TIMx blocks stay at 72 MHz so stock HAL projects boot; every F103 run says so.

### Watchdog fidelity

| Backend | Armed and never fed |
|---|---|
| `simavr:atmega328p` | reboots at the right virtual time, repeatedly; reboots are reported (`watchdog_resets`) |
| `renode:stm32f103` | resets once, then the core does not resume |
| `renode:nrf52840` | never fires |
| `qemu:esp32*` | disabled at launch (`wdt_disable=true`), on purpose: co-sim pauses the guest every chunk |
| `renode:stm32f072`, `stm32f4_discovery`, `sifive_fe310`, `rp2040` | unverified |

## Support matrix

| Architecture | Backend | Platform | Proof |
|---|---|---|---|
| ATmega328P | `simavr:atmega328p` | libsimavr | proven end-to-end |
| STM32F072 | `renode:stm32f072` | stock `stm32f072.repl` | GPIO, UART, ADC (channels 0 and 3) proven; timing and watchdog unverified |
| STM32F103 | `renode:stm32f103` | stock, with inline clock declarations | proven: UART banner, solved LED current, PC13 toggles |
| STM32F4 Discovery | `renode:stm32f4_discovery` | stock | config shipped, not run end-to-end |
| nRF52840 | `renode:nrf52840` | stock | UART boot proven (Zephyr shell); `twi0`/`twi1`/`spi2` bridge registration verified |
| SiFive FE310 | `renode:sifive_fe310` | stock, `post_load_setup` PRCI bring-up | UART boot proven (Zephyr) |
| RP2040 | `renode:rp2040` (alias `renode:pico`) | Hauksbee's own platform, vendored C# support bundle | proven: real boot ROM into `main`, UART0, GP25 through the solver, ADC 0-3, I2C both directions; no SPI bridge, no PIO, core 1 unproven |
| ESP32 | `qemu:esp32` | Espressif fork `esp32` | proven: UART, GPIO toggle, solved LED current |
| ESP32-C3 | `qemu:esp32c3` | `esp32c3` | proven |
| ESP32-S3 | `qemu:esp32s3` | `esp32s3` | machine boots and locksteps; app proof pending an S3 flash image |
| ESP32-C6/H2 | none | not in the fork's machine list | out of scope |
| nRF5340 | none | Renode has no SPU, nRF53 IPC or network-core model | out of scope |

RP2040 costs about eight seconds of bring-up per run (Renode compiles the
bundle's 377 kB of C# on every machine creation). Bundle provenance:
`crates/hauksbee-mcu/db/mcu/rp2040/README.md`.

## How the external backends bridge

**Renode**: spawns `renode --disable-xwt --hide-log -p -P <port>`, connects a
Monitor client, brings up the machine, and steps with `emulation RunFor
"<s>"`. GPIO out: after each `RunFor`, each port's ODR is read
(`sysbus.<port> ReadDoubleWord <odr>`) and diffed into edge callbacks (STM32F1
`0x0C`, STM32F0/F4 `0x14`, nRF52 `0x4` peripheral-relative, FE310 `0x0C`,
RP2040 SIO `GPIO_OUT` at `0xD000_0010`). Direction: the dir register beside
it. GPIO in: `sysbus.<port> OnGPIO <bit> <bool>`. UART: a server socket
terminal connected to the USART. Rails: 3.3 V.

**QEMU**: spawns `qemu-system-xtensa` or `qemu-system-riscv32` with
`-machine <esp32|esp32s3|esp32c3>`, a merged flash image (`-drive
file=...,if=mtd,format=raw`; a bare app `.elf` is merged in-process with the
default bootloader and partition table), a QMP socket, a serial socket and a
gdbstub. Lockstep is QMP `cont`, a bounded window, QMP `stop`; virtual time is
credited from QEMU's own RESUME/STOP timestamps. `-icount` breaks esp32 boot
and is not used. GPIO out reads the real OUT/ENABLE registers on the patched
build, else the RTC-RAM mailbox; GPIO in pokes the `hauksbee_gpio_in` mailbox
word. The shipped `esp32s3` descriptor declares bank `'0'` only, so GPIO32+
is not observed (a warning names the missing banks).

## Firmware input

`--firmware` (and the web drop zone) resolves in three tiers:

1. a compiled `.elf` or `.hex`, as is (PlatformIO: `.pio/build/<env>/firmware.elf`; ESP-IDF: under `build/`);
2. a zip: `.pio/build/<env>/firmware.elf` outranks a stray `.elf`, which outranks a `.hex`; newest wins a tie;
3. a PlatformIO project directory (or a zip with `platformio.ini` and no built image), built with your own `pio run`; a missing `pio` or a failed build is an error, never a fallback.

Building a project executes its build scripts; only hand Hauksbee projects
you would build yourself.

## Serial attach

```bash
hauksbee run board.kicad_pcb --firmware fw.elf --serial-attach --serial-wait 30 --seconds 10
# host serial: pty on /dev/ttys006
# host serial: peer ATTACHED on /dev/ttys006
```

| Flag | Effect |
|---|---|
| `--serial-attach` | open a host endpoint onto the emulated UART and run live (needs `--firmware`) |
| `--serial-transport pty\|tcp` | `pty` (default) gives a device path unmodified tools use; `tcp` a loopback port (Windows has no pty) |
| `--serial-wait SECS` | hold at t=0 until a peer opens the port, then fail loudly if none does |
| `--serial-no-pace` | free-run before the first peer; afterwards bytes arrive on a fixed compressed wall/sim schedule |
| `--serial-mcu REF` | which MCU's UART on a multi-MCU board (default all) |

Sim time is paced to wall-clock by default. Output before attach is held
(64 KiB) and flushed on attach. Attach/detach transitions and byte counts are
narrated; a session nobody attached to is reported as such. Bytes are
transparent (NUL, `0x0A`, `0x0D` pass); no baud emulation, no modem-control
lines. `--serial-attach` prints its own summary, not the co-sim table.

## Recipes

| Board | Firmware | Backend | Notes |
|---|---|---|---|
| `testdata/boards/stm32_bluepill_demo.kicad_pcb` | `testdata/firmware/stm32_blinky/` (`make`, `arm-none-eabi-gcc`) | `renode:stm32f103` | PA5 -> R1 -> LED, PC13 blink, USART1 banner `hello from stm32`; `cargo test -p hauksbee-engine --test stm32_renode_cosim` |
| `testdata/boards/esp32_devkit_demo.kicad_pcb` | `testdata/firmware/esp32_blinky/` (esp-idf v5.4, `build.sh` -> `flash.bin`) | `qemu:esp32` | GPIO2 -> LED, GPIO4 blink, UART0; `--test esp32_qemu_cosim` |
| `testdata/boards/esp32c3_devkit_demo.kicad_pcb` | the same app for C3 (`flash_c3.bin`) | `qemu:esp32c3` | |
| `testdata/firmware/renode_demos/nrf52840-zephyr_shell.board` | `nrf52840-zephyr_shell.elf` | `renode:nrf52840` | `hauksbee run <board> --firmware <elf> --headless --seconds 2` boots to `uart:~$` |
| `testdata/firmware/rp2040_blink_uart/` | pico-sdk 2.1.1 | `renode:rp2040` | `tests/renode_rp2040*.rs` |
| `testdata/firmware/stm32f072_blinky/` | `blinky.elf` / `quiet.elf` | `renode:stm32f072` | `cargo test -p hauksbee-mcu --test renode_stm32f072` |

A new part needs a routing entry (`crates/hauksbee-models/db/mcu.toml` is the
template; `nrf52840` and `fe310` are worked examples with `p0<bit>` roles):

```toml
[[models]]
id = "mynewpart"
kind = "mcu"
[models.match]
value_re = "(?i)^MYNEWPART"
[models.params]
backend = "renode:mynewpart"
[models.pins]
"1" = "vss"
"2" = "vdd"
"3" = "p013"       # LED on P0.13
"4" = "p006"       # UART TXD
"5" = "p008"       # UART RXD
```

## Limits

- GPIO on Renode and QEMU is polled once per chunk; edges faster than the
  chunk alias. Use `--chunk-us` or a `[timing]` contract
  ([CI.md](../ci/CI.md#timing)) and match firmware switching rates to the chunk.
- Each Renode `RunFor` and register read is a TCP round trip; long runs with
  many polled ports are round-trip bound.
- One firmware image per board is loaded onto every MCU.
- A crystal is bound `Ignore` (the clock comes from the MCU model); its load
  caps stay. Board-wide all-zero voltages usually mean a passive bound with
  an absurd value: check `--report`.
- QEMU: SAR ADC, I2C and SPI are firmware mailbox contracts; unmodified vendor
  bus traffic is invisible. WiFi/BT, RMT, I2S, LEDC/MCPWM, touch and Hall
  sensors are not modelled, so firmware that blocks on them may not reach
  `app_main`.
- Watchdog and timing gaps per part are listed above and reported per run.
