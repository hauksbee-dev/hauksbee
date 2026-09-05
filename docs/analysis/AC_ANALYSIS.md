# AC / small-signal analysis

Hauksbee runs a SPICE-style `.AC` analysis: linearise about the DC operating
point, solve the complex MNA system `(G + jwC) x = b` across a frequency sweep,
and report magnitude (dB) and phase per net. Loop-stability metrics (gain
crossover, phase margin, gain margin) make loop stability a CI check.

## CLI

```
hauksbee run <board> --ac <fstart>:<fstop>:<points>[:dec|:oct|:lin] \
    [--ac-node NET]... [--ac-csv FILE] [--ac-loop NET] [--json]
hauksbee run regulator.kicad_pcb --ac 10:1e6:50 --ac-node VOUT --ac-loop FB
hauksbee run <board> --list-nets          # find exact net names
hauksbee sim deck.cir --ac --print V(out) # a SPICE deck with an .ac card
```

| Flag | Meaning |
|---|---|
| `--ac 10:1e6:20` | 10 Hz to 1 MHz, 20 points per decade |
| `--ac-node OUT` | report this net (repeatable; default every non-ground net) |
| `--ac-csv sweep.csv` | write `net,freq_hz,mag_db,phase_deg` rows |
| `--ac-loop FB` | also report DC/low-f loop gain, gain crossover, phase margin, phase crossover, gain margin at `FB` (text only) |

Sweep forms (the fourth field; in a deck, the first argument of `.ac`):

| Form | `points` means | CLI | SPICE |
|---|---|---|---|
| `dec` (default) | per decade | `--ac 10:1e6:20` | `.ac dec 20 10 1e6` |
| `oct` | per octave | `--ac 20:20e3:5:oct` | `.ac oct 5 20 20e3` |
| `lin` | total, linear | `--ac 10:1e6:100:lin` | `.ac lin 100 10 1e6` |

The last point is pinned to `fstop`. An unknown mode is refused by name. The
CI `[ac]` block accepts `dec` or `lin` only.

JSON (`--ac --json`): `ac.valid` (+ `reason`), `ac.nets[] {net,
points:[[freq, mag_db, phase]]}`, `no_signal_path_nets[]`, `not_found_nets[]`,
`coverage`. A circuit whose DC solve does not converge has no AC result
(`valid:false`, exit 3).

## Which sources drive

Checked in order (`solve_at` in `crates/hauksbee-solve/src/ac.rs`):

1. **Explicit `AC` cards win.** Any source carrying `AC <mag> [phase]` drives
   `mag * exp(j*phase)`; every other source is AC-grounded. Bare `AC` means
   magnitude 1, phase 0.
2. **A dedicated injection source drives alone.** A `Vsource`/`Isource`
   named `VINJ`, `VLOOP`, `VAC`, `IINJ`, `ILOOP` or `IAC` (or containing
   `_VINJ`, `_VLOOP`, `_IINJ`, `_ILOOP`, case-insensitive) drives `1 + 0j`;
   everything else, bias rails included, is AC-grounded.
3. **Otherwise every independent source drives at `1 + 0j`.** The result is a
   superposition, not a transfer function, and the CLI says so on stderr
   (suppressed under `--json`).

## Loop stability

Break the feedback net at a low-impedance-driving / high-impedance-load point,
insert a 0 V `Vsource` named `VLOOP`, and pass the far side as `--ac-loop`.
The loop gain is `T(jw) = -V_out / V_inj` (summing-junction convention, so a
stable single-pole loop reads ~+90 deg margin). Reported:

- **DC / low-frequency loop gain**: magnitude at the first swept point, always.
- **Gain crossover `f_c`**: the highest-frequency *downward* 0 dB crossing
  (log-frequency interpolation). No downward crossing reports none.
- **Phase margin**: `180 deg + phase(T)` at `f_c`; >= 45 deg is the usual bar.
- **Phase crossover and gain margin**: read at the lowest-frequency -180 deg
  crossing at or above `f_c` (unwrapped phase), falling back to the lowest
  crossing overall.

```
Loop stability at net 'D13':
  DC/low-f loop gain : -4.12 dB
  gain crossover     : none in band (loop never reaches 0 dB)
  phase crossover    : none in band (phase never reaches -180 deg)
```

## CI

The sweep is seed-independent, computed once on the biased circuit
(overrides, supplies and net drives applied) and shared across fuzz seeds.

```toml
[ac]
fstart = 10.0       # Hz
fstop  = 1e6        # Hz
points = 20         # per decade (dec) or total (lin)
sweep  = "dec"      # "dec" | "lin"

[[assert]]
kind = "phase_margin"
net  = "FB"          # the loop break / output net
min  = 45            # and/or max, degrees

[[assert]]
kind = "ac_gain"
net  = "OUT"
min  = -3.0          # and/or max, dB
freq_hz = 1000       # omit to bound over the whole band
```

`phase_margin` is red if the loop never crosses 0 dB or the margin leaves
`[min, max]`. `ac_gain` bounds the magnitude at `freq_hz` (log-interpolated)
or over the band.

## Small-signal stamps

| Device | Stamp |
|---|---|
| Resistor | `1/R` (temperature-adjusted if enabled) |
| Capacitor / inductor | `jwC` / branch row `v = jwL i` |
| Independent sources | AC amplitude/phase per the rules above |
| Diode, BJT, MOSFET | Newton tangents at the operating point (`gd`; `gpi`, `gmu`, `go`, `gm`; `gm`, `gds`) |
| Behavioural op-amp | `out = gain*(vp - vn)`, open when rail-pinned |
| V-switch | quiescent conductance plus control transconductance (drop with `effects.switch_ctrl_gm = false`) |
| Comparator / digital | no small-signal path |

Initial conditions are ignored (`.ic` / UIC), matching SPICE `.ac`. The
complex system is solved by dense LU with partial pivoting.

## Limits

- Averaged, not switching-level: a converter is analysed on its averaged model.
- Small-signal only; large-signal stability, slew and start-up need the
  transient solver.
- The behavioural op-amp has a single-gain tangent and no internal poles.
- Accuracy is bounded by the DC operating point.
- Validated against closed forms (RC corner, -20 dB/decade, RLC peak and
  notch, single-pole op-amp loop) in `crates/hauksbee-solve/tests/ac_validation.rs`
  and end-to-end in `crates/hauksbee-ci/tests/ac_stability.rs`.
