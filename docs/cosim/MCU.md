# MCU co-simulation backends

Hauksbee co-simulates emulated microcontroller firmware against the solved
analog circuit. Every backend presents the same lockstep contract to the
engine (`hauksbee-mcu::Mcu`): run N microseconds of firmware, exchange GPIO
pin states, ADC voltages, and UART bytes. The scheduler does not care which
emulator sits behind the trait, so adding an architecture means adding a
backend, not touching the co-sim loop.

## The `Mcu` trait (the uniform contract)

```
load_firmware(path)       load one compiled .elf / .hex image into the core
run_micros(us)            advance firmware by us microseconds (lockstep step)
set_digital_in(pin, hi)   drive an external input pin
set_analog_in(ch, volts)  inject an ADC voltage
on_pin_change(cb)         callback per GPIO output edge:
                          (PinId{port,bit}, level, cycle stamp)
on_input_responder(cb)    SYNCHRONOUS responder: per cycle-stamped output edge,
                          returns input pins before the next instruction
on_input_responder_batch(cb)  same contract, but all pins changed by one GPIO
                              port write arrive as one atomic cycle-stamped batch
input_responder_batches_atomic()  true only when the backend preserves that
                                  hardware port-write boundary (default false)
uart_write(bytes)         inject UART RX bytes
on_uart(cb)               callback per UART TX byte
on_i2c(cb) / on_spi(cb)   intercept bus bytes, return the slave's reply
on_spi_controller(name,cb)  same, routed to ONE named SPI controller (a slave on
                          spi2 must not see spi3's traffic)
```

That is the coupling surface, the part a peripheral model or the scheduler
drives. The trait (`crates/hauksbee-mcu/src/traits.rs`) also carries status and
fidelity accessors the engine reads rather than drives: `state`, `frequency`,
`reset`, `cycle_exact`, `pins_configured_output`, `uart_rx_overflow`, `uart_rx_pending`,
`set_active_ports`. `reset` is implemented on the AVR backend only; on Renode and
QEMU it errors, and nothing in the engine calls it in response to a rail event,
so a board that browns out has its analog collapse caught by a `rail` assertion
while its firmware keeps executing as though the supply held.

### `on_input_responder`, closing a readback inside the firmware's bit-bang loop

`on_pin_change` reports output edges, and the scheduler can collapse them and
react on the *next* analog chunk. That is too coarse for a firmware that
bit-bangs a clock and `digitalRead`s the resulting serial-out bit in the SAME
tight loop, e.g. the Tarski `_ReadShiftRegisterWord`, which for 16 bits does
`digitalRead(MISO)` then pulses SCLK with back-to-back, sub-µs
`digitalWrite`s. By the time the chunk ends, the firmware has already
finished reading. Injecting MISO once per chunk would read `0x0000`: the
firmware is long past those cycles before the next injection arrives.

`on_input_responder_batch` fixes this: the AVR backend invokes it from the same
per-port output hook that fires `on_pin_change`, and it raises the pins it
returns onto their ioport input IRQs *synchronously*, before the firmware's
next instruction. All bits changed by one port-register write are delivered in
one batch, so a multipin device evaluates the externally visible final levels,
not a transient bit-order artifact. The legacy single-edge hook remains the
source-compatible fallback. A backend must separately advertise
`input_responder_synchronous()` before the engine trusts that optional hook; it
may then own a memory with one mutable MCU input and otherwise ground-only
inputs. A dynamic HIGH-active read gate must also keep the power-on bus released
until its first callback; zero-trigger and power-on-active memories stay on the
ordinary tick path. Multipin gates, addresses, or data still require an atomic
batch. One unambiguous weak pull directly to a known supply may seed a GPIO's
physical idle level when the backend synchronously reports DDR transitions; a
PORT-latch write while that pin is input still leaves the pull unchanged. An
unpulled input remains unknown until DDR first drives it, and that first drive
establishes state without inventing an edge. Conflicting or strong pulls,
pulled 74HC595 controls, supplied non-GPIO nodes, and any node another MCU,
edge-driven device, or analogue component can influence stay on the ordinary
tick path.
The engine installs an edge-driven `Hc165Chain` here: on
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
for the F103 SPI1 fragment, `[[soc.adc]]` injection recipes). A descriptor can
also declare `[soc.clock_control]`: board binding renders its `{present}`
command before boot and each Renode slice renders `{micros}`, while the
part-specific platform model owns the register layout and startup state
machine. The STM32F103 uses this to gate and time HSE/PLL readiness from an
assembled crystal across its semantic `osc_in`/`osc_out` pin roles. The shipped
descriptors are embedded through `include_str!` (the binary stays
self-contained, and the file stays the single source of truth), and
`RenodeConfig::stm32f103()` and friends are thin accessors that load their
descriptor. Validation refuses rather than fakes: unknown/mismatched
backend, empty `platform_repl`, zero-width or overlapping ports, duplicate
controllers, unknown `expected_e_machine`, ambiguous ADC inject, missing
clock-control placeholders, and (through `deny_unknown_fields`) any mistyped
field are all named load-time errors.

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
| GPIO out (`on_pin_change`) | yes (per-edge IRQ) | yes (ODR poll over TCP) | yes (real MMIO register on capability-probed patched QEMU; named RAM-mailbox fallback on upstream prebuilt) |
| GPIO in (`set_digital_in`) | yes | yes | firmware-mailbox contract (gdbstub `M` write) |
| UART (`uart_write` / `on_uart`) | yes | yes | yes (serial socket) |
| ADC inject (`set_analog_in`) | yes | per-platform `AdcChannelMap` (Monitor feed command or result-word write), and only where a descriptor declares one: `renode:stm32f072` inputs 0..7 and `renode:rp2040` inputs 0..3 are live-proven. STM32F103/F4/nRF52/FE310 model no ADC peripheral in their stock platforms, so injections there are DROPPED and surfaced on every report surface that runs the external backend (run text, `--plain`, `--json`, hauksbee-ci, and the TUI pane), never silently. The synchronous web front door refuses external-emulator runs before these scheduler signals exist and points to live/CLI co-sim instead. See "ADC / bus coverage by platform" | yes, RAM-mailbox count slots (firmware contract) |
| I2C slave models (`on_i2c`) | yes (TWI decode) | yes on platforms whose descriptor names controllers (STM32F103/F4 `i2c1`, nRF52840 `twi0`/`twi1`); a slave bound on a controller-less platform is recorded as UNEXERCISED on every report surface that runs that backend, and a CI `peripheral` assertion against it FAILS | yes, RAM-mailbox bus cells (firmware contract); plus temperature pushes into the machine's own tmp105 |
| SPI slave models (`on_spi`) | yes | yes on platforms with named controllers (STM32F103 `spi1` via `extra_repl`, F4 `spi2`/`spi3`, nRF52840 `spi2`); controller-less platforms get the same UNEXERCISED recording/surfacing | yes, RAM-mailbox bus cells (firmware contract) |
| Drive direction (`pins_configured_output`) | yes (DDR hooks) | yes on dir-mapped platforms: STM32F103 (CRL/CRH), STM32F4 (MODER), nRF52840 (DIR), polled alongside the ODR; RP2040/FE310 carry no verified dir map and stay direction-blind | yes on the capability-probed patched build (real GPIO ENABLE); no on the mailbox fallback, which carries levels only |

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
`WriteDoubleWord` into the result word the firmware reads. The shipped F072
config feeds its stock modeled converter; the stock STM32F103/F4/nRF52/FE310
configs ship no map, because those Renode platforms model no ADC peripheral.
hauksbee records an unmapped channel's
injections as DROPPED and surfaces them on all four batch report surfaces: the
run text, `--plain`, `--json` (`CosimJson.adc_dropped` + a coverage note), and
all hauksbee-ci formats, naming the channel, MCU, and board net, so a batch run
whose firmware never received its analog inputs can never read as healthy. The
interactive TUI carries it too, off the same `Scheduler::adc_dropped` signal
through one shared enumeration. The synchronous web front door does not run a
Renode/QEMU-only board, so its report is unavailable rather than falsely green
and names the live/CLI path that does carry this signal.
Espressif QEMU exposes no host hook for its I2C RX FIFO, GPSPI transfers, or
SAR ADC, so all three ride the RAM mailbox (`qemu/mod.rs::mailbox`, the same
contract as the GPIO words): ADC counts land in fixed slots, and I2C/SPI
transactions go through request/response cells with a sequence handshake,
serviced once per chunk and surfaced through the same `on_i2c`/`on_spi`
callbacks as the other backends. That is a **firmware contract, not general
firmware support**: unmodified vendor firmware sees none of it (the cells
are gated on a `BUS_MAGIC` word), and each mailbox function retires the day
the fork grows the corresponding peripheral hook.

### Clock fidelity by backend: does a firmware delay cost the right virtual time?

Two of the assertions hauksbee sells are time-based, so this is the table to
read before you trust one. It answers a single question per backend: firmware
whose real-silicon half-period is a known quantity is booted, and the ratio
below is the rate simulated time ran at, divided by the rate the part runs at.
`1.00x` means a `sleep_ms(20)` costs 20 ms of virtual time. Above `1.00x`
means virtual time is CHEAPER than the board's, which is the direction that
turns a rate no hardware can hit into a passing assertion.

Every number is measured on this host, not derived from a datasheet.

| Backend | ratio | how it is measured |
|---------|-------|--------------------|
| `simavr:atmega328p` | **1.00x, exact** | cycle-exact by construction: 80,000 cycles per 5 ms chunk is 16.000 MHz, and one `_delay_ms(5)` toggle per chunk |
| `renode:stm32f072` | **unmeasured; timing claims qualified** | its reset-default 8 MHz is declared and load-checked, but the stock RCC is a stub and no two-sided clock-truth firmware gate exists |
| `renode:rp2040` | **1.00x** | stock pico-sdk firmware, 20 ms of virtual time per `sleep_ms(20)` |
| `renode:stm32f103` | **1.00x** (was 9.09x fast) | `tests/clock_truth.rs`, a SysTick-timed 100 ms half-period |
| `renode:stm32f4_discovery` | **1.00x** (was 4.51x fast) | same gate |
| `renode:nrf52840` | **1.00x** (was 6.58x fast) | same gate |
| `renode:sifive_fe310` | **1.00x on `mtime`** (the stock platform declared it 1892x wrong); instruction timing corrected but not silicon-gated, see below | `tests/clock_truth.rs`, an mtime-timed 100 ms half-period, two-sided (the gate is also proven to FAIL a deliberately wrong rate) |
| `qemu:esp32` / `-s3` / `-c3` | **0.94x measured, host-load dependent** (was 1.35x-1.63x biased when the QMP round-trip slack went uncredited) | `tests/qemu_clock_truth.rs`, `vTaskDelay(100 ms)` toggles priced in credited time; carries a runtime timing caveat, see below |

**What was wrong, and why nothing caught it.** Four Renode platforms ran
simulated time at the EMULATOR's clock rate instead of the part's. The stock
`platforms/cpus/stm32f103.repl` declares `nvic systickFrequency: 72000000`
against a descriptor that declares an 8 MHz part, and 72/8 is exactly the 9.09x
measured; `stm32f4.repl` carries the same 72 MHz line against a 16 MHz part, for
4.51x. Every stock platform also left `cpu PerformanceInMips` at Renode's 100,
against roughly 8 MIPS of real F103 silicon, which is the 7.14x an instruction
busy-wait showed. The nRF52840's platform declared neither property, so both
fell to Renode defaults with no relation to the part.

Nothing complained because the descriptor's `frequency_hz` was decorative on
Renode: it cancels out of both `cycles = seconds * frequency_hz` and
`Mcu::frequency`, so the descriptor could disagree with its platform by 9x in
silence. RP2040 was right for the opposite reason: its platform is hauksbee's
own, and it set `PerformanceInMips` and `systickFrequency` to the part's real
125 MHz and carried real clock-tree models.

**How it is prevented rather than just corrected.** Each affected descriptor now
declares the part's clock inline (`platform_repl` accepts multi-line `.repl`
source, so a stock platform is extended with a `using` line and two properties
rather than vendored), and the descriptor loader cross-checks those declarations
against `frequency_hz` and REFUSES a mismatch, or a Renode platform that
declares no core clock at all (`soc::check_clock_declarations`). A new part
cannot be added with a lying clock. `tests/clock_truth.rs` then measures each
gated part against real firmware and fails outside 5%, because a declaration
proves what the platform says and only a measurement proves what it does.

**Which clock each part declares, and why.** The reset default, in every case
where firmware has not configured a PLL: 8 MHz HSI on the F103 (RM0008 §7.2),
16 MHz HSI on the F407 (RM0090 §6.2), 16 MHz HFROSC on the FE310 (FE310-G002
manual §6.2). The nRF52840 has no choice to make: its M4 runs at 64 MHz whether
the source is the internal oscillator or the crystal. The reset default is the
honest choice because Renode's STM32 and nRF platforms model no clock tree that
could follow a firmware's PLL bring-up, so one fixed rate has to be declared,
and the rate the part runs at before firmware touches anything is the one that
is true for every image rather than for some.

**The measurement trap, stated because it nearly buried the bug.** GPIO is
observed by polling the output register once per chunk, so a half-period at or
below the chunk width aliases. At 5 ms chunks the 9x-fast F103 firmware read a
perfect 100 edges in 2 s and looked exact; the same firmware at 200 us chunks
read 450. Any clock measurement here uses a chunk at least 10x finer than the
half-period a WRONG sim would produce, not merely finer than the right one, and
the gate measures the sim time at which the Nth edge arrives rather than an edge
count, so a missed edge can only make the part look slow and never fast.

**What was closed since, and how.**

- **The FE310's CLINT (`mtime`) was declared at 62 MHz** by the stock platform,
  while the real FE310 drives `mtime` from the 32.768 kHz always-on RTC tick.
  It stayed wrong for a while because no in-tree firmware exercised `mtime` on
  an observable path, and a 1892x edit to a timer nobody can measure is how the
  original core-clock defect was introduced. The missing oracle was built
  first: `testdata/firmware/clock_truth/fe310_tick.rs` toggles a pin every 3277
  mtime ticks (100.006 ms of real time on any FE310), and the descriptor's
  `clint frequency: 32768` override is held by a TWO-SIDED gate in
  `tests/clock_truth.rs`: the corrected platform measures 1.00x, and the same
  measurement against a deliberately restored 62 MHz fails loudly, so the gate
  is proven able to tell right from wrong. The mtime domain is separate from
  the core clock, so the loader's `PerformanceInMips` cross-check deliberately
  does not police it; the measurement is what holds it.
- **`qemu:esp32*` used to run 1.35x-1.6x biased, with the bias uncredited.**
  `run_seconds` does QMP `cont`, host sleep, QMP `stop`, and the guest keeps
  running during the round trips. That slack was called unmeasurable and left
  uncredited; it is not unmeasurable: QEMU stamps its RESUME/STOP events with
  the host time of the state transition, and the backend now credits the
  measured cont→stop window. `tests/qemu_clock_truth.rs` prices the same
  firmware delays in both currencies from one run: 1.63x biased under the
  old requested-window crediting, 0.94x under the measured crediting, on this
  host. The boot-window floor is unchanged and its chunks are credited from
  the same measurement.

**What remains open, and how a run says so.** These are systematic, so prose
here is not enough: each one is a `Mcu::timing_limitation` sentence that
reaches all four batch report surfaces of an affected run (the same channel and
discipline as the watchdog section below: default text, `--plain` heads-ups,
`--json` `CosimJson.timing_limitations` plus `notes[]` coverage entries, and
every hauksbee-ci format), plus `hauksbee models lint` before a run happens. The
interactive TUI carries it as well, from the same
`Scheduler::timing_limitations` signal. The synchronous web report refuses
external-emulator runs before a scheduler exists; its live/CLI alternatives
carry the limitation.

- **ESP32 virtual time is wall-paced.** icount breaks esp32 boot (measured, see
  `src/qemu/mod.rs`), so even with the cont→stop window measured, TCG pace
  tracks the host clock only approximately and degrades under host load. The
  `qemu:` backends state this on every run; treat ESP32 time as correct to
  within a few percent on an idle host, not as a clock.
- **The F103's TIMx blocks stay at 72 MHz** while its core and SysTick are at
  8 MHz. That is deliberate: only the post-PLL timer rate lets a stock CubeMX
  HAL project boot at all on a platform with no clock tree, and the alternative
  was a HAL time base landing at 139 Hz. The cost is that bare-metal firmware
  running TIMx from the reset-default HSI sees its timers 9x fast. The paths a
  delay loop and a SysTick tick take are gated; a TIMx time base is not, and
  the descriptor's `timing_limitation` says so on every F103 run.

**What the gates do NOT cover, on any Renode part.** The clock-truth gates
measure timer-paced delays: SysTick on the Cortex-M parts, `mtime` on the
FE310, each part's dominant delay path. Raw instruction busy-waits ride
`PerformanceInMips` instead, which encodes the one-instruction-per-cycle
approximation on every Renode CPU and is silicon-gated nowhere: the FE310's
went from Renode's 100 to the part's 16 (roughly right instead of roughly 6x
fast), and the M-class parts carry the same identity. This is a bounded
approximation shared by every part above, not a per-part divergence, which is
why it is stated here rather than as a per-run `timing_limitation` sentence;
judge firmware by its timer-paced delays, which are the gated paths.

### Watchdog fidelity by backend (an unserviced watchdog may not reset)

A firmware whose recovery path depends on the watchdog biting is a firmware
whose watchdog is untested if the co-simulator's watchdog does not bite. That is
a coverage hole in exactly the sense a dropped ADC injection is, so it is
surfaced the same way, on all four batch report surfaces: `hauksbee run` default
text, `--plain` heads-ups, `--json` (`CosimJson.watchdog_limitations` /
`CosimJson.watchdog_resets` plus `notes[]` coverage entries), and every
hauksbee-ci format as a `COVERAGE HOLE` warning. `hauksbee models lint` states
it per descriptor before a run happens. The interactive TUI co-sim pane carries
both statements. The simavr web run can report a reboot, while the synchronous
web report refuses the external backends that emit watchdog limitations (see
[Which surface carries a coverage hole](#which-surface-carries-a-coverage-hole)).

`Mcu::watchdog_limitation` returns the whole sentence, which every surface that
carries the signal renders verbatim through one shared formatter
(`scheduler::watchdog_limitation_message`) so two of them cannot word the same
gap differently. `Mcu::watchdog_resets` reports reboots that DID happen, because firmware
behaviour observed after a reboot belongs to a rebooted core and an assertion
that passed across one was not measuring the run it claimed. The two mean
nothing apart: a backend that cannot reboot at all reports zero resets, so a
quiet counter is only good news next to a quiet limitation. A part that claims
full fidelity (only `simavr` does) produces NOTHING on any surface, and that
silence is what makes the warning worth reading.

| Backend | armed and never fed | how it is known |
|---------|---------------------|-----------------|
| `simavr:atmega328p` | **reboots, at the right virtual time, repeatedly** | `tests/avr_watchdog.rs`, two-sided against the same firmware with the arming line removed |
| `renode:stm32f103` | **resets once, then the core does not resume** | measured: the heartbeat stops at the timeout and the boot marker flips a second time, then 450 ms of silence where the part would reboot every 50 ms |
| `renode:nrf52840` | **never fires** | measured: RUNSTATUS reads 1 and CRV reads back a correct 32768 Hz reload, and 1.000 s of simulated time gives zero resets where the part gives twenty |
| `qemu:esp32` / `-s3` / `-c3` | **disabled on purpose** | the backend passes `wdt_disable=true` for the timer groups at launch |
| `renode:stm32f072`, `renode:stm32f4_discovery`, `renode:sifive_fe310`, `renode:rp2040` | **unverified** | nobody has run a starved watchdog to the timeout on these parts, and the two that were measured disagree with each other, so neither can be inferred |

Disabling the ESP32 watchdogs is the right call and stays: co-simulation pauses
the guest at every chunk boundary while the analog side solves, and a running
timer-group watchdog would read those pauses as a hung firmware and reset a core
that is doing nothing wrong. What was wrong was that the trade lived only in a
source comment, where a user reading a green report never saw it.

**The AVR watchdog used to hang the co-simulator**, which is worth knowing
because the symptom looked like slowness rather than a bug.
`wdt_enable(WDTO_15MS)` with no `wdt_reset()` livelocked: simavr's `avr_reset`
zeroes `avr->cycle`, and the backend's step loop ran against an absolute
cumulative cycle target that a rewound counter can never reach, so the chunk in
which the watchdog first fired never returned while the emulator kept executing
firmware at full speed. The loop now follows a rewound counter, re-anchoring the
target and keeping the unspent part of the chunk's budget so the rebooted core
still runs out the simulated time the engine paid for. A watchdog reboot is
reported as a finding, not treated as a silent restart, because an assertion
that passed across one was not measuring the run it claimed to.

### ADC / bus coverage by platform (and how a hole is surfaced)

An external co-sim can degrade silently: the platform has no ADC injection
path, or no bus controller for a bound sensor, GPIO/UART still work, and the
report reads healthy. hauksbee makes that impossible to miss. It surfaces every
hole below on all four batch report surfaces: `hauksbee run` default text,
`--plain` heads-ups, `--json` (`CosimJson.adc_dropped` /
`CosimJson.unexercised_buses` plus `notes[]` coverage entries), and every
hauksbee-ci format (human, JUnit, GitHub annotations, as `COVERAGE HOLE`
warnings). A CI `peripheral` assertion against an unexercised bus device
**fails** instead of green-passing on the slave's power-on defaults. The TUI
carries them too from the same signals. The synchronous web report does not run
the external backend that can emit them; it returns an unavailable refusal and
points to live/CLI co-sim. The per-surface matrix is the next subsection.

#### Which surface carries a coverage hole

"The four batch report surfaces" above means the `hauksbee run` default text
summary, `--plain`, `--json`, and hauksbee-ci. The counting unit is one column
per consumer, so hauksbee-ci's human report, JUnit file and GitHub annotations
are one column here, not three; where a claim is about the CI artifacts
individually (the waiver disclosure in [CI.md](../ci/CI.md), for instance) they
are counted separately and the unit is stated there.

Two more co-sim surfaces render outside a `run` invocation and are not in the
matrix below, because they do not read a finished run's caveats:

- `hauksbee models lint` states a part's `watchdog_limitation` and
  `timing_limitation` from the descriptor, before any run happens. It has no
  `--json`; its output is text for a person.
- `hauksbee serve`'s live sim streams frames over a websocket
  (`hauksbee-server` `SimFrame`). It drives the same scheduler, including the
  Renode and QEMU backends, and carries none of the twelve disclosures below. Treat it
  as a scope, not a report.

Twelve typed per-run co-sim disclosures share one completeness contract. Six
are limitations (holes), watchdog reboot and drive override are observed
events, driver contention is an electrical fault, per-core timing coverage is
a measured non-hole bound, `TIMING INVALID` is a strict refusal, and fallback
windows are second-class qualifications. Counted from the emitting call sites
rather than from intent. The `--plain`
column is yes throughout because `--plain` renders the default text co-sim
summary AND its own heads-ups (`run_headless` is called with `quiet = cfg.json`,
so only `--json` silences the text block):

| Coverage caveat | default text | `--plain` | `--json` | hauksbee-ci | TUI | web front door |
|---|---|---|---|---|---|---|
| dropped ADC injections | yes | yes | yes | yes | yes | not run (external backend) |
| unexercised buses | yes | yes | yes | yes | yes | not run (external backend) |
| watchdog limitation | yes | yes | yes | yes | yes | not run (external backend) |
| watchdog reboots | yes | yes | yes | yes | yes | yes |
| timing limitation | yes | yes | yes | yes | yes | not run (external backend) |
| per-core timing coverage | yes | yes | yes (field) | yes | yes | yes (field) |
| strict timing replay refusal | yes | yes | yes (field) | yes (refuses timing claims) | yes | yes (field + refusal) |
| fallback-qualified windows | yes | yes | yes (field) | yes (field/qualification) | yes | yes (field) |
| short pulses | yes | yes | yes | yes | yes | yes |
| driver contentions | yes | yes | yes | no | yes | yes |
| drive conflicts | no | yes | yes | no | yes | yes |
| heuristic SPI framing | yes | yes | yes | assertion detail | yes | yes (field) |

The TUI carries all twelve typed disclosures and preserves six distinct
dispositions. The synchronous web front door carries every disclosure its
simavr-only run can emit and refuses any run containing an external backend before the four
external-specific scheduler signals exist. This is not a coverage score and
must not be presented as an all-green 10/10-style claim. The default text,
`--plain`, `--json`, and hauksbee-ci retain the command-specific projections in
the table.

"yes (field)" means the caveat arrives as a structured field with no caveat
sentence attached, so a consumer has to read the field to learn about it.
"assertion detail" means hauksbee-ci attaches it to a `peripheral` assertion's
detail line and to the `spi_framing` field, so a spec with no `peripheral`
assertion gets the field only.

**One enumeration behind all six columns.** Each class already had exactly one
wording (`AdcDrop::message`, `scheduler::watchdog_reset_message`, and so on), but
nothing said WHICH classes existed, so every surface re-listed them by hand and
the interactive ones fell behind. `reports::coverage::CoverageInputs::
from_scheduler` is now the single extraction point from the scheduler and
`::caveats` the single enumeration, and each caveat's sentence is produced by the
formatter that already owned that wording rather than paraphrased. The four batch
columns still read the scheduler accessors directly, which is why they are the
three `no` cells above; they were never the surface that fell behind.

**Reading the TUI column.** The co-sim pane renders the WHOLE caveat list, so a
class added to the enumeration appears there without a second edit. Screen space
is handled by splitting count from wording: a persistent two-line banner directly
under the pane's status lines gives separate limitation, timing-bound,
strict-refusal, fallback-qualification, observed-event and electrical-fault
counts plus the class names, and `c`
opens an overlay carrying every disclosure's full sentence plus its
disposition-specific next action (scrolled with ↑/↓): missing coverage names an
input or model that can unlock it, while a measured timing bound, observed event,
fallback qualification or electrical fault tells you how to interpret or improve
that result. The banner renders ABOVE the GPIO
table and the UART tail, both of which grow during a run, because a caveat the
user has to scroll to find has not been surfaced; `render.rs`'s
`the_coverage_banner_sits_above_the_growing_panes` pins that ordering. The footer
carries the total disclosure count, so it is on screen whichever pane has focus;
the banner separately breaks that total into holes, timing bounds, refusals,
fallback spans, events and faults. The pane
also keeps its own analog-validity refusal, failed-chunk count, MCU-substitution
caveat and firmware-ran signal, which are not in this count (see below). Its
caveats are counted at the latest chunk boundary, which the overlay says out
loud: a caveat that only becomes true later appears later, and the finished run's
report is the record.

The dashboard honors an explicit `hauksbee run --tui --chunk-us N` value all
the way into the worker. Narrowing that value can improve a poll-boundary
backend's edge resolution and strict replay capacity. It cannot improve a
cycle-exact backend beyond one emulated core cycle; that disclosure instead
points to a justified clock configuration or hardware measurement.

**Reading the web column.** Per-core timing coverage rides the structural
`cosim.timing_coverage` JSON field rather than a finding, the same tier
`--json` gives it, because it is a resolution statement present on every run with
a live core: as a finding it would demote every healthy report's headline through
`cosim_caveat_headline` and stop being read. Heuristic SPI framing rides the
`spi_framing` field for the same reason it does in `--json`. Strict replay
failures ride `timing_refusals` and populate the report's typed refusal; they
must never be rendered as an ordinary warning or a pass. Fallback spans ride
`fallback_windows`, retaining the method, fidelity note and optional measured
error estimate so the browser and standalone HTML label them second-class.
These optional JSON fields preserve older report consumers without adding required
fields to the public Rust `WebCosimSection` literal. Of the remaining
eight, seven are note-level `WebFinding` cards (which do demote a bare "Looks
healthy") and driver contention is a serious one, because two push-pull drivers
fighting a net is a real electrical fault rather than a caveat about the run. Four
of the seven notes are structurally empty on this path, which co-sims AVR in
process: `simavr` reports
no watchdog limitation and no timing limitation, its ADC injection is exact, and
it decodes TWI and SPI natively, so there is no dropped injection or unexercised
bus to report. The synchronous report refuses boards containing Renode/QEMU before
these signals exist; the projection is wired for a future backend addition but
is not counted as current operational coverage. Watchdog reboots were the class
this surface reached and did not report: `simavr`'s watchdog does bite and
`Mcu::watchdog_resets` counts the reboots, so a run whose firmware was rebooted
mid-window used to read quiet here. `interactive_coverage_parity.rs` proves both
sides of that against the same firmware with and without its one arming line.

Two further co-sim signals sit outside this count for stated reasons. The
`analog_valid` / failed-window refusal has its own run-validity contract.
`Scheduler::uart_rx_overflow` is the same class as a dropped ADC injection but
is read by one consumer only, `hauksbee run --serial-attach`, so it belongs to
that command rather than to the report surfaces.

| Platform | ADC injection map | I2C controllers | SPI controllers |
|----------|-------------------|-----------------|-----------------|
| `simavr:atmega328p` | exact (in-process, always) | native TWI decode | native |
| `renode:stm32f072` | inputs 0..7 through the stock `STM32F0_ADC` `SetDefaultValue` feed, proven end-to-end against firmware ADC reads on channels 0 and 3; package inputs 8/9 remain unmapped and drop + warn | none (unverified `STM32F7_I2C` bridge) | none (unverified `STM32SPI` bridge) |
| `renode:stm32f103` | **none**: stock `stm32f103.repl` models no ADC (verified live); injections drop + warn | `i2c1` | `spi1` (via `extra_repl`) |
| `renode:stm32f4_discovery` | **none**: same reason | `i2c1` | `spi2`, `spi3` (see below) |
| `renode:nrf52840` | **none**: the live repl models no ADC/SAADC | `twi0`, `twi1` (live-verified: bridge registers on both) | `spi2` (live-verified registration) |
| `renode:sifive_fe310` | none | none (unverified peripherals, a bound slave warns + fails CI assertions) | none |
| `renode:rp2040` | inputs 0..3 (proven end-to-end against stock `adc_read()`); input 4 is the on-die temperature sensor, not an external node, so it drops + warns | `i2c0`, `i2c1` (proven end-to-end, both directions) | **none**: the vendored PL022 bit-bangs onto GPIO and never dispatches to a registered slave, so a bound slave warns + fails CI assertions (see below) |
| `qemu:esp32/-s3/-c3` | RAM-mailbox contract | RAM-mailbox contract + machine tmp105 | RAM-mailbox contract |

Why the F4 lists `spi2`/`spi3` and not `spi1`: the base `stm32f4.repl` that
`stm32f4_discovery.repl` includes already defines all three. Registering `spi1`
again (the `extra_repl` trick the F103 needs, because its platform defines no
SPI at all) raises a Renode redefinition/address conflict: the Monitor dumps the
peripheral's method list instead of accepting the command, and the bridge
registration that follows then panics. So the F4 descriptor omits `extra_repl`
and binds bridges to the already-existing `spi2`/`spi3`
(`crates/hauksbee-mcu/db/mcu/stm32f4_discovery.soc.toml`).

Why no ADC map on the remaining stock Renode platforms: Renode 1.16.1's
shipped STM32F1/F4/nRF52840 platform descriptions register no ADC peripheral
at all, and Renode's
`Analog.STM32_ADC` speaks the F0/L0 register layout. Registering it at an
F1/F4 address would let firmware read a wrongly-laid-out peripheral (fake
fidelity), and inventing a RAM result word is a firmware contract, not the
real converter. So the honest state is: no map, loud drop, warning on every
surface. A board that knows where its counts must land supplies
`[[soc.adc]]` in its own descriptor (`$HAUKSBEE_MCU_DIR`, no recompile).

The F072 is the counterexample: stock `stm32f0.repl` registers
`Analog.STM32F0_ADC`, whose `SetDefaultValue` method accepts a voltage and a
channel. `renode_stm32f072.rs` feeds 1.650 V and 0.825 V into channel 0 and 3.300
V into channel 3, then observes the firmware's own conversions return 0x800,
0x400 and 0xfff. It separately injects unsupported package channel 8 and proves
the loud dropped-channel record. That is a modeled converter path, not a RAM
contract or a register-layout substitution.

RP2040 is the exception because its platform is not Renode's. Its ADC model is
vendored alongside the rest of the SoC (see the support-bundle section below)
and takes a real voltage through `SetDefaultVoltageOnChannel`, so there is a
converter to feed rather than a wrong-layout model to abuse.

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
  `$HAUKSBEE_QEMU_RISCV32`, then `$HAUKSBEE_QEMU_DIR`, then the reviewed
  source build under `~/.hauksbee-qemu-esp-patched/qemu/bin/`, then
  `~/.hauksbee-qemu-esp/qemu/bin/`, then the esp-idf idf_tools install,
  then `PATH` (rejecting Homebrew's mainline qemu, which has no esp32
  machine). If none is found, instantiation fails with a clear install
  message (tests skip).

- **GPIO out (poll), real register when proved**: Espressif's pinned QEMU
  release discards `GPIO_OUT_REG` writes. The reviewed patch at
  `scripts/qemu-patches/esp32-gpio-register-state.patch` retains OUT/ENABLE
  state for ESP32/S3/C3 and exposes paired read-only `gpio-out`/`gpio-enable`
  QOM properties. The backend probes both live properties and only then reads
  the real MMIO OUT and ENABLE words.
  Ordinary ESP-IDF firmware is therefore visible without any Hauksbee code.
  An unpatched binary fails closed to the legacy RTC-RAM output mirror and
  prints that mailbox requirement; the edge-diff logic is identical.

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

A row is **Proven** only if it has actually been run end-to-end (a real emulator
booting real firmware against the solved circuit, with the output recorded).
Anything short of that says what it is instead of borrowing the word. Every
`renode:`/`qemu:` row's config is a `db/mcu/<part>.soc.toml` descriptor, named
by the `backend:part` in the first column; the built-in constructors load these
files. The AVR row has none by design (simavr's own part database does the
work), and the bottom two rows have none because they have no backend to
configure.

| Architecture | Backend | Emulator / platform | End-to-end proof |
|--------------|---------|---------------------|--------------------------|
| ATmega328P (AVR) | `simavr:atmega328p` | libsimavr (in-process) | **Proven** (pre-existing AVR demo) |
| STM32F072C8/CB (Cortex-M0) | `renode:stm32f072` | Renode `stm32f072.repl` / `stm32f0.repl` | **GPIO/UART/ADC proven**: alternating PC6 GPIO, exact USART1 banner, two-sided quiet firmware, and stock-converter ADC injection on channels 0 and 3; the built-in 48-pin model resolves the corpus BMS MCU. Timing and watchdog fidelity remain explicitly unverified. |
| STM32F103 (Cortex-M3, blue pill) | `renode:stm32f103` | Renode `stm32f103.repl` | **Proven**: "hello from stm32", R1 current via solver, PC13 toggles |
| ESP32 (Xtensa LX6) | `qemu:esp32` | Espressif QEMU `esp32` | **Proven**: "hello from esp32", R1 = 3.727 mA via solver, GPIO4 27 toggles, run-to-run stable |
| ESP32-C3 (RISC-V RV32IMC) | `qemu:esp32c3` | Espressif QEMU `esp32c3` | **Proven**: "hello from esp32", R1 = 3.727 mA via solver, GPIO4 32 toggles |
| nRF52840 (Cortex-M4) | `renode:nrf52840` | Renode `nrf52840.repl` | **Proven (UART boot)**: Zephyr shell `uart:~$` through the bridge. Bus controllers `twi0`/`twi1`/`spi2` live-verified (bridge registration, `renode_nrf52840_bus.rs`); an end-to-end nRF sensor round-trip awaits an nRF bus firmware fixture. Solved-LED-current proof would need a custom blinky ELF (the GPIO bridge is the STM32-proven ODR-poll). |
| SiFive FE310 (RISC-V RV32, HiFive1) | `renode:sifive_fe310` | Renode `sifive-fe310.repl` | **Proven (UART boot)**: "BOOTING ZEPHYR OS ... shell>" through the bridge (needs `post_load_setup`: PRCI tags + `cpu PC vinit`). |
| STM32F4 Discovery (Cortex-M4) | `renode:stm32f4_discovery` | Renode `stm32f4_discovery.repl` | Config shipped; platform present; not yet run end-to-end |
| ESP32-S3 (Xtensa LX7) | `qemu:esp32s3` | Espressif QEMU `esp32s3` | **Wiring proven (machine boots, channels connect); app proof pending an S3 image.** The builtin `esp32s3` model entry binds to `qemu:esp32s3` (regression: `mcu_family_router.rs`), and `esp32_qemu_cosim.rs` boots the fork's real `esp32s3` machine from a blank flash (the ROM idles), connects QMP + gdbstub + UART, and steps the lockstep. The full app-level proof (UART banner + solved LED current, like the ESP32/C3 rows) needs a merged S3 flash image, which requires esp-idf with the esp32s3 Xtensa toolchain (`idf.py set-target esp32s3`; recipe in `testdata/firmware/esp32_blinky/build.sh`). |
| RP2040 (dual Cortex-M0+, Raspberry Pi Pico) | `renode:rp2040` | hauksbee's own `rp2040.repl` + vendored peripheral models, compiled by Renode at run time (support bundle) | **Proven**: stock pico-sdk firmware boots through the real boot ROM into `main`, UART0 banner + GP25 toggling at 3.300 V through the solver on a corpus board. GPIO out/dir via SIO, UART0 TX, ADC inputs 0..3 and the I2C bridge on `i2c0`/`i2c1` are each proven end-to-end. SPI bridge unavailable, PIO absent, core 1 unproven (see note below) |
| ESP32-C6, ESP32-H2 |, |, | Not in the Espressif QEMU fork's machine list; out of scope |
| nRF5340 (ZSWatch-class) |, |, | See note below |

### nRF5340 / ZSWatch, honestly

ZSWatch is an **nRF5340**, not the nRF52840 proven above, and hauksbee claims
no config for it. The gap is precise rather than vague, and it is a different
gap from the one RP2040 had.

No nRF5340 platform exists in the installed Renode 1.16.1 portable build
(`platforms/cpus/` carries `nrf52840.repl` and zero `*nrf5340*` files), and
none exists on Renode `master` either. What matters is why that cannot be
patched the way RP2040's was: with RP2040 the peripheral **models** existed
in a third-party repo and only needed vendoring, whereas here the models do
not exist anywhere to vendor. The pieces line up like this:

- **Cortex-M33 is supported.** The core the nRF5340 application processor uses
  is a Renode-supported CPU, so the ISA is not the obstacle.
- **The nRF52-series peripheral models exist** (`renode-infrastructure` carries
  the UARTE/GPIO/TWI/SPI family), and some carry over.
- **There is no SPU model.** The TrustZone-capable `cpuapp` image configures the
  System Protection Unit as part of its own start-up, and `renode-infrastructure`
  has nothing that answers those registers.
- **There is no nRF53 IPC model.** `NRF_Bellboard` is the nRF54 mailbox, a
  different peripheral; the nRF5340's inter-processor communication block is
  unmodelled.
- **There is nothing for the network core** at all.

So the honest estimate is that a boot-only nRF5340 is days of work (writing SPU
and IPC models from the product specification, then debugging a Zephyr boot
against them), not an afternoon of vendoring. Consequence, stated plainly: the
ZSWatch nRF5340 known-fault miss stands. Any check on that board that needs
firmware to run is still out of reach, and the nRF52840 proof is the closest
Nordic part hauksbee can actually co-simulate.

### RP2040 / Raspberry Pi Pico, honestly

RP2040 co-sim runs, and the platform it runs on is hauksbee's, not Renode's.
Renode 1.16.1 ships no rp2040 platform (`platforms/cpus/` carries only
`picosoc` and `litex_picorv32`, unrelated RISC-V soft cores) and neither does
Renode `master`, so unlike the STM32F1 case there was nothing to extend: the
peripheral **models** were missing, not just their wiring. They are vendored
into `crates/hauksbee-mcu/db/mcu/rp2040/` and compiled by Renode at run time
through the support-bundle mechanism described in the next section.

The proof is a real boot. Stock pico-sdk 2.1.1 firmware
(`testdata/firmware/rp2040_blink_uart/`) loads, runs through the real boot ROM's
function table during `runtime_init`, reaches `main`, prints on UART0 and drives
GP25. Running it against the corpus RP2040-minimal board for 2.00 s of simulated
time gives the UART banner `hauksbee rp2040: main reached` followed by 50 `led
on`/`led off` lines, and net `GPIO25` swinging 0.000 V to 3.300 V with 99 toggles
through the solver across the board's 52 nets. Three live integration suites
re-prove it on every run: `tests/renode_rp2040.rs` (boot, GPIO, UART),
`tests/renode_rp2040_adc.rs`, `tests/renode_rp2040_bus.rs`.

Per-feature tiers, which is where the honesty lives:

- **GPIO out and direction, via SIO. Proven end-to-end, two-sided.** RP2040 has
  no per-port ODR: the output value lives in the SIO (single-cycle IO) block,
  `GPIO_OUT` at `0xD000_0010` (offset `0x10` from `SIO_BASE`), `GPIO_OE` at
  `0xD000_0020`, `GPIO_IN` at `0xD000_0004` (datasheet 2.3.1.7). The ODR-diff
  poll is pointed at SIO `GPIO_OUT` and the direction decode at `GPIO_OE`, which
  is the F1-vs-F4 offset discipline adapted to a part with no port bank. Both
  read back the driven value on the live machine (measured `GPIO_OUT` =
  `GPIO_OE` = `0x02000000` after `main`, bit 25 = the Pico LED). Two-sided
  because a firmware that never touches a pin produces no edges.
- **UART0 TX. Proven end-to-end.** The SDK's `stdio_init_all` plus `printf`
  arrives on the host socket in order. UART1 is defined by the platform but is
  not exercised by anything, so it is declared and unproven.
- **Timers, clocks, resets. Proven as far as the SDK booting and `sleep_ms`
  costing the right virtual time** (20 ms of virtual time per `sleep_ms(20)`,
  measured). That is not a fidelity claim about alarm edge cases.
- **ADC injection, inputs 0..3. Proven end-to-end**, two voltages on one running
  machine: an engine-pushed node voltage reaches the converter and stock
  `adc_read()` returns the matching 12-bit code. Input 4 is the on-die
  temperature sensor, not an external node, so mapping it to a circuit voltage
  would be a lie about what the pin is. It stays unmapped and takes the merged
  policy's loud once-per-channel drop instead of a fake count.
- **I2C bus-slave bridge on `i2c0` and `i2c1`. Proven end-to-end, both
  directions.** Real pico-sdk firmware writes a register index to a
  host-modelled slave, reads two bytes back, and prints what the host sent.
  Registration alone would not have earned the word.
- **SPI bus-slave bridge. NOT available.** `SPI.PL022` in the vendored model
  names `NullRegistrationPointPeripheralContainer<ISPIPeripheral>` as its base
  class but never calls `RegisteredPeripheral`: the word `Transmit` does not
  occur in the file. It bit-bangs the transfer onto GPIO pins and samples MISO
  from a GPIO pin, which is how it interworks with PIO. A slave registered at
  the null registration point, which is exactly what hauksbee's SPI bridge is,
  is never invoked. So `spi_controllers` is empty on purpose: listing a
  controller would install a bridge that silently sees nothing and make a bound
  SPI sensor read as "answered zeroes". The reproduction is kept as an ignored
  test, `rp2040_spi_bridge_probe` in `tests/renode_rp2040_bus.rs`, with the
  reason as its ignore message. Fixing it needs either the upstream model
  dispatching to its registered peripheral or a pin-level SPI bridge on the
  hauksbee side.
- **PIO. Absent.** Upstream models PIO as an extra CPU backed by a native C++
  library shipped prebuilt for x86-64 only, which will not load into an arm64
  Renode. `rp2040_pio.cs` is vendored because the SIO, GPIO, SPI and ADC models
  reference its types and will not compile without it; it is never instantiated.
  Any firmware whose observable behaviour goes through PIO (the usual WS2812
  driver, PIO-driven I2C/SPI, `pico_stdio_pio`) produces nothing observable.
- **Second core. Declared and un-halted, but unproven.** The platform declares
  both Cortex-M0+ cores, `cpu1` starts un-halted, and the SIO is genuinely
  shared (its GPIO registers are one set of state behind a single model), so
  observation is core-agnostic by construction and a pin driven from core 1
  would be seen. What is not claimed is that a real multicore launch works: the
  SDK's `multicore_launch_core1` hands off through the SIO FIFO and the boot
  ROM's core-1 wait loop, and no two-core firmware has been run against this.
  State queries (`cpu_path`) use core 0. Treat single-core firmware as the
  supported case.

The scheduler dispatches `backend = "renode:rp2040"` (alias `renode:pico`) to
`crates/hauksbee-mcu/db/mcu/rp2040.soc.toml`, and the built-in
`crates/hauksbee-models/db/mcu.toml` entries for both the bare `rp2040` and the
`rpi_pico` module name that backend, so a bound RP2040 board is routed into
co-sim with no user model layer needed.

### Support bundles: shipping peripheral models Renode does not have

RP2040 is the first descriptor to use a mechanism other parts can reuse, so it
is worth stating separately from RP2040 itself. Renode compiles C# at run time:
`include <file.cs>` on the Monitor drives its bundled compiler and registers the
resulting peripheral types, which is already how the I2C/SPI bridge peripherals
get into a machine. A **support bundle** is that mechanism scaled up to a whole
SoC: a set of `.cs` peripheral models plus the data files the platform reads (an
SVD, a boot ROM image), embedded in the hauksbee binary, unpacked to a temp
directory, and `include`d before the platform description parses.

A descriptor opts in with `[soc] support_bundle = "<name>"`. At machine
bring-up, before `machine LoadPlatformDescription`, the backend unpacks the
bundle into a fresh temp directory, runs `path add <dir>` so bare `@name`
references inside the bundle's own `.repl` resolve without path rewriting, and
`include`s each C# source **in the declared order**, because Renode's C#
include is order sensitive: a later file referencing an earlier file's type
fails to compile if the order is wrong. The literal `{support}` token in the
descriptor's `platform_repl`, `extra_setup` and `post_load_setup` is then
substituted with the unpacked directory, so the descriptor stays readable
(`@{support}/rp2040.repl`) while the paths Renode sees are absolute. The
directory is removed when the backend drops, and it is per-process and
content-addressed so parallel test binaries never share or race one. The
mechanism is `crates/hauksbee-mcu/src/renode/support.rs`; provenance and
licences for the RP2040 bundle's contents are in
`crates/hauksbee-mcu/db/mcu/rp2040/README.md`.

Files are unpacked rather than referenced from the source tree because an
installed `hauksbee` is one binary with no repository beside it. It is the same
decision the `include_str!`-ed `db/mcu/*.soc.toml` descriptors already make,
applied to files that must exist on disk because Renode, not hauksbee, is the
one reading them.

**The cost is real and worth knowing before you wonder why a suite is slow.**
Every machine creation compiles the whole bundle: for RP2040 that is 23 C#
sources, about 377 kB, and Renode's compiler runs on each new machine rather
than once per process. Measured on the 2.00 s corpus run above: 15.5 s of
wall clock in total, of which the simulation itself accounted for 7.3 s, so
bring-up costs roughly eight seconds before any firmware instruction executes.
The three RP2040 integration suites therefore take tens of seconds each,
measured between 8 s and 28 s per suite across runs for one to six tests, where
the whole five-test STM32 suite takes 6 s to 11 s.
Nothing is wrong when that happens; the trade is paying compile time for a
platform that does not otherwise exist.

### ESP32 in Renode, honestly

ESP32 remains **not usable in Renode** as of 1.16.1: no `esp32.repl` /
`esp32c3.repl` ships, and the ESP32 SoC peripheral set is unmodelled. That
gap is exactly why the `qemu:` backend (Espressif QEMU fork) serves the
ESP32 family rather than Renode. Both Xtensa (ESP32) and RISC-V (ESP32-C3)
ESP32 parts are proven through QEMU above.

### Co-sim fidelity notes (debugging all-zero or "never driven" results)

- **Crystal-clocked boards.** A crystal/resonator is bound high-impedance
  (`ComponentKind::Ignore`); the clock comes from the MCU model. The binder
  catches a crystal-like part before the passive first-char heuristic
  (`crates/hauksbee-engine/src/binder.rs`, `is_crystal_like`), because a
  reference such as `Crystal1` starting with `C` and a value such as `16MHz`
  would otherwise parse as a 16-megafarad capacitor: an absurd cap makes the
  solve collapse and drives **every** net to 0 V / "never driven" on
  essentially any crystal-clocked MCU board. The two load caps are genuine
  passives and stay. If you see board-wide all-zero co-sim voltages, check
  `--report` for any passive with an absurd capacitance. See
  [`LIMITATIONS.md`](../about/LIMITATIONS.md) Fixed #4.
- **ESP32 GPIO output needs the reviewed QEMU source build for arbitrary
  firmware.** Espressif's prebuilt discards output state and therefore uses the
  named mailbox fallback. `install-sims.sh --qemu-patched-source` removes that
  output contract after a live capability probe. GPIO input remains a firmware
  mailbox contract; see [`LIMITATIONS.md`](../about/LIMITATIONS.md).

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

## Talking to the board from your own software (`--serial-attach`)

A co-sim that only reports at the end is a closed box. `--serial-attach` opens a
host serial port onto the emulated MCU's UART, so software on your own machine
drives the simulated board the way it drives real hardware over USB serial: the
same pyserial script, the same vendor configuration tool, the same `minicom`
session, unchanged.

```bash
hauksbee run board.kicad_pcb --firmware fw.elf --serial-attach --serial-wait 30 --seconds 10
# host serial: pty on /dev/ttys006
# host serial: attach your own software with one of:
# host serial:   python3 -c "import serial; s=serial.Serial('/dev/ttys006', 115200, timeout=1); ..."
# host serial:   minicom -D /dev/ttys006
# host serial:   screen /dev/ttys006 115200
# host serial: wired to the UART of U1
# host serial: waiting up to 30s for a peer to open /dev/ttys006 ...
# host serial: peer ATTACHED on /dev/ttys006
# host serial: peer DETACHED (92 byte(s) in, 8 byte(s) out so far); the co-sim keeps running
# host serial: session over /dev/ttys006 (6.000s simulated in 10.02s wall): 92 byte(s)
#              host->MCU, 8 byte(s) MCU->host, 1 peer attach(es)
```

Then, in another terminal, ordinary client code:

```python
import serial
s = serial.Serial("/dev/ttys006", 115200, timeout=2)
s.write(bytes([0x05]))          # this firmware's "identify yourself" command
print(s.read_until(bytes([0x04])).hex(" "))   # -> 06 30 31 30 04  (ACK, v0.1.0, TRN_END)
```

Nothing in that script knows it is talking to a simulator.

**Why a pty and not a socket.** A pseudo-terminal has a device path, so
unmodified software works unmodified; a TCP socket would make you rewrite your
tool, and a closed-source vendor tool cannot be rewritten at all. `pty` is
therefore the default. `--serial-transport tcp` gives a loopback port for a tool
that already speaks sockets, or for a platform with no pty.

| flag | what it does |
|------|--------------|
| `--serial-attach` | open the endpoint and run the live co-sim (needs `--firmware`) |
| `--serial-transport pty\|tcp` | how the host attaches; `pty` (default) needs no changes to your tool |
| `--serial-wait SECS` | hold the co-sim at t=0 until your tool opens the port, then fail loudly if it never does |
| `--serial-no-pace` | free-run before the first peer, then use a compressed wall/sim clock for load-independent host timing |
| `--serial-mcu REF` | which MCU's UART to bridge on a multi-MCU board (default: all of them) |

**What the session guarantees, and what it says when it cannot.**

- **Sim time is paced to wall-clock time** by default, because a host tool's
  `timeout=2` and `time.sleep(0.1)` are wall-clock quantities. A free-running
  AVR co-sim would be over before the script's first write. `--serial-no-pace`
  free-runs only before the first attachment, then uses a fixed 1/20 compressed
  wall/sim mapping. That mapping remains continuous through later handoffs.
- **Output produced before you attach is held** (bounded, 64 KiB) and flushed on
  attach, so a boot banner is not lost to the gap between reading the device
  path and starting your tool. Past the cap the newest bytes are dropped and
  counted, and the summary says so; it is never silent.
- **Observed attach and detach transitions are printed.** A peer may attach
  late, disconnect mid-run, and reattach; the co-sim keeps running throughout.
  A close+reopen completed between two PTY polls is one continuous observation,
  so the compressed wall/sim clock deliberately remains continuous too.
- **A host record longer than the emulated RX fifo arrives whole.** `uart_write`
  queues bytes and meters them under the emulated UART's own flow control (the
  fifo-truncation defect described above), so a 90-byte or 4 KiB single write is
  delivered in order rather than truncated at 64. If the firmware genuinely does
  not drain its receiver, the session reports the overflow per MCU rather than
  pretending the bytes arrived.
- **The link is byte-transparent for a configured serial peer**: NUL, `0x0A`
  and `0x0D` pass through untouched in both directions. The endpoint applies
  raw mode on every observed attach, and each process must configure its own tty
  before writing (normal behavior for pyserial, minicom, screen, and vendor
  serial clients). A close+reopen hidden between probes cannot be reconfigured
  by the endpoint before that replacement process's first write.
- **A session nobody attached to is reported as such**, not as a quiet success.

Honest limits: every PTY process must configure its serial fd before writing and
must remain open until its command is consumed (use a
protocol acknowledgement or a transport drain); a write followed by immediate
close is not delivery evidence on Darwin. There is no baud-rate emulation (the endpoint is transparent, and the
firmware's own UART divisor sets the pace at which simavr delivers bytes), no
modem-control lines (no RTS/CTS/DTR, no hardware flow control), and no pty on
Windows, where `--serial-transport tcp` is the only option. `--serial-attach` and
the report flags (`--headless`, `--json`, `--plain`) are separate surfaces: a
serial session prints its own narration and summary, not the co-sim activity
table.

Proofs: `crates/hauksbee-mcu/tests/host_serial_pty.rs` (transport: binary
transparency with a cooked-mode premise test, late attach, disconnect and
reattach, an oversized single write, hangup on teardown) and
`crates/hauksbee-engine/tests/host_serial_cosim.rs` (the whole path against a
real emulated ATmega328P, driven by a peer that only ever sees the printed
device path).

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
and answers `i`/`v` commands. It also mirrors output into the legacy mailbox so
the same fixture proves compatibility with Espressif's unpatched prebuilt. The
independent `esp32_native_gpio` fixture contains no mailbox at all and proves
the patched backend observes ordinary ESP-IDF `gpio_set_level` writes.

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
#    For arbitrary-firmware GPIO output observation instead of the mailbox
#    compatibility path: scripts/install-sims.sh --qemu-patched-source

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

Backend: `RenodeConfig::nrf52840()` (ports gpio0/gpio1, OUT register at 0x4
peripheral-relative, uart0). That 0x4 is the datasheet's block-relative 0x504
minus the 0x500 register window Renode registers `gpio0`/`gpio1` at, the footgun
the descriptor header spells out; a read at 0x504 logs an unhandled offset and
returns 0, so no edge could ever fire. Firmware: any nRF52840 ELF (e.g. a
Zephyr blinky). Renode
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
description = "SiFive FE310-G002 (RISC-V RV32IMAC, HiFive1)"

[models.match]
value_re = "(?i)^(FE310|FE310-G00[0-9]|HiFive1)"
mpn_re   = "(?i)^FE310"

[models.params]
# Renode backend: brings up sifive-fe310.repl. One 32-bit GPIO port (the
# platform names it gpioInputs); roles "p0<bit>".
backend = "renode:sifive_fe310"

[models.pins]
"1"  = "vss"
"2"  = "vdd"
"3"  = "p019"       # HiFive1 LED is on GPIO 19
"4"  = "p021"       # second GPIO for blink
"5"  = "p017_txd"   # uart0 TX
"6"  = "p016_rxd"   # uart0 RX

[models.ratings]
max_pin_current_a = 0.02
max_current_a = 0.1
max_voltage_v = 3.6
```

Backend: `RenodeConfig::sifive_fe310()` (one 32-bit GPIO port, registered
by the stock platform as `gpioInputs`, output value register at 0x0C,
uart0). Renode ships `platforms/cpus/sifive-fe310.repl`
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
  family). The shipped STM32F072 map uses its stock modeled converter for
  external inputs 0..7. The stock STM32F103/F4/nRF52/FE310 platform
  descriptions model no ADC at all, so
  `set_analog_in` delivers counts only where a
  `RenodeConfig::adc_channels` recipe says how (a Monitor feed command for
  a modeled ADC, or a `WriteDoubleWord` into the result word the firmware
  reads). hauksbee records an unmapped channel's drop and surfaces it on every
  report path that runs the external backend (run text, `--plain`, `--json`
  `CosimJson.adc_dropped` + notes, the hauksbee-ci human/JUnit/GitHub reports,
  and the interactive TUI's co-sim pane), naming the channel, MCU, and net,
  never a stderr-only whisper. The synchronous web report refuses this backend
  before the signal exists and points to live/CLI co-sim.
  The STM32 demo couples through the GPIO/LED path, not the ADC, so it
  carries no map. The AVR backend's ADC injection is fully wired and exact.

- **One firmware per machine.** The engine loads one firmware ELF/HEX for
  all MCUs on a board, matching the existing AVR behaviour.

### QEMU (ESP32 family) specific limitations

- **GPIO output no longer requires a firmware contract when the reviewed
  source build is installed.** Run
  `scripts/install-sims.sh --qemu-patched-source`; it fetches the exact pinned
  Espressif commit, applies the reviewed patch, builds both architectures, and
  live-probes paired `gpio-out`/`gpio-enable` QOM capabilities on ESP32,
  ESP32-S3, and ESP32-C3 before committing the install. The backend then reads
  real GPIO OUT and ENABLE state. The prebuilt upstream release remains usable,
  but output falls back to the demo's mailbox with a named runtime limitation.
  GPIO input is still injected through `hauksbee_gpio_in`: the fork drops writes
  to `GPIO_IN_REG`, so arbitrary firmware cannot yet consume host-driven pins.

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

- **Descriptor bank coverage still bounds which GPIOs are observable.** The
  shipped `esp32s3` SoC descriptor declares bank `'0'` only, so GPIO32..48 (bank
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
