# Add a microcontroller family

The sibling of [add-an-mcu-variant.md](add-an-mcu-variant.md), for a part
whose family Hauksbee does not ship. Which tier you are in is decided by the
emulator, not the part.

| Tier | Situation | What you write | Rust? |
|---|---|---|---|
| A | supported family, sibling part | one `.soc.toml`, one routing entry | no |
| B | new family, but Renode/QEMU already models the part | the same two files, a firmware fixture, a test | no |
| C | the emulator has no model of the part | tier B plus vendored peripheral models and a support-bundle registration | one static list in `src/renode/support.rs` |

```bash
ls ~/renode-portable/Renode.app/Contents/MacOS/platforms/cpus/   # macOS; Linux: ~/renode-portable/platforms/cpus/
hauksbee models list --builtin
hauksbee doctor --backends
```

A `.repl` for your part or a close sibling puts you in tier B.

## Resolution

`SocConfig::resolve` searches `$HAUKSBEE_MCU_DIR/<part>.soc.toml`, then
`~/.config/hauksbee/mcu/<part>.soc.toml`, then the embedded descriptors. A
spec can name a directory without the env var:

```toml
[mcu]
descriptor_dir = "mcu"   # relative to the spec file; HAUKSBEE_MCU_DIR still wins
```

An override beats a built-in of the same name; a descriptor that exists but
fails validation aborts the run naming the file and field.

## The field reference

Schema: `RenodeSoc` and `QemuSoc` in `crates/hauksbee-mcu/src/soc.rs`, both
`deny_unknown_fields`.

### `[soc]`, Renode

| Field | Required | Meaning |
|---|---|---|
| `backend` | yes | `"renode"` |
| `machine` | yes | the Monitor machine name, e.g. `"f072"` |
| `platform_repl` | yes | single line: a Renode path (`@platforms/cpus/x.repl`, `@/abs/mine.repl`, `@{support}/x.repl`); multi-line: inline `.repl` source written to a temp file |
| `support_bundle` | no | peripheral models compiled before the platform loads (tier C) |
| `cpu_path` | yes | e.g. `"sysbus.cpu"` |
| `uart` | no | the UART bridged to a host socket; omit for none (blank is refused) |
| `frequency_hz` | yes | the part's core clock; cross-checked against the platform's `cpu PerformanceInMips` (MHz) and, on Cortex-M, `nvic systickFrequency` (Hz); a mismatch or a missing declaration is refused |
| `expected_e_machine` | yes | `EM_ARM`, `EM_RISCV`, `EM_XTENSA`, `EM_AVR`; a wrong-architecture ELF is refused |
| `mcu_label` | yes | the human name in reports |
| `watchdog_limitation` | no | one sentence, rendered verbatim on every report surface; omitting it claims silicon-faithful watchdog behaviour |
| `timing_limitation` | no | likewise for timing; omitting it claims a firmware delay costs the right virtual time |
| `extra_setup` | no | Monitor commands after the platform loads, before firmware |
| `post_load_setup` | no | Monitor commands after firmware load; `{cpu}` substitutes `cpu_path` |
| `[soc.clock_control]` | no | board-presence and virtual-time commands (`{present}`, `{micros}`) for a platform clock model (the F103's HSE/PLL readiness) |

Declare the **reset-default** clock; a stock platform has no clock tree to
follow a firmware's PLL bring-up. The smallest legal Renode platform:

```toml
platform_repl = """
using "platforms/cpus/stm32f072.repl"

nvic:
    systickFrequency: 8000000

cpu:
    PerformanceInMips: 8
"""
frequency_hz = 8_000_000
```

The `{support}` token in `platform_repl`, `extra_setup` and `post_load_setup`
is replaced with the unpacked bundle directory; using it without a
`support_bundle` is refused.

### `[[soc.ports]]`

| Field | Required | Meaning |
|---|---|---|
| `letter` | yes | the port letter in a pin id (`'C'`, or `'0'` for a single-bank part) |
| `peripheral` | yes | the platform's name, **without** `sysbus.` |
| `odr_offset` | yes | byte offset of the output-data register |
| `width` | yes | 1 to 32 |
| `dir` | no | `{ offset, encoding }`, encoding `moder` (2 bits/pin, `0b01` = output; AF not counted), `stm32f1_crl_crh` (CRL at `offset`, CRH at `offset + 4`), or `dir_bits` (1 bit/pin) |

Omit `dir` and every output-state change reports as a drive. A wrong map
suppresses edges silently. `moder`/`stm32f1_crl_crh` on a port wider than 16
is refused.

### `[soc.i2c]`, `[soc.spi]`

`controllers` (array of names) and, for SPI, `extra_repl` (a peripheral
definition spliced in). Name only a controller you have watched a byte cross:
a bridge on a model that never dispatches answers zeroes. Left empty, a bound
sensor is recorded UNEXERCISED and fails a CI `peripheral` assertion.

### `[[soc.adc]]`

`channel`, `full_scale_volts`, `max_count`, and exactly one of
`monitor_command` (with `{count}`, `{millivolts}` or `{volts}` substituted per
chunk) or `memory_word` (an address written with the count). A command with
no token is a lint finding. Zero `max_count`, non-positive
`full_scale_volts`, or a duplicated `channel` is refused. Unmapped channels
drop loudly.

### `[soc]`, QEMU

`backend = "qemu"`, `arch` (`xtensa` | `riscv32`), `machine`, `gpio_qom_path`
(device exposing paired `gpio-out`/`gpio-enable` capabilities on the patched
build), `icount_shift`, `frequency_hz`, `expected_e_machine`, `mcu_label`,
`[[soc.banks]]` (`letter`, `width`, `peripheral_out_reg`,
`peripheral_enable_reg`, mailbox `out_reg`/`in_reg`), `[soc.i2c].buses`
(mailbox bus names). The QEMU backend exists for the Espressif fork; a
descriptor cannot add a machine to somebody else's emulator.

### What the loader refuses

Unknown `backend`; backend disagreeing with the resolving spec; a backend
this build lacks; empty `platform_repl`, `machine`, `cpu_path` or `uart`;
`frequency_hz` 0; `{support}` without a bundle; a clock declaration
disagreeing with `frequency_hz` or absent; unknown `expected_e_machine` or
`support_bundle`; a port of width 0 or > 32; duplicate port letters; a `dir`
encoding narrower than its port; duplicate controller names; an ADC entry
with neither or both injection forms, a duplicate channel, or bad scale
values; unknown QEMU `arch`; any unknown field. `hauksbee models lint`
additionally reports a blank `mcu_label` and a tokenless ADC command.

## Iterate with `models lint`

```
$ hauksbee models lint crates/hauksbee-mcu/db/mcu/stm32f072.soc.toml
soc descriptor '...stm32f072.soc.toml': ok (renode:stm32f072)
  part: STM32F072 (ARM Cortex-M0) on machine "f072"
  gpio port A: 16 pins on sysbus.gpioPortA, output state at +0x14, direction at +0x0 (Moder)
  adc channel 0: 0..4095 counts over 0..3.3 V via monitor "sysbus.adc SetDefaultValue {millivolts} 0"
  note: no i2c or spi controllers: a bound sensor is recorded UNEXERCISED ...
1 item(s) checked, 0 finding(s): clean
```

It runs the same loader a co-sim runs and prints back what the descriptor
will do. A finding exits 2 and names the field.

## Tier B, worked: STM32F072

The shipped `crates/hauksbee-mcu/db/mcu/stm32f072.soc.toml` was built this way.

1. **Read the platform, not the datasheet.** `cat platforms/cpus/stm32f072.repl`
   and its `stm32f0.repl` include: `gpioPortA..F` at `0x48000000`, ODR at
   `0x14`, MODER at `0x00`, `usart1`, `adc: Analog.STM32F0_ADC`. Copying the
   F103 descriptor (ODR `0x0C`) would boot, print nothing, and report every
   pin never driven.
2. **Two firmware images** from one source (`testdata/firmware/stm32f072_blinky/`):
   `blinky.elf` toggles PC6, holds PA5, prints on USART1; `quiet.elf`
   (`-DQUIET`) configures nothing. The quiet image is what proves the bridge
   is not fabricating edges.
3. **Read the offsets off a running machine** with a `.resc` script
   (`mach create`, `LoadPlatformDescription`, `LoadELF`, `emulation RunFor
   "0.3"`, `sysbus.gpioPortC ReadDoubleWord 0x14`, `peripherals`). Check that
   consecutive windows alternate (`0x40, 0x00, ...`) and that `quiet.elf`
   reads zeros.
4. **ADC**: typing `sysbus.adc` at the Monitor lists its methods;
   `SetDefaultValue (valueInmV, channel)` takes millivolts, so the recipe is
   `monitor_command = "sysbus.adc SetDefaultValue {millivolts} 0"`. Proven
   against firmware reads (1650 mV -> 0x800). Internal channels (temperature,
   VREFINT, VBAT) stay unmapped.
5. **Write the descriptor** with the clock, ports (`dir = { offset = 0x00,
   encoding = "moder" }`), ADC channels 0-7, empty controller lists (the
   `STM32F7_I2C` model there is unproven), and a `watchdog_limitation`.
6. **Install and lint** under `~/.config/hauksbee/mcu/`.
7. **Route the part**: a `[[models]] kind = "mcu"` entry with
   `backend = "renode:stm32f072"` and a package-faithful pin map (cite the
   datasheet table). `hauksbee models resolve <board> --models-dir <dir>`
   shows it winning.
8. **Test** (`crates/hauksbee-mcu/tests/renode_stm32f072.rs`): a no-emulator
   layer pinning offsets, widths, encodings and ADC tokens; a live two-sided
   boot asserting alternating PC6 edges, the exact 22-byte banner, the exact
   configured-output set (PC6, PA5, not the AF pins), no edges/bytes/outputs
   from `quiet.elf`, the ADC round trip, and an unmapped channel landing in the
   dropped list. Skips name what is missing.

## Tier C: no platform exists

Renode compiles C# at run time (`include <file.cs>`). A **support bundle** is
a set of `.cs` peripheral models plus the data files the platform reads (SVD,
boot ROM), embedded in the binary, unpacked to a temp directory at machine
creation, and `include`d in declared order before the platform parses.

```toml
support_bundle = "rp2040"
platform_repl = "@{support}/rp2040.repl"
extra_setup = ["sysbus LoadELF @{support}/bootrom.elf"]
```

The file list and include order live in
`crates/hauksbee-mcu/src/renode/support.rs` (order is load-bearing: a file
referencing an earlier file's type fails to compile otherwise; a file that is
never instantiated may still be required for types). The RP2040 bundle
(`crates/hauksbee-mcu/db/mcu/rp2040/`) is the shipped example; its `README.md`
records origin, upstream commit, licence texts, content hashes, refresh
procedure and what is deliberately not vendored (PIO, an x86-64-only native
library). Cost: the bundle compiles on every machine creation, about eight
seconds for RP2040's 377 kB.

## Where TOML stops

| You want | Route |
|---|---|
| a part on an existing platform | `.soc.toml` |
| a platform fix (extra peripheral, clock register) | inline `platform_repl`, or `[soc.spi].extra_repl` |
| a bring-up sequence | `extra_setup` / `post_load_setup` |
| peripheral models the emulator lacks | a support bundle plus one list in `support.rs` |
| a different emulator | a new `Mcu` implementation (Rust) |
| a new AVR part | nothing; simavr's part database does it |
| a new ESP32-family part | the Espressif fork must model it first |

## Contribution checklist

- `hauksbee models lint <part>.soc.toml` is clean; every offset was read off a
  running machine and the transcript is in the file's header comment.
- `dir` present only with a verified read-back; controllers named only when
  a byte crossed; ADC tokens match the model's method signature; internal-only
  channels and unmodelled peripherals stated as gaps.
- Each coupling (GPIO out/in, UART, ADC, I2C, SPI, direction, timers)
  labelled proven end-to-end, boot-only, or absent (with why), in the header
  and in [MCU.md](../cosim/MCU.md)'s matrix.
- A no-emulator test layer plus a two-sided live test whose skips name what
  is missing; fixture source and build recipe checked in.
- Vendored files: README with provenance, licence text beside the files,
  byte-for-byte copies with fixes sent upstream.
- A routing entry with a pin map; to ship built-in, add the descriptor to
  `EMBEDDED` in `crates/hauksbee-mcu/src/soc.rs` and the entry to
  `crates/hauksbee-models/db/mcu.toml`.

Maintained upstream: the descriptor schema, validation, resolution order, the
support-bundle mechanism, the `Mcu` contract. Yours: the offsets and
capability claims for your part, and any vendored models. An override
directory in your own repository, named by `[mcu] descriptor_dir`, is a
perfectly good end state.
