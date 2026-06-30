# MCU co-simulation backends

Hauksbee co-simulates emulated microcontroller firmware against the solved analog
circuit. Every backend presents the same lockstep contract to the engine
(`hauksbee-mcu::Mcu`): run N microseconds of firmware, exchange GPIO pin states,
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

Three backends live in `hauksbee-mcu`, each behind a cargo feature (all on by
default):

| Feature  | Backend         | Parts                              | Mechanism                                            | Links |
|----------|-----------------|------------------------------------|------------------------------------------------------|-------|
| `avr`    | `AvrMcu`        | ATmega328P / Arduino                | in-process libsimavr via FFI                         | libsimavr (GPL-3.0) |
| `renode` | `RenodeBackend` | STM32 / nRF52 / SiFive RISC-V       | external headless Renode over Monitor TCP + UART socket | nothing native (sockets only) |
| `qemu`   | `QemuBackend`   | ESP32 / ESP32-S3 / ESP32-C3         | external Espressif QEMU over QMP + gdbstub + UART socket | nothing native (sockets only) |

A `--no-default-features --features renode,qemu` build is GPL-free: both the
Renode and QEMU backends talk to their emulator over TCP and spawn it as a child
process, so they link no GPL code.

### Why QEMU (the Espressif fork) for the ESP32 family

Renode (as of 1.16.1) ships **no** `esp32.repl` / `esp32c3.repl` (verified: the
portable distribution's `platforms/cpus/` has nrf52840 and sifive-fe310 but no
esp32 of any kind). Neither the Xtensa ESP32/S3 nor the RISC-V ESP32-C3 has a
turnkey Renode platform. Espressif maintains a QEMU fork with the ESP32 SoC
peripherals modelled (GPIO matrix, UART, SPI-flash controller, timers) and
publishes **native macOS-arm64 / Linux prebuilt binaries**. So the ESP32 path is
a separate, backend-pluggable QEMU backend, which is exactly why the engine's
backend dispatch is pluggable (`qemu:<part>` alongside `renode:<part>`).

### QEMU lockstep mechanism (chosen by measurement, not assumption)

The contract is the same as Renode's: advance a bounded amount of guest virtual
time, block until done, then exchange GPIO/UART. QEMU is driven with **QMP
`cont` -> bounded wall window -> QMP `stop`**, the QMP analogue of Renode's
`RunFor`.

The theoretically ideal primitive is `-icount shift=N`, which makes virtual time
a deterministic function of executed instructions (bit-exact reproducibility).
**We tested it and it does not work on the Espressif esp32 (Xtensa) machine:**
with `-icount` at any shift (4/6/8/auto), with or without `sleep=off`, the esp32
machine produces ZERO UART output in a 15 s wall window, versus ~1 s to the
"hello from esp32" banner with no icount. icount on these Xtensa machines is
undocumented by Espressif and empirically breaks boot, so it is off.

Determinism without icount comes from the guest's own deterministic peripheral
timers (FreeRTOS tick, UART baud generator), sampled only at chunk boundaries.
The integration test asserts this directly: across repeated runs the boot banner
is identical and the GPIO toggle count is stable to within a chunk or two. That
is the same standard a logic analyser sampling a real board at the chunk rate
meets. Alternatives rejected: qtest `clock_step` (replaces the accelerator, can't
boot a real flash image through TCG); gdbstub single-stepping (millions of steps
per chunk over RSP is far too slow).

### How the QEMU backend bridges each path

- **Process**: `QemuBackend::new(config, flash_image)` spawns
  `qemu-system-xtensa` (ESP32/S3) or `qemu-system-riscv32` (ESP32-C3) headless
  with `-machine <esp32|esp32s3|esp32c3>`, boots the merged flash image
  (`-drive file=...,if=mtd,format=raw`), and opens a QMP control socket, a serial
  socket, and (best-effort) a gdbstub. It is killed on drop. The binary is found
  via `$HAUKSBEE_QEMU_XTENSA` / `$HAUKSBEE_QEMU_RISCV32`, then `$HAUKSBEE_QEMU_DIR`,
  then `~/.galvani-qemu-esp/qemu/bin/`, then the esp-idf idf_tools install, then
  `PATH` (rejecting Homebrew's mainline qemu, which has no esp32 machine). If none
  is found, instantiation fails with a clear install message (tests skip).

- **GPIO out (poll) via a RAM mailbox**: the Espressif QEMU `esp32.gpio` model
  does **not** implement read-back of `GPIO_OUT_REG` (a host read over QMP `xp`
  or the gdbstub returns 0 regardless of the driven level; verified empirically;
  RAM, by contrast, round-trips exactly). So the demo firmware mirrors its GPIO
  output word to a fixed RAM mailbox (RTC slow memory, `0x5000_0000`), and the
  backend reads THAT word each chunk, diffs it, and synthesises per-bit edges.
  The bit layout matches `GPIO_OUT_REG`, so the edge synthesis is byte-for-byte
  the Renode ODR-poll, only at a RAM address. The real `gpio_set_level` writes
  still happen; the mailbox is only the observation path the model lacks.

- **GPIO in (push)**: the backend pokes the mailbox `hauksbee_gpio_in` word over
  the gdbstub `M` packet; the firmware reads it where it would read `GPIO_IN_REG`.

- **UART (bidirectional)**: `-serial tcp:...,server,nowait` exposes UART0 as a
  raw socket, bridged exactly like the Renode backend's UART.

- **Rails**: ESP32 parts are 3.3 V, selected by the `qemu:` backend prefix.

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
  drop. Renode is located via `$HAUKSBEE_RENODE`, then `renode` on `PATH`, then
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

A row is **Proven** only if it was actually run end-to-end on this branch (a real
emulator booting real firmware against the solved circuit, with the output
recorded). Anything not run for real is labelled honestly.

| Architecture | Backend | Emulator / platform | Proof run on this branch |
|--------------|---------|---------------------|--------------------------|
| ATmega328P (AVR) | `simavr:atmega328p` | libsimavr (in-process) | **Proven** (pre-existing AVR demo) |
| STM32F103 (Cortex-M3, blue pill) | `renode:stm32f103` | Renode `stm32f103.repl` | **Proven**: "hello from stm32", R1 current via solver, PC13 toggles |
| ESP32 (Xtensa LX6) | `qemu:esp32` | Espressif QEMU `esp32` | **Proven**: "hello from esp32", R1 = 3.727 mA via solver, GPIO4 27 toggles, run-to-run stable |
| ESP32-C3 (RISC-V RV32IMC) | `qemu:esp32c3` | Espressif QEMU `esp32c3` | **Proven**: "hello from esp32", R1 = 3.727 mA via solver, GPIO4 32 toggles |
| nRF52840 (Cortex-M4) | `renode:nrf52840` | Renode `nrf52840.repl` | **Proven (UART boot)**: Zephyr shell `uart:~$` through the bridge. Solved-LED-current proof would need a custom blinky ELF (the GPIO bridge is the STM32-proven ODR-poll). |
| SiFive FE310 (RISC-V RV32, HiFive1) | `renode:sifive_fe310` | Renode `sifive-fe310.repl` | **Proven (UART boot)**: "BOOTING ZEPHYR OS ... shell>" through the bridge (needs `post_load_setup`: PRCI tags + `cpu PC vinit`). |
| STM32F4 Discovery (Cortex-M4) | `renode:stm32f4_discovery` | Renode `stm32f4_discovery.repl` | Config shipped; platform present; not run on this branch |
| ESP32-S3 (Xtensa LX7) | `qemu:esp32s3` | Espressif QEMU `esp32s3` | Config shipped; machine present in the fork; not run on this branch (no S3 firmware built) |
| ESP32-C6, ESP32-H2 | — | — | Not in the Espressif QEMU fork's machine list; out of scope |
| nRF5340 (ZSWatch-class) | — | — | See note below |

### nRF5340 / ZSWatch, honestly

ZSWatch is an **nRF5340**, not the nRF52840 proven above. The Renode 1.16.1
portable distribution ships `platforms/cpus/nrf52840.repl` but **no** nRF5340
platform of any kind (verified: zero `*nrf5340*` files in the build). So nRF5340
is NOT proven and no config is claimed for it. The nRF52840 proof is the closest
Nordic part proven; a ZSWatch-class nRF5340 board would need an nRF5340 Renode
platform (upstream Renode carries some nRF5340 work, but it is not in this
portable build and was not run).

### ESP32 in Renode, honestly

ESP32 remains **not usable in Renode** as of 1.16.1: no `esp32.repl` /
`esp32c3.repl` ships, and the ESP32 SoC peripheral set is unmodelled. That gap is
exactly why the ESP32 family is served by the `qemu:` backend (Espressif QEMU
fork) rather than Renode. Both Xtensa (ESP32) and RISC-V (ESP32-C3) ESP32 parts
are proven through QEMU above.

### Co-sim fidelity notes (debugging all-zero or "never driven" results)

- **Crystal-clocked boards.** A crystal/resonator is bound high-impedance
  (`ComponentKind::Ignore`); the clock comes from the MCU model. Before this fix
  a crystal valued `16Mhz` (or any `C`-referenced one) was mis-bound as a
  16-gigafarad capacitor that made the solve singular and drove **every** net to
  0 V / "never driven". If you see board-wide all-zero co-sim voltages, confirm
  you are on a build with `39128bb`+ and check `--report` for any passive with an
  absurd capacitance. See [`LIMITATIONS.md`](LIMITATIONS.md) Fixed #4.
- **ESP32 GPIO needs the firmware mailbox** (stock third-party firmware is
  GPIO-invisible). The exact, empirically-validated reason and the QEMU-fork
  patch that would remove the requirement are in
  [`hunts/esp32-qemu-i2c-status.md`](hunts/esp32-qemu-i2c-status.md).

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
#   ~/renode-portable/  (or put `renode` on PATH, or set HAUKSBEE_RENODE).

# run the co-sim (the integration test does exactly this)
cargo test -p hauksbee-engine --test stm32_renode_cosim -- --nocapture
```

The model db entry (`db/mcu.toml`, id `stm32f103c8`) maps the part to
`backend = "renode:stm32f103"` and carries the LQFP-48 pin map (PC13, PA5, PA9/10,
power). The scheduler's `instantiate_renode` turns `renode:stm32f103` into
`RenodeConfig::stm32f103()`.

### ESP32 (the proven QEMU demo)

Board: `testdata/boards/esp32_devkit_demo.kicad_pcb` — U1 ESP32-WROOM-32, GPIO2 ->
330 Ohm R1 -> LED -> GND (the analog current path the solver computes), GPIO4 ->
4k7 -> GND (the blink indicator), UART0 on the module's U0TXD/U0RXD.

Firmware: `testdata/firmware/esp32_blinky/` — esp-idf C app. Drives GPIO2 HIGH at
boot, toggles GPIO4 at ~5 Hz, prints `hello from esp32` on UART0 and answers
`i`/`v` commands. Because the Espressif QEMU `esp32.gpio` model does not expose
GPIO output register read-back, the firmware mirrors its GPIO output word to a
fixed RAM mailbox (`0x5000_0000`, RTC slow memory) that the backend reads; the
real `gpio_set_level` writes still happen (see the firmware header and the
limitations section).

Install (two pieces, both native macOS-arm64 / Linux):

```
# 1. Espressif QEMU fork binary (small, ~4 MB; no esp-idf needed for it):
#    grab the prebuilt release and unpack to ~/.galvani-qemu-esp/qemu
#    https://github.com/espressif/qemu/releases   (qemu-xtensa-softmmu-... and
#    qemu-riscv32-softmmu-... for the C3), or:
#    python $IDF_PATH/tools/idf_tools.py install qemu-xtensa qemu-riscv32
#    (the backend also honours $HAUKSBEE_QEMU_XTENSA / $HAUKSBEE_QEMU_RISCV32.)
#    NOTE: Homebrew's mainline qemu-system-xtensa has NO esp32 machine and is
#    rejected by the discovery (it checks `-machine help` for esp32).

# 2. esp-idf, only to BUILD the firmware image (~3-5 GB, one-time):
git clone --depth=1 --branch v5.4 https://github.com/espressif/esp-idf.git ~/esp/esp-idf
cd ~/esp/esp-idf && git submodule update --init --depth=1 && ./install.sh esp32,esp32c3
. ~/esp/esp-idf/export.sh
cd testdata/firmware/esp32_blinky && ./build.sh   # produces flash.bin

# run the co-sim (the integration test does exactly this)
cargo test -p hauksbee-engine --test esp32_qemu_cosim -- --nocapture
```

The committed `flash.bin` (merged bootloader + partition table + app) lets the
test run without rebuilding; `build.sh` regenerates it. The model db entry
(`db/mcu.toml`, id `esp32_wroom`) maps the part to `backend = "qemu:esp32"`; the
scheduler's `instantiate_qemu` turns `qemu:esp32` into `QemuConfig::esp32()` and
passes the flash image (the engine's "firmware" path) to QEMU to boot.

### ESP32-C3 (RISC-V, same QEMU backend)

Board: `testdata/boards/esp32c3_devkit_demo.kicad_pcb`; firmware: the same
`esp32_blinky` app rebuilt for the C3 target (`idf.py -B build_c3 set-target
esp32c3 build`, merged to `flash_c3.bin`). Backend `qemu:esp32c3` uses
`qemu-system-riscv32 -machine esp32c3`. Proven identically to the ESP32 above.

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

### QEMU (ESP32 family) specific limitations

- **GPIO output is observed through a RAM mailbox, not the GPIO register.** The
  Espressif QEMU `esp32.gpio` model does not implement read-back of
  `GPIO_OUT_REG` (a host read returns 0 regardless of the driven level; writes
  to `GPIO_IN_REG` are dropped; RAM round-trips fine). So the demo firmware
  mirrors its GPIO output word to a fixed RAM mailbox the backend reads, and
  reads injected inputs from it. Consequence: **GPIO co-sim requires
  mailbox-aware firmware** (the committed demo is). Arbitrary third-party ESP32
  firmware would boot and produce UART, but its GPIO output would not be visible
  to the solver unless it maintained the mailbox. A cleaner future fix is a small
  patch to the fork's gpio model to honour register read-back, which would remove
  the mailbox requirement entirely.

- **No `-icount` on the Xtensa esp32 machine.** Measured: `-icount` (any shift,
  with/without `sleep=off`) prevents the esp32/esp32s3 machines from booting
  (zero UART in 15 s). The lockstep therefore uses QMP stop/cont over the
  free-running virtual clock; timing is wall-bounded, not instruction-counted, so
  determinism is logic-level (stable banner + toggle count) rather than bit-exact.
  The RISC-V esp32c3 machine tolerates icount per Espressif's docs, but the
  backend keeps the same icount-free mechanism for uniformity.

- **ESP32 ADC (SAR) is not modelled** by the QEMU fork, so `set_analog_in` is a
  documented no-op (as for Renode). The demo couples through the GPIO/LED path.

- **No I2C/SPI interception** for the QEMU backend (as for Renode).

- **Control round-trip cost.** Each chunk is a QMP cont/stop pair plus a mailbox
  read; the wall window per chunk floors at 8 ms so boot clears in a reasonable
  number of chunks. Coarse co-sim chunks (a few ms) keep the round-trip count
  modest, as with Renode.

- **Unmodelled ESP32 peripherals.** The fork models GPIO matrix, UART, SPI-flash,
  and timers, but WiFi/BT radio, RMT, I2S, the LEDC/MCPWM generators, and the
  touch/Hall sensors are not (or only partially) modelled. Firmware that blocks on
  one of those at boot may not reach `app_main`; the demo avoids them.
