# CI for hardware

**On every layout change: boot the firmware on the emulated board, assert the
rail comes up at 4.96 V, assert the UART says hello, assert the LED blinks.**

One habit transformed software: run the tests on every commit. A red build
stops a regression before it reaches anyone. Hardware has had nothing like it.
A board change goes from a layout edit to a fab order to a reflow oven to a
bench session weeks later. The first time anyone learns the rail browns out is
with a multimeter in hand.

`hauksbee-ci` closes that loop. It runs the hauksbee PCB emulator headless in
a pipeline. Point it at a board file and (optionally) a firmware ELF, and give
it a short list of assertions in a checked-in TOML file. It boots the firmware
on the board the layout actually implements. It then reports, with an exit
code, a JUnit report, and inline annotations, whether the board still does
what it is supposed to. The bug that cost weeks on the bench becomes a
one-line regression that fails on the broken layout forever.

This is not lint and it is not a DRC. It is the board, alive, under
assertions: the analog rails, the firmware's serial output, the blink
frequency, the stress ratings, all checked the way a test suite checks a
function.

**Where the web page fits.** `hauksbee serve`'s drop-a-board page is the quick
look: one board, one optional firmware image, one plain-language report. The
TOML spec on this page is the *repeatable* check: it pins down how the board
is powered, which firmware boots, and what must hold, so a pipeline can run it
on every commit. Assertions exist only in specs and run only through
`hauksbee-ci`. The browser never evaluates them. When a quick look turns into
something you want to keep true, `hauksbee-ci init <board>` turns the board
into a starter spec.

### The static-check corpus gates (a separate, complementary layer)

The bring-up CI above runs *firmware on a board under assertions*. The static
checks (`--drc`, `--lint`, `--si`) have their own enforcement: a
**zero-false-positive corpus gate**, the standing discipline that a check
ships only if it raises no findings on the known-good famous corpus. These
are encoded as corpus-gated cargo tests rather than spec assertions. They are
the CI-level guarantee for the checks the bug-hunt tooling work added:

- **DRC clearance tolerance**: `cargo test -p hauksbee-extract --test drc`
  (boundary/at-rule/sub-rule cases) plus the `drc_corpus` / `eagle_drc_corpus`
  sweeps stay green. The at-rule noise drops (bms-c1 137 -> 0, pd-sink 66 -> 4).
- **Trace ampacity + input-cap ripple** (`--si` checks 6, 7):
  `HAUKSBEE_REQUIRE_CORPUS=1 cargo test -p hauksbee-engine --test si_ampacity_ripple`
  asserts the checks fire on a genuinely undersized routed trace and raise
  **zero findings** across the famous corpus (the `famous_corpus_has_no_ampacity
  _or_ripple_findings` sweep is the assertion). The hand-checked mppt-1210 C1
  1.66x ripple case is a unit test in `checks::ripple`.
- **Device-decode** (`--lint`): `cargo test -p hauksbee-engine device_decode`
  pins the CYPD3177 Table-2 decode against the hunt's hand-derived detents.

Run `HAUKSBEE_REQUIRE_CORPUS=1` to turn a missing corpus into a hard failure so
none of these gates can vacuously green-out.

## The model

![Every layout change runs the spec headless and produces an exit code, JUnit XML and inline annotations](../assets/diagrams/ci-model.svg)

One spec describes one headless co-simulation and the things that must hold for
the build to pass. Check it into the hardware repo next to the board. Wire it
into GitHub Actions (`integrations/github-action`) or run it from KiCad
(`integrations/kicad-plugin`) or call the binary from any CI.

## Quick start

Rather than hand-write a spec from a blank page, scaffold one from your board,
`init` detects the supplies, MCU, and nets and emits a starter spec you edit:

```bash
hauksbee-ci init my_board.kicad_pcb   # writes a starter spec beside the board and prints its path
# edit the scaffolded spec's [[assert]] blocks (it lands at my_board.toml), then:
hauksbee-ci run my_board.toml
```

Or run the bundled examples straight away:

```bash
cargo build --release -p hauksbee-ci

# Boot the demo firmware on a small board and check rail + UART + blink.
./target/release/hauksbee-ci run crates/hauksbee-ci/examples/blinky.toml

# The flagship regression: the Tarski power-up brownout (this one FAILS).
./target/release/hauksbee-ci run crates/hauksbee-ci/examples/tarski_brownout.toml
echo $?   # 1: the rail collapses on a fuzzed power-up state

# The same board, repaired (this one PASSES).
./target/release/hauksbee-ci run crates/hauksbee-ci/examples/tarski_brownout_repaired.toml --junit results.xml
echo $?   # 0
```

### Custom parts: the model library and `--models-dir`

`hauksbee-ci run` binds the board against the same layered model library as
`hauksbee run`: the builtin db, then installed packs, then the user model dirs
(`~/.hauksbee/models`, `~/.config/hauksbee/models`), then `--models-dir DIR`
(highest priority). A custom `[[models]]` entry, including a
`kind = "mcu"` routing entry that maps a part value to a `renode:<part>` /
`qemu:<part>` backend and its SoC descriptor, therefore binds in CI exactly
as it does interactively:

```bash
hauksbee-ci run ci/board.toml --models-dir hardware/models
```

MCU SoC descriptors themselves resolve from `$HAUKSBEE_MCU_DIR` /
`~/.config/hauksbee/mcu` before the built-ins, in CI and interactive runs
alike (see `docs/extending/add-an-mcu-variant.md` for the full two-file
recipe). `hauksbee-ci init`'s scaffold uses the same layered library, so the
detected MCU matches what a run would bind. Note: the spec's top-level `mcu`
key does NOT select the MCU. It is an informational note only. The board's
part value plus the routing entries decide.

## Spec format

A spec is TOML, designed to be pleasant to hand-write. Unknown keys, unknown
nets, and unknown component references are loud errors, and a misspelled net
name lists its near-matches ("did you mean `ANALOG_VDD`?").

### Top level

| Key             | Type     | Default       | Meaning                                                       |
| --------------- | -------- | ------------- | ------------------------------------------------------------- |
| `name`          | string   | `"hauksbee-ci"`| Label shown in reports.                                       |
| `board`         | path     | required      | Board file: `.kicad_sch` (schematic), `.kicad_pcb`, `.net`, Eagle `.brd`, IPC-D-356, Altium `.PcbDoc`, a gerber folder/zip, or Board-as-Code `.board` (bare or zipped; compiled in-process, no routing step needed). The same formats `hauksbee run` accepts. |
| `firmware`      | path     | none          | Firmware to boot on the detected MCU: a compiled ELF/hex, a PlatformIO project directory, or a zip of either (same three input tiers as `run --firmware`, see [`../cosim/MCU.md`](../cosim/MCU.md)). |
| `mcu`           | string   | none          | Informational note only, nothing reads it. The MCU comes from the BOARD's part value via `[[models]] kind = "mcu"` routing entries (builtin, user model dirs, `--models-dir`); this field cannot force a backend. |
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

Per kind:

- `bench` takes `volts`, `current_limit_a`.
- `wall` takes `volts`, `r_out_ohms`, `ripple_vpp`, `ripple_hz`.
- `usb` takes `usb = "5v0.5a" | "5v1.5a" | "5v3a"`.
- `battery` takes `chemistry = "liion" | "alkaline" | "nimh" | "lifepo4"`,
  `cells`, `capacity_mah`, `soc`, `r_internal_ohms`.
- `ideal` takes `volts`.

### Net drives: `[[net_drive]]`

Force a net to a fixed voltage for the whole run: external stimulus, a strap, or
a register bit that the firmware does not set.

```toml
[[net_drive]]
net = "WSEL"
volts = 5.0
```

### Rail suppression: `suppress_rail`

By default hauksbee attaches an ideal rail to every recognised supply net. When a
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

### The as-built overlay: `asbuilt`

`[[override]]` swaps a component's *value string* before binding. Some boards
differ from their design files more radically: traces cut with a knife, pins
lifted, jumper wires soldered, parts replaced. That is the physical rework
record. That delta lives in a declarative `.asbuilt.toml` overlay, and a spec
can reference one. hauksbee applies it to the bound board before every run,
ahead of any harness attachment:

```toml
asbuilt = "tarski.asbuilt.toml"   # relative to the spec file, like `board`
```

The overlay is fail-loud: an entry that matches nothing (or a different number
of devices/terminals than it declares) aborts the run with a line-numbered
error and a did-you-mean suggestion. The flagship example is the Tarski
board's validated surgery, `testdata/tarski.asbuilt.toml`. This is the same
file the engine CLI takes as `hauksbee run board --asbuilt
board.asbuilt.toml`.

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

Seed 0 is always the all-low baseline. The rest are spread deterministically,
so a run is reproducible.

### Component tolerances: `[[tolerance]]` + `[ensemble]`

A board that only meets its assertions at *nominal* component values is a
latent defect: real parts are ±1% / ±5% / ±10%, and some fraction of assembled
units will land outside the window. Declare the tolerances and hauksbee-ci
replays the whole assertion set across an **ensemble** of sampled builds, on
every commit.

```toml
[[tolerance]]
ref = "R*"                 # a literal ref, or a pattern (* matches any run)
percent = 10.0             # ±10% of the component's value
distribution = "gaussian"  # optional; default "uniform"

[ensemble]
seeds = 24                 # Monte-Carlo member count (default 16)
mode = "monte-carlo"       # "monte-carlo" (default) | "corners"
```

Rules apply in order and the **last matching rule wins** per component, so a
broad `ref = "R*"` can be followed by a tighter `ref = "R7"`. The nominal is
the component's board value (after any `[[override]]`). An override can also
declare its own spread, and then the `value` becomes the nominal:

```toml
[[override]]
ref = "R_Shunt15301"
value = "0.05"
tolerance = 1.0            # the repaired shunt is a ±1% part
```

Distributions: `"uniform"` samples the full ±tolerance band (assumes nothing
about vendor binning, stresses the edges hardest, and is the default).
`"gaussian"` uses the standard EDA convention, sigma = tolerance/3 truncated
at the tolerance bound (a part outside its marked tolerance would have been
binned out at the factory).

**What a green ensemble means, and does not.** Monte-Carlo is *sampled
coverage*: "passed 24/24 sampled tolerance seeds" is statistical evidence over
the tolerance space, **never a worst-case proof**, and the report words it
exactly that way. An assertion passes only if it holds on **every** member.

**Corner mode** (`mode = "corners"`) is the deterministic complement: instead
of random samples it enumerates every all-min/all-max combination of the
toleranced components (2^n runs). For a response that is *monotonic* in each
component value (dividers, ladders, most DC bias networks) the true worst
case is a corner, so a green corner run bounds it. For non-monotonic responses
(filters peaking mid-band, matched pairs) the interior can be worse than any
corner, so the report claims boundedness **only for monotonic responses**.
Full enumeration is capped at 2^10 = 1024 components' corners. Above 10
toleranced components, corner mode refuses and points at Monte-Carlo. Corner
mode does not compose with `[fuzz]` (the corner index enumerates min/max
combinations, not fuzz seeds). Monte-Carlo does compose with `[fuzz]`.

**Reproducibility is doctrine.** Every sampled value is a pure function of
(spec, seed, component reference): seed 0 is always the nominal baseline, the
tolerance stream is domain-separated from the net-fuzz stream (adding a
tolerance never changes which fuzz levels seed N straps), and when `[fuzz]`
and `[ensemble]` are both present one shared seed stream drives both (the
member count is the larger of the two `seeds`). A failure names the seed and
the exact sampled values:

```
[FAIL] VOUT stays in [2.4, 2.6] V across the tolerance ensemble
      seed 8: VOUT: min=2.695V ... [R1=9.17k, R2=10.7k]; passed 19/24 seeds (failing: 8, 10, 16, 18, 19)
```

Re-run that one build in isolation. It reproduces byte-identically:

```bash
hauksbee-ci run ci/divider.toml --seed 8
```

A runnable two-sided demo ships in the examples: the same 10k/10k divider
passes at nominal and fails across the ±10% ensemble
(`crates/hauksbee-ci/examples/tolerance_divider.toml`), and its corner variant
lands exactly on the hand-computable [2.25, 2.75] V envelope
(`tolerance_divider_corners.toml`). Both are pinned as integration tests
against that analytic envelope (`crates/hauksbee-ci/tests/tolerance.rs`).

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

A `min` checks the worst (lowest) the net dipped to in the window. A `max`
checks the worst (highest) it rose to.

**`uart`**: the firmware's serial output contains a string or matches a regex.

```toml
[[assert]]
kind = "uart"
contains = "hauksbee-demo v1"   # or: matches = "v\\d+\\.\\d+"
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
over-power / reverse-bias / over-temperature faults across the run.

```toml
[[assert]]
kind = "no_faults"
```

**`max_current`**: a component's peak through-current stays below a ceiling.
(Tracked for resistors and diodes. Other kinds are covered by `no_faults`.)

```toml
[[assert]]
kind = "max_current"
ref = "R1"
amps = 0.02
```

**`max_temp`**: a component's steady-state junction temperature
(`Tj = Tambient + P * theta_JA`) stays below a ceiling. Omit `celsius` to check
against the device's own max junction temperature (from the model DB, or the
per-package-class default). The ambient is set once per spec with the top-level
`ambient_c` key (default 25 C). See [THERMAL.md](../checks/THERMAL.md) for the model.

```toml
ambient_c = 70          # top-level: hot-enclosure ambient for the whole run

[[assert]]
kind = "max_temp"
ref = "U1"
celsius = 125           # optional; defaults to the device's own Tj(max)
```

**`boot-coverage`**: a control net (a gate / enable / reset / chip-select) that
the firmware must *actively drive* to a defined level within a deadline of
reset, with no stress fault during the boot window before it is first driven.

```toml
[[assert]]
kind = "boot-coverage"
net = "GATE_CTRL"     # the control net to watch
min = 3.0             # the driven level (V) the firmware must reach
deadline_ms = 20.0    # by this long after reset
```

See [the boot-coverage section](#boot-coverage-watching-the-firmware-define-a-hi-z-control-net) for what problem this solves and the two-sided demo.

**`model_coverage`**: how much of the board bound to a real device model.

Analogue accuracy is capped by model availability, and part of that cap is
outside your control: many vendors encrypt their SPICE and IBIS models. What
you can control is whether the number is visible and whether it is allowed to
fall. Pin what your board reaches today, and the day a new part drops coverage
the build says so instead of quietly simulating a hole.

```toml
[[assert]]
kind = "model_coverage"
min_critical = 1.0          # every active IC binds to a real model
max_active_unresolved = 0   # nothing unresolved sits on a connected net
# min_resolved = 0.85       # optional: all parts, as a board-wide trend line
```

`min_critical` is the metric that matters. An unbound regulator changes the
answer, an unbound 0402 resistor usually does not, so the ratio counts active
ICs rather than every part on the board. `max_active_unresolved` counts the
unresolved parts whose pins touch a real node, which are the ones whose open
default actually moves the solve.

A failure names the parts, because that list is the next action:

```
[FAIL] model_coverage
      active ICs bound 0/4 (0.0%), floor 95.0%; 4 unresolved on connected
      nets (limit 0): U1, U3, U2, U4
```

Each name is either a model you can write, which takes one TOML file and no
recompile ([extending/](../extending/README.md)), or a part whose vendor keeps
its model closed. Those need different responses, and the report tells them
apart by naming both.

At least one threshold is required. An assertion with none would sit in a spec
looking like a coverage gate while checking nothing, which is the failure this
assertion exists to prevent, so the spec loader rejects it.

## Waiving a finding you have judged

A check that fires on your board and is wrong leaves you two bad options: live
with a red build, or stop running the check. Nobody removes one rule, they drop
the suite, and then the tool stops catching the things it was right about.

A waiver is the third option. Put `hauksbee-waivers.toml` beside the board:

```toml
[[waive]]
check = "si"                      # "si", "lint" or "drc"
kind = "controlled_impedance"     # the rule, as it appears in --json
nets = ["USB_DP", "USB_DM"]       # or refs = ["U3"]
reason = "measured 92 ohm on the fab's stackup; our stackup file is wrong"
until = "2026-12-31"
```

`hauksbee run <board> --check` and the combined `--json` report both read it.
Findings an active waiver covers come out of the gate and appear in their own
section:

```
== Waived (2) ==
These findings fired and were overruled. They are not in the gate.
  drc/short: GND to +5V on B.Cu
      because: the pour bridges these on purpose (until 2026-12-31)
```

Four rules, all enforced rather than suggested:

- **`reason` is required.** Six months on, a waiver with no reason cannot be
  told apart from a bug.
- **`until` is required.** A waiver that never lapses is a disabled check
  wearing a different hat. On the date it expires the finding comes back, and
  the report names the lapsed waiver so the red is explainable.
- **`nets` or `refs` is required.** Without one, the waiver silences the rule
  across the whole board, which is turning the check off with extra steps. A
  second occurrence elsewhere still gates.
- **Waived is not hidden.** A board carrying ten overruled findings must not
  look like a clean one. Active waivers that matched nothing are called out
  too, since either the finding is fixed or the waiver no longer describes what
  fires.

A waiver file that does not parse is a warning, not a failed run, and every
finding it would have covered gates. A typo must never quietly disable a check.

Clearance violations are not waivable: they do not gate on their own, so there
is nothing to excuse them from.

**`phase_margin` / `ac_gain`**: small-signal loop-stability and frequency-response
gates, driven by an `[ac]` sweep block on the spec. `phase_margin` bounds the
feedback loop's phase margin (degrees) and `ac_gain` bounds a net's magnitude
(dB) at a frequency. The spec format and worked assertions live in
[AC_ANALYSIS.md](../analysis/AC_ANALYSIS.md#ci), since the sweep block is documented
alongside the analysis it drives.

**`rail_window`**: over a transient `[[scenario]]` window, a rail's min/max
voltage stays within bounds and any dip below a floor recovers within a deadline
(the brownout/inrush check).

```toml
[[assert]]
kind = "rail_window"
net = "VBUS"
scenario = "load_step"    # the [[scenario]] whose window to judge over
min = 3.0                 # brownout floor
dip_below = 3.1           # optionally: how long it may sit below a level
for_max_ms = 5
recover_to = 3.2
recover_within_ms = 20
```

**`protection_trip`**: a battery/e-fuse protection cutoff fired (or must NOT
have fired) within a scenario window, `supply_net`, `expect_trip = true|false`,
optional `scenario`.

**`peripheral`**: a bus-slave / sensor / VCD-sink peripheral's end-of-run state,
by `id` and `field` (e.g. a `vcd_sink`'s `transitions` count, an EEPROM's last
address). Pairs with a `[[peripheral]]` block below.

**`hwtrace`**: compare the simulated waveform against a captured scope trace
(features like period / rise-time / overshoot within tolerance), `trace =
"trace.toml"`. See [the hardware-trace docs](../../testdata/hwtraces) for the
capture format.

Beyond `[[assert]]`, a spec can also declare **`[[peripheral]]`** (attach an I2C/
SPI slave model, a VCD waveform sink, or an ADC feed to the co-sim),
**`[[sensor]]`** (a declarative register-map sensor defined inline), and
**`[[scenario]]`** (a transient load profile + optional decoupling ESR/ESL that
the `rail_window` / `protection_trip` assertions judge over). The field
reference for each block is in [PERIPHERALS.md](../cosim/PERIPHERALS.md) (peripheral /
sensor) and [TRANSIENTS.md](../checks/TRANSIENTS.md) (scenario). For a runnable
`[[sensor]]` example see `lm75_thermostat.toml` in
`crates/hauksbee-ci/examples/`. `[[peripheral]]` and `[[scenario]]` blocks are
exercised by `crates/hauksbee-ci/tests/peripherals.rs` and
`tests/spec_and_assertions.rs` (copy-paste-able TOML fixtures).

## Output

- **Human report** to stdout: each assertion `PASS`/`FAIL` with the measured
  value, then a `GREEN`/`RED` summary.
- **Exit code**: `0` if every assertion passed, `1` if any failed, `2` on a
  spec/board error.
- **JUnit XML** with `--junit out.xml`: one `<testcase>` per assertion, so
  GitLab, Jenkins, GitHub, Buildkite, or anything else surfaces the results.
- **GitHub annotations**: when `GITHUB_ACTIONS` is set, `::error` / `::notice`
  workflow commands are emitted so failures show inline in the Checks UI.

For a machine-readable single-board result outside the assertion runner,
`hauksbee run <board> --json` emits a documented object with a top-level
`ok`/`verdict`/`serious_count` rollup, see [JSON_OUTPUT.md](../analysis/JSON_OUTPUT.md).

## Worked example: the Tarski power-up brownout

This is the bug that motivated the whole project, turned into a regression
test.

On the Tarski board the analog rail `ANALOG_VDD` is fed through a part
labeled a "shunt", `R_Shunt15301`, which is **1 kΩ**. A current-sense shunt
should be milliohms. Separately, the 74HC595 weight registers power up in
undefined states (no pull-ups on `OE'`/`SRCLR'`/`RCLK`, and the bootloader
clocks garbage in over SCLK), so a stray bit can enable an inhibitory synapse
weight at boot. That weight drives a miswired base path that pulls
destruction-scale current. Through the 1 kΩ shunt, that single cell collapses
the *entire* rail.

No single-defect tool predicts this. It is an interaction: a wrong resistor
value, plus a floating register, plus a wiring error, compounding. On the
bench it showed up as "voltages low enough to affect operation" and cost
weeks.

The flagship spec encodes it. The board is the real brownout cell, extracted
verbatim from the 3,442-component Tarski input system and checked in as a
standalone netlist (`testdata/tarski_brownout_cell.net`), bound by the ordinary
pipeline:

```toml
# crates/hauksbee-ci/examples/tarski_brownout.toml
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
# crates/hauksbee-ci/examples/tarski_brownout_repaired.toml
[[override]]
ref = "R_Shunt15301"
value = "0.05"            # milliohm-class sense shunt instead of 1 kΩ
```

```
[PASS] ANALOG_VDD comes up across all power-up register states
       ANALOG_VDD: min=4.966V (>= 4.9V) [settled 4.966V] (held across 8 seeds)

1/1 assertions passed - GREEN
```

With the milliohm shunt the same enabled weight cannot drop the rail. It
holds at **4.966 V** across all eight power-up states. The single override is
the whole difference between RED and GREEN. That is the model: a bug that
cost weeks on the bench is now caught in 0.1 s, on every layout change, by a
regression that can never be silently lost.

These two specs are also an integration test
(`crates/hauksbee-ci/tests/flagship_brownout.rs`), so hauksbee's own CI
proves the broken layout stays red and the fixed one stays green.

## Boot-coverage: watching the firmware define a Hi-Z control net

There is a class of fault the *netlist alone cannot adjudicate*. A control
net (a MOSFET gate, a level-translator enable, a display reset, a
chip-select) is driven only by an MCU GPIO that goes high-impedance at reset.
Its power-up default is therefore undefined. Whether that is a *bug* depends
on the intended default state of the controlled load, and the netlist does
not encode that: a display that must be on by default and a haptic motor that
must be off by default have byte-identical netlist topology. Two real such
faults (Watchy e-paper RES#, ZSWatch DISPLAY-EN) are recorded as honest
misses of the static checks for exactly this reason. A static check firing
there would be a confident false positive on a shipped board, on the very
same topology that is correct elsewhere.

The `boot-coverage` assertion makes the class decidable by running the
firmware. Instead of guessing the intended default, it **watches what the
firmware actually does**. It runs the co-sim from reset and requires the MCU
to drive the named control net to a defined level within a deadline, with no
stress fault during the boot window before the net is first driven (rails
hold while the input is still undefined).

### The two-sided demo

One constructed board, `crates/hauksbee-ci/examples/boards/boot_gate.kicad_pcb`:
an ATmega328P whose PB0 drives a 2N7002 N-MOSFET gate, with **no pull resistor on
the gate net** - so at reset the gate floats (Hi-Z), exactly the undefined-default
shape. Two real AVR firmware variants (built with `avr-gcc`,
`testdata/firmware/boot_gate_{a,b}/`):

```bash
# variant A configures PB0 promptly and drives the gate HIGH -> PASS
hauksbee-ci run crates/hauksbee-ci/examples/boot_gate_pass.toml
#   [PASS] GATE_CTRL driven to >= 3 V within 20 ms of reset
#          control net 'GATE_CTRL' driven to >= 3 V at 1.00 ms (<= 20 ms), boot window clean

# variant B never touches PB0; the gate floats the whole run -> FAIL
hauksbee-ci run crates/hauksbee-ci/examples/boot_gate_fail.toml
#   [FAIL] GATE_CTRL driven to >= 3 V within 20 ms of reset
#          control net 'GATE_CTRL' was NEVER driven to >= 3 V
#          (firmware left it Hi-Z / undefined through the whole run)
```

The check has teeth only because variant B goes RED: the same board, the
same assertion, two firmwares, opposite verdicts. This is pinned as an
integration test, `crates/hauksbee-ci/tests/boot_coverage.rs`.

### Backend reach (stated honestly)

This proof uses the **AVR (simavr)** backend, one of hauksbee's **three**
co-sim backends. The mechanism is backend-agnostic and runs on all three.
Besides AVR, the **Renode** backend co-sims STM32, **nRF52**, and SiFive
RISC-V, and the **QEMU** backend co-sims ESP32 / ESP32-S3 / ESP32-C3 (see
`docs/cosim/MCU.md` for the full matrix). nRF52 works out of the box today:
`hauksbee run` boots the bundled
`testdata/firmware/renode_demos/nrf52840-zephyr_shell.board` +
`nrf52840-zephyr_shell.elf` pair to the Zephyr `uart:~$` prompt through
Renode.

A backend covers both faulted boards named above: ZSWatch is nRF52 (Renode)
and Watchy is ESP32 (QEMU), and both architectures co-sim. What each still
needs to run *this* boot-coverage check is its own firmware image built for
the target.

Honest per-backend caveat for boot-coverage: GPIO-drive detection reads the
port's output register once per co-sim chunk. On AVR (cycle-accurate simavr)
and Renode (STM32/nRF52 ODR poll) that read is direct. The ESP32 QEMU model
does not expose GPIO output read-back, so the firmware must mirror its output
word to the RAM mailbox the backend reads (the bundled ESP32 demo does).
Edges faster than the chunk alias on the poll bridge either way, so match the
firmware switching rate to the chunk size (see `docs/cosim/MCU.md`
limitations).

## Schematic-stage CI

**Catch it before you even lay out the board.** Point a spec's `board` at a
`.kicad_sch` and hauksbee-ci runs the same headless co-simulation against the
schematic, with no PCB in existence yet. hauksbee derives the netlist
geometrically from the schematic, the way eeschema derives it (wires, pins,
junctions, labels, power symbols, hierarchy). See `docs/ingest/SCHEMATICS.md`.
Everything else in this document, supplies, drives, fuzzing, every assertion
kind, works unchanged.

This is where the most expensive hardware bugs are cheapest to catch. They
are schematic-level faults: the original Raspberry Pi 4's USB-C port that
would not charge from compliant cables was a *schematic* mistake (the two CC
pins shared one resistor instead of one each), shipped on millions of boards.
A rail that browns out, a missing pull-up, a power pin on the wrong net, an
interaction between a wrong value and a floating strap: these are all
decidable from the schematic alone, weeks before a layout exists.
Schematic-stage CI is the commit that turns "we found it on the bench" into
"the build went red."

```toml
# A schematic-stage spec: board is a .kicad_sch hierarchy root.
name = "power-up sanity (schematic)"
board = "hardware/myboard.kicad_sch"
duration_ms = 1

[[supply]]
net = "VCC"
kind = "ideal"
volts = 5.0

[[assert]]
kind = "voltage"
net = "VCC"
min = 4.99
max = 5.01

[[assert]]
kind = "no_faults"
```

A runnable example against KiCad's own `pic_programmer` demo (a 2-sheet
hierarchy) ships at `crates/hauksbee-ci/examples/pic_programmer_schematic.toml`:

```bash
hauksbee-ci run crates/hauksbee-ci/examples/pic_programmer_schematic.toml
```

### Load the hierarchy root, not a sub-sheet

hauksbee loads a `.kicad_sch` board **by path** so its sheet hierarchy
resolves: sub-sheets live in sibling files, and only the path-based loader
follows them. Point the spec at the **hierarchy root**. If you point it at a
sub-sheet (a file referenced by another `.kicad_sch` in the same or parent
directory), hauksbee-ci stops with a clear error naming the root, rather than
silently extracting one page and running a partial board:

```
invalid spec: board pic_sockets.kicad_sch is a sub-sheet of
pic_programmer.kicad_sch. Point the spec at the hierarchy root
(pic_programmer.kicad_sch) so the whole design is loaded, not one page of it
```

### The agreement guarantee

For a project that has *both* a schematic and a layout, the same spec run at
either stage returns the same verdict. Tests validate that the schematic
netlist and the PCB netlist induce the identical partition of pins into nets
(see hauksbee-extract's cross-validation tests), so a `voltage` / `no_faults`
/ blink check passes on the `.kicad_sch` exactly when it passes on the
`.kicad_pcb`.

That property matters: the schematic-stage check is not a weaker
approximation you re-do later. It is the *same* check, available earlier. An
integration test enforces this
(`crates/hauksbee-ci/tests/schematic_ci.rs`): it runs the `pic_programmer`
spec against both the schematic and the layout, and the per-assertion results
must agree.

### Editor integration: the honest state

KiCad's PCB editor (pcbnew) has an action-plugin API, and hauksbee-ci ships a
plugin for it (`integrations/kicad-plugin`). The **schematic editor
(eeschema) has no equivalent yet**: KiCad's new IPC plugin API is implemented
for the PCB editor only in KiCad 9 and 10, schematic-editor support is
explicitly future work, and headless operation through `kicad-cli` only
arrives in KiCad 11. This documentation does not fake an eeschema button that
cannot exist.

So drive schematic-stage CI the way it is actually natural to drive it:

- **Pre-commit hook** (recommended): `integrations/pre-commit` runs the matching
  spec whenever a staged `.kicad_sch` (or `.kicad_pcb`) changes and blocks the
  commit if it goes RED. The commit, not the editor, is the right gate for a
  schematic-level fault, and this is the most natural schematic-stage
  integration anyway.
- **CLI**: `hauksbee-ci run myboard-schematic.toml` from a Makefile, a watch
  script, or by hand.
- **From pcbnew, on the project**: the pcbnew plugin discovers every spec next to
  the board (and in a sibling `ci/`), including specs whose `board` is the
  project's `.kicad_sch`, so you can run a schematic-stage check from the PCB
  editor on the same project today.

When eeschema gains a plugin API, the entry point drops in next to the
existing one: the shared core (`hauksbee_ci_core.py`) is already
file-type-agnostic. It only handles the spec path, and the binary does the
rest.

## Wiring it into your repo

- **Pre-commit (schematic or layout)**: copy the `repos:` entry from
  `integrations/pre-commit/.pre-commit-config.yaml` into your repo and run
  `pre-commit install`. Hardware checks then run before a commit lands. See
  `integrations/pre-commit/README.md`.
- **GitHub Actions**: copy `integrations/github-action/example-workflow.yml`
  into `.github/workflows/`. It builds hauksbee-ci (with cargo caching), runs
  your spec on every board/firmware change, and publishes the JUnit results to
  the Checks tab. See `integrations/github-action/README.md`.
- **KiCad (pcbnew)**: install the pcbnew action plugin from
  `integrations/kicad-plugin` to run a spec on the open board and see the
  verdict in a dialog. eeschema has no plugin API yet (see above). Use the
  pre-commit hook or CLI for schematic-stage checks.
- **Any CI**: call `hauksbee-ci run spec.toml --junit results.xml` and consume
  the exit code and the JUnit file.

## Exit codes (the pipeline contract)

Two commands gate, with two deliberately different contracts:

`hauksbee-ci run <spec.toml>` is the gate for specs (assertions):

| exit | meaning |
|---|---|
| 0 | every assertion held (GREEN) |
| 1 | at least one assertion failed (RED) |
| 2 | spec / board error (desynced spec, missing board, bad TOML) |
| 3 | invalid for analysis: the analog solve aborted, so the result is not trustworthy and the run refuses to pretend |

`hauksbee run <board>` static reports (`--lint`, `--drc`, `--si`, `--usb-c`,
`--check`, bare `--json`) are **report commands: they exit 0 even when they
print a serious finding**, unless you pass `--strict` (alias
`--fail-on-findings`). Gating a pipeline on `hauksbee run` without `--strict`
gates on nothing. When a gate-grade finding is printed without `--strict`,
the CLI says so on stderr.

| exit | meaning |
|---|---|
| 0 | clean, or a report-only run without `--strict` |
| 2 | gate-grade findings, under `--strict` (or `--strict-boot` for the boot-safety advisory; co-sim stress faults also gate under `--strict`) |
| 3 | invalid for analysis (aborted analog solve, zero-activity co-sim under `--strict`, thermal table with no usable coverage) |

What `--strict` gates on, per report: `--drc` true copper shorts (clearance
notes never gate), `--lint` high/medium findings, `--si` any real finding,
`--usb-c` a serious CC verdict, and `--check` / bare `--json` the union of
these. On a board format newer than the validated range (KiCad 10+),
possibly-phantom shorts do not gate. The printed caveat says to cross-check.

## Limitations

- The MCU co-sim runs on three backends, each co-simming its own cores (no
  silent fall-back to AVR): **AVR** (ATmega/ATtiny via simavr), **Renode**
  (STM32, nRF52, SiFive RISC-V), and **QEMU** (ESP32/-S3/-C3). Renode and
  QEMU are external emulators located at run time. If the emulator is not
  installed, instantiation fails with a clear install message rather than
  degrading to a different core. `hauksbee doctor --backends` reports which
  backends this build can actually locate. GPIO edges are sampled once per
  co-sim chunk, so signals faster than the chunk alias (see
  `docs/cosim/MCU.md`).
- `max_current` peak tracking covers resistors and diodes directly. The
  `no_faults` stress monitor covers other component kinds instead of a
  numeric ceiling.
- Fuzzing perturbs the named nets' initial logic levels (the undefined
  power-up bits). It does not yet randomize internal MCU RAM.
- Tolerance-ensemble members run serially. Each member is an independent
  bind+solve, so a parallel runner is a natural follow-up, but the co-sim
  backend's thread-safety is unproven and a racy runner would be worse than a
  slow one.
- Tolerance sampling lives in the CI spec layer. Deck-level `{mc(nominal,
  tol)}` / `{gauss(nominal, tol, sigma)}` parameter functions for
  `hauksbee sim` SPICE decks are a planned follow-up, not yet implemented.
- Schematic-stage CI loads the hierarchy root and recurses its sub-sheets.
  The "you pointed at a sub-sheet" guard is best-effort: it detects a
  sub-sheet referenced from the same or parent directory (the layouts real
  projects use). A hierarchy nested more than one directory deep can slip
  past the guard. The fix is the same either way: point the spec at the root
  `.kicad_sch`.
- Bus membership in schematics is not yet modelled, so a design that carries
  nets over buses extracts those nets split into their members (it never
  over-connects). Cross-validated corpus projects without buses match their
  layout net-for-net. See `docs/ingest/SCHEMATICS.md`.
- The flagship brownout fixture is a trimmed cell of the full Tarski board,
  so the regression runs in milliseconds. The full 3,442-component board
  extracts in ~0.3 s but is heavier to co-simulate. Trim to the subcircuit a
  given check cares about, the way the fixture does.
