# Signal-integrity static checks (`--si`)

Seven closed-form checks over extracted geometry, netlist, part values and
stackup. Checks 1-5 live in `crates/hauksbee-extract/src/si.rs`; 6 and 7 in
`hauksbee-engine` `checks::ampacity` / `checks::ripple` (they need bound
models). All run with `hauksbee run <board> --si`; `--ampacity` prints the
trace-capacity table alone.

Severity: `high` (functional failure), `medium` (margin), `low`, `info` (a
computed value, never a finding). Every check has an "unknown -> info, never
a fire" path: a missing constant or out-of-reach geometry produces silence or
an info note. Under `--strict`, any non-info finding gates.

## 1. Crystal load capacitance (`crystal_load_cap`)

`CL_board = C1*C2/(C1+C2) + Cstray`, `Cstray = 4.0 pF`. A finding needs the
board CL to deviate from the crystal's spec by more than 8 pF (`high` below
0.5x or above 1.6x spec, else `medium`). The spec CL is known only from a
recognised part number (`ABM8-272` = 18 pF); a value that is only a frequency
yields an info note with the computed CL. Both load caps absent on a discrete
crystal is `medium`; one missing is `info`. Ceramic resonators (Murata
CSTxE/CERALOCK, ZTT, `RESONATOR` footprints) and RTCs with integrated caps
(PCF8523, PCF8563, PCF85063, RV-8263, RV-3028, DS3231, AB18) are never
flagged. Split-keyboard `r`-prefixed mirrors are handled.

## 2. I2C rise time (`i2c_rise_time`)

`t_r = 0.8473 * Rpull * Cbus`; limit 1000 ns standard mode, 300 ns fast mode
(assumed only when the net name carries `FM`/`FAST`).
`Cbus = devices * 10 pF + routed_length * C'`. With a declared stackup and a
bus that stays on F.Cu over a lower-layer pour, `C'` is computed from the
net's narrowest width (`sqrt(Er_eff)/(c0*Z0)`); otherwise the range 0.038 to
0.15 pF/mm is carried and the finding fires only when the **low** end already
exceeds the limit (`high` past 1.5x, else `medium`). When the pin term alone
exceeds the limit the finding carries full severity; when only the trace term
tips it, severity is capped at `medium`. Without a layout the routing term is
absent and the note says so. Audits sufficiency of a pull-up that exists;
absence is netlint's `missing_i2c_pullup`.

## 3. Antenna keepout (`antenna_keepout`)

Projects a cited keepout rectangle into board coordinates from the module's
placement and reports other nets' copper (segments, arcs, vias, zone fills,
foreign pads) inside it: `high` when a ground net intrudes, `medium`
otherwise. Table: ESP32-WROOM-32/32E/32D/32U, local `x[-9, 9]`,
`y[-20.3, -5.3]` (Espressif 15 mm keepout). Modules whose rectangle and
footprint origin could not be verified are not in the table and never fire.
Whether the 15 mm band is stricter than shipping practice on boards that
flood ground within it is an open hardware question; the check reports what
the datasheet says.

## 4. USB differential pair (`usb_diff_pair`)

Pairs found by name (`D+/D-`, `DP/DM`, `USB_DP/USB_DM`, `UD+/UD-`, ...);
routed discrete-trace length per leg (segments plus arc chords, no vias, no
pours); `skew = |len(D+) - len(D-)|`. FS vs HS is not inferable, so the
lenient full-speed budget applies: `medium` over 15 mm, else `info` with the
measured skew and the 1.25 mm HS reference. Width/gap necking is `info` only.

## 5. Controlled impedance (`controlled_impedance`)

Quasi-static closed forms (IPC-2141 microstrip and stripline, National
Semiconductor edge-coupled differential), matched to published calculators
within a fraction of a percent; not a field solve.

```
Z0    = (87 / sqrt(Er + 1.41)) * ln(5.98*H / (0.8*W + T))      microstrip
Z0    = (60 / sqrt(Er)) * ln(4*H / (0.67*pi*(0.8*W + T)))       stripline
Zdiff = 2*Z0 * (1 - 0.48 * exp(-0.96 * S / H))                  differential microstrip
```

`H`, `T`, `Er` come from KiCad's `(setup (stackup ...))`: F.Cu thickness and
the first dielectric below it. Without a stackup the check computes against an
`ASSUMED` default (1.51 mm FR4, 0.035 mm copper, Er 4.3) and reports `info`
only. Stripline is implemented but not auto-applied.

| Class | Detected by | Target | Tolerance |
|---|---|---|---|
| USB D+/D- | the diff-pair detector | 90 ohm differential | +-15 % |
| Ethernet / MDI | `TRD`/`TRX`/`MDI`/`MX0..3`/`ETH` with `_P`/`_N`/`+-` | 100 ohm differential | +-15 % |
| 50 ohm single-ended | RF feed names only (`RF`, `RF_IN/OUT`, `ANT*`) | 50 ohm | +-15 % |

A deviation is a finding (`high` past +-30 %, `medium` 15-30 %) only when:

1. the stackup is real (not the default), and
2. a reference plane is verified under a differential pair (points at 0.5 mm
   pitch tested against the adjacent layer's fill polygons; anti-pads are
   excused with the board's own clearance; a void must span >= 2 mm; a
   hatched pour or an absent fill reports `reference plane unverified`; a real
   void reports `reference missing under trace` instead of an impedance), and
3. the board declares `(stackup (dielectric_constraints yes))`.

Without gate 3 every impedance is surfaced as `info`. The closed form has no
co-planar-ground term, so on ground-flanked 4-layer routing it over-estimates
`Zdiff` by roughly 25-35 %; the intent gate is what keeps that from being a
false positive. No crosstalk, reflection, loss or FS/HS inference.

## 6. Trace ampacity (`trace_ampacity`)

IPC-2221, `I = k * dT^0.44 * A^0.725` (`k` = 0.048 outer, 0.024 inner),
applied at the net's **ampacity** bottleneck: the (layer, width) pair with
the lowest rating over every layer the net is routed on. Copper weight and
side come from the stackup (`thickness` per copper layer, 0.035 mm per oz;
`F.Cu`/`B.Cu` outer, `In<N>.Cu` inner, unrecognised names inner, position
otherwise). Without a stackup the weight defaults to 1 oz and the row is
labelled `ASSUMED`; **an assumed weight never produces a verdict** (an `info`
note saying "NOT a verdict"). Only a bottleneck whose weight the board
declares raises `high`.

A net carrying any copper zone is `Poured` and never rated. Only
`(segment)`/`(arc)` widths are read.

Current is attributed only from a `[models.current_program]` equation tagged
`semantics = "regulated_current"`
([MODELS.md](../models/MODELS.md#board-programmed-currents)), evaluated from
the fitted programming network; independent loads on one side of a rail are
summed, and a rail both sourced and consumed takes the larger directional
total. Direction comes from `current_in_roles`/`current_out_roles`. Converter
limits, regulator/connector/FET ratings, `protection_limit` equations and
generic placeholder models never seed a current. An unreadable programming
network attributes nothing and names the part and pin.

## 7. Input-capacitor ripple (`input_cap_ripple`)

For a buck stage recovered structurally (`checks::converter`: a switch node
tying a power FET to the inductor, with a bulk cap on the input rail),
`I_rms = I_out * sqrt(D - D^2)` with `D = Vout/Vin`, compared with the cap's
`ratings.max_ripple_current_a`. Fires only when topology, an exact
part-specific ripple rating (matched by MPN or value; the shipped example is
`EKYB630ELL122MLN3S`, 3.0 A at 100 kHz), an attributable `I_out`, and a duty
from rail names carrying exactly one voltage token each (`PWR_IN_12V`,
`CORE_OUT_3V3`) are all known. Buck vs boost direction is accepted only from
`VIN`/`VOUT` or `*_IN`/`*_OUT` names. A parallel input-cap bank, a missing
rating, or an unknown duty abstains with an info note.

## Reproduce

```bash
hauksbee run <board>.kicad_pcb --si
hauksbee run <board>.kicad_pcb --si --json | jq '.findings[] | select(.check=="si")'
cargo test -p hauksbee-extract --lib si::
cargo test -p hauksbee-engine --lib checks::ripple checks::ampacity checks::converter
cargo test -p hauksbee-engine --test it si_ampacity_ripple
```
