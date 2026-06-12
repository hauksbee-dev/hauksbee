# MCU co-simulation backends

Galvani co-simulates emulated microcontroller firmware against the solved analog
circuit. Every backend presents the same lockstep contract to the engine
(`galvani-mcu::Mcu`): run N microseconds of firmware, exchange GPIO pin states,
ADC voltages, and UART bytes. The scheduler does not care which emulator is
behind the trait, so adding an architecture is adding a backend, not touching the
co-sim loop.

## The `Mcu` trait (the uniform contract)

```
run_micros(us)            advance firmware by us microseconds (lockstep step)
set_digital_in(pin, hi)   drive an external input pin
set_analog_in(ch, volts)  inject an ADC voltage
on_pin_change(cb)         callback per GPIO output edge: (PinId{port,bit}, level)
uart_write(bytes)         inject UART RX bytes
on_uart(cb)               callback per UART TX byte
```

`PinId { port: char, bit: u8 }` is the generic pin address. AVR uses port letters
`B/C/D`; STM32 uses `A`..`G` with bit `0..15`; nRF52 and RISC-V use port `'0'`/`'1'`
with bit `0..31`. The engine maps a board's pads to these via the model db's pin
roles (e.g. `pc13`, `pa5`, `pb5_sck`).

## Backends

Two backends live in `galvani-mcu`, each behind a cargo feature (both on by
default):

| Feature  | Backend         | Mechanism                                            | Links |
|----------|-----------------|-----------------------------------------------------|-------|
| `avr`    | `AvrMcu`        | in-process libsimavr via FFI                         | libsimavr (GPL-3.0) |
| `renode` | `RenodeBackend` | external headless Renode over Monitor TCP + UART socket | nothing native (sockets only) |

A `--no-default-features --features renode` build is GPL-free: the Renode backend
talks to Renode over TCP and spawns it as a child process, so it links no GPL code.

### Why Renode for non-AVR

Renode (Antmicro, open source) ships faithful models for STM32 families, nRF52,
and many RISC-V machines, with **precise virtual-time control**. The single
primitive that makes tight lockstep work is:

```
emulation RunFor "0.0001"
```

`RunFor` advances virtual time by exactly the given interval and blocks until it
elapses, then pauses. That is the co-sim step: advance the firmware a bounded
amount, exchange pin/UART state, solve the analog chunk, repeat. With
`emulation SetGlobalAdvanceImmediately true` Renode runs at host speed rather
than pacing to wall-clock, which is what we want when the analog solver sets the
pace. Renode's virtual-time resolution is 1 ns.

QEMU and unicorn were considered as fallbacks. They were not needed: QEMU's
`-icount` gives deterministic time but no clean bounded "run for exactly T then
stop and let me poke peripherals" loop over a stable control socket, and unicorn
is a raw CPU emulator with no peripheral models (no USART, no GPIO blocks), so the
firmware's `printf`/blink would have nothing to drive. Renode gives both the time
control and the peripheral models, so the firmware runs unmodified.

### How the Renode backend bridges each path

- **Process**: `RenodeBackend::new(config)` spawns `renode --disable-xwt
  --hide-log -p -P <port>` headless, connects a Monitor TCP client, brings up the
  machine from the config, and connects a UART socket terminal. It is killed on
  drop. Renode is located via `$GALVANI_RENODE`, then `renode` on `PATH`, then
  `~/renode-portable/...`. If none is found, instantiation fails with a clear
  install message (tests skip; they do not silently fall back to AVR).

- **GPIO out (poll)**: Renode has no generic "read pin" Monitor command, but a
  GPIO peripheral's output-data register is memory-mapped, so after each `RunFor`
  the backend reads each port's ODR with `sysbus.<port> ReadDoubleWord <odr>`,
  diffs it against the previous snapshot, and synthesises per-bit edge callbacks.
  This mirrors exactly how the simavr backend's port hook detects bit edges, so
  the scheduler sees identical behaviour. ODR offsets per family: STM32F1 `0x0C`,
  STM32F4 `0x14`, nRF52 `0x504`, FE310 `0x0C`.

- **GPIO in (push)**: `sysbus.<port> OnGPIO <bit> <bool>` drives an input pin.

- **UART (bidirectional)**: `emulation CreateServerSocketTerminal <port> "t" false`
  + `connector Connect sysbus.<usart> t` exposes the UART as a raw TCP socket.
  Bytes the firmware transmits arrive on the socket; bytes written to the socket
  are injected into the UART receiver. The trailing `false` disables Renode's
  terminal config handshake so the stream is raw both ways.

- **Rails**: the engine drives STM32-class GPIO outputs at 3.3 V (vs 5 V for
  classic AVR), selected by the `renode:` backend prefix.

## Per-architecture support matrix

Status as observed against Renode 1.16.1 with the platform files that ship in the
portable distribution.

| Architecture | Renode machine / platform file              | Backend config            | GPIO ODR | UART      | Status |
|--------------|---------------------------------------------|---------------------------|----------|-----------|--------|
| STM32F103 (Cortex-M3, blue pill) | `platforms/cpus/stm32f103.repl`   | `RenodeConfig::stm32f103()`        | 0x0C | usart1 | **Proven end-to-end** (see below) |
| STM32F4 Discovery (Cortex-M4) | `platforms/boards/stm32f4_discovery.repl` | `RenodeConfig::stm32f4_discovery()` | 0x14 | usart2 | Config shipped; platform present |
| nRF52840 (Cortex-M4) | `platforms/cpus/nrf52840.repl`              | `RenodeConfig::nrf52840()`         | 0x504 | uart0  | Config shipped; platform present |
| SiFive FE310 (RISC-V RV32, HiFive1) | `platforms/cpus/sifive-fe310.repl`  | `RenodeConfig::sifive_fe310()`     | 0x0C | uart0  | Config shipped; platform present |
| ESP32 (Xtensa) | — | — | — | — | **Not viable in Renode today** (see below) |

"Config shipped" means the `RenodeConfig` and the Renode platform are present and
the backend brings the machine up over the same code path proven for STM32F103;
running one needs a matching firmware ELF and a board whose model maps its part to
the `renode:<part>` backend.

### ESP32, honestly

ESP32 is **not usable in Renode** as of 1.16.1. The Xtensa ISA decoder exists
(added for Sound Open Firmware DSP work) but there is no `esp32.repl` /
`xtensa_lx6.repl` / `esp32c3.repl` in `platforms/cpus/`, and the ESP32 SoC
peripheral set (WiFi, BT, RMT, I2S, GDMA) is unmodeled. Neither the Xtensa
ESP32 (LX6/LX7) nor the RISC-V ESP32-C3/C6 have turnkey platforms in mainline
Antmicro Renode. Espressif maintains some out-of-tree work; nothing here depends
on it. For an ESP32 product today the realistic path is QEMU's Espressif fork (a
separate backend, not Renode), which is why the architecture is backend-pluggable.

## Recipes

### STM32F103 blue pill (the proven demo)

Board: `testdata/boards/stm32_bluepill_demo.kicad_pcb` — U1 STM32F103C8, PA5 ->
330 Ohm R1 -> LED -> GND (the analog current path the solver computes), PC13 ->
4k7 -> GND (the blink indicator), USART1 on PA9/PA10.

Firmware: `testdata/firmware/stm32_blinky/` — bare-metal C (no vendor SDK), builds
with `arm-none-eabi-gcc`. Blinks PC13 at ~5 Hz, drives PA5 HIGH at boot, prints
`hello from stm32` on USART1 at boot and answers `i`/`v` commands.

```
# build firmware (once)
brew install arm-none-eabi-gcc
make -C testdata/firmware/stm32_blinky

# install Renode (portable, no system dotnet/mono needed on Apple Silicon)
#   download renode-<ver>-dotnet.osx-arm64-portable.dmg from
#   https://github.com/renode/renode/releases , mount it, and copy Renode.app to
#   ~/renode-portable/  (or put `renode` on PATH, or set GALVANI_RENODE).

# run the co-sim (the integration test does exactly this)
cargo test -p galvani-engine --test stm32_renode_cosim -- --nocapture
```

The model db entry (`db/mcu.toml`, id `stm32f103c8`) maps the part to
`backend = "renode:stm32f103"` and carries the LQFP-48 pin map (PC13, PA5, PA9/10,
power). The scheduler's `instantiate_renode` turns `renode:stm32f103` into
`RenodeConfig::stm32f103()`.

### nRF52840 (recipe, same path)

Model entry (add to `db/mcu.toml`):

```toml
[[models]]
id = "nrf52840"
kind = "mcu"
[models.match]
value_re = "(?i)^nRF52840"
[models.params]
backend = "renode:nrf52840"
[models.pins]
# nRF GPIO is two 32-bit ports; roles "p0<bit>" and "p1<bit>".
"1" = "p013"   # e.g. an LED on P0.13 (nRF52840-DK LED1)
"2" = "vdd"
"3" = "vss"
"4" = "p006"   # UART TXD on the DK
"5" = "p008"   # UART RXD on the DK
```

Backend: `RenodeConfig::nrf52840()` (ports gpio0/gpio1, OUT register at 0x504,
uart0). Firmware: any nRF52840 ELF (e.g. a Zephyr blinky). Renode ships
`platforms/cpus/nrf52840.repl` and `platforms/boards/nrf52840dk_nrf52840.repl`.

### SiFive FE310 / HiFive1 (RISC-V, recipe, same path)

```toml
[[models]]
id = "fe310"
kind = "mcu"
[models.match]
value_re = "(?i)^(FE310|HiFive1)"
[models.params]
backend = "renode:sifive_fe310"
[models.pins]
"1" = "p019"   # HiFive1 LED is on GPIO 19
"2" = "vdd"
"3" = "vss"
"4" = "p017"   # uart0 TX
"5" = "p016"   # uart0 RX
```

Backend: `RenodeConfig::sifive_fe310()` (one 32-bit gpio0, output value register
at 0x0C, uart0). Renode ships `platforms/cpus/sifive-fe310.repl` and
`scripts/single-node/sifive_fe310.resc` with a prebuilt demo ELF reference. This
exercises the identical Monitor/UART/RunFor path as STM32 on a RISC-V core,
proving the backend is ISA-agnostic.

## Limitations

- **GPIO is polled, not interrupt-driven on the host.** The backend reads each
  configured port's ODR once per `run_micros` chunk. Edges faster than the chunk
  alias, exactly like a real logic analyser sampling at the chunk rate. Match the
  firmware's switching rate to the analog chunk size (the demo blinks at ~5 Hz vs
  ~50-100 us chunks, comfortably oversampled). Bit-banged MHz signals are not
  resolved by the poll bridge; they would need the binary external-control GPIO
  event channel (future work, see below).

- **Monitor round-trip cost.** Each `RunFor` and each ODR read is a TCP
  request/response. A long co-sim with many ports polled every chunk spends most
  of its wall time in monitor round-trips, not emulation. Mitigations not yet
  implemented: poll only ports that have bound drivers, batch reads with a single
  `-e`-style multi-command, or move to Renode's binary ExternalControlServer
  (`GPIOPort GetState/SetState/RegisterEvent`, `RunFor`, `GetTime`) which avoids
  ASCII framing entirely.

- **ADC injection is a no-op for the Renode backend.** Renode's ADC peripheral
  API is per-SoC (`FeedSample` / `SetDefaultValue` vary by family), so
  `set_analog_in` is documented as a no-op until a per-platform ADC map is added.
  The STM32 demo couples through the GPIO/LED path, not the ADC, so it is
  unaffected. The AVR backend's ADC injection is fully wired.

- **I2C/SPI interception is not wired for Renode** (it is for AVR). The hooks
  exist on the trait and return cleanly; peripheral-bus interception over the
  Monitor is future work.

- **One firmware per machine.** The engine loads one firmware ELF/HEX for all
  MCUs on a board, matching the existing AVR behaviour.
