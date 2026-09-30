---
name: firmware-cosim
description: Boot real compiled firmware on a board's emulated MCU against the solved circuit with Hauksbee, capture UART and waveforms, and assert on the result. Use when asked to run firmware on a board, check what the firmware does to rails or pins, test boot behaviour, or decide whether a firmware co-sim result is trustworthy.
---

# Firmware co-simulation

Hauksbee runs the firmware image on an emulator and couples its pins to the
analog solve of the board. A result is only as good as three things: the MCU
bound to a real backend, the backend is installed, and the assertion can fail.

## 1. Check the backend before promising a run

```bash
hauksbee doctor            # or: hauksbee doctor --json
hauksbee models list --builtin
```

| backend | parts | how it runs |
|---|---|---|
| `avr` | ATmega328P and Arduino boards built on it | libsimavr linked in-process (default source builds; not in the permissive bundle) |
| `renode` | STM32F072, STM32F103, STM32F4 Discovery, nRF52840, SiFive FE310, RP2040 | external Renode (`hauksbee install renode`) |
| `qemu-xtensa`, `qemu-riscv32` | ESP32, ESP32-S3, ESP32-C3 | Espressif QEMU fork (`hauksbee install esp-qemu`); Homebrew's mainline QEMU does not work |

Proof level differs by part. `docs/cosim/MCU.md` ("Per-architecture support
matrix") says which rows are proven end to end and which peripherals each
backend implements. For example, STM32F103 in stock Renode models no ADC, so
ADC injections there are dropped and reported. ESP32-S3 boots, but has no
app-level proof yet. ESP32-C6/H2 and nRF5340 are not supported.

Over MCP, `list_capabilities` returns the same backend probe.

## 2. Give it firmware

`--firmware` (CLI) or `firmware =` (spec) accepts:

1. a compiled `.elf` or `.hex` (most explicit);
2. a zip: a `.pio/build/<env>/firmware.elf` wins over a stray `.elf`, which
   wins over a `.hex`, and the report says which image ran;
3. a PlatformIO project directory, built with your own `pio run`. Building
   runs the project's scripts, so only build projects you trust.

A directory with neither `platformio.ini` nor a built image is an error that
says so.

## 3. Run it headless

```bash
hauksbee run BOARD --firmware fw.elf --headless --seconds 0.5 \
  --probe D13,+5V --probe-csv probe.csv
```

Output: the most active nets (min, max, toggles), the numerical
qualification, timing coverage per MCU (for example `U1 (simavr:atmega328p):
edge timestamps +/-0.062 us ... cycle-exact stamps`), the UART bytes, and the
evidence summary. `--chunk-us N` narrows the solver chunk (100 us in this
run) when you care about pulses shorter than it. `--serial-attach` opens a
host pty onto the MCU's UART for your own serial tools.

## 4. Assert on it

Put it in a spec so the result gates (see the `write-a-ci-spec` skill):
`uart` (`contains`, or `matches` for a regex), `toggle` (`freq_hz`,
`tolerance`), `boot_coverage` (a control net driven to `min` volts within
`deadline_ms` of reset), `voltage` and `rail_window` under firmware load,
and `peripheral` for attached I2C/SPI models.

```bash
hauksbee-ci run crates/hauksbee-ci/examples/blinky.toml         # GREEN, 4/4
hauksbee-ci run crates/hauksbee-ci/examples/boot_gate_fail.toml # RED: GATE_CTRL never driven
```

## 5. What counts as a real pass

Call it a pass only if all of these hold:

- The exit code is 0 and, in JSON, `passed`, `assertions_passed` and
  `run_valid` are all true.
- The MCU ran on its own backend: `substitutions` is empty, and the timing
  coverage line names the expected `backend:part`.
- `timing_refusals` is empty. Any entry makes the run INVALID.
- The firmware visibly ran: a UART banner, toggles on a driven net, or a
  `boot_coverage` pass. A rail that holds with the MCU idle is not evidence
  about the firmware.
- The asserted rail is not fed by an `ideal` supply.
- Nothing the assertion depends on was dropped or unexercised: an ADC
  injection on a platform with no ADC, or an I2C/SPI slave on a controller-less
  platform. These are reported in the run text and in the JSON coverage
  fields (`coverage_warnings`, `assumptions`).
- Evidence is `clean`, or `qualified` with the caveats reported to the user.

Refusals are answers. Firmware on a board with no bound processor exits 3 with
`refused claim: firmware behavior on this board`. Over MCP it returns
`status: "invalid_for_analysis"` with `isError: false`. A missing emulator
exits 1 with install instructions. Report either as "not run", never as a
failure of the firmware.
