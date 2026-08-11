# Add a microcontroller: a family hauksbee does not support yet

**Goal.** Make hauksbee co-simulate firmware for an MCU family it has never
heard of, and get that support merged so nobody has to repeat the work.

This is the sibling of [add-an-mcu-variant.md](add-an-mcu-variant.md). That page
covers a *variant of a family hauksbee already supports*: an STM32F103 sibling,
where the emulator platform exists, the register layout is known, and the answer
is two small TOML files. Start there if your part is an STM32, an nRF52, an
ESP32, an RP2040 or an ATmega. This page covers the harder cases, and it is
honest about which of them stay easy and which do not.

**Adding an MCU is a welcomed contribution with a documented path, not a
core-team activity.** The three tiers below, the checklist at the end, and the
worked STM32F072 example all exist because the intended outcome is a pull request
from someone who does not work on hauksbee. The last section says exactly what
the maintainer will and will not take on afterwards.

## The three tiers, and what each costs

Which tier you are in is decided by your emulator, not by your part. Find out
first, because the answer changes the work by two orders of magnitude.

| Tier | Situation | What you write | Rust? | Realistic effort |
|---|---|---|---|---|
| **A** | The family is supported and your part is a sibling | one `.soc.toml`, one routing entry | no | an afternoon |
| **B** | The family is new, but your emulator already models the part | one `.soc.toml`, one routing entry, one firmware fixture, one test | no | a day or two |
| **C** | The emulator does not model the part at all | all of tier B, plus vendored peripheral models and a support-bundle registration | one list in `support.rs` | weeks |

Tier A is [add-an-mcu-variant.md](add-an-mcu-variant.md). Tier B is the worked
example below. Tier C is the RP2040, whose support was built exactly this way and
is the case study for the last third of this page.

To find out which tier you are in, look for a platform:

```bash
# Renode: the stock platform descriptions ship inside the install.
ls ~/renode-portable/Renode.app/Contents/MacOS/platforms/cpus/   # macOS portable
ls ~/renode-portable/platforms/cpus/                             # Linux portable

# What hauksbee already has, and which emulators this build can find:
hauksbee models list --builtin
hauksbee doctor --backends
```

A `.repl` for your part, or for a close sibling in the same family, puts you in
tier B. Nothing puts you in tier C.

## How a part reaches a co-simulation

Two files do two different jobs, and both are needed:

- **The descriptor** answers "what is `renode:stm32f072`?". It is a
  `<part>.soc.toml` file: the platform to load, the CPU path, the register
  offsets, the UART and bus controller names.
- **The routing entry** answers "which components ARE a `renode:stm32f072`?". It
  is a `[[models]] kind = "mcu"` entry in the model library, matching your
  board's part value and carrying the backend string plus the pin-role map.

At co-sim time the scheduler hands the backend string to `SocConfig::resolve`,
which searches, highest priority first:

1. `$HAUKSBEE_MCU_DIR/<part>.soc.toml`, an explicit override directory;
2. `~/.config/hauksbee/mcu/<part>.soc.toml`, your standing descriptor directory;
3. the descriptors embedded in the binary.

A `hauksbee-ci` spec can declare the same layer without an environment variable:

```toml
[mcu]
descriptor_dir = "mcu"   # resolved relative to the spec file
```

An explicitly set `HAUKSBEE_MCU_DIR` wins over the spec field, because the
environment variable is the operator's override of last resort and a spec must
not be able to silently defeat it.

Two guarantees hold at every layer. An override beats a built-in of the same
part name. And a descriptor file that *exists* for the requested part is always
parsed, with any validation error aborting the run and naming the file and the
field: hauksbee never quietly falls back to a lower-priority descriptor.

## The field reference

The schema is `RenodeSoc` and `QemuSoc` in
`crates/hauksbee-mcu/src/soc.rs`. Both are `deny_unknown_fields`, so a mistyped
field name is a loud parse error rather than a value that vanishes.

### `[soc]`, Renode

| Field | Required | Type | What it does |
|---|---|---|---|
| `backend` | yes | `"renode"` | Which loader claims the file |
| `machine` | yes | string | The Monitor prompt name, e.g. `"f072"` |
| `platform_repl` | yes | string | The platform. See the two forms below |
| `support_bundle` | no | string | Peripheral models to compile before the platform loads (tier C) |
| `cpu_path` | yes | string | The CPU for state queries, e.g. `"sysbus.cpu"` |
| `uart` | no | string | The UART to bridge to a host socket. Omit the field for no UART; a blank string is refused |
| `frequency_hz` | yes | integer | The part's core clock. Renode clocks the platform from the `.repl`, so this does not set the emulated clock by itself, which is why the loader CROSS-CHECKS it against the platform's own declarations and refuses a mismatch (see below). 0 is refused |
| `expected_e_machine` | yes | `EM_ARM`, `EM_RISCV`, `EM_XTENSA`, `EM_AVR` | The ISA gate. A wrong-architecture ELF is refused before it runs as garbage |
| `mcu_label` | yes | string | The human name in reports and errors |
| `watchdog_limitation` | no | string | How this part's watchdog fidelity falls short, as one sentence rendered verbatim in batch reports and the interactive TUI, and in `hauksbee models lint`. The synchronous web front door refuses external-emulator co-sim before scheduler limitations exist; its refusal points to the live app or CLI instead. Omitting this field CLAIMS that an armed, never-fed watchdog reboots the core the way silicon does, so omit it only if you measured that |
| `timing_limitation` | no | string | How this part's timing fidelity falls short, one sentence, same verbatim rendering. Omitting it claims a firmware delay costs the virtual time it costs on silicon, which is a clock-truth-gate measurement, not a default |
| `extra_setup` | no | array of strings | Monitor commands run after the platform loads, before the firmware |
| `post_load_setup` | no | array of strings | Monitor commands run after the firmware loads. `{cpu}` is substituted with `cpu_path` |

`platform_repl` has two forms, told apart by whether the string contains a
newline:

- a **single line** is a Renode path: `"@platforms/cpus/stm32f072.repl"` resolves
  inside the Renode installation, and `"@/abs/path/mine.repl"` resolves on disk;
- **multiple lines** are inline `.repl` **source**, which the backend writes to a
  temp file at bring-up. This is how a descriptor ships platform fixes as data
  rather than depending on a file installed beside Renode. The shipped
  `stm32f103.soc.toml` carries a whole HAL-boot clock tree this way. An inline
  platform usually starts with `using "platforms/cpus/<stock>.repl"` so it
  extends the stock one instead of replacing it.

So a `.repl` is not TOML and never becomes TOML, but it does not have to be a
separate file: it can be a multi-line string inside one.

**You will use the inline form, because a Renode part has to declare its core
clock and the single-line form cannot.** The loader requires the platform to
declare `cpu PerformanceInMips` (the part's core clock in MHz) and, on a
Cortex-M part, `nvic systickFrequency` (the same clock in Hz), and refuses any
value that disagrees with `frequency_hz`. So the smallest legal Renode
`platform_repl` extends a stock file with three lines:

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

This is not ceremony. Four shipped platforms used to run simulated time at
Renode's clock rate instead of the part's, measured 9.09x, 6.58x and 4.51x fast,
because the stock `.repl` declared a 72 MHz SysTick and a 100 MIPS core whatever
part the descriptor claimed, and `frequency_hz` cancels out of the engine's own
arithmetic so nothing disagreed. Declare the RESET DEFAULT unless you can justify
otherwise: a platform with no clock-tree model cannot follow a firmware's PLL
bring-up, so the rate the part runs at before firmware touches anything is the
one that is true for every image. Other clock domains (a watchdog's own
oscillator, a timer block on its own bus branch) are yours to get right and are
deliberately not policed by the cross-check. `docs/cosim/MCU.md` carries the
measured per-backend table and `crates/hauksbee-mcu/tests/clock_truth.rs` is the
gate that keeps it true.

The literal `{support}` token in `platform_repl`, `extra_setup` and
`post_load_setup` is replaced with the unpacked support-bundle directory. A
descriptor that uses the token without declaring a `support_bundle` is refused at
load, naming the field, because nothing can ever substitute it.

### `[[soc.ports]]`, one per GPIO port

| Field | Required | Type | What it does |
|---|---|---|---|
| `letter` | yes | single character | The port letter the engine uses in a pin id (`'C'`, or `'0'` for a single-bank part) |
| `peripheral` | yes | string | The platform's name for the port, **without** the `sysbus.` prefix; the backend adds it |
| `odr_offset` | yes | integer | Byte offset of the output-data register within the peripheral |
| `width` | yes | 1 to 32 | Pins in this port |
| `dir` | no | inline table | Where and how to read the direction register |

`dir = { offset = 0x00, encoding = "moder" }`. `offset` is the register's byte
offset within the peripheral, and `encoding` is one of three:

| `encoding` | Register shape |
|---|---|
| `moder` | STM32F0/F4/L4/F7 MODER: 2 bits per pin, `0b01` is a general-purpose output. Alternate-function mode is deliberately not counted, since an AF pin may be an input function |
| `stm32f1_crl_crh` | STM32F1 CRL/CRH: 4 bits per pin, CRL at `offset` covers pins 0 to 7 and CRH is read at `offset + 4` for pins 8 to 15. Any non-zero MODE field is an output |
| `dir_bits` | one bit per pin, 1 is an output (nRF52 `DIR`, RP2040 SIO `GPIO_OE`) |

Omit `dir` and direction stays unobservable: every output-state change is
reported as a drive, which is the conservative answer and the right default.
**A wrong `dir` map is worse than none**, because a mask that reads as zero
suppresses every edge in silence. Add one only once both the offset and the
emulator model's read-back are verified against a running machine.

`moder` and `stm32f1_crl_crh` decode 16 pins by construction, so pairing either
with a port wider than 16 would drop the top pins' edges silently. That is
refused at load, naming the port, its width and the encoding: a mask that reads
as zero above pin 15 is worse than omitting `dir`, which at least reports every
output-state change. Use `dir_bits`, or narrow the port.

### `[soc.i2c]`, `[soc.spi]`

| Field | Required | Type | What it does |
|---|---|---|---|
| `controllers` | no | array of strings | Controller names that can host engine-provided bus slaves |
| `extra_repl` (spi only) | no | string | A peripheral definition spliced in when the stock platform lacks one |

**Only name a controller you have watched a byte cross.** Naming one installs a
bridge peripheral; if that model never dispatches to a registered slave, a bound
sensor answers zeroes and the report reads healthy. That is the RP2040's SPI
story, recorded in `crates/hauksbee-mcu/db/mcu/rp2040.soc.toml`. Left empty, a
bound sensor is recorded as UNEXERCISED, surfaced on the four batch report
surfaces (run text, `--plain`, `--json`, hauksbee-ci, though not the TUI), and
fails a `hauksbee-ci` peripheral assertion. Empty is the honest default.

### `[[soc.adc]]`, one per injectable channel

| Field | Required | Type | What it does |
|---|---|---|---|
| `channel` | yes | integer | The engine-facing channel index |
| `full_scale_volts` | yes | float | The voltage that maps to `max_count`. Injected volts are clamped to this |
| `max_count` | yes | integer | The count at full scale, e.g. 4095 for 12 bits |
| `monitor_command` | one of the two | string | A Monitor command run each chunk |
| `memory_word` | one of the two | integer | An address written with the count each chunk |

Exactly one of the last two. In a `monitor_command`, three tokens are
substituted: `{count}` for a model fed raw codes, `{millivolts}` for an integer
millivolt feed, and `{volts}` for a model whose method takes a real voltage.
Picking the wrong one is silently off by the full-scale factor, so read the
model's own signature before choosing. Picking none of them is a `models lint`
finding: the command would run every chunk and feed the same constant, so
injection would appear to work and never change.

`full_scale_volts` and `max_count` are both divisors in the volts-to-count
conversion, so a zero `max_count` or a non-positive or non-finite
`full_scale_volts` is refused at load. So is the same `channel` mapped twice: the
backend keys injection on the channel index, and the second recipe would shadow
the first.

A channel with no entry is not faked. Its injections are DROPPED and reported as
a coverage hole on the same four batch surfaces, so a batch run whose firmware
never received its analog inputs cannot read as healthy.

### `[soc]`, QEMU

The QEMU backend exists for the Espressif fork and the ESP32 family, which
observes GPIO through a RAM mailbox rather than register read-back, so a bank
carries mailbox addresses instead of a register offset.

| Field | Required | Type | What it does |
|---|---|---|---|
| `backend` | yes | `"qemu"` | Which loader claims the file |
| `arch` | yes | `"xtensa"` or `"riscv32"` | Which fork binary to run |
| `machine` | yes | string | The QEMU machine name |
| `icount_shift` | yes | integer | The instruction-count shift the lockstep uses |
| `frequency_hz` | yes | integer | Advisory clock |
| `expected_e_machine` | yes | `EM_*` name | The ISA gate |
| `mcu_label` | yes | string | The human name |
| `[[soc.banks]]` | no | table array | `letter`, `out_reg`, `in_reg`, `width` |
| `[soc.i2c].buses` | no | array of strings | Mailbox bus names |

### What the loader checks, and what it does not

The loader refuses a descriptor that has no correct execution. Every refusal is a
named error naming the field, and each aborts the run:

unknown `backend`; a descriptor whose declared backend disagrees with the spec
that resolved it; a backend this build was not compiled with; an empty
`platform_repl`, `machine`, `cpu_path` or `uart`; a `frequency_hz` of 0; a
`{support}` token in a field with no `support_bundle` to substitute it; a Renode
platform declaring a core clock (`cpu PerformanceInMips`, `nvic
systickFrequency`) that disagrees with `frequency_hz`, or declaring none at all;
an unknown `expected_e_machine`; an unknown `support_bundle` (with the list this
build carries); a port of zero width or wider than 32; two ports sharing a letter; a
`dir` encoding that decodes fewer pins than its port is wide; a duplicated bus
controller name; an ADC entry with neither or both injection forms; a duplicated
ADC `channel`; an ADC `max_count` of 0 or a non-positive or non-finite
`full_scale_volts`; an unknown QEMU `arch`; and, through `deny_unknown_fields`,
any field name that is not in the tables above.

Two things the loader accepts and `hauksbee models lint` reports, because both
execute and what they mean depends on what you meant: a blank `mcu_label` (it
reaches reports and arch-mismatch errors, never the emulator), and an ADC
`monitor_command` with no substitution token.

What no check can catch is a *plausible wrong number*. An `odr_offset` from
the wrong family in the same vendor's range reads a real register that is not the
output data register, and the co-sim then reports every pin as never driven, with
no error anywhere. That is why these values are reviewed data with the
verification written next to them, and why the walkthrough below reads every
offset off a running machine before trusting it.

## Iterating in seconds, not minutes

Validate and inspect a descriptor without an emulator, firmware or a board:

```
$ hauksbee models lint crates/hauksbee-mcu/db/mcu/examples/stm32f072.soc.toml
soc descriptor 'crates/hauksbee-mcu/db/mcu/examples/stm32f072.soc.toml': ok (renode:stm32f072)
  part: STM32F072 (ARM Cortex-M0) on machine "f072"
  platform: @platforms/cpus/stm32f072.repl
  cpu: sysbus.cpu   clock: 8000000 Hz
  uart bridge: sysbus.usart1
  gpio port A: 16 pins on sysbus.gpioPortA, output state at +0x14, direction at +0x0 (Moder)
  ...
  i2c controllers: none
  spi controllers: none
  adc channel 0: 0..4095 counts over 0..3.3 V via monitor "sysbus.adc SetDefaultValue {millivolts} 0"
  ...
  setup commands: 0 before the firmware loads, 0 after
  note: no i2c or spi controllers: a bound sensor is recorded UNEXERCISED and a CI peripheral assertion against it fails
1 item(s) checked, 0 finding(s): clean
```

It runs the same loader a co-sim runs, so "lint said ok" and "the co-sim accepts
it" cannot disagree, then prints back what the descriptor will actually do: which
register each port reads, which buses exist, which channels are injectable, and a
`note:` per capability the descriptor leaves absent. Notes are advisories and do
not affect the exit code. No ADC map and no bus controllers is usually exactly
right, and a warning about it would train you to ignore the output.

A finding does affect the exit code (2), and names the field:

```
$ hauksbee models lint mypart.soc.toml
soc descriptor 'mypart.soc.toml': ERROR: port '0' is 32 bits wide but its dir
encoding "moder" decodes only 16 pins; pins 16 and above would read as inputs
and their edges would be dropped silently. Use "dir_bits", or narrow the port
1 item(s) checked, 1 finding(s)
```

Run it after every edit. The full boot is the last check, not the first.

## Tier B, worked: STM32F072 on a stock Renode platform

The STM32F0 family is not one of hauksbee's shipped parts. Renode 1.16.1 ships
`platforms/cpus/stm32f072.repl`. That is tier B, and the whole part is one file:
`crates/hauksbee-mcu/db/mcu/examples/stm32f072.soc.toml`.

### Step 1, read the platform, not the datasheet

The datasheet tells you what the silicon does. The descriptor has to describe
what the *emulator* does, and those differ. Open the `.repl` and its includes:

```bash
R=~/renode-portable/Renode.app/Contents/MacOS/platforms/cpus
cat $R/stm32f072.repl     # thin: flash size, sram size, and `using "./stm32f0.repl"`
cat $R/stm32f0.repl       # the real platform
```

What that gives you, for this part:

```
cpu: CPU.CortexM @ sysbus            cpuType: "cortex-m0"
usart1: UART.STM32F7_USART @ sysbus 0x40013800
gpioPortA: GPIOPort.STM32_GPIOPort @ sysbus <0x48000000, +0x400>
gpioPortB..gpioPortF                          0x48000400 .. 0x48001400
i2c1, i2c2: I2C.STM32F7_I2C
spi1, spi2: SPI.STM32SPI
adc: Analog.STM32F0_ADC @ sysbus 0x40012400   referenceVoltage: 3.3
rcc: Python.PythonPeripheral                  a stub, see the tier note below
```

**The trap this example exists to show.** The obvious move is to copy the shipped
`stm32f103.soc.toml`, since both parts are STM32s. It produces a machine that
boots, prints nothing, and reports every pin as never driven, with no error:

| Register | STM32F103 (F1) | STM32F072 (F0) |
|---|---|---|
| GPIO block base | `0x40010800` on APB2 | `0x48000000` on AHB2 |
| output data register | ODR at `0x0C` | ODR at `0x14` |
| direction register | CRL `0x00` / CRH `0x04` | MODER at `0x00`, 2 bits per pin |
| USART status | SR at `0x00` | ISR at `0x1C` |
| USART data | DR at `0x04` | RDR `0x24` / TDR `0x28` |

The F0's ODR offset matches the **F4's**, not the F1's, which is precisely the
coincidence that makes the nearest-named descriptor the wrong one to copy.

### Step 2, build a firmware fixture that drives something, and one that does not

`testdata/firmware/stm32f072_blinky/` is register-level C with no vendor SDK,
built by `make` with `arm-none-eabi-gcc`. One source produces two images, and the
second one is the point:

- `blinky.elf` configures PC6 as an output and toggles it, holds PA5 high,
  brings up USART1 on PA9/PA10, prints a banner, and answers commands;
- `quiet.elf` (the same source with `-DQUIET`) configures nothing and drives
  nothing.

Without the quiet image, a bridge that fabricated edges would pass every test you
write. With it, the claim "this descriptor observes real firmware activity" has
both halves.

### Step 3, read the offsets off a running machine

Before writing the descriptor, prove each value. Drive Renode directly: it is far
faster than a co-sim round trip and it is the only way to distinguish "the
register is at this offset" from "the emulator's model happens to return zero
there".

```bash
cat > /tmp/probe.resc <<'EOF'
mach create "f072"
machine LoadPlatformDescription @platforms/cpus/stm32f072.repl
sysbus.usart1 CreateFileBackend @/tmp/f072-uart.txt true
sysbus LoadELF @/abs/path/testdata/firmware/stm32f072_blinky/blinky.elf
emulation RunFor "0.3"
sysbus.gpioPortC ReadDoubleWord 0x0
sysbus.gpioPortC ReadDoubleWord 0x14
sysbus.gpioPortA ReadDoubleWord 0x0
sysbus.gpioPortA ReadDoubleWord 0x14
peripherals
quit
EOF
cd ~/renode-portable/Renode.app/Contents/MacOS
./renode --disable-xwt --console --hide-log -e "include @/tmp/probe.resc"
cat /tmp/f072-uart.txt
```

What came back, on Renode 1.16.1 (d66b0c2a-202602160921):

```
sysbus.gpioPortC ReadDoubleWord 0x00  ->  0x00001000   MODER: PC6 = 0b01, output
sysbus.gpioPortC ReadDoubleWord 0x14  ->  0x00000040   ODR:   PC6 driven
sysbus.gpioPortA ReadDoubleWord 0x00  ->  0x28280400   MODER: PA5 output, PA9/PA10 AF
sysbus.gpioPortA ReadDoubleWord 0x14  ->  0x00000020   ODR:   PA5 held HIGH
```

and `/tmp/f072-uart.txt` held `hello from stm32f072` (22 bytes). So ODR is at
`0x14`, MODER is at `0x00` and reads back, the peripheral names are
`gpioPortA` through `gpioPortF`, and the console UART is `sysbus.usart1`.

Two more checks before believing any of it.

**Is it a blink or one lucky sample?** Six consecutive 50 ms windows read port C's
ODR as `0x40, 0x00, 0x40, 0x00, 0x40, 0x00`. The toggle is observable at that
offset.

**Are those the firmware's values or the platform's defaults?** Run `quiet.elf`
on the same platform. Port C reported MODER `0x00000000` and ODR `0x00000000`,
port A reported MODER `0x28000000` (the SWD pins' reset value) with ODR
`0x00000000`, and the UART file was zero bytes. Every reading in the table above
is the firmware's.

### Step 4, the ADC, because this platform actually has one

Type a peripheral's name at the Monitor and Renode lists its callable methods.
`sysbus.adc` on this platform gave, among the boilerplate:

```
Void SetDefaultValue (Decimal valueInmV, Int32? channel = null)
Void FeedVoltageSampleToChannel (Int32 channel, Decimal valueInmV, UInt32 repeat)
```

`SetDefaultValue` suits the engine's cadence: the Monitor is idle between run
windows, so a per-chunk write lands before the next instruction and every later
conversion returns it. It takes millivolts, so the recipe uses `{millivolts}`.

Proven against firmware doing a real ADEN / ADSTART / read-`ADC_DR` sequence:

```
sysbus.adc SetDefaultValue 1650 0   ->  firmware printed adc0=00000800   (2048)
sysbus.adc SetDefaultValue  825 0   ->  firmware printed adc0=00000400   (1024)
sysbus.adc SetDefaultValue 3300 3   ->  firmware printed adc3=00000fff   (4095)
channel 0 re-read afterwards        ->  firmware printed adc0=00000400   (1024)
```

`1650/3300 * 4095 = 2047.5` and `825/3300 * 4095 = 1023.75`, so the counts are the
converter's own arithmetic on the fed voltage, and the last line shows the
channels hold independent state. Channels 16 to 18 (on-die temperature, VREFINT,
VBAT) are not external nodes, so they stay unmapped and take the loud drop rather
than a fabricated count.

### Step 5, write the descriptor

```toml
[soc]
backend = "renode"
machine = "f072"
platform_repl = """
using "platforms/cpus/stm32f072.repl"

# The F072's reset default: the HSI is the system clock after reset (RM0091
# §6.2) and the walkthrough firmware enables no PLL. Both declarations are
# cross-checked against frequency_hz below.
nvic:
    systickFrequency: 8000000

cpu:
    PerformanceInMips: 8
"""
cpu_path = "sysbus.cpu"
uart = "sysbus.usart1"
frequency_hz = 8_000_000
expected_e_machine = "EM_ARM"
mcu_label = "STM32F072 (ARM Cortex-M0)"
watchdog_limitation = """\
Watchdog behaviour is UNVERIFIED on this part in this co-simulator: nobody has \
measured whether an unserviced watchdog resets the core here, so a firmware \
recovery path that depends on it is untested on this run.\
"""

[[soc.ports]]
letter = "C"
peripheral = "gpioPortC"
odr_offset = 0x14
width = 16
dir = { offset = 0x00, encoding = "moder" }
# ... ports A, B, D, E, F identically

[[soc.adc]]
channel = 0
monitor_command = "sysbus.adc SetDefaultValue {millivolts} 0"
full_scale_volts = 3.3
max_count = 4095
# ... channels 1 through 7

[soc.i2c]
controllers = []

[soc.spi]
controllers = []
```

Read the real file for the reasoning: every value carries the transcript it came
from, and the two empty controller lists carry the reason they are empty. The
I2C peripheral here is `I2C.STM32F7_I2C`, a different model class from the
`I2C.STM32F4_I2C` behind the proven F103 and F4 bridges, so that proof does not
carry across and the list stays empty until someone runs the test. The SPI
peripheral is `SPI.STM32SPI`, which *is* the class behind the F4's proven
bridges, which makes it the cheapest capability to add next, and it still needs
the live test before it is claimed.

### Step 6, install it and check it

```bash
mkdir -p ~/.config/hauksbee/mcu
cp crates/hauksbee-mcu/db/mcu/examples/stm32f072.soc.toml ~/.config/hauksbee/mcu/
hauksbee models lint ~/.config/hauksbee/mcu/stm32f072.soc.toml
```

`renode:stm32f072` now resolves to your file, with no recompile.

### Step 7, route your board's part to it

The descriptor alone is inert: nothing on a board says it is an F072. Add a
routing entry, in `~/.config/hauksbee/models/` or a directory you pass with
`--models-dir`:

```toml
[[models]]
id = "stm32f072cb"
kind = "mcu"
description = "STM32F072CB Cortex-M0 MCU, LQFP-48"

[models.match]
value_re = "(?i)^STM32F072C[8B]"

[models.params]
backend = "renode:stm32f072"

[models.pins]
# Package pin number to role, so the binder can tell which net a firmware GPIO
# drive lands on. Roles use the "p<port><bit>" convention the binder's
# gpio_of_role parses, and an ADC-capable pin adds its channel:
#
#   "<pin>" = "pa5"        a plain GPIO
#   "<pin>" = "pa0_adc0"   also ADC1_IN0
#   "<pin>" = "pb6_i2c1_scl"
#   "<pin>" = "vdd" / "vss"
#
# Take the numbers from the package pinout of the exact part, and say which
# document they came from. The shipped entries in
# crates/hauksbee-models/db/mcu.toml cite theirs (the STM32F103C8 map cites the
# KiCad MCU_ST_STM32F1 symbol and ST DS5319); a pin map from the wrong package
# variant binds firmware drives to the wrong nets, with no error.
```

`hauksbee models resolve <board> --models-dir <dir>` shows which entry won for
each component, and is the surface to debug when the match does not take.

Without a matching entry, a known family falls back to a built-in router, which
is fine for a true sibling and will never name YOUR part. A new part needs the
entry.

### Step 8, the test that makes it a contribution

`crates/hauksbee-mcu/tests/renode_stm32f072.rs`, in two layers, is the shape to
copy:

- **No emulator.** The descriptor loads, and the offsets, widths, encodings, ADC
  tokens and empty controller lists are all pinned. Milliseconds, on any machine.
- **A live boot, two-sided.** `blinky.elf` produces alternating PC6 edges, exactly
  the 22-byte USART banner, and exactly the configured-output set the firmware
  configured (PC6 and PA5, and *not* PA9/PA10, since `moder` does not count
  alternate-function pins). `quiet.elf` produces no edges, no bytes and no
  configured outputs. Plus the ADC round trip at three voltages across two
  channels, with the first channel still holding its own value afterwards.

Assert levels and counts, not "something happened": the alternation is what
distinguishes a toggle from a bridge re-reporting one register value, and an exact
byte count is what distinguishes the firmware's output from the bridge's.

The live half skips when Renode is absent or the fixtures are not built, and says
which, so a skip is never mistaken for a pass.

```bash
cargo test -p hauksbee-mcu --test renode_stm32f072
```

## Tier C, worked: RP2040, where no platform existed

Everything above assumed an emulator platform. Sometimes there is not one, and
no `.repl` can help, because a platform description wires peripheral classes
together and cannot conjure a class the emulator never compiled. Renode 1.16.1
ships no RP2040 platform, and neither does its `master`: `platforms/cpus/` has
`picosoc.repl` and `litex_picorv32.repl`, neither of which is an RP2040. There
was no SIO, no RP2040 clock tree, no RP2040 timer, no PL011-with-DREQ UART. The
*models* were missing, not their wiring.

Renode compiles C# at run time: `include <file.cs>` on the Monitor drives its
bundled compiler and registers the resulting peripheral types. A **support
bundle** is that mechanism scaled up to a whole SoC. It carries the `.cs`
peripheral models plus the data files the platform reads, embedded in the
hauksbee binary, unpacked to a temp directory at machine creation, and
`include`d in a declared order before the platform description parses. No .NET
SDK and no prebuilt DLL is needed on the user's machine.

The descriptor side is three lines:

```toml
support_bundle = "rp2040"
platform_repl = "@{support}/rp2040.repl"
extra_setup = ["sysbus LoadELF @{support}/bootrom.elf"]
```

The rest is `crates/hauksbee-mcu/src/renode/support.rs`: the file list, and the
`include` order, which is load-bearing. Renode's C# include is order sensitive,
so a later file referencing an earlier file's type fails to compile if the order
is wrong. Two entries in the RP2040 list look droppable and are not:
`rp2040_pio.cs` is never instantiated, but the SIO, GPIO, SPI and ADC models
reference its types, so omitting it is a compile error rather than a smaller
platform; and `w25q16.cs` provides a flash type the platform declares, so the
description will not parse without it.

That list is the one Rust change in this tier. It is a static array, not logic.

### The licensing discipline

Vendoring somebody else's peripheral models into this repository is a licensing
act, and it is governed by a written record, not by good intentions.
`crates/hauksbee-mcu/db/mcu/rp2040/README.md` is the pattern to follow, and a
tier-C contribution is not reviewable without its equivalent. It states, per
file or file group:

- **the exact origin**: the upstream repository and the **commit or tag**, plus
  the path within it. Not "from GitHub";
- **the licence**, with the licence text checked in beside the files
  (`LICENSE.Renode_RP2040`, `LICENSE.pico-sdk`, `LICENSE.pico-bootrom`);
- **a content hash** for any binary or compressed artifact, so a refresh that
  silently changed the bytes is detectable;
- **the refresh procedure**: clone upstream at the commit you want and copy the
  same file list. The RP2040 `.cs` files are byte-for-byte copies and nothing in
  that directory is edited. When a fix is needed it goes upstream and the file is
  re-copied, so the directory never becomes a silent fork;
- **what is deliberately not vendored, and the capability that costs**. PIO is
  absent because upstream models it as an extra CPU backed by a prebuilt
  x86-64-only native library, so any firmware whose observable behaviour goes
  through PIO produces nothing. That is recorded as a capability gap in the
  README and in the descriptor, not left to be discovered later.

### The costs, stated

The C# compiles on **every machine creation**. The RP2040 bundle's roughly 377 kB
of sources adds about eight seconds of bring-up per run. The bytes are in the
binary whether or not the part is used. And a vendored model is a model you did
not write and cannot fully vouch for: the RP2040's SPI slave bridge is impossible
because the vendored PL022 bit-bangs onto GPIO pins and never dispatches to a
registered peripheral, which is a property of upstream's model rather than a
missing descriptor entry.

## Where TOML stops

| You want | Route |
|---|---|
| A part on an existing emulator platform | `.soc.toml`, no Rust |
| A platform fix: an extra peripheral, a clock-tree register | inline `.repl` source in `platform_repl`, or `[soc.spi].extra_repl`, no Rust |
| A bring-up sequence: clock tags, a corrected PC or vector table | `extra_setup` / `post_load_setup`, no Rust |
| Peripheral models the emulator does not have | a support bundle: vendored `.cs`, plus one list in `src/renode/support.rs` |
| A different emulator entirely | a new `Mcu` trait implementation. Rust, by design |
| A new AVR part | nothing to describe: simavr's own part database does the work |
| A new ESP32-family part | the Espressif QEMU fork has to model it first. A descriptor cannot add a machine to somebody else's emulator binary |

The `Mcu` trait (`crates/hauksbee-mcu/src/traits.rs`) is a lockstep contract, not
a configuration surface, which is why a new backend stays code. Everything above
that line is data.

## The contribution checklist

Grade a pull request against this. Each item is a thing that has gone wrong
before.

**The descriptor**

- [ ] `hauksbee models lint <part>.soc.toml` is clean.
- [ ] Every register offset was read off a **running machine**, and the
      transcript is in the file's header comment. Not the datasheet alone: the
      emulator's registration point can differ from the block base, which is the
      exact bug that made the nRF52840's `odr_offset` read zero forever.
- [ ] A `dir` map is present only if its read-back was verified against a running
      machine. That its encoding covers the port's width is enforced at load, so
      the reviewable half is the read-back.
- [ ] `[soc.i2c]` and `[soc.spi]` name only controllers a byte has been watched
      to cross. Empty otherwise, with the reason written down.
- [ ] `[[soc.adc]]` uses the substitution token the model's own method signature
      calls for, and channels that are not external nodes are left unmapped.
- [ ] Internal-only channels and unmodelled peripherals are stated as gaps, not
      omitted quietly.

**Capability tiers, per feature**

- [ ] Each coupling (GPIO out, GPIO in, UART, ADC, I2C, SPI, direction, timers)
      is labelled **proven end-to-end**, **boot-only**, or **absent**, following
      [docs/cosim/MCU.md](../cosim/MCU.md), in the descriptor's header comment.
- [ ] "Proven end-to-end" means a test drove it and asserted the result. Not
      "the peripheral is registered", and not "it should work".
- [ ] Every "absent" says *why*, and whether it is a missing descriptor entry or
      a property of the emulator's model. Those have very different fixes.

**The test**

- [ ] A validation layer that needs no emulator, so the descriptor is checkable
      in seconds on any machine.
- [ ] A two-sided live test: firmware that drives a net passes, and firmware that
      drives nothing fails to produce edges, bytes or configured outputs. One
      source with a build flag is the cheapest way to get the negative half.
- [ ] Skips name what is missing (the emulator, or the fixture) and never read as
      a pass. A test that returns early having checked nothing is worse than no
      test.
- [ ] The firmware fixture's source and build recipe are checked in, not just the
      compiled ELF.

**Anything vendored (tier C only)**

- [ ] A `README.md` beside the files with origin, upstream commit or tag,
      licence, content hashes for binaries, and the refresh procedure.
- [ ] The licence text checked in beside the files.
- [ ] Files copied byte for byte, with fixes sent upstream rather than applied
      locally, so the directory does not become a silent fork.
- [ ] What is deliberately not vendored, and the capability that costs.
- [ ] The bring-up cost measured and written down.

**Wiring**

- [ ] A `[[models]] kind = "mcu"` routing entry with a pin map, so a real board
      resolves to the part. `hauksbee models resolve <board>` shows it winning.
- [ ] To ship as a built-in rather than an override: the descriptor added to
      `EMBEDDED` in `crates/hauksbee-mcu/src/soc.rs`, and the routing entry added
      to `crates/hauksbee-models/db/mcu.toml`.
- [ ] The support matrix in [docs/cosim/MCU.md](../cosim/MCU.md) updated with the
      same tiers the descriptor claims.

## What the maintainer maintains

Stated plainly, so nothing is a surprise six months later.

**Maintained:** the descriptor schema, its validation, the resolution order, the
support-bundle mechanism, and the `Mcu` contract the backends implement. If a
schema change breaks your descriptor, fixing the descriptors in the tree is part
of that change.

**Yours:** the register offsets and capability claims for your part, and any
vendored models. Nobody else has the silicon or the reference manual. A merged
descriptor whose claims turn out to be wrong gets its tier corrected downward, or
the field removed, rather than a guess at what was meant.

**Not maintained:** upstream emulator models. If a vendored peripheral is wrong,
the fix goes upstream and the file is re-copied. hauksbee will not carry a patched
fork of somebody else's model.

An override-directory descriptor that lives in your own repository is a perfectly
good end state. It needs no pull request, and `[mcu] descriptor_dir` keeps it
beside your `hauksbee-ci` spec. Upstream it when you want other people's boards
to work out of the box.

---

See [add-an-mcu-variant.md](add-an-mcu-variant.md) for a sibling of an already
supported family, [docs/cosim/MCU.md](../cosim/MCU.md) for the backend contract
these descriptors configure, and [add-a-sensor.md](add-a-sensor.md) for the
peripherals your new MCU's firmware will talk to.
