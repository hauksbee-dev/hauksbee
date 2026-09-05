# Steady-state thermal: junction temperature from dissipation

```
Tj = Tambient + P_dissipated * theta_JA
```

`P_dissipated` is the device's dissipation at the operating point (the same
number the over-power check uses), `theta_JA` the junction-to-ambient thermal
resistance (C/W), `Tambient` the ambient (default 25 C). When `Tj` exceeds the
device's maximum junction temperature the stress monitor raises an
`overtemperature` fault through the same channel as over-current and
over-power.

## Surfaces

| Surface | Invocation |
|---|---|
| CLI report | `hauksbee run <board> --thermal [--ambient C]` |
| Board-as-Code | `hauksbee check-code <code> [--ambient C]` |
| CI | `[[assert]] kind = "max_temp"` plus top-level `ambient_c`; `no_faults` also catches over-temperature ([CI.md](../ci/CI.md)) |
| JSON | `thermal` section: `valid` (+ `reason`), `ambient_c`, `devices[] {reference, tj_c, over_limit}`, `coverage` |

`--thermal` runs a short headless co-sim and prints a per-device table. Exit
codes: 0 on a fully covered table; 3 when no dissipating device has a model;
3 **by default** on a PARTIAL table (rows exist while an active power IC on
the live circuit is open or unresolved), because it understates the load.
`--no-strict-thermal` opts out of that escalation (exit 0, caveat still
printed); `--strict-thermal` is accepted as a no-op.

## Where the numbers come from

**theta_JA**: `theta_ja_c_per_w` in the model entry wins. Otherwise a
package-class default, chosen on the pessimistic side of published ranges:

| Package | C/W | Package | C/W |
|---|---|---|---|
| SOT-23 | 250 | SOD-123 | 340 |
| SOT-23-5/6, SOT-353/363 | 220 | SOD-323 / SOD-523 | 450 |
| SOT-89 | 140 | SMA / SMB / SMC | 90 / 75 / 60 |
| SOT-223 | 65 | DO-41 / DO-201 / DO-35 | 100 |
| SOIC-8 | 120 | QFN / DFN | 50 |
| SOIC-14/16 | 90 | LQFP / TQFP / QFP | 60 |
| TSSOP / MSOP | 150 | TO-92 | 200 |
| DPAK / D2PAK / TO-220 | 70 / 50 / 62 | unrecognised | 200 |

Chip resistors and capacitors use a size-derived value: 01005 1200, 0201 900,
0402 600, 0603 400, 0805 300, 1206 220, 1210 160, 2010 120, 2512 80 C/W.
The imperial code is matched smallest-body-first (01005's metric code is
`0402`).

**Chip-resistor power rating**: `ratings.max_power_w` wins. Otherwise from the
imperial size code: 01005 1/32 W, 0201 1/20, 0402 1/16, 0603 1/10, 0805 1/8,
1206 1/4, 1210 1/2, 2010 3/4, 2512 1 W. A metric-only name (`R_3216Metric`)
maps to its imperial equivalent. DIN axial codes carry their own ratings
(DIN0204 0.125 W, DIN0207 0.25, DIN0309 0.5, DIN0411 1, DIN0414 2, DIN0516 3,
DIN0617 5). Anything else derives no rating and the part is named as a
coverage gap (`coverage_warnings` in CI, a coverage note in `run`), one note
per unreadable package. Only resistor-like parts (`Resistor_*` / `R_*`
footprints, or `R`/`RN`/`RA` prefixes) get a footprint-derived wattage.

`theta_jc_c_per_w` may be carried in the model DB but is informational.

**Maximum Tj**: `max_junction_temp_c` in the model DB, else 150 C for power
packages (TO-220, DPAK, D2PAK, SOT-223, SMA/SMB/SMC) and 125 C otherwise.

## What the model does not do

- No neighbour coupling or heat spreading: each part is an isolated lump into
  a fixed ambient. Carry a measured `theta_ja_c_per_w` when the copper is known.
- No transient thermal: the estimate is the steady state, so the fault is a
  sustained rating (it must persist across several solver chunks).
- Duty-cycled dissipation is time-weighted over each chunk (a 25 % PWM
  deposits 25 % of the always-on energy); multi-unit packages pool their
  units' energy before the shared theta_JA applies.

## Worked check

A 30 ohm 2512 across 5 V dissipates 0.833 W; at 80 C/W that is 91.7 C at
25 C ambient (within 125 C) and 156.7 C at 90 C ambient (over). Shipped as
`crates/hauksbee-ci/examples/power_resistor_cool.toml` (green) and
`power_resistor_hot.toml` (red).
