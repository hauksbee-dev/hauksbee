# Steady-state thermal: junction temperature from dissipation

The stress monitor already knows how much power every device dissipates at the
operating point, and whether that exceeds its rated power. The thermal monitor
takes the next step: it turns dissipation into a **junction temperature** and
flags parts that run too hot. "Over its rated power" becomes "what junction
temperature does it reach, and is that over its limit".

This is the first-order, steady-state estimate every datasheet thermal section
starts from:

```
Tj = Tambient + P_dissipated * theta_JA
```

- `P_dissipated` (W): the device's power dissipation at the operating point, the
  same number the over-power check already computes.
- `theta_JA` (C/W): junction-to-ambient thermal resistance, still air, no
  heatsink, on a typical board.
- `Tambient` (C): the air around the board. Default 25 C (the datasheet
  reference ambient), overridable per run.

When `Tj` exceeds the device's maximum junction temperature, the monitor raises
an **`overtemperature`** fault through the same fault channel as over-current,
over-power, and the rest, so it surfaces in the live UI, in `check-code`, and as
a `hauksbee-ci` assertion with no new plumbing.

## Where the numbers come from

### theta_JA defaults by package class

If a part's model entry carries an explicit `theta_ja_c_per_w`, that wins.
Otherwise the monitor derives one from the footprint's package body. The
defaults are representative **still-air, single-layer/typical-board** datasheet
figures, chosen on the *pessimistic* (hotter) side of each package's published
range, because over-estimating temperature is the safe direction for a screening
check and real boards rarely beat the best-case JEDEC 2s2p numbers. Sources are
the package thermal sections of the parts that use each body, cross-checked
against JEDEC JESD51 and the Vishay / onsemi / TI / Nexperia package notes.

| Package           | theta_JA (C/W) | source / note                                    |
|-------------------|----------------|--------------------------------------------------|
| SOT-23 / SOT-23-3 | 250            | Nexperia / onsemi SOT-23 small-signal, still air |
| SOT-23-5/6, SOT-353/363 | 220      | slightly more pin/copper than the 3-pin body     |
| SOT-89            | 140            | onsemi SOT-89                                    |
| SOT-223           | 65             | onsemi NCP1117 SOT-223, tab to ~1 in^2 copper    |
| SOIC-8            | 120            | TI SOIC (D) 8-pin, JEDEC low-K                   |
| SOIC-14/16        | 90             | larger SOIC body                                 |
| TSSOP / MSOP      | 150            | TI PW / DGK, JEDEC low-K                          |
| DPAK (TO-252)     | 70             | onsemi DPAK to ~1 in^2 copper                    |
| D2PAK (TO-263)    | 50             | onsemi D2PAK to copper                           |
| TO-220            | 62             | onsemi TO-220 **free air** (no heatsink)         |
| TO-92             | 200            | onsemi TO-92 still air                           |
| SOD-123 (diode)   | 340            | Vishay SOD-123 small-signal diode                |
| SOD-323 / SOD-523 | 450            | smaller diode bodies run hotter                  |
| SMA (DO-214AC)    | 90             | Vishay SMA rectifier to recommended pad          |
| SMB (DO-214AA)    | 75             | Vishay SMB rectifier to recommended pad          |
| SMC (DO-214AB)    | 60             | Vishay SMC rectifier to recommended pad          |
| DO-41 / DO-201 / DO-35 | 100       | through-hole axial rectifier / small-signal, free air |
| QFN / DFN         | 50             | bottom thermal pad to copper                     |
| LQFP / TQFP / QFP | 60             | moderate leadframe body                          |
| (unrecognised)    | 200            | conservative small-SMD fallback                  |

Chip resistors and capacitors use a **size-derived** theta_JA, because a hot
0402 with no copper flood genuinely runs ~600 C/W while a 2512 is nearer
~80 C/W. These bracket the chip-resistor power deratings the stress monitor
already encodes (a 0402 at 1/16 W reaching its rated rise is the same physics):

| Chip size | theta_JA (C/W) |
|-----------|----------------|
| 01005     | 1200           |
| 0201      | 900            |
| 0402      | 600            |
| 0603      | 400            |
| 0805      | 300            |
| 1206      | 220            |
| 1210      | 160            |
| 2010      | 120            |
| 2512      | 80             |

The size token is matched smallest-body-first, and 01005 has to be tested before
0402: KiCad names a chip footprint with the imperial code beside its metric twin,
and 01005's metric code *is* 0402 (`R_01005_0402Metric`), so a plain substring
search would hand the smallest, worst-cooling body the 0402 figure of 600 and
under-estimate its temperature.

### Chip-resistor power rating (and why the fallback is 1/16 W)

An explicit `ratings.max_power_w` always wins. Otherwise the rating comes from
the same imperial size token, smallest-body-first for the same metric-collision
reason: 01005 1/32 W, 0201 1/20 W, 0402 1/16 W, 0603 1/10 W, 0805 1/8 W,
1206 1/4 W, 1210 1/2 W, 2010 3/4 W, 2512 1 W.

The unrecognised case is split three ways, because a single fallback cannot be
conservative for both surface-mount and through-hole parts:

| Footprint evidence | Rating | Basis |
|--------------------|--------|-------|
| Recognised imperial chip size code | the table above | `ChipPackage` |
| Metric-only chip code (`R_3216Metric` = imperial 1206) | the imperial equivalent | `ChipPackage` |
| Recognised DIN axial body code | per the DIN table below | `ThtAxial` |
| Anything else, including an unrecognised SMD size, an axial body with no DIN code, or a `Power` axial | none derived | `Unknown` |

The DIN codes are size evidence and are **not** interchangeable, so each carries
its own rating: DIN0204 0.125 W, DIN0207 0.25 W, DIN0309 0.5 W, DIN0411 1 W,
DIN0414 2 W, DIN0516 3 W, DIN0617 5 W. A blanket 1/4 W for anything axial
over-rates a DIN0204 twofold and suppresses its overpower check, and under-rates
a DIN0411 fourfold. An axial footprint carrying no DIN code has no size evidence
at all (the codes span 0.125 W to 5 W) and abstains.

A metric-only name is read **before** the imperial pass, and a name carrying a
separate imperial token is left to it. That ordering is what keeps both cases
right: `R_0402Metric` is metric 0402, an imperial 01005 at 1/32 W (reading its
`0402` as imperial rates it 1/16 W, double its real limit), while KiCad's dual
form `R_0201_0603Metric` must be read from its imperial `0201` at 1/20 W, not
from the metric `0603`.

There is **no floor for an unrecognised size**, because no direction is
conservative. A 1/4 W default exceeds a real 0402 (1/16 W) by 4x and suppresses
genuine overpower findings. A 1/16 W floor undercuts everything above the
smallest and invents them: a real 0603 behind a custom footprint name dissipating
80 mW sits inside its 100 mW rating but outside a guessed 62.5 mW one. So the size
is either read or the part abstains and is named. A `Power` axial abstains for the
same reason (those bodies are 1 W and up).

**Only resistors get a footprint-derived wattage.** `ComponentKind::Passive` also
covers capacitors, inductors and ferrite beads, whose limits are current and
voltage rather than a chip-resistor wattage; handing an
`Inductor_SMD:L_0805_2012Metric` an 0805 resistor's 1/8 W would invent an
overpower fault out of ordinary coil heating, and the 1/16 W floor makes that
misfire easier to hit, not harder. `DeviceMeta::is_resistor_like` gates it on the
footprint library/body name (`Resistor_*`, `R_*`) with the reference-designator
prefix (`R`, `RN`, `RA`) as the fallback, and anything naming another passive
family is excluded outright. A non-resistor passive therefore gets no derived
rating and is not reported as an overpower coverage hole either.

A flat 1/4 W for everything unrecognised would not be conservative on an SMD
board: 1/4 W **exceeds** a real 0402 (1/16 W) by 4x and an 0603 (1/10 W) by
2.5x, so an overstressed chip resistor whose footprint string went unrecognised
would have its overpower check silently suppressed. The conservative floor for a
chip resistor is the smallest one anyone ships, 1/16 W. A through-hole axial body
genuinely is 1/4 W, so it keeps that figure; applying the chip floor there would
invent overpower faults on correct designs.

When the footprint says neither, nothing is derived. Guessing would be wrong in
opposite directions, so the device is reported instead: `StressMonitor::
power_coverage_gaps` names the affected parts and the unlock (a model with
`ratings.max_power_w`, or a footprint / BOM line naming the package). It reports
**one note per unreadable package**, with a count and up to five representative
references, because a board carrying fifty resistors from one unparseable library
is one coverage hole with fifty instances and fifty near-identical notes is how an
honesty channel stops being read. The CI
report carries it in `coverage_warnings` alongside the co-sim coverage holes, and
`BoundBoard::power_coverage_gaps` feeds the same sentence into the evidence map
on the `run` / `--plain` / `--json` / TUI surfaces, so the gap is not visible in
CI alone. An overpower check that did not run is a visible gap, not a pass.

`theta_jc_c_per_w` (junction-to-case) can also be carried in the model DB. It
is informational today. The free-air estimate uses `theta_JA`. A heatsinked
path (`theta_JC + theta_CS + theta_SA`) is a future extension.

### Maximum junction temperature

Each device's max Tj comes from `max_junction_temp_c` in the model DB when
present. When absent, the monitor applies a per-package-class default:

- **150 C** for power packages (TO-220, DPAK, D2PAK, SOT-223, SMA/SMB/SMC) that
  exist to dissipate.
- **125 C** for everything else (the common industrial discrete / passive / IC
  limit).

### Ambient

`Tambient` defaults to **25 C**, the "TA = 25 C" reference most ratings are
quoted at. Override it:

- CLI: `hauksbee run <board> --thermal --ambient 70`
- CLI: `hauksbee check-code <code> --ambient 70`
- CI: the top-level `ambient_c = 70` key in the spec.

## Board-level spreading: what this does and does not do

**This is a per-device theta_JA model, not a board thermal field solve.** Each
part is treated as an isolated lump dumping its heat to a single fixed ambient
through one thermal resistance. That is honest and defensible for a screening
check, and it is what a datasheet's `theta_JA` figure already bakes in (the
"ambient" in `theta_JA` is the air a little distance from the part, on a
reference board).

What it deliberately does **not** capture:

- **Neighbor coupling / heat spreading.** A hot regulator next to a
  small-signal transistor will, in reality, raise that transistor's case
  temperature. This model does not propagate one part's heat into another's
  ambient. Doing that properly is a board thermal field problem (a thermal
  FEM/CFD over the copper, the dielectric, the air), and faking a coupling
  coefficient from placement distance would invent numbers with no datasheet
  behind them. The model does not fake a field solve.
- **Copper-pour / layer-count effects.** The `theta_JA` defaults assume a typical
  board. A massive ground pour under a DPAK, or a 4-layer stackup, lowers the
  real theta_JA below these still-air figures (the estimate is then
  pessimistic). A bare 1-layer board with thin traces runs hotter. Carry a
  measured `theta_ja_c_per_w` per part when the board's copper is known.
- **Transient thermal.** There is no thermal mass / time constant here. The
  estimate is the *steady state* the part settles to if the dissipation holds.
  A brief power spike on a switching edge does not heat a junction, which is
  exactly why the over-temperature check is treated as a **sustained** rating
  (it must persist across several solver chunks before it trips), the same way
  the continuous over-current check is.
- **Duty-cycled dissipation is time-weighted, not sampled.** The
  temperature-driving power is the device's dissipation integrated over the
  solver's accepted steps within each chunk, divided by the chunk's simulated
  time. A firmware PWM waveform that switches inside a chunk therefore heats
  by its duty-cycle average (a 25% duty deposits 25% of the always-on energy,
  whatever the chunk width or pulse phase), which is what the junction
  physically averages when the PWM period is short against the thermal time
  constant. Sampling the chunk endpoint instead would read the full peak or
  zero depending on phase. For a multi-unit package the siblings' integrated
  energies pool before the shared theta_JA is applied. The continuous
  Overpower rating consumes the same per-unit time-weighted average, since a
  wattage rating is the same heating physics under another name.

If you need true board spreading or transient junction response, that is a
thermal FEM/CFD coupled to the electrical solve, and it is out of scope for this
estimator by design.

## Surfaces

- **`hauksbee run <board> --thermal [--ambient C]`**: runs a short headless
  co-sim and prints a per-device junction-temperature table, marking any part
  over its limit. Informational (exits 0).
- **`hauksbee check-code <code> [--ambient C]`**: the over-temperature fault
  shows up in the fault report alongside the others. A destroyed part fails
  the check.
- **`hauksbee-ci`**: the `max_temp` assertion (per-device or explicit ceiling)
  and the top-level `ambient_c` key. The `no_faults` assertion also catches
  over-temperature. See [CI.md](../ci/CI.md).
- **Live UI**: an over-temperature fault rides the frame's fault list and lands
  in the fault overlay, carrying the component, the temperature it reached, and
  the limit it crossed. The junction temperature of every *other* part is not on
  the wire: `crates/hauksbee-server/src/protocol.rs` has no per-component
  temperature field, so the UI sees the parts that went over, not a live
  heat-map. A heat-map needs a protocol addition first.

## Worked check

A 30 ohm 2512 power resistor across a 5 V rail dissipates `5^2 / 30 = 0.833 W`.
In a 2512 (theta_JA = 80 C/W):

- at 25 C ambient: `Tj = 25 + 0.833 * 80 = 91.7 C`, within the 125 C limit.
- at 90 C ambient: `Tj = 90 + 0.833 * 80 = 156.7 C`, over the 125 C limit, so
  the monitor raises `overtemperature`.

This is the two-sided example shipped as
`crates/hauksbee-ci/examples/power_resistor_cool.toml` (green) and
`power_resistor_hot.toml` (red), and the canonical hand-check in the test suite
(`0.5 W in a SOT-23 at 250 C/W in 25 C ambient = 150 C exactly`).
