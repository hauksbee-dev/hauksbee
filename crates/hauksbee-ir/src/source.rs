//! Independent source waveforms.
//!
//! Each [`SourceKind`] evaluates to a scalar at a given time, used identically
//! by voltage and current sources. The closed forms match ngspice's `sin`,
//! `pulse`, and `pwl` so cross-checks line up without surprises.

use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;

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
        }
    }

    /// The value at `t = 0`, used to seed the DC operating point.
    pub fn dc_value(&self) -> f64 {
        match self {
            SourceKind::Dc(v) => *v,
            _ => self.eval(0.0),
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
