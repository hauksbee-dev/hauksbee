# Signal-integrity static checks (`--si`)

Seven pure-arithmetic, physics-grounded static checks over the data hauksbee
already extracts (copper geometry, netlists, part values, board stackup) plus a
small table of cited datasheet constants. Each targets a bug class that really
ships. Checks 1 to 5 live in `crates/hauksbee-extract/src/si.rs` (+ the
`si/impedance.rs` submodule for check 5). Checks 6 (trace ampacity) and 7
(input-cap ripple) live in `hauksbee-engine` `checks::ampacity` / `checks::ripple`
because their current attribution needs the bound DB models, and are merged into
the same `--si` report, the way `--lint` merges the strap lint. All run as
`hauksbee run <board> --si`, and follow the same calibration discipline as the
rest of the tool:

> **Zero false positives on the known-good corpus, or the check does not fire.**

Every check has an explicit *unknown -> info, never a fire* path, so a missing
datasheet constant or an out-of-reach geometry produces silence (or an
informational note carrying the computed value), never a confident false
positive. The `info` notes are observations on the record, not findings: they
make the negative auditable.

The checks were swept across the full known-good corpus (KiCad `.kicad_pcb` +
Eagle `.brd`, 67 board files: Arduino, Adafruit, SparkFun, MNT Reform, Olimex
ESP32-EVB + Pico-PC, Watchy + history, ZSWatch mainboard + DevKit, LumenPnP,
Corne, Lily58, RP2040-minimal. The corpus's `hunt/` subtree is excluded from the
gate by design: those boards are suspected-fault targets, not proven-good
references). **Result: one open question, and nothing else.** Checks 1, 2 and 4 to
7 raise no finding on any known-good board, of any class. Check 3
(`antenna_keepout`) fires `high` on 11 of the 12 Olimex ESP32-EVB revisions, on
ground copper inside Espressif's 15 mm band. Whether that is a genuine RF
compromise on a shipping board or a keepout rectangle stricter than practice is a
hardware question nobody has settled, so the corpus gate is `#[ignore]`d and runs
under `-- --ignored` until it is (check 3 carries the geometry). Four tool
defects were found and fixed by chasing each surprise to the file (the Tarski
meta-lesson). They are recorded below.

Severity ladder: `high` (functional failure), `medium` (margin / robustness),
`low` (cosmetic), `info` (a computed value, not a finding).

---

## 1. Crystal load capacitance (`crystal_load_cap`)

### Model

A parallel-resonant crystal is specified for a load capacitance `CL`. The
board must present that `CL` across the crystal terminals for it to run on
frequency. The two external load caps `C1`, `C2` (one per terminal to ground)
and the parasitic stray `Cstray` combine as

```
CL_board = (C1 * C2) / (C1 + C2) + Cstray
```

The two caps are in series across the crystal (both return to ground). The
stray adds in parallel. `Cstray` folds the short PCB-trace parasitics and the MCU/IC
pin capacitance.

### Constants, thresholds and error band

- `Cstray = 4.0 pF` - the textbook midpoint (2-3 pF short trace + ~1-2 pF pin).
  Honest error band: roughly +-3 pF.
- `CL_TOLERANCE = 8.0 pF` - a finding requires the board CL to deviate from the
  crystal spec by more than 8 pF. This is set wide *on purpose*: a typical MHz
  crystal trims +-20..30 ppm, ~4-6 pF of CL slack near 18 pF, and the stray model
  carries +-3 pF. Summing these, only past 8 pF can the deviation be real rather
  than model noise. So the model uncertainty can never produce the finding on its
  own.
- Severity: `high` when `CL_board < 0.5 * spec` or `> 1.6 * spec` (the oscillator
  may not start or is grossly off). `medium` otherwise.

### The honest reach

The crystal's CL spec is **almost never in the netlist**: nearly every corpus
board puts only the frequency in the value field (`"12MHz"`, `"8MHz"`,
`"32.768 kHz"`). CL is derivable only from a recognised part-number value, held
in a small cited table (`KNOWN_CRYSTAL_CL`):

| Value match | CL | Citation |
|-------------|----|----------|
| `ABM8-272`  | 18 pF | Abracon ABM8 datasheet, `-272` = 18 pF CL option |

`ABM8G` (used on MNT Reform, value = frequency only) is **deliberately absent**:
it ships in several CL options and the ordering code is not in the file, so its
CL is unknown -> info, not a guess.

When CL is unknown, the check emits an `info` note with the computed `CL_board`
and no judgement. When *both* load caps are absent on a discrete crystal it fires
`medium` (a real omission). A single missing cap is `info` (often the
series-resistor topology that could not be traced).

Parts that integrate their own load caps are recognised and never flagged for
"missing caps":

- **Ceramic resonators** (Murata CSTxE / CERALOCK, ZTT, or a `RESONATOR`
  footprint): the 3-terminal centre pin is the integrated cap node.
- **RTCs with integrated oscillator caps**: PCF8523, PCF8563, PCF85063, RV-8263,
  RV-3028, DS3231, ABRACON AB18.

### Calibration evidence

- RP2040 minimal `Y1 = ABM8-272-T3`, two 15 pF caps -> `CL_board ~ 11.5 pF` vs
  the 18 pF spec, within 8 pF: **info, ok** (the documented RP2040 hint).
- crkbd Corne (both split halves), Lily58, SparkFun Pro Micro, Arduino Uno Y1,
  MNT Reform Y1/Y2/Y3: value = frequency only -> **info, CL on the record, no
  fire**.
- Arduino Uno Y2 (CSTCE16M0V53), SparkFun RedBoard Y1 (RESONATOR-SMD): ceramic
  resonators -> **silent**.
- MNT Reform Y4 (32.768 kHz on PCF8523): no external caps, RTC integrated caps ->
  **silent** (no note at all, since there is no CL to report).
- Watchy Y1 (32.768 kHz on PCF8563): the integrated-cap table stops the
  "missing caps" fire, but the board *does* fit discrete 18 pF caps, so the
  computed CL goes on the record as an **info** note:

  ```
  [info] crystal_load_cap - Y1 (32.768KHz): board presents CL ~ 13.0 pF (C1=18p, C2=18p, +4p stray); crystal CL spec unknown from value, no judgement
  ```

### Tool defects found and fixed (by chasing the corpus fires)

1. **Split-keyboard mirror prefix.** Corne / Lily58 duplicate the right half with
   a lowercase `r` prefix (`rY1`, `rC1`, `rC2`). The type classifiers rejected
   them, hiding the load caps and manufacturing a false "no caps" finding on a
   shipped board. Fixed: strip a single `r` mirror-prefix before classification.
2. **Eagle double-pad count.** The Eagle `.brd` extractor lists each pad once per
   signal contact, so a two-terminal cap shows four pin entries. The
   pad-count test (`== 2`) rejected them, hiding the Pro Micro / Arduino load
   caps. Fixed: count *distinct* pad numbers (the same class as the round-1 0201
   four-pad-resistor bug).
3. **Ceramic resonators** were not recognised as integrating their caps. Added.
4. **PCF8563** (Watchy RTC) was not in the integrated-cap RTC table. Added.

---

## 2. I2C rise time (`i2c_rise_time`)

### Model

I2C is open-drain: the pull-up charges the bus capacitance, and the spec caps the
30%->70% rise time

```
t_r = 0.8473 * Rpull * Cbus
```

(`0.8473 = ln(0.7/0.3)`, the RC charge between the I2C VIL/VIH thresholds.) The
limit is **1000 ns standard mode** (100 kHz) and **300 ns fast mode** (400 kHz).
Too-weak a pull (R too high) or too much bus capacitance blows it.

Bus capacitance is `Cbus = devices * 10 pF + trace_length * (0.038 to 0.15 pF/mm)`:

- 10 pF per I2C device pin (a common datasheet figure, refinable per part).
- **0.038 to 0.15 pF/mm** of routed trace, a range rather than a number, summed
  from the layout's discrete segments and true arc sweeps via `routed_length_mm`,
  the same geometry the USB skew check reads.

The trace figures are derived, not picked: for a transmission line
`C' = sqrt(Er_eff) / (c0 * Z0)`, which on FR4 (`Er_eff ~ 3`) is 0.116 pF/mm at
50 ohm, 0.077 pF/mm at 75 ohm and 0.057 pF/mm at 100 ohm; the widest,
closest-coupled realistic case (`Er_eff 3.2`, `Z0 40 ohm`) gives 0.149 pF/mm.

**When the board declares a stackup, this is computed rather than assumed.**
`trace_capacitance_pf_per_mm` takes the net's narrowest track width and the
declared stackup, gets `Z0` from the same microstrip model check 5 uses, and
returns `C' = sqrt(Er_eff) / (c0 * Z0)` with `Er_eff` from the standard Hammerstad
approximation. A 0.25 mm trace on 1.51 mm FR4 works out at 0.044 pF/mm, and the
note says it was computed from the board stackup and the net's track width. There
is no range left to bracket, and the remedy for an assumed run is therefore a real
one: declaring the stackup changes the answer.

Without a stackup the impedance is unknown, so hauksbee does not pretend to know
it. It carries the whole range, and **which end is used where** follows this
module's standing rule that a check fires only when even the most lenient
assumption is violated:

| Figure | Value | Used for |
|--------|-------|----------|
| `C_TRACE_PF_PER_MM_LOW` | 0.038 pF/mm (thin 2-layer, 150 ohm) | **gating whether a finding is raised at all** |
| `C_TRACE_PF_PER_MM_HIGH` | 0.15 pF/mm (40 ohm worst case) | reported alongside, so the reader sees the worst case the geometry permits |

Firing on the high end would fail real boards: a 700 mm 150 ohm route on an
8-device bus behind a 10 kohm pull is ~902 ns at its own impedance (in spec) but
well over the limit if charged 0.15 pF/mm. Findings therefore quote both numbers
and are raised only when the low bound is already over the limit.

The low figure is the bottom of the range hauksbee will reason about, **not** a
proof that no route is lower: impedance rises without bound as a trace narrows
and its plane recedes, and the 10 pF per device pin beside it is a
datasheet-typical figure rather than a floor either. The messages say "the low
end of the plausible range" and never claim it is the lowest possible.

Because of that, **severity depends on what the verdict rests on.** The
pin-capacitance figure needs no geometry: device count and pull-up value both come
from the netlist. When that alone exceeds the limit, the finding does not rest on
the routing and carries full severity. It says exactly that, and no more: the
10 pF per pin is still a datasheet-typical figure rather than a measurement, so
the message names it and points at per-part models as the way to tighten it. When the limit is only
exceeded once the trace term is added, the shortfall is true for the impedance
range assumed and false above it, so it is capped at `medium` and states that the
device pins alone are within the limit and a higher-impedance route would pass.
The check still fires, because a long bus really is the failure mode this exists
to catch; it just does not dress an assumption as a measurement. Both constants are
pinned against the closed form by
`si::tests::trace_capacitance_per_mm_matches_transmission_line_physics`, so a
units slip (a pF/inch or pF/cm figure written as pF/mm, which would inflate every
bus by an order of magnitude) fails a test rather than shipping.

The trace term is real either way: on a 10-device bus behind a 10 kohm pull, an
800 mm run adds 30 pF even at the low end, taking `Cbus` to 130 pF and `t_r` from
~847 ns (in spec) to ~1105 ns (out). Dropping it under-reports rise time, so
a `.kicad_pcb` layout is always folded in when one is uploaded. **Without a layout the routing
term is unavailable**, and the note says so in words (`routing capacitance NOT
counted - upload the .kicad_pcb layout to include trace copper`): the
device-count number is a floor, not a verdict. With a layout the note states the
routed length it counted, so a reader can tell the two apart.

This **upgrades** the existing netlint `missing_i2c_pullup` check from
"is a pull-up present" to "is the pull-up *sufficient*". No-pull-up is
netlint's job. This check only audits a bus that already has a pull.

### Threshold and mode inference

Mode inference is **conservative**: assume standard mode (1000 ns) unless the net
name encodes fast mode (`FM` / `FAST`). The check fires only when even the most
lenient assumed mode is violated, with `high` past 1.5x the limit and `medium`
otherwise. This is exactly what keeps it silent on the proven-good corpus buses.

### Calibration evidence

Measured on the real layouts, with the routed term included:

`Cbus` and `t_r` are given as the reported range (low bound first):

| Board / bus | Pull | Devices | Routing | `Cbus` | `t_r` | Verdict |
|-------------|------|---------|---------|--------|-------|---------|
| Olimex UEXT SDA (REV-L) | 2.2 kohm | 1 | 73 mm | 13-21 pF | 24-39 ns | ok |
| ZSWatch RTC SDA (PCA9306+RV-8263) | 3.3 kohm | 2 | 1 mm | 20 pF | 56 ns | ok |
| ZSWatch Extension SDA | 1.8 kohm | 8 | 62 mm | 82-89 pF | 126-136 ns | ok |

Routed copper is a real term without being the dominant one: on the Olimex UEXT
bus, one device pin and 73 mm of track puts a fifth to a half of the capacitance
in the trace, so a device-count-only model rates it ~19 ns instead of 24-39 ns.

The ZSWatch 8-device Extension bus is the corpus's closest-to-the-limit I2C bus
and still sits ~7x under standard mode. The designers chose 1.8 kohm precisely
because of the device count. It is the discriminating no-fire (a regression that
mis-scaled the RC or counted the connector as a device would push it over and the
corpus test would go red).

Hand-checked physics: `0.8473 * 4700 * 100 pF = 398 ns` (the classic 4.7k/100pF
~ 400 ns). `0.8473 * 10000 * 250 pF = 2118 ns` (blows standard mode).

---

## 3. Antenna keepout (`antenna_keepout`)

### Model

A PCB-trace antenna (chip antenna, or the integrated antenna of an
ESP32-WROOM / nRF module) needs a copper-free, ground-free region around and
beyond it. Ground plane or routed copper inside the keepout detunes the
antenna and absorbs radiated power. The symptom is poor range / sensitivity (the Watchy /
Inkplate-6 bad-WiFi class).

The check locates antenna-bearing parts in a cited keepout table, projects the
datasheet keepout rectangle into board coordinates using the part's placement
`(x, y, rotation)`, and tests whether any **other** net's copper (segments, arcs,
vias, zone fills, foreign pads) falls inside it. The module's own pads and net
are excluded. Severity is `high` when a *ground* net intrudes (detunes hardest),
`medium` otherwise.

### Keepout table (cited, verified)

| Module | Keepout (local mm) | Citation |
|--------|--------------------|----------|
| ESP32-WROOM-32 / 32E / 32D / 32U | `x[-9, 9]`, `y[-20.3, -5.3]` | Espressif ESP32-WROOM-32 hardware design guidelines: 15 mm antenna keepout |

The WROOM rectangle's origin convention was **verified empirically** against the
real corpus footprint (OLIMEX `ESP-WROOM-32_MODULE`): its pads span local-y
`[-5.31, +12.30]`, so the antenna edge (the pad-free end) is at local
`y ~ -5.3`, and the 15 mm keepout extends from there to `y = -20.3`, module-wide.

### The honest reach (a dropped entry, recorded)

Other integrated-antenna modules are **deliberately not in the table**. A guessed
keepout rectangle for the u-blox NORA-B106 (ZSWatch) produced dozens of spurious
intrusions - a textbook false positive - because neither its datasheet keepout
rectangle nor the footprint-origin convention could be verified to the precision
this check needs. Per the discipline (do not fabricate constants), it was
dropped: an unverified module gets no keepout and never fires. Adding one back
requires the cited datasheet rectangle plus a placement-verified corpus board.

### Calibration evidence, and the one unsettled question in `--si`

The corpus's ESP32-WROOM-32 boards are the twelve Olimex ESP32-EVB revisions, and
they are the only known-good boards where any `--si` check fires. Measured on
REV-L: `U3` sits at `y = 79.883`, so its antenna edge is at `y ~ 74.57`, and the
board outline starts at `y = 67.056`. Roughly 7.5 mm of Espressif's 15 mm band
therefore hangs off the board in free space, and the other 7.5 mm lies over
copper Olimex floods with ground.

Eleven of the twelve revisions report that ground copper:

```
$BIN run "$C/olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_pcb" --si
  [high] antenna_keepout - U3 (ESP32-WROOM-32E-N4): 21 foreign copper intrusion(s) inside the antenna keepout [Espressif ESP32-WROOM-32 hardware design guidelines: 15 mm antenna keepout]: nets ["GND"], primitive kinds ["track", "via", "zone"], e.g. track on net 'GND' at (78.74, 74.30) mm
```

The intrusion count runs 17 to 22 across those eleven revisions, always on `GND`,
always a mix of track, via and zone fill. REV-A is the one revision that is
genuinely clear:

```
$BIN run "$C/olimex_esp32/HARDWARE/REV-A/ESP32-EVB_Rev_A.kicad_pcb" --si
  [info] antenna_keepout - U3 (ESP-WROOM-32): antenna keepout [Espressif ESP32-WROOM-32 hardware design guidelines: 15 mm antenna keepout] is clear of foreign copper - ok
```

This is a hardware question, not a tool question, and it is open: a shipping,
widely used board either has a real RF compromise here, or the 15 mm rectangle is
stricter than practice and the check needs a refinement (a partial-band or
edge-proximity term). Asserting the finding is correct would be exactly as
unearned as asserting it is not, and whitelisting the Olimex boards would hide
whichever answer it turns out to be. So the two affected tests carry
`#[ignore = "unsettled: ..."]` and the corpus gate is run explicitly with
`-- --ignored` (see Reproduce). Every other known-good board class, and every
other `--si` check on them, is silent.

Independent of that question, the check's fire path is proven on a synthetic board
with a ground pour placed inside the keepout, and the keepout geometry itself is
pinned by the empirically verified footprint origin above.

---

## 4. USB differential pair (`usb_diff_pair`)

### Model

A USB D+/D- pair must be length-matched so the two edges arrive together.
Intra-pair skew converts differential into common-mode and erodes the eye. The
check finds D+/D- pairs by net name (`D+/D-`, `DP/DM`, `USB_DP/USB_DM`, `UD+/UD-`,
etc.), sums each leg's routed discrete-trace length per layer (segment + arc
chord), and compares.

```
skew = | length(D+) - length(D-) |
```

### Thresholds

- **Full-speed** (12 Mbps): very tolerant. The 8.33 ns bit time swamps any sane
  board mismatch, so the check uses a lenient 15 mm budget.
- **High-speed** (480 Mbps): tight, commonly <= 1.25 mm.

Since FS vs HS is not always inferable from the netlist, the **default is the
lenient FS limit**: the check fires `medium` only on a gross skew over 15 mm, and
otherwise reports `info` with the measured skew and the HS limit for reference.

Width / gap consistency is reported as an **info note only**: a diff pair necking
down at its connector/IC pad entry is universal and benign (e.g. the ZSWatch
DevKit's D- tapers 0.170 -> 0.127 mm at the USB-C pads), so it must never be a
fire.

### The honest reach

Routed discrete-trace length only. Vias add a small out-of-plane length
approximated as zero, and arcs use the straight chord (under-estimates by a few
percent, well inside the skew tolerance). No poured-net length.

### Calibration evidence

Every corpus USB pair is full-speed (RP2040, Watchy, MNT Reform, crkbd) and reads
a small skew well inside the FS budget -> **info, never a fire**. Example: RP2040
minimal D+/D- = 19.2 / 19.1 mm, skew 0.10 mm. The ZSWatch DevKit pairs (skew
0.72 mm, with the benign pad neck-down) are `info`, correctly not a finding.

---

## 5. Controlled impedance (`controlled_impedance`)

Tells a USB / Ethernet / high-speed designer whether their controlled-impedance
traces are routed to the right characteristic impedance, from trace geometry +
the board stackup, using the standard quasi-static closed-form formulas. Lives in
`crates/hauksbee-extract/src/si/impedance.rs`.

### Formulas (hand-checked against a published reference calculator)

These are the same equations the online calculators (Polar's IPC-2141 form,
chemandy, the National Semiconductor differential form) use, so the tool matches
them to a fraction of a percent. They are **quasi-static closed-form estimates,
not a field solve.**

- **Single-ended microstrip** (IPC-2141), a trace on an outer copper layer over
  the nearest reference plane:

  ```
  Z0 = (87 / sqrt(Er + 1.41)) * ln(5.98*H / (0.8*W + T))
  ```

  `H` = dielectric height to the plane, `W` = trace width, `T` = copper
  thickness, `Er` = substrate dielectric constant.

- **Single-ended stripline** (IPC-2141), a trace between two planes:

  ```
  Z0 = (60 / sqrt(Er)) * ln(4*H / (0.67*pi*(0.8*W + T)))
  ```

- **Differential microstrip** (National Semiconductor / Wadell), an edge-coupled
  pair:

  ```
  Zdiff = 2*Z0 * (1 - 0.48 * exp(-0.96 * S / H))
  ```

  `S` = edge-to-edge trace spacing (measured from the routed geometry: the median
  centerline gap between a D+ segment and a D- segment, minus the two
  half-widths). `Z0` is the single-ended microstrip impedance of one leg.

#### Validation (formula vs reference calculator)

| Case | W / S / H / T / Er (mm) | Tool | Reference calculator | Δ |
|------|--------------------------|------|----------------------|---|
| 50-ohm microstrip | W 0.3, H 0.2, T 0.035, Er 4.3 | **53.5 Ω** | 53.5 Ω (chemandy / mycalctools IPC-2141) | <0.1% |
| microstrip #2 | W 0.25, H 0.2, T 0.035, Er 4.3 | **59.2 Ω** | 59.3 Ω | <0.2% |
| wide-trace ~50 Ω | W 2.9, H 1.51, T 0.035, Er 4.5 | **48.1 Ω** | ~48 Ω | within band |
| stripline | W 0.15, H 0.5, T 0.035, Er 4.3 | **52.5 Ω** | 52.5 Ω (hand) | <0.1% |
| 90-ohm USB diff | W 0.3, S 0.2, H 0.2, T 0.035, Er 4.3 | **87.4 Ω** | 87.4 Ω (hand, NatSemi form) | exact |

(The prompt's "~0.3 mm on 0.2 mm FR4 Er 4.3 is roughly 50 ohm" lands at 53.5 Ω
on the IPC-2141 empirical form, the same number every published calculator
returns. The USB geometry W 0.3 / S 0.2 / H 0.2 gives 87.4 Ω, inside 90 Ω
±15%.)

### Stackup: what the board provides vs what is assumed

KiCad stores the stackup in `(setup (stackup ...))`: each physical layer carries
a `type` (`copper` / `core` / `prepreg` / mask / silk), `thickness`, and for
dielectrics `material` + `epsilon_r` + `loss_tangent`. The check reads the F.Cu
copper thickness `T`, the **first dielectric under F.Cu** as the microstrip
reference height `H`, and its `epsilon_r` as `Er`.

35 corpus `.kicad_pcb` files carry a stackup with dielectric constants in it, and
the two halves of the corpus look quite different:

| | Known-good boards (17 files) | Whole corpus incl. `hunt/` (35 files) |
|--|------------------------------|---------------------------------------|
| Dielectric material | `FR4`, all of them | `FR4`, plus `2313` prepreg on sbc-a13 |
| `Er` (first dielectric under F.Cu) | 4.5, all of them | 4.0 (VendettaFC) to 4.6 (solar Meshtastic node), 4.5 on most |
| `T` (F.Cu) | 0.035 mm (1 oz), all of them | 0.035 mm bar three: 0.07 mm on VendettaESC and moco-rd501, 0.0696 mm on the wafel eval board |
| `H` (reference height) | 0.1 mm (Reform eDP input) to 1.51 mm (2-layer keyboards), 0.196 mm on the 4-layer watch boards | 0.079 mm (sbc-a13) to 1.51 mm, 0.0994 mm on MokyaLora |

Solder-mask layers also declare an `epsilon_r` (3.3 to 3.8 on the `Liquid Ink`
masks), which is why the raw range of `epsilon_r` values in the corpus runs from
3.3. The check never reads those: `Er` comes from the first *dielectric* under
F.Cu, which is what the microstrip references.

When the file has **no stackup** (e.g. the RP2040 minimal board), the check does
not guess silently: it computes against a **stated default assumption** (1.51 mm
FR4 core, `T` = 0.035 mm 1 oz copper, `Er` = 4.3) and reports the result as
`info` only, with the word `ASSUMED` in the note, never a finding.

Stripline is implemented and validated but not auto-applied: identifying which
inner layer is a solid reference plane (needed for the plane-to-plane `H`) is not
something the stackup block alone states, and the corpus controlled-impedance
nets are outer-layer microstrip. (Documented reach, not a guess.)

### Targets and tolerance

| Class | Detected by | Target | Tolerance |
|-------|-------------|--------|-----------|
| USB D+/D- | the shared diff-pair detector (`D+/D-`, `DP/DM`, `USB_D±`, `UD±`, …) | 90 Ω differential | ±15% |
| Ethernet / MDI pair | name convention `TRD`/`TRX`/`MDI`/`MX0..3`/`ETH` + `_P`/`_N`/`±` | 100 Ω differential | ±15% |
| 50 Ω single-ended | RF-feed names only (`RF`, `RF_IN/OUT`, `ANT*`) | 50 Ω | ±15% |

±15% is the looser of the two common fab tolerances (±10% / ±15%). The looser
band keeps the model's own few-percent error from ever producing the finding on
its own. A deviation past ±30% is `high` (the link will reflect / fail), 15-30%
is `medium`. Intra-pair skew and width necking are surfaced by check 4
(`usb_diff_pair`). This check adds the impedance.

The single-ended set is deliberately narrow (RF feedlines only): an ordinary GPIO
or a bare `CLK` is **not** assumed to be a 50 Ω controlled line, so it is never
judged.

### The two honesty gates (and what a board that declares control gets)

A deviation becomes a **finding** only when BOTH hold. Anything short is an
`info` note carrying the computed impedance and the deviation:

1. **A real file stackup** (not the default assumption).
2. **Declared impedance-control intent**: KiCad's
   `(stackup (dielectric_constraints yes))`. This is the hard-won corpus lesson.
   The closed-form model is a genuine estimate with a real error band on dense
   real boards: it has **no co-planar-ground term** and assumes the trace
   references the nearest plane at the dielectric height, so on 4-layer boards
   with ground-flanked routing it over-estimates `Zdiff` by ~25-35% (every
   genuinely-coupled corpus USB/Ethernet pair reads 119-125 Ω against a 90/100 Ω
   target). And on 2-layer boards a full-speed USB pair that was *deliberately
   not* impedance-controlled (every keyboard / trackball in the corpus) reads
   140-160 Ω and is perfectly fine. We cannot tell a full-speed pair (impedance
   irrelevant) from a high-speed one (impedance critical) from the netlist. So
   the board must itself say it is impedance-controlled before a deviation is a
   defect.

**Every known-good corpus board sets `dielectric_constraints no`** (they chose not
to control these nets), so the check is silent across that whole set while still
computing and surfacing every impedance as an auditable `info` note.

Three boards in the corpus's `hunt` subtree do declare `dielectric_constraints
yes`, and they are where the intent gate opens. **MokyaLora** (`H` = 0.0994 mm,
Er 4.5) is the one that fires, four findings on its 50 Ω RF feed network:

```
$BIN run "$C/hunt/mokyalora/hardware/kicad/MokyaLora.kicad_pcb" --si | grep controlled_impedance
  [medium] controlled_impedance - Net-(ANT2-MTCH): W~0.100 mm microstrip -> Z0 ~ 59 ohm vs target 50 ohm, single-ended: +17.6% deviation exceeds +-15% tolerance [board stackup H=0.099 mm, T=0.035 mm, Er=4.50]
  [high] controlled_impedance - Net-(U10-VCC_RF): W~0.254 mm microstrip -> Z0 ~ 33 ohm vs target 50 ohm, single-ended: -34.5% deviation exceeds +-15% tolerance [board stackup H=0.099 mm, T=0.035 mm, Er=4.50]
  [high] controlled_impedance - Net-(U10-GND_RF_2): W~0.254 mm microstrip -> Z0 ~ 33 ohm vs target 50 ohm, single-ended: -34.5% deviation exceeds +-15% tolerance [board stackup H=0.099 mm, T=0.035 mm, Er=4.50]
  [high] controlled_impedance - Net-(U10-ANT_OFF): W~0.250 mm microstrip -> Z0 ~ 33 ohm vs target 50 ohm, single-ended: -33.6% deviation exceeds +-15% tolerance [board stackup H=0.099 mm, T=0.035 mm, Er=4.50]
  [info] controlled_impedance - /MCU_RP2350B/USB_DP / /MCU_RP2350B/USB_DN: W~0.142 mm, S~0.200 mm microstrip -> Zdiff ~ 92 ohm vs target 90 ohm, USB differential (+2.7%, within +-15%) - ok [board stackup H=0.099 mm, T=0.035 mm, Er=4.50]
  [info] controlled_impedance - Net-(ANT1-FEED): W~0.120 mm microstrip -> Z0 ~ 54 ohm vs target 50 ohm, single-ended (+8.3%, within +-15%) - ok [board stackup H=0.099 mm, T=0.035 mm, Er=4.50]
  [info] controlled_impedance - Net-(U10-RF_IN): W~0.120 mm microstrip -> Z0 ~ 54 ohm vs target 50 ohm, single-ended (+8.3%, within +-15%) - ok [board stackup H=0.099 mm, T=0.035 mm, Er=4.50]
  [info] controlled_impedance - Net-(ANT2-FEED): W~0.120 mm microstrip -> Z0 ~ 54 ohm vs target 50 ohm, single-ended (+8.3%, within +-15%) - ok [board stackup H=0.099 mm, T=0.035 mm, Er=4.50]
```

Note the last four lines: the same board's USB pair and its three in-band RF feeds
stay `info`, so the fire is per-net, not per-board. The other two, **VendettaESC**
and **sbc-a13**, declare control
but carry no net the target table recognises as controlled-impedance (no USB or
MDI pair, no RF-feed name), so nothing is computed and the check says nothing.

That split is the design working as intended: a board that declares impedance
control yet routes a line out of band is the genuine bug class this fires on, and
the known-good boards never make the declaration.

### Calibration evidence

- **Watchy** (4-layer, `H` = 0.28 mm, Er 4.5, `dielectric_constraints no`): the
  USB pair computes `Zdiff ~ 125 Ω` and the `LNA_IN/RF` feed `Z0 ~ 63 Ω`. Both
  are out of band, both are `info` (the board did not declare control) - **not a
  fire.** The intent gate in action on a real board.
- **ZSWatch** mainboard + DevKit, **MNT Reform** motherboards (the `ETH0_*` and
  `LPCUD_*` pairs), **lily58 / corne / Reform keyboards + trackball**, **LumenPnP
  mobo**: all compute a real impedance (119 Ω … 209 Ω), all `dielectric_constraints
  no`, all `info`. **Zero findings.**
- **RP2040 minimal**: no stackup -> USB pair estimate under the `ASSUMED` default
  stackup, `info` only.

### What this is NOT

- **Not a field solver.** Quasi-static closed-form only, no 2D/3D EM solve.
- **No co-planar-ground / ground-flanked term**, no via-stub or reference-plane-
  transition modelling. This is the single biggest source of the over-estimate
  above.
- **No crosstalk, no reflection / TDR, no insertion-loss / dielectric-loss sim**
  (the `loss_tangent` in the stackup is read but not yet used).
- **No FS-vs-HS inference**: it cannot tell whether a USB pair actually needs to
  be 90 Ω, which is exactly why the `dielectric_constraints` intent gate exists.
- Spacing is the median routed segment gap, not the designer's intended gap.
  Arcs and poured copper are not used for the spacing measure.

---

## 6. Trace ampacity (`trace_ampacity`)

### Model

IPC-2221 ampacity, `I = k * dT^0.44 * A^0.725` (k = 0.048 outer, 0.024 inner),
applied to the **narrowest routed segment** on a net (the series bottleneck), at
the copper weight and layer side that segment is built in. The physics, the Poured-net exemption, and the never-invent-a-
current rule all live in `hauksbee-extract`'s `trace_current` module and are
unit-tested there. What `--si` adds is the *attribution* layer
(`hauksbee-engine` `checks::ampacity`) that decides which net carries how much
current, from the bound DB models, so the check runs automatically instead of
by hand.

### Copper weight and layer side (per layer, from the stackup)

Ampacity is not a per-board constant, so the check does not rate every net the
same way. `CopperWeights::from_root` reads the same `(setup (stackup ...))` block
check 5 uses, and takes each **copper** layer's declared `thickness` plus its
position in the declared top-to-bottom order. Thickness converts back to weight
at 0.035 mm per oz; the first and last copper entries are the outer layers and
everything between them is internal, which is what selects IPC-2221's `k`.

Each **trace-routed** net is then rated at its **ampacity** bottleneck: the
(layer, width) pair with the lowest IPC-2221 rating, taken over every layer the
net is routed on (`NetCopper::min_width_by_layer`, resolved by
`TraceAudit::bottleneck`). A net carrying any copper zone is `Poured` and is not
rated at all, on any layer, per honest-reach item 1: hauksbee does not rasterise
a fill, so it cannot tell a pour that genuinely carries the current from one that
leaves a narrow inner-layer segment in series. That exemption is deliberately
coarse and is the check's largest remaining reach limit.

That is deliberately *not* the narrowest segment. While every net was rated as
1 oz external, minimum width and minimum ampacity were the same thing. Once
weight and the internal/external constant differ per layer they diverge: 0.3 mm
of 1 oz outer copper carries ~1.0 A, while 0.5 mm of 0.5 oz inner copper carries
only ~0.44 A. Ranking by width on such a net rates the 0.3 mm outer segment and
passes a cited 0.8 A while the wider inner segment is the real choke. Ties
resolve to the lower-ampacity, then internal, then lighter, then
alphabetically-first layer, so the choice is deterministic.

Outer layers are identified by **name** (`F.Cu` / `B.Cu`), not by position in the
stackup, because position lies on a truncated declaration: a stackup listing only
`F.Cu` and `In1.Cu` would make `In1.Cu` the last copper entry and rate inner
copper with the external constant, doubling its apparent capacity. Positional
first/last is the fallback only for a stackup that names no `F.Cu`/`B.Cu` at all.

This matters by about 3x. On the common 4-layer build (1 oz outer, 0.5 oz inner),
a 0.5 mm trace rates **~1.45 A as 1 oz external** copper and **~0.44 A as 0.5 oz
internal** copper: half the `k` and half the cross-section. Rating everything as
1 oz external therefore let genuinely undersized inner-layer traces pass, which
is why a cited 1.0 A on that trace fires on `In1.Cu` and stays silent on `F.Cu`.

When the board declares **no stackup**, the 1 oz external default still stands
(the verdict on such boards is unchanged), but it is not printed as a fact about
the board. The two assumed cases are kept apart, because saying "declares no
stackup" about a board that declares one is itself a false claim:

| Case | `CopperSource` | Message |
|------|----------------|---------|
| No stackup in the layout | `AssumedNoStackup` | `ASSUMED 1 oz external - the layout declares no stackup, so upload a stackup declaration or fab drawing ...` |
| Stackup present, this layer absent or zero-thickness | `AssumedLayerMissing` | `ASSUMED 1 oz internal - the layout declares a stackup but no copper weight for In9.Cu, so upload ... that covers every layer` |
| Layer declared with a thickness | `Stackup` | `0.50 oz internal In1.Cu, per the board stackup` |

Only the **weight** falls back to 1 oz. The internal/external side is inferred
from the layer's own name (`F.Cu` / `B.Cu` external, `In<N>.Cu` internal,
anything unrecognised internal), because defaulting an undeclared `In1.Cu` to
external doubles its apparent capacity and would suppress the very findings
per-layer rating exists to recover. `TraceAudit` therefore has no `external`
field at all: which constant applies is a property of the layer, never a
per-audit choice.

The `--ampacity` table carries the matching header and a per-row
`1 oz ext (assumed)` / `1 oz int (assumed)` basis, so measured and assumed ratings
are never confusable. Only the weight is defaulted; the side comes from the layer
name, which is why the header says so rather than claiming every fallback is
external.

**The reported bottleneck and the verdict's evidence are separate.** The finding
always reports the net's true worst point, so the width it names as the fix
actually settles the net; substituting a declared-but-higher-rated segment would
name a width that still leaves the real choke undersized.
`TraceCurrentFinding::declared_shortfall` separately carries the lowest-rated
segment whose weight the board **declared**, when that segment independently fails,
and that is what decides severity. When the two differ, the message names both.

**An assumed weight never produces a verdict.** A shortfall computed on assumed
copper is real under that assumption and false under another: a 0.25 mm trace
rates 0.88 A as 1 oz and 1.45 A as 2 oz, so a cited 1.2 A would fail the estimate
and pass the real board. Those rows are therefore reported at `info` with the
words "This is NOT a verdict", naming the upload that would make it one. Only a
bottleneck whose weight was read from a declared stackup raises a `high` finding.
This is the same rule the controlled-impedance check above already follows, where
a defaulted stackup is always informational.

### Attribution (the zero-false-positive boundary)

A current is attributed only from an explicit, citeable operating-current
source. Today that means a `[models.current_program]` equation tagged
`semantics = "regulated_current"`, such as a linear charger's constant-current
phase, evaluated from the parts actually fitted. The schema supports a simple
`I = k_volts / R` law, a continuous two-branch inverse-resistance law, and a
program-voltage-scaled sense-resistor law. Independent loads on the same side of
a rail are summed. When one regulated stage sources a rail and another consumes
it, the check takes the larger directional total instead of counting the same
through-current twice; every contributor stays in the citation. Direction comes
only from the model's required `current_in_roles` / `current_out_roles`; control
and sense roles are never selected by name heuristics.

Converter output-current limits, regulator/connector/FET ratings, and equations
tagged `protection_limit` are capabilities or trip thresholds, not proof of board
draw, so they never seed steady-state ampacity. Generic/placeholder models never
seed it either. Everything else is left unattributed and the IPC engine skips it.

For a programmable part, `ratings.max_current_a` remains a **device-level
analysis threshold** (normally an absolute limit, or a deliberately documented
lower operating ceiling), not a load or a promise of normal operation. The separate
`current_program.max_operating_current_a` is the declared domain of the sourced
transfer equation.
The populated DC-equivalent resistance is read from the layout with a bounded
nodal-conductance solve from the programming pin to ground. Every simultaneously
populated series, parallel, or bridge branch participates; a closed solder link
is a short and a capacitor/open link is open. An unclassified numeric fuse,
thermistor, multi-terminal branch, conflicting identity, incomplete terminal,
or network beyond the explicit hop/node/edge bounds refuses the calculation.
Repeated physical pad records collapse only for this electrical-terminal
topology. When the network cannot be read, the part attributes nothing and an
info finding names which part, which pin, and what would close the gap: the
alternative, falling back on either ceiling, reports a number nobody measured.

Every `regulated_current` model must supply a sourced
`max_operating_current_a`; validation refuses an unbounded inverse law. Above
that domain the default `above_domain = "abstain"` produces an undetermined
attribution. It clamps to the domain endpoint only when the model explicitly
declares `above_domain = "saturate"`, which itself requires the sourced endpoint.
The programming transfer within that domain is still a point estimate unless
its model supplies separately sourced tolerances. Hauksbee does not promote a
typical-only datasheet row to a guaranteed maximum, so the citation says nominal
rather than claiming unprovided component or resistor tolerances.

For a sense-scaled regulated law, every sense role is paired with a required
far-side role. Exactly one adjacent shunt must connect each declared pair and all
shunts must have equal nominal resistance; a mismatch, extra branch, or wrong
net is undetermined. Full branch current is attributed only to the model's main
power terminals, never to Kelvin-sense or ground-reference stubs. The checked-in
LTC4020 equation is instead tagged `protection_limit`, so it does not seed
steady-state ampacity at all.

### The honest reach

- **Pour exemption.** A net that carries a copper zone is reported `Poured` and
  never flagged: its real cross-section is the plane, not the discrete pad-entry
  stubs, and measuring the stub would be a guaranteed false positive (the
  mppt/pd-sink VBUS/VDC pours, and the LumenPnP motor supply, are exactly this).
- **Routed traces only.** Only discrete `(segment)`/`(arc)` widths are read,
  width, not length (length is voltage drop, a separate concern).
- It fires on a genuinely under-width *routed* trace carrying a cited current,
  and is silent everywhere it cannot see the real conductor or no current is
  attributable.

### Calibration evidence

- Integration: `--si` surfaces an ampacity finding on a synthetic programmed
  charger whose regulated output is routed on an undersized discrete trace **and
  whose board declares its copper weight**. It stays silent when the rail is
  poured, when only a regulator rating is known, and when no operating-current
  source is attributable (`crates/hauksbee-engine/tests/si_ampacity_ripple.rs`).
- Two-sided on the copper-weight evidence, same file: the identical 0.05 mm rail
  with **no** declared stackup produces an `info` note saying "NOT a verdict"
  rather than a finding. The arithmetic is why, not squeamishness: 0.05 mm carries
  0.27 A as 1 oz but 0.46 A as 2 oz, which is above the cited 0.40 A, so the
  shortfall is an artefact of the assumed weight and the board may be fine.
  `tests/waiver_gate.rs` declares a stackup on its fixture for the same reason,
  since a note is not a thing a waiver can gate.
- Zero-FP corpus sweep across the famous boards (gated by
  `HAUKSBEE_REQUIRE_CORPUS=1`) raises no ampacity findings. That sweep caught,
  and forced the fix of, two attribution false positives: the generic power-FET
  fallback's placeholder 20 A treated as a cited rail current, and a charger's
  datasheet ceiling treated as its load. The second one is the Olimex ESP32-EVB
  rev D, whose MCP73833 is programmed at 200 mA by a 4.99 kΩ resistor on PROG:
  charging the part's 1.00 A ceiling to `+5V` and the battery net raised two High
  findings, five times over, on rails a shipped board has always driven fine.
- Programmed-current attribution is pinned two-sided in
  `crates/hauksbee-engine/src/checks/ampacity.rs`: the Olimex topology
  (`PROG -> 4.99k -> closed link -> GND`) yields the equation's own answer and a
  citation naming the resistor; an open link yields no attribution and a recorded
  gap; a filter cap on PROG is not read as the programming element; the equation
  abstains beyond the part's declared operating domain unless saturation is an
  explicit model fact. Unitless capacitors,
  numeric fuses, thermistors, populated parallel branches, repeated physical
  pads, simultaneous chargers, and mismatched Kelvin paths each have a direct
  regression. A plain regulator rating is explicitly proven *not* to become a
  load.
- The LumenPnP motor-driver sweep (`trace_current_corpus.rs`) pins the poured-
  rail / adequately-sized-coil-trace honest negative at the TMC2226 datasheet
  maximum.

---

## 7. Input-capacitor ripple current (`input_cap_ripple`)

### Model

A buck converter chops its input current between 0 and `I_out` at duty
`D = Vout/Vin`, so the input bulk cap carries an RMS ripple

```
I_rms = I_out * sqrt(D - D^2)      (worst case 0.5 * I_out at D = 0.5)
```

which is compared against the cap's rated RMS ripple current. Over-running the
rating ages the cap out early through I^2*ESR self-heating. The converter
topology (switch node / input rail / output rail / bulk caps) is recovered
structurally from the netlist + part kinds by `checks::converter` (a switch-node
net tying a power FET to the power inductor, with a bulk cap on the input rail),
so a discrete power stage built from a gate driver + external FETs is read the
way an engineer reads it off the schematic.

### Cap ripple rating

Only a part-specific datasheet value (`Ratings.max_ripple_current_a`) is
decision-grade. Hauksbee does not estimate ripple capability from capacitance:
dielectric, construction, can size, ESR, temperature, and frequency all matter,
so a guessed class number would be precision without evidence. The built-in
production example is the exact United Chemi-Con `EKYB630ELL122MLN3S` ordering
code: 63 V, 1200 uF and 3.0 A_rms at 100 kHz / 105 C. The rule is anchored to
that MPN, not to every 1200 uF capacitor. Put the ordering code in an `MPN`,
`Manufacturer Part`, or `Part Number` property (or use it as the component
value) for that evidence to bind. Source: [United Chemi-Con product
page](https://www.chemi-con.co.jp/en/products/detail-condenser.php?part_number=EKYB630ELL122MLN3S).

### The zero-false-positive boundary

The check fires **only when topology, an exact cap ripple rating, an attributable
`I_out`, and nominal duty are all known**. Duty is computed as `Vout/Vin` only
when both directional rail names contain exactly one conventional voltage token
(`12V`, `3V3`, or `3.3V`, for example `PWR_IN_12V` and `CORE_OUT_3V3`). Missing,
ambiguous, zero, or non-step-down voltage pairs abstain; the old unconditional
`D=0.5` assumption is not used for findings. When any input is absent the check
emits an info note (the negative is on the record) and does not fire. It never
invents a rating, current, duty, or decision threshold. `I_out` comes from the same operating-
current contract as ampacity: simultaneous `regulated_current` loads are summed;
converter/OCP limits and regulator/connector/FET ratings are capabilities and
excluded. A parallel input-capacitor bank also abstains: sharing depends on each
part's frequency-dependent impedance, so assigning the full ripple to one
arbitrarily selected capacitor would be a false positive.

Topology itself is the first place the check can abstain, and that abstention is
now visible. Synchronous switching connectivity is **reversible**: the same
graph of FETs, inductor and caps is equally consistent with a buck and a boost,
so a direction is accepted only from explicit rail names (`VIN` / `VOUT`, or a
`*_IN` / `*_OUT` suffix). Numeric voltage ordering alone is not evidence. A stage
that cannot be oriented emits an info note naming the inductor, the switch node,
both rails, and the unlock (rename the rails to carry their role, or give the
controller a model declaring its topology), because dropping it silently would
produce byte-for-byte the same report as a board with no converter on it. Stages
that classify normally emit no such note.

### Calibration evidence (the hunt's mppt-1210-hus C1)

- Hand-checked unit test: `C1` (1200 uF, rated 3.0 A_rms at 100 kHz / 105 C)
  across a 10 A buck input at `D ~ 0.5` carries `0.5 * 10 = 5.0 A_rms`,
  ~1.66x its rating (`ripple::tests::mppt_1210_c1_overstress_is_1_66x`).
- End-to-end integration resolves the shipped exact MPN, a 10 A programmed
  operating load, discrete buck topology, and `VIN_5V -> VOUT_2V5`; it raises
  one 5.0 A_rms versus 3.0 A_rms finding. The paired `VIN -> VOUT` fixture has
  the same exact rating and current but no voltage evidence, and must abstain. A
  second paired fixture adds a parallel input bulk cap and pins the impedance-
  sharing abstention.
- On the real mppt-1210 board, `--si` recovers the buck stage (input rail
  SOLAR+, switch node SW_NODE, inductor L1, and the input capacitor bank) and
  honestly records that its parallel-cap impedance sharing is unknown. The
  layout also does not establish `I_out` or nominal duty (the 10 A charge
  current is a system spec, not a part rating), so it emits an info note rather
  than a fabricated finding. The over-ripple physics is the hand-checked unit
  test's job. The board path proves topology recovery and honest abstention; the
  synthetic integration proves the automated positive path without importing
  the private hunt board into the model database.
- Corpus sweep raises no ripple findings on the known-good famous boards.

---

## Reproduce

```bash
cd hauksbee
BIN=target/release/hauksbee; cargo build --release -p hauksbee-engine
C=../board-corpus/famous

# Per-board SI report (findings + the auditable info notes).
$BIN run "$C/rp2040_minimal_kicad/minimal/RP2040_minimal_r2/RP2040_minimal_r2.kicad_pcb" --si
$BIN run "$C/zswatch_mainboard/watch/ZSWatch-Watch.kicad_pcb" --si
$BIN run "$C/olimex_esp32/HARDWARE/REV-L/ESP32-EVB_Rev_L.kicad_pcb" --si

# Controlled-impedance estimate on a board WITH a stackup (Watchy: computes the
# real Zdiff, info because the board declares dielectric_constraints no).
$BIN run "$C/watchy/Watchy.kicad_pcb" --si | grep controlled_impedance

# The same check on a board that DOES declare dielectric_constraints yes:
# MokyaLora's RF feed network fires 1 medium + 3 high, its USB pair stays info.
$BIN run "$C/hunt/mokyalora/hardware/kicad/MokyaLora.kicad_pcb" --si | grep controlled_impedance

# Trace ampacity + input-cap ripple (checks 6, 7): the mppt-1210 buck stage is
# recovered and its input bulk cap C1 is reported (I_out not auto-attributable,
# so an honest info note, not a fabricated finding).
$BIN run ../hunt-boards/mppt-1210-hus/kicad/mppt-1210-hus.kicad_pcb --si | grep -E 'ripple|ampacity'

# The calibration guards over the whole corpus (KiCad + Eagle). The two Olimex
# antenna_keepout tests are #[ignore]d while that question is open, so the
# zero-false-positive gate itself only runs under `-- --ignored`, and reports the
# 11 Olimex findings when it does.
HAUKSBEE_REQUIRE_CORPUS=1 cargo test -p hauksbee-extract --test si_corpus
HAUKSBEE_REQUIRE_CORPUS=1 cargo test -p hauksbee-extract --test si_corpus -- --ignored

# The hand-checked impedance formulas vs the reference calculator (unit tests).
cargo test -p hauksbee-extract --lib si::

# Trace-ampacity + input-cap ripple integration + corpus sweep (checks 6, 7),
# including the hand-checked mppt-1210 C1 1.66x ripple unit test.
HAUKSBEE_REQUIRE_CORPUS=1 cargo test -p hauksbee-engine --test si_ampacity_ripple
cargo test -p hauksbee-engine --lib checks::ripple checks::ampacity checks::converter
```
