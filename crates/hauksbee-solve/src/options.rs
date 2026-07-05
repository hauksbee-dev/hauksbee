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

/// How each Newton iteration assembles the linearized system `g x = rhs`.
///
/// `Interpreted` is the classic reference assembly: one walk of the whole
/// device list per iteration, every matrix write finding its slot by binary
/// search. It is the bit-for-bit oracle path and the default.
///
/// `Planned` routes assembly through the compiled [`crate::StampPlan`]
/// (`docs/dev-plans/03-solver-performance.md` §5): the constant backbone
/// (resistors, source/inductor incidence, per-dt reactive conductances) is
/// replayed as a flat list of pre-resolved slot writes, and only the
/// nonlinear / time-varying devices are re-evaluated, writing through
/// pre-resolved per-device slot tables instead of per-write binary searches.
/// The accumulation ORDER differs from `Interpreted`, so results match to
/// solver tolerance (reltol/vntol), not bit-for-bit; contexts the plan does
/// not cover (DC solves, staged regularizers, event-frozen solves) fall back
/// to the interpreted assembly automatically. Explicit opt-in only: no
/// default path flips to `Planned` here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AssemblyMode {
    /// Classic interpreted device walk (the bit-for-bit reference).
    #[default]
    Interpreted,
    /// Two-tier compiled assembly through the [`crate::StampPlan`].
    Planned,
}

/// Whether the partitioned engine may execute its per-sweep island work on a
/// rayon thread pool (dev-plan 03 §3.4).
///
/// The sweep itself is an order-free double-buffered Jacobi exchange (see
/// `partitioned::sweep`): every island reads a frozen exchange buffer and
/// writes only its own scratch, so the result is bit-for-bit identical no
/// matter how many threads execute it, or in what order. This policy therefore
/// only chooses HOW the identical work runs, never WHAT it computes; `Off` is
/// the sequential debugging/oracle switch, not a different numerical path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ParallelPolicy {
    /// Parallelize when the island count clears a measured threshold (small
    /// boards stay sequential: dispatching two islands to a pool loses to the
    /// pool overhead). Pool sized by `available_parallelism`, capped.
    #[default]
    Auto,
    /// Always sequential (the reference execution order).
    Off,
    /// Force a pool of exactly `n` threads and engage it regardless of the
    /// Auto threshold. This is the explicit `--threads N`-style knob, and what
    /// the determinism gate (§3.5) uses to pin 1/2/4/8-thread runs.
    Threads(usize),
}

/// How the transient march obtains its state at `t = 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DcInit {
    /// Solve a DC operating point at `t = 0` and seed the march from it (the
    /// reference behaviour). If no operating point converges the run fails:
    /// there is nothing to march from.
    #[default]
    Solve,
    /// UIC-style power-on start: skip the DC solve entirely. The unknown vector
    /// is set to `x(0) = 0` (all node voltages and branch currents zero), every
    /// reactive element's history is zeroed (capacitor voltage `x1 = x2 = 0`,
    /// inductor current `x1 = x2 = 0`, derivatives `dx1 = 0`), the `t = 0`
    /// sample is emitted as zeros, and the march proceeds from rest with the
    /// existing backward-Euler first step.
    ///
    /// Pair it with [`crate::SourceKind::Ramped`] sources so the board sees a
    /// physical power-ramp: sources rise from zero and the state integrates up,
    /// and there is no DC solve to fail. This is the honest way to carry a
    /// circuit (or sub-circuit) whose DC operating point is unreachable: a
    /// free-running oscillator with no stable DC, or a group that stalls in DC
    /// homotopy. The `t = 0` of such a run is a power-on rest state, not a
    /// settled operating point.
    FromZero,
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
    /// SPDT break-before-make: the two throws of a recognized SPDT pair
    /// (binder-emitted `_s0`/`_s1` legs sharing the common node) never conduct
    /// simultaneously; in the select transition band the losing throw is
    /// driven toward `roff` by a winner-take-all on select margin. This is a
    /// CORRECTNESS fix to the switch model (the bare smooth-tanh model bridges
    /// both throws at mid-band, injecting a weight-independent common-mode
    /// current; a real SN74LVC1G3157 never does that), promoted from the
    /// HAUKSBEE_SPDT_BBM env knob to the default per dev-plan 02 section 2.6.
    /// `false` is the explicit compat switch restoring the bridging model.
    #[serde(default = "default_true")]
    pub spdt_bbm: bool,
    /// Break-before-make winner-take-all sharpness (per unit select margin);
    /// formerly HAUKSBEE_SPDT_BBM_K.
    #[serde(default = "default_spdt_bbm_k")]
    pub spdt_bbm_k: f64,
    /// Stamp the switch control-node transconductance (the Newton tangent
    /// coupling the through-current to the control voltage). Not required for
    /// correctness (the same root is reached without it, with more
    /// iterations); on a torn column whose gate control is a high-Z boundary
    /// node the summed back-coupling throttles the control's slew (measured
    /// ~15x slower than its RC), so such callers set this `false`. Formerly
    /// HAUKSBEE_SW_NO_CTRL_GM (inverted).
    #[serde(default = "default_true")]
    pub switch_ctrl_gm: bool,
    /// Gain (1/V) of the smooth logistic comparator transfer the STAGED solve
    /// path uses (every normal solve keeps the discrete bang-bang model);
    /// formerly HAUKSBEE_CMP_K.
    #[serde(default = "default_cmp_smooth_gain")]
    pub cmp_smooth_gain: f64,
}

fn default_true() -> bool {
    true
}
fn default_spdt_bbm_k() -> f64 {
    6.0
}
fn default_cmp_smooth_gain() -> f64 {
    2000.0
}

impl Default for DeviceEffects {
    fn default() -> Self {
        DeviceEffects {
            early_effect: true,
            junction_caps: true,
            series_resistance: true,
            temperature: true,
            spdt_bbm: true,
            spdt_bbm_k: 6.0,
            switch_ctrl_gm: true,
            cmp_smooth_gain: 2000.0,
        }
    }
}

/// One rung of the robustness ladder: a named permission for an escalation
/// mechanism the solver may engage when the plain path is not enough. Each
/// maps one-to-one onto a formerly-`HAUKSBEE_*` env knob (noted per variant),
/// and each is BIT-IDENTICAL to baseline when not reached: granting a
/// strategy that never fires changes nothing. That is the plan's invariant
/// (dev-plan 02 section 2.6) and what the migration gates pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strategy {
    /// Global Armijo line search in the per-step transient Newton (the
    /// globalization for traveling mesh overshoots). Formerly
    /// HAUKSBEE_NEWTON_LINESEARCH; the transient driver also arms it
    /// automatically as part of [`Strategy::TransientDyn`].
    LineSearch,
    /// Markowitz dynamic re-pivot fallback when the frozen LU ordering hits a
    /// singular pivot, in the DC / staged solves. Formerly HAUKSBEE_DC_DYN.
    DynamicPivot,
    /// Force the dynamic-pivot fallback on for EVERY transient step (the old,
    /// slower lever kept for stubborn boards; the event-freeze retry arms it
    /// locally where needed). Formerly HAUKSBEE_TRANSIENT_DYN_GLOBAL.
    DynamicPivotEveryStep,
    /// The event-driven staged DC solve: freeze comparator/switch states per
    /// inner Newton solve, re-derive Gauss-Seidel until consistent. Formerly
    /// HAUKSBEE_CMP_EVENT.
    EventFreeze,
    /// Pseudo-transient continuation rescue in the staged DC ladder. Formerly
    /// HAUKSBEE_PTC.
    Ptc,
    /// Accept a DC iterate whose KCL residual is below
    /// [`SolverOptions::residual_accept_tol`] even though the Newton step
    /// tolerance was never met (the diode-mesh limit-cycle backstop; a sub-nA
    /// residual IS an operating point). Formerly HAUKSBEE_DC_RESID_ACCEPT.
    ResidualAccept,
    /// The stiff-transient bundle the spike-path marches need: staged
    /// regularizers on every step, the per-step event-freeze retry, and the
    /// Armijo line search. Formerly HAUKSBEE_TRANSIENT_DYN. (A bundle because
    /// that is exactly what the env var armed; round 2 may split it.)
    TransientDyn,
}

impl Strategy {
    /// Stable bit for the activation diagnostics.
    pub(crate) fn bit(self) -> u16 {
        match self {
            Strategy::LineSearch => 1 << 0,
            Strategy::DynamicPivot => 1 << 1,
            Strategy::DynamicPivotEveryStep => 1 << 2,
            Strategy::EventFreeze => 1 << 3,
            Strategy::Ptc => 1 << 4,
            Strategy::ResidualAccept => 1 << 5,
            Strategy::TransientDyn => 1 << 6,
        }
    }

    /// Every strategy, in the canonical escalation order (cheapest first).
    pub const ALL: [Strategy; 7] = [
        Strategy::LineSearch,
        Strategy::DynamicPivot,
        Strategy::DynamicPivotEveryStep,
        Strategy::EventFreeze,
        Strategy::Ptc,
        Strategy::ResidualAccept,
        Strategy::TransientDyn,
    ];
}

/// The typed escalation ladder that replaces the `HAUKSBEE_*` control-flow
/// env vars (dev-plan 02 section 2.6). Round-1 semantics: MEMBERSHIP is the
/// permission, consulted by the existing solve paths exactly where the env
/// reads used to sit; the insertion order documents the intended escalation
/// order (round 2, the partitioner selecting ladder aggressiveness per
/// island, builds on that). `Copy` because `SolverOptions` is copied through
/// every orchestration layer, which is also how the ladder reaches nested
/// sub-solves: the job the process-global env vars used to do covertly.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct RobustnessLadder {
    steps: [Option<Strategy>; 8],
}

impl RobustnessLadder {
    /// The empty ladder: the plain solver, bit-identical to the classic
    /// no-env-vars-set behavior. This is the default.
    pub const fn none() -> Self {
        RobustnessLadder { steps: [None; 8] }
    }

    /// The full ladder in canonical order: every escalation the solver has.
    /// This is the substrate the flagship spike paths run on (the exact set
    /// `tarski_decomp::solve_inference` used to arm via env).
    pub fn full() -> Self {
        let mut l = RobustnessLadder::none();
        for s in Strategy::ALL {
            l = l.with(s);
        }
        l
    }

    /// Grant a strategy (idempotent; appended in call order).
    pub fn with(mut self, s: Strategy) -> Self {
        if self.has(s) {
            return self;
        }
        for slot in self.steps.iter_mut() {
            if slot.is_none() {
                *slot = Some(s);
                return self;
            }
        }
        unreachable!("ladder capacity covers every Strategy variant");
    }

    /// Is this strategy granted?
    pub fn has(&self, s: Strategy) -> bool {
        self.steps.iter().any(|&x| x == Some(s))
    }

    /// Revoke a strategy, preserving the order of the rest. The per-island
    /// selection (round 2: `orchestrate::staged::select_group_ladder`) only
    /// ever TRIMS with this: the caller's ladder is the ceiling, never
    /// escalated past.
    pub fn without(mut self, s: Strategy) -> Self {
        let mut out = [None; 8];
        let mut k = 0;
        for slot in self.steps.iter().flatten() {
            if *slot != s {
                out[k] = Some(*slot);
                k += 1;
            }
        }
        self.steps = out;
        self
    }

    /// Granted strategies in escalation order.
    pub fn steps(&self) -> impl Iterator<Item = Strategy> + '_ {
        self.steps.iter().filter_map(|&s| s)
    }
}

/// Tuning for the event-freeze retry machinery (the discrete-state
/// Gauss-Seidel that resolves comparator/switch flips). These were env-tuned
/// knobs; they are solver configuration, so they live here.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EventRetryTuning {
    /// Inner Newton budget per frozen solve. `None` uses
    /// `max(max_newton, 400)` (the frozen inner circuit converges linearly in
    /// its tail, so it legitimately needs more than the outer budget).
    /// Formerly HAUKSBEE_TRAN_INNER_MAXIT.
    pub inner_max_newton: Option<usize>,
    /// Switch-flip budget per Gauss-Seidel pass: large enough to flip a
    /// neuron's whole gate fan-out in one pass, small enough to stop discrete
    /// thrash. Formerly HAUKSBEE_TRAN_FLIP_BUDGET.
    pub flip_budget: usize,
    /// Try the SMOOTH-comparator retry mode before the frozen mode (the
    /// membrane-crosses-up FIRE regime prefers smooth, the refractory reset
    /// prefers frozen; both are always tried, this is only the order).
    /// Formerly HAUKSBEE_TRAN_CMP_SMOOTH.
    pub smooth_comparator_first: bool,
}

impl Default for EventRetryTuning {
    fn default() -> Self {
        EventRetryTuning {
            inner_max_newton: None,
            flip_budget: 64,
            smooth_comparator_first: false,
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
    /// How the `t = 0` state is obtained: a DC solve (default) or a power-on
    /// start from zero. See [`DcInit`].
    #[serde(default)]
    pub dc_init: DcInit,
    /// Whether to partition the circuit into islands before solving.
    #[serde(default)]
    pub partitioning: Partitioning,
    /// How Newton assembles the linearized system; see [`AssemblyMode`].
    #[serde(default)]
    pub assembly: AssemblyMode,
    /// Whether the partitioned sweep may run islands on a thread pool; see
    /// [`ParallelPolicy`]. Never changes results (the sweep is order-free by
    /// construction), only wall time.
    #[serde(default)]
    pub parallel: ParallelPolicy,
    /// Coupling granularity for the partitioned path, in `[0, 1]`. At `1.0` the
    /// orchestrator runs extra Gauss-Seidel relaxation sweeps per step to tighten
    /// inter-island agreement (more accurate, slower); at `0.0` it does a single
    /// sweep (fastest, looser coupling). Ignored when `partitioning == Off`.
    #[serde(default = "default_granularity")]
    pub granularity: f64,
    /// The robustness escalations this run may use. Empty (the default) is the
    /// plain classic solver; see [`RobustnessLadder`].
    #[serde(default)]
    pub ladder: RobustnessLadder,
    /// Event-freeze retry tuning; see [`EventRetryTuning`].
    #[serde(default)]
    pub event_retry: EventRetryTuning,
    /// KCL-residual bar (A) for [`Strategy::ResidualAccept`]; ignored unless
    /// that strategy is granted. Formerly HAUKSBEE_DC_RESID_TOL.
    #[serde(default = "default_residual_accept_tol")]
    pub residual_accept_tol: f64,
}

fn default_residual_accept_tol() -> f64 {
    1e-9
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
            dc_init: DcInit::Solve,
            partitioning: Partitioning::Auto,
            assembly: AssemblyMode::Interpreted,
            parallel: ParallelPolicy::Auto,
            granularity: 1.0,
            ladder: RobustnessLadder::none(),
            event_retry: EventRetryTuning::default(),
            residual_accept_tol: 1e-9,
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
