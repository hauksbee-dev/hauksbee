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
source voltage rather than a regulated 3.3 V. On the battery side that is the
cell voltage. The USB-fed side is therefore modelled as the LDO's stiff
*output*, a 3.3 V bench supply with a 3 A limit: the fixture has no VBUS net to
regulate down, and stamping the USB 5 V straight onto the 3V3 net puts 5 V on the
ESP32's VDD, which the stress monitor correctly flags as an overvoltage against
the module's 3.6 V absolute maximum. That does not change the demonstrated
physics: the headline is the battery-side protection tripping on the inrush while
the stiff supply rides it out.

Two sides, same firmware activity (`esp32_cold_boot_inrush`, ~1.2 A surge),
ESR/ESL decoupling on both:

| side             | supply                       | protection | rail behaviour                                   | result   |
|------------------|------------------------------|------------|--------------------------------------------------|----------|
| **battery**      | 1S LiPo, 0.25 Ω, 400 mAh     | 1 A / 2 ms | **TRIPS** at ~4 ms; rail collapses below 3 V for 6.40 ms (dips to -0.300 V on the ESL kick) | brownout caught |
| **stiff supply** | 3.3 V bench, 3 A limit       | none       | rail holds at **min 3.299 V** (max 3.300 V), no faults | survives |

The contrast is the whole point: the same activity is fatal on a small cell and
harmless behind a stiff supply, which is why the Inkplate failures were
intermittent and supply-dependent. The trip is the protection state machine
firing on the real solved rail current. The survival is the measured rail staying
up.

### (c) Corpus calibration: Olimex ESP32-EVB, must PASS

`crates/hauksbee-ci/tests/olimex_burst_calibration.rs`, board
`board-corpus/olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch`
(corpus-gated).

A standard ESP32 WiFi-TX burst (240 mA over 40 mA baseline) on the **real**
Olimex EVB with its robust wall supply holds **+3.3V at min 3.300 V**: the 0.1 Ω
wall source and the board's own bulk plus decoupling swallow the burst outright,
so the rail never leaves its nominal and never dips below the 3.1 V threshold at
all.

```
$ hauksbee-ci run olimex_burst.toml
hauksbee-ci: Olimex ESP32-EVB: WiFi burst on robust supply (calibration)
  board: board-corpus/olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_sch
  seeds: 1

  UNPOWERED RAIL: VDD1A-2A
        These nets name a supply but not a voltage, so nothing powered
        them and they sat at 0 V. Every analog result below was solved
        around that, so a fault it names may be an artifact rather than
        a finding about your board. Add a [[supply]] for each, then run
        again before acting on anything here.

  [PASS] 3V3 holds through WiFi burst on a robust supply
        +3.3V window: min=3.300V (>= 3V), dip<3.1V for 0.00ms (<= 5ms) [min=3.300V max=3.300V]
  [PASS] no stress faults raised
        no stress faults

2/2 assertions passed in 5.24s - GREEN
```

The EVB's `VDD1A-2A` net carries no `[[supply]]` in this spec, so the runner says
so instead of quietly solving around a 0 V rail. It is the Ethernet PHY's
analog-supply label, a net of its own rather than part of +3.3V, so the burst and
the rail window judging it are untouched.

This is the calibration side. A robust board ridden by a normal burst must not go
red, or the load / supply models would be too pessimistic and the Inkplate-class
red could not be trusted. It passing is what makes the brownout meaningful.

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

## Tolerance corner sweep

A board that meets its assertions only at *nominal* component values is a latent
defect: real boards are built from ±1% resistors and ±20% X7R caps, so some
fraction of assembled units lands outside the window. `crates/hauksbee-ci/src/tolerance.rs`
turns that into a CI property, replaying the whole assertion set across an
*ensemble* of component values and passing only when every member passes.

- **Per-component tolerance metadata**: a `[[tolerance]]` table names components
  by a `ref` glob (`*` is the only wildcard, kept teachable) and a `percent`
  spread, with an optional `distribution`: `uniform` (the default, which stresses
  the tolerance edges hardest) or `gaussian` (sigma = tol/3, truncated by
  rejection at ±tol, the usual EDA reading of a datasheet tolerance as a 3-sigma
  bound). Rules apply in order and the last matching rule wins, so a broad
  `ref = "R*"` can be tightened by a later `ref = "R7"`. An `[[override]]`
  carrying a `tolerance` is applied after all the rules, with the override's
  `value` as the nominal.
- **Two runners, selected by `[ensemble] mode`**: `monte-carlo` (the default)
  runs `seeds` members (default 16), each sampling every toleranced component
  independently. `corners` enumerates all `2^n` all-min/all-max combinations
  deterministically: member k puts component i, in sorted-reference order, at its
  min when bit i of k is 0 and its max when bit i is 1, so member 0 is all-min
  and member `2^n - 1` is all-max. Corner mode refuses above
  `CORNER_CAP = 10` toleranced components (1024 runs) and points at Monte-Carlo,
  because silently truncating the corner set would fake the very bounded claim
  the mode exists to make. In corner mode `seeds` is ignored and `[fuzz]` must be
  absent: the two ensembles do not compose.
- **Determinism**: every sampled value is a pure function of `(seed, reference,
  rule)`, drawn from a splitmix64 stream domain-separated by a `"tol:"` tag, so a
  failing member re-runs byte-identical under `--seed N`, and adding a tolerance
  never changes which fuzz levels a seed straps. Seed 0 is always the nominal
  baseline, so "nominal passes but the ensemble fails" is visible inside one run.
- **Reporting**: every assertion is re-evaluated per member, and the report names
  the worst member with its component values and words the strength of the claim
  honestly. A passing Monte-Carlo ensemble is statistical evidence, not a
  worst-case bound; corner mode's bound holds where the response is *monotonic in
  each value*, and the report says so on every verdict it backs.
- **Interior probes**: corner mode does not take that monotonicity on trust. It
  runs a small stratified Latin-hypercube sample of the interior alongside the
  corners (4 probes for one toleranced component, 6 for two, 8 from three up), and
  an interior point that fails where every corner passed FAILS the assertion and
  says the corners did not bound it. A clean probe set is evidence for the
  monotonicity the bound needs, never proof of it, and the wording says so: each
  member is judged against the assertion's own window, so a probe worse than every
  corner still passes while it stays in band.

The worked example is `crates/hauksbee-ci/examples/tolerance_divider_corners.toml`:
a 10k/10k divider off a 5 V rail with ±10% on both resistors, so four corners.
The divider is monotonic in each resistor, so its true worst case *is* a corner
and VOUT spans exactly [2.25, 2.75] V. The wide window holds on all four; the
tight one fails and names the corner that broke it, with the values that did it:

```
$ hauksbee-ci run crates/hauksbee-ci/examples/tolerance_divider_corners.toml
hauksbee-ci: divider tolerance corners
  board: boards/tolerance_divider.kicad_pcb
  seeds: 10
  tolerance corners: 4 deterministic min/max corner(s) + 6 interior probe(s) over 2 component(s): the corners bound the worst case where the response is monotonic in each value, and the probes sample the interior for a point that breaks an assertion the corners passed

  [PASS] VOUT within the designed [2.2, 2.8] V envelope on every corner
        VOUT: min=2.460V (>= 2.2V), max=2.460V (<= 2.8V) [settled 2.460V] (held on all 4 min/max tolerance corners and on 6 interior Latin-hypercube probe(s): no interior point sampled broke this assertion, which is evidence for the monotonicity the corner bound needs, not proof of it, and a probe inside the window is not compared against the corners' own margin)
  [FAIL] VOUT within the tight [2.4, 2.6] V window on every corner
        corner 1: VOUT: min=2.250V < required 2.4V <- FAILED HERE, max=2.250V (<= 2.6V) [settled 2.250V] [R1=11k(max), R2=9k(min)]; passed 7/10 corners + interior probes (failing: 1, 2, 5)
        why: VOUT settled 0.150 V below your floor (2.250 V vs min 2.4 V)

1/2 assertions passed in 0.03s - RED
next: the "voltage" section of https://docs.hauksbee.dev/docs/ci/ci explains this check and its knobs
```

One piece is still missing: an **ESR tolerance band** on the capacitor parasitics
of section 2. `[[tolerance]]` spreads a component's own value (R, C, L), so a
corner sweep already walks a decoupling cap to its minimum capacitance, but the
ESR and ESL stay at the package-class default from that section's table. Walking
min-C and max-ESR together, the real worst corner for a decoupling network, needs
the parasitics to carry their own band.

The tolerance model's own reference is `docs/ci/CI.md`, which specifies the
ensemble and what each corner means.
