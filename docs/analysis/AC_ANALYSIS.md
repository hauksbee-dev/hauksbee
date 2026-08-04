# AC / small-signal analysis

Hauksbee can run a standard SPICE `.AC` small-signal analysis: linearize the
circuit about its DC operating point, then solve the complex MNA system across a
frequency sweep and report magnitude (dB) and phase at any node. On top of that
it computes loop-stability metrics (Bode data, gain crossover, phase margin) so
loop stability can be a CI check.

This is the thing power-supply (SMPS) and analog engineers reach for first:
Bode magnitude/phase, phase margin, gain crossover, and frequency response.

## The model

At each frequency `f` (angular frequency `w = 2*pi*f`) the solver assembles a
complex modified-nodal-analysis system

```
(G + jwC) x = b
```

and solves for the complex node-voltage phasors `x`. `G` is the real
conductance/transconductance backbone, `jwC` the reactive part, and `b` the AC
stimulus. The system is built from each device's **small-signal stamp**,
linearized at the DC operating point:

| Device | Small-signal stamp |
|---|---|
| Resistor | real conductance `G = 1/R` (temperature-adjusted if enabled) |
| Capacitor | admittance `jwC` |
| Inductor | MNA branch row `v = jwL * i` |
| Independent V source | its AC drive amplitude/phase on its branch row |
| Independent I source | its AC drive amplitude/phase injected `p -> n` |
| Diode | `gd = dI/dVd` at the bias |
| BJT | Gummel-Poon tangents at the bias: `gpi`, `gmu`, `go`, transconductance `gm` |
| MOSFET | `gm`, `gds` at the bias (triode / saturation / subthreshold) |
| Op-amp (behavioral) | `out = gain*(vp - vn)` through the output stage, open when rail-pinned at the OP |
| V-switch | quiescent conductance at the OP control voltage, plus the control transconductance `gm = (v_a - v_b) * dgsw/dvctrl` |
| Comparator | digital output: no small-signal path |

### What is linearized

The active-device conductances and transconductances are exactly the Newton
tangents the transient solver already computes (`dI/dV`, `dI/dV_control`),
frozen at the converged DC operating point. AC reuses:

- the same MNA `Layout` (node + branch unknown numbering),
- the same `dc_operating_point` solve (real Newton with gmin / source-stepping
  homotopy),

and extends only the linear-algebra step to complex. The DC OP is computed
once per sweep. Only the per-frequency complex assemble-and-solve repeats.

Initial conditions are deliberately ignored. AC enters the solver through
`dc_operating_point_no_ic`, which forces `use_ic = false`, matching SPICE `.ac`
(which always linearizes about the ordinary DC bias and ignores `.ic` / UIC).
The plain `dc_operating_point` entry point honours an IC whenever any cap or
inductor carries one, which pins a capacitor to a short and an inductor's
branch current to a fixed value: the transient *initial state*, not the
steady-state bias. Reusing it would evaluate every nonlinear tangent (`gd`,
`gm`/`gpi`/`go`, `gds`) at the wrong point and silently corrupt the Bode
response.

### Which sources drive

The RHS `b` is decided by three rules, checked in order (`solve_at` in
`crates/hauksbee-solve/src/ac.rs`):

1. **Explicit `AC` cards win.** If any source in the deck carries an
   `AC <mag> [phase]` token, that is the authoritative excitation: each listed
   source drives its own complex amplitude `mag * exp(j*phase)` and every other
   source is AC-grounded. A bare `AC` with no numbers means magnitude 1,
   phase 0, the SPICE convention. This is parsed off the source card by
   `extract_ac_spec` in `crates/hauksbee-ir/src/spice.rs` and carried on the
   circuit as `ac_stimulus`.
2. **Otherwise a dedicated injection source drives alone.** A `Vsource` or
   `Isource` named `VINJ`, `VLOOP`, `VAC`, `IINJ`, `ILOOP`, or `IAC` (or any
   name containing `_VINJ`, `_VLOOP`, `_IINJ`, or `_ILOOP`; the match is
   case-insensitive) is recognized as the chosen injection point: it drives
   `1 + 0j` and every other source,
   including the DC bias rails, is AC-grounded. That gives a true single-input
   transfer function without writing a SPICE deck.
3. **Otherwise every independent source is driven at `1 + 0j`.** This is the
   fallback for an extracted board that names no injection point, and the
   result is a superposition of all stimuli, not a single-input transfer
   function. `hauksbee run --ac` says so on stderr (text output; suppressed
   under `--json`) rather than letting it pass as a transfer function:

   ```
   NOTE: no dedicated AC injection source: the sweep drove every independent
   source (power rails included), so this Bode is a superposition, not a
   single-input transfer function. To measure a real transfer function, name
   the drive source VINJ/VLOOP/IINJ/ILOOP (insert one at the input/loop with
   board-as-code: https://docs.hauksbee.dev/docs/ingest/board_as_code), then re-run --ac.
   ```

### Complex solver

The complex system is solved with a dense LU with partial pivoting over
`num_complex::Complex64` (`ComplexSystem` in `hauksbee-solve`). Partial
pivoting is mandatory because MNA voltage-source / inductor rows have a
structurally zero diagonal. This is the same algorithm LAPACK's `zgesv` uses.
For Hauksbee's board sizes (tens to low hundreds of unknowns, one solve per
frequency), dense `O(n^3)` is fast and easy to trust. The real sparse
Gilbert-Peierls LU is left untouched.

## Loop stability and loop injection

Loop-gain measurement needs the loop broken at one point and a small signal
injected there. Hauksbee follows the standard SPICE single-injection practice:

1. The board author breaks the feedback net at a low-impedance-driving /
   high-impedance-load node (the normal case is an op-amp / error-amp feedback
   divider tap) and inserts a `0 V` injection `Vsource`, e.g. `VLOOP`.
2. Because that name is recognized as a dedicated injection source, the AC
   analysis drives it alone and AC-grounds the bias rails, so the node phasor
   on the far side of the break is the open-loop response to the injection.
3. The loop gain is the negative of the return ratio at the break:

   ```
   T(jw) = -V_out(jw) / V_inj
   ```

   The minus sign is the summing-junction convention (the returned signal is
   subtracted at the error node), so a stable single-pole loop reads ~+90 deg
   phase margin rather than appearing inverted. `LoopStability::from_response`
   applies this. Magnitude is unchanged, only phase carries the 180 deg.

From the resulting Bode data it computes:

- **DC / low-frequency loop gain**: the magnitude (dB) at the first swept point,
  reported unconditionally so a loop that never reaches 0 dB in band still says
  how much gain it started with.
- **Gain crossover frequency** `f_c`: the **highest-frequency downward** `0 dB`
  crossing of `|T|`, that is, the last sweep interval where the magnitude goes
  from `> 0 dB` to `<= 0 dB` as frequency rises, by linear interpolation in
  log-frequency / dB between the bracketing points. A non-monotonic loop can
  cross unity several times (dip below, peak back above); the final descent
  through unity is the one that governs stability, so taking the first crossing
  would report an optimistic margin. A loop that never crosses downward (always
  above, always below, or only rising through unity at the band edge) reports no
  crossover.
- **Phase margin**: `180 deg + phase(T)` at `f_c`. `>= 45 deg` is the usual bar.
- **Phase crossover frequency** and **gain margin**: every `-180 deg` crossing of
  the unwrapped phase is collected (either direction), and the margin is read at
  the **lowest-frequency** crossing at or above `f_c`, the first point past unity
  gain where extra loop gain would push the response onto the critical point. If
  no crossing lies at or above `f_c` (or the gain never crosses unity), it falls
  back to the lowest-frequency crossing, which errs conservative for a
  conditionally stable loop.

Phase is unwrapped before the margin search, so a sweep that winds past -180 deg
is handled correctly.

`--ac-loop` prints them together (text surface only). The DC/low-f gain line is
always there; the crossover pairs collapse to one honest line each when the
response never reaches the crossing. From
`--ac 100:1e5:2 --ac-loop D13` on `blinky.kicad_pcb`, a node with a rolling-off
response and no feedback path:

```
Loop stability at net 'D13':
  DC/low-f loop gain : -4.12 dB
  gain crossover     : none in band (loop never reaches 0 dB)
  phase crossover    : none in band (phase never reaches -180 deg)
```

## CLI

```
hauksbee run <board> --ac <fstart>:<fstop>:<points>[:dec|:oct|:lin] \
    [--ac-node NET]... [--ac-csv FILE] [--ac-loop NET]
```

- `--ac 10:1e6:20` sweeps 10 Hz to 1 MHz at 20 points per decade (the default
  spacing; see the sweep forms below).
- `--ac-node OUT` reports the Bode table for net `OUT` (repeatable, defaults to
  every non-ground net).
- `--ac-csv sweep.csv` writes `net,freq_hz,mag_db,phase_deg` rows.
- `--ac-loop FB` additionally reports gain crossover, phase margin, phase
  crossover, and gain margin for net `FB`.

Example:

```
hauksbee run regulator.kicad_pcb --ac 10:1e6:50 --ac-node VOUT --ac-loop FB
```

### Sweep forms

Three spacings, the SPICE set. On the CLI the form is the optional fourth field
of `--ac`; in a SPICE deck it is the first argument of the `.ac` card.

| Form | What `points` means | CLI | SPICE card |
|---|---|---|---|
| `dec` (default) | points per decade, geometric spacing | `--ac 10:1e6:20` or `--ac 10:1e6:20:dec` | `.ac dec 20 10 1e6` |
| `oct` | points per octave, geometric spacing | `--ac 20:20e3:5:oct` | `.ac oct 5 20 20e3` |
| `lin` | total points over the band, linear spacing | `--ac 10:1e6:100:lin` | `.ac lin 100 10 1e6` |

`dec` and `oct` differ only in the base of the geometric step, 10 against 2, so
`oct` at `n` points per octave lands roughly `3.32n` points per decade. Both pin
the last point to `fstop`, so a band that is not a whole number of decades or
octaves still includes its endpoint. Anything else is refused by name rather than
falling back to a default: the deck form reports an unknown `.ac` sweep type, the
CLI form reports `unknown sweep mode '<x>' (dec|oct|lin)`.

The CI `[ac]` block takes `dec` or `lin` only; `oct` is refused there.

## CI

A spec adds an `[ac]` sweep block and `phase_margin` / `ac_gain` assertions. The
AC analysis is seed-independent (it linearizes about the DC operating point), so
it is computed once on the biased circuit (overrides, supplies, and net drives
applied so the bias matches the run) and shared across fuzz seeds.

```toml
board = "hardware/smps.kicad_pcb"
duration_ms = 1

[ac]
fstart = 10.0       # Hz
fstop  = 1e6        # Hz
points = 20         # per decade (dec) or total (lin)
sweep  = "dec"      # "dec" | "lin"

# Loop stability gate: the feedback loop must have >= 45 deg phase margin.
[[assert]]
kind = "phase_margin"
net  = "FB"          # the loop break / output net
min  = 45

# Frequency-response gate: the filter must be >= -3 dB at 1 kHz.
[[assert]]
kind = "ac_gain"
net  = "OUT"
min  = -3.0
freq_hz = 1000
```

`phase_margin` fails (red) if the loop never crosses 0 dB or the margin is
outside `[min, max]`. `ac_gain` bounds the magnitude (dB) at a frequency (log
interpolated) or, with no `freq_hz`, over the whole band.

## Analytic validation

The solver is validated against closed-form responses to tight tolerances
(`crates/hauksbee-solve/tests/ac_validation.rs`). These are the confidence bar.

| Check | Closed form | Tolerance | Result |
|---|---|---|---|
| RC low-pass at corner | `-3.0103 dB`, `-45 deg` at `f = 1/(2*pi*RC)` | 1e-3 dB / 1e-3 deg | pass |
| RC low-pass rolloff | `-20 dB/decade` above the corner | 0.1 dB/dec | pass |
| RC low-pass full sweep | `H = 1/(1 + jwRC)`, 20 pts/dec, 10 Hz..1 MHz | 1e-6 in `|H|`, 1e-3 deg | pass |
| Series RLC peak (V across C) | peak at `f0*sqrt(1 - 1/(2Q^2))`, peak `|H| = Q/sqrt(1 - 1/(4Q^2))` | 2% freq, 0.2 dB | pass |
| Series RLC at `f0` | `|H(f0)| = Q`, phase `-90 deg` | 1e-4 rel, 1e-2 deg | pass |
| Series LC notch | deep notch `|V| -> 0` at `f0` | `< 1e-6` | pass |
| Op-amp single-pole loop | `f_c ~ A0*fp` (10 MHz), phase margin `~90 deg` | 10% on `f_c`, `80..100 deg` | pass |

The op-amp loop uses a single dominant pole (`A0 = 1e5`, `fp = 100 Hz`). A
single pole gives ~90 deg phase margin, the textbook unconditionally-stable
case. The CI surface is exercised end-to-end on the same representative
compensated-feedback loop in `crates/hauksbee-ci/tests/ac_stability.rs`: a
`phase_margin >= 45` assertion passes, and `phase_margin >= 120` (impossible
for a single pole) fails with the measured margin reported.

### Demonstration on real corpus hardware

The AC solver runs end-to-end on real corpus boards (the DC operating point
solves and the complex sweep completes). For example:

```
hauksbee run crates/hauksbee-ci/examples/boards/blinky.kicad_pcb --ac 100:1e5:2
```

Blinky names no injection source, so this is the rule-3 superposition sweep and
the CLI says so on stderr. Two of the reported nets:

```
AC sweep: net '+5V' (7 points)
┌────────────────┬───────────────┬───────────────┐
│ Freq (Hz)      │ Mag (dB)      │ Phase (deg)   │
├────────────────┼───────────────┼───────────────┤
│       100.0000 │       -0.0000 │         0.000 │
│       316.2278 │       -0.0000 │         0.000 │
│      1000.0000 │       -0.0000 │         0.000 │
│      3162.2777 │       -0.0000 │         0.000 │
│     10000.0000 │       -0.0000 │         0.000 │
│     31622.7766 │       -0.0000 │         0.000 │
│    100000.0000 │       -0.0000 │         0.000 │
└────────────────┴───────────────┴───────────────┘

AC sweep: net 'D13' (7 points)
┌────────────────┬───────────────┬───────────────┐
│ Freq (Hz)      │ Mag (dB)      │ Phase (deg)   │
├────────────────┼───────────────┼───────────────┤
│       100.0000 │       -4.1249 │       -51.404 │
│       316.2278 │      -12.2524 │       -75.834 │
│      1000.0000 │      -22.0118 │       -85.436 │
│      3162.2777 │      -31.9870 │       -88.553 │
│     10000.0000 │      -41.9845 │       -89.540 │
│     31622.7766 │      -51.9842 │       -89.848 │
│    100000.0000 │      -61.9842 │       -89.931 │
└────────────────┴───────────────┴───────────────┘
```

(the full run also reports `ADC0`, `LED_A`, and the internal driver/supply nets.)

The `+5V` rail is flat at `0 dB`: it is one of the driven sources, so it sits at
the stimulus. `ADC0`, the R2/R3 divider tap, is flat at `-6.0206 dB` across the
whole band, the purely resistive `20*log10(1/2)`. `D13` is *not* flat: it is a
single-pole rolloff, `-4.12 dB` at 100 Hz falling through `-22.01 dB` at 1 kHz to
`-61.98 dB` at 100 kHz, a clean `-20 dB/decade`, with phase asymptoting to
`-89.9 deg`. The pole is the LED's junction capacitance (`cjo = 2 pF` in the
`led_red` model) against the very high impedance the node sees at the operating
point, where the reverse-biased diode's `gd` is negligible. Widening the sweep
down to `0.1 Hz` shows the flat `-0.03 dB` region and puts the corner near 80 Hz.

No bundled corpus
board exposes a compensated regulator feedback loop that binds to a *continuous*
small-signal model (corpus regulators bind as behavioral blocks), so the
loop-stability / phase-margin demonstration is on the **representative**
single-pole op-amp loop described above, explicitly labeled as representative.

## Honest limitations

- **Averaged, not switching-level.** AC linearizes about one DC operating point,
  so it sees the averaged small-signal behaviour. It does not capture
  cycle-by-cycle switching dynamics of an SMPS. For a switching converter, run
  AC on the averaged model, not the switching-level netlist.
- **Small-signal only.** It says nothing about large-signal stability,
  slew-limited recovery, or start-up behaviour. Use the transient solver for
  those.
- **Behavioral blocks.** The behavioral op-amp uses a single-gain tangent (no
  internal poles unless they are in the surrounding RC network). Comparators
  and digital outputs have no small-signal path and contribute only their
  quiescent conductance. Voltage switches do have a continuous small-signal
  model (the smooth tanh conductance is differentiable) and contribute their
  quiescent conductance plus the control transconductance; that modulation path
  can be dropped with `effects.switch_ctrl_gm = false`, which leaves only the
  quiescent conductance.
- **Operating-point dependence.** The accuracy of the active-device tangents is
  only as good as the DC operating point. A circuit whose DC solve does not
  converge has no AC analysis.
- **Unnamed drive falls back to superposition.** With no `AC` card and no
  recognized injection source, every independent source is driven at unit AC
  amplitude, so the reported node phasors are the response to that combined
  stimulus rather than a single-input transfer function. This is a default, not
  a ceiling: name the drive `VINJ` / `VLOOP` / `IINJ` / `ILOOP` or put an
  `AC <mag> [phase]` card on the source, and the sweep drives that source alone.
