# Add an MCU variant: an STM32 sibling via `.soc.toml`, no recompile

**Goal.** Make hauksbee co-simulate firmware for an MCU part it does not ship,
by writing two small TOML files: a SoC descriptor (validated fail-loud,
resolved from a user directory at runtime) and a `[[models]]` routing entry
that maps your board's part value to it. **No recompile is the whole point.**
The worked example is an STM32F103 sibling on the Renode backend. The shipped
descriptor to start from is `crates/hauksbee-mcu/db/mcu/stm32f103.soc.toml`,
and the schema/loader is `crates/hauksbee-mcu/src/soc.rs`.

**What you need:** the part's reference manual (GPIO register offsets), a
Renode platform (`.repl`) that models the part, and the part's peripheral
names in that platform.

## How resolution works

At co-sim time an MCU is named by a `backend:part` spec, for example
`renode:stm32f103`, the backend string the binder attached to your board's
MCU (Step 4 below is where that string comes from). The scheduler's backend
instantiation hands the spec to `SocConfig::resolve` (in `soc.rs`), which
searches, highest priority first:

1. `$HAUKSBEE_MCU_DIR/<part>.soc.toml`, an explicit override directory,
2. `~/.config/hauksbee/mcu/<part>.soc.toml`, your standing descriptor dir,
3. the embedded built-ins (shipped via `include_str!`, so the binary stays
   self-contained while the db file stays the single source of truth).
   `hauksbee models list --builtin` prints them.

Drop `mypart.soc.toml` in (1) or (2) and every co-sim that names
`renode:mypart` loads it. That is the entire installation procedure. Two
properties are guaranteed:

- **Override beats builtin.** A `stm32f103.soc.toml` in (1) or (2) wins over
  the shipped F103 descriptor, the same layering rule as the model library.
- **Fail loud, never skip.** If a descriptor file *exists* for the requested
  part, hauksbee parses it, and any validation error aborts the run, naming
  the file and the failing field. hauksbee never silently skips an invalid
  override in favor of a lower-priority descriptor.

The `part` half of the spec is a *filename*. hauksbee validates the `backend`
half against what the file declares, so a `backend = "qemu"` file resolved
under a `renode:` spec fails with a named `BackendMismatch`, not a confusing
schema error.

## Step 1, copy the nearest shipped descriptor

```
cp crates/hauksbee-mcu/db/mcu/stm32f103.soc.toml my_dir/stm32f107.soc.toml
```

The shipped F103 file (abridged). Read the real one. Its comments carry the
reasoning:

```toml
[soc]
backend = "renode"
machine = "f103"
platform_repl = """
using "platforms/cpus/stm32f103.repl"

# The part's reset-default core clock, cross-checked against frequency_hz
# below. The real file's platform is much longer; this is the clock part.
nvic:
    systickFrequency: 8000000

cpu:
    PerformanceInMips: 8
"""
cpu_path = "sysbus.cpu"
uart = "sysbus.usart1"
frequency_hz = 8_000_000
expected_e_machine = "EM_ARM"
mcu_label = "STM32F103 (ARM Cortex-M3)"

[[soc.ports]]
letter = "A"
peripheral = "gpioPortA"
odr_offset = 0x0C
width = 16
# ... ports B..G

[soc.i2c]
controllers = ["i2c1"]

[soc.spi]
controllers = ["spi1"]
extra_repl = "spi1: SPI.STM32SPI @ sysbus 0x40013000"
```

## Step 2, edit the fields for your part

Go field by field. The schema is `RenodeSoc` in `soc.rs`. QEMU descriptors
have their own shape (`arch`, `icount_shift`, mailbox-style `banks`), see
`esp32.soc.toml`:

- `platform_repl`: the Renode platform to load. A multi-line string is inline
  `.repl` source, which is the form you want: the loader requires the platform to
  declare the part's core clock (`cpu PerformanceInMips` in MHz, and
  `nvic systickFrequency` in Hz on a Cortex-M part) and refuses a value that
  disagrees with `frequency_hz`, so extend a stock file with `using` rather than
  pointing straight at it. A single-line `@platforms/...` path resolves inside
  the Renode installation, a local `.repl` file path also works, and
  `@{support}/<name>.repl` names a platform that arrives in a support bundle
  (whose `.repl` is checked for the same declarations).
- `support_bundle` (optional): the name of a bundle of peripheral models to
  unpack and compile before the platform loads, for a part Renode models not
  at all. See the last section below.
- `cpu_path` / `uart`: the platform's names for the CPU and console UART
  (`sysbus.`-qualified, exactly as Renode's monitor addresses them).
- `frequency_hz`: the part's core clock, and NOT advisory. It is cross-checked
  against the platform's own declarations above and a mismatch is refused at
  load, because four shipped platforms once ran 4.5x to 9x fast on the strength
  of the two disagreeing in silence.
- `expected_e_machine` (`EM_ARM`, `EM_RISCV`, `EM_XTENSA`, `EM_AVR`). hauksbee
  checks firmware ELFs against this field, so an ESP32 binary refuses loudly on
  an ARM part. `mcu_label` is the report string.
- `watchdog_limitation` (optional): one sentence saying how this part's watchdog
  fidelity falls short, rendered verbatim on every report surface. Leaving it out
  claims that a starved watchdog reboots the core the way silicon does, which on
  the shipped parts is true of exactly one of them, so leave it out only if you
  measured it.
- `[[soc.ports]]`, one per GPIO port: `letter`, the platform `peripheral`
  name (**without** `sysbus.`, the backend prepends it when polling),
  `odr_offset` (the output-data register's byte offset), and `width` (the port's
  bit count, usually 16 on STM32 and 32 on nRF52).
- `[[soc.ports]].dir`, optional, an inline sub-table saying where and how to read
  that port's direction/mode register:

  ```toml
  dir = { offset = 0x00, encoding = "stm32f1_crl_crh" }
  ```

  `offset` is the register's byte offset within the peripheral. `encoding` is one
  of three:

  | encoding | register shape |
  |---|---|
  | `moder` | STM32F4/L4/F7 MODER: 2 bits per pin, `0b01` = general-purpose output. AF mode is deliberately not counted as an output |
  | `stm32f1_crl_crh` | STM32F1 CRL/CRH: 4 bits per pin, CRL at `offset` covers pins 0-7 and CRH is read at `offset + 4` for pins 8-15. Any non-zero MODE field is an output |
  | `dir_bits` | one bit per pin, 1 = output (nRF52 `DIR`, RP2040 SIO `GPIO_OE`) |

  Omit `dir` and direction stays unobservable: every ODR bit change is reported
  as a drive. That is the conservative answer, and it is the right default.
  **A wrong `dir` map is worse than none**, because a mask that reads as zero
  suppresses every edge silently. Only add one once you have verified both the
  offset and the Renode model's read-back against a live machine.
- `[soc.i2c]` / `[soc.spi]`, controller names. `spi.extra_repl` splices in a
  peripheral definition the stock platform lacks. The F103 file adds SPI1
  this way, with a comment that explains why the IRQ-less single line
  suffices for polling-mode firmware.
- `extra_setup` / `post_load_setup`, monitor commands that run around
  firmware load. The SiFive FE310's PRCI clock-tag bring-up lives in
  `post_load_setup` in `sifive_fe310.soc.toml`. Keeping that footgun in a
  reviewed data file instead of a constructor is this schema's reason to
  exist.
- `[[soc.adc]]`, ADC channel injection recipes (`channel`,
  `full_scale_volts`, `max_count`, and exactly one of `monitor_command` or
  `memory_word`).

> **Why this is data at all: the ODR footgun.** The STM32F1 GPIO output-data
> register sits at offset `0x0C`. The F4 family moved it to `0x14`. Read the
> wrong offset and you observe the wrong register, silently. That exact bug
> class is why the per-part Rust constructors gave way to reviewed TOML. The
> F103 file's header comment tells the story, compare it to
> `stm32f4_discovery.soc.toml`. When you write your descriptor, triple-check
> the ODR offset against the reference manual.

**Trap: values are backend-facing strings, not pretty names.** The plan's
original sketch wrote `sysbus.gpioPortA` and `platforms/...`. The shipped
schema stores exactly what the backend consumes: `gpioPortA` (no `sysbus.`
prefix, the backend adds it) but `@platforms/...` (with the `@`) and
`sysbus.usart1` (with the prefix, because the monitor command uses it
verbatim). Copy a shipped file and keep each field's existing shape rather
than normalizing them to look consistent. They are inconsistent because the
backend is.

## Step 3, install the descriptor

```
mkdir -p ~/.config/hauksbee/mcu
cp my_dir/stm32f107.soc.toml ~/.config/hauksbee/mcu/
```

The spec `renode:stm32f107` now resolves to your descriptor. Validation
happens at load with named errors: unknown backend, empty `platform_repl`,
duplicate or zero-width port letters, duplicate controllers, unknown
`expected_e_machine`, ambiguous ADC injection. The schema is
`deny_unknown_fields`, so hauksbee rejects a typo'd field name instead of
letting it vanish. If you fat-finger `od_offset`, you find out immediately,
with the file path and field named in the error.

## Step 4, route your board's part to the descriptor

The descriptor answers "what is `renode:stm32f107`?". A second, equally
necessary file answers "which components ARE a `renode:stm32f107`?": a
`[[models]] kind = "mcu"` routing entry in the model library. It matches your
board's part value and carries the backend string plus the pin-role map (the
full recipe with a second RISC-V template is in
[docs/cosim/MCU.md](../cosim/MCU.md#adding-a-genuinely-new-mcu-variant-the-recipe-pattern)):

```toml
# ~/.config/hauksbee/models/stm32f107.toml  (or a dir given by --models-dir)
[[models]]
id = "stm32f107"
kind = "mcu"

[models.match]
value_re = "(?i)^STM32F107"

[models.params]
backend = "renode:stm32f107"     # <- the spec Step 3's descriptor resolves

[models.pins]
# pin-number -> role map; copy the nearest builtin entry in
# crates/hauksbee-models/db/mcu.toml and adjust (roles like "pa5", "pc13",
# "pb6_i2c1_scl" are what the binder's gpio_of_role parses).
"1" = "pc13"
# ...
```

How each entry point picks it up:

- **`hauksbee run`** loads `~/.hauksbee/models`, `~/.config/hauksbee/models`,
  and any `--models-dir DIR` (highest priority). The board's part value
  matches `value_re`, the entry's `backend` string reaches the scheduler, and
  the scheduler resolves the descriptor.
- **`hauksbee-ci run`** uses the same layered library and also takes
  `--models-dir`. Check the routing entry into your hardware repo and pass
  it in CI. The spec file's top-level `mcu` field is an **informational note
  only**, nothing reads it. The MCU comes from the board via the routing
  entry, exactly as in `hauksbee run`.
- Without a matching entry, known families fall back to a built-in router
  (for example, any `STM32F1xx` value binds `renode:stm32f103`). That is
  fine for F103 siblings that share its layout, but it will never name YOUR
  part. A new part needs the routing entry.

`hauksbee models resolve <board> [--models-dir DIR]` shows, per component,
which entry won and from which layer. Use it as the debugging surface when
the match does not take.

## Step 5, the proof

Mechanism tests (no emulator needed):

```
cargo test -p hauksbee-mcu --test soc_descriptors
```

This pins the resolver: `resolve_from_override_dir_adds_a_part_as_data` (a
new part purely as data), `override_dir_is_fail_loud_and_beats_builtin` (an
invalid override aborts and names the file, a valid one wins over the
builtin), plus descriptor/constructor equivalence and every named validation
error. `cargo test -p hauksbee-engine --lib soc_wiring` pins that the
scheduler's backend instantiation actually consults the override dirs (the
product path, not just the library function).

The end-to-end proof is a real boot, and you can run it from a clone. Copy the
shipped F103 descriptor into an override directory and point
`HAUKSBEE_MCU_DIR` at it: the resolver then loads *your* file rather than the
embedded one, which is the same path your new part will take. Renode must be
installed; `hauksbee doctor` reports whether it was found.

```
$ mkdir mcu && cp crates/hauksbee-mcu/db/mcu/stm32f103.soc.toml mcu/
$ HAUKSBEE_MCU_DIR=./mcu hauksbee run testdata/boards/stm32_bluepill_demo.kicad_pcb \
    --firmware testdata/firmware/stm32_blinky/blinky.elf --headless --seconds 1

co-sim: 1.00s on an external emulator (slow: roughly wall-clock per simulated second; this is normal for Renode/QEMU). Progress:
  ... 0.20 / 1.00s simulated
  ... 0.40 / 1.00s simulated
  ... 0.60 / 1.00s simulated
  ... 0.80 / 1.00s simulated

simulated 1.000s in 13.37s wall (0.075x realtime) over 5 nets

most active nets:
┌────────────────────────────┬──────────┬──────────┬──────────┐
│ Net                        │ min (V)  │ max (V)  │ toggles  │
├────────────────────────────┼──────────┼──────────┼──────────┤
│ PC13_LED                   │    0.000 │    3.265 │        6 │
│ LED_A                      │    1.905 │    1.905 │        0 │
│ PA5_OUT                    │    3.116 │    3.116 │        0 │
│ +3V3                       │    3.300 │    3.300 │        0 │
│ GND                        │    0.000 │    0.000 │        0 │
└────────────────────────────┴──────────┴──────────┴──────────┘

UART output (18 bytes):
hello from stm32
```

The toggling PC13 and the UART banner show the descriptor's port map and
`uart` field doing real work. Your own part needs one more piece, the
`[[models]]` routing entry from step 4, plus `--models-dir` pointing at it, so
the board's MCU value resolves to `renode:<yourpart>`. The same board, firmware
and override run in a `hauksbee-ci` spec the same way.

**Try the footgun, once.** Edit `mcu/stm32f103.soc.toml` and change every
`odr_offset = 0x0C` to `0x14` (the F4 family's offset), then re-run. The board
still boots, the UART banner still prints, and `PC13_LED` reports
`0.000 / 0.000 / 0 toggles`. Nothing errors. That silent, plausible, entirely
wrong result is why the register offsets are reviewed data instead of code, and
why the walkthrough keeps saying to check them against the reference manual.

## The honest boundary

A descriptor configures the **backends that already exist** (Renode, QEMU).
Two things stay Rust, deliberately (`soc.rs` module docs):

- **A new emulator backend** is a new implementation of the `Mcu` trait
  (`crates/hauksbee-mcu/src/traits.rs`), a lockstep contract, not a config.
- **simavr parts**: simavr's own part database does the work. There is
  nothing for a descriptor to describe.

If your part needs a peripheral the Renode platform does not model (an ADC, a
missing SPI block), `extra_repl`/`extra_setup` can splice in simple
definitions, as long as Renode has the peripheral *class* and the platform
merely fails to instantiate it.

When Renode has no model of the peripheral at all, and especially when it has
no platform for your part at all, a `.repl` cannot help: it cannot conjure a
class the emulator never compiled. That case is what `support_bundle` is for.
Renode compiles C# at run time, so a bundle carries the `.cs` peripheral models
themselves, plus whatever data files the platform reads (an SVD, a boot ROM
image), embedded in the hauksbee binary, unpacked to a temp directory at machine
creation, and `include`d in a declared dependency order before the platform
parses. The descriptor then refers to the unpacked directory as `{support}`.
Registering a new bundle means adding its files and their include order to
`crates/hauksbee-mcu/src/renode/support.rs`; the shipped example is RP2040,
whose whole SoC arrives this way (`crates/hauksbee-mcu/db/mcu/rp2040/`,
provenance and licences in that directory's `README.md`). Budget for the cost:
the compile happens on every machine creation, and RP2040's ~377 kB of C# adds
roughly eight seconds of bring-up per run.

---

See [docs/cosim/MCU.md](../cosim/MCU.md) for the backend contract these descriptors
configure, and [add-a-sensor.md](add-a-sensor.md) for the peripherals your new
MCU's firmware will talk to.
