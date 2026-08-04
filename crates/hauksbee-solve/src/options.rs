//! Solver configuration: integration method, tolerances, and physics toggles.
//!
//! Every physical effect that can be turned off is a named boolean here.
//! Turning physics off is a product feature, for debugging ("does the bug
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
/// `Planned` routes assembly through the compiled [`crate::StampPlan`]: the
/// constant backbone (resistors, source/inductor incidence, per-dt reactive
/// conductances) is replayed as a flat list of pre-resolved slot writes, and
/// only the nonlinear / time-varying devices are re-evaluated, writing through
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

/// The classic SPICE device-evaluation bypass (dev-plan 03 §6): reuse a
/// quiescent nonlinear device's previous linearization (its recorded matrix +
/// RHS stamp) when none of the unknowns it reads moved more than
/// `0.1·(reltol·max(|v|,|v_last|) + vntol)` since its last evaluation.
///
/// `Off` (the default) is the reference: no cache exists, no movement test
/// runs, and every solve path is bit-for-bit the classic assembly. `On` is an
/// explicit opt-in speed knob whose accepted steps must match the no-bypass
/// reference to solver tolerance (reltol), never bit-for-bit; the iterate
/// PATH may change, the answer may not (§6.2's gate). The bypass machinery
/// carries SPICE's safety discipline internally: never on the first two
/// iterations of a solve, never on DC / event-frozen solves or the trials
/// immediately after an event-resolved step, cache invalidated across steps
/// (the charge-companion history moves per step). See `bypass.rs` for the
/// cache design and the excluded device classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum NewtonBypass {
    /// No bypass (the bit-for-bit reference path).
    #[default]
    Off,
    /// Skip re-evaluating quiescent nonlinear devices within a step's Newton
    /// iteration sequence.
    On,
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
    /// Pair it with [`hauksbee_ir::SourceKind::Ramped`] sources so the board sees a
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

/// How a `VSwitch`'s conductance moves between `roff` and `ron` as its control
/// voltage crosses the band `[voff, von]`.
///
/// These are two DIFFERENT DEVICES, not two numerical approximations of one, so
/// the choice is a modelling decision the caller makes rather than a tuning
/// knob:
///
/// * [`SwitchModel::Hysteretic`] is the SPICE3 `.model SW/CSW` device, which is
///   what `.model S... SW(VT=… VH=…)` in a deck asks for and what ngspice
///   implements: a LATCHING relay. It closes the instant the control passes
///   `von = VT + VH`, opens the instant it falls below `voff = VT - VH`, and
///   HOLDS its previous state anywhere in between. Measured against
///   ngspice-45.2 (`crates/hauksbee-solve/tests/decks/switch_*`): the transition
///   is a single-timestep snap from exactly `roff` to exactly `ron`, with no
///   interpolation region at all, and a genuine memory band. `VH` is a
///   HYSTERESIS half-width; it is not a transition width.
///
/// * [`SwitchModel::Smooth`] is a real analog pass element (a CMOS switch, a
///   MOSFET used as a gate): a monotone, memoryless, differentiable conductance
///   ramp across `[voff, von]`, saturating to exactly `roff` at or below `voff`
///   and exactly `ron` at or above `von`. It has NO hysteresis, because a bare
///   pass gate has none. Use it when the two band edges genuinely describe where
///   the device starts and finishes conducting (the board binder's analog
///   switches), and when a differentiable device is needed for the coupled
///   Newton (the SPDT break-before-make path in `stamp_vswitch`).
///
/// Default is [`SwitchModel::Hysteretic`], because the overwhelming majority of
/// `VSwitch` devices arrive from a SPICE `.model SW` card whose meaning is fixed
/// by SPICE3, and because it is the setting under which an ngspice cross-check
/// is comparing the same device on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SwitchModel {
    /// SPICE3 / ngspice `SW`: latching relay, hard snap at each threshold.
    #[default]
    Hysteretic,
    /// Real analog pass element: smooth monotone ramp, no memory.
    Smooth,
}

/// Device-physics switches. Each `false` removes the corresponding term from
/// every device that has it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DeviceEffects {
    /// BJT/MOS Early effect (output-conductance from base-width / channel-len
    /// modulation). Off => ideal current sources in saturation.
    pub early_effect: bool,
    /// Charge storage: junction (depletion) & diffusion capacitances. Off =>
    /// DC behaviour even in transient (fast, but loses switching dynamics).
    /// Honored for DIODES (cjo/vj/m/tt, dev-plan 04 §3.1), BJTs (cje/cjc/tf/tr,
    /// §3.2: two charge banks) and MOSFETs (gate charges + body-diode depletion,
    /// §3.3), all charge-based companions with LTE participation and jwC in AC.
    /// A model without charge fields stamps identically whatever this toggle
    /// says.
    pub junction_caps: bool,
    /// Ohmic series resistances. Honored for BJTs (RB/RE/RC via layout-private
    /// internal nodes, dev-plan 04 §3.2). The diode's RS is NOT stamped yet,
    /// a diode model carrying a nonzero RS logs once (under HAUKSBEE_DEBUG)
    /// instead of silently ignoring it (§3.4); it can ride the same
    /// internal-node machinery in a follow-up.
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
    /// current; a real SN74LVC1G3157 never does that), so it is on by default
    /// per dev-plan 02 section 2.6. `false` is the explicit compat switch
    /// restoring the bridging model.
    #[serde(default = "default_true")]
    pub spdt_bbm: bool,
    /// Break-before-make winner-take-all sharpness (per unit select margin).
    #[serde(default = "default_spdt_bbm_k")]
    pub spdt_bbm_k: f64,
    /// Stamp the switch control-node transconductance (the Newton tangent
    /// coupling the through-current to the control voltage). Not required for
    /// correctness (the same root is reached without it, with more
    /// iterations); on a torn column whose gate control is a high-Z boundary
    /// node the summed back-coupling throttles the control's slew (measured
    /// ~15x slower than its RC), so such callers set this `false`.
    #[serde(default = "default_true")]
    pub switch_ctrl_gm: bool,
    /// Gain (1/V) of the smooth logistic comparator transfer the STAGED solve
    /// path uses (every normal solve keeps the discrete bang-bang model).
    #[serde(default = "default_cmp_smooth_gain")]
    pub cmp_smooth_gain: f64,
    /// Which `VSwitch` device the solver stamps. See [`SwitchModel`]; the
    /// default is the SPICE3 `SW` relay a deck's `.model` card asks for.
    #[serde(default)]
    pub switch_model: SwitchModel,
    /// Width of a [`SwitchModel::Hysteretic`] relay's conductance ramp, as a
    /// fraction of its own `|von - voff|` band.
    ///
    /// The relay's switching THRESHOLDS are exact whatever this is; the ramp only
    /// says how abruptly the conductance crosses one of them. It exists because a
    /// literal step is worse on both counts that matter: no real switch is
    /// discontinuous, and a flat pin costs the per-step Newton its continuous path
    /// through the transition (measured: a hard pin handed the synapse mirror's
    /// cold diode-connected base a 0.69 V jump in a single step and it settled at
    /// a false root).
    ///
    /// The default 0.01 is a timing error of `0.01*|von-voff| / dv_ctrl_dt`: on the
    /// synapse deck's `SW(VT=1.5 VH=1.0)` band driven by a 0.3 V/µs edge, 20 mV of
    /// ramp is 67 ns of switching-instant uncertainty against a 400 µs march.
    /// Shrink it for a sharper edge, at the cost of a stiffer step; a value at or
    /// below zero is treated as the smallest usable ramp rather than a step.
    #[serde(default = "default_switch_transition_frac")]
    pub switch_transition_frac: f64,
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
fn default_switch_transition_frac() -> f64 {
    0.01
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
            switch_model: SwitchModel::Hysteretic,
            switch_transition_frac: 0.01,
        }
    }
}

/// One rung of the robustness ladder: a named permission for an escalation
/// mechanism the solver may engage when the plain path is not enough. Each is
/// BIT-IDENTICAL to baseline when not reached: granting a strategy that never
/// fires changes nothing. That is the invariant (dev-plan 02 section 2.6) the
/// gates pin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strategy {
    /// Global Armijo line search in the per-step transient Newton (the
    /// globalization for traveling mesh overshoots). The transient driver also
    /// arms it automatically as part of [`Strategy::TransientDyn`].
    LineSearch,
    /// Markowitz dynamic re-pivot fallback when the frozen LU ordering hits a
    /// singular pivot, in the DC / staged solves.
    DynamicPivot,
    /// Force the dynamic-pivot fallback on for EVERY transient step (the blunt,
    /// slower lever kept for stubborn boards; the event-freeze retry arms it
    /// locally where needed).
    DynamicPivotEveryStep,
    /// The event-driven staged DC solve: freeze comparator/switch states per
    /// inner Newton solve, re-derive Gauss-Seidel until consistent.
    EventFreeze,
    /// Pseudo-transient continuation rescue in the staged DC ladder.
    Ptc,
    /// Accept a DC iterate whose KCL residual is below
    /// [`SolverOptions::residual_accept_tol`] even though the Newton step
    /// tolerance was never met (the diode-mesh limit-cycle backstop; a sub-nA
    /// residual IS an operating point).
    ResidualAccept,
    /// The stiff-transient bundle the spike-path marches need: staged
    /// regularizers on every step, the per-step event-freeze retry, and the
    /// Armijo line search. Granted as one unit because stiff marches need all
    /// three together; a later round may split it.
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

/// The typed escalation ladder: the explicit, per-run control-flow permission
/// set (dev-plan 02 section 2.6). MEMBERSHIP is the permission, consulted by
/// the solve paths at each escalation point; the insertion order documents the
/// intended escalation order (the partitioner selecting ladder aggressiveness
/// per island builds on that). `Copy` because `SolverOptions` is copied through
/// every orchestration layer, which is also how the ladder reaches nested
/// sub-solves without any process-global state.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct RobustnessLadder {
    steps: [Option<Strategy>; 8],
}

impl RobustnessLadder {
    /// The empty ladder: the plain solver with no escalation granted. This is
    /// the default.
    pub const fn none() -> Self {
        RobustnessLadder { steps: [None; 8] }
    }

    /// The full ladder in canonical order: every escalation the solver has.
    /// This is the substrate the flagship spike paths run on.
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
    pub inner_max_newton: Option<usize>,
    /// Switch-flip budget per Gauss-Seidel pass: large enough to flip a
    /// neuron's whole gate fan-out in one pass, small enough to stop discrete
    /// thrash.
    pub flip_budget: usize,
    /// Try the SMOOTH-comparator retry mode before the frozen mode (the
    /// membrane-crosses-up FIRE regime prefers smooth, the refractory reset
    /// prefers frozen; both are always tried, this is only the order).
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
    /// The classic SPICE device-evaluation bypass; see [`NewtonBypass`].
    /// Off by default (dev-plan 03 §6.2: promoted only after its gates pass);
    /// `On` is a per-run opt-in, never flipped by any internal path.
    #[serde(default)]
    pub newton_bypass: NewtonBypass,
    /// Whether the partitioned sweep may run islands on a thread pool; see
    /// [`ParallelPolicy`]. Never changes results (the sweep is order-free by
    /// construction), only wall time.
    #[serde(default)]
    pub parallel: ParallelPolicy,
    /// Fidelity granularity in `[0, 1]`, consumed by the engine layer to scale
    /// tolerances and physics fidelity (1.0 = full). The partitioned path's
    /// relaxation sweep count is NOT derived from it: the inter-island exchange
    /// iterates until the boundary change relaxes under the reltol/vntol
    /// convention, which granularity already loosens through the tolerances
    /// themselves.
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
    /// that strategy is granted.
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
            newton_bypass: NewtonBypass::Off,
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
