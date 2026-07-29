# Add a hardware trace: from scope capture to CI gate

You have a physical board, an oscilloscope (or logic analyzer), and a
simulation of the same board. This walkthrough turns one capture session into
a permanent regression gate: a checked-in trace, and every future run of the
simulator compares against it, feature by feature. It needs no Rust: a
directory, two TOML files, and the instrument's own export.

This is oracle tier **T6**: the one tier whose
oracle is reality itself, and therefore the one tier where the sim and the
oracle can *honestly disagree*. Everything about the format follows from
taking that seriously.

## Why features, not waveforms

A real capture carries component tolerances (that 100 kΩ is ±5%), probe
loading, supply drift, and the MCU's RC-oscillator jitter. A *correct*
simulation does not match it sample for sample, and a simulation that did
would be suspicious, not validated. So the comparison never diffs waveforms
pointwise.

It extracts the quantities an EE reads off a scope's measure menu: settled
level, min/max, period, duty, pulse width, and edge count. It pulls these
from **both** waveforms and compares each within the tolerance the trace
itself declares. The capture is the oracle. The tolerances are its error
bars, and you state them.

## Step 1: capture, and write down everything

Probe the net you care about, trigger, and export what the instrument gives
you:

- **Scope → CSV.** Any export whose rows are `time, volts` works. The
  loader skips vendor preamble lines automatically.
- **Logic analyzer → VCD.** Standard `$timescale` / `$var` / `#time` VCD.
  A logic analyzer records *bits, not volts*, so a VCD channel allows only
  timing features. The loader refuses a `max` feature on one by name.

While the probe is still on the board, write down the instrument and probe
(10x? loading matters), the supply and its measured voltage, the firmware
build, and the temperature if unusual. This goes in the trace file next. A
capture that skips this step becomes impossible to interpret a year later.
This is exactly what happened to the Tarski bring-up session
(`testdata/hwtraces/README.md` records what little survived).

## Step 2: the trace directory

Traces live at `testdata/hwtraces/<board>/<scenario>/` (or anywhere in your
own repo: the CI spec points at the file). Copy the seed as a template:

```
testdata/hwtraces/avr-blinky/led-blink-scope/
├── d13.csv        # the instrument export, unmodified
├── trace.toml     # provenance + channels + feature assertions
└── spec.toml      # the CI spec that replays the scenario in sim
```

`trace.toml` (full model: `crates/hauksbee-ci/src/hwtrace.rs`):

```toml
[trace]
board = "crates/hauksbee-ci/examples/boards/blinky.kicad_pcb"
scenario = "demo firmware blinking D13 every 100 ms, USB 5v0.5a supply"
provenance = "real"            # or "synthetic"; MANDATORY, see the trap below
instrument = "Rigol DS1054Z, 10x passive probe"
date = "2026-07-08"

[[channel]]
net = "D13"                    # the board net the probe was on
file = "d13.csv"               # .csv (scope) or .vcd (logic analyzer)
probe = "U1 pin 19"

[[channel.feature]]
kind = "period"                # level|min|max|period|duty|pulse_width|edge_count
reltol = 0.10                  # ±10%: the MCU's RC clock tolerance, not a guess
```

Every feature needs an `abstol`, a `reltol`, or both (the loader refuses one
without: "hardware traces carry their own error bars and must state them").
Derive them from physics, not from what makes the test pass: clock tolerance
for timing features, datasheet V_OH spread plus probe loading for levels.
Optional per-feature fields: `after_ms` (skip the boot transient) and
`threshold` (edge-detection level in volts: default is 50% of each
waveform's own swing, the scope convention).

## Step 3: the spec that replays the scenario

The comparison only means something if the sim runs the *same experiment*:
same board, same firmware, same supply the capture session used. That is a
normal `hauksbee-ci` spec with one new assertion kind:

```toml
name = "avr-blinky vs hardware trace"
board = "../../../../crates/hauksbee-ci/examples/boards/blinky.kicad_pcb"
firmware = "../../../firmware/demo/demo.hex"
duration_ms = 1000             # match the capture window; edge_count needs it

[[supply]]
net = "+5V"
kind = "usb"
usb = "5v0.5a"

[[assert]]
kind = "hwtrace"
trace = "trace.toml"
```

Because `hwtrace` is an ordinary assertion, everything the CI runner does
composes with it: tolerance ensembles, fuzz seeds (the features must hold on
every seed), JUnit output, and exit codes. It runs in a hardware repo's CI
like any other gate, not just in this repo's test suite. One `hwtrace`
assertion expands to one report line per channel and feature, each showing
the simulated value, the captured value, the delta, and the band that judged
them.

Two timing details matter. Set `duration_ms` to the capture's window: edge
counts over different windows are not comparable, and the harness refuses
the comparison rather than scaling it. Set `frame_ms` well below your
fastest feature: the frame cadence sets the sim waveform's sample rate, and
the default 1 ms resolves a 200 ms blink fine but not a 104 µs UART bit.

## Step 4: run it

```
hauksbee-ci run testdata/hwtraces/avr-blinky/led-blink-scope/spec.toml
```

```
  [PASS] hwtrace D13 period
        D13 period: sim 201.0000 ms vs captured 200.1500 ms (Δ +0.8500 ms within ±20.0150 ms)
  [PASS] hwtrace D13 max
        D13 max: sim 4.5947 V vs captured 4.7443 V (Δ -0.1496 V within ±0.3000 V)
```

When a feature fails, the line carries both values. That is the point.
A real disagreement between sim and hardware is a *finding*, not a nuisance.
Document the delta and its suspected cause in the trace's `notes`, instead
of widening the tolerance until it disappears. (The recalled Tarski bring-up
delta, a measured V_out peak of 2.1 to 2.3 V against the sim's ~1.5 V, is
the canonical example this tier exists to settle.)

## The trap: provenance

`provenance` is mandatory and takes exactly two values. `"real"` means the
bytes came off an instrument probing a physical board. `"synthetic"` means
someone built them from datasheet-typical behavior, another simulator, or
by hand. A synthetic trace is legitimate scaffolding (the seed traces are
synthetic, and say so), but it validates the **harness**, not the simulator.

The report appends `[SYNTHETIC trace, validates the harness, not the
hardware]` to every one of its lines, so a green run can never quietly
impersonate hardware validation. A trace file without the field does not
load. If you regenerate a synthetic capture, keep its generator script
beside it (see `testdata/hwtraces/avr-blinky/gen_synthetic.py`), so the
construction stays inspectable.

## The test that proves it

The corpus harness walks every `testdata/hwtraces/*/*/spec.toml`, runs it,
and prints the per-feature table:

```
cargo test -p hauksbee-ci --test hwtrace -- --nocapture
```

The same file also proves the failure path: a deliberate mismatch between a
300 ms and a 200 ms period must fail, naming the feature and both values. It
also proves the honesty rules: no provenance means refused, a voltage
feature on a VCD means refused, and a feature without tolerance means
refused. If you add a scenario directory with a `spec.toml`, the corpus test
picks it up with no code change.
