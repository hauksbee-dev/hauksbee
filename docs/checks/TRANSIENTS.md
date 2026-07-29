# Transient scenarios: dynamic loads, decoupling, and brownout

**DC analysis cannot see a brownout.** A rail that sits at 3.30 V at the
operating point can still collapse the instant a radio keys up, a motor stalls,
or a load steps faster than the decoupling can follow. Those failures are
dynamic: inrush tripping a battery protection, rail sag under a servo burst,
brownout from a load step into inadequate decoupling. The transient scenario
layer makes them visible, asserts on them, and fails the build when a board
cannot ride them out.

The pieces:

1. **Load profiles**: a declarative, datasheet-cited current model per part
   (ESP32 boot/WiFi-TX/deep-sleep, a generic MCU, a servo/BLDC class), stamped
   as a time-varying current sink on the part's supply pin.
2. **Capacitor ESR/ESL**: opt-in series parasitics that turn decoupling from
   ideal to honest, so the rail sags the way the real board's does.
3. **Battery protection**: a BMS over-current cutoff on the battery supply, so
   the inrush-vs-protection interaction is modelled.
4. **The scenario runner**: a `[[scenario]]` spec section that attaches a
   profile to a part and judges the rail over the transient window
   (min/max/dip-duration/recovery, protection-trip yes/no).

Everything is cross-checked against hand math: the analytic decoupling-sag
tests below match the solver to better than 1%, and the same Isource machinery
is verified to give identical results in both the monolithic and partitioned
solver paths.

---

## 1. Load profiles

A load profile is a named, time-varying current draw a part presents on its
supply pin. It is consumed by the transient layer as a current sink (an
`Isource`) stamped on the part's supply node, so the rail sees the same dI/dt a
real chip imposes. Profiles live in `crates/hauksbee-models/db/load_profiles.toml`
(embedded at compile time, cited inline to datasheets) and are parsed by
`hauksbee_models::profile`.

### Schema

```toml
[[models]]
id = "esp32_boot_wifi"            # stable key, referenced from a scenario
description = "..."               # human text for reports
[models.match]                    # optional auto-binding rule
value_re = "(?i)esp32"

[[models.segment]]                # ordered piecewise / periodic segments
level_a    = 0.040               # steady current this segment holds (A)
rise_s     = 0.001               # linear ramp from the previous level (s)
duration_s = 0.0                 # time held after the ramp (s); <=0 on the
                                 #   LAST segment = "hold to end of window"

[[models.segment]]
level_a    = 0.240               # WiFi TX burst level
rise_s     = 0.0005
duration_s = 0.010
period_s   = 0.100               # >0 => repeats with this period (a burst train)
idle_a     = 0.040               # between-bursts level (default: baseline)
jitter_s   = 0.0                 # optional deterministic period jitter (s),
                                 #   seeded from (scenario seed, segment index)
```

A segment ramps over `rise_s` from the previous level to `level_a`, then holds
for `duration_s`. A segment with `period_s > 0` is a **burst train**: it fires
for `rise_s + duration_s`, idles at `idle_a` for the rest of the period, and
repeats. Jitter is deterministic (splitmix64 over `(seed, index)`), so a run is
reproducible while still being able to spread bursts out.

The evaluator is `LoadProfile::current_at(t, seed)`. A scenario can also define
an inline `[[profile]]` in its spec, bypassing the database.

### Hand-authored profiles (cited)

| profile id              | what it models                          | key current numbers (source)                                                                 |
|-------------------------|-----------------------------------------|----------------------------------------------------------------------------------------------|
| `esp32_boot_wifi`       | ESP32 boot + WiFi association bursts     | 40 mA active baseline; 240 mA TX bursts, 10 ms wide, 100 ms cadence (ESP32 datasheet RF-TX avg 240 mA) |
| `esp32_wifi_tx_peak`    | ESP32 worst-case TX inrush               | 500 mA peak bursts (ESP32-WROOM-32 module note: ~500 mA packet-burst peak)                    |
| `esp32_cold_boot_inrush`| ESP32 cold-boot RF surge                 | ~1.2 A leading inrush (6 ms) settling to 240 mA bursts (RF-cal + bulk-cap inrush; Inkplate #7/#10 + Espressif brownout guidance) |
| `esp32_deep_sleep`      | ESP32 deep sleep                         | ~10 uA RTC-only (ESP32 datasheet deep-sleep)                                                  |
| `mcu_generic`           | generic Cortex-M MCU active + sleep tick | 20 mA active, 50 uA sleep dips (STM32F405 IDD class: run ~38 mA @168 MHz, stop ~tens of uA)   |
| `mcu_active`            | generic MCU steady active                | 20 mA                                                                                         |
| `servo_sg90`            | hobby servo stall/run                    | 10 mA idle, ~700 mA stall inrush, ~200 mA run (TowerPro SG90: stall ~650 mA @ 4.8 V)          |
| `bldc_phase_burst`      | small BLDC phase start/run               | 1.5 A start inrush, 0.6 A run with commutation-rate pulses                                    |

The `rise_s` ramp is the cheap stand-in for an L/R current-rise phase character
on the motor classes (a finite current slope rather than a step edge).

### How it drives the solver (both paths)

The load is a `DynamicLoad` peripheral
(`crates/hauksbee-engine/src/peripherals/load.rs`) that owns an `Isource`
(`p = supply node`, `n = GROUND`, so it pulls current out of the rail) and sets
its value each chunk from `profile.current_at(t)`. This rides the existing
`Isource` machinery:

- **Monolithic path**: `stamp.rs` evaluates the source at `ctx.time` and stamps
  it into the RHS.
- **Partitioned path**: the linear-island builder routes each `Isource` as a
  **current-input column** in the island's pre-factored RHS, evaluated at the
  step's end time (`partitioned.rs`).

`decoupling_sag.rs::profile_isource_agrees_in_both_solver_paths` drives the same
PWL-current-step decoupling network through `Partitioning::Off` and
`Partitioning::Auto` and asserts the rails agree to better than 0.1% and both
match the closed form to <1%. So a profile-driven load is carried identically
by either path. No extra machinery was needed.

---

## 2. Capacitor ESR/ESL

An ideal capacitor is a perfect short to dI/dt. A real one is not. Its **ESR**
sets the floor on how far a rail sags during a fast load step (the step current
flows through ESR as an instantaneous IR drop), and its **ESL** sets how fast
the cap can respond at all. Modeling decoupling as ideal makes every rail look
better than it is.

### How it is stamped

Rather than widen the shared `Device::Capacitor` IR, an ESR/ESL capacitor is
stamped as a **series R-L-C network** between the two original pads, with one or
two internal nodes (`crates/hauksbee-engine/src/decoupling.rs`):

![A real capacitor stamped as a series network between its two pads: pad_a through R_esr to internal node n1, through L_esl to internal node n2, then through C to pad_b](../assets/diagrams/capacitor-model.svg)

This is purely additive: the solver already handles R, L and C, and it works
identically in both solver paths (the RLC island is linear). When ESR and ESL
are both zero, the network collapses to the ideal capacitor (zero legs are
skipped), so existing analytic capacitor tests are bit-unchanged.

### Defaults (opt-in, cited)

Defaults are looked up by package / dielectric class, inferred from each cap's
footprint string and value. They are **off by default**: the binder stamps ideal
capacitors unless a scenario requests parasitics (`[decoupling] parasitics =
true`) or names a per-ref override. This keeps global behaviour unchanged.

| class        | ESR     | ESL    | source                                   |
|--------------|---------|--------|------------------------------------------|
| MLCC 0201    | 80 mΩ   | 0.3 nH | Murata GRM033 datasheets / SimSurfing    |
| MLCC 0402    | 50 mΩ   | 0.4 nH | Murata GRM155 datasheets / SimSurfing    |
| MLCC 0603    | 30 mΩ   | 0.6 nH | Murata GRM188 datasheets / SimSurfing    |
| MLCC 0805    | 20 mΩ   | 0.8 nH | Murata GRM21 datasheets / SimSurfing     |
| MLCC 1206    | 15 mΩ   | 1.2 nH | Murata GRM31 datasheets / SimSurfing     |
| Electrolytic | 1.0 Ω   | 5 nH   | Nichicon / Panasonic alum-poly datasheets|
| Tantalum     | 0.5 Ω   | 3 nH   | KEMET T49x / AVX TAJ datasheets          |

MLCC ESR is the high-frequency series-resistance minimum. ESL is the mounted
self-inductance (body plus a short pad loop). Electrolytic / tantalum ESR is the
datasheet 100 kHz figure for a mid-value part. These are deliberately
representative class defaults, not a per-MPN table. A per-part override
(`esr_ohms` / `esl_henries`) wins when a datasheet number is known.

The footprint inference (`EsrEsl::from_footprint`, split into the unit-testable
`class_from_footprint`) buckets `CP_*` / large-can / radial / axial footprints as
electrolytic, explicit tantalum markers (TANTALUM/TANT, EIA case codes,
`CASE-A..D`) as tantalum, and MLCC footprints by their size code in *either*
imperial (`0402`) or the equivalent metric form (`1005Metric`), with a dedicated
0201 class (highest ESR of the ladder). It falls back to 0603 MLCC for an
unrecognized name. Parasitics remain **opt-in**, so broadening the inference does
not change any default solver result.

---

## 3. Battery protection (BMS over-current cutoff)

Real protected LiPo cells (and the DW01 / S-8261-class protection ICs on bare
cells) trip a MOSFET open when the pack current stays above a threshold for a
short delay, and auto-recover when the load is removed. That is exactly the
dynamic the Inkplate cold-boot inrush hits: the pack is fine at steady state but
the TX surge trips protection.

`PowerSupply::Battery` carries an optional `BatteryProtection`
(`crates/hauksbee-engine/src/power_supply.rs`):

```
protection_trip_a   = 1.0     # over-current trip threshold (A)
protection_delay_ms = 2.0     # sustained time above trip before latching (ms)
protection_reset_a  = <trip>  # current to fall below to re-arm (A)
```

The state machine integrates time above `trip_a`. Once it exceeds `delay_s`
the cutoff latches and the battery commands ~0 V (the rail collapses behind
the cell's internal resistance). It re-arms after the load stays below `reset_a` for
`delay_s`. A brief spike shorter than the delay does **not** trip. Verified by
`power_supply.rs::protection_trips_on_sustained_overcurrent_and_holds_under_brief_spike`.

---

## 4. The scenario runner

A `[[scenario]]` block attaches a load profile to a part and (optionally) turns
the board's decoupling honest. Scenario-aware assertions then judge the rail.

```toml
board = "hardware/board.kicad_sch"
duration_ms = 60
frame_ms = 0.2

[[supply]]
net = "+3.3V"
kind = "battery"
chemistry = "liion"
cells = 1
capacity_mah = 400
r_internal_ohms = 0.25
protection_trip_a = 1.0          # BMS cutoff
protection_delay_ms = 2.0

[decoupling]
parasitics = true                # opt-in ESR/ESL on every cap
[[decoupling.override]]          # per-cap datasheet values
ref = "C2"
esr_ohms = 0.012

[[scenario]]
id = "coldboot"
part = "U1"                      # the part the load attaches to
profile = "esp32_cold_boot_inrush"
supply_net = "+3.3V"            # explicit, or inferred from U1's power pins
start_ms = 2.0                  # when the profile's activity begins
seed = 0                        # deterministic jitter

[[assert]]
kind = "protection_trip"
supply_net = "+3.3V"
expect_trip = true              # the trip is required (or forbidden)

[[assert]]
kind = "rail_window"
scenario = "coldboot"           # scope to the scenario's window
net = "+3.3V"
min = 3.0                       # rail floor over the window
dip_below = 3.0                 # "dipped" while below this
for_max_ms = 1.0                # must not stay dipped longer than this
recover_to = 3.2                # ... and must climb back to here
recover_within_ms = 200         # ... within this long of first dipping
```

If `supply_net` is omitted, the runner infers it from the part's power pins
(VDD / VCC / VBAT / 3V3 / 5V class). Assertions integrate with the existing
`AssertResult` path, so they get JUnit XML, the human report, the process exit
code, and GitHub annotations for free.

The `rail_window` assertion measures over the scenario's window
(`start_ms` to end of run): minimum / maximum voltage, total dip time below a
threshold (rectangular integration on the frame grid), and recovery time (first
dip below `dip_below` to last sample below `recover_to`). `protection_trip`
reports whether any battery supply's protection latched during the run.

---

## Validation

### (a) Analytic decoupling sag: solver vs hand math, <1%

`crates/hauksbee-solve/tests/decoupling_sag.rs`. Every test computes the exact
closed form in the test and checks the solver against it.

- **ESR step + linear discharge**: a 10 uF cap (30 mΩ ESR) pre-charged to 3.3 V,
  hit by a 500 mA current step. Hand math:
  `v_node(t) = V0 - I*R_esr - (I/C)*t`. The instantaneous ESR sag is
  `I*R_esr = 15 mV`. The discharge slope is `I/C = 50,000 V/s`. The solver
  matches the full waveform to **< 1%**, the ESR sag to within 10%, and the
  slope to within 1%.
- **First-order load-step droop**: an ideal 5 V rail behind 200 mΩ feeding a
  100 uF bulk cap, hit by a 300 mA step. Hand math:
  `v(t) = V0 - I*R_s*(1 - exp(-t/R_s*C))`, settling to a `I*R_s = 60 mV` sag with
  `tau = R_s*C`. Solver matches to **< 1%**, and the steady sag to within 1% of
  60 mV.
- **Both solver paths agree**: the same PWL-current-step network through
  `Partitioning::Off` and `Partitioning::Auto` matches the closed form to <1% and
  each other to **< 0.1%**, proving the current-input-column machinery carries a
  profile-driven load correctly.

### (b) The Inkplate-class two-sided demo

`crates/hauksbee-ci/tests/inkplate_class_demo.rs`, board
`testdata/inkplate_class.net`.

**This is a representative reconstruction, not the real Inkplate board.** No
native Inkplate design files exist in the corpus. The netlist is a hand-built
minimal board of the same topology class (ESP32-WROOM + a small 3V3 LDO +
bulk/decoupling caps on a LiPo). The pattern source is the documented Inkplate 6
WiFi cold-boot brownout (issues #7 / #10): the board resets / browns out when
WiFi comes up on a cold boot under battery power.

Honest scope: the board's 3V3 LDO is present in the netlist but its closed-loop
regulation is a behavioral converter model owned by a sibling layer, not
stamped here, so the supply leg drives the rail directly and the rail sits at the
source voltage (cell voltage on battery, ~5 V minus droop on USB) rather than a
regulated 3.3 V. That does not change the demonstrated physics: the headline is
the battery-side protection tripping on the inrush while the stiff USB source
rides it out.

Two sides, same firmware activity (`esp32_cold_boot_inrush`, ~1.2 A surge),
ESR/ESL decoupling on both:

| side             | supply                       | protection | rail behaviour                                   | result   |
|------------------|------------------------------|------------|--------------------------------------------------|----------|
| **battery**      | 1S LiPo, 0.25 Ω, 400 mAh     | 1 A / 2 ms | **TRIPS** at ~4 ms; rail collapses below 3 V for 6.4 ms (dips to ~−0.3 V on the ESL kick) | brownout caught |
| **USB-supplemented** | USB 5V / 3A (stiff)      | none       | rail holds at **min 4.900 V**, no faults          | survives |

The contrast is the whole point: the same activity is fatal on a small cell and
fine on USB, which is why the Inkplate failures were intermittent and
supply-dependent. The trip is the protection state machine firing on the real
solved rail current. The survival is the measured rail staying up.

### (c) Corpus calibration: Olimex ESP32-EVB, must PASS

`crates/hauksbee-ci/tests/olimex_burst_calibration.rs`, board
`board-corpus/famous/olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch`
(corpus-gated).

A standard ESP32 WiFi-TX burst (240 mA over 40 mA baseline) on the **real**
Olimex EVB with its robust wall supply holds **+3.3V at min 3.266 V** (a 40 mV
sag from 3.306 V), no brownout, no faults: **GREEN**. This is the calibration
side. A robust board ridden by a normal burst must not go red, or the load /
supply models would be too pessimistic and the Inkplate-class red could not be
trusted. It passing is what makes the brownout meaningful.

---

## Scriptable waveforms: `--probe`

A transient run's shape is often what you want to inspect, not just a pass/fail.
`--probe` records named nets' node voltages every co-sim chunk of a `--headless`
run and writes them to a CSV, so you can plot or post-process the waveform with
no UI:

```bash
hauksbee run board.kicad_pcb --firmware fw.hex --headless --seconds 2 \
  --probe +5V,GATE --probe D13 --probe-csv out.csv
```

- `--probe` takes a comma-separated net list and is repeatable. An unknown net
  is a loud error (with near-matches) before the run starts, so a typo never
  yields a silently-empty column.
- `--probe-csv` sets the output path. The header is `time_s` followed by one
  column per probed net, one row per chunk, feed it straight to a plotting
  script, a notebook, or a diff against a golden capture.

This is the headless, scriptable counterpart to watching a net in the TUI, and
pairs with the scenario runner above: probe the rail while a `[[scenario]]` load
step runs to capture the exact sag profile behind a `rail_window` verdict.

---

## Stretch: tolerance corner sweep (design sketch)

Not yet implemented. The sketch:

- **Per-component tolerance metadata**: add an optional `tolerance_pct` to the
  model DB / spec override for resistors and caps (e.g. `±1%` R, `±20%` X7R C),
  and an ESR tolerance band on the parasitics.
- **A corner / Monte-Carlo runner**: re-run a scenario across deterministic
  seeds, each seed perturbing every toleranced component within its band
  (splitmix64 from `(corner_seed, ref)`, the same deterministic-PRNG pattern the
  fuzz layer already uses). Worst-case corners (all decoupling at min C / max
  ESR, supply at min V) get explicit corners. The rest are sampled.
- **Reporting**: re-evaluate the scenario's `rail_window` / `protection_trip`
  assertions per corner and report the worst-case margin (the corner with the
  deepest sag, the longest dip, the soonest trip), so the build fails on the
  worst corner rather than the nominal one.

The hooks are already in place: the runner is seed-parameterized
(`run_spec` loops seeds and an assertion must hold across all of them), and the
deterministic-jitter pattern is established, so a corner runner is a re-run loop
over perturbed component values plus a margin-aggregating reporter.
