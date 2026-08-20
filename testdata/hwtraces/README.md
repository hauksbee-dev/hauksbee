# Hardware traces (oracle tier T6)

Captured waveforms from physical boards, compared feature-by-feature against
the simulated run of the same board + firmware + scenario. This is the one
oracle tier where sim and reality can honestly disagree — component
tolerances, probe loading, supply drift, timing jitter — so the comparison is
**feature-based** (period, duty, levels, edge counts), never pointwise, and
every feature carries the trace's own stated error bars.

Layout: `<board>/<scenario>/` containing

- `trace.toml` — instrument, probe points, conditions, **provenance**, and
  the per-channel feature assertions (format: `crates/hauksbee-ci/src/hwtrace.rs`)
- the capture beside it — `.csv` (scope export) or `.vcd` (logic analyzer)
- `spec.toml` — the hauksbee-ci spec that replays the scenario in simulation,
  with an `[[assert]] kind = "hwtrace"` pointing at the trace

Run one scenario: `hauksbee-ci run <scenario>/spec.toml`.
Run the whole corpus: `cargo test -p hauksbee-ci --test hwtrace -- --nocapture`.

## The provenance rule

Every `trace.toml` MUST declare `provenance = "real"` or `"synthetic"`.
A synthetic trace (constructed from datasheet-typical behavior or another
simulator) proves the *harness* works; it validates nothing about the
simulator, and the report banners it as such. Passing constructed data off
as a hardware capture is exactly the fake this repo refuses.

## Current inventory

| Trace | Provenance | What it proves |
|---|---|---|
| `avr-blinky/led-blink-scope` | **synthetic** | the CSV pipeline end-to-end |
| `avr-blinky/led-blink-la` | **synthetic** | the VCD pipeline end-to-end |

**There is no real hardware capture in this repository yet** (verified by
sweep, 2026-07-08). The tier's validation value begins when one is added —
see `docs/extending/add-a-hardware-trace.md` for the workflow.

## First real-capture targets: the Tarski bring-up

The physical Tarski board was brought up with an oscilloscope alongside the
sim; that knowledge survives only as recalled numbers, with no capture files.
The recalled observations, recorded here so they are not lost again:

- hidden-neuron **V_out peak ≈ 2.1–2.3 V** — vs the faithful sim's ~1.5 V-pinned
  peak: the load-bearing discrepancy this tier exists to adjudicate honestly
  (document the delta and its suspected cause; never tune it away)
- **comparator output swing = full 5 V** (rail-to-rail)
- **supply sag** under activity (qualitative; no numbers survive)
- spike **pulse width** (mentioned, value not recorded)
- the **missing-cap neuron variant**: shorter pulse, larger swing

Re-capturing these as real CSV traces (`max`, `pulse_width`, `min` features
on the V_out and comparator nets) is the highest-value hardware session this
tier can receive.
