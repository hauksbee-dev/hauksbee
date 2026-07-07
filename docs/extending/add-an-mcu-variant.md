# Add an MCU variant: an STM32 sibling via `.soc.toml`, no recompile

**Goal.** Make hauksbee co-simulate firmware for an MCU part it doesn't ship,
by writing one SoC descriptor file — TOML, validated fail-loud, resolved from
a user directory at runtime. **No recompile is the whole point.** The worked
example is an STM32F103 sibling on the Renode backend; the shipped descriptor
we start from is `crates/hauksbee-mcu/db/mcu/stm32f103.soc.toml`, and the
schema/loader is `crates/hauksbee-mcu/src/soc.rs`.

**What you need:** the part's reference manual (GPIO register offsets), a
Renode platform (`.repl`) that models the part, and the part's peripheral
names in that platform.

## How resolution works

A co-sim names its MCU as a `backend:part` spec, e.g. `renode:stm32f103`.
`SocConfig::resolve` (in `soc.rs`) searches, highest priority first:

1. `$HAUKSBEE_MCU_DIR/<part>.soc.toml` — explicit override directory,
2. `~/.config/hauksbee/mcu/<part>.soc.toml` — your standing descriptor dir,
3. the embedded built-ins (shipped via `include_str!`, so the binary is
   self-contained while the db file stays the single source of truth).

Drop `mypart.soc.toml` in (1) or (2) and resolve `renode:mypart` — that's the
entire installation procedure. The `part` half of the spec is a *filename*;
the `backend` half is validated against what the file declares, so a
`backend = "qemu"` file handed to a Renode context fails with a named
`BackendMismatch`, never a confusing schema error.

## Step 1 — copy the nearest shipped descriptor

```
cp crates/hauksbee-mcu/db/mcu/stm32f103.soc.toml my_dir/stm32f107.soc.toml
```

The shipped F103 file (abridged — read the real one, its comments carry the
reasoning):

```toml
[soc]
backend = "renode"
machine = "f103"
platform_repl = "@platforms/cpus/stm32f103.repl"
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

## Step 2 — edit the fields for your part

Field by field (the schema is `RenodeSoc` in `soc.rs`; QEMU descriptors have
their own shape — `arch`, `icount_shift`, mailbox-style `banks` — see
`esp32.soc.toml`):

- `platform_repl` — the Renode platform to load. `@platforms/...` paths
  resolve inside the Renode installation; a local `.repl` file path also
  works.
- `cpu_path` / `uart` — the platform's names for the CPU and console UART
  (`sysbus.`-qualified, exactly as Renode's monitor addresses them).
- `frequency_hz`, `expected_e_machine` (`EM_ARM`, `EM_RISCV`, `EM_XTENSA`,
  `EM_AVR` — firmware ELFs are checked against it so an ESP32 binary refuses
  loudly on an ARM part), `mcu_label` (report string).
- `[[soc.ports]]` — one per GPIO port: `letter`, the platform `peripheral`
  name (**without** `sysbus.` — the backend prepends it when polling), the
  ODR register offset, and the pin width.
- `[soc.i2c]` / `[soc.spi]` — controller names; `spi.extra_repl` splices a
  peripheral definition the stock platform lacks (the F103 file adds SPI1
  this way, with a comment explaining why the IRQ-less single line suffices
  for polling-mode firmware).
- `extra_setup` / `post_load_setup` — monitor commands run around firmware
  load. The SiFive FE310's PRCI clock-tag bring-up lives in
  `post_load_setup` in `sifive_fe310.soc.toml`; that footgun living in a
  reviewed data file instead of a constructor is this schema's reason to
  exist.
- `[[soc.adc]]` — ADC channel injection recipes (`channel`,
  `full_scale_volts`, `max_count`, and exactly one of `monitor_command` or
  `memory_word`).

> **Why this is data at all — the ODR footgun.** The STM32F1 GPIO output-data
> register sits at offset `0x0C`; the F4 family moved it to `0x14`. Read the
> wrong offset and you observe the wrong register — silently. That exact bug
> class is why the per-part Rust constructors were replaced by reviewed TOML
> (the F103 file's header comment tells the story; compare
> `stm32f4_discovery.soc.toml`). When you write your descriptor, the ODR
> offset is the number to triple-check against the reference manual.

**Trap — values are backend-facing strings, not pretty names.** The plan's
original sketch wrote `sysbus.gpioPortA` and `platforms/...`; the shipped
schema stores exactly what the backend consumes: `gpioPortA` (no `sysbus.`
prefix — the backend adds it) but `@platforms/...` (with the `@`) and
`sysbus.usart1` (with the prefix, because the monitor command uses it
verbatim). Copy a shipped file and preserve each field's existing shape rather
than normalizing them to look consistent — they are inconsistent because the
backend is.

## Step 3 — install and resolve

```
mkdir -p ~/.config/hauksbee/mcu
cp my_dir/stm32f107.soc.toml ~/.config/hauksbee/mcu/
```

Any co-sim (CI spec, `hauksbee run`) that names `renode:stm32f107` now loads
your descriptor. Validation happens at load with named errors: unknown
backend, empty `platform_repl`, duplicate or zero-width port letters,
duplicate controllers, unknown `expected_e_machine`, ambiguous ADC injection.
The schema is `deny_unknown_fields`, so a typo'd field name is rejected
instead of vanishing — if you fatfinger `od_offset`, you find out immediately.

## Step 4 — the test that proves the mechanism

The override path itself is pinned by
`resolve_from_override_dir_adds_a_part_as_data` in
`crates/hauksbee-mcu/tests/soc_descriptors.rs`: it writes a descriptor for a
part no built-in knows into a temp `$HAUKSBEE_MCU_DIR`, resolves it with no
recompile, and asserts the override dir also *wins* over a built-in of the
same name. The same file proves the shipped descriptors reproduce the deleted
Rust constructors field-for-field and that every validation failure is named.

```
cargo test -p hauksbee-mcu --test soc_descriptors
```

Green looks like:

```
test resolve_from_override_dir_adds_a_part_as_data ... ok
test renode_descriptors_equal_constructors ... ok
...
test result: ok. 19 passed; 0 failed
```

For *your* descriptor, the end-to-end proof is a co-sim smoke run: point a CI
spec's MCU at `renode:<yourpart>` with a firmware blink ELF and assert a GPIO
toggle — the descriptor loading, platform resolving, and port map are all on
that path. (Renode must be installed; `hauksbee doctor` checks backend
availability.)

## The honest boundary

A descriptor configures the **backends that already exist** (Renode, QEMU).
Two things stay Rust, deliberately (`soc.rs` module docs, plan 06 §2):

- **A new emulator backend** is a new implementation of the `Mcu` trait
  (`crates/hauksbee-mcu/src/traits.rs`) — a lockstep contract, not a config.
- **simavr parts** — simavr's own part database does the work; there is
  nothing for a descriptor to describe.

If your part needs a peripheral the Renode platform doesn't model (an ADC, a
missing SPI block), `extra_repl`/`extra_setup` can splice simple definitions,
but a genuinely unmodeled peripheral is a Renode platform contribution, not a
hauksbee descriptor field.

---

See [docs/MCU.md](../MCU.md) for the backend contract these descriptors
configure, and [add-a-sensor.md](add-a-sensor.md) for the peripherals your new
MCU's firmware will talk to.
