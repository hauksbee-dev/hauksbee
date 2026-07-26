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
| QFN / DFN         | 50             | bottom thermal pad to copper                     |
| LQFP / TQFP / QFP | 60             | moderate leadframe body                          |
| (unrecognised)    | 200            | conservative small-SMD fallback                  |

Chip resistors and capacitors use a **size-derived** theta_JA, because a hot
0402 with no copper flood genuinely runs ~600 C/W while a 2512 is nearer
~80 C/W. These bracket the chip-resistor power deratings the stress monitor
already encodes (a 0402 at 1/16 W reaching its rated rise is the same physics):

| Chip size | theta_JA (C/W) |
|-----------|----------------|
| 0201      | 900            |
| 0402      | 600            |
| 0603      | 400            |
| 0805      | 300            |
| 1206      | 220            |
| 1210      | 160            |
| 2010      | 120            |
| 2512      | 80             |

`theta_jc_c_per_w` (junction-to-case) can also be carried in the model DB. It is
informational today; the free-air estimate uses `theta_JA`. A heatsinked path
(`theta_JC + theta_CS + theta_SA`) is a future extension.

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

- **Neighbour coupling / heat spreading.** A hot regulator next to a small-signal
  transistor will, in reality, raise that transistor's case temperature. This
  model does not propagate one part's heat into another's ambient. Doing that
  properly is a board thermal field problem (a thermal FEM/CFD over the copper,
  the dielectric, the air), and faking a coupling coefficient from placement
  distance would invent numbers with no datasheet behind them. We do not fake a
  field solve.
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
  the continuous over-current check is. A device that dissipates only in short
  bursts will read its steady-state-at-peak temperature, which over-estimates;
  that is the safe direction.

If you need true board spreading or transient junction response, that is a
thermal FEM/CFD coupled to the electrical solve, and it is out of scope for this
estimator by design.

## Surfaces

- **`hauksbee run <board> --thermal [--ambient C]`**: runs a short headless
  co-sim and prints a per-device junction-temperature table, marking any part
  over its limit. Informational (exits 0).
- **`hauksbee check-code <code> [--ambient C]`**: the over-temperature fault
  shows up in the fault report alongside the others; a destroyed part fails the
  check.
- **`hauksbee-ci`**: the `max_temp` assertion (per-device or explicit ceiling)
  and the top-level `ambient_c` key. The `no_faults` assertion also catches
  over-temperature. See [CI.md](../ci/CI.md).
- **Live UI**: per-component junction temperature is exported next to the stress
  fraction for the heat-map, and over-temperature faults appear in the fault
  overlay.

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
