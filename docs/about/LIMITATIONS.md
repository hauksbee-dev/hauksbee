# Limitations

What is open today, and the two structural ceilings that will not close.

## Structural ceilings

**Model availability caps analogue coverage.** A part Hauksbee cannot model
it cannot simulate, and many vendor models are NDA or encrypted. Board-level
questions (does a rail sag, does a part exceed its rating, does firmware drive
what it thinks it drives) are answerable from datasheet-grade terminal
behaviour, which is what the model library holds. The gap is named per part
(`hauksbee run` prints the ratio and the unresolved references), closed with
one TOML file ([MODELS.md](../models/MODELS.md)), and gateable with the
`model_coverage` assertion ([CI.md](../ci/CI.md)).

**A false positive costs more than a miss.** A check that misfires can be
overruled per finding with a waiver (reason and expiry required, waived
findings printed, never hidden). Every check must stay silent on real shipped
boards before it lands. A run that cannot produce a trustworthy answer exits
3 instead of passing.

## Open limitations

**Windows.** The native artifact compiles Renode and QEMU backends but not
AVR/libsimavr. There is no pty, so `--serial-attach` requires
`--serial-transport tcp`.

**A co-sim chunk no fallback can solve holds stale voltages.** A chunk whose
primary solve fails is retried on a ladder (bounded step, backward Euler,
cold-start backward Euler, quartered chunk); a rescued chunk is recorded in
`cosim.fallback_windows` with its method and a measured `error_estimate_v`.
A chunk no rung carries keeps the previous chunk's voltages; the run names the
failed spans, `--strict` fails on them, and values inside are invalid. The
usual cause is structural: unresolved active parts leaving nodes floating, or
conflicting rails.

**Clock rate.** `simavr:atmega328p`, `renode:rp2040`, `renode:stm32f103`,
`renode:stm32f4_discovery`, `renode:nrf52840` and `renode:sifive_fe310` are
measured at the part's rate. `renode:stm32f072` is declared but unmeasured.
The `qemu:esp32*` family is wall-clock paced (about 0.94x on an idle host,
load dependent) and says so through `timing_limitations`. The F103's TIMx
blocks stay at the post-PLL 72 MHz so stock HAL projects boot; every F103 run
says so. See [MCU.md](../cosim/MCU.md#clock-fidelity).

**Watchdog.** Only `simavr` reboots a starved watchdog like silicon.
`renode:stm32f103` resets once then stops; `renode:nrf52840` never fires;
the ESP32 family's timer-group watchdogs are disabled at launch; the other
Renode parts are unverified. Each is reported per run
(`watchdog_limitations`, `watchdog_resets`).

**Power-up state.** No POR/BOR (a brownout is caught by a rail assertion
while firmware keeps running); straps are not sampled at the reset latch (the
strap lint is static); no fuse/option bytes/eFuse/UICR (a factory-fuse
ATmega328P at 1 MHz is simulated at 16). The STM32F103 HSE/PLL readiness is
gated on an assembled crystal (presence and timing, not oscillator physics).

**Parallel EEPROM** (AT28C256) models the bus, stored bytes, protection, page
boundaries and the 150 us inter-byte deadline, with busy polling; not the
charge-pump physics, endurance or 12 V erase.

**I2C/SPI slave co-sim.** Exact on AVR; on Renode through C# bridges wherever
a descriptor names controllers; on QEMU-ESP32 through a firmware mailbox. A
slave on a controller-less platform is recorded unexercised and a
`peripheral` assertion against it fails. SPI framing without a resolvable CS
net falls back to chunk boundaries (`spi_framing` reports the tier). Renode
ADC injection exists only where a descriptor maps channels (F072 inputs 0..7,
RP2040 inputs 0..3); unmapped channels drop loudly. QEMU ESP32 SAR ADC, I2C and
SPI are mailbox contracts; GPIO output is register-backed only on the patched
source build ([SIMULATORS.md](../cosim/SIMULATORS.md)).

**nRF5340** has no co-sim backend (Renode has no SPU, nRF53 IPC or network
core model). nRF52840 is the closest proven part.

**PCB-only extraction has no pin functions**; multi-unit packages fall back
to db pin maps.

**Device decode** is per part (CYPD3177, BQ2407x) and does not read silk
labels ([DEVICE_DECODE.md](../checks/DEVICE_DECODE.md)).

**KiCad 5 legacy `.sch`** is not read; the layout still is.

**KiCad 10 boards** (format >= `20260000`) keep a `version_warning`: the
name-only nets and keyhole antipads are handled, but the finding set is not
yet exact native-DRC parity, so shorts on those boards are demoted and do not
fail strict gates. `.kicad_pro` clearance rules are read. Cross-check with
`--oracle`.

**Eagle pours.** The `.brd` stores a pour's outline and settings, not its
fill, so pour-to-copper pairs are not checked; overlapping same-rank pours of
different signals are reported as shorts, which over-reports on some boards.
A tie drawn as a supply symbol is read from the companion `.sch` as context
only ([SHORTS.md](../checks/SHORTS.md)).

**Gerber footprint inference on stripped films** (no X2 attributes): pads are
windowed from the package name with a 4 mm fallback, so stitching vias can
inflate pad counts. Re-export with X2 attributes or supply the native layout
([GERBER.md](../ingest/GERBER.md)).

**Capacitor ESR/ESL** stays opt-in (`[decoupling] parasitics = true`).
