# Add an MCU variant (a sibling of a supported family)

Two TOML files, no recompile: a **SoC descriptor** (`<part>.soc.toml`) that
says what `renode:<part>` is, and a **routing entry** (`[[models]] kind =
"mcu"`) that says which board components are that part. The worked example is
an STM32F103 sibling on Renode. Shipped descriptors:
`crates/hauksbee-mcu/db/mcu/*.soc.toml`; schema: `crates/hauksbee-mcu/src/soc.rs`.
The full field reference is in [add-a-microcontroller.md](add-a-microcontroller.md#the-field-reference).

## Resolution

At co-sim time the binder's `backend = "renode:stm32f107"` string is resolved
by `SocConfig::resolve`, highest priority first:

1. `$HAUKSBEE_MCU_DIR/<part>.soc.toml`;
2. `~/.config/hauksbee/mcu/<part>.soc.toml` (or the spec's
   `[mcu] descriptor_dir = "..."`, relative to the spec; the env var wins);
3. the descriptors embedded in the binary (`hauksbee models list --builtin`).

An override beats a built-in of the same name. A descriptor that exists but
fails validation aborts the run naming the file and field; there is no silent
fallback. The `backend` half of the spec is checked against the file's own
`backend` (a `qemu` file under a `renode:` spec is a `BackendMismatch`).

## Step 1: copy the nearest descriptor

```
cp crates/hauksbee-mcu/db/mcu/stm32f103.soc.toml my_dir/stm32f107.soc.toml
```

```toml
[soc]
backend = "renode"
machine = "f103"
platform_repl = """
using "platforms/cpus/stm32f103.repl"

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
dir = { offset = 0x00, encoding = "stm32f1_crl_crh" }
# ... ports B..G

[soc.i2c]
controllers = ["i2c1"]

[soc.spi]
controllers = ["spi1"]
extra_repl = "spi1: SPI.STM32SPI @ sysbus 0x40013000"
```

## Step 2: edit the fields

- `platform_repl`: a multi-line string is inline `.repl` source; extend a stock
  platform with `using` and declare the part's core clock (`cpu
  PerformanceInMips` in MHz and, on Cortex-M, `nvic systickFrequency` in Hz).
  The loader refuses a value that disagrees with `frequency_hz`, or a platform
  that declares no clock. Declare the reset default. A single-line
  `@platforms/...` path also works but cannot carry the clock declaration.
- `cpu_path`, `uart`: `sysbus.`-qualified, as Renode's monitor names them.
- `frequency_hz`: the part's core clock, cross-checked as above.
- `expected_e_machine`: `EM_ARM`, `EM_RISCV`, `EM_XTENSA`, `EM_AVR`. A
  wrong-architecture ELF is refused.
- `watchdog_limitation`, `timing_limitation` (optional): one sentence each,
  rendered verbatim on every report surface. Omitting one claims silicon
  fidelity, so omit only what you measured.
- `[[soc.ports]]`: `letter`, `peripheral` (**without** `sysbus.`),
  `odr_offset`, `width`; optional `dir = { offset, encoding }` with encoding
  `moder` (STM32F0/F4/L4/F7, 2 bits per pin), `stm32f1_crl_crh` (CRL at
  `offset`, CRH at `offset + 4`), or `dir_bits` (nRF52 `DIR`, RP2040 `GPIO_OE`).
  Omit `dir` and every ODR change reports as a drive (the conservative
  default). A wrong `dir` map suppresses edges silently; add one only after
  verifying the read-back on a running machine.
- `[soc.i2c]` / `[soc.spi]`: controller names. `spi.extra_repl` splices in a
  peripheral the stock platform lacks.
- `extra_setup` / `post_load_setup`: monitor commands around firmware load.
- `[[soc.adc]]`: `channel`, `full_scale_volts`, `max_count`, and exactly one of
  `monitor_command` or `memory_word`.

The ODR offset is the classic trap: STM32F1 `0x0C`, STM32F4/F0 `0x14`. A wrong
offset boots fine and reports every pin as never driven.

## Step 3: install

```
mkdir -p ~/.config/hauksbee/mcu
cp my_dir/stm32f107.soc.toml ~/.config/hauksbee/mcu/
hauksbee models lint ~/.config/hauksbee/mcu/stm32f107.soc.toml
```

The schema is `deny_unknown_fields`; a typo'd field is a named error.

## Step 4: route the board's part

```toml
# ~/.config/hauksbee/models/stm32f107.toml  (or a --models-dir)
[[models]]
id = "stm32f107"
kind = "mcu"

[models.match]
value_re = "(?i)^STM32F107"

[models.params]
backend = "renode:stm32f107"

[models.pins]
# pin-number -> role; copy the nearest entry in crates/hauksbee-models/db/mcu.toml
"1" = "pc13"
```

`hauksbee run` and `hauksbee-ci run` both load `~/.hauksbee/models`,
`~/.config/hauksbee/models` and `--models-dir DIR`. Without a matching entry a
known family falls back to a built-in router (any `STM32F1xx` value binds
`renode:stm32f103`), which never names your part.
`hauksbee models resolve <board> [--models-dir DIR]` shows which entry won.

## Step 5: prove it

```
cargo test -p hauksbee-mcu --test soc_descriptors        # resolver + validation, no emulator
mkdir mcu && cp crates/hauksbee-mcu/db/mcu/stm32f103.soc.toml mcu/
HAUKSBEE_MCU_DIR=./mcu hauksbee run testdata/boards/stm32_bluepill_demo.kicad_pcb \
    --firmware testdata/firmware/stm32_blinky/blinky.elf --headless --seconds 1
```

A toggling `PC13_LED` and the `hello from stm32` UART banner show the port map
and `uart` field working through your override. Change every `odr_offset` to
`0x14` and re-run: it still boots, still prints, and reports 0 toggles. That
silent wrong result is why the offsets are reviewed data.

## Boundary

A descriptor configures an existing backend. A new emulator is a new `Mcu`
trait implementation (`crates/hauksbee-mcu/src/traits.rs`); simavr parts need
no descriptor. When Renode has no model of a peripheral at all, use a
`support_bundle` (vendored C# compiled at run time; see
[add-a-microcontroller.md](add-a-microcontroller.md#tier-c-no-platform-exists)).
