# MCU co-simulation backends

Hauksbee co-simulates emulated microcontroller firmware against the solved
analog circuit. Every backend presents the same lockstep contract to the
engine (`hauksbee-mcu::Mcu`): run N microseconds of firmware, exchange GPIO
pin states, ADC voltages, and UART bytes. The scheduler does not care which
emulator sits behind the trait, so adding an architecture means adding a
backend, not touching the co-sim loop.

## The `Mcu` trait (the uniform contract)

```
run_micros(us)            advance firmware by us microseconds (lockstep step)
set_digital_in(pin, hi)   drive an external input pin
set_analog_in(ch, volts)  inject an ADC voltage
on_pin_change(cb)         callback per GPIO output edge: (PinId{port,bit}, level)
on_input_responder(cb)    SYNCHRONOUS responder: per output edge, returns input
                          pins to drive immediately (before the next instruction)
uart_write(bytes)         inject UART RX bytes
on_uart(cb)               callback per UART TX byte
```

### `on_input_responder`, closing a readback inside the firmware's bit-bang loop

`on_pin_change` reports output edges, and the scheduler can collapse them and
react on the *next* analog chunk. That is too coarse for a firmware that
bit-bangs a clock and `digitalRead`s the resulting serial-out bit in the SAME
tight loop, e.g. the Tarski `_ReadShiftRegisterWord`, which for 16 bits does
`digitalRead(MISO)` then pulses SCLK with back-to-back, sub-µs
`digitalWrite`s. By the time the chunk ends, the firmware has already
finished reading. Injecting MISO once per chunk (the old behaviour) reads
`0x0000`.

`on_input_responder` fixes this: the AVR backend invokes it from the same
per-port output hook that fires `on_pin_change`, and it raises the pins it
returns onto their ioport input IRQs *synchronously*, before the firmware's
next instruction. The engine installs an edge-driven `Hc165Chain` here: on
the PL falling edge it latches the parallel inputs (the spike latches) into
a QH-emit bit sequence, and on each SCLK rising edge it presents the next
bit on MISO. This is the read-direction analogue of the edge-driven
`Hc595Chain` write path. Both resolve the bit-banged clock per edge, not per
chunk. Renode/QEMU keep the default no-op (they push state once per chunk).

`PinId { port: char, bit: u8 }` is the generic pin address. AVR uses port
letters `B/C/D`; STM32 uses `A`..`G` with bit `0..15`; nRF52 and RISC-V use
port `'0'`/`'1'` with bit `0..31`. The engine maps a board's pads to these
through the model db's pin roles (e.g. `pc13`, `pa5`, `pb5_sck`).

## Backends

Three backends live in `hauksbee-mcu`, each behind a cargo feature (all on
by default):

| Feature  | Backend         | Parts                              | Mechanism                                            | Links |
|----------|-----------------|------------------------------------|------------------------------------------------------|-------|
| `avr`    | `AvrMcu`        | ATmega328P / Arduino                | in-process libsimavr via FFI                         | libsimavr (GPL-3.0) |
| `renode` | `RenodeBackend` | STM32 / nRF52 / SiFive RISC-V       | external headless Renode over Monitor TCP + UART socket | nothing native (sockets only) |
| `qemu`   | `QemuBackend`   | ESP32 / ESP32-S3 / ESP32-C3         | external Espressif QEMU over QMP + gdbstub + UART socket | nothing native (sockets only) |

A `--no-default-features --features renode,qemu` build is GPL-free: both the
Renode and QEMU backends talk to their emulator over TCP and spawn it as a
child process, so they link no GPL code.

### Parts are data, not code (`db/mcu/*.soc.toml`)

A *part* (an STM32F103, an ESP32-C3) is a reviewed descriptor file, not a
hand-written Rust constructor. Each lives under
`crates/hauksbee-mcu/db/mcu/<part>.soc.toml`, loaded through one validated
path (`RenodeConfig::from_soc_toml` / `QemuConfig::from_soc_toml`) with
fail-loud, named errors, the same discipline as `sensor_spec.rs`. The
descriptor carries everything a backend needs to drive the part: the
platform `.repl` path, the CPU path, the per-port register offsets (the
F1-vs-F4 ODR footgun lives here as `odr_offset` data, not scattered logic),
the UART/I2C/SPI controller names, the expected ISA
(`expected_e_machine = "EM_ARM"`), and the awkward per-part fixups
(`post_load_setup` for the FE310 PRCI/`vinit` bring-up, `[soc.spi].extra_repl`
for the F103 SPI1 fragment, `[[soc.adc]]` injection recipes). The shipped
descriptors are embedded through `include_str!` (the binary stays
self-contained, and the file stays the single source of truth), and
`RenodeConfig::stm32f103()` and friends are thin accessors that load their
descriptor. Validation refuses rather than fakes: unknown/mismatched
backend, empty `platform_repl`, zero-width or overlapping ports, duplicate
controllers, unknown `expected_e_machine`, ambiguous ADC inject, and (through
`deny_unknown_fields`) any mistyped field are all named load-time errors.

**Add an MCU variant (purely as data, no recompile).** Copy the closest
`db/mcu/<part>.soc.toml`, edit the fields (platform `.repl`, ODR offsets,
UART, ISA), drop it in `$HAUKSBEE_MCU_DIR` or `~/.config/hauksbee/mcu/`, and
add a `[[models]] kind = "mcu"` routing entry naming `renode:<yourpart>`
(the recipe pattern below). The scheduler's backend instantiation resolves
every `renode:<part>` / `qemu:<part>` string through `SocConfig::resolve`,
so the override directories are the product path, not just a library
function: an override directory wins over a built-in of the same name, and
an *invalid* override for a part in use aborts the run naming the file and
field, never a silent fallback to the built-in. That is the acceptance bar:
a new Renode MCU added without touching hauksbee's source. The equivalence
and validation tests live in `crates/hauksbee-mcu/tests/soc_descriptors.rs`,
and the product-path wiring test lives in `hauksbee-engine`'s
`soc_wiring_tests`. `docs/extending/add-an-mcu-variant.md` is the full
worked walkthrough, with a real boot transcript. `hauksbee models list
--builtin` enumerates the shipped descriptors.

**What honestly stays Rust.** A descriptor only *configures* one of the
three existing backends. A wholly new emulator backend is a new `Mcu` trait
implementation, not a descriptor. And simavr part support is not data here
either: simavr's own part database does the work, and the descriptor would
only name the part. Those two remain code changes by design.

### Peripheral-coupling coverage (what each backend actually implements)

The `Mcu` trait above is the full contract, and all three backends now
implement every coupling, at different fidelity tiers, stated honestly
below. GPIO (both directions) and UART co-sim work identically on all
three; ADC injection and I2C/SPI byte interception are exact on the
in-process AVR backend and bridged/contracted on the external emulators.

| Coupling | AVR (`simavr`) | Renode (STM32/nRF/RISC-V) | QEMU (ESP32/-S3/-C3) |
|----------|----------------|---------------------------|----------------------|
| GPIO out (`on_pin_change`) | yes (per-edge IRQ) | yes (ODR poll over TCP) | yes (RAM-mailbox diff) |
| GPIO in (`set_digital_in`) | yes | yes | yes (gdbstub `M` write) |
| UART (`uart_write` / `on_uart`) | yes | yes | yes (serial socket) |
| ADC inject (`set_analog_in`) | yes | per-platform `AdcChannelMap` (Monitor feed command or result-word write); **no shipped Renode platform carries a map** (their stock `.repl`s model no ADC, verified live), so injections there are DROPPED and surfaced as a coverage warning on every report surface (see "ADC / bus coverage by platform") | yes, RAM-mailbox count slots (firmware contract) |
| I2C slave models (`on_i2c`) | yes (TWI decode) | yes on platforms whose descriptor names controllers (STM32F103/F4 `i2c1`, nRF52840 `twi0`/`twi1`); a slave bound on a controller-less platform is recorded as UNEXERCISED and surfaced on every report surface, and a CI `peripheral` assertion against it FAILS | yes, RAM-mailbox bus cells (firmware contract); plus temperature pushes into the machine's own tmp105 |
| SPI slave models (`on_spi`) | yes | yes on platforms with named controllers (STM32F103 `spi1` via `extra_repl`, F4 `spi1-3`, nRF52840 `spi2`); controller-less platforms get the same UNEXERCISED recording/surfacing | yes, RAM-mailbox bus cells (firmware contract) |
| Drive direction (`pins_configured_output`) | yes (DDR hooks) | yes on dir-mapped platforms: STM32F103 (CRL/CRH), STM32F4 (MODER), nRF52840 (DIR), polled alongside the ODR; RP2040/FE310 carry no verified dir map and stay direction-blind | no (mailbox carries levels only) |

Drive direction is what lets a boot-state check tell a held-LOW output from
a floating input. A backend reports it observable
(`Mcu::drive_direction_observable`) only when the configured-output set is
authoritative. On a Renode part it flips true exactly when every polled
port's SoC descriptor carries a verified `dir = { offset, encoding }` map,
and the ODR poll then also masks out pins not configured as outputs (an
input pin's ODR bit is not a drive). Unmapped platforms keep the
conservative behavior: every ODR change reports, direction stays
unobservable, and boot-coverage diagnoses hedge instead of asserting Hi-Z.
Direction-blind backends also feed the live sim's per-net honesty flag
(`SimFrame.unobserved_drive_nets`, see the QEMU limitations below): nets
whose MCU pin has never reported a level are disclosed to the UI as "static
level, drive not observable" instead of being presented as measurements.

The tiers: `simavr` runs in-process over FFI with cycle-accurate
TWI/SPI/ADC IRQ callbacks, so interception is exact. Renode is driven over
the Monitor TCP channel: I2C/SPI slaves are real Renode peripherals
(generated C# bridges that call back into the engine), and ADC counts are
injected per chunk through a per-platform recipe
(`RenodeConfig::adc_channels`): either a modeled ADC's feed command or a
`WriteDoubleWord` into the result word the firmware reads. The stock
STM32F103/F4/nRF52/FE310 configs ship no map, because those Renode
platforms model no ADC peripheral. hauksbee records an unmapped channel's
injections as DROPPED and surfaces them on every report surface: the run
text, `--plain`, `--json` (`CosimJson.adc_dropped` + a coverage note), and
all hauksbee-ci formats, naming the channel, MCU, and board net, so a run
whose firmware never received its analog inputs can never read as healthy.
Espressif QEMU exposes no host hook for its I2C RX FIFO, GPSPI transfers, or
SAR ADC, so all three ride the RAM mailbox (`qemu/mod.rs::mailbox`, the same
contract as the GPIO words): ADC counts land in fixed slots, and I2C/SPI
transactions go through request/response cells with a sequence handshake,
serviced once per chunk and surfaced through the same `on_i2c`/`on_spi`
callbacks as the other backends. That is a **firmware contract, not general
firmware support**: unmodified vendor firmware sees none of it (the cells
are gated on a `BUS_MAGIC` word), and each mailbox function retires the day
the fork grows the corresponding peripheral hook.

### ADC / bus coverage by platform (and how a hole is surfaced)

An external co-sim can degrade silently: the platform has no ADC injection
path, or no bus controller for a bound sensor, GPIO/UART still work, and the
report reads healthy. hauksbee makes that impossible to miss. It surfaces
every hole below on **all** report surfaces: `hauksbee run` default text,
`--plain` heads-ups, `--json` (`CosimJson.adc_dropped` /
`CosimJson.unexercised_buses` plus `notes[]` coverage entries), and every
hauksbee-ci format (human, JUnit, GitHub annotations, as `COVERAGE HOLE`
warnings). A CI `peripheral` assertion against an unexercised bus device
**fails** instead of green-passing on the slave's power-on defaults.

| Platform | ADC injection map | I2C controllers | SPI controllers |
|----------|-------------------|-----------------|-----------------|
| `simavr:atmega328p` | exact (in-process, always) | native TWI decode | native |
| `renode:stm32f103` | **none**: stock `stm32f103.repl` models no ADC (verified live); injections drop + warn | `i2c1` | `spi1` (via `extra_repl`) |
| `renode:stm32f4_discovery` | **none**: same reason | `i2c1` | `spi1`, `spi2`, `spi3` |
| `renode:nrf52840` | **none**: the live repl models no ADC/SAADC | `twi0`, `twi1` (live-verified: bridge registers on both) | `spi2` (live-verified registration) |
| `renode:sifive_fe310` / `renode:rp2040` | none | none (unverified peripherals, a bound slave warns + fails CI assertions) | none |
| `qemu:esp32/-s3/-c3` | RAM-mailbox contract | RAM-mailbox contract + machine tmp105 | RAM-mailbox contract |

Why no Renode ADC maps: Renode 1.16.1's shipped STM32F1/F4/nRF52840
platform descriptions register no ADC peripheral at all, and Renode's
`Analog.STM32_ADC` speaks the F0/L0 register layout. Registering it at an
F1/F4 address would let firmware read a wrongly-laid-out peripheral (fake
fidelity), and inventing a RAM result word is a firmware contract, not the
real converter. So the honest state is: no map, loud drop, warning on every
surface. A board that knows where its counts must land supplies
`[[soc.adc]]` in its own descriptor (`$HAUKSBEE_MCU_DIR`, no recompile).

We verified the nRF52840 controller names against the live Renode 1.16.1
(`peripherals` lists `twi0`/`twi1`/`spi2`, and the Hauksbee bridge
peripherals register on them, `tests/renode_nrf52840_bus.rs`). Renode
models the pre-EasyDMA TWI/SPI register interfaces there. TWIM/SPIM
(EasyDMA-only) firmware drives registers the model does not implement, and
an end-to-end nRF sensor round-trip awaits an nRF bus firmware fixture.

### Why QEMU (the Espressif fork) for the ESP32 family

Renode (as of 1.16.1) ships **no** `esp32.repl` / `esp32c3.repl` (verified:
the portable distribution's `platforms/cpus/` has nrf52840 and sifive-fe310
but no esp32 of any kind). Neither the Xtensa ESP32/S3 nor the RISC-V
ESP32-C3 has a turnkey Renode platform. Espressif maintains a QEMU fork with
the ESP32 SoC peripherals modelled (GPIO matrix, UART, SPI-flash
controller, timers) and publishes **native macOS-arm64 / Linux prebuilt
binaries**. So the ESP32 path is a separate, backend-pluggable QEMU
backend, which is exactly why the engine's backend dispatch is pluggable
(`qemu:<part>` alongside `renode:<part>`).

### QEMU lockstep mechanism (chosen by measurement, not assumption)

The contract is the same as Renode's: advance a bounded amount of guest
virtual time, block until done, then exchange GPIO/UART. QEMU is driven
with **QMP `cont` -> bounded wall window -> QMP `stop`**, the QMP analogue
of Renode's `RunFor`.

The theoretically ideal primitive is `-icount shift=N`, which makes virtual
time a deterministic function of executed instructions (bit-exact
reproducibility). **We tested it, and it does not work on the Espressif
esp32 (Xtensa) machine:** with `-icount` at any shift (4/6/8/auto), with or
without `sleep=off`, the esp32 machine produces ZERO UART output in a 15 s
wall window, versus ~1 s to the "hello from esp32" banner with no icount.
icount on these Xtensa machines is undocumented by Espressif and
empirically breaks boot, so it stays off.

Determinism without icount comes from the guest's own deterministic
peripheral timers (FreeRTOS tick, UART baud generator), sampled only at
chunk boundaries. The integration test asserts this directly: across
repeated runs the boot banner stays identical and the GPIO toggle count
stays stable to within a chunk or two. That meets the same standard a logic
analyser sampling a real board at the chunk rate meets. Alternatives
rejected: qtest `clock_step` (replaces the accelerator, cannot boot a real
flash image through TCG); gdbstub single-stepping (millions of steps per
chunk over RSP runs far too slow).

### How the QEMU backend bridges each path

- **Process**: `QemuBackend::new(config, firmware)` spawns
  `qemu-system-xtensa` (ESP32/S3) or `qemu-system-riscv32` (ESP32-C3)
  headless with `-machine <esp32|esp32s3|esp32c3>`, boots a merged flash
  image (`-drive file=...,if=mtd,format=raw`), and opens a QMP control
  socket, a serial socket, and (best-effort) a gdbstub. The firmware may be
  either that merged image (validated first: the bootloader must sit at
  the offset this chip's ROM reads, or the backend refuses with a named
  error instead of boot-looping) or a bare app `.elf`, from which the
  backend builds the merged image in-process (`qemu/flashimage.rs`,
  espflash's elf2image + default bootloader/partition table), so the
  artifact a user's build actually produces boots directly. QEMU is killed
  on drop. The backend finds the binary through `$HAUKSBEE_QEMU_XTENSA` /
  `$HAUKSBEE_QEMU_RISCV32`, then `$HAUKSBEE_QEMU_DIR`, then
  `~/.hauksbee-qemu-esp/qemu/bin/`, then the esp-idf idf_tools install,
  then `PATH` (rejecting Homebrew's mainline qemu, which has no esp32
  machine). If none is found, instantiation fails with a clear install
  message (tests skip).

- **GPIO out (poll) through a RAM mailbox**: the Espressif QEMU `esp32.gpio`
  model does **not** implement read-back of `GPIO_OUT_REG` (a host read
  over QMP `xp` or the gdbstub returns 0 regardless of the driven level,
  verified empirically; RAM, by contrast, round-trips exactly). So the demo
  firmware mirrors its GPIO output word to a fixed RAM mailbox (RTC slow
  memory, `0x5000_0000`), and the backend reads THAT word each chunk, diffs
  it, and synthesises per-bit edges. The bit layout matches `GPIO_OUT_REG`,
  so the edge synthesis matches the Renode ODR-poll byte-for-byte, only at
  a RAM address. The real `gpio_set_level` writes still happen; the
  mailbox is only the observation path the model lacks.

- **GPIO in (push)**: the backend pokes the mailbox `hauksbee_gpio_in`
  word over the gdbstub `M` packet; the firmware reads it where it would
  read `GPIO_IN_REG`.

- **UART (bidirectional)**: `-serial tcp:...,server,nowait` exposes UART0
  as a raw socket, bridged exactly like the Renode backend's UART.

- **Rails**: ESP32 parts run at 3.3 V, selected by the `qemu:` backend
  prefix.

### Why Renode for non-AVR

Renode (Antmicro, open source) ships faithful models for STM32 families,
nRF52, and many RISC-V machines, with **precise virtual-time control**. The
single primitive that makes tight lockstep work is:

```
emulation RunFor "0.0001"
```

`RunFor` advances virtual time by exactly the given interval and blocks
until it elapses, then pauses. That is the co-sim step: advance the
firmware a bounded amount, exchange pin/UART state, solve the analog
chunk, repeat. With `emulation SetGlobalAdvanceImmediately true` Renode
runs at host speed rather than pacing to wall-clock, which is what we want
when the analog solver sets the pace. Renode's virtual-time resolution is
1 ns.

We considered QEMU and unicorn as fallbacks. Neither was needed: QEMU's
`-icount` gives deterministic time but no clean bounded "run for exactly T
then stop and let me poke peripherals" loop over a stable control socket,
and unicorn is a raw CPU emulator with no peripheral models (no USART, no
GPIO blocks), so the firmware's `printf`/blink would have nothing to
drive. Renode gives both the time control and the peripheral models, so
the firmware runs unmodified.

### How the Renode backend bridges each path

- **Process**: `RenodeBackend::new(config)` spawns `renode --disable-xwt
  --hide-log -p -P <port>` headless, connects a Monitor TCP client, brings
  up the machine from the config, and connects a UART socket terminal. It
  is killed on drop. Renode is located through `$HAUKSBEE_RENODE`, then
  `renode` on `PATH`, then `~/renode-portable/...`. If none is found,
  instantiation fails with a clear install message (tests skip; they do
  not silently fall back to AVR).

- **GPIO out (poll)**: Renode has no generic "read pin" Monitor command,
  but a GPIO peripheral's output-data register is memory-mapped, so after
  each `RunFor` the backend reads each port's ODR with `sysbus.<port>
  ReadDoubleWord <odr>`, diffs it against the previous snapshot, and
  synthesises per-bit edge callbacks. This mirrors exactly how the simavr
  backend's port hook detects bit edges, so the scheduler sees identical
  behaviour. ODR offsets per family: STM32F1 `0x0C`, STM32F4 `0x14`, nRF52
  `0x4` (peripheral-relative: Renode registers `gpio0/1` at the `0x…500`
  register window, so the datasheet's block-relative `0x504` reads as
  unhandled, see `db/mcu/nrf52840.soc.toml`), FE310 `0x0C`. **RP2040 is the
  exception**: it is not a memory-mapped GPIO port bank, so the poll
  targets the SIO block's `GPIO_OUT` (offset `0x10` from `SIO_BASE =
  0xD000_0000`), which is where the driven output levels actually live
  (datasheet §2.3.1.7).

- **GPIO drive direction (poll)**: on dir-mapped platforms the backend
  reads each polled port's direction/mode register alongside the ODR,
  STM32F1 CRL+CRH (4-bit nibbles, MODE≠0 = output), STM32F4 MODER (2
  bits/pin, `0b01` = GP output), nRF52 DIR (1 bit/pin), decodes it to a
  per-pin output mask, masks the ODR diff with it, and surfaces it through
  `Mcu::pins_configured_output` (the same trait surface the AVR DDR hooks
  feed). We verified each map against the installed Renode's peripheral
  model read-back before shipping; a platform without a verified map
  carries no `dir` entry and stays honestly direction-blind (a wrong map
  would mask real edges, which is worse than the conservative status quo).

- **GPIO in (push)**: `sysbus.<port> OnGPIO <bit> <bool>` drives an input
  pin.

- **UART (bidirectional)**: `emulation CreateServerSocketTerminal <port>
  "t" false` + `connector Connect sysbus.<usart> t` exposes the UART as a
  raw TCP socket. Bytes the firmware transmits arrive on the socket, and
  bytes written to the socket inject into the UART receiver. The trailing
  `false` disables Renode's terminal config handshake so the stream stays
  raw both ways.

- **Rails**: the engine drives STM32-class GPIO outputs at 3.3 V (vs 5 V
  for classic AVR), selected by the `renode:` backend prefix.

## Per-architecture support matrix

A row is **Proven** only if it was actually run end-to-end on this branch (a
real emulator booting real firmware against the solved circuit, with the
output recorded). We label anything not run for real honestly. Every row's
config is a `db/mcu/<part>.soc.toml` descriptor, named by the `backend:part`
in the first column; the built-in constructors load these files.

| Architecture | Backend | Emulator / platform | Proof run on this branch |
|--------------|---------|---------------------|--------------------------|
| ATmega328P (AVR) | `simavr:atmega328p` | libsimavr (in-process) | **Proven** (pre-existing AVR demo) |
| STM32F103 (Cortex-M3, blue pill) | `renode:stm32f103` | Renode `stm32f103.repl` | **Proven**: "hello from stm32", R1 current via solver, PC13 toggles |
| ESP32 (Xtensa LX6) | `qemu:esp32` | Espressif QEMU `esp32` | **Proven**: "hello from esp32", R1 = 3.727 mA via solver, GPIO4 27 toggles, run-to-run stable |
| ESP32-C3 (RISC-V RV32IMC) | `qemu:esp32c3` | Espressif QEMU `esp32c3` | **Proven**: "hello from esp32", R1 = 3.727 mA via solver, GPIO4 32 toggles |
| nRF52840 (Cortex-M4) | `renode:nrf52840` | Renode `nrf52840.repl` | **Proven (UART boot)**: Zephyr shell `uart:~$` through the bridge. Bus controllers `twi0`/`twi1`/`spi2` live-verified (bridge registration, `renode_nrf52840_bus.rs`); an end-to-end nRF sensor round-trip awaits an nRF bus firmware fixture. Solved-LED-current proof would need a custom blinky ELF (the GPIO bridge is the STM32-proven ODR-poll). |
| SiFive FE310 (RISC-V RV32, HiFive1) | `renode:sifive_fe310` | Renode `sifive-fe310.repl` | **Proven (UART boot)**: "BOOTING ZEPHYR OS ... shell>" through the bridge (needs `post_load_setup`: PRCI tags + `cpu PC vinit`). |
| STM32F4 Discovery (Cortex-M4) | `renode:stm32f4_discovery` | Renode `stm32f4_discovery.repl` | Config shipped; platform present; not run on this branch |
| ESP32-S3 (Xtensa LX7) | `qemu:esp32s3` | Espressif QEMU `esp32s3` | **Wiring proven (machine boots, channels connect); app proof pending an S3 image.** The builtin `esp32s3` model entry binds to `qemu:esp32s3` (regression: `mcu_family_router.rs`), and `esp32_qemu_cosim.rs` boots the fork's real `esp32s3` machine from a blank flash (the ROM idles), connects QMP + gdbstub + UART, and steps the lockstep. The full app-level proof (UART banner + solved LED current, like the ESP32/C3 rows) needs a merged S3 flash image, which requires esp-idf with the esp32s3 Xtensa toolchain (`idf.py set-target esp32s3`; recipe in `testdata/firmware/esp32_blinky/build.sh`). |
| RP2040 (dual Cortex-M0+, Raspberry Pi Pico) | `renode:rp2040` | Renode `rp2040.repl` | Config shipped + unit-tested; **platform ABSENT in installed Renode 1.16.1**; smoke skip-gated (see note below) |
| ESP32-C6, ESP32-H2 |, |, | Not in the Espressif QEMU fork's machine list; out of scope |
| nRF5340 (ZSWatch-class) |, |, | See note below |

### nRF5340 / ZSWatch, honestly

ZSWatch is an **nRF5340**, not the nRF52840 proven above. The Renode 1.16.1
portable distribution ships `platforms/cpus/nrf52840.repl` but **no**
nRF5340 platform of any kind (verified: zero `*nrf5340*` files in the
build). So nRF5340 is NOT proven, and hauksbee claims no config for it. The
nRF52840 proof is the closest Nordic part proven; a ZSWatch-class nRF5340
board would need an nRF5340 Renode platform (upstream Renode carries some
nRF5340 work, but it is not in this portable build and was not run).

### RP2040 / Raspberry Pi Pico, honestly

`db/mcu/rp2040.soc.toml` (loaded by `RenodeConfig::rp2040()`) ships as a
data-driven descriptor, and it is unit-tested (`rp2040_config_shape`, the
serde round-trip, and the descriptor equivalence suite). What is **proven**
is the config shape and the datasheet-grounded register offsets. What is
**not** proven is a real boot, for one honest reason: the Renode build
installed here (portable **v1.16.1.16858**) ships **no rp2040 platform**:
`platforms/cpus/` carries only `picosoc` and `litex_picorv32` (unrelated
RISC-V soft cores), no `rp2040.repl` and no Raspberry Pi Pico board. The
integration smoke `tests/renode_rp2040.rs` therefore checks for the
platform `.repl` first and **skips loudly** with that reason rather than
pretending.

Two things could not be verified offline and stay best-effort in the
config, called out in `db/mcu/rp2040.soc.toml`'s header comment:

- **SIO, not a port bank.** RP2040 GPIO output does not live in a per-port
  ODR; it lives in the SIO (single-cycle IO) block. The output-state
  register is SIO `GPIO_OUT` at `0xD000_0010` (offset `0x10` from
  `SIO_BASE`), with `GPIO_OE` at `0xD000_0020` and `GPIO_IN` at
  `0xD000_0004` (datasheet §2.3.1.7). The ODR-poll points at SIO
  `GPIO_OUT`, the faithful adaptation of the F1-vs-F4 ODR-offset discipline
  to a part that has no port ODR.
- **Unverified Renode modeling.** The SIO peripheral's *name* in Renode's
  `rp2040.repl` (assumed `sio`) and whether Renode's SIO model reads
  `GPIO_OUT` back as the driven value stay unconfirmed until the smoke
  runs on a Renode that carries the platform. That run will confirm or
  correct both.

Deliberately **not** wired, to refuse rather than fake: **ADC** (RP2040 has
a SAR ADC, but no Renode ADC model is verified, so `adc_channels` is empty,
so an unmapped channel gets the merged policy's loud once-per-channel drop,
never a fake count); **I2C/SPI** (`i2c_controllers`/`spi_controllers` empty;
the RP2040 Renode peripheral set is unverified, so no bridge is installed
rather than claiming an unproven bus).

The scheduler dispatches `backend = "renode:rp2040"` (alias `renode:pico`)
to this config. The built-in `db/mcu.toml` rp2040 entry deliberately
carries **no** `backend` param yet: it exists for the BOOTSEL strap lint,
and auto-routing every bound RP2040 board into a Renode platform the
installed build does not ship would turn working binds into boot failures.
A board that wants RP2040 co-sim opts in with a user model layer setting
`backend = "renode:rp2040"`, and gets a loud platform-load error until its
Renode carries `rp2040.repl`.

### ESP32 in Renode, honestly

ESP32 remains **not usable in Renode** as of 1.16.1: no `esp32.repl` /
`esp32c3.repl` ships, and the ESP32 SoC peripheral set is unmodelled. That
gap is exactly why the `qemu:` backend (Espressif QEMU fork) serves the
ESP32 family rather than Renode. Both Xtensa (ESP32) and RISC-V (ESP32-C3)
ESP32 parts are proven through QEMU above.

### Co-sim fidelity notes (debugging all-zero or "never driven" results)

- **Crystal-clocked boards.** A crystal/resonator is bound high-impedance
  (`ComponentKind::Ignore`); the clock comes from the MCU model. Before
  this fix a crystal valued `16Mhz` (or any `C`-referenced one) was
  mis-bound as a 16-gigafarad capacitor that made the solve singular and
  drove **every** net to 0 V / "never driven". If you see board-wide
  all-zero co-sim voltages, confirm you are on a build with `39128bb`+ and
  check `--report` for any passive with an absurd capacitance. See
  [`LIMITATIONS.md`](../about/LIMITATIONS.md) Fixed #4.
- **ESP32 GPIO needs the firmware mailbox** (stock third-party firmware is
  GPIO-invisible). The reason is empirically validated (the fork's GPIO
  model exposes no output read-back), and we have specified the QEMU-fork
  patch that would remove the requirement; see
  [`LIMITATIONS.md`](../about/LIMITATIONS.md) (deferred section).

## What `--firmware` accepts (and the web drop zone too)

The loaders boot one compiled image, but you do not have to dig it out
yourself. Firmware input resolves in three tiers:

1. **A compiled `.elf` or `.hex`**: passes through untouched. This is
   always the most explicit option; for PlatformIO the image lives at
   `.pio/build/<env>/firmware.elf`, for ESP-IDF/CMake under `build/`.
2. **A zip archive** (web upload or `--firmware fw.zip`): hauksbee searches
   it for built images. A `.pio/build/<env>/firmware.elf` outranks a stray
   `.elf`, which outranks a `.hex`. The newest entry wins a tie, and the
   report says exactly which image ran.
3. **A PlatformIO project** (a directory on the CLI, or a zip that carries
   a `platformio.ini` but no built image): built with *your own* `pio run`
   (detect-don't-bundle, like the Renode/ngspice oracles). The env comes
   from `default_envs`, falling back to the newest built artifact. A
   missing `pio` or a failing build is a plain, actionable error, never a
   silent fallback. A CLI project directory is always rebuilt (`pio run`
   is incremental); a zip prefers an image it already contains, so an
   upload never kicks off a toolchain download you did not ask for.

One caveat, stated plainly: building a project executes its build scripts
(`extra_scripts` in `platformio.ini` is arbitrary code). That is the nature
of building software; only hand hauksbee projects you would build yourself.

## Recipes

### STM32F103 blue pill (the proven demo)

Board: `testdata/boards/stm32_bluepill_demo.kicad_pcb`, U1 STM32F103C8, PA5
-> 330 Ohm R1 -> LED -> GND (the analog current path the solver computes),
PC13 -> 4k7 -> GND (the blink indicator), USART1 on PA9/PA10.

Firmware: `testdata/firmware/stm32_blinky/`, bare-metal C (no vendor SDK),
builds with `arm-none-eabi-gcc`. It blinks PC13 at ~5 Hz, drives PA5 HIGH at
boot, prints `hello from stm32` on USART1 at boot and answers `i`/`v`
commands.

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
`backend = "renode:stm32f103"` and carries the LQFP-48 pin map (PC13, PA5,
PA9/10, power). The scheduler's `instantiate_renode` turns
`renode:stm32f103` into `RenodeConfig::stm32f103()`.

### ESP32 (the proven QEMU demo)

Board: `testdata/boards/esp32_devkit_demo.kicad_pcb`, U1 ESP32-WROOM-32,
GPIO2 -> 330 Ohm R1 -> LED -> GND (the analog current path the solver
computes), GPIO4 -> 4k7 -> GND (the blink indicator), UART0 on the module's
U0TXD/U0RXD.

Firmware: `testdata/firmware/esp32_blinky/`, esp-idf C app. It drives GPIO2
HIGH at boot, toggles GPIO4 at ~5 Hz, prints `hello from esp32` on UART0
and answers `i`/`v` commands. Because the Espressif QEMU `esp32.gpio`
model does not expose GPIO output register read-back, the firmware mirrors
its GPIO output word to a fixed RAM mailbox (`0x5000_0000`, RTC slow
memory) that the backend reads; the real `gpio_set_level` writes still
happen (see the firmware header and the limitations section).

Install (two pieces, both native macOS-arm64 / Linux):

```
# 1. Espressif QEMU fork binary (small, ~4 MB; no esp-idf needed for it):
#    grab the prebuilt release and unpack to ~/.hauksbee-qemu-esp/qemu
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

The committed `flash.bin` (merged bootloader + partition table + app) lets
the test run without rebuilding; `build.sh` regenerates it. The model db
entry (`db/mcu.toml`, id `esp32_wroom`) maps the part to `backend =
"qemu:esp32"`; the scheduler's `instantiate_qemu` turns `qemu:esp32` into
`QemuConfig::esp32()` and passes the flash image (the engine's "firmware"
path) to QEMU to boot.

### ESP32-C3 (RISC-V, same QEMU backend)

Board: `testdata/boards/esp32c3_devkit_demo.kicad_pcb`; firmware: the same
`esp32_blinky` app rebuilt for the C3 target (`idf.py -B build_c3
set-target esp32c3 build`, merged to `flash_c3.bin`). Backend
`qemu:esp32c3` uses `qemu-system-riscv32 -machine esp32c3`, proven
identically to the ESP32 above.

### nRF52840 (works out of the box)

The nRF52840 ships **built in**: no `db/mcu.toml` edit, no
`--models-dir`. Any component whose value matches `nRF52840` binds to the
`renode:nrf52840` backend directly (`hauksbee models resolve <board>`
prints `nrf52840  builtin(0)  mcu`).

A committed board + firmware pair runs end to end today:

```
hauksbee run testdata/firmware/renode_demos/nrf52840-zephyr_shell.board \
  --firmware testdata/firmware/renode_demos/nrf52840-zephyr_shell.elf \
  --headless --seconds 2
```

This boots the bundled Zephyr shell to the `uart:~$` prompt through
Renode. The board is a minimal nRF52840-DK-style skeleton: the SoC on a
3V3 rail, P0.13 driving LED1 through a 330R resistor, and P0.06/P0.08
broken out as the UART. The LED stays dark; the shell firmware never
toggles P0.13, so 0 V is the correct, predicted result, not a miss.

Backend: `RenodeConfig::nrf52840()` (ports gpio0/gpio1, OUT register at
0x504, uart0). Firmware: any nRF52840 ELF (e.g. a Zephyr blinky). Renode
ships `platforms/cpus/nrf52840.repl` and
`platforms/boards/nrf52840dk_nrf52840.repl`. The built-in pin map wires pad
3 -> P0.13 (LED), pads 5/6 -> P0.06/P0.08 (UART); a board that pins the
parts differently just needs its own `[models.pins]` in a `--models-dir`
override (the recipe pattern below).

### Adding a genuinely new MCU variant (the recipe pattern)

The parts above ship built in. For a part hauksbee does *not* yet know, the
same path applies: add a `[[models]]` entry mapping the value regex to a
`renode:<part>` / `qemu:<part>` backend and its pin roles. The nRF52840
entry in `crates/hauksbee-models/db/mcu.toml` is a worked template, and
`docs/extending/add-an-mcu-variant.md` walks the whole process (backend
descriptor, pin roles, validation). Example skeleton for a hypothetical new
part:

```toml
[[models]]
id = "mynewpart"
kind = "mcu"
[models.match]
value_re = "(?i)^MYNEWPART"
[models.params]
backend = "renode:mynewpart"
[models.pins]
# nRF-style GPIO is two 32-bit ports; roles "p0<bit>" and "p1<bit>".
"1" = "vss"
"2" = "vdd"
"3" = "p013"   # e.g. an LED on P0.13
"4" = "p006"   # UART TXD
"5" = "p008"   # UART RXD
```

### SiFive FE310 / HiFive1 (RISC-V, built in)

The FE310 also ships built in (`renode:sifive_fe310`); any value matching
`FE310`/`HiFive1` binds with no config. The built-in entry is shown here as
the reference pin map (and as a second worked template for the recipe
pattern above):

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

Backend: `RenodeConfig::sifive_fe310()` (one 32-bit gpio0, output value
register at 0x0C, uart0). Renode ships `platforms/cpus/sifive-fe310.repl`
and `scripts/single-node/sifive_fe310.resc` with a prebuilt demo ELF
reference. This exercises the identical Monitor/UART/RunFor path as STM32
on a RISC-V core, proving the backend stays ISA-agnostic.

## Limitations

- **GPIO is polled, not interrupt-driven on the host.** The backend reads
  each configured port's ODR once per `run_micros` chunk. Edges faster than
  the chunk alias, exactly like a real logic analyser sampling at the chunk
  rate. Match the firmware's switching rate to the analog chunk size (the
  demo blinks at ~5 Hz vs ~50-100 us chunks, comfortably oversampled).
  Bit-banged MHz signals are not resolved by the poll bridge; they would
  need the binary external-control GPIO event channel (future work, see
  below).

- **Monitor round-trip cost.** Each `RunFor` and each ODR read is a TCP
  request/response. A long co-sim with many ports polled every chunk spends
  most of its wall time in monitor round-trips, not emulation. Mitigations
  not yet implemented: poll only ports that have bound drivers, batch
  reads with a single `-e`-style multi-command, or move to Renode's binary
  ExternalControlServer (`GPIOPort GetState/SetState/RegisterEvent`,
  `RunFor`, `GetTime`), which avoids ASCII framing entirely.

- **ADC injection needs a per-platform map on Renode.** Renode's ADC
  peripheral API is per-SoC (`FeedSample` / `SetDefaultValue` vary by
  family), and the stock STM32F103/F4/nRF52/FE310 platform descriptions
  model no ADC at all, so `set_analog_in` delivers counts only where a
  `RenodeConfig::adc_channels` recipe says how (a Monitor feed command for
  a modeled ADC, or a `WriteDoubleWord` into the result word the firmware
  reads). hauksbee records an unmapped channel's drop and surfaces it on
  EVERY report surface (run text, `--plain`, `--json`
  `CosimJson.adc_dropped` + notes, and the hauksbee-ci human/JUnit/GitHub
  reports), naming the channel, MCU, and net, never a stderr-only whisper.
  The STM32 demo couples through the GPIO/LED path, not the ADC, so it
  carries no map. The AVR backend's ADC injection is fully wired and exact.

- **One firmware per machine.** The engine loads one firmware ELF/HEX for
  all MCUs on a board, matching the existing AVR behaviour.

### QEMU (ESP32 family) specific limitations

- **GPIO output is observed through a RAM mailbox, not the GPIO
  register.** The Espressif QEMU `esp32.gpio` model does not implement
  read-back of `GPIO_OUT_REG` (a host read returns 0 regardless of the
  driven level; writes to `GPIO_IN_REG` are dropped; RAM round-trips
  fine). So the demo firmware mirrors its GPIO output word to a fixed RAM
  mailbox the backend reads, and reads injected inputs from it.
  Consequence: **GPIO co-sim requires mailbox-aware firmware** (the
  committed demo is). Arbitrary third-party ESP32 firmware would boot and
  produce UART, but its GPIO output would not be visible to the solver
  unless it maintained the mailbox. A cleaner future fix is a small patch
  to the fork's gpio model to honour register read-back, which would
  remove the mailbox requirement entirely.

- **No `-icount` on the Xtensa esp32 machine.** Measured: `-icount` (any
  shift, with/without `sleep=off`) prevents the esp32/esp32s3 machines from
  booting (zero UART in 15 s). The lockstep therefore uses QMP stop/cont
  over the free-running virtual clock; timing runs wall-bounded, not
  instruction-counted, so determinism runs logic-level (stable banner +
  toggle count) rather than bit-exact. The RISC-V esp32c3 machine
  tolerates icount per Espressif's docs, but the backend keeps the same
  icount-free mechanism for uniformity.

- **ESP32 ADC (SAR) is not modelled** by the QEMU fork, so
  `set_analog_in` writes the modeled 12-bit count into a fixed RAM-mailbox
  slot instead: firmware reads the count from the slot where it would read
  the SAR result register, with a validity mask distinguishing "never
  injected" from an honest zero. Like the GPIO words, this is a firmware
  contract. The demo couples through the GPIO/LED path.

- **I2C/SPI byte interception rides the same mailbox**: the fork exposes
  no host hook for the I2C RX FIFO or GPSPI transfers, so a participating
  firmware submits transaction-level requests through mailbox cells (gated
  on a `BUS_MAGIC` word; sequence handshake; one transaction per bus per
  chunk) and the backend surfaces each byte through the standard
  `on_i2c`/`on_spi` callbacks, writing replies back into guest RAM.
  Unmodified vendor firmware is untouched, and its real-controller bus
  traffic stays host-invisible until the fork grows a peripheral hook.
  Temperature sensors additionally reach unmodified firmware through the
  machine's own emulated tmp105 through `set_i2c_device_temperature`.

- **Mailbox bank coverage bounds which GPIOs are observable at all.** The
  mailbox carries one output-mirror word per declared bank; the shipped
  `esp32s3` SoC descriptor declares bank `'0'` only, so GPIO32..48 (bank
  `'1'`: the Watchy v3's display RES/DC/CS on GPIO33/34/35 and its
  SCK/MOSI on GPIO47/48) are **not co-simulated at all**, firmware drives
  and reads of them are invisible, and the backend prints a warning naming
  the missing banks at attach time. Extending the descriptor's bank list
  (plus the firmware's mirror) is the fix; until then those pins stay
  permanently unobserved.

- **The live sim discloses unobservable pins instead of presenting static
  levels as measurements.** On a backend that cannot report drive
  direction (this QEMU mailbox: levels only, and no GPSPI/I2C-controller
  hook, see above), a pin whose driver has never reported a level might be
  genuinely undriven or driven invisibly. Either way the solved net
  voltage is just the passive network's idle level (a pull-up reads 3.3 V,
  a floating trace reads 0 V). Every such net is listed per frame in the
  wire protocol's `SimFrame.unobserved_drive_nets`, so the UI can label
  the reading "static level; MCU drive not observable on this backend"
  rather than "measured". Direction-observable backends (simavr,
  dir-mapped Renode platforms) never populate it: there, an undriven pin's
  level IS a real measurement.

- **Unmodelled ESP32 peripherals.** The fork models GPIO matrix, UART,
  SPI-flash, and timers, but WiFi/BT radio, RMT, I2S, the LEDC/MCPWM
  generators, and the touch/Hall sensors are not (or only partially)
  modelled. Firmware that blocks on one of those at boot may not reach
  `app_main`; the demo avoids them.
