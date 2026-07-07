//! Independent source waveforms.
//!
//! Each [`SourceKind`] evaluates to a scalar at a given time, used identically
//! by voltage and current sources. The closed forms match ngspice's `sin`,
//! `pulse`, and `pwl` so cross-checks line up without surprises.

use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;

/// A source's small-signal AC stimulus, captured from its `AC <mag> [phase]`
/// spec (SPICE `.AC` drive). This is *not* a time-domain waveform: it is the
/// complex amplitude the linearized AC analysis injects at the swept frequency,
/// held constant across the sweep. Phase is in degrees, matching the SPICE
/// source-card convention.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AcStim {
    /// AC magnitude (V or A). SPICE defaults a bare `AC` token to 1.0.
    pub mag: f64,
    /// AC phase in degrees. Defaults to 0 when the card gives only a magnitude.
    pub phase_deg: f64,
}

/// One breakpoint of a piecewise-linear source.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PwlPoint {
    /// Time (s).
    pub t: f64,
    /// Value at `t` (V or A).
    pub v: f64,
}

/// An independent source's time behaviour.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SourceKind {
    /// Constant value.
    Dc(f64),
    /// `offset + amplitude * sin(2*pi*freq*(t - delay) + phase)` after `delay`,
    /// with exponential damping `theta`. Matches SPICE `SIN`.
    Sin {
        offset: f64,
        amplitude: f64,
        freq: f64,
        delay: f64,
        /// Damping factor (1/s); 0 means undamped.
        theta: f64,
        /// Phase in degrees.
        phase: f64,
    },
    /// Trapezoidal pulse. Matches SPICE `PULSE`.
    Pulse {
        /// Initial value before `delay` and the level returned to each period.
        v1: f64,
        /// Pulsed value.
        v2: f64,
        delay: f64,
        rise: f64,
        fall: f64,
        /// Time held at `v2` (excluding edges).
        width: f64,
        /// Full period; `<= 0` means a single, non-repeating pulse.
        period: f64,
    },
    /// Piecewise linear, held flat past the last point. Matches SPICE `PWL`.
    Pwl(Vec<PwlPoint>),
    /// A linear power-on ramp wrapped around another source. The inner
    /// waveform's AMPLITUDE is scaled by `min(t / scale_to, 1).max(0)`, so it
    /// grows linearly from zero at `t = 0` to full at `t = scale_to` and holds
    /// full thereafter. There is NO time shift: the ramp multiplies the
    /// amplitude in place over `[0, scale_to]`; it does not delay the inner
    /// waveform (a `Sin` still starts oscillating at `t = 0`, merely at
    /// vanishing amplitude). Used for pseudo-transient / power-on starts: bring
    /// every source up from zero so a circuit with no reachable DC operating
    /// point integrates from rest instead of failing a DC solve.
    Ramped {
        /// Time (s) at which the ramp reaches full amplitude. Expected `> 0`.
        scale_to: f64,
        /// The waveform being ramped.
        inner: Box<SourceKind>,
    },
}

impl SourceKind {
    /// The source value at time `t` (seconds).
    pub fn eval(&self, t: f64) -> f64 {
        match self {
            SourceKind::Dc(v) => *v,
            SourceKind::Sin {
                offset,
                amplitude,
                freq,
                delay,
                theta,
                phase,
            } => {
                if t < *delay {
                    *offset + amplitude * (phase.to_radians()).sin()
                } else {
                    let td = t - delay;
                    let damp = if *theta != 0.0 {
                        (-theta * td).exp()
                    } else {
                        1.0
                    };
                    offset + amplitude * damp * (TAU * freq * td + phase.to_radians()).sin()
                }
            }
            SourceKind::Pulse {
                v1,
                v2,
                delay,
                rise,
                fall,
                width,
                period,
            } => pulse(*v1, *v2, *delay, *rise, *fall, *width, *period, t),
            SourceKind::Pwl(points) => pwl(points, t),
            SourceKind::Ramped { scale_to, inner } => {
                // `min(t / scale_to, 1).max(0) * inner(t)`. Guard scale_to <= 0
                // (a degenerate zero-length ramp) as "full amplitude at once".
                let ramp = if *scale_to > 0.0 {
                    (t / scale_to).min(1.0).max(0.0)
                } else {
                    1.0
                };
                ramp * inner.eval(t)
            }
        }
    }

    /// The value at `t = 0`, used to seed the DC operating point.
    pub fn dc_value(&self) -> f64 {
        match self {
            SourceKind::Dc(v) => *v,
            // A `Ramped` source is zero at t=0 by construction (the ramp factor
            // is 0), which is exactly the power-on rest value this seeds.
            _ => self.eval(0.0),
        }
    }

    /// Wrap `self` in a [`SourceKind::Ramped`] that reaches full amplitude at
    /// `t_ramp`. See the variant docs for the envelope shape.
    pub fn ramped(self, t_ramp: f64) -> SourceKind {
        SourceKind::Ramped {
            scale_to: t_ramp,
            inner: Box::new(self),
        }
    }
}

fn pulse(
    v1: f64,
    v2: f64,
    delay: f64,
    rise: f64,
    fall: f64,
    width: f64,
    period: f64,
    t: f64,
) -> f64 {
    if t < delay {
        return v1;
    }
    // Fold time into the current period when the source repeats.
    let local = if period > 0.0 {
        let p = (t - delay) % period;
        if p < 0.0 {
            p + period
        } else {
            p
        }
    } else {
        t - delay
    };

    let rise = rise.max(0.0);
    let fall = fall.max(0.0);
    if local < rise {
        // Ramp v1 -> v2.
        if rise == 0.0 {
            v2
        } else {
            v1 + (v2 - v1) * (local / rise)
        }
    } else if local < rise + width {
        v2
    } else if local < rise + width + fall {
        if fall == 0.0 {
            v1
        } else {
            v2 + (v1 - v2) * ((local - rise - width) / fall)
        }
    } else {
        v1
    }
}

fn pwl(points: &[PwlPoint], t: f64) -> f64 {
    if points.is_empty() {
        return 0.0;
    }
    if t <= points[0].t {
        return points[0].v;
    }
    let last = points[points.len() - 1];
    if t >= last.t {
        return last.v;
    }
    // Linear interpolation between the bracketing breakpoints.
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        if t >= a.t && t <= b.t {
            let span = b.t - a.t;
            if span == 0.0 {
                return b.v;
            }
            let frac = (t - a.t) / span;
            return a.v + (b.v - a.v) * frac;
        }
    }
    last.v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramped_scales_amplitude_over_the_window() {
        // Ramp a 2 V DC over [0, 1e-6]: 0 at/before t=0, linear to full at
        // scale_to, held full after, and the inner value untouched thereafter.
        let src = SourceKind::Dc(2.0).ramped(1e-6);

        // At and before t=0 the ramp factor is 0.
        assert_eq!(src.eval(0.0), 0.0);
        assert_eq!(src.eval(-1.0), 0.0);
        // dc_value uses eval(0.0): a power-on source rests at zero.
        assert_eq!(src.dc_value(), 0.0);

        // Linear through the window: half-way is half amplitude.
        assert!((src.eval(0.5e-6) - 1.0).abs() < 1e-12);
        assert!((src.eval(0.25e-6) - 0.5).abs() < 1e-12);

        // At and after scale_to the inner behaviour is full and unshifted.
        assert!((src.eval(1e-6) - 2.0).abs() < 1e-12);
        assert!((src.eval(5e-6) - 2.0).abs() < 1e-12);
    }

    #[test]
    fn ramped_does_not_time_shift_the_inner() {
        // A sine ramped in has vanishing amplitude near t=0 but is NOT delayed:
        // the envelope multiplies, it does not shift the phase.
        let sin = SourceKind::Sin {
            offset: 0.0,
            amplitude: 1.0,
            freq: 1e3,
            delay: 0.0,
            theta: 0.0,
            phase: 90.0, // cos-like: inner(0) = 1
        };
        let ramped = sin.clone().ramped(1e-3);
        // inner(0) = 1, ramp(0) = 0 => product 0 (no shift, just scaled).
        assert_eq!(ramped.eval(0.0), 0.0);
        // At full ramp the product equals the bare inner value.
        let t = 2e-3;
        assert!((ramped.eval(t) - sin.eval(t)).abs() < 1e-12);
    }
}
