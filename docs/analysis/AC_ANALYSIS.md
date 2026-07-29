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
| Independent V source | unit AC drive (`1 + 0j`) on its branch row |
| Independent I source | unit AC current injected `p -> n` |
| Diode | `gd = dI/dVd` at the bias |
| BJT | Gummel-Poon tangents at the bias: `gpi`, `gmu`, `go`, transconductance `gm` |
| MOSFET | `gm`, `gds` at the bias (triode / saturation / subthreshold) |
| Op-amp (behavioral) | `out = gain*(vp - vn)` through the output stage, open when rail-pinned at the OP |
| V-switch | quiescent conductance at the OP control voltage |
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
2. The AC analysis drives every independent source with unit amplitude, so the
   node phasor on the far side of the break is the open-loop response to the
   injection.
3. The loop gain is the negative of the return ratio at the break:

   ```
   T(jw) = -V_out(jw) / V_inj
   ```

   The minus sign is the summing-junction convention (the returned signal is
   subtracted at the error node), so a stable single-pole loop reads ~+90 deg
   phase margin rather than appearing inverted. `LoopStability::from_response`
   applies this. Magnitude is unchanged, only phase carries the 180 deg.

From the resulting Bode data it computes:

- **Gain crossover frequency** `f_c`: where `|T| = 0 dB`, by linear interpolation
  in log-frequency / dB between the bracketing sweep points.
- **Phase margin**: `180 deg + phase(T)` at `f_c`. `>= 45 deg` is the usual bar.
- **Phase crossover frequency** and **gain margin**: where the (unwrapped) phase
  passes `-180 deg`, and how far below 0 dB the gain is there.

Phase is unwrapped before the margin search, so a sweep that winds past -180 deg
is handled correctly.

## CLI

```
hauksbee run <board> --ac <fstart>:<fstop>:<points>[:lin] \
    [--ac-node NET]... [--ac-csv FILE] [--ac-loop NET]
```

- `--ac 10:1e6:20` sweeps 10 Hz to 1 MHz at 20 points per decade (append `:lin`
  for linear spacing with `points` total points).
- `--ac-node OUT` reports the Bode table for net `OUT` (repeatable, defaults to
  every non-ground net).
- `--ac-csv sweep.csv` writes `net,freq_hz,mag_db,phase_deg` rows.
- `--ac-loop FB` additionally reports gain crossover, phase margin, phase
  crossover, and gain margin for net `FB`.

Example:

```
hauksbee run regulator.kicad_pcb --ac 10:1e6:50 --ac-node VOUT --ac-loop FB
```

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

reports a flat `0 dB` response on the unit-driven `+5V` rail and the resistive
`D13` LED node, as expected for a purely resistive divider. No bundled corpus
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
  quiescent conductance. Switches sit frozen at their OP conductance.
- **Operating-point dependence.** The accuracy of the active-device tangents is
  only as good as the DC operating point. A circuit whose DC solve does not
  converge has no AC analysis.
- **Single stimulus convention.** Every independent source is driven at unit AC
  amplitude, so the reported node phasors are the transfer function from that
  combined stimulus. For a clean single-input transfer function, drive the
  circuit from one source (the usual case) or break the loop with a dedicated
  injection source as described above.
```
