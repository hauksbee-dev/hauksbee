# Transient scenarios: dynamic loads, decoupling, brownout

DC analysis cannot see a brownout. The transient layer stamps a time-varying
current load on a part's supply pin, optionally makes decoupling caps honest
with ESR/ESL, models battery protection, and judges the rail over the
scenario window from a `hauksbee-ci` spec.

## Load profiles

A profile is a named, piecewise/periodic current draw, consumed as an
`Isource` from the part's supply node to ground. Built-ins live in
`crates/hauksbee-models/db/load_profiles.toml`; a spec can define its own
inline as `[[profile]]`.

```toml
[[profile]]                       # spec-local; the db uses [[models]] with the same segment schema
id = "my_burst"
description = "..."

[[profile.segment]]
level_a    = 0.040               # steady current after the ramp (A)
rise_s     = 0.001               # linear ramp from the previous level (s)
duration_s = 0.0                 # hold time; <= 0 on the LAST segment = hold to end

[[profile.segment]]
level_a    = 0.240
rise_s     = 0.0005
duration_s = 0.010
period_s   = 0.100               # > 0: repeat with this period (burst train)
idle_a     = 0.040               # level between bursts (default: previous level)
jitter_s   = 0.0                 # deterministic period jitter, seeded per (scenario seed, segment)
```

Built-in profiles: `esp32_boot_wifi` (40 mA + 240 mA 10 ms bursts at 100 ms),
`esp32_wifi_tx_peak` (500 mA bursts), `esp32_cold_boot_inrush` (~1.2 A for
6 ms then bursts), `esp32_deep_sleep` (~10 uA), `mcu_generic` (20 mA with
50 uA sleep dips), `mcu_active` (20 mA), `servo_sg90` (10 mA idle, ~700 mA
stall, ~200 mA run), `bldc_phase_burst` (1.5 A start, 0.6 A run). A model card
can also own a load (`[[models.behavioral.profiled_loads]]`,
[MODELS.md](../models/MODELS.md)).

## Capacitor ESR/ESL

Off by default. `[decoupling] parasitics = true` stamps each capacitor as a
series R-L-C between its pads (zero legs are skipped). Defaults by class,
inferred from footprint and value: MLCC 0201 80 mOhm / 0.3 nH, 0402 50 / 0.4,
0603 30 / 0.6, 0805 20 / 0.8, 1206 15 / 1.2; electrolytic 1.0 ohm / 5 nH;
tantalum 0.5 ohm / 3 nH; unrecognised names fall back to 0603 MLCC. Metric
size codes and `CP_*`/radial/axial/tantalum markers are recognised.

```toml
[decoupling]
parasitics = true
[[decoupling.override]]
ref = "C2"
esr_ohms = 0.012
esl_henries = 0.5e-9
```

## Battery protection

A `battery` supply can carry a BMS over-current cutoff:

```toml
[[supply]]
net = "+3.3V"
kind = "battery"
chemistry = "liion"
cells = 1
capacity_mah = 400
r_internal_ohms = 0.25
protection_trip_a   = 1.0    # trip threshold (A)
protection_delay_ms = 2.0    # sustained time above trip before latching
protection_reset_a  = 1.0    # optional; default = trip
```

Once current stays above `protection_trip_a` for the delay, the cell commands
~0 V; it re-arms after the load stays below `protection_reset_a` for the
delay. A spike shorter than the delay does not trip.

## Scenarios and assertions

```toml
[[scenario]]
id = "coldboot"
part = "U1"                      # required: the part the load attaches to
profile = "esp32_cold_boot_inrush"
supply_net = "+3.3V"            # optional; inferred from the part's power pins
start_ms = 2.0
seed = 0

[[assert]]
kind = "protection_trip"
supply_net = "+3.3V"
expect_trip = true              # required (or forbidden)
scenario = "coldboot"           # optional

[[assert]]
kind = "rail_window"
scenario = "coldboot"
net = "+3.3V"
min = 3.0                       # rail floor over the window (and/or max)
dip_below = 3.0                 # "dipped" while below this
for_max_ms = 1.0                # must not stay dipped longer
recover_to = 3.2                # and must climb back to here
recover_within_ms = 200         # within this long of first dipping
```

`rail_window` measures min/max over the scenario window (from `start_ms` to
end of run), total dip time below `dip_below` (frame-grid integration) and
recovery time (first dip to last sample below `recover_to`).
`protection_trip` reports whether any battery supply's protection latched.

## Probing waveforms

```bash
hauksbee run board.kicad_pcb --firmware fw.hex --headless --seconds 2 \
  --probe +5V,GATE --probe D13 --probe-csv out.csv
```

`--probe` takes comma-separated nets and is repeatable; an unknown net is a
loud error with near-matches. `--probe-csv` writes `time_s` then one column
per net, one row per co-sim chunk. `--chunk-us N` sets the chunk width.

## Tolerance ensembles

`[[tolerance]]` and `[ensemble]` replay every assertion across component
values: `monte-carlo` (default, `seeds` members, seed 0 nominal) or `corners`
(all `2^n` min/max combinations, capped at 10 toleranced components, plus a
small Latin-hypercube interior sample whose failure fails the assertion).
Full reference: [CI.md](../ci/CI.md#component-tolerances). Worked example:
`crates/hauksbee-ci/examples/tolerance_divider_corners.toml`.

## Limits

- The solver matches closed-form decoupling sag to < 1 %
  (`crates/hauksbee-solve/tests/decoupling_sag.rs`), identically in the
  monolithic and partitioned paths.
- `[[tolerance]]` spreads R/C/L values only; ESR/ESL stay at the class default,
  so the min-C / max-ESR worst corner is not walked.
- `testdata/inkplate_class.net` with `crates/hauksbee-ci/tests/inkplate_class_demo.rs`
  is a representative reconstruction of the Inkplate cold-boot brownout, not
  the real board: the battery side trips and the stiff-supply side survives
  the same inrush.
