# CI for hardware

**On every layout change: boot the firmware on the emulated board, assert the
rail comes up at 4.96 V, assert the UART says hello, assert the LED blinks.**

Software was transformed by one habit: run the tests on every commit. A red
build stops a regression before it reaches anyone. Hardware has had nothing like
it. A board change goes from a layout edit to a fab order to a reflow oven to a
bench session weeks later, and the first time anyone learns the rail browns out
is with a multimeter in hand.

`galvani-ci` closes that loop. It runs the galvani PCB emulator headless in a
pipeline. Point it at a board file and (optionally) a firmware ELF, give it a
short list of assertions in a checked-in TOML file, and it boots the firmware on
the board the layout actually implements and tells you, with an exit code, a
JUnit report, and inline annotations, whether the board still does what it is
supposed to. The bug that cost weeks on the bench becomes a one-line regression
that fails on the broken layout forever.

This is not lint and it is not a DRC. It is the board, alive, under assertions:
the analogue rails, the firmware's serial output, the blink frequency, the
stress ratings, all checked the way a test suite checks a function.

## The model

```
layout change ──▶ galvani-ci run ci/power-up.toml
                     │  extract the circuit from the board file
                     │  bind every component to a device model
                     │  attach the configured power supplies
                     │  boot the firmware on the emulated MCU
                     │  run N fuzzed power-up seeds
                     ▼
                  assertions ──▶ exit 0 (green) / 1 (red)
                              ──▶ JUnit XML  (any CI ingests it)
                              ──▶ ::error    (GitHub annotations)
```

One spec describes one headless co-simulation and the things that must hold for
the build to pass. Check it into the hardware repo next to the board. Wire it
into GitHub Actions (`integrations/github-action`) or run it from KiCad
(`integrations/kicad-plugin`) or call the binary from any CI.

## Quick start

```bash
cargo build --release -p galvani-ci

# Boot the demo firmware on a small board and check rail + UART + blink.
./target/release/galvani-ci run crates/galvani-ci/examples/blinky.toml

# The flagship regression: the Tarski power-up brownout (this one FAILS).
./target/release/galvani-ci run crates/galvani-ci/examples/tarski_brownout.toml
echo $?   # 1: the rail collapses on a fuzzed power-up state

# The same board, repaired (this one PASSES).
./target/release/galvani-ci run crates/galvani-ci/examples/tarski_brownout_repaired.toml --junit results.xml
echo $?   # 0
```

## Spec format

A spec is TOML, designed to be pleasant to hand-write. Unknown keys, unknown
nets, and unknown component references are loud errors, and a misspelled net
name lists its near-matches ("did you mean `ANALOG_VDD`?").

### Top level

| Key             | Type     | Default       | Meaning                                                       |
| --------------- | -------- | ------------- | ------------------------------------------------------------- |
| `name`          | string   | `"galvani-ci"`| Label shown in reports.                                       |
| `board`         | path     | required      | Board file (`.kicad_pcb`, `.net`, Eagle `.brd`, IPC-D-356).   |
| `firmware`      | path     | none          | Firmware ELF/hex to boot on the detected MCU.                 |
| `mcu`           | string   | none          | MCU-kind hint (informational; the binder auto-detects).       |
| `duration_ms`   | float    | `100`         | Simulated time to run.                                        |
| `frame_ms`      | float    | `1`           | Sampling cadence (how often nets are read).                   |
| `suppress_rail` | [string] | `[]`          | Nets whose auto-rail is removed (fed only through the board). |

Paths are resolved relative to the spec file's directory.

### Power supplies: `[[supply]]`

The engine already models bench, wall, USB, battery, and ideal legs. Attach one
to a supply net to run the board under a realistic source.

```toml
[[supply]]
net = "+5V"
kind = "bench"            # ideal | bench | wall | usb | battery
volts = 5.0
current_limit_a = 1.0     # bench: CC foldback above this
```

Per kind: `bench` takes `volts`, `current_limit_a`; `wall` takes `volts`,
`r_out_ohms`, `ripple_vpp`, `ripple_hz`; `usb` takes `usb = "5v0.5a" | "5v1.5a"
| "5v3a"`; `battery` takes `chemistry = "liion" | "alkaline" | "nimh" |
"lifepo4"`, `cells`, `capacity_mah`, `soc`, `r_internal_ohms`; `ideal` takes
`volts`.

### Net drives: `[[net_drive]]`

Force a net to a fixed voltage for the whole run: external stimulus, a strap, or
a register bit that the firmware does not set.

```toml
[[net_drive]]
net = "WSEL"
volts = 5.0
```

### Rail suppression: `suppress_rail`

By default galvani attaches an ideal rail to every recognised supply net. When a
rail is physically fed *through* a board component (a sense shunt, a ferrite, a
regulator you want to exercise), suppress its auto-rail so the droop or collapse
is visible:

```toml
suppress_rail = ["ANALOG_VDD"]
```

### Component overrides: `[[override]]`

Swap a component's value before binding. This is how a repair is expressed:
change the wrong part to the right one and re-run the same assertions.

```toml
[[override]]
ref = "R_Shunt15301"
value = "0.05"            # the documented milliohm sense shunt
```

### Initial-state fuzzing: `[fuzz]`

Real boards power up with undefined register and latch states. Fuzzing runs the
sim under several seeds, each strapping the listed nets to a random level. An
assertion passes only if it holds on **every** seed, which is exactly what
catches a bug that only one power-up state triggers (see the brownout below).

```toml
[fuzz]
seeds = 16
nets = ["WSEL", "OE_N", "RCLK"]   # undefined power-up bits
levels = [0.0, 5.0]               # the two states each is strapped between
```

Seed 0 is always the all-low baseline; the rest are spread deterministically, so
a run is reproducible.

### Assertions: `[[assert]]`

At least one is required (a check with no assertions passes vacuously, so the
loader rejects it). Each `kind`:

**`voltage`**: a net within bounds, optionally only after the rail settles.

```toml
[[assert]]
kind = "voltage"
net = "ANALOG_VDD"
min = 4.9            # and/or max
after_ms = 50        # only sample at/after this time
```

A `min` checks the worst (lowest) the net dipped to in the window; a `max`
checks the worst (highest) it rose to.

**`uart`**: the firmware's serial output contains a string or matches a regex.

```toml
[[assert]]
kind = "uart"
contains = "galvani-demo v1"   # or: matches = "v\\d+\\.\\d+"
mcu = "U1"                      # optional; defaults to all MCUs
```

**`toggle`**: a net toggles at an expected frequency (a blink check) or a
minimum number of times.

```toml
[[assert]]
kind = "toggle"
net = "D13"
freq_hz = 5.0        # or: min_toggles = 8
tolerance = 0.25     # fractional, default 25%
```

**`no_faults`**: the stress monitor raised no over-current / over-voltage /
over-power / reverse-bias faults across the run.

```toml
[[assert]]
kind = "no_faults"
```

**`max_current`**: a component's peak through-current stays below a ceiling.
(Tracked for resistors and diodes; other kinds are covered by `no_faults`.)

```toml
[[assert]]
kind = "max_current"
ref = "R1"
amps = 0.02
```

## Output

- **Human report** to stdout: each assertion `PASS`/`FAIL` with the measured
  value, then a `GREEN`/`RED` summary.
- **Exit code**: `0` if every assertion passed, `1` if any failed, `2` on a
  spec/board error.
- **JUnit XML** with `--junit out.xml`: one `<testcase>` per assertion, so
  GitLab, Jenkins, GitHub, Buildkite, or anything else surfaces the results.
- **GitHub annotations**: when `GITHUB_ACTIONS` is set, `::error` / `::notice`
  workflow commands are emitted so failures show inline in the Checks UI.

## Worked example: the Tarski power-up brownout

This is the bug that motivated the whole project, turned into a regression test.

On the Tarski board the analogue rail `ANALOG_VDD` is fed through a part labelled
a "shunt", `R_Shunt15301`, which is **1 kΩ**. A current-sense shunt should be
milliohms. Separately, the 74HC595 weight registers power up in undefined states
(no pull-ups on `OE'`/`SRCLR'`/`RCLK`, and the bootloader clocks garbage in over
SCLK), so a stray bit can enable an inhibitory synapse weight at boot. That
weight drives a miswired base path that pulls destruction-scale current, and
through the 1 kΩ shunt that single cell collapses the *entire* rail.

No single-defect tool predicts this. It is an interaction: a wrong resistor
value, plus a floating register, plus a wiring error, compounding. On the bench
it showed up as "voltages low enough to affect operation" and cost weeks.

The flagship spec encodes it. The board is the real brownout cell, extracted
verbatim from the 3,442-component Tarski input system and checked in as a
standalone netlist (`testdata/tarski_brownout_cell.net`), bound by the ordinary
pipeline:

```toml
# crates/galvani-ci/examples/tarski_brownout.toml
name = "tarski power-up brownout (as designed)"
board = "../../../testdata/tarski_brownout_cell.net"
duration_ms = 1

suppress_rail = ["ANALOG_VDD"]            # rail fed only through the shunt

[[net_drive]]
net = "+5V"
volts = 5.0
[[net_drive]]
net = "+5P"
volts = 5.0
[[net_drive]]
net = "Net-(ANALOG_SWITCH3905-S)"          # the 74HC595 Q4 weight bit
volts = 0.0

[fuzz]                                     # power-up the weight bit randomly
seeds = 8
nets = ["Net-(ANALOG_SWITCH3905-S)"]
levels = [0.0, 5.0]

[[assert]]
kind = "voltage"
name = "ANALOG_VDD comes up across all power-up register states"
net = "ANALOG_VDD"
min = 4.9
```

Run it:

```
[FAIL] ANALOG_VDD comes up across all power-up register states
       seed 1: ANALOG_VDD: min=0.759V (>= 4.9V) [settled 0.759V]

0/1 assertions passed - RED
```

Seed 1 is the one where the weight bit booted high. The rail collapses from
**4.96 V to 0.759 V**: one stray boot-time bit makes the whole network
non-functional, and the fuzzing is what surfaces it (a single all-low run would
have looked healthy).

Now the repair. The documented fix is a real milliohm sense shunt. Expressed as
a one-line override on the same board, same fuzz, same assertion:

```toml
# crates/galvani-ci/examples/tarski_brownout_repaired.toml
[[override]]
ref = "R_Shunt15301"
value = "0.05"            # milliohm-class sense shunt instead of 1 kΩ
```

```
[PASS] ANALOG_VDD comes up across all power-up register states
       ANALOG_VDD: min=4.966V (>= 4.9V) [settled 4.966V] (held across 8 seeds)

1/1 assertions passed - GREEN
```

With the milliohm shunt the same enabled weight cannot drop the rail; it holds
at **4.966 V** across all eight power-up states. The single override is the whole
difference between RED and GREEN. That is the model: a bug that cost weeks on the
bench is now caught in 0.1 s, on every layout change, by a regression that can
never be silently lost.

These two specs are also an integration test
(`crates/galvani-ci/tests/flagship_brownout.rs`), so galvani's own CI proves the
broken layout stays red and the fixed one stays green.

## Wiring it into your repo

- **GitHub Actions**: copy `integrations/github-action/example-workflow.yml`
  into `.github/workflows/`. It builds galvani-ci (with cargo caching), runs
  your spec on every board/firmware change, and publishes the JUnit results to
  the Checks tab. See `integrations/github-action/README.md`.
- **KiCad**: install the pcbnew action plugin from `integrations/kicad-plugin`
  to run a spec on the open board and see the verdict in a dialog.
- **Any CI**: call `galvani-ci run spec.toml --junit results.xml` and consume
  the exit code and the JUnit file.

## Limitations

- The MCU co-sim currently targets AVR (ATmega328P via simavr); other cores fall
  back to that backend.
- `max_current` peak tracking covers resistors and diodes directly; other
  component kinds are covered by the `no_faults` stress monitor rather than a
  numeric ceiling.
- Fuzzing perturbs the named nets' initial logic levels (the undefined power-up
  bits); it does not yet randomize internal MCU RAM.
- The flagship brownout fixture is a trimmed cell of the full Tarski board, so
  the regression runs in milliseconds. The full 3,442-component board extracts
  in ~0.3 s but is heavier to co-simulate; trim to the subcircuit a given check
  cares about, the way the fixture does.
