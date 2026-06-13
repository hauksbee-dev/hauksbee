//! Solver configuration: integration method, tolerances, and physics toggles.
//!
//! Every physical effect that can be turned off is a named boolean here.
//! Turning physics off is a product feature — for debugging ("does the bug
//! survive without junction caps?") and for raw speed when fidelity isn't
//! needed. Defaults are realistic; the named toggles let a caller trade
//! accuracy for speed deliberately.

use serde::{Deserialize, Serialize};

/// Numerical integration formula for reactive elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Integration {
    /// Trapezoidal rule: 2nd order, A-stable, but can ring on stiff steps.
    Trapezoidal,
    /// Gear / BDF-2: 2nd order, stiffly stable, damps ringing. Needs two
    /// history points so the first step falls back to backward Euler.
    Gear2,
    /// Backward Euler: 1st order, maximally damped. Mostly a fallback.
    BackwardEuler,
}

/// Whether the solver splits the circuit into islands before time-marching.
///
/// `Off` runs the classic monolithic engine: one global MNA matrix, one Newton
/// loop per step. It is the reference path and is bit-for-bit identical to the
/// pre-partitioning solver.
///
/// `Auto` lets the engine partition the circuit at ideal-source boundaries,
/// route purely linear islands through a state-space matrix-exponential advance
/// (exact per fixed step), and solve each nonlinear island on its own small
/// matrix, exchanging boundary values once per step (Gauss-Seidel). When the
/// topology or options make partitioning unprofitable or unsafe (adaptive step,
/// no separable linear island, strongly-coupled nonlinear core) it transparently
/// falls back to the monolithic path, so results never regress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Partitioning {
    /// Always solve one global system (reference behaviour).
    Off,
    /// Partition when it is safe and faster; fall back to monolithic otherwise.
    #[default]
    Auto,
}

/// How the timestep is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum StepControl {
    /// Constant `dt` for the whole run.
    Fixed { dt: f64 },
    /// Adaptive with local-truncation-error control between `dt_min`/`dt_max`.
    Adaptive {
        dt_initial: f64,
        dt_min: f64,
        dt_max: f64,
    },
}

/// Device-physics switches. Each `false` removes the corresponding term from
/// every device that has it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeviceEffects {
    /// BJT/MOS Early effect (output-conductance from base-width / channel-len
    /// modulation). Off => ideal current sources in saturation.
    pub early_effect: bool,
    /// Charge storage: diode/BJT junction & diffusion capacitances. Off => DC
    /// behaviour even in transient (fast, but loses switching dynamics).
    pub junction_caps: bool,
    /// Ohmic series resistances (diode RS, BJT RB/RE/RC). Off => ideal contacts.
    pub series_resistance: bool,
    /// Temperature dependence of saturation currents and thermal voltage.
    /// Off => everything evaluated at TNOM regardless of `temperature_c`.
    pub temperature: bool,
}

impl Default for DeviceEffects {
    fn default() -> Self {
        DeviceEffects {
            early_effect: true,
            junction_caps: true,
            series_resistance: true,
            temperature: true,
        }
    }
}

/// Full solver configuration.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SolverOptions {
    /// Integration formula.
    pub integration: Integration,
    /// Timestep strategy.
    pub step: StepControl,
    /// Relative tolerance for Newton convergence and LTE control.
    pub reltol: f64,
    /// Absolute voltage tolerance (V).
    pub vntol: f64,
    /// Absolute current tolerance (A).
    pub abstol: f64,
    /// Charge tolerance (C), used by the capacitor LTE estimate.
    pub chgtol: f64,
    /// Maximum Newton iterations per timestep before giving up / cutting dt.
    pub max_newton: usize,
    /// Conductance added from every node to ground for numerical robustness.
    pub gmin: f64,
    /// Ambient temperature (Celsius). Used when `effects.temperature` is on.
    pub temperature_c: f64,
    /// Physics toggles.
    pub effects: DeviceEffects,
    /// Enable gmin-stepping then source-stepping homotopy if the plain DC
    /// operating-point Newton fails to converge.
    pub dc_homotopy: bool,
    /// Whether to partition the circuit into islands before solving.
    #[serde(default)]
    pub partitioning: Partitioning,
    /// Coupling granularity for the partitioned path, in `[0, 1]`. At `1.0` the
    /// orchestrator runs extra Gauss-Seidel relaxation sweeps per step to tighten
    /// inter-island agreement (more accurate, slower); at `0.0` it does a single
    /// sweep (fastest, looser coupling). Ignored when `partitioning == Off`.
    #[serde(default = "default_granularity")]
    pub granularity: f64,
}

fn default_granularity() -> f64 {
    1.0
}

impl Default for SolverOptions {
    fn default() -> Self {
        SolverOptions {
            integration: Integration::Trapezoidal,
            step: StepControl::Adaptive {
                dt_initial: 1e-9,
                dt_min: 1e-18,
                dt_max: f64::INFINITY,
            },
            reltol: 1e-3,
            vntol: 1e-6,
            abstol: 1e-12,
            chgtol: 1e-14,
            max_newton: 100,
            gmin: 1e-12,
            temperature_c: 27.0,
            effects: DeviceEffects::default(),
            dc_homotopy: true,
            partitioning: Partitioning::Auto,
            granularity: 1.0,
        }
    }
}

impl SolverOptions {
    /// Convenience: fixed-step trapezoidal with the given `dt`.
    pub fn fixed(dt: f64) -> Self {
        SolverOptions {
            step: StepControl::Fixed { dt },
            ..Default::default()
        }
    }

    /// Convenience: adaptive run seeded with `dt_initial`.
    pub fn adaptive(dt_initial: f64, dt_max: f64) -> Self {
        SolverOptions {
            step: StepControl::Adaptive {
                dt_initial,
                dt_min: dt_initial * 1e-9,
                dt_max,
            },
            ..Default::default()
        }
    }

    /// The effective temperature to evaluate models at (TNOM if temp is off).
    pub fn model_temp(&self) -> f64 {
        if self.effects.temperature {
            self.temperature_c
        } else {
            27.0
        }
    }
}
