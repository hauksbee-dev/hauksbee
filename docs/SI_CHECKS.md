# Signal-integrity static checks (`--si`)

Five pure-arithmetic, physics-grounded static checks over the data hauksbee
already extracts (copper geometry, netlists, part values, board stackup) plus a
small table of cited datasheet constants. Each targets a bug class that really
ships. They live in `crates/hauksbee-extract/src/si.rs` (+ the
`si/impedance.rs` submodule for check 5), run as `hauksbee run <board> --si`, and
follow the same calibration discipline as the rest of the tool
(`docs/FAMOUS_SWEEP.md`, `docs/KNOWN_FAULTS_VALIDATION.md`):

> **Zero false positives on the known-good corpus, or the check does not fire.**

Every check has an explicit *unknown -> info, never a fire* path, so a missing
datasheet constant or an out-of-reach geometry produces silence (or an
informational note carrying the computed value), never a confident false
positive. The `info` notes are observations on the record, not findings: they
make the negative auditable.

The checks were swept across the full corpus (KiCad `.kicad_pcb` + Eagle `.brd`,
60+ board files: Arduino, Adafruit, SparkFun, MNT Reform, Olimex ESP32-EVB +
Pico-PC, Watchy + history, ZSWatch mainboard + DevKit, LumenPnP, Corne, Lily58,
RP2040-minimal). **Result: zero findings on every known-good board.** Four tool
defects were found and fixed by chasing each surprise to the file (the Tarski
meta-lesson); they are recorded below.

Severity ladder: `high` (functional failure), `medium` (margin / robustness),
`low` (cosmetic), `info` (a computed value, not a finding).

---

## 1. Crystal load capacitance (`crystal_load_cap`)

### Model

A parallel-resonant crystal is specified for a load capacitance `CL`; the board
must present that `CL` across the crystal terminals for it to run on frequency.
The two external load caps `C1`, `C2` (one per terminal to ground) and the
parasitic stray `Cstray` combine as

```
CL_board = (C1 * C2) / (C1 + C2) + Cstray
```

The two caps are in series across the crystal (both return to ground); the stray
adds in parallel. `Cstray` folds the short PCB-trace parasitics and the MCU/IC
pin capacitance.

### Constants, thresholds and error band

- `Cstray = 4.0 pF` - the textbook midpoint (2-3 pF short trace + ~1-2 pF pin).
  Honest error band: roughly +-3 pF.
- `CL_TOLERANCE = 8.0 pF` - a finding requires the board CL to deviate from the
  crystal spec by more than 8 pF. This is set wide *on purpose*: a typical MHz
  crystal trims +-20..30 ppm, ~4-6 pF of CL slack near 18 pF, and the stray model
  carries +-3 pF; summing these, only past 8 pF can the deviation be real rather
  than model noise. So the model uncertainty can never produce the finding on its
  own.
- Severity: `high` when `CL_board < 0.5 * spec` or `> 1.6 * spec` (the oscillator
  may not start or is grossly off); `medium` otherwise.

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
`medium` (a real omission); a single missing cap is `info` (often the
series-resistor topology we could not trace).

Parts that integrate their own load caps are recognised and never flagged for
"missing caps":

- **Ceramic resonators** (Murata CSTxE / CERALOCK, ZTT; or a `RESONATOR`
  footprint): the 3-terminal centre pin is the integrated cap node.
- **RTCs with integrated oscillator caps**: PCF8523, PCF8563, PCF85063, RV-8263,
  RV-3028, DS3231.

### Calibration evidence

- RP2040 minimal `Y1 = ABM8-272-T3`, two 15 pF caps -> `CL_board ~ 11.5 pF` vs
  the 18 pF spec, within 8 pF: **info, ok** (the documented RP2040 hint).
- crkbd Corne (both split halves), Lily58, SparkFun Pro Micro, Arduino Uno Y1,
  MNT Reform Y1/Y2/Y3: value = frequency only -> **info, CL on the record, no
  fire**.
- Arduino Uno Y2 (CSTCE16M0V53), SparkFun RedBoard Y1 (RESONATOR-SMD): ceramic
  resonators -> **silent**.
- MNT Reform Y4 (32.768 kHz on PCF8523), Watchy Y1 (32.768 kHz on PCF8563): RTC
  integrated caps -> **silent**.

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

Bus capacitance is `Cbus = devices * 10 pF + trace_length * 1.0 pF/mm`:

- 10 pF per I2C device pin (a common datasheet figure; refinable per part).
- 1.0 pF/mm of routed trace (a conservative microstrip-over-plane figure; the
  geometry refinement is a documented future improvement - the device-count term
  is the floor used today).

This **upgrades** the existing netlint `missing_i2c_pullup` check from
"is a pull-up present" to "is the pull-up *sufficient*". No-pull-up is netlint's
job; this check only audits a bus that already has a pull.

### Threshold and mode inference

Mode inference is **conservative**: assume standard mode (1000 ns) unless the net
name encodes fast mode (`FM` / `FAST`). The check fires only when even the most
lenient assumed mode is violated, with `high` past 1.5x the limit and `medium`
otherwise. This is exactly what keeps it silent on the proven-good corpus buses.

### Calibration evidence

| Board / bus | Pull | Devices | `Cbus` | `t_r` | Verdict |
|-------------|------|---------|--------|-------|---------|
| Olimex UEXT (REV-L) | 2.2 kohm | 1 | ~10 pF | ~19 ns | ok |
| ZSWatch RTC (PCA9306+RV-8263) | 3.3 kohm | 2 | ~20 pF | ~56 ns | ok |
| ZSWatch Extension bus | 1.8 kohm | 8 | ~80 pF | ~122 ns | ok |

The ZSWatch 8-device Extension bus is the corpus's closest-to-the-limit I2C bus
and still sits ~8x under standard mode; the designers chose 1.8 kohm precisely
because of the device count. It is the discriminating no-fire (a regression that
mis-scaled the RC or counted the connector as a device would push it over and the
corpus test would go red).

Hand-checked physics: `0.8473 * 4700 * 100 pF = 398 ns` (the classic 4.7k/100pF
~ 400 ns); `0.8473 * 10000 * 250 pF = 2118 ns` (blows standard mode).

---

## 3. Antenna keepout (`antenna_keepout`)

### Model

A PCB-trace antenna (chip antenna, or the integrated antenna of an
ESP32-WROOM / nRF module) needs a copper-free, ground-free region around and
beyond it; ground plane or routed copper inside the keepout detunes the antenna
and absorbs radiated power. The symptom is poor range / sensitivity (the Watchy /
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

### Calibration evidence

The corpus's only ESP32-WROOM-32 (Olimex ESP32-EVB) mounts the module at the
board's top edge (board top at `y = 67.1`), so the 15 mm keepout band lands
**off the board** in free space and the check is correctly clear (`info`) - the
textbook-correct edge placement. The check ships as a calibrated guard: it fires
only on a genuine copper intrusion under/near the antenna (proven on a synthetic
board with a ground pour in the keepout), and is silent on the corpus.

---

## 4. USB differential pair (`usb_diff_pair`)

### Model

A USB D+/D- pair must be length-matched so the two edges arrive together;
intra-pair skew converts differential into common-mode and erodes the eye. The
check finds D+/D- pairs by net name (`D+/D-`, `DP/DM`, `USB_DP/USB_DM`, `UD+/UD-`,
etc.), sums each leg's routed discrete-trace length per layer (segment + arc
chord), and compares.

```
skew = | length(D+) - length(D-) |
```

### Thresholds

- **Full-speed** (12 Mbps): very tolerant. The 8.33 ns bit time swamps any sane
  board mismatch; we use a lenient 15 mm budget.
- **High-speed** (480 Mbps): tight, commonly <= 1.25 mm.

Since FS vs HS is not always inferable from the netlist, the **default is the
lenient FS limit**: the check fires `medium` only on a gross skew over 15 mm, and
otherwise reports `info` with the measured skew and the HS limit for reference.

Width / gap consistency is reported as an **info note only**: a diff pair necking
down at its connector/IC pad entry is universal and benign (e.g. the ZSWatch
DevKit's D- tapers 0.170 -> 0.127 mm at the USB-C pads), so it must never be a
fire.

### The honest reach

Routed discrete-trace length only; vias add a small out-of-plane length
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
  centreline gap between a D+ segment and a D- segment, minus the two
  half-widths). `Z0` is the single-ended microstrip impedance of one leg.

#### Validation (formula vs reference calculator)

| Case | W / S / H / T / Er (mm) | Tool | Reference calculator | Δ |
|------|--------------------------|------|----------------------|---|
| 50-ohm microstrip | W 0.3, H 0.2, T 0.035, Er 4.3 | **53.5 Ω** | 53.5 Ω (chemandy / mycalctools IPC-2141) | <0.1% |
| microstrip #2 | W 0.25, H 0.2, T 0.035, Er 4.3 | **59.2 Ω** | 59.3 Ω | <0.2% |
| wide-trace ~50 Ω | W 2.9, H 1.51, T 0.035, Er 4.5 | **48.1 Ω** | ~48 Ω | within band |
| stripline | W 0.15, H 0.5, T 0.035, Er 4.3 | **52.5 Ω** | 52.5 Ω (hand) | <0.1% |
| 90-ohm USB diff | W 0.3, S 0.2, H 0.2, T 0.035, Er 4.3 | **87.4 Ω** | 87.4 Ω (hand, NatSemi form) | exact |

(The prompt's "~0.3 mm on 0.2 mm FR4 Er 4.3 is roughly 50 ohm" lands at 53.5 Ω on
the IPC-2141 empirical form, the same number every published calculator returns;
the USB geometry W 0.3 / S 0.2 / H 0.2 gives 87.4 Ω, inside 90 Ω ±15%.)

### Stackup: what the board provides vs what is assumed

KiCad stores the stackup in `(setup (stackup ...))`: each physical layer carries
a `type` (`copper` / `core` / `prepreg` / mask / silk), `thickness`, and for
dielectrics `material` + `epsilon_r` + `loss_tangent`. The check reads the F.Cu
copper thickness `T`, the **first dielectric under F.Cu** as the microstrip
reference height `H`, and its `epsilon_r` as `Er`. (17 of the famous-corpus
`.kicad_pcb` files carry a full stackup; all are FR4, F.Cu 0.035 mm, Er 4.5, with
`H` from 0.196 mm on the 4-layer watch boards up to 1.51 mm on the 2-layer
keyboards.)

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

±15% is the looser of the two common fab tolerances (±10% / ±15%); the looser
band keeps the model's own few-percent error from ever producing the finding on
its own. A deviation past ±30% is `high` (the link will reflect / fail), 15-30%
is `medium`. Intra-pair skew and width necking are surfaced by check 4
(`usb_diff_pair`); this check adds the impedance.

The single-ended set is deliberately narrow (RF feedlines only): an ordinary GPIO
or a bare `CLK` is **not** assumed to be a 50 Ω controlled line, so it is never
judged.

### The two honesty gates (why it is silent on the whole corpus)

A deviation becomes a **finding** only when BOTH hold; anything short is an
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

**Every known-good corpus board sets `dielectric_constraints no`** (they chose
not to control these nets), so the check is silent across the whole corpus while
still computing and surfacing every impedance as an auditable `info` note. A board
that declares impedance control yet routes a pair out of band is the genuine bug
class this fires on.

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

- **Not a field solver.** Quasi-static closed-form only; no 2D/3D EM solve.
- **No co-planar-ground / ground-flanked term**, no via-stub or reference-plane-
  transition modelling - the single biggest source of the over-estimate above.
- **No crosstalk, no reflection / TDR, no insertion-loss / dielectric-loss sim**
  (the `loss_tangent` in the stackup is read but not yet used).
- **No FS-vs-HS inference**: it cannot tell whether a USB pair actually needs to
  be 90 Ω, which is exactly why the `dielectric_constraints` intent gate exists.
- Spacing is the median routed segment gap, not the designer's intended gap;
  arcs and poured copper are not used for the spacing measure.

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

# The zero-false-positive gate over the whole corpus (KiCad + Eagle), plus the
# hand-checked impedance formulas vs the reference calculator (unit tests).
HAUKSBEE_REQUIRE_CORPUS=1 cargo test -p hauksbee-extract --test si_corpus
cargo test -p hauksbee-extract --lib si::
```
