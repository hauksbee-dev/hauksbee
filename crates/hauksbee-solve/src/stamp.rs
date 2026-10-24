//! Device companion models and Newton-linearized stamps.
//!
//! Each device contributes (a) conductances into the MNA matrix `g` and (b)
//! equivalent currents into the right-hand side `rhs`. Linear elements stamp a
//! constant conductance once per matrix; reactive elements use a companion
//! model derived from the integration rule; nonlinear elements stamp the
//! Newton tangent (`gd = dI/dV`) plus an equivalent current so the linear
//! solve yields the next Newton iterate.
//!
//! Sign convention: positive branch current flows from the first listed node to
//! the second (anode->cathode, drain->source, ...). Conductance between nodes
//! `a` and `b` stamps `+g` on the diagonals and `-g` off-diagonal, the standard
//! nodal pattern.
//!

use crate::options::{Integration, SolverOptions};
use crate::sparse::SparseMatrix;
use crate::system::{Layout, ReactiveState};
use hauksbee_ir::{
    BjtModel, Circuit, Device, DeviceId, DiodeModel, MosLevel, MosfetModel, NodeId, SourceKind,
};

/// Integration coefficients for one timestep: a reactive companion model of the
/// form `i = geq * v + ieq` (capacitor) or `v = req * i + veq` (inductor).
#[derive(Debug, Clone, Copy)]
pub struct IntegCoeffs {
    /// Companion conductance `dq/dt` factor: `C/dt` (BE) etc.
    pub g: f64,
    /// History coefficient applied to the previous state.
    pub a1: f64,
    /// Two-step-back coefficient (Gear-2).
    pub a2: f64,
    /// The step size these coefficients were built for. Carried so stamps
    /// with an explicit time-domain update (the op-amp's single-pole/slew
    /// output follower) can see the actual `dt` instead of reverse-deriving
    /// it from `g` (whose relation to `dt` is rule-dependent). DC callers
    /// pass a dummy step; every DC stamp path is gated on `ctx.dc` and never
    /// reads it.
    pub dt: f64,
}

impl IntegCoeffs {
    /// Coefficients for the chosen rule at step size `dt`. `first_step` forces
    /// backward Euler where a multi-step rule lacks history.
    ///
    /// `dt_prev` is the spacing of the stored history pair (x1..x2), i.e. the
    /// previous ACCEPTED step. Only the Gear-2/BDF2 branch reads it: a
    /// two-step rule differentiates through THREE points, and its stencil
    /// weights depend on how far apart those points actually sit. Under the
    /// adaptive controller the grid is non-uniform, so applying the
    /// uniform-grid weights there silently degrades BDF2 to first order,
    /// exactly on the stiff problems Gear-2 is chosen for. Callers on a
    /// uniform grid (fixed step, unit tests) pass `dt_prev == dt`; a
    /// non-positive `dt_prev` (no accepted step yet) also means "assume
    /// uniform", which is exact because the seeded history has x2 == x1.
    pub fn for_step(method: Integration, dt: f64, dt_prev: f64, first_step: bool) -> Self {
        match method {
            Integration::Trapezoidal if !first_step => Self {
                g: 2.0 / dt,
                a1: 2.0 / dt,
                a2: 0.0,
                dt,
            },
            Integration::Gear2 if !first_step => {
                // Variable-step BDF2. Let h = t_n - t_{n-1} (this step),
                // h_prev = t_{n-1} - t_{n-2} (spacing of the stored history),
                // and r = h / h_prev. Fitting a quadratic through
                // (t_n, q_n), (t_{n-1}, q_{n-1}), (t_{n-2}, q_{n-2}) and
                // differentiating at t_n gives the standard non-uniform
                // three-point backward-difference weights (Hairer & Wanner,
                // "Solving ODEs II", ch. III.5; the same form SPICE-family
                // simulators use for variable-order Gear):
                //
                //   dq/dt |_{t_n} ≈ [ (1+2r)/(1+r) · q_n
                //                     - (1+r)      · q_{n-1}
                //                     + r²/(1+r)   · q_{n-2} ] / h
                //
                // Sanity: the weights sum to zero (a constant has zero
                // derivative), and r = 1 collapses them to the familiar
                // uniform stencil (3 q_n - 4 q_{n-1} + q_{n-2}) / (2h),
                // bit-for-bit, since (1+2r) = 3, (1+r)·h = 2h and r² = 1 are
                // all exact and each coefficient is then a single correctly
                // rounded division of the same real value as before (a unit
                // test pins this identity).
                //
                // The companion model consumes these as
                // dq/dt = g·q_n − (a1·q_{n-1} − a2·q_{n-2}), so a1/a2 carry
                // the sign flip of the middle/last weights.
                let r = if dt_prev > 0.0 { dt / dt_prev } else { 1.0 };
                Self {
                    g: 2.0f64.mul_add(r, 1.0) / ((1.0 + r) * dt),
                    a1: (1.0 + r) / dt,
                    a2: (r * r) / ((1.0 + r) * dt),
                    dt,
                }
            }
            // Backward Euler, and the first step of the two-step methods (to start cleanly).
            Integration::BackwardEuler | Integration::Trapezoidal | Integration::Gear2 => Self {
                g: 1.0 / dt,
                a1: 1.0 / dt,
                a2: 0.0,
                dt,
            },
        }
    }
}

/// Everything stamping needs that varies per step / per Newton iteration.
pub struct StampCtx<'a> {
    pub circuit: &'a Circuit,
    pub layout: &'a Layout,
    pub opts: &'a SolverOptions,
    /// Current Newton iterate of all unknowns (node voltages then branches).
    pub x: &'a [f64],
    /// Previous Newton iterate (the last accepted point), used as the anchor for
    /// pn-junction voltage limiting. Equal to `x` on the very first iteration of
    /// an operating point (no prior point yet), which disables limiting then.
    pub x_prev: &'a [f64],
    pub time: f64,
    pub coeffs: IntegCoeffs,
    pub state: &'a ReactiveState,
    /// True during a DC operating-point solve (reactive elements are open/short
    /// and sources held at their DC value).
    pub dc: bool,
    /// In the DC solve, pin reactive elements that declare an initial condition
    /// to that value (the transient "initial conditions" operating point).
    pub use_ic: bool,
    /// Extra conductance to ground for gmin / source stepping homotopy.
    pub gmin: f64,
    /// Source scaling for source-stepping homotopy (1.0 = full).
    pub src_scale: f64,
    /// Tiny resistance (ohms) added in series with every ideal voltage source
    /// (stamped on the branch-row diagonal) during the staged-DC fallback. It
    /// keeps the frozen sparse ordering from hitting a zero pivot on a Vsource
    /// branch when a downstream junction conducts (a numerical, not physical,
    /// singularity). 0.0 disables it, keeping the normal path bit-identical.
    pub branch_reg: f64,
    /// Optional FROZEN comparator output decisions (keyed by device id),
    /// supplied only by the staged-DC event-driven outer loop. When present, a
    /// comparator's `high`/low state is held at the frozen value for the whole
    /// inner Newton solve instead of being recomputed from the chattering output
    /// node each iteration; the outer loop re-evaluates the decisions between
    /// inner solves and re-solves until they stop flipping. This turns the
    /// otherwise-discontinuous comparator network into a smooth circuit Newton
    /// can converge, killing the limit cycle. `None` (every normal solve) keeps
    /// the self-deciding behaviour bit-identical.
    pub cmp_freeze: Option<&'a std::collections::HashMap<DeviceId, bool>>,
    /// Optional FROZEN analog-switch on/off decisions (keyed by device id),
    /// supplied only by the staged-DC event-driven outer loop. The many analog
    /// switches that fuse a switched current-mirror mesh each carry a
    /// tanh-blended conductance whose control node sits near the transition for
    /// a coupled core; left self-deciding, the inner Newton limit-cycles as the
    /// switch flips on/off every iteration. When present, each switch's
    /// conductance is PINNED to its frozen rail (full `ron` or `roff`, no
    /// control dependence) so the inner circuit is smooth and Newton converges;
    /// the outer Gauss-Seidel loop re-derives every switch's rail from the
    /// converged control voltages and re-solves until none flip. `None` (every
    /// normal solve) keeps the smooth tanh + control tangent bit-identical.
    pub switch_freeze: Option<&'a std::collections::HashMap<DeviceId, bool>>,
    /// LATCHED analog-switch states under [`crate::options::SwitchModel::Hysteretic`]
    /// (keyed by device id), supplied by [`crate::newton::newton_solve`]'s
    /// hysteretic outer loop.
    ///
    /// Distinct from `switch_freeze` in what it MEANS, which is why it is a
    /// separate channel rather than a reuse. `switch_freeze` is a regularization:
    /// the staged rescue pins a switch flat to hide a discontinuity Newton cannot
    /// cross, and the pinned value carries no control dependence at all. This is
    /// the DEVICE: the latch says which of the relay's two thresholds is currently
    /// the live one (`von` while open, `voff` while closed), and the stamp still
    /// resolves the conductance from the control voltage across a narrow ramp
    /// about that threshold. So the switching INSTANT is the relay's, exact to
    /// within the ramp width, while the device stays differentiable and Newton
    /// keeps a continuous path through the transition.
    ///
    /// `switch_freeze` wins where both are present (the rescue path is mid-flight
    /// and owns the decision).
    pub switch_latch: Option<&'a std::collections::HashMap<DeviceId, bool>>,
    /// SPDT leg sibling map (device id -> complementary throw's device id) for
    /// the smooth break-before-make coupling. Consulted only when
    /// `effects.spdt_bbm` (the device-model default); empty/ignored otherwise.
    pub spdt_sibling: &'a std::collections::HashMap<DeviceId, DeviceId>,
    /// Per-junction EVALUATION-voltage state for pn-junction limiting, supplied
    /// by the live Newton iteration ([`crate::newton::newton_solve_core`]'s main
    /// stamp) and absent everywhere else (residual probes, plan builds, tests).
    ///
    /// This is SPICE's own anchoring, and the distinction it carries is load
    /// bearing. Anchoring pnjlim on the previous ITERATE makes a collapsed
    /// junction a fixed point: when a relay hands a stiff divider to a
    /// diode-connected base, the first Newton step launches the base to the
    /// LINEAR network voltage (measured: 4.95 V), the lagged anchor keeps the
    /// junction evaluated dead, the next linear solve reproduces the same
    /// point, and the step norm reads converged at a root with `exp(40)*Is` =
    /// 235 A of raw imbalance. Anchoring on the previous EVALUATION voltage
    /// instead lets the evaluation point climb its log schedule (`vt*ln`)
    /// every iteration while the node waits above, so within a handful of
    /// iterations the junction wakes, conducts for real, and pulls the node
    /// into the true root, which is how ngspice solves the same make step.
    ///
    /// `None` evaluates against `x_prev` exactly as before, so every path that
    /// does not thread the state is bit-identical.
    pub junction_eval: Option<&'a JunctionEval>,
}

/// Per-junction evaluation-voltage store for SPICE-style pnjlim anchoring
/// (see [`StampCtx::junction_eval`]). Built once per workspace; values reset
/// to NaN ("unseeded, fall back to the `x_prev` anchor") at every solve entry.
pub struct JunctionEval {
    /// Device id -> base slot in `vals`. A diode owns 1 slot (vd); a BJT owns
    /// 2 (vbe, vbc).
    slots: std::collections::HashMap<DeviceId, usize>,
    /// The last evaluation voltage per junction; NaN = unseeded this solve.
    vals: std::cell::RefCell<Vec<f64>>,
    /// SPICE's `icheck`: set when any junction was LIMITED during the current
    /// stamp (evaluated at a voltage other than the iterate's own). An
    /// iteration that limited has not evaluated the real circuit, so its step
    /// norm says nothing about a root: while the evaluation anchor is still
    /// climbing its log schedule toward a suddenly-connected junction, the
    /// ITERATE can sit perfectly still at the linear-network voltage
    /// (measured: two zero-step iterations "converged" the mirror deck's make
    /// onto the 235 A false root). The Newton loop reads-and-clears this each
    /// iteration and refuses to call a limited iteration converged.
    limited: std::cell::Cell<bool>,
    /// Anchors every solve's first iteration starts from instead of NaN, per
    /// slot (NaN = none), until [`Self::clear_entries`]. Empty, the normal
    /// case, makes [`Self::reset`] exactly the all-NaN reset. The transient
    /// sets these for junctions whose seed voltage was never evaluated through
    /// their own equation (see `Workspace::enter_junction_at`).
    entry: Vec<f64>,
}

impl JunctionEval {
    /// Scan the circuit and allocate slots for every junction-limited device.
    pub fn build(circuit: &Circuit) -> Self {
        let mut slots = std::collections::HashMap::new();
        let mut n = 0usize;
        for (id, dev) in circuit.iter() {
            match dev {
                Device::Diode { .. } => {
                    slots.insert(id, n);
                    n += 1;
                }
                Device::Bjt { .. } => {
                    slots.insert(id, n);
                    n += 2;
                }
                _ => {}
            }
        }
        Self {
            slots,
            vals: std::cell::RefCell::new(vec![f64::NAN; n]),
            limited: std::cell::Cell::new(false),
            entry: Vec::new(),
        }
    }

    /// Read-and-clear the "some junction was limited this stamp" flag.
    pub const fn take_limited(&self) -> bool {
        self.limited.replace(false)
    }

    /// Mark every junction unseeded. Called at solve entry so no evaluation
    /// anchor leaks between solves (each solve's first iteration anchors on
    /// its own entry point, exactly the previous behaviour).
    pub fn reset(&self) {
        let mut vals = self.vals.borrow_mut();
        if self.entry.is_empty() {
            vals.fill(f64::NAN);
        } else {
            vals.copy_from_slice(&self.entry);
        }
    }

    /// Start junction `k` of `id` from evaluation anchor `v` at every solve
    /// entry until [`Self::clear_entries`]. A device with no junction slot
    /// is ignored.
    pub fn set_entry(&mut self, id: DeviceId, k: usize, v: f64) {
        if let Some(&s) = self.slots.get(&id) {
            if self.entry.is_empty() {
                self.entry = vec![f64::NAN; self.vals.borrow().len()];
            }
            self.entry[s + k] = v;
        }
    }

    /// Drop every entry anchor: solves start unseeded again.
    pub fn clear_entries(&mut self) {
        self.entry.clear();
    }

    /// The stored evaluation anchor for junction `k` of `id`, or `fallback`
    /// (the x_prev-derived anchor) when unseeded or untracked.
    fn anchor(&self, id: DeviceId, k: usize, fallback: f64) -> f64 {
        match self.slots.get(&id) {
            Some(&s) => {
                let v = self.vals.borrow()[s + k];
                if v.is_finite() {
                    v
                } else {
                    fallback
                }
            }
            None => fallback,
        }
    }

    /// Record the evaluation voltage this iteration actually used. `raw` is
    /// the iterate's own junction voltage; a stored value that differs means
    /// the evaluation was limited, which denies convergence this iteration
    /// (see `limited`).
    #[allow(clippy::float_cmp)] // exact: any limiter change (NaN included) denies convergence
    fn store(&self, id: DeviceId, k: usize, v: f64, raw: f64) {
        if let Some(&s) = self.slots.get(&id) {
            self.vals.borrow_mut()[s + k] = v;
            if v != raw {
                self.limited.set(true);
            }
        }
    }
}

impl StampCtx<'_> {
    #[inline]
    fn v(&self, node: NodeId) -> f64 {
        self.layout.node(node).map_or(0.0, |i| self.x[i])
    }
    /// Node voltage at the PREVIOUS Newton iterate (the limiting anchor).
    #[inline]
    fn v_prev(&self, node: NodeId) -> f64 {
        self.layout.node(node).map_or(0.0, |i| self.x_prev[i])
    }
}

/// Stamp a conductance `g` between two nodes into the matrix.
#[inline]
fn stamp_cond<S: StampSink>(sink: &mut S, layout: &Layout, a: NodeId, b: NodeId, g: f64) {
    stamp_cond_idx(sink, layout.node(a), layout.node(b), g);
}

/// Stamp an equivalent current source pushing `i` from `a` to `b`.
#[inline]
fn stamp_current<S: StampSink>(sink: &mut S, layout: &Layout, a: NodeId, b: NodeId, i: f64) {
    stamp_current_idx(sink, layout.node(a), layout.node(b), i);
}

/// Where device stamps write. Two implementations: the real `(matrix, rhs)`
/// assembly, and the matrix-free residual accumulator (assembly-economy
/// sub-lever A). Generic and monomorphized, so the default assembly path
/// compiles to exactly the writes it always did (its bit-exactness contract
/// is pinned by the fixture waveform hash and the solve suites).
pub trait StampSink {
    /// Accumulate `g[row][col] += v`.
    fn g(&mut self, row: usize, col: usize, v: f64);
    /// Accumulate `rhs[row] += v`.
    fn i(&mut self, row: usize, v: f64);
}

/// The classic assembly: writes into the sparse matrix and the rhs vector.
struct MatrixSink<'a> {
    g: &'a mut SparseMatrix,
    rhs: &'a mut [f64],
}

impl StampSink for MatrixSink<'_> {
    #[inline]
    fn g(&mut self, row: usize, col: usize, v: f64) {
        self.g.add(row, col, v);
    }
    #[inline]
    fn i(&mut self, row: usize, v: f64) {
        self.rhs[row] += v;
    }
}

/// Reserve the structural slots a device will ever touch, so the symbolic
/// factorization sees the complete pattern even when a value is momentarily 0.
pub fn reserve_pattern(circuit: &Circuit, layout: &Layout, matrix: &mut SparseMatrix) {
    // Slots are gathered per row and merged once at the end
    // (`SparseMatrix::touch_all`): touching one at a time inserted into a
    // sorted row, quadratic in a rail row's thousands of entries, and the
    // co-sim builds this pattern for every chunk.
    let mut pending = PendingSlots(vec![Vec::new(); matrix.dim()]);
    let m = &mut pending;
    let touch = |m: &mut PendingSlots, a: Option<usize>, b: Option<usize>| {
        if let Some(a) = a {
            m.touch(a, a);
        }
        if let Some(b) = b {
            m.touch(b, b);
        }
        if let (Some(a), Some(b)) = (a, b) {
            m.touch(a, b);
            m.touch(b, a);
        }
    };
    // gmin on every node diagonal.
    for i in 0..layout.size {
        m.touch(i, i);
    }
    for (id, dev) in circuit.iter() {
        let mut ns: Vec<Option<usize>> = dev.nodes().iter().map(|&n| layout.node(n)).collect();
        // A series-resistance BJT's internal unknowns join
        // the all-pairs set: the ohmic stamps couple external<->internal and
        // the relocated Gummel-Poon core couples the internals among
        // themselves; the union's all-pairs covers both (and the toggle-off
        // unit diagonal is inside it too).
        if let Some(ints) = layout.bjt_internal(id) {
            ns.extend(ints.iter().copied());
        }
        // A series-resistance MOSFET's internal drain/source unknowns (the
        // datasheet-Rds(on) split) join the all-pairs set the same way: the
        // ohmic stamps couple external<->internal and the relocated channel,
        // body-diode and gate-charge companions couple the internals to the
        // gate/bulk and each other. The union's all-pairs covers every slot
        // (and the toggle-off unit diagonal is inside it too).
        if let Some(ints) = layout.mos_internal(id) {
            ns.extend(ints.iter().copied());
        }
        // A series-resistance diode's intrinsic anode. Reserving is not
        // bookkeeping: a coordinate outside the pattern is silently discarded,
        // so an allocated-but-unreserved node leaves the junction stamped
        // nowhere and the diode stops conducting entirely.
        if let Some(int) = layout.diode_internal(id) {
            ns.push(Some(int));
        }
        // All-pairs structural coupling among a device's nodes covers every
        // companion/tangent stamp it can produce.
        for i in 0..ns.len() {
            for j in 0..ns.len() {
                touch(m, ns[i], ns[j]);
            }
        }
        if let Some(br) = layout.branch(id) {
            // Branch couples to every node the device touches, both ways, plus
            // its own diagonal slot. Two-terminal branch devices (Vsource,
            // Inductor) get exactly the p/n coupling they always did; the VCVS
            // branch row additionally needs its control columns (br, cp) and
            // (br, cn) reserved or `add_at` lands outside the frozen pattern.
            m.touch(br, br);
            for &n in &ns {
                if let Some(n) = n {
                    m.touch(br, n);
                    m.touch(n, br);
                }
            }
        }
        // Branch-current reads (F/H control, behavioral `I(...)` deps)
        // reserve slots in the CONTROL source's branch COLUMN, a column
        // belonging to a DIFFERENT device. This is the structurally new bit
        // vs E/G: the reservation walk needs the resolved control branch
        // index, and it has it because `Layout::new` runs before the pattern
        // freeze and the reference was resolved to a DeviceId at load time. A
        // dangling id (control source without a branch, e.g. a partition that
        // failed to keep it in this system) is unstampable, and the loud
        // panic here beats `add_at` landing outside the frozen pattern later.
        for ctrl in dev.controlling_sources() {
            let cbr = layout
                .branch(ctrl)
                .expect("control source must own a branch unknown in this system");
            match dev {
                // CCCS: gain*i_ctrl enters the p/n KCL rows.
                Device::Cccs { .. } => {
                    for &n in &ns {
                        if let Some(n) = n {
                            m.touch(n, cbr);
                        }
                    }
                }
                // CCVS: -transres*i_ctrl lives in H's OWN branch row.
                Device::Ccvs { .. } => {
                    let br = layout.branch(id).expect("ccvs owns a branch unknown");
                    m.touch(br, cbr);
                }
                // Behavioral: an I(...) partial lands in the p/n rows
                // (I-output) or the device's own branch row (V-output).
                // Touching every node row × control column is the same
                // superset rule as the all-pairs reservation above.
                Device::Behavioral { output, .. } => match output {
                    hauksbee_ir::BOutput::Current => {
                        for &n in &ns {
                            if let Some(n) = n {
                                m.touch(n, cbr);
                            }
                        }
                    }
                    hauksbee_ir::BOutput::Voltage => {
                        let br = layout
                            .branch(id)
                            .expect("V-output behavioral source owns a branch unknown");
                        m.touch(br, cbr);
                    }
                },
                // Coupling: the mutual companion writes the SYMMETRIC pair of
                // branch-row cross slots (l1 row × l2 column and vice versa).
                // Each of the two controlling_sources iterations touches both
                // (touch is idempotent), keeping this arm self-contained
                // instead of splitting one reservation across iterations. The
                // windings' own diagonals/incidence are already covered by
                // their branch reservation above.
                Device::Coupling { l1, l2, .. } => {
                    let b1 = layout
                        .branch(*l1)
                        .expect("coupled winding owns a branch unknown");
                    let b2 = layout
                        .branch(*l2)
                        .expect("coupled winding owns a branch unknown");
                    m.touch(b1, b2);
                    m.touch(b2, b1);
                }
                _ => unreachable!("controlling_sources() is non-empty for F/H/B/K only"),
            }
        }
    }
    matrix.touch_all(pending.0);
}

/// Structural slots awaiting [`SparseMatrix::touch_all`], one column list per
/// row, in any order and with repeats.
struct PendingSlots(Vec<Vec<usize>>);

impl PendingSlots {
    fn touch(&mut self, row: usize, col: usize) {
        self.0[row].push(col);
    }
}

pub fn stamp_all(ctx: &StampCtx, g: &mut SparseMatrix, rhs: &mut [f64]) {
    let mut sink = MatrixSink { g, rhs };
    stamp_into(ctx, &mut sink);
}

fn stamp_into<S: StampSink>(ctx: &StampCtx, sink: &mut S) {
    // gmin / homotopy conductance to ground.
    if ctx.gmin > 0.0 {
        for i in 0..ctx.layout.size {
            // Branch rows shouldn't get a shunt; only node rows (< n_nodes).
            if i < ctx.layout.n_nodes {
                sink.g(i, i, ctx.gmin);
            }
        }
    }
    // Staged-DC branch regularization: a negligible series resistance on every
    // Vsource/Inductor branch diagonal, so the frozen ordering always has a
    // nonzero pivot there even when a conducting diode reshapes the elimination.
    if ctx.branch_reg > 0.0 {
        for i in ctx.layout.n_nodes..ctx.layout.size {
            sink.g(i, i, -ctx.branch_reg);
        }
    }
    for (id, dev) in ctx.circuit.iter() {
        stamp_device(ctx, id, dev, sink);
    }
}

/// Stamp one device through a [`StampSink`]. `pub(crate)` so the compiled
/// two-tier assembly (`plan.rs`) can re-stamp tier-2 devices through the SAME
/// interpreted device code with a slot-resolving sink: the physics formulas
/// exist here once, whichever assembly drives them.
#[allow(clippy::too_many_lines)] // one match arm per device kind
pub fn stamp_device<S: StampSink>(ctx: &StampCtx, id: DeviceId, dev: &Device, sink: &mut S) {
    match dev {
        Device::Resistor {
            a, b, ohms, tc1, ..
        } => {
            // A non-positive resistance is a SHORT, not an open: skipping the
            // stamp would leave the nodes coupled only through gmin (an open
            // with a ~1e12 Ω leak), silently breaking a 0-Ω jumper's net.
            // Clamp to the shared `MIN_RESISTANCE_OHMS` floor (the SPICE loader
            // announces it when it substitutes) so a stiff conductance
            // couples the nodes. Also covers a tc1 derating driving r below 0.
            let r = resistor_value(*ohms, *tc1, ctx).max(hauksbee_ir::MIN_RESISTANCE_OHMS);
            stamp_cond(sink, ctx.layout, *a, *b, 1.0 / r);
        }
        Device::Capacitor {
            a, b, farads, ic, ..
        } => stamp_capacitor(ctx, id, *a, *b, *farads, *ic, sink),
        Device::Inductor {
            a, b, henries, ic, ..
        } => stamp_inductor(ctx, id, *a, *b, *henries, *ic, sink),
        Device::Vsource { p, n, kind, .. } => stamp_vsource(ctx, id, *p, *n, kind, sink),
        Device::Isource { p, n, kind, .. } => {
            let val = source_value(ctx, kind);
            stamp_current(sink, ctx.layout, *p, *n, val * ctx.src_scale);
        }
        Device::Diode { a, k, model, .. } => stamp_diode(ctx, id, *a, *k, model, sink),
        Device::Bjt { c, b, e, model, .. } => stamp_bjt(ctx, id, *c, *b, *e, model, sink),
        Device::Mosfet {
            d,
            g: gate,
            s,
            b,
            model,
            ..
        } => stamp_mosfet(ctx, id, *d, *gate, *s, *b, model, sink),
        Device::VSwitch {
            a,
            b,
            ctrl_p,
            ctrl_n,
            von,
            voff,
            ron,
            roff,
            ..
        } => stamp_vswitch(
            ctx, id, *a, *b, *ctrl_p, *ctrl_n, *von, *voff, *ron, *roff, sink,
        ),
        Device::OpAmp {
            out,
            inp,
            inn,
            reference,
            gain,
            pole_hz,
            slew,
            rail_lo,
            rail_hi,
            supply,
            ..
        } => stamp_opamp(
            ctx,
            id,
            (*out, *inp, *inn, *reference),
            (*gain, *pole_hz, *slew),
            (*rail_lo, *rail_hi),
            *supply,
            sink,
        ),
        Device::Comparator {
            out,
            inp,
            inn,
            out_lo,
            out_hi,
            hysteresis,
            tpd_s,
            supply,
            ..
        } => stamp_comparator(
            ctx,
            id,
            (*out, *inp, *inn),
            (*out_lo, *out_hi, *hysteresis, *tpd_s),
            *supply,
            sink,
        ),
        Device::Vcvs {
            p, n, cp, cn, gain, ..
        } => stamp_vcvs(ctx, id, *p, *n, *cp, *cn, *gain, sink),
        Device::Vccs {
            p, n, cp, cn, gm, ..
        } => {
            // Current gm*(v_cp - v_cn) flows p -> n: +gm at (p,cp)/(n,cn),
            // -gm at (p,cn)/(n,cp). Four matrix entries, no RHS, no unknown.
            // The gain is a device property, not a source value, so it is NOT
            // scaled by the source-stepping homotopy (matching SPICE, which
            // steps independent sources only).
            add_transconductance(
                sink,
                ctx.layout.node(*p),
                ctx.layout.node(*n),
                ctx.layout.node(*cp),
                ctx.layout.node(*cn),
                *gm,
            );
        }
        Device::Cccs {
            p,
            n,
            ctrl_src,
            gain,
            ..
        } => {
            // I(p->n) = gain * i_ctrl, where i_ctrl is the LIVE branch-current
            // unknown of the control Vsource (resolved by the loader's name
            // pass, retargeted by sub-circuit extraction). Two matrix entries
            // in the control branch COLUMN, no RHS, no unknown of its own. The
            // gain is a device property, not a source value: not scaled by the
            // source-stepping homotopy (matching SPICE).
            let cbr = ctx
                .layout
                .branch(*ctrl_src)
                .expect("cccs control source owns a branch unknown");
            if let Some(pi) = ctx.layout.node(*p) {
                sink.g(pi, cbr, *gain);
            }
            if let Some(ni) = ctx.layout.node(*n) {
                sink.g(ni, cbr, -*gain);
            }
        }
        Device::Ccvs {
            p,
            n,
            ctrl_src,
            transres,
            ..
        } => stamp_ccvs(ctx, id, *p, *n, *ctrl_src, *transres, sink),
        Device::Behavioral {
            name,
            p,
            n,
            output,
            expr,
            deps,
        } => stamp_behavioral(ctx, id, name, *p, *n, *output, expr, deps, sink),
        // A coupling stamps NOTHING here on purpose: it is a relationship,
        // not an element. Its physics lives in the WINDINGS' stamps,
        // `stamp_inductor` consults `layout.mutual_partners` (built from the
        // Coupling devices at layout time) for the −M·coeffs.g cross terms
        // and each winding's mutual history voltage. An arm that stamped the
        // cross terms HERE as well would double-count them, so the one home
        // is the inductor stamp and this arm is deliberately inert.
        Device::Coupling { .. } => {}
    }
}

// --- behavioral B-source (nonlinear expression device) -----

thread_local! {
    /// First behavioral-expression fault of the current assembly (device-named,
    /// human-readable). The stamp interface is infallible by design (a sink of
    /// accumulations), so an expression that errors or produces a non-finite
    /// value at some Newton iterate cannot return an error, instead the
    /// faulting device stamps NOTHING, notes itself here, and the solve
    /// drivers check [`take_behavioral_fault`] immediately after each
    /// assembly: the Newton iteration is aborted as non-converged (never
    /// solved against a silently incomplete matrix), a line-search trial
    /// residual reads +inf (a poisoned point is never accepted), and the
    /// final non-convergence refusal carries the device name. Thread-local
    /// because assemblies run on rayon workers in the partitioned engine.
    static BEHAVIORAL_FAULT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

pub fn note_behavioral_fault(name: &str, detail: &str) {
    BEHAVIORAL_FAULT.with(|f| {
        let mut f = f.borrow_mut();
        if f.is_none() {
            *f = Some(format!("behavioral source `{name}`: {detail}"));
        }
    });
}

/// Drain the fault noted by the most recent assembly on this thread, if any.
/// Callers MUST invoke this after every assembly that can contain a
/// [`Device::Behavioral`] and treat `Some` as "this iterate is unusable".
pub fn take_behavioral_fault() -> Option<String> {
    BEHAVIORAL_FAULT.with(|f| f.borrow_mut().take())
}

/// Evaluate a behavioral expression and its finite-difference partials at the
/// dependency values `vals` (slot-aligned with `deps`). Returns
/// `(f, partials)` or a human-readable fault description.
///
/// FD scheme (shipped default; symbolic d/dv is the later
/// upgrade): forward difference per slot with step
/// `delta_k = reltol*|x_k| + floor`, where the floor is the per-quantity
/// convergence floor, `vntol` for a `V(...)` slot, `abstol` for an
/// `I(...)` slot, so the perturbation is always resolvable at the same
/// scale the Newton convergence test cares about. Any non-finite value
/// (IEEE semantics: `1/0 = inf`, `ln(-1) = NaN`, see `CompiledExpr::eval`)
/// or structural eval error is a fault: partial derivatives from a poisoned
/// point must never enter the matrix.
pub fn behavioral_eval_partials(
    expr: &hauksbee_ir::CompiledExpr,
    deps: &[hauksbee_ir::BDep],
    vals: &mut [f64],
    time: f64,
    opts: &SolverOptions,
) -> Result<(f64, Vec<f64>), String> {
    let f0 = match expr.eval(vals, time) {
        Ok(v) if v.is_finite() => v,
        Ok(v) => {
            return Err(format!(
                "expression `{}` evaluated to {v} at the current iterate \
                 (dep values {vals:?}, time {time:.6e})",
                expr.src()
            ))
        }
        Err(e) => return Err(e),
    };
    let mut partials = Vec::with_capacity(deps.len());
    for k in 0..deps.len() {
        let floor = match deps[k] {
            hauksbee_ir::BDep::Volt(_) => opts.vntol,
            hauksbee_ir::BDep::Branch(_) => opts.abstol,
        };
        let x0 = vals[k];
        let delta = opts.reltol.mul_add(x0.abs(), floor);
        vals[k] = x0 + delta;
        let fk = expr.eval(vals, time);
        vals[k] = x0;
        let fk = match fk {
            Ok(v) if v.is_finite() => v,
            Ok(v) => {
                return Err(format!(
                    "expression `{}` evaluated to {v} while probing the \
                     partial for dependency {k} (perturbed value {})",
                    expr.src(),
                    x0 + delta
                ))
            }
            Err(e) => return Err(e),
        };
        let g = (fk - f0) / delta;
        if !g.is_finite() {
            return Err(format!(
                "finite-difference partial for dependency {k} of `{}` is {g}",
                expr.src()
            ));
        }
        partials.push(g);
    }
    Ok((f0, partials))
}

/// Stamp a behavioral B-source: the Newton companion of a nonlinear source.
///
/// With `f = f(x_dep, time)` and FD partials `g_k = df/dx_k` at the current
/// iterate (`x_k0` the iterate's dependency values):
///
/// * **I-output**: `i(p->n) = f`, linearized
///   `i = sum_k g_k*x_k + (f0 - sum_k g_k*x_k0)`. The `g_k` stamp into the
///   p/n rows at each dependency's column (a multi-input VCCS with numeric
///   gains); the constant term is an equivalent current source `p -> n`.
/// * **V-output**: constraint row `v_p - v_n - f = 0` on its own branch
///   unknown (incidence identical to `stamp_vsource`), linearized to
///   `v_p - v_n - sum_k g_k*x_k = f0 - sum_k g_k*x_k0`, `-g_k` at
///   `(branch_row, dep_col)`, the constant on the branch RHS (a VCVS with
///   numeric gains plus a Newton-companion value).
///
/// A dependency column is a node unknown for `V(...)` slots (ground = no
/// column) or the control source's branch unknown for `I(...)` slots. Like
/// every dependent source, the value is a device property: never scaled by
/// source-stepping homotopy (`src_scale`), matching SPICE. The same stamp
/// serves DC (time = 0) and transient; the constraint has no memory. Sense
/// contract: dependency-node ROWS receive nothing (only their columns are
/// referenced), which the conduction cross-check enforces mechanically.
///
/// On an expression fault the device stamps NOTHING and notes a device-named
/// fault for the drivers (see [`BEHAVIORAL_FAULT`]), never a NaN into the
/// matrix.
fn stamp_behavioral<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    name: &str,
    p: NodeId,
    n: NodeId,
    output: hauksbee_ir::BOutput,
    expr: &hauksbee_ir::CompiledExpr,
    deps: &[hauksbee_ir::BDep],
    sink: &mut S,
) {
    // Dependency values and columns at the current iterate.
    let mut vals: Vec<f64> = Vec::with_capacity(deps.len());
    let mut cols: Vec<Option<usize>> = Vec::with_capacity(deps.len());
    for d in deps {
        match d {
            hauksbee_ir::BDep::Volt(dn) => {
                vals.push(ctx.v(*dn));
                cols.push(ctx.layout.node(*dn));
            }
            hauksbee_ir::BDep::Branch(src) => {
                let cbr = ctx
                    .layout
                    .branch(*src)
                    .expect("behavioral I(...) control source owns a branch unknown");
                vals.push(ctx.x[cbr]);
                cols.push(Some(cbr));
            }
        }
    }
    let (f0, partials) = match behavioral_eval_partials(expr, deps, &mut vals, ctx.time, ctx.opts) {
        Ok(r) => r,
        Err(detail) => {
            note_behavioral_fault(name, &detail);
            return;
        }
    };
    // Newton-companion constant: f0 - sum_k g_k * x_k0.
    let mut const_term = f0;
    for (g, x0) in partials.iter().zip(&vals) {
        const_term -= g * x0;
    }
    match output {
        hauksbee_ir::BOutput::Current => {
            let (pi, ni) = (ctx.layout.node(p), ctx.layout.node(n));
            for (g, col) in partials.iter().zip(&cols) {
                if let Some(col) = col {
                    if let Some(pi) = pi {
                        sink.g(pi, *col, *g);
                    }
                    if let Some(ni) = ni {
                        sink.g(ni, *col, -*g);
                    }
                }
            }
            stamp_current(sink, ctx.layout, p, n, const_term);
        }
        hauksbee_ir::BOutput::Voltage => {
            let br = ctx
                .layout
                .branch(id)
                .expect("V-output behavioral source owns a branch unknown");
            stamp_branch_incidence(sink, ctx, br, p, n);
            for (g, col) in partials.iter().zip(&cols) {
                if let Some(col) = col {
                    sink.g(br, *col, -*g);
                }
            }
            sink.i(br, const_term);
        }
    }
}

fn resistor_value(ohms: f64, tc1: Option<f64>, ctx: &StampCtx) -> f64 {
    match tc1 {
        Some(tc) if ctx.opts.effects.temperature => {
            ohms * tc.mul_add(ctx.opts.model_temp() - 27.0, 1.0)
        }
        _ => ohms,
    }
}

fn source_value(ctx: &StampCtx, kind: &SourceKind) -> f64 {
    if ctx.dc {
        kind.dc_value()
    } else {
        kind.eval(ctx.time)
    }
}

// --- reactive companion models ----------------------------------------------

fn stamp_capacitor<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    a: NodeId,
    b: NodeId,
    c: f64,
    ic: Option<f64>,
    sink: &mut S,
) {
    if ctx.dc {
        // For the initial-conditions operating point, pin the capacitor voltage
        // to its IC via a stiff penalty conductance (keeps the pattern fixed).
        if ctx.use_ic {
            if let Some(vic) = ic {
                let gpin = 1e9; // very stiff; drives v across the cap -> vic
                stamp_cond(sink, ctx.layout, a, b, gpin);
                stamp_current(sink, ctx.layout, a, b, -gpin * vic);
                return;
            }
        }
        // Otherwise open circuit at DC; the gmin shunt keeps nodes solvable.
        return;
    }
    let geq = ctx.coeffs.g * c;
    // companion current source from history.
    let v_prev = ctx.state.x1[id.0 as usize];
    let dv_prev = ctx.state.dx1[id.0 as usize];
    let ieq = match ctx.opts.integration {
        Integration::Trapezoidal => {
            // i = geq*v - (geq*v_prev + i_prev), with i_prev = C*dv_prev.
            geq.mul_add(v_prev, c * dv_prev)
        }
        Integration::Gear2 => {
            let v_prev2 = ctx.state.x2[id.0 as usize];
            c * ctx.coeffs.a1.mul_add(v_prev, -(ctx.coeffs.a2 * v_prev2))
        }
        Integration::BackwardEuler => c * ctx.coeffs.a1 * v_prev,
    };
    stamp_cond(sink, ctx.layout, a, b, geq);
    // Equivalent current ieq flows like the capacitor current a->b.
    stamp_current(sink, ctx.layout, a, b, -ieq);
}

fn stamp_inductor<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    a: NodeId,
    b: NodeId,
    l: f64,
    ic: Option<f64>,
    sink: &mut S,
) {
    let br = ctx
        .layout
        .branch(id)
        .expect("inductor has a branch unknown");
    let ai = ctx.layout.node(a);
    let bi = ctx.layout.node(b);
    // KCL: the branch current enters the two node equations (matrix columns).
    if let Some(ai) = ai {
        sink.g(ai, br, 1.0);
    }
    if let Some(bi) = bi {
        sink.g(bi, br, -1.0);
    }

    if ctx.dc && ctx.use_ic {
        if let Some(iic) = ic {
            // Initial-conditions point: pin the branch current, i = iic. The
            // branch row carries no voltage terms in this mode.
            sink.g(br, br, 1.0);
            sink.i(br, iic);
            return;
        }
    }

    // Branch-row voltage relation: v_a - v_b (- req*i) = veq.
    if let Some(ai) = ai {
        sink.g(br, ai, 1.0);
    }
    if let Some(bi) = bi {
        sink.g(br, bi, -1.0);
    }
    if ctx.dc {
        // Steady-state short circuit: v_a - v_b = 0.
        return;
    }
    // Companion: v = req*i + veq, here written in branch row as
    // v_a - v_b - req*i = veq.
    let req = l * ctx.coeffs.g; // L/dt * (rule factor)
    let i_prev = ctx.state.x1[id.0 as usize];
    let di_prev = ctx.state.dx1[id.0 as usize];
    let veq = match ctx.opts.integration {
        Integration::Trapezoidal => req.mul_add(i_prev, l * di_prev),
        Integration::Gear2 => {
            let i_prev2 = ctx.state.x2[id.0 as usize];
            l * ctx.coeffs.a1.mul_add(i_prev, -(ctx.coeffs.a2 * i_prev2))
        }
        Integration::BackwardEuler => l * ctx.coeffs.a1 * i_prev,
    };
    // Branch row: v_a - v_b - req*i = -(req*i_prev + v_prev) = -veq.
    sink.g(br, br, -req);
    sink.i(br, -(veq));

    // Mutual inductance: with couplings the branch relation
    // generalizes verbatim from `v = L·di/dt` to `v_j = Σ_k L_jk·di_k/dt`
    // (L_jj the self term already stamped above, L_jk = M = k·sqrt(Lj·Lk)).
    // Per partner k this adds one matrix cross term −M·coeffs.g at
    // (this branch row, partner branch column), L stamped DIRECTLY, never
    // inverted, so a k=1 group's singular inductance matrix is harmless (the
    // node-incidence terms keep the MNA system regular), plus the partner's
    // history contribution to this winding's veq, the exact mutual analogue
    // of the self expressions above (each history is the PARTNER's own
    // ReactiveState slot; no shared state exists or is needed, because the
    // state advance derives di/dt from divided differences of branch current
    // without touching L). K-free decks take the empty-slice fast path: one
    // branch, zero float ops, bit-identical.
    for &(pid, m) in ctx.layout.mutual_partners(id) {
        let pbr = ctx
            .layout
            .branch(pid)
            .expect("coupled winding owns a branch unknown");
        sink.g(br, pbr, -(m * ctx.coeffs.g));
        let veq_m = match ctx.opts.integration {
            Integration::Trapezoidal => (m * ctx.coeffs.g).mul_add(
                ctx.state.x1[pid.0 as usize],
                m * ctx.state.dx1[pid.0 as usize],
            ),
            Integration::Gear2 => {
                m * ctx.coeffs.a1.mul_add(
                    ctx.state.x1[pid.0 as usize],
                    -(ctx.coeffs.a2 * ctx.state.x2[pid.0 as usize]),
                )
            }
            Integration::BackwardEuler => m * ctx.coeffs.a1 * ctx.state.x1[pid.0 as usize],
        };
        sink.i(br, -veq_m);
    }
}

/// The branch incidence an ideal voltage-source branch `br` shares with
/// every voltage-defined device: `+1` in the p KCL row and `-1` in the n row,
/// mirrored in the branch row.
fn stamp_branch_incidence<S: StampSink>(
    sink: &mut S,
    ctx: &StampCtx,
    br: usize,
    p: NodeId,
    n: NodeId,
) {
    if let Some(pi) = ctx.layout.node(p) {
        sink.g(pi, br, 1.0);
        sink.g(br, pi, 1.0);
    }
    if let Some(ni) = ctx.layout.node(n) {
        sink.g(ni, br, -1.0);
        sink.g(br, ni, -1.0);
    }
}

fn stamp_vsource<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    p: NodeId,
    n: NodeId,
    kind: &SourceKind,
    sink: &mut S,
) {
    let br = ctx.layout.branch(id).expect("vsource has a branch unknown");
    stamp_branch_incidence(sink, ctx, br, p, n);
    let val = source_value(ctx, kind) * ctx.src_scale;
    sink.i(br, val);
}

/// VCVS: the branch-current unknown pattern of an ideal Vsource, with the
/// constraint row `v_p - v_n - gain*(v_cp - v_cn) = 0` in place of a fixed
/// value. The branch incidence (`±1` in the p/n KCL rows and the branch row)
/// is identical to `stamp_vsource`; the control terms land in the branch ROW
/// only (columns cp/cn), so the control pair's own rows stay untouched, that
/// is the sense-terminal contract `Device::sense_nodes` declares. The RHS is
/// zero (nothing time-varying, nothing to `src_scale`: the gain is a device
/// property, not an independent source value). The same stamp serves DC and
/// transient; the constraint has no memory.
fn stamp_vcvs<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    p: NodeId,
    n: NodeId,
    cp: NodeId,
    cn: NodeId,
    gain: f64,
    sink: &mut S,
) {
    let br = ctx.layout.branch(id).expect("vcvs has a branch unknown");
    stamp_branch_incidence(sink, ctx, br, p, n);
    if let Some(cpi) = ctx.layout.node(cp) {
        sink.g(br, cpi, -gain);
    }
    if let Some(cni) = ctx.layout.node(cn) {
        sink.g(br, cni, gain);
    }
}

/// CCVS: an ideal-Vsource branch pattern whose constraint row reads
/// `v_p - v_n - transres*i_ctrl = 0`. The branch incidence (±1 in the p/n KCL
/// rows and mirrored in the branch row) is identical to `stamp_vsource`; the
/// dependence lands as a single `-transres` entry in H's OWN branch row at the
/// CONTROL source's branch COLUMN, reading another device's unknown, never
/// touching the control loop's node rows (the control coupling is declared via
/// `Device::controlling_source`, not `sense_nodes`, precisely because it is a
/// branch-current read). RHS is zero (the transresistance is a device
/// property, not an independent source value, no `src_scale`). Same stamp for
/// DC and transient: the constraint has no memory.
fn stamp_ccvs<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    p: NodeId,
    n: NodeId,
    ctrl_src: DeviceId,
    transres: f64,
    sink: &mut S,
) {
    let br = ctx.layout.branch(id).expect("ccvs has a branch unknown");
    let cbr = ctx
        .layout
        .branch(ctrl_src)
        .expect("ccvs control source owns a branch unknown");
    stamp_branch_incidence(sink, ctx, br, p, n);
    sink.g(br, cbr, -transres);
}

// --- nonlinear devices ------------------------------------------------------

/// Junction limiting (pnjlim): bound the per-iteration change in junction
/// voltage to keep `exp()` from overflowing and to damp Newton oscillation.
fn pnjlim(vnew: f64, vold: f64, vt: f64, vcrit: f64) -> f64 {
    if vnew > vcrit && (vnew - vold).abs() > 2.0 * vt {
        if vold > 0.0 {
            let arg = 1.0 + (vnew - vold) / vt;
            if arg > 0.0 {
                vt.mul_add(arg.ln(), vold)
            } else {
                vcrit
            }
        } else {
            vt * (vnew / vt).ln()
        }
    } else {
        vnew
    }
}

/// Critical voltage where the diode curve has maximum curvature, the natural
/// clamp point for limiting.
fn vcrit(is: f64, vt: f64) -> f64 {
    vt * (vt / (std::f64::consts::SQRT_2 * is)).ln()
}

/// Evaluate the diode current and conductance at junction voltage `vd`.
///
/// Three regions:
/// * forward / weak reverse (`vd >= -3 n·Vt`): the Shockley exponential;
/// * reverse (`-bv <= vd < -3 n·Vt`): tiny constant leakage `-Is`;
/// * breakdown (`vd < -bv`, only when the model gives a finite `bv`):
///   `i = -IBV·exp(-(bv+vd)/(n·Vt))`, ngspice's own shape. The SIGN is the
///   point: the current is REVERSE (negative, cathode->anode) and grows
///   exponentially as `vd` drops below `-bv`. `IBV` is the current the knee is
///   placed at, and it is what makes a Zener regulate: a datasheet states its
///   voltage AT a test current, and scaling breakdown by `Is` instead puts a
///   5.1 V part at 5.8 V under a normal shunt load. A model that names no
///   `ibv` falls back to `Is`, which is continuous at `vd == -bv` (both
///   branches give `-Is` there). The
///   exponent is clamped like the BJT's (`exp(40)·Is` bounds the current to a
///   few kA at typical `Is`), and `stamp_diode` additionally junction-limits
///   the per-iteration move in MIRRORED coordinates (see there), so a Newton
///   iterate far past breakdown cannot poison the solve. `bv` defaults to
///   infinity, which makes the breakdown branch unreachable: existing decks
///   are bit-identical.
pub fn diode_eval(model: &DiodeModel, vd: f64, t_c: f64, temp_on: bool) -> (f64, f64) {
    let is = if temp_on { model.is_at(t_c) } else { model.is };
    let nvt = model.n * hauksbee_ir_thermal(t_c, temp_on);
    if vd >= -3.0 * nvt {
        let e = (vd / nvt).exp();
        let id = is * (e - 1.0);
        let gd = is * e / nvt;
        (id, gd)
    } else if !model.bv.is_finite() {
        // Reverse region with no breakdown modelled: tiny linear leakage,
        // conductance ~ gmin handled outside.
        let id = -is;
        let gd = is / nvt * 1e-3;
        (id, gd)
    } else {
        // Reverse, with breakdown: leakage PLUS the breakdown exponential, as
        // one continuous expression rather than two regions meeting at -bv.
        //
        // The two only meet there when the breakdown scale equals `is`. A Zener
        // states its knee at a test current (49 mA for a 1N4733A), so `ibv` is
        // seven orders above `is` and a hard switch at -bv is a step
        // discontinuity of that size. Newton oscillates across it and one
        // iterate lands deep enough that the clamped exponential evaluates to
        // ibv*exp(40), which is 1e16 A. Summing is also the more honest curve:
        // above the knee the exponential is negligible and the leakage is what
        // is left, below it the exponential dominates.
        let ibv = model.ibv.filter(|v| *v > 0.0).unwrap_or(is);
        let e = (-(model.bv + vd) / nvt).min(40.0).exp();
        let id = -(is + ibv * e);
        let gd = (is / nvt).mul_add(1e-3, ibv * e / nvt);
        (id, gd)
    }
}

/// SPICE's standard depletion-capacitance knee: below `FC·vj` the physical
/// `Cj(v) = cjo (1 - v/vj)^-m` applies; above it the singular expression is
/// replaced by its tangent-matched linear continuation.
pub const DIODE_FC: f64 = 0.5;

/// Whether a diode carries charge under the current effects toggles: the
/// `junction_caps` gate AND at least one charge-producing model field. A
/// default model (`cjo == 0`, `tt == 0`) never stores charge, so decks whose
/// diode models predate this physics are bit-identical whatever the toggle.
#[inline]
pub fn diode_has_charge(model: &DiodeModel, effects: &crate::options::DeviceEffects) -> bool {
    effects.junction_caps && (model.cjo > 0.0 || model.tt > 0.0)
}

/// Diode stored charge `Q(vd)` and its derivative `C(vd) = dQ/dvd`, the pair
/// the charge-based companion model integrates. `id`/`gd` are the junction
/// current and conductance already evaluated at the same `vd` (diffusion
/// charge is `tt·id`, diffusion capacitance `tt·gd`).
///
/// Junction (depletion) part, SPICE-standard with `FC = 0.5`:
/// * `vd < FC·vj`:  `Cj = cjo·(1 - vd/vj)^-m`,
///   `Qj = cjo·vj·(1 - (1 - vd/vj)^(1-m))/(1-m)` (log form at `m == 1`);
/// * `vd >= FC·vj`: the linearized continuation
///   `Cj = cjo·(F3 + m·vd/vj)/F2`,
///   `Qj = cjo·(F1 + (F3·(vd - FC·vj) + (m/2vj)·(vd² - (FC·vj)²))/F2)`
///   with `F1 = vj·(1-(1-FC)^(1-m))/(1-m)`, `F2 = (1-FC)^(1+m)`,
///   `F3 = 1 - FC·(1+m)`.
///
/// At the knee both `Cj` and `dCj/dv` match (`Cj(FC·vj) = cjo·(1-FC)^-m`,
/// slope `cjo·m/vj·(1-FC)^-(1+m)` from either side), so `Q` is C²-continuous:
/// Newton sees no derivative kink to chatter on. The continuation exists
/// because the physical form is singular at `vd == vj` and a forward-biased
/// junction crosses `FC·vj` on every switching edge.
///
/// Stamping from `Q` (not from `C·dv/dt` with `C` frozen over the step) is
/// what conserves charge for a NONLINEAR capacitance: the companion integrates
/// `i = dQ/dt` exactly in `Q`, and Newton linearizes `Q(v)` afresh each
/// iteration with `dQ/dv = C(v)`.
pub fn diode_charge(model: &DiodeModel, vd: f64, id: f64, gd: f64) -> (f64, f64) {
    let mut q = 0.0;
    let mut c = 0.0;
    if model.cjo > 0.0 {
        let (qj, cj) = depletion_charge(model.cjo, model.vj, model.m, DIODE_FC, vd);
        c += cj;
        q += qj;
    }
    if model.tt > 0.0 {
        q += model.tt * id;
        c += model.tt * gd;
    }
    (q, c)
}

/// SPICE depletion charge/capacitance for one graded junction: `(Qj, Cj)` at
/// junction voltage `vd`, with the `fc` knee's linear continuation documented
/// on [`diode_charge`]. Shared by the diode (`cjo/vj/m` from its model card)
/// and the BJT's two junctions (`cje`/`cjc` with the card's own `vje`/`vjc`,
/// `mje`/`mjc` and `fc`); the arithmetic is the diode's original,
/// expression-for-expression, so extracting it moved no diode bit.
pub fn depletion_charge(cjo: f64, vj: f64, m: f64, fc: f64, vd: f64) -> (f64, f64) {
    let fcv = fc * vj;
    if vd < fcv {
        let arg = 1.0 - vd / vj;
        let sarg = arg.powf(-m);
        let c = cjo * sarg;
        let q = if (1.0 - m).abs() < 1e-9 {
            -cjo * vj * arg.ln()
        } else {
            // arg^(1-m) == arg * arg^-m, reusing the power already paid.
            cjo * vj * (1.0 - arg * sarg) / (1.0 - m)
        };
        (q, c)
    } else {
        let f2 = (1.0 - fc).powf(1.0 + m);
        let f3 = fc.mul_add(-(1.0 + m), 1.0);
        let f1 = if (1.0 - m).abs() < 1e-9 {
            -vj * (1.0 - fc).ln()
        } else {
            vj * (1.0 - (1.0 - fc).powf(1.0 - m)) / (1.0 - m)
        };
        let c = cjo * (f3 + m * vd / vj) / f2;
        let q =
            cjo * (f1 + (f3 * (vd - fcv) + (m / (2.0 * vj)) * vd.mul_add(vd, -(fcv * fcv))) / f2);
        (q, c)
    }
}

/// Whether a BJT stores charge under the current effects toggles: the
/// `junction_caps` gate AND at least one charge-producing model field. A
/// default model (`cje == cjc == tf == tr == 0`) never stores charge, so
/// decks whose BJT models predate this physics are bit-identical whatever
/// the toggle; the same contract [`diode_has_charge`] pins for diodes.
#[inline]
pub fn bjt_has_charge(model: &BjtModel, effects: &crate::options::DeviceEffects) -> bool {
    effects.junction_caps
        && (model.cje > 0.0 || model.cjc > 0.0 || model.tf > 0.0 || model.tr > 0.0)
}

/// BJT base-emitter stored charge `Q(vbe)` and capacitance `dQ/dvbe`, in
/// POLARITY-FOLDED (NPN-reference) space: depletion (`cje`, SPICE-default
/// knee) plus forward diffusion `tf·i_cc`, where `i_cc = cf` is the forward
/// transport current the Gummel-Poon core already evaluates (`gif` its
/// tangent); the transit-time charge rides the core's own transport
/// evaluation instead of recomputing it.
pub fn bjt_charge_be(model: &BjtModel, vbe: f64, cf: f64, gif: f64) -> (f64, f64) {
    let mut q = 0.0;
    let mut c = 0.0;
    if model.cje > 0.0 {
        let (qj, cj) = depletion_charge(model.cje, model.vje, model.mje, model.fc, vbe);
        q += qj;
        c += cj;
    }
    if model.tf > 0.0 {
        q += model.tf * cf;
        c += model.tf * gif;
    }
    (q, c)
}

/// BJT base-collector stored charge and capacitance (folded space):
/// depletion (`cjc`) plus reverse diffusion `tr·i_ec` with `i_ec = cr` the
/// reverse transport current (`gir` its tangent).
pub fn bjt_charge_bc(model: &BjtModel, vbc: f64, cr: f64, gir: f64) -> (f64, f64) {
    let mut q = 0.0;
    let mut c = 0.0;
    if model.cjc > 0.0 {
        let (qj, cj) = depletion_charge(model.cjc, model.vjc, model.mjc, model.fc, vbc);
        q += qj;
        c += cj;
    }
    if model.tr > 0.0 {
        q += model.tr * cr;
        c += model.tr * gir;
    }
    (q, c)
}

/// Both BJT stored charges `(Q_be, Q_bc)` at FOLDED junction voltages,
/// recomputing the transport currents through the same clamped exponentials
/// the stamp uses; the seed/advance/LTE entry point (the stamp itself reuses
/// its already-evaluated `cf`/`gif`/`cr`/`gir` via the per-junction helpers).
pub fn bjt_charges_at(model: &BjtModel, vbe: f64, vbc: f64, t_c: f64, temp_on: bool) -> (f64, f64) {
    let is = if temp_on { model.is_at(t_c) } else { model.is };
    let vt = hauksbee_ir_thermal(t_c, temp_on);
    let nvf = model.nf * vt;
    let nvr = model.nr * vt;
    let ef = (vbe / nvf).clamp(-40.0, 40.0).exp();
    let er = (vbc / nvr).clamp(-40.0, 40.0).exp();
    let cf = is * (ef - 1.0);
    let cr = is * (er - 1.0);
    let gif = is * ef / nvf;
    let gir = is * er / nvr;
    (
        bjt_charge_be(model, vbe, cf, gif).0,
        bjt_charge_bc(model, vbc, cr, gir).0,
    )
}

/// FOLDED junction voltages `(vbe, vbc)` of a BJT at the solution vector `x`,
/// measured at the INTRINSIC (internal) nodes when series resistance is
/// stamped; the voltages its stored charges are functions of. This is the
/// single node-resolution rule the transient/partitioned seed-advance arms
/// and the AC stamp share with `stamp_bjt`: internal unknowns when the
/// `series_resistance` toggle is on AND the layout allocated them, the
/// external nodes otherwise.
#[allow(clippy::many_single_char_names)] // names follow SPICE junction/terminal notation (vbe/vbc, d/g/s/b)
pub fn bjt_junction_voltages(
    layout: &Layout,
    x: &[f64],
    id: DeviceId,
    c: NodeId,
    b: NodeId,
    e: NodeId,
    model: &BjtModel,
    effects: &crate::options::DeviceEffects,
) -> (f64, f64) {
    let [ci, bi, ei] = bjt_effective_nodes(layout, id, c, b, e, effects);
    let v = |i: Option<usize>| i.map_or(0.0, |i| x[i]);
    let sign = model.polarity.sign();
    (sign * (v(bi) - v(ei)), sign * (v(bi) - v(ci)))
}

/// The unknown indices the Gummel-Poon core (and the charges) live on:
/// `[c, b, e]` as internal-node indices where the layout allocated them and
/// the `series_resistance` toggle honors them, external node indices
/// otherwise (per terminal, a zero-valued resistance allocated no internal
/// node and stays external).
pub fn bjt_effective_nodes(
    layout: &Layout,
    id: DeviceId,
    c: NodeId,
    b: NodeId,
    e: NodeId,
    effects: &crate::options::DeviceEffects,
) -> [Option<usize>; 3] {
    let ext = [layout.node(c), layout.node(b), layout.node(e)];
    if !effects.series_resistance {
        return ext;
    }
    layout.bjt_internal(id).map_or(ext, |ints| {
        [ints[0].or(ext[0]), ints[1].or(ext[1]), ints[2].or(ext[2])]
    })
}

// --- MOSFET charge helpers --------------------------------

/// Meyer-limit intrinsic gate-drain capacitance fraction: `Cgd -> (1/2)·Cox`
/// in deep triode (Meyer's `vds -> 0` limit); zero in saturation/cutoff where
/// only the overlap remains; the Miller-charge asymmetry that sets switching
/// timing.
pub const MOS_CGD_MEYER: f64 = 0.5;

/// Whether a MOSFET stores charge under the current effects toggles: the
/// `junction_caps` gate AND at least one charge-producing model field (gate
/// overlap/oxide caps or bulk depletion caps). A default model never stores
/// charge, so decks whose MOS models predate this physics are bit-identical
/// whatever the toggle; the [`diode_has_charge`]/[`bjt_has_charge`] contract.
#[inline]
pub fn mos_has_charge(model: &MosfetModel, effects: &crate::options::DeviceEffects) -> bool {
    effects.junction_caps && (model.has_gate_charge() || model.cbd > 0.0 || model.cbs > 0.0)
}

/// Numerically stable `ln(1 + e^x)` (softplus).
#[inline]
fn softplus(x: f64) -> f64 {
    if x > 40.0 {
        x
    } else if x < -40.0 {
        x.exp()
    } else {
        x.exp().ln_1p()
    }
}

/// Numerically stable logistic `1 / (1 + e^-x)`.
#[inline]
fn sigmoid(x: f64) -> f64 {
    if x >= 0.0 {
        1.0 / (1.0 + (-x).exp())
    } else {
        let e = x.exp();
        e / (1.0 + e)
    }
}

/// SGP base-charge Early factor `1 - vbc/VAF - vbe/VAR`: exactly 1.0 with the
/// effect off, and each term only for a finite Early voltage.
pub fn bjt_early(model: &BjtModel, on: bool, vbe: f64, vbc: f64) -> f64 {
    let mut e = 1.0;
    if on && model.vaf.is_finite() {
        e -= vbc / model.vaf;
    }
    if on && model.var.is_finite() {
        e -= vbe / model.var;
    }
    e
}

/// The Early factor's contributions `(∂/∂vbc, ∂/∂vbe)` to the transport-current
/// row, `(cf-cr)·∂q1_inv`: zero unless `free` and that Early voltage is finite.
pub fn bjt_early_partials(model: &BjtModel, free: bool, cf: f64, cr: f64) -> (f64, f64) {
    let d = |v: f64| {
        if free && v.is_finite() {
            -(cf - cr) / v
        } else {
            0.0
        }
    };
    (d(model.vaf), d(model.var))
}

/// Body-effect threshold `(vth, dvth/dvbs)` at the folded bulk-to-source
/// voltage `vbs`, for a model with `gamma > 0`.
#[must_use]
pub fn mos_body_vth(model: &MosfetModel, vbs: f64) -> (f64, f64) {
    let phi = model.phi.max(1e-6);
    let arg = phi - vbs;
    if arg > 0.0 {
        let sq = arg.sqrt();
        // Guard the sqrt singularity at vbs -> phi: cap the derivative at
        // the value it has 1 mV short of the pole (vth itself stays exact).
        let dvth = -model.gamma / (2.0 * sq.max(1e-3 * phi.sqrt()));
        (model.gamma.mul_add(sq - phi.sqrt(), model.vto), dvth)
    } else {
        // Forward-biased bulk clamped at phi: threshold floor, no slope.
        (model.gamma.mul_add(0.0 - phi.sqrt(), model.vto), 0.0)
    }
}

/// Level-1 channel current and tangents `(id, gm, gds)`.
///
/// Taken at FOLDED, post-swap voltages (`vov = vgs - vth`, `vds >= 0`), shared
/// by the transient stamp and the AC tangent so the two linearizations CANNOT
/// drift apart.
///
/// Shichman-Hodges in forward/reverse (source-end minus drain-end) form:
///
///   `id = beta/2 · [f(vov)² - f(vov - vds)²] · (1 + lambda·vds)`
///
/// with the overdrive join `f(x) = x` for `x >= w` and `f(x) = w·e^(x/w - 1)`
/// below, `w = 2·n·Vt`: the only C1 junction of the square law with an
/// exponential whose current slope is n·Vt. With the source end at least `w`
/// past threshold this IS Level 1: triode `beta·(vov·vds - vds²/2)` while the
/// drain end is too, saturation `beta/2·vov²` once the drain-end term has
/// decayed (it e-folds per n·Vt beyond pinch-off). So a switch 0.07 V over
/// threshold in saturation carries ngspice's current exactly, where the
/// former softplus overdrive carried 1.36x at 0.09 V and 1.73x at 2·n·Vt.
/// Anywhere else the join exceeds `max(x, 0)` by at most `0.162·w²` in `f²`
/// (its worst point, a fifth of the way up), so
/// `|id - id_level1| <= 0.081·beta·w²·(1 + lambda·vds)`, an excess that lives
/// within a few n·Vt of threshold at either channel end.
///
/// Below threshold the same expression is the physical subthreshold channel,
/// `i_s·e^(vov/(n·Vt))·(1 - e^(-vds/(n·Vt)))`, saturating within a few n·Vt of
/// drain bias. A regional form with saturation voltage `f(vov)` shrinks the
/// cutoff triode window to microvolts instead, so an off device's drain
/// conductance collapses a millionfold within microvolts of its source: a
/// floating drain (an off P-FET stage whose drain nothing else loads) then
/// limit-cycles Newton through every homotopy rung. Both ends share `f`, so
/// `id == 0` at `vds == 0` with the same `gds` from either side of the
/// drain/source swap, and `id`, `gm` and `gds` are continuous everywhere
/// (their slopes jump only where a channel end crosses the join).
/// Channel-length modulation multiplies every region including the tail.
#[inline]
#[must_use]
pub fn mos_channel(
    beta: f64,
    nvt: f64,
    vov: f64,
    vds: f64,
    lambda: f64,
    gmin: f64,
) -> (f64, f64, f64) {
    let w = 2.0 * nvt;
    let join = |x: f64| {
        let e = (x / w - 1.0).min(0.0).exp(); // f'(x), 1 above the join
        (if x >= w { x } else { w * e }, e)
    };
    let ((ff, dff), (fr, dfr)) = (join(vov), join(vov - vds));
    let clm = lambda.mul_add(vds, 1.0);
    let i0 = 0.5 * beta * (ff - fr) * (ff + fr);
    let gm = beta * ff.mul_add(dff, -fr * dfr) * clm;
    let gds = (beta * fr * dfr).mul_add(clm, i0 * lambda);
    (i0 * clm, gm.max(gmin), gds.max(gmin))
}

/// THE CHARGE-MODEL CHOICE: the gate
/// charges are the "simpler-but-honest" per-junction alternative to true
/// Meyer capacitances, for a structural reason, not just a stability one.
/// Meyer's `Cgs(vgs, vds)`/`Cgd(vgs, vds)` depend on TWO junction voltages,
/// so they do not integrate to a per-junction two-terminal `Q(v)` at all
/// (the classic Meyer charge-non-conservation problem); the solver's
/// reactive machinery is charge-based `Q(v)` companions precisely so
/// nonlinear capacitance conserves charge. Instead each gate junction gets a
/// genuine `Q(v)` whose capacitance matches Meyer's REGION LIMITS, smoothly
/// interpolated over a `delta`-wide transition at the threshold (`softplus`
/// makes Q C-infinity-smooth, so Newton sees continuous C and dC/dv through
/// every switching edge; the FC-knee discipline applied to the region
/// switch):
///
/// * [`mos_charge_gs`]: `C = c_ov + c_ox` BELOW threshold falling to
///   `c_ov + (1/2)·c_ox` above. The below-threshold `c_ox` is Meyer's
///   CUTOFF GATE-BULK capacitance (`Cgb = Cox`) REFERENCED TO THE SOURCE:
///   in the target case (discrete MOSFET, bulk tied to source) that is
///   electrically exact, and dropping it instead (the first cut of this
///   model) made every gate charge ~`Cox` too fast below threshold; the
///   switching edges led ngspice by the whole subthreshold gate-RC
///   (measured on the load-switch decks: worst pointwise errors 5.45
///   NMOS / 54.5 PMOS without it). The ON value is Meyer's DEEP-TRIODE
///   limit `Cox/2`, not the saturation `2/3·Cox`: a switch dwells in
///   triode whenever it is on and only transits saturation briefly during
///   the drain swing, and the choice is cross-check-driven, with `2/3`
///   the load-switch turn-off edge lagged ngspice by 5-8 ns (gate
///   discharging through an over-stated triode Cgs; worst pointwise error
///   0.76), with `1/2` the same edge aligns to 0.8 ns (worst 0.32,
///   mid-edge).
/// * [`mos_charge_gd`]: `C = c_ov` in cutoff/saturation rising to
///   `c_ov + (1/2)·c_ox` in triode (`vgd` crossing the threshold is the
///   drain end of the channel forming); the Miller-charge asymmetry that
///   sets switching timing.
///
/// What is given up vs Meyer is the region interpolation only a
/// two-voltage C can express (Meyer's Cgs rises to `2/3·Cox` in
/// saturation; ours holds the triode `Cox/2` there), bounded and visible
/// as nanosecond-scale edge skew in the ngspice cross-check tolerances of
/// those decks.
#[inline]
#[allow(clippy::similar_names)] // c_ov / c_ox: overlap vs oxide capacitance
pub fn mos_charge_gs(c_ov: f64, c_ox: f64, vth: f64, delta: f64, v: f64) -> (f64, f64) {
    let mut q = c_ov * v;
    let mut c = c_ov;
    if c_ox > 0.0 {
        let u = (v - vth) / delta;
        // C falls from c_ox to (1/2)c_ox across the threshold:
        //   C = c_ox·(1 - sigmoid(u)/2),  Q = c_ox·((v-vth) - delta·softplus(u)/2)
        // (charge referenced to v = vth; the companion only ever uses dQ).
        q += c_ox * ((v - vth) - delta * softplus(u) / 2.0);
        c += c_ox * (1.0 - sigmoid(u) / 2.0);
    }
    (q, c)
}

/// Gate-drain stored charge: overlap plus the triode-side intrinsic rise
/// (see [`mos_charge_gs`] for the model choice discussion).
#[inline]
#[allow(clippy::similar_names)] // c_ov / c_ox: overlap vs oxide capacitance
pub fn mos_charge_gd(c_ov: f64, c_ox: f64, vth: f64, delta: f64, v: f64) -> (f64, f64) {
    let mut q = c_ov * v;
    let mut c = c_ov;
    if c_ox > 0.0 {
        let u = (v - vth) / delta;
        q += MOS_CGD_MEYER * c_ox * delta * softplus(u);
        c += MOS_CGD_MEYER * c_ox * sigmoid(u);
    }
    (q, c)
}

/// All four MOSFET stored charges `(Q_gs, Q_gd, Q_bd, Q_bs)` at FOLDED
/// junction voltages; the seed/advance/LTE entry point, computing exactly
/// what the stamp's companions integrate. Gate charges are the
/// [`mos_gate_charge`] shape at the zero-bias threshold `vto` (the charge
/// region switch deliberately ignores the body effect: the common
/// bulk-tied-to-source switch has none, and a body-biased charge switch
/// would couple three terminals into what must stay a two-terminal `Q(v)`);
/// bulk charges are the depletion helper with the model's `pb`/`mj`.
#[allow(clippy::similar_names)] // names follow SPICE junction/terminal notation (vbe/vbc, d/g/s/b)
pub fn mos_charges_at(
    model: &MosfetModel,
    vgs: f64,
    vgd: f64,
    vbd: f64,
    vbs: f64,
    t_c: f64,
    temp_on: bool,
) -> (f64, f64, f64, f64) {
    let vt = hauksbee_ir_thermal(t_c, temp_on);
    let delta = 2.0 * model.n_sub.max(1.0) * vt;
    let q_gs = mos_charge_gs(model.cgs_ov, model.c_ox, model.vto, delta, vgs).0;
    let q_gd = mos_charge_gd(model.cgd_ov, model.c_ox, model.vto, delta, vgd).0;
    let q_bulk = |cbx: f64, v: f64| {
        if cbx > 0.0 {
            depletion_charge(cbx, model.pb, model.mj, DIODE_FC, v).0
        } else {
            0.0
        }
    };
    (q_gs, q_gd, q_bulk(model.cbd, vbd), q_bulk(model.cbs, vbs))
}

/// FOLDED junction voltages `(vgs, vgd, vbd, vbs)` of a MOSFET at the
/// solution vector `x`, at the PHYSICAL terminals (no drain/source symmetry
/// swap: the swap is a channel-evaluation device; the charges live on the
/// real gate/bulk junctions). Bulk defaults to source when absent, exactly
/// as the stamp resolves it. Drain and source are read at the stamp's
/// EFFECTIVE unknowns (internal behind rd/rs when `series_r` is on): the
/// history must hold the charge the companion integrates. Read at the
/// external pins instead, the Olimex EVB's FET3 (rs = 30 mOhm, 480 pF Cgs)
/// stored a charge off by `Cgs*I*rs` every step, which the trapezoid turned
/// into a ~10 mA gate current growing as `2/dt` and a self-sustaining
/// 300 MHz ring with its gate capacitor's ESL.
#[allow(clippy::many_single_char_names)] // names follow SPICE junction/terminal notation (vbe/vbc, d/g/s/b)
pub fn mos_junction_voltages(
    layout: &Layout,
    x: &[f64],
    id: DeviceId,
    [d, g, s]: [NodeId; 3],
    b: Option<NodeId>,
    model: &MosfetModel,
    series_r: bool,
) -> (f64, f64, f64, f64) {
    let v = |n: NodeId| layout.node(n).map_or(0.0, |i| x[i]);
    let ints = layout.mos_internal(id).filter(|_| series_r);
    let at = |k: usize, n: NodeId| ints.and_then(|i| i[k]).map_or_else(|| v(n), |i| x[i]);
    let (vd, vs) = (at(0, d), at(1, s));
    let sign = model.polarity.sign();
    let vb = v(b.unwrap_or(s));
    (
        sign * (v(g) - vs),
        sign * (v(g) - vd),
        sign * (vb - vd),
        sign * (vb - vs),
    )
}

/// Thermal voltage helper that respects the temperature toggle.
fn hauksbee_ir_thermal(t_c: f64, temp_on: bool) -> f64 {
    let t = if temp_on { t_c } else { 27.0 };
    hauksbee_ir::thermal_voltage_c(t)
}

fn stamp_diode<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    a: NodeId,
    k: NodeId,
    model: &DiodeModel,
    sink: &mut S,
) {
    let temp_on = ctx.opts.effects.temperature;
    let t_c = ctx.opts.model_temp();
    let is = if temp_on { model.is_at(t_c) } else { model.is };
    let vt = hauksbee_ir_thermal(t_c, temp_on);
    let nvt = model.n * vt;
    let vc = vcrit(is, nvt);
    // The breakdown branch is scaled by IBV, not IS, so it needs its own
    // critical voltage. Limiting a knee placed at 49 mA against a vcrit derived
    // from a 1 nA saturation current lets a Newton iterate land far enough past
    // -bv that the clamped exponential still evaluates to IBV*exp(40), which is
    // 1e16 A and destroys the step.
    let ibv = model.ibv.filter(|v| *v > 0.0).unwrap_or(is);
    let vc_bd = vcrit(ibv, nvt);

    // Series resistance: the junction moves onto an intrinsic anode and `rs`
    // bridges it to the external one, the shape the BJT's rb/re/rc and the
    // MOSFET's rd/rs already use. It is not a refinement for the parts that
    // carry a real one: a power Schottky's whole 1 A to 3 A rise is this
    // resistance rather than the exponential, and an LED at rs = 6 drops
    // 120 mV at 20 mA.
    let a_ext = ctx.layout.node(a);
    let k_i = ctx.layout.node(k);
    let mut a_eff = a_ext;
    if let Some(a_int) = ctx.layout.diode_internal(id) {
        if ctx.opts.effects.series_resistance && model.rs > 0.0 {
            stamp_cond_idx(sink, a_ext, Some(a_int), 1.0 / model.rs);
            a_eff = Some(a_int);
        } else {
            sink.g(a_int, a_int, 1.0);
        }
    }
    // Read the junction at the EFFECTIVE unknowns. With no series resistor
    // `a_eff` IS `layout.node(a)`, reproducing `ctx.v(a) - ctx.v(k)` exactly:
    // the bit-identity fast path every existing deck takes.
    let vx = |i: Option<usize>| i.map_or(0.0, |i| ctx.x[i]);
    let vx_prev = |i: Option<usize>| i.map_or(0.0, |i| ctx.x_prev[i]);
    let vd_raw = vx(a_eff) - vx(k_i);
    // The limiting anchor: the last EVALUATION voltage where the live Newton
    // threads its junction state (see StampCtx::junction_eval), the previous
    // iterate otherwise.
    let vd_iter_prev = vx_prev(a_eff) - vx_prev(k_i);
    let vd_last = ctx
        .junction_eval
        .map_or(vd_iter_prev, |je| je.anchor(id, 0, vd_iter_prev));
    // Junction limiting. The forward exponential is limited by pnjlim toward
    // vcrit; the breakdown exponential is the same curve MIRRORED about
    // vd = -bv, so in the breakdown region (vd below -bv, with a 10·nvt skirt,
    // ngspice's region test) the per-iteration move is limited in mirrored
    // coordinates vdtemp = -(vd + bv), pnjlim toward the same vcrit, and
    // mapped back. Without this, one Newton iterate landing volts past -bv
    // evaluates exp() at full depth and poisons the step exactly the way an
    // unlimited forward branch would. `bv == INFINITY` (the default) makes
    // the region test unsatisfiable, keeping every deck without a breakdown
    // voltage on the plain pnjlim path.
    let mut vd = if model.bv.is_finite() && vd_raw < 10.0f64.mul_add(nvt, -model.bv).min(0.0) {
        -(pnjlim(-(vd_raw + model.bv), -(vd_last + model.bv), nvt, vc_bd) + model.bv)
    } else {
        pnjlim(vd_raw, vd_last, nvt, vc)
    };

    // STAGED-DC diode pre-limit (the relaxed-seed overflow cure). On the staged
    // path only (branch_reg > 0), HARD-CLAMP the junction voltage so the very
    // FIRST stamp from the relaxed (diodes-off) seed cannot overflow exp(). The
    // relaxed seed leaves some stretcher-diode cathodes floating; their first
    // forward bias can be tens of volts, and on iteration 1 pnjlim sees a zero
    // delta (anchor == iterate) so it does not limit, diode_eval then evaluates
    // exp(vd/nvt) ≈ 1e30 A and poisons the Newton step (measured: dc_residual
    // ~1e30 A at the V_out nets). Clamping vd to a few hundred mV past vcrit
    // bounds the current to a sane magnitude without changing the root: a real
    // forward junction sits ~0.6–0.9 V, far below this clamp, so at any genuine
    // operating point the clamp is inactive and the solve is unchanged. The
    // normal path (branch_reg == 0) keeps the unclamped behaviour bit-identical.
    if ctx.branch_reg > 0.0 {
        let vd_max = vc + 0.4; // a few hundred mV into strong conduction
        if vd > vd_max {
            vd = vd_max;
        }
    }

    if let Some(je) = ctx.junction_eval {
        je.store(id, 0, vd, vd_raw);
    }

    let (idc, gd_raw) = diode_eval(model, vd, t_c, temp_on);
    let gd = gd_raw.max(ctx.opts.gmin);
    // Newton equivalent current: ieq = id - gd*vd.
    let ieq = gd.mul_add(-vd, idc);
    stamp_cond_idx(sink, a_eff, k_i, gd);
    stamp_current_idx(sink, a_eff, k_i, ieq);

    // Charge storage: junction (depletion) + diffusion
    // capacitance as a CHARGE-BASED companion in parallel with the DC
    // junction. Open at DC exactly like a capacitor; only active when the
    // model actually stores charge AND effects.junction_caps is on, so
    // charge-free decks are bit-identical whatever the toggle.
    if ctx.dc || !diode_has_charge(model, &ctx.opts.effects) {
        return;
    }
    let (q, c) = diode_charge(model, vd, idc, gd_raw);
    // The companion integrates i = dQ/dt in Q itself (charge conservation for
    // a voltage-DEPENDENT capacitance): with the same IntegCoeffs the linear
    // capacitor uses, the rule's discrete current is
    //   i_hat(v) = coeffs.g·Q(v) - ieq_hist,
    // where ieq_hist carries the Q/dQ history exactly as the capacitor's
    // history term carries C·v_prev (= Q_prev) and C·dv_prev (= i_prev): the
    // diode's ReactiveState slots hold CHARGE in x1/x2 and dQ/dt in dx1, so
    // this rides the identical integration machinery; the linear capacitor
    // IS this formula specialized to Q = C·v. Newton then linearizes Q(v) at
    // the iterate with dQ/dv = C(vd):
    //   i_lin(v) = geq·v + (i_hat(vd) - geq·vd),  geq = coeffs.g·C(vd).
    let sl = id.0 as usize;
    let q_prev = ctx.state.x1[sl];
    let ieq_hist = match ctx.opts.integration {
        Integration::Trapezoidal => ctx.coeffs.g.mul_add(q_prev, ctx.state.dx1[sl]),
        Integration::Gear2 => ctx
            .coeffs
            .a1
            .mul_add(q_prev, -(ctx.coeffs.a2 * ctx.state.x2[sl])),
        Integration::BackwardEuler => ctx.coeffs.a1 * q_prev,
    };
    let geq = ctx.coeffs.g * c;
    let ieq_q = geq.mul_add(-vd, ctx.coeffs.g.mul_add(q, -ieq_hist));
    stamp_cond_idx(sink, a_eff, k_i, geq);
    stamp_current_idx(sink, a_eff, k_i, ieq_q);
}

#[allow(clippy::too_many_lines, clippy::similar_names)] // one Gummel-Poon evaluation; SPICE names
fn stamp_bjt<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    c: NodeId,
    b: NodeId,
    e: NodeId,
    model: &BjtModel,
    sink: &mut S,
) {
    let sign = model.polarity.sign();
    let temp_on = ctx.opts.effects.temperature;
    let t_c = ctx.opts.model_temp();
    let is = if temp_on { model.is_at(t_c) } else { model.is };
    let vt = hauksbee_ir_thermal(t_c, temp_on);

    // Series resistances rb/re/rc: the layout allocated an
    // internal unknown per NONZERO terminal resistance (see
    // `Layout::bjt_internal`, zero-valued resistances allocate nothing, so a
    // default model takes none of these branches). With the toggle on, stamp
    // the ohmic conductance between each external terminal and its internal
    // node and move the Gummel-Poon core (and the charges below) onto the
    // internal nodes. With the toggle off, the core stays on the external
    // nodes, exactly the no-series-R physics, and each allocated-but-unused
    // internal unknown is pinned to 0 by a unit diagonal (an isolated row
    // coupled to nothing, so it cannot influence the solution).
    let ext = [ctx.layout.node(c), ctx.layout.node(b), ctx.layout.node(e)];
    let mut eff = ext;
    // The base's ohmic resistance, when the card modulates it with current
    // (SPICE IRB/RBM), is stamped below once this iterate's base current is
    // known; `deferred_rb` carries its internal node there.
    let mut deferred_rb = None;
    if let Some(&ints) = ctx.layout.bjt_internal(id) {
        if ctx.opts.effects.series_resistance {
            let rs = [model.rc, model.rb, model.re];
            for t in 0..3 {
                if let Some(int_i) = ints[t] {
                    if t == 1 && model.base_resistance_modulated() {
                        deferred_rb = Some(int_i);
                    } else {
                        stamp_cond_idx(sink, ext[t], Some(int_i), 1.0 / rs[t]);
                    }
                    eff[t] = Some(int_i);
                }
            }
        } else {
            for int_i in ints.into_iter().flatten() {
                sink.g(int_i, int_i, 1.0);
            }
        }
    }
    let [ci, bi, ei] = eff;
    let vx = |i: Option<usize>| i.map_or(0.0, |i| ctx.x[i]);
    let vx_prev = |i: Option<usize>| i.map_or(0.0, |i| ctx.x_prev[i]);

    // Intrinsic junction voltages with polarity folded in.
    let vbe = sign * (vx(bi) - vx(ei));
    let vbc = sign * (vx(bi) - vx(ci));
    // Separate critical voltages per junction (SPICE3's VCRITF/VCRITR):
    // pnjlim's gate (`vnew > vcrit`) and its `arg <= 0` fallback (return
    // vcrit) are only self-consistent when vcrit is built from the SAME n·Vt
    // the call limits with, feeding the vbc call the nf-scaled vcrit made
    // the reverse limiter fire at the wrong threshold whenever nf != nr.
    // Iteration-path only; the converged root is unchanged.
    let vcrit_be = vcrit(is, model.nf * vt);
    let vcrit_bc = vcrit(is, model.nr * vt);
    // Limiting anchors: the last EVALUATION voltages when the live Newton
    // threads its junction state (see StampCtx::junction_eval), the previous
    // iterate otherwise. The evaluation anchor is what lets a collapsed
    // diode-connected base climb its log schedule while the node itself waits
    // at the linear-network voltage, instead of the false root becoming a
    // fixed point of the lagged-iterate anchor (the mirror deck's 235 A
    // "converged" make step).
    let vbe_iter_prev = sign * (vx_prev(bi) - vx_prev(ei));
    let vbc_iter_prev = sign * (vx_prev(bi) - vx_prev(ci));
    let (vbe_last, vbc_last) = ctx
        .junction_eval
        .map_or((vbe_iter_prev, vbc_iter_prev), |je| {
            (
                je.anchor(id, 0, vbe_iter_prev),
                je.anchor(id, 1, vbc_iter_prev),
            )
        });
    let vbe_raw_j = vbe;
    let vbc_raw_j = vbc;
    let vbe = pnjlim(vbe, vbe_last, model.nf * vt, vcrit_be);
    let vbc = pnjlim(vbc, vbc_last, model.nr * vt, vcrit_bc);
    if let Some(je) = ctx.junction_eval {
        je.store(id, 0, vbe, vbe_raw_j);
        je.store(id, 1, vbc, vbc_raw_j);
    }

    let nvf = model.nf * vt;
    let nvr = model.nr * vt;

    // Forward / reverse transport currents (clamped exponent for safety).
    let ef = (vbe / nvf).clamp(-40.0, 40.0).exp();
    let er = (vbc / nvr).clamp(-40.0, 40.0).exp();
    let cf = is * (ef - 1.0); // forward diffusion
    let cr = is * (er - 1.0); // reverse diffusion

    // Base-width modulation (Early effect): SGP's base-charge factor with
    // IKF absent is qb = q1 = 1/(1 - vbc/VAF - vbe/VAR), and the transport
    // current is DIVIDED by qb, i.e. MULTIPLIED by (1 - vbc/VAF - vbe/VAR),
    // which is > 1 in forward-active (vbc < 0), so ic grows with vce and
    // ro = VAF/IC, the classic Early slope; the -vbe/VAR term is the REVERSE
    // Early voltage, shrinking ic as the forward junction charges the base
    // (it dominates in saturation/reverse-active, so a model that parses VAR
    // and drops it mis-simulates there silently).
    // (The sign is easy to get backwards: DIVIDING by (1 - vbc/VAF) shrinks ic
    // with vce, and no bias deck catches it, their collector points are
    // bias-network-set; only the amplifier-gain cross-check against
    // ngspice exposes it. For the default model both `vaf` and `var` are
    // infinite, `early == 1.0`, and `1/1.0 == 1.0` exactly, so default-model
    // decks are insensitive to the whole factor.)
    let early = bjt_early(model, ctx.opts.effects.early_effect, vbe, vbc);
    let q1_inv_early = early.max(0.1); // = 1/q1, clamped far from use

    // High-injection (Webster) roll-off, the second half of the SGP base charge:
    //
    //   q2 = cf/IKF + cr/IKR
    //   qb = (q1/2)·(1 + sqrt(1 + 4·q2))       so   1/qb = (1/q1)·qbfac
    //   qbfac = 2/(1 + sqrt(1 + 4·q2))
    //
    // With both knees at INFINITY, q2 is 0, the square root is 1, and qbfac is
    // exactly 1.0, so every model that does not set a knee is bit-identical to
    // before: the whole factor costs a knee-less deck nothing.
    //
    // Without this term beta never falls with current, which is not a small
    // approximation. The genuine ON Semiconductor 2N3904 card gives beta 164 at
    // 20 mA; the same card with IKF and ISE dropped gives 440.
    let q2 = if ctx.opts.effects.early_effect {
        let mut q = 0.0;
        if model.ikf.is_finite() && model.ikf > 0.0 {
            q += (cf / model.ikf).max(0.0);
        }
        if model.ikr.is_finite() && model.ikr > 0.0 {
            q += (cr / model.ikr).max(0.0);
        }
        q
    } else {
        0.0
    };
    let sq = 4.0f64.mul_add(q2, 1.0).sqrt();
    let qbfac = 2.0 / (1.0 + sq);
    let q1_inv = q1_inv_early * qbfac; // = 1/qb

    // Transport (collector) current and the base components. The two leakage
    // terms are the low-current beta droop: recombination in the junction
    // depletion regions, with their own (typically n = 2) emission coefficients
    // and NO 1/qb dependence, since they are not transport current.
    let ict = (cf - cr) * q1_inv;
    let nve = model.ne * vt;
    let nvc = model.nc * vt;
    let (ile, gle) = if model.ise > 0.0 {
        let e = (vbe / nve).clamp(-40.0, 40.0).exp();
        (model.ise * (e - 1.0), model.ise * e / nve)
    } else {
        (0.0, 0.0)
    };
    let (ilc, glc) = if model.isc > 0.0 {
        let e = (vbc / nvc).clamp(-40.0, 40.0).exp();
        (model.isc * (e - 1.0), model.isc * e / nvc)
    } else {
        (0.0, 0.0)
    };
    let ibe = cf / model.bf + ile;
    let ibc = cr / model.br + ilc;

    // Conductances (derivatives wrt the junction voltages).
    let gpi = is * ef / (nvf * model.bf) + gle; // d ibe / d vbe
    let gmu = is * er / (nvr * model.br) + glc; // d ibc / d vbc
    let gif = is * ef / nvf; // d cf / d vbe
    let gir = is * er / nvr; // d cr / d vbc
    let gm = (gif * q1_inv).max(ctx.opts.gmin); // forward transconductance

    // Terminal currents into c and b (NPN reference, then sign-folded); the
    // emitter's is minus their sum (KCL), folded directly into the RHS below.
    let ic = ict - ibc;
    let ib = ibe + ibc;

    // Current-dependent base resistance (SPICE3 `bjtload`): evaluated at this
    // iterate's base current and base charge and stamped as a plain
    // conductance, with no derivative through ib, exactly as SPICE does. The
    // root is the card's; only the Newton path differs from a fixed rb.
    if let Some(int_i) = deferred_rb {
        let rbb = model.base_resistance(ib, q1_inv).max(1e-6);
        stamp_cond_idx(sink, ext[1], Some(int_i), 1.0 / rbb);
    }

    // Linearized stamp: the EXACT tangent, the true partials
    //   ∂ic/∂vbe = gif·q1_inv
    //   ∂ic/∂vbc = -gir·q1_inv - gmu - (cf-cr)/vaf  (Early term only when the
    //              effect is active and the clamp is not)
    //   ∂ib/∂vbe = gpi,  ∂ib/∂vbc = gmu
    // stamped directly per junction voltage, on whichever unknowns (ci, bi,
    // ei) resolved to above: the intrinsic internal nodes when series
    // resistance relocated the core inside, the externals otherwise.
    //
    // The EXACT tangent, on every path. An approximate Jacobian (∂ic/∂vbc
    // mapped through a go-between-c-e plus gm-on-vbe, mismatching the true
    // partial by gir) defines the same root but limit-cycles Newton on a
    // hard-saturated switching edge, failing the step at dt_min.
    //
    // Scope: this is the LARGE-SIGNAL (DC/transient Newton) stamp. The AC
    // small-signal stamp (`stamp_bjt_ac` in ac.rs) carries its own gm/go
    // admittance form on the external-node path; aligning it with these
    // partials is separate, AC-oracle-gated work.
    //
    // `sign` folds out of the matrix entries (sign² = 1) and folds
    // the RHS brackets whole; folding only some terms breaks PNP convergence.
    {
        // The base-charge factor's own partials (both zero unless the effect
        // is on, the voltage is finite, and the clamp is not engaged):
        // ∂q1_inv/∂vbc = -1/vaf, ∂q1_inv/∂vbe = -1/var, each contributing
        // (cf-cr)·∂q1_inv to the transport-current row.
        let early_free = ctx.opts.effects.early_effect && early > 0.1;
        let (early_deriv, var_deriv) = bjt_early_partials(model, early_free, cf, cr);
        // The high-injection factor's partials. qbfac = 2/(1 + s) with
        // s = sqrt(1 + 4·q2), so
        //   ∂qbfac/∂q2 = -4 / (s·(1 + s)²)
        // and q2 depends on each junction through its own transport current:
        //   ∂q2/∂vbe = gif/IKF,  ∂q2/∂vbc = gir/IKR.
        //
        // Getting this wrong does not produce a wrong answer, only a slower or
        // stalled Newton, which is the failure mode that looks like a modelling
        // bug and is not. Both terms vanish when the knees are infinite.
        let dqbfac = -4.0 / (sq * (1.0 + sq) * (1.0 + sq));
        // Each derivative is gated on ITS OWN term being unclamped, not on
        // the total q2: q2 sums (cf/IKF).max(0) and (cr/IKR).max(0), and in
        // forward-active operation cr < 0 clamps the reverse term out while
        // the forward one stays live. Gating on q2 > 0 alone would keep
        // gir/IKR in the tangent for a term whose clamped derivative is
        // exactly zero: a wrong (if usually small) Jacobian entry, the same
        // class of inexactness this stamp exists to avoid. cf > 0 stands in
        // for cf/IKF > 0 since the gate already requires IKF > 0.
        let dq2_dvbe = if model.ikf.is_finite() && model.ikf > 0.0 && cf > 0.0 {
            gif / model.ikf
        } else {
            0.0
        };
        let dq2_dvbc = if model.ikr.is_finite() && model.ikr > 0.0 && cr > 0.0 {
            gir / model.ikr
        } else {
            0.0
        };
        let knee_be = (cf - cr) * q1_inv_early * dqbfac * dq2_dvbe;
        let knee_bc = (cf - cr) * q1_inv_early * dqbfac * dq2_dvbc;
        // gif·q1_inv (gmin-floored like the legacy path) plus the VAR term,
        // each Early partial scaled by qbfac since the two factors multiply.
        let gc_be = var_deriv.mul_add(qbfac, gm) + knee_be;
        let gc_bc = early_deriv.mul_add(qbfac, (-gir).mul_add(q1_inv, -gmu)) + knee_bc;
        // Row c: gc_be·vbe + gc_bc·vbc; row b: gpi·vbe + gmu·vbc; row e is
        // minus their sum (KCL). Each junction-voltage dependence is a
        // transconductance from the terminal pair into (b, x).
        add_transconductance(sink, ci, ei, bi, ei, gc_be);
        add_transconductance(sink, ci, ei, bi, ci, gc_bc);
        add_transconductance(sink, bi, ei, bi, ei, gpi);
        add_transconductance(sink, bi, ei, bi, ci, gmu);
        let ic_eq = sign * (ic - (gc_be * vbe + gc_bc * vbc));
        let ib_eq = sign * (ib - (gpi * vbe + gmu * vbc));
        inject(sink, ci, -ic_eq);
        inject(sink, bi, -ib_eq);
        inject(sink, ei, ic_eq + ib_eq);
    }

    // Charge storage: base-emitter and base-collector
    // charges as CHARGE-BASED companions, exactly the diode's companion
    // applied per junction, bank A of the device's `ReactiveState` slots
    // holds Q_be, bank B holds Q_bc (both in FOLDED space, like every voltage
    // above). Open at DC like a capacitor; only active when the model stores
    // charge AND the toggle allows, so charge-free decks are bit-identical.
    if ctx.dc || !bjt_has_charge(model, &ctx.opts.effects) {
        return;
    }
    // Diffusion charges reuse the transport currents (cf/cr) and tangents
    // (gif/gir) the core just evaluated at the LIMITED junction voltages.
    let (q_be, c_be) = bjt_charge_be(model, vbe, cf, gif);
    let (q_bc, c_bc) = bjt_charge_bc(model, vbc, cr, gir);
    let sl = id.0 as usize;
    // Each junction's companion is the diode's, verbatim: the rule's discrete
    // current i_hat(v) = coeffs.g·Q(v) - ieq_hist rides the same IntegCoeffs
    // machinery (slots hold CHARGE in x1/x2, dQ/dt in dx1), Newton linearizes
    // with dQ/dv = C at the iterate. The folded charge current flows b->e
    // (resp. b->c) in NPN-reference space; `sign` maps it to real space,
    // the conductance stamp is polarity-free (sign² = 1) while the RHS
    // residual folds, mirroring the DC bracket above.
    let hist = |x1: f64, dx1: f64, x2: f64| match ctx.opts.integration {
        Integration::Trapezoidal => ctx.coeffs.g.mul_add(x1, dx1),
        Integration::Gear2 => ctx.coeffs.a1.mul_add(x1, -(ctx.coeffs.a2 * x2)),
        Integration::BackwardEuler => ctx.coeffs.a1 * x1,
    };
    let geq_be = ctx.coeffs.g * c_be;
    let ieq_be = geq_be.mul_add(
        -vbe,
        ctx.coeffs.g.mul_add(
            q_be,
            -hist(ctx.state.x1[sl], ctx.state.dx1[sl], ctx.state.x2[sl]),
        ),
    );
    stamp_cond_idx(sink, bi, ei, geq_be);
    inject(sink, bi, -sign * ieq_be);
    inject(sink, ei, sign * ieq_be);

    let geq_bc = ctx.coeffs.g * c_bc;
    let ieq_bc = geq_bc.mul_add(
        -vbc,
        ctx.coeffs.g.mul_add(
            q_bc,
            -hist(
                ctx.state.xb[0].x1[sl],
                ctx.state.xb[0].dx1[sl],
                ctx.state.xb[0].x2[sl],
            ),
        ),
    );
    stamp_cond_idx(sink, bi, ci, geq_bc);
    inject(sink, bi, -sign * ieq_bc);
    inject(sink, ci, sign * ieq_bc);
}

#[allow(
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::many_single_char_names
)] // one Level-1 evaluation; SPICE names
fn stamp_mosfet<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    d: NodeId,
    gnode: NodeId,
    s: NodeId,
    b: Option<NodeId>,
    model: &MosfetModel,
    sink: &mut S,
) {
    match model.level {
        MosLevel::Level1 => {}
    }
    let sign = model.polarity.sign();
    let vt = hauksbee_ir_thermal(ctx.opts.model_temp(), ctx.opts.effects.temperature);
    let beta = model.beta();

    // Series resistances rd/rs (the datasheet-Rds(on) split, mirroring the BJT
    // rb/re/rc machinery): the layout allocated an internal DRAIN unknown when
    // rd > 0 and an internal SOURCE unknown when rs > 0 (see
    // `Layout::mos_internal`, a zero resistance allocates nothing, so a default
    // model takes none of these branches). With the toggle on, stamp the ohmic
    // conductance between each external terminal and its internal node and move
    // the WHOLE transistor intrinsic (channel, body junctions, gate charges)
    // onto the internal nodes, exactly where a real device's drain/source
    // diffusion resistance sits, so a power FET's on-state drop lives in rd + rs
    // rather than a spuriously-low channel. With the toggle off, the intrinsic
    // stays on the external nodes (the no-series-R physics) and each
    // allocated-but-unused internal unknown is pinned to 0 by a unit diagonal
    // (an isolated row coupling to nothing). rd/rs NEVER relabel the physical
    // drain/source: rd is always the drain terminal's resistance and rs the
    // source terminal's, independent of the Vds symmetry swap below.
    let di_ext = ctx.layout.node(d);
    let si_ext = ctx.layout.node(s);
    let mut d_eff = di_ext;
    let mut s_eff = si_ext;
    if let Some(&ints) = ctx.layout.mos_internal(id) {
        if ctx.opts.effects.series_resistance {
            if let Some(di_int) = ints[0] {
                stamp_cond_idx(sink, di_ext, Some(di_int), 1.0 / model.rd);
                d_eff = Some(di_int);
            }
            if let Some(si_int) = ints[1] {
                stamp_cond_idx(sink, si_ext, Some(si_int), 1.0 / model.rs);
                s_eff = Some(si_int);
            }
        } else {
            for int_i in ints.into_iter().flatten() {
                sink.g(int_i, int_i, 1.0);
            }
        }
    }
    // Read drain/source voltages at the EFFECTIVE unknowns (internal when a
    // series resistor moved the intrinsic inside, external otherwise). For the
    // default rd == rs == 0 model d_eff/s_eff ARE `layout.node(d)`/`node(s)` and
    // `vx()` reproduces `ctx.v()` exactly; the bit-identity fast path.
    let vx = |i: Option<usize>| i.map_or(0.0, |i| ctx.x[i]);
    let vx_prev = |i: Option<usize>| i.map_or(0.0, |i| ctx.x_prev[i]);

    // Fold polarity: work in N-channel space.
    let mut vd = sign * vx(d_eff);
    let vg = sign * ctx.v(gnode);
    let mut vs = sign * vx(s_eff);
    // Ensure drain is the higher terminal (symmetry handled by swap).
    let swap = vd < vs;
    if swap {
        std::mem::swap(&mut vd, &mut vs);
    }
    let vgs = vg - vs;
    let vds = vd - vs;

    // Body-effect threshold shift. With a bulk terminal present and
    // `gamma > 0`, `vth = vto + gamma·(sqrt(phi - vbs) - sqrt(phi))` at the
    // folded bulk-to-EFFECTIVE-source voltage (the post-swap source, ngspice
    // measures the body effect from whichever terminal acts as the source).
    // `gamma == 0` (every model without a body-effect term) takes the plain
    // `vto` path bit-identically and never reads the bulk voltage.
    let bulk = b.unwrap_or(s);
    let body_effect = model.gamma > 0.0 && b.is_some();
    let vbs_f = if body_effect {
        sign.mul_add(ctx.v(bulk), -vs)
    } else {
        0.0
    };
    // d vth / d vbs feeds the gmb transconductance below (0 unless body effect).
    let (vth, dvth_dvbs) = if body_effect {
        mos_body_vth(model, vbs_f)
    } else {
        (model.vto, 0.0)
    };

    let nsub = model.n_sub.max(1.0);
    // Channel evaluation via the shared C1 Level 1 channel (see
    // [`mos_channel`]); the AC tangent calls the SAME function at the OP.
    let lambda = if ctx.opts.effects.early_effect {
        model.lambda
    } else {
        0.0
    };
    let (ids, gm, gds) = mos_channel(beta, nsub * vt, vgs - vth, vds, lambda, ctx.opts.gmin);

    // Map back through the swap and polarity. The drain current flows d->s in
    // N-channel space; after a swap it flows the other way. The drain/source
    // unknowns are the EFFECTIVE (internal when rd/rs moved the intrinsic in)
    // nodes; the gate carries no series resistance, so it stays external.
    let gi = ctx.layout.node(gnode);
    let (dn, sn) = if swap { (s_eff, d_eff) } else { (d_eff, s_eff) };

    // Conductances: gds between drain-source, gm couples drain current to vgs.
    stamp_cond_idx(sink, dn, sn, gds);
    add_transconductance(sink, dn, sn, gi, sn, gm);

    // Equivalent current: id_eq = ids - gm*vgs - gds*vds, flowing d->s.
    let mut ieq = gds.mul_add(-vds, gm.mul_add(-vgs, ids));
    if body_effect {
        // Bulk transconductance gmb = d ids / d vbs = -gm · d vth / d vbs:
        // the drain current additionally depends on the bulk-to-effective-
        // source voltage. The bulk row itself receives nothing (the bulk is
        // still a SENSE terminal unless the model carries body-diode fields).
        let gmb = -gm * dvth_dvbs;
        let bi = ctx.layout.node(bulk);
        add_transconductance(sink, dn, sn, bi, sn, gmb);
        ieq -= gmb * vbs_f;
    }
    let ieq_signed = sign * ieq;
    inject(sink, dn, -ieq_signed);
    inject(sink, sn, ieq_signed);

    // Trapezoidal/Gear/BE history bracket shared by every charge companion
    // below; the diode's companion verbatim (slots hold CHARGE in
    // x1/x2, dQ/dt in dx1; the discrete current is coeffs.g·Q(v) - hist).
    let hist = |x1: f64, dx1: f64, x2: f64| match ctx.opts.integration {
        Integration::Trapezoidal => ctx.coeffs.g.mul_add(x1, dx1),
        Integration::Gear2 => ctx.coeffs.a1.mul_add(x1, -(ctx.coeffs.a2 * x2)),
        Integration::BackwardEuler => ctx.coeffs.a1 * x1,
    };
    let sl = id.0 as usize;

    // Body diode: STRUCTURAL bulk-junction physics, the
    // BJT-junction discipline; the DC branches exist whenever the model
    // carries them (`body_is > 0`), un-toggled; the depletion charges ride
    // `junction_caps`. Each junction is bulk->drain / bulk->source in folded
    // space (the bulk is the anode of an N-channel body diode; `sign` folds
    // PMOS). A junction is skipped only when it is a genuine SHORT, i.e. the
    // bulk and the junction terminal resolve to the SAME unknown.
    // This is the reverse-conduction path of every synchronous-rectifier /
    // flyback deck; `body_is` defaults to 0 (BIT-IDENTITY deviation from
    // ngspice's 1e-14 default, documented on the model field).
    if model.has_body_diode() {
        // The bulk carries no series resistance of its own, so its effective
        // unknown is always the external one (ngspice's `bNode`).
        let bulk_i = ctx.layout.node(bulk);
        // (its EFFECTIVE unknown, depletion cap, secondary bank index). The
        // bulk junctions sit at the internal drain/source (behind rd/rs),
        // where a real device's body diode connects, exactly as ngspice wires
        // them (`bNode` to `dNodePrime`/`sNodePrime`, mos1set.c). The short
        // test therefore compares ELECTRICAL unknowns, not physical terminals:
        // with rd/rs the internal drain/source is a distinct unknown, so a
        // netlist body tie (the discrete bulk-to-source default) does NOT
        // short the junction, and the diode must be stamped between the
        // external bulk and the internal terminal. At rd == rs == 0 both sides
        // resolve to the same unknown and the junction is skipped.
        let junctions = [(d_eff, model.cbd, 1usize), (s_eff, model.cbs, 2usize)];
        for (term_i, cbx, bank) in junctions {
            if term_i == bulk_i {
                continue;
            }
            let v_raw = sign * (ctx.v(bulk) - vx(term_i));
            // DC junction: Shockley branch with pn-junction limiting, exactly
            // the diode's convergence discipline.
            let vj = if model.body_is > 0.0 {
                let v_prev = sign * (ctx.v_prev(bulk) - vx_prev(term_i));
                let vcr = vcrit(model.body_is, vt);
                let vj = pnjlim(v_raw, v_prev, vt, vcr);
                let e = (vj / vt).clamp(-40.0, 40.0).exp();
                let ij = model.body_is * (e - 1.0);
                let gj = (model.body_is * e / vt).max(ctx.opts.gmin);
                stamp_cond_idx(sink, bulk_i, term_i, gj);
                let ieq_j = ij - gj * vj;
                inject(sink, bulk_i, -sign * ieq_j);
                inject(sink, term_i, sign * ieq_j);
                vj
            } else {
                v_raw
            };
            // Depletion charge companion (open at DC like every capacitor).
            if !ctx.dc && ctx.opts.effects.junction_caps && cbx > 0.0 {
                let (q, c) = depletion_charge(cbx, model.pb, model.mj, DIODE_FC, vj);
                let bk = &ctx.state.xb[bank];
                let geq = ctx.coeffs.g * c;
                let ieq_c = geq.mul_add(
                    -vj,
                    ctx.coeffs
                        .g
                        .mul_add(q, -hist(bk.x1[sl], bk.dx1[sl], bk.x2[sl])),
                );
                stamp_cond_idx(sink, bulk_i, term_i, geq);
                inject(sink, bulk_i, -sign * ieq_c);
                inject(sink, term_i, sign * ieq_c);
            }
        }
    }

    // Gate charges (gated by `junction_caps`, "the single
    // biggest reason a MOS switching deck disagrees with ngspice"): Q_gs on
    // bank A, Q_gd on secondary bank 0, each a two-terminal charge companion.
    // "PHYSICAL (unswapped)" means Q_gs stays on the real SOURCE side and Q_gd
    // on the real DRAIN side whichever way the channel evaluation swapped d/s
    // for Vds < 0; the charge does not follow the symmetry swap. It DOES
    // follow the intrinsic onto the internal nodes (d_eff/s_eff): the gate
    // capacitance connects to the intrinsic drain/source diffusion, behind
    // rd/rs, so Cgd/Cgs reference the effective (internal when present)
    // unknowns while the gate itself stays external. The charge-model choice
    // (Meyer region limits on a smooth two-terminal Q(v)) is documented on
    // [`mos_gate_charge`].
    if !ctx.dc && ctx.opts.effects.junction_caps && model.has_gate_charge() {
        let delta = 2.0 * nsub * vt;
        let vgs_p = sign * (ctx.v(gnode) - vx(s_eff));
        let vgd_p = sign * (ctx.v(gnode) - vx(d_eff));
        let (q_gs, c_gs) = mos_charge_gs(model.cgs_ov, model.c_ox, model.vto, delta, vgs_p);
        let geq_gs = ctx.coeffs.g * c_gs;
        let ieq_gs = geq_gs.mul_add(
            -vgs_p,
            ctx.coeffs.g.mul_add(
                q_gs,
                -hist(ctx.state.x1[sl], ctx.state.dx1[sl], ctx.state.x2[sl]),
            ),
        );
        stamp_cond_idx(sink, gi, s_eff, geq_gs);
        inject(sink, gi, -sign * ieq_gs);
        inject(sink, s_eff, sign * ieq_gs);

        let (q_gd, c_gd) = mos_charge_gd(model.cgd_ov, model.c_ox, model.vto, delta, vgd_p);
        let bk = &ctx.state.xb[0];
        let geq_gd = ctx.coeffs.g * c_gd;
        let ieq_gd = geq_gd.mul_add(
            -vgd_p,
            ctx.coeffs
                .g
                .mul_add(q_gd, -hist(bk.x1[sl], bk.dx1[sl], bk.x2[sl])),
        );
        stamp_cond_idx(sink, gi, d_eff, geq_gd);
        inject(sink, gi, -sign * ieq_gd);
        inject(sink, d_eff, sign * ieq_gd);
    }
}

/// The `VSwitch` conductance ramp, shared verbatim by the transient/DC stamp and
/// by the AC small-signal stamp.
///
/// Returns `(s, ds_dvctrl, ramp_off, ramp_on)`, where `s` is the selection
/// fraction that log-interpolates the conductance
/// `gsw = exp(ln goff + s*(ln gon - ln goff))`, and `ds_dvctrl` is its exact
/// derivative (the control tangent both stamps need).
///
/// One function because the two paths drifted before: `.ac` kept its own copy of
/// an unclamped tanh, so a relay whose DC solve had it latched hard open was
/// reported by `.ac` as a mid-band modulator with a gain of 6.27 while the DC
/// sensitivity at the same bias was exactly 0. A duplicated device model is a
/// divergence waiting to be measured.
///
/// `latched` is `Some(on)` for a [`crate::options::SwitchModel::Hysteretic`]
/// relay, naming which of its two thresholds is live; `None` is the smooth pass
/// element, whose ramp spans the whole `[voff, von]` band.
pub fn vswitch_ramp(
    vctrl: f64,
    von: f64,
    voff: f64,
    latched: Option<bool>,
    transition_frac: f64,
) -> (f64, f64, f64, f64) {
    let (ramp_off, ramp_on) = latched.map_or((voff, von), |on| {
        // ONE-SIDED, anchored exactly on the live threshold, with the ramp
        // lying on the side the relay is moving TOWARD. An open relay is at
        // exactly `roff` at `von` and reaches `ron` at `von + w`; a closed one
        // is at exactly `ron` at `voff` and reaches `roff` at `voff - w`.
        //
        // Anchored rather than centred because the threshold is a strict
        // inequality in the model and in ngspice ("closed WHEN the control
        // passes von"), so at the threshold itself the relay is still in its
        // old state. A centred ramp put the half-conductance point there and
        // disagreed maximally with the oracle at exactly the one control
        // voltage the deck cares about (measured on `switch_sw_thresholds`:
        // 0.99 relative error at VC = 2.5 V and nowhere else).
        //
        // It also makes the residual timing error one-signed: the switch can
        // now only ever act LATE, by at most `w / dv_ctrl_dt`, never early.
        // Turning on early is the failure that cost a large relative error on a
        // switched-mirror array, because it hands the load current before the circuit
        // being modelled would have.
        let w = (transition_frac * (von - voff).abs()).max(1e-12)
            * if von >= voff { 1.0 } else { -1.0 };
        if on {
            (voff - w, voff)
        } else {
            (von, von + w)
        }
    });
    let width = ramp_on - ramp_off;
    let width = if width == 0.0 { 1e-12 } else { width };
    // q = 0 at `ramp_off`, 1 at `ramp_on`; the division carries the orientation.
    let q = ((vctrl - ramp_off) / width).clamp(0.0, 1.0);
    // Smoothstep: monotone, C1, and it REACHES both rails at finite control
    // voltage, which the previous unclamped tanh never did (measured on
    // `von=2.5 voff=0.5 ron=10 roff=1e9`: 12.2 Ohm at vctrl=3 V against a stated
    // 10, and 819 MOhm at vctrl=0 against a stated 1e9). Zero slope at both ends
    // means a switch outside its band contributes no spurious control tangent and
    // Newton sees no kink where the clamp engages.
    let s = q * q * 2.0f64.mul_add(-q, 3.0);
    let ds_dvctrl = 6.0 * q * (1.0 - q) / width;
    (s, ds_dvctrl, ramp_off, ramp_on)
}

/// Log-interpolated switch conductance at selection fraction `s`.
pub fn vswitch_g_from_s(s: f64, ron: f64, roff: f64) -> f64 {
    let gon = 1.0 / ron.max(1e-12);
    let goff = 1.0 / roff.max(1e-12);
    s.mul_add(gon.ln() - goff.ln(), goff.ln()).exp()
}

/// Which threshold of a hysteretic relay is live, or `None` for the smooth model
/// (and for any switch the latch has no entry for yet).
pub fn vswitch_latched(
    opts: &SolverOptions,
    latch: Option<&std::collections::HashMap<DeviceId, bool>>,
    id: DeviceId,
) -> Option<bool> {
    if opts.effects.switch_model != crate::options::SwitchModel::Hysteretic {
        return None;
    }
    latch?.get(&id).copied()
}

#[allow(clippy::similar_names)] // self/sibling pairs: vmid/svmid, span/sspan
fn stamp_vswitch<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    a: NodeId,
    b: NodeId,
    cp: NodeId,
    cn: NodeId,
    von: f64,
    voff: f64,
    ron: f64,
    roff: f64,
    sink: &mut S,
) {
    let gon = 1.0 / ron.max(1e-12);
    let goff = 1.0 / roff.max(1e-12);

    // Event-frozen path: the staged-DC outer loop supplies a FROZEN on/off
    // decision for this switch. Pin the conductance to that rail (full ron or
    // roff) with NO control-node dependence, so the inner circuit is smooth and
    // strictly through-conducting. This is the limit-cycle cure for the
    // analog-switch mesh: with every switch's state fixed, the fused core is a
    // smooth resistor network Newton converges; the outer loop re-derives each
    // switch's rail from the converged control voltages and re-solves until no
    // switch flips, so the fixed point is a true root with every switch state
    // consistent with its own control. (Mirrors the comparator cmp_freeze.)
    if let Some(freeze) = ctx.switch_freeze {
        let vmid = 0.5 * (von + voff);
        let on = freeze
            .get(&id)
            .copied()
            .unwrap_or_else(|| (ctx.v(cp) - ctx.v(cn)) > vmid);
        let gsw = if on { gon } else { goff };
        stamp_cond(sink, ctx.layout, a, b, gsw);
        return;
    }

    let vctrl_raw = ctx.v(cp) - ctx.v(cn);

    // HYSTERETIC (SPICE3 `SW`) threshold selection. The latch says which
    // threshold is live: an OPEN relay closes when the control rises past `von`, a
    // CLOSED one opens when it falls below `voff`, and neither responds to
    // anything in between.
    //
    // The full band is emphatically NOT the transition width, which is the bug
    // this replaces: `VH` is a hysteresis HALF-WIDTH, so ramping across
    // `[VT-VH, VT+VH]` put the 50%-conductance point at `VT` and started
    // conducting a whole `VH` early. Measured on a switched-mirror deck's
    // `SW(VT=1.5 VH=1.0)` with a 10 us 0->3 V edge: ngspice closes at
    // vctrl = 2.5 V and opens at 0.5 V, while the band-wide ramp was effectively
    // closed by ~1.6 V and open again by ~1.4 V, landing both edges ~3 us early.
    // On a slewing integrator node that was a large relative divergence.
    let latched = crate::stamp::vswitch_latched(ctx.opts, ctx.switch_latch, id);
    let frac = ctx.opts.effects.switch_transition_frac;
    let (_, _, ramp_off, ramp_on) = vswitch_ramp(vctrl_raw, von, voff, latched, frac);
    let vmid = 0.5 * (ramp_on + ramp_off);
    let span = (ramp_on - ramp_off).abs().max(1e-12);
    // CONTROL LIMITING (staged / dynamic path only, branch_reg > 0). The
    // log-interpolated conductance spans many decades (ron ~ ohms, roff ~ 1e9), so
    // a single Newton iteration that moves the control voltage across the
    // transition makes gsw jump multiple decades at once. On a switch carrying
    // real current (a gating switch whose control is a slowly climbing
    // node) that snap destabilizes the coupled solve and the per-step Newton
    // fails right at the flip. Mirroring pn-junction limiting for diodes, bound
    // the per-iteration movement to about one transition width, anchored on the
    // previous iterate. branch_reg == 0 (every normal solve) keeps vctrl unlimited
    // and the path bit-identical.
    let vctrl = if ctx.branch_reg > 0.0 {
        let vctrl_old = ctx.v_prev(cp) - ctx.v_prev(cn);
        let u_new = 3.0 * (vctrl_raw - vmid) / span;
        let u_old = 3.0 * (vctrl_old - vmid) / span;
        let du = (u_new - u_old).clamp(-1.0, 1.0); // at most one transition width
        vmid + (u_old + du) * span / 3.0
    } else {
        vctrl_raw
    };
    let lgon = gon.ln();
    let lgoff = goff.ln();
    let (mut s, ds_dvctrl, _, _) = vswitch_ramp(vctrl, von, voff, latched, frac);

    // BREAK-BEFORE-MAKE for SPDT legs (effects.spdt_bbm, the device-model
    // default; clearing that field restores the plain bridging model). A real
    // SN74LVC1G3157 SPDT is NEVER low-Z to both throws at
    // once: as the SELECT crosses its threshold the leaving throw opens before the
    // entering throw closes. The bare smooth-tanh model breaks this -- at the
    // SELECT transition MID-band BOTH complementary legs sit at the geometric-mean
    // conductance sqrt(ron*roff) (~70-200 kohm), so the common node `a` is
    // simultaneously low-Z to BOTH throws. In a gated current-mirror stage the two
    // throws are an integrator node (s1) and a fixed rail (s0 = GND or a supply).
    // Both legs half-on therefore wires the rail straight onto the integrator
    // through `a`, injecting a CONTROL-INDEPENDENT common-mode current that
    // the latched gating state does not account for -- a spurious pedestal.
    // Enforcing break-before-make:
    // WINNER-TAKE-ALL between the two throws by their SELECT margin: the throw
    // whose control sits FURTHER past its own threshold is the selected one and
    // keeps (most of) its conductance; the LOSER is driven toward roff. With
    //   margin = (vctrl - vmid)/span        (>0: past the make threshold)
    //   s_eff  = s * sigmoid(K*(margin_self - margin_sib))
    // a cleanly selected throw (margin_self >> margin_sib) is unchanged, the
    // opposite throw collapses to roff, and AT the exact crossover both halve --
    // so the rail throw never bridges to the integrator throw, while the GENUINELY
    // selected throw (s1 once its slowly climbing control passes the
    // band centre) still conducts the gated current. This is the
    // analog realisation of the same on-margin tie-break the frozen event path
    // (`eval_switch_states`) uses, made smooth/differentiable for the per-step
    // Newton. K sets the transition sharpness (effects.spdt_bbm_k, default 6).
    let bbm = ctx.opts.effects.spdt_bbm;
    if bbm {
        if let Some(&sib) = ctx.spdt_sibling.get(&id) {
            if let Some(Device::VSwitch {
                ctrl_p: scp,
                ctrl_n: scn,
                von: svon,
                voff: svoff,
                ..
            }) = ctx.circuit.devices.get(sib.0 as usize)
            {
                let svmid = 0.5 * (svon + svoff);
                let sspan = (svon - svoff).abs().max(1e-9);
                let margin_self = (vctrl - vmid) / span;
                let margin_sib = ((ctx.v(*scp) - ctx.v(*scn)) - svmid) / sspan;
                let k_bbm = ctx.opts.effects.spdt_bbm_k;
                let arg = (k_bbm * (margin_self - margin_sib)).clamp(-40.0, 40.0);
                let win = 1.0 / (1.0 + (-arg).exp());
                s *= win;
            }
        }
    }

    let gln = s.mul_add(lgon - lgoff, lgoff);
    let gsw = gln.exp();

    // Conductance stamp between the through nodes (a, b).
    stamp_cond(sink, ctx.layout, a, b, gsw);

    // CONTROL-NODE JACOBIAN (the true tangent). The switch current
    //   i = gsw * (v_a - v_b)
    // depends on the control voltage vctrl through gsw. The MNA companion of a
    // voltage-controlled conductance adds a transconductance coupling the (a,b)
    // current rows to the (cp,cn) control columns, with
    //   gm_ctrl = d i / d vctrl = (v_a - v_b) * d gsw / d vctrl
    //   d gsw / d vctrl = gsw * (ln(gon) - ln(goff)) * ds/dvctrl
    // with ds/dvctrl from `vswitch_ramp` (zero outside the band, where the
    // smoothstep is clamped, so a fully-driven switch stamps no control tangent).
    // and a matching equivalent-current correction so the linearization is
    // exact at the operating point (same root, faster convergence). Stamping
    // this makes each switch Newton-linearized instead of a Picard fixed point.
    // The control-node transconductance is a Newton TANGENT (it makes each switch
    // Newton-linearized rather than a Picard fixed point); it is not required for
    // correctness -- the same root is reached without it, just with more
    // iterations. For a torn feedforward stage whose gating-switch control is a
    // HIGH-Z boundary node (a captured node driven through an external
    // RC, drawing zero conduction current), summing this term across the many
    // switch legs on that one control node with a large mirror voltage
    // across them couples back into the iterative solution and throttles the
    // control's slew so the gate never closes (the node charges far
    // slower than its R*C). effects.switch_ctrl_gm = false drops the
    // back-coupling so the control node is purely exogenous; the conductance
    // stamp + the switch's own a/b dynamics still track the flip. The default
    // (true) keeps the tangent, the classic behavior.
    let skip_ctrl_gm = !ctx.opts.effects.switch_ctrl_gm;
    let dgsw_dvctrl = gsw * (lgon - lgoff) * ds_dvctrl;
    let vab = ctx.v(a) - ctx.v(b);
    let gm_ctrl = vab * dgsw_dvctrl;
    if gm_ctrl != 0.0 && !skip_ctrl_gm {
        let (ai, bi) = (ctx.layout.node(a), ctx.layout.node(b));
        let (cpi, cni) = (ctx.layout.node(cp), ctx.layout.node(cn));
        // i_a += gm_ctrl * (v_cp - v_cn), i_b -= ... .
        add_transconductance(sink, ai, bi, cpi, cni, gm_ctrl);
        // Equivalent-current correction: the transconductance term contributes
        // gm_ctrl * vctrl to the linearized current, which is already implicit
        // in the conductance stamp at the operating point. Subtract it back via
        // the RHS so the residual equals the true device current there.
        let ieq = gm_ctrl * vctrl;
        inject(sink, ai, ieq);
        inject(sink, bi, -ieq);
    }
}

/// One transient step of the op-amp's output-follower dynamics: the internal
/// drive EMF is the previous accepted EMF relaxed toward the UNCLAMPED ideal
/// target through the single pole (exact first-order update over `dt`), THEN
/// saturated to `rails`, THEN rate-limited by the slew spec. Lagging the
/// CLAMPED target instead gives `dv/dt = (rail-v)*2*pi*pole_hz`: at the physical
/// `pole_hz = GBW/AOL` (7 Hz, LM358) a follower's 10 mV step crawls at 418 V/s,
/// not `2*pi*GBW*vd` = 44 kV/s capped by the datasheet slew, and a rate clip
/// can never speed an edge up. This order honours both, agrees with `.ac` (same
/// roll-off of the same raw target) and makes the clamp the anti-windup, the
/// state being the output itself. Shared by the Newton stamp (every
/// iteration reads the SAME frozen `v_prev`, so the step's dynamics are a
/// fixed affine map of the current inputs, no mid-Newton state mutation) and
/// by the on-accept state advance in `transient.rs` (which persists the
/// result as the next step's `v_prev`, exactly the capacitor's roll-forward
/// discipline).
///
/// Returns `None` when NEITHER mechanism is active (`pole_hz` and `slew` both
/// absent/zero/non-finite): the caller keeps the classic instantaneous stamp
/// bit-identically. Otherwise `Some((v_out, dscale))` where `v_out` is the
/// EMF to drive this step and `dscale` = `∂v_out/∂target` is the factor the
/// input-coupling tangent must be scaled by (`α = 1-exp(-dt/τ)` through the
/// pole, 0 when a rail or the slew clip pins the output, which is then this
/// step's constant, like a frozen comparator rail). Infinite `rails` skip the
/// saturation (the rail-crossing limiter's interior segment, below).
pub fn opamp_transient_output(
    v_prev: f64,
    target: f64,
    pole_hz: Option<f64>,
    slew: Option<f64>,
    rails: (f64, f64),
    dt: f64,
) -> Option<(f64, f64)> {
    let pole = pole_hz.filter(|p| *p > 0.0 && p.is_finite());
    // Slew is stored in V/µs (the datasheet unit); convert to V/s here, the
    // only place the transient path consumes it.
    let slew_v_per_s = slew.filter(|s| *s > 0.0 && s.is_finite()).map(|s| s * 1e6);
    if pole.is_none() && slew_v_per_s.is_none() {
        return None;
    }
    // Single pole: exact discrete step of dv/dt = (target - v)/τ over dt.
    // (Exact, not a companion discretization: the output follower is driven
    // by the frozen v_prev, so the closed form costs one exp and is
    // unconditionally stable at any dt/τ. So is the CLOSED loop, whose per-step
    // gain is now α·gain, not rail-limited: the stamp is affine in the iterate,
    // so a follower solves exactly to ((1-α)v_prev + αAvin)/(1+αA), factor
    // (1-α)/(1+αA) in [0,1) for every dt, so a coarse dt costs edge accuracy,
    // not stability.)
    let alpha = pole.map_or(1.0, |p| {
        let tau = 1.0 / (std::f64::consts::TAU * p);
        1.0 - (-dt / tau).exp()
    });
    // Lag the RAW target, then saturate; ∂v_out/∂target is α while the output
    // is free and 0 the moment a rail or the slew clip pins it.
    let lagged = (target - v_prev).mul_add(alpha, v_prev);
    let free = lagged > rails.0 && lagged < rails.1;
    let mut v_out = lagged.clamp(rails.0, rails.1);
    let mut dscale = if free { alpha } else { 0.0 };
    if let Some(sr) = slew_v_per_s {
        let dv_max = sr * dt;
        let dv = v_out - v_prev;
        if dv.abs() > dv_max {
            v_out = v_prev + dv_max.copysign(dv);
            dscale = 0.0;
        }
    }
    Some((v_out, dscale))
}

fn stamp_opamp<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    (out, inp, inn, reference): (NodeId, NodeId, NodeId, Option<NodeId>),
    (gain, pole_hz, slew): (f64, Option<f64>, Option<f64>),
    (rail_lo, rail_hi): (hauksbee_ir::Rail, hauksbee_ir::Rail),
    supply: hauksbee_ir::Supply,
    sink: &mut S,
) {
    stamp_quiescent(ctx, supply, sink);
    // Behavioral: drive `out` toward clamp(gain*(vp-vn)) through a stiff
    // conductance, modeling an ideal output stage with finite open-loop gain.
    // In TRANSIENT, when `pole_hz`/`slew` are set, the drive EMF is not the
    // ideal target itself but the target passed through the device's
    // single-pole + slew output dynamics (`opamp_transient_output`), anchored
    // at the EMF persisted from the last ACCEPTED step (`ReactiveState.x1`,
    // seeded from the DC point and rolled forward on step acceptance only,
    // the capacitor's companion-state lifecycle). The DC operating point and
    // the AC path are untouched: a single pole passes DC unattenuated, and
    // slew is a large-signal (transient-only) limit.
    // A card-bound rail rides its supply PIN's net, resolved at THIS iterate and
    // held constant through the linearization: explicit, no ∂out/∂V(pin) column.
    // A supply net is stiff next to the 1 Ω stage; the solution is exact anyway.
    let gout = 1.0; // output stage conductance (1 ohm), strong source
    let (rail_lo, rail_hi) = hauksbee_ir::Rail::band(rail_lo, rail_hi, |n| ctx.v(n));
    let vp = ctx.v(inp);
    let vn = ctx.v(inn);
    let vref = reference.map_or(0.0, |n| ctx.v(n));
    let raw = gain.mul_add(vp - vn, vref);
    let target = raw.clamp(rail_lo, rail_hi);
    // Linearize: out = target + gain*(d vp - d vn) within rails.
    let in_rail = target > rail_lo && target < rail_hi;
    // Rail-crossing limiting, the op-amp's analogue of pnjlim. When the raw
    // (unclamped) target leaps from one side of the rail band clear to the
    // other between Newton iterates, the iterate has jumped OVER the linear
    // region, whose width in input volts is only (rail_hi-rail_lo)/gain. With
    // the saturated flat stamp on both sides, a follower (out tied to inn)
    // driven near a rail then degenerates to the Picard map
    // v_out <- clamp(gain*(vp - v_out)), which flips rail to rail forever and
    // the solve never converges (the stormduino LM358 on /D13 under GPIO
    // drive). The cure is the same as pnjlim's: evaluate the device on the
    // segment the iterate skipped, here the interior linear segment, by
    // keeping the raw target and the input tangents. Inside the band the
    // segment is one straight line, so this is an exact linearization, not a
    // model change: any point the solve CONVERGES to has raw_prev == raw,
    // which cannot cross, so converged results are untouched and both the
    // in-rail and the steadily-saturated stamps stay bit-identical.
    let crossed = !in_rail && {
        let vp0 = ctx.v_prev(inp);
        let vn0 = ctx.v_prev(inn);
        let vref0 = reference.map_or(0.0, |n| ctx.v_prev(n));
        let raw0 = gain.mul_add(vp0 - vn0, vref0);
        (raw >= rail_hi && raw0 <= rail_lo) || (raw <= rail_lo && raw0 >= rail_hi)
    };
    // On a crossing the EMF is the raw target: the RHS then cancels against
    // the tangent terms so the stamped row is exactly out = vref+gain*(vp-vn)
    // at the NEXT iterate, the interior segment itself. Feeding the clamped
    // target with live tangents instead would stamp the segment offset by
    // (clamp(raw)-raw), volts to megavolts of error at the crossing point.
    // Widening the rails away IS "keep the raw target": the dynamics clamp for
    // themselves, after the lag, so a crossing is the ABSENCE of that clamp.
    let wide = (f64::NEG_INFINITY, f64::INFINITY);
    let rails = if crossed { wide } else { (rail_lo, rail_hi) };
    let dynamics = if ctx.dc {
        None
    } else {
        let v_prev = ctx.state.x1[id.0 as usize];
        opamp_transient_output(v_prev, raw, pole_hz, slew, rails, ctx.coeffs.dt)
    };
    // `dscale` scales every input-coupling tangent: the linearized output is
    // v_out(op) + dscale * d(target), so the classic couplings (∂target/∂vp =
    // gain, ∂/∂vn = -gain, ∂/∂vref = 1) each pick up the same factor. The
    // no-dynamics path keeps emf = clamp(raw) and dscale = 1, or 0 when
    // saturated, which is what the guard on the couplings below used to say:
    // bit-identical to the classic instantaneous stamp, in DC and transient.
    let flat = if in_rail || crossed { 1.0 } else { 0.0 };
    let (emf, dscale) = dynamics.unwrap_or_else(|| (raw.clamp(rails.0, rails.1), flat));
    let oi = ctx.layout.node(out);
    // The stage's Norton current `gout * (emf - V(out))` returns through the
    // supply pins, not ground: its `out` row is mirrored into theirs.
    let mut sink = SupplyMirror::new(sink, ctx, oi, supply, gout * (emf - ctx.v(out)));
    let sink = &mut sink;
    if let Some(oi) = oi {
        sink.g(oi, oi, gout);
        sink.i(oi, gout * emf);
        if dscale != 0.0 {
            if let Some(ri) = reference.and_then(|n| ctx.layout.node(n)) {
                sink.g(oi, ri, -gout * dscale);
                sink.i(oi, -(gout * dscale * vref));
            }
            // Couple output to inputs through the gain (tangent).
            if let Some(pi) = ctx.layout.node(inp) {
                sink.g(oi, pi, -gout * dscale * gain);
                sink.i(oi, -(gout * dscale * gain * vp));
            }
            if let Some(ni) = ctx.layout.node(inn) {
                sink.g(oi, ni, gout * dscale * gain);
                sink.i(oi, gout * dscale * gain * vn);
            }
        }
    }
}

/// An output stage's package quiescent current, `hi -> lo`, drawn once the
/// supply is up ([`hauksbee_ir::Supply::quiescent_draw`]) and linearised at this
/// iterate. It rides on the part's own device rather than a separate source
/// so the stamp adds no device, no island coupling and no new matrix slot (the
/// supply pins are already the device's terminals): a board with dozens of
/// op-amps solves exactly as with plain sources wherever its supplies are up.
/// Scaled with the independent sources under source stepping, as the plain
/// current source it replaces was.
fn stamp_quiescent<S: StampSink>(ctx: &StampCtx, supply: hauksbee_ir::Supply, sink: &mut S) {
    let Some(hi) = supply.hi.filter(|_| supply.quiescent_a != 0.0) else {
        return;
    };
    let lo = supply.lo.unwrap_or(NodeId::GROUND);
    let v = ctx.v(hi) - ctx.v(lo);
    let (amps, slope) = supply.quiescent_draw(v);
    let (amps, slope) = (amps * ctx.src_scale, slope * ctx.src_scale);
    stamp_cond(sink, ctx.layout, hi, lo, slope);
    stamp_current(sink, ctx.layout, hi, lo, slope.mul_add(-v, amps));
}

/// A sink that mirrors every write to an output stage's `out` row into the
/// rows of its supply pins, negated and split by [`hauksbee_ir::Supply::share_hi`].
///
/// The `out` row reads `sum(G*V) - rhs`, the current the stage draws OUT of the
/// node, which is minus the current it delivers; the supply rows read what
/// leaves the supply pins, which is that delivered current, so their entries
/// are the `out` row's negated. Row sums then vanish, and ground, which the
/// stage's EMF is referenced to and which is not an unknown, carries nothing.
/// The share is held at the iterate (explicit, like the rail band), so it adds
/// no Jacobian entries and is exact where Newton converges.
struct SupplyMirror<'a, S: StampSink> {
    inner: &'a mut S,
    row: usize,
    /// `(row, share)` of each supply pin that is an unknown.
    legs: [Option<(usize, f64)>; 2],
}

impl<'a, S: StampSink> SupplyMirror<'a, S> {
    /// Mirror row `out` for a stage delivering `amps` at this iterate.
    fn new(
        inner: &'a mut S,
        ctx: &StampCtx,
        out: Option<usize>,
        supply: hauksbee_ir::Supply,
        amps: f64,
    ) -> Self {
        let hi = hauksbee_ir::Supply::share_hi(amps);
        let leg = |node: Option<NodeId>, share: f64| {
            let row = ctx.layout.node(node?).filter(|r| Some(*r) != out)?;
            Some((row, share))
        };
        Self {
            inner,
            row: out.unwrap_or(usize::MAX),
            legs: [leg(supply.hi, hi), leg(supply.lo, 1.0 - hi)],
        }
    }
}

impl<S: StampSink> StampSink for SupplyMirror<'_, S> {
    #[inline]
    fn g(&mut self, row: usize, col: usize, v: f64) {
        self.inner.g(row, col, v);
        if row == self.row {
            for (r, share) in self.legs.into_iter().flatten() {
                self.inner.g(r, col, -share * v);
            }
        }
    }
    #[inline]
    fn i(&mut self, row: usize, v: f64) {
        self.inner.i(row, v);
        if row == self.row {
            for (r, share) in self.legs.into_iter().flatten() {
                self.inner.i(r, -share * v);
            }
        }
    }
}

/// The differential input `V(inp) - V(inn)` at which a comparator whose output
/// is `high` flips. `hysteresis` is the FULL band width, the datasheet VHYS
/// (VT+ minus VT-), so the two trip points sit at plus/minus half of it.
pub fn comparator_trip(high: bool, hysteresis: f64) -> f64 {
    0.5 * if high { -hysteresis } else { hysteresis }
}

/// One end of a push-pull comparator's output swing at this iterate: its
/// voltage, and the unknown it tracks with unit slope when it is a supply pin
/// rather than the card's fixed level.
#[derive(Clone, Copy)]
struct Swing {
    v: f64,
    pin: Option<usize>,
}

/// The `(low, high)` swing a comparator's output stage can reach at this
/// iterate. A push-pull stage cannot drive past its own supply pins: where a
/// pin sits inside the card's `out_lo..out_hi` levels (a starved or
/// still-rising rail) that end of the swing follows the pin. A rail inverted
/// below the floor collapses the band onto the floor, as [`hauksbee_ir::Rail::band`]
/// does for an op-amp.
///
/// The pin is a Newton unknown, not a value held at the iterate: a rail fed
/// through a shunt is anything but stiff next to the 1 ohm stage, and holding
/// it explicit turns the rail/output loop into a fixed-point iteration that
/// cycles forever at the tolerance (measured on a starved analog
/// supply rail: plain Newton parked on a micro-volt-scale two-cycle). A stage
/// without supply pins, or whose supply sits outside its levels, keeps the
/// card's fixed levels and stamps exactly as before.
fn comparator_swing(
    ctx: &StampCtx,
    (out_lo, out_hi): (f64, f64),
    supply: hauksbee_ir::Supply,
) -> (Swing, Swing) {
    let fixed = |v| Swing { v, pin: None };
    let Some(hi_pin) = supply.hi else {
        return (fixed(out_lo), fixed(out_hi));
    };
    let pin = |n: Option<NodeId>, level: f64, inside: fn(f64, f64) -> bool| {
        let v = n.map_or(0.0, |n| ctx.v(n));
        if inside(v, level) {
            Swing {
                v,
                pin: n.and_then(|n| ctx.layout.node(n)),
            }
        } else {
            fixed(level)
        }
    };
    let lo = pin(supply.lo, out_lo, |v, level| v > level);
    let hi = pin(Some(hi_pin), out_hi, |v, level| v < level);
    (lo, if hi.v < lo.v { lo } else { hi })
}

/// Drive a comparator's output unknown `oi` toward `target` through the 1 ohm
/// stage, with `target`'s tangent `w` to each swing end that tracks a supply
/// pin. The stage's Norton current returns through the supply pins, not
/// ground (see `SupplyMirror`), so the caller can add further tangents to the
/// same mirrored row through the returned sink.
fn drive_comparator<'a, S: StampSink>(
    ctx: &StampCtx,
    sink: &'a mut S,
    oi: usize,
    supply: hauksbee_ir::Supply,
    target: f64,
    ends: [(Swing, f64); 2],
) -> SupplyMirror<'a, S> {
    let gout = COMPARATOR_GOUT;
    let amps = gout * (target - ctx.x[oi]);
    let mut sink = SupplyMirror::new(sink, ctx, Some(oi), supply, amps);
    sink.g(oi, oi, gout);
    sink.i(oi, gout * target);
    for (end, w) in ends {
        if let Some(col) = end.pin.filter(|&c| c != oi && w != 0.0) {
            sink.g(oi, col, -gout * w);
            sink.i(oi, -(gout * w * end.v));
        }
    }
    sink
}

/// A comparator output stage's conductance (1 ohm, a strong source).
const COMPARATOR_GOUT: f64 = 1.0;

/// A comparator's LATCHED transient state: the decision its internal latch
/// holds, how long ago that decision changed, and where its output stands
/// across its swing (`0` = low end, `1` = high end), all at the last ACCEPTED
/// step. A march keeps it in [`ReactiveState`]: `xb[0].x1` holds the decision
/// (`+1` high, `-1` low; `0` = never seeded, the state of a history no march
/// seeded), `xb[1].x1` its age and `x1` the output position.
///
/// Why a latch. A comparator with hysteresis is bistable: once its input
/// passes the trip, internal positive feedback carries the output to the other
/// rail whatever the input does next, unless it recrosses the OTHER trip a
/// whole band away. The smooth staged transfer (a 2 mV-wide logistic of the
/// input) has no such memory. Inside one Newton solve it is a linear amplifier
/// of gain ~2500, so a comparator whose output resets its own input (a
/// pulse generator whose output closes the switch that discharges its
/// integrator, any relaxation oscillator) found a REGULATING root with the output parked mid-swing, and
/// the march crawled through it at 0.1 ns steps until its Newton budget ran out
/// (observed on a large board: several such stages held mid-swing with their
/// inputs a few mV over threshold until the step budget ran out, then the
/// fallback ladder ran for minutes more).
/// Reading the hysteresis side from the iterate instead has no root at all:
/// the trip jumps the whole band as the output passes mid-swing.
///
/// Latched, the decision is taken from the accepted state between steps (the
/// march lands a step on each input crossing, see `crossing_fraction`), and
/// within a step the output is a function of time alone: the inner Newton sees
/// the comparator as a scheduled source and converges as it does on any
/// feed-forward stage.
///
/// The output follows the decision with the card's propagation delay `tpd_s`
/// (input crossing to output mid-swing) as an INERTIAL delay: it holds for
/// `tpd_s / 2`, then slews linearly across its swing in `tpd_s`, so it passes
/// mid-swing `tpd_s` after the decision. A decision that reverts inside the
/// hold never reaches the output (an input pulse too short for the part to
/// answer), and a longer one keeps its width, shifted by the delay: a relaxation
/// oscillator's output holds for the delay after its integrator has started to
/// reset, as the real part does. The card gives no edge time; the slew over
/// `tpd_s` is a bounded slope that keeps the output stage continuous for the
/// march, not a datasheet rise time. A part without a stated delay steps at
/// the decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComparatorLatch {
    /// The decision the internal latch holds.
    pub high: bool,
    /// Seconds since `high` last changed (infinite when no change is pending).
    pub age: f64,
    /// Output position across the swing, `0` low end .. `1` high end.
    pub frac: f64,
}

impl ComparatorLatch {
    /// The latch of comparator `id` in `state`, `None` when no march seeded it.
    #[must_use]
    pub fn of(state: &ReactiveState, id: DeviceId) -> Option<Self> {
        let i = id.0 as usize;
        let side = *state.xb[0].x1.get(i)?;
        (side != 0.0).then(|| Self {
            high: side > 0.0,
            age: state.xb[1].x1[i],
            frac: state.x1[i],
        })
    }

    /// A settled latch: deciding `high`, no change pending, output at `frac`.
    #[must_use]
    pub const fn settled(high: bool, frac: f64) -> Self {
        Self {
            high,
            age: f64::INFINITY,
            frac,
        }
    }

    /// Store this latch as comparator `id`'s accepted history in `state`.
    pub fn store(self, state: &mut ReactiveState, id: DeviceId) {
        let i = id.0 as usize;
        let side = if self.high { 1.0 } else { -1.0 };
        for (bank, v) in [(0, side), (1, self.age)] {
            let b = &mut state.xb[bank];
            (b.x1[i], b.x2[i], b.dx1[i]) = (v, v, 0.0);
        }
        (state.x1[i], state.x2[i], state.dx1[i]) = (self.frac, self.frac, 0.0);
    }

    /// The hold before the output starts toward a new decision.
    fn hold(tpd_s: f64) -> f64 {
        0.5 * tpd_s
    }

    /// Output position after `h` more seconds of following the decision.
    #[must_use]
    pub fn frac_after(self, tpd_s: f64, h: f64) -> f64 {
        let slew = |frac: f64, high: bool, dt: f64| {
            if tpd_s <= 0.0 {
                return if high { 1.0 } else { 0.0 };
            }
            let moved = dt / tpd_s;
            if high {
                (frac + moved).min(1.0)
            } else {
                (frac - moved).max(0.0)
            }
        };
        let switch_in = Self::hold(tpd_s) - self.age;
        if switch_in <= 0.0 {
            slew(self.frac, self.high, h)
        } else if h <= switch_in {
            slew(self.frac, !self.high, h)
        } else {
            slew(
                slew(self.frac, !self.high, switch_in),
                self.high,
                h - switch_in,
            )
        }
    }

    /// Time from now to the next corner of the output's trajectory (a pending
    /// hold ending, or a slew reaching its end), or `None` when it is settled.
    /// A step that strides a corner smears a kink the march must land on.
    #[must_use]
    pub fn next_corner(self, tpd_s: f64) -> Option<f64> {
        if tpd_s <= 0.0 {
            return None;
        }
        let switch_in = Self::hold(tpd_s) - self.age;
        if switch_in > 0.0 {
            return Some(switch_in);
        }
        let left = if self.high {
            1.0 - self.frac
        } else {
            self.frac
        };
        (left > 0.0).then_some(left * tpd_s)
    }

    /// The latch after an accepted step of `h` whose end state has input
    /// difference `d`: the output moved along its trajectory, and the latch
    /// re-decided across its hysteresis band. A decision reverting inside its
    /// hold cancels it (nothing pending). Also returns whether it re-decided.
    #[must_use]
    pub fn advanced(self, tpd_s: f64, h: f64, d: f64, hysteresis: f64) -> (Self, bool) {
        let frac = self.frac_after(tpd_s, h);
        let age = self.age + h;
        let high = d > comparator_trip(self.high, hysteresis);
        if high == self.high {
            return (Self { high, age, frac }, false);
        }
        let pending = age < Self::hold(tpd_s);
        let age = if pending { f64::INFINITY } else { 0.0 };
        (Self { high, age, frac }, true)
    }
}

/// The `(low, high)` output swing of a push-pull comparator at node voltages
/// `v`, by the same rule as the stamp's (see `comparator_swing`).
#[must_use]
pub fn comparator_swing_volts(
    v: impl Fn(NodeId) -> f64,
    (out_lo, out_hi): (f64, f64),
    supply: hauksbee_ir::Supply,
) -> (f64, f64) {
    let Some(hi_pin) = supply.hi else {
        return (out_lo, out_hi);
    };
    // Exactly `comparator_swing`'s rule, an absent low pin reading 0 V.
    let pin_lo = supply.lo.map_or(0.0, &v);
    let lo = if pin_lo > out_lo { pin_lo } else { out_lo };
    let pin_hi = v(hi_pin);
    let hi = if pin_hi < out_hi { pin_hi } else { out_hi };
    (lo, if hi < lo { lo } else { hi })
}

/// The supply pin a push-pull comparator's settled output follows with unit
/// gain at node voltages `v`, by `comparator_swing`'s rule: the high end while
/// the output is high and its supply pin sits below the card's VOH, the low end
/// while it is low and its ground/negative pin sits above VOL. `None` when the
/// held end is the card's fixed level (small-signal AC ground). The `.ac` stamp
/// reads this so a supply-limited output carries the DC stamp's tangent.
#[must_use]
pub fn comparator_held_pin(
    v: impl Fn(NodeId) -> f64,
    v_out: f64,
    (out_lo, out_hi): (f64, f64),
    supply: hauksbee_ir::Supply,
) -> Option<NodeId> {
    let hi_pin = supply.hi?;
    let pin_lo = supply.lo.map_or(0.0, &v);
    let lo = (pin_lo > out_lo).then_some(supply.lo).flatten();
    let lo_v = if pin_lo > out_lo { pin_lo } else { out_lo };
    let pin_hi = v(hi_pin);
    let (hi, hi_v) = if pin_hi < out_hi {
        (Some(hi_pin), pin_hi)
    } else {
        (None, out_hi)
    };
    // A collapsed swing (supply below VOL) holds the low end either way.
    let (hi, hi_v) = if hi_v < lo_v { (lo, lo_v) } else { (hi, hi_v) };
    if v_out > 0.5 * (lo_v + hi_v) {
        hi
    } else {
        lo
    }
}

fn stamp_comparator<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    (out, inp, inn): (NodeId, NodeId, NodeId),
    (out_lo, out_hi, hyst, tpd_s): (f64, f64, f64, f64),
    supply: hauksbee_ir::Supply,
    sink: &mut S,
) {
    stamp_quiescent(ctx, supply, sink);
    let (lo, hi) = comparator_swing(ctx, (out_lo, out_hi), supply);
    let vp = ctx.v(inp);
    let vn = ctx.v(inn);
    // Hysteresis: the trip point depends on the current output state.
    let high = ctx.v(out) > 0.5 * (lo.v + hi.v);
    // Event-driven staged path: the outer loop supplies a FROZEN decision for
    // this comparator. Hold its output state fixed for the whole inner Newton
    // solve so it cannot chatter; the threshold is set by the frozen state's
    // hysteresis side. This is the limit-cycle cure: with every comparator's
    // state fixed, the inner circuit is smooth and Newton converges; the outer
    // loop re-evaluates and re-solves if any decision flipped.
    // A comparator the map leaves out decides for itself as usual (the
    // pinned-residual probe freezes only the pinned ones).
    if let Some(&high) = ctx.cmp_freeze.and_then(|freeze| freeze.get(&id)) {
        // PIN the output to its frozen rail with a CONSTANT target (no input
        // dependence). A smooth input-dependent transfer still swings the output
        // whenever the comparator inputs pass near threshold, which makes the
        // output node chase the inputs and the inner Newton limit-cycle (the
        // output flip-flops at the step cap forever). Decoupling the output from
        // the inputs entirely, output := frozen rail, makes the inner circuit
        // strictly feed-FORWARD through the comparator, so Newton converges
        // cleanly. The outer event loop is what enforces input/output
        // consistency: it re-derives each comparator's rail from the converged
        // inputs and re-solves until no rail flips, so the fixed point is a true
        // root with every output consistent with its own inputs.
        let (target, weights) = if high {
            (hi.v, [0.0, 1.0])
        } else {
            (lo.v, [1.0, 0.0])
        };
        if let Some(oi) = ctx.layout.node(out) {
            drive_comparator(
                ctx,
                sink,
                oi,
                supply,
                target,
                [(lo, weights[0]), (hi, weights[1])],
            );
        }
        return;
    }

    // Latched transient path (see `ComparatorLatch`): a staged-armed march
    // step drives the output to where it has slewed toward the decision the
    // accepted state holds, independent of this step's inputs.
    if ctx.branch_reg > 0.0 && !ctx.dc {
        if let Some(latch) = ComparatorLatch::of(ctx.state, id) {
            let frac = latch.frac_after(tpd_s, ctx.coeffs.dt);
            if let Some(oi) = ctx.layout.node(out) {
                drive_comparator(
                    ctx,
                    sink,
                    oi,
                    supply,
                    (hi.v - lo.v).mul_add(frac, lo.v),
                    [(lo, 1.0 - frac), (hi, frac)],
                );
            }
            return;
        }
    }

    let thresh = comparator_trip(high, hyst);

    // Staged-DC path (branch_reg > 0) without an event-freeze map: the bare
    // bang-bang transfer below has no tangent, so on a stiff board where a
    // comparator input sits near threshold the output chatters between rails
    // every Newton iteration. Replace it, for the staged solve only, with a
    // smooth high-gain logistic transfer that has a real derivative. A real
    // LMV7219 has finite (high) open-loop gain, so this is physically faithful;
    // the discrete model below is kept bit-identical for every normal solve
    // (branch_reg == 0 and no freeze map).
    if ctx.branch_reg > 0.0 {
        let span = hi.v - lo.v;
        let k = ctx.opts.effects.cmp_smooth_gain; // 1/V; ~2 mV transition width at the default
        let d = vp - vn - thresh;
        let kd = (k * d).clamp(-40.0, 40.0);
        let e = (-kd).exp();
        let sig = 1.0 / (1.0 + e); // logistic in (0,1)
        let target = span.mul_add(sig, lo.v);
        let dsig = k * sig * (1.0 - sig);
        let dtarget_dvp = span * dsig; // d target / d vp  (= -d/d vn)
        if let Some(oi) = ctx.layout.node(out) {
            let gout = COMPARATOR_GOUT;
            let mut sink =
                drive_comparator(ctx, sink, oi, supply, target, [(lo, 1.0 - sig), (hi, sig)]);
            if let Some(pi) = ctx.layout.node(inp) {
                sink.g(oi, pi, -gout * dtarget_dvp);
                sink.i(oi, -(gout * dtarget_dvp * vp));
            }
            if let Some(ni) = ctx.layout.node(inn) {
                sink.g(oi, ni, gout * dtarget_dvp);
                sink.i(oi, gout * dtarget_dvp * vn);
            }
        }
        return;
    }

    let (target, weights) = if vp - vn > thresh {
        (hi.v, [0.0, 1.0])
    } else {
        (lo.v, [1.0, 0.0])
    };
    if let Some(oi) = ctx.layout.node(out) {
        drive_comparator(
            ctx,
            sink,
            oi,
            supply,
            target,
            [(lo, weights[0]), (hi, weights[1])],
        );
    }
}

// --- low-level stamp helpers ------------------------------------------------

#[inline]
/// [`stamp_cond`] against already-resolved unknown indices, so a device whose
/// intrinsic sits on an internal node can stamp there.
fn stamp_cond_idx<S: StampSink>(sink: &mut S, a: Option<usize>, b: Option<usize>, g: f64) {
    if let Some(ai) = a {
        sink.g(ai, ai, g);
    }
    if let Some(bi) = b {
        sink.g(bi, bi, g);
    }
    if let (Some(ai), Some(bi)) = (a, b) {
        sink.g(ai, bi, -g);
        sink.g(bi, ai, -g);
    }
}

/// [`stamp_current`] against already-resolved unknown indices.
fn stamp_current_idx<S: StampSink>(sink: &mut S, a: Option<usize>, b: Option<usize>, i: f64) {
    if let Some(ai) = a {
        sink.i(ai, -i);
    }
    if let Some(bi) = b {
        sink.i(bi, i);
    }
}

/// Stamp a transconductance: current into `(ip, in)` proportional to the
/// voltage across `(cp, cn)`. `i = gm * (v_cp - v_cn)` added at ip, removed
/// at in.
#[inline]
fn add_transconductance<S: StampSink>(
    sink: &mut S,
    ip: Option<usize>,
    in_: Option<usize>,
    cp: Option<usize>,
    cn: Option<usize>,
    gm: f64,
) {
    if let Some(ip) = ip {
        if let Some(cp) = cp {
            sink.g(ip, cp, gm);
        }
        if let Some(cn) = cn {
            sink.g(ip, cn, -gm);
        }
    }
    if let Some(in_) = in_ {
        if let Some(cp) = cp {
            sink.g(in_, cp, -gm);
        }
        if let Some(cn) = cn {
            sink.g(in_, cn, gm);
        }
    }
}

#[inline]
fn inject<S: StampSink>(sink: &mut S, node: Option<usize>, val: f64) {
    if let Some(n) = node {
        sink.i(n, val);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::Integration;
    use crate::system::ReactiveState;
    use crate::test_fixtures::{bjt, diode, stamp_ctx, trapz, GND};
    use hauksbee_ir::{BDep, Circuit, CompiledExpr, NodeId};

    fn diode_charge_model() -> DiodeModel {
        DiodeModel {
            cjo: 4e-12,
            vj: 0.7,
            m: 0.45,
            tt: 20e-9,
            ..DiodeModel::default()
        }
    }

    fn bjt_charge_model() -> BjtModel {
        BjtModel {
            cje: 20e-12,
            cjc: 8e-12,
            tf: 400e-12,
            tr: 50e-9,
            ..BjtModel::default()
        }
    }

    /// Assemble `c` at iterate `x` (previous iterate `x_prev`) with the given
    /// options, returning `(layout, matrix, rhs, residual `g*x - rhs`)`.
    fn assemble(
        c: &Circuit,
        x: &[f64],
        x_prev: &[f64],
        opts: &SolverOptions,
        state: &ReactiveState,
        dc: bool,
        gmin: f64,
        branch_reg: f64,
    ) -> (Layout, SparseMatrix, Vec<f64>, Vec<f64>) {
        let layout = Layout::new(c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(c, &layout, &mut m);
        let mut ctx = stamp_ctx(c, &layout, opts, x, state, trapz(1e-9));
        ctx.x_prev = x_prev;
        ctx.dc = dc;
        ctx.gmin = gmin;
        ctx.branch_reg = branch_reg;
        let mut rhs = vec![0.0; layout.size];
        stamp_all(&ctx, &mut m, &mut rhs);
        let f: Vec<f64> = (0..layout.size)
            .map(|i| m.row(i).iter().map(|&(j, v)| v * x[j]).sum::<f64>() - rhs[i])
            .collect();
        (layout, m, rhs, f)
    }

    fn at(m: &SparseMatrix, r: usize, c: usize) -> f64 {
        m.row(r)
            .iter()
            .find(|(j, _)| *j == c)
            .map_or(0.0, |(_, v)| *v)
    }

    /// A comparator whose swing tracks a starved supply pin stamps that pin's
    /// tangent: on the output row, the Jacobian column for the rail matches a
    /// finite difference of the residual, on the bang-bang and the staged
    /// smooth transfer alike.
    #[test]
    fn comparator_swing_tangent_matches_finite_difference() {
        let mut c = Circuit::new();
        let (rail, inp, out) = (c.node("RAIL"), c.node("INP"), c.node("OUT"));
        c.add(Device::Comparator {
            name: "K".into(),
            out,
            inp,
            inn: GND,
            out_lo: 0.05,
            out_hi: 4.95,
            hysteresis: 0.0075,
            tpd_s: 0.0,
            supply: hauksbee_ir::Supply {
                hi: Some(rail),
                lo: None,
                quiescent_a: 1e-3,
            },
        });
        let layout = Layout::new(&c);
        let (ri, pi, oi) = (
            layout.node(rail).unwrap(),
            layout.node(inp).unwrap(),
            layout.node(out).unwrap(),
        );
        let opts = SolverOptions::default();
        let state = ReactiveState::new(c.devices.len());
        // Inputs a hair above the trip so the smooth transfer is mid-swing too.
        for (vin, branch_reg) in [(1.0, 0.0), (1.0, 1e-2), (4.0e-3, 1e-2)] {
            let mut x = vec![0.0; layout.size];
            (x[ri], x[pi], x[oi]) = (0.3, vin, 0.02);
            let (_, m, _, f0) = assemble(&c, &x, &x, &opts, &state, true, 0.0, branch_reg);
            let h = 1e-7;
            x[ri] += h;
            let (_, _, _, f1) = assemble(&c, &x, &x, &opts, &state, true, 0.0, branch_reg);
            let fd = (f1[oi] - f0[oi]) / h;
            let jac = at(&m, oi, ri);
            assert!(
                jac != 0.0,
                "rail column stamped (vin {vin}, reg {branch_reg})"
            );
            assert!(
                (fd - jac).abs() < 1e-5,
                "vin {vin} reg {branch_reg}: fd {fd} vs J {jac}"
            );
        }
    }

    fn fd_matches_cap(q: impl Fn(f64) -> (f64, f64), points: &[f64], what: &str) {
        for &v in points {
            let (_, c) = q(v);
            let d = 1e-7;
            let dq = (q(v + d).0 - q(v - d).0) / (2.0 * d);
            assert!(
                (dq - c).abs() < 1e-4 * c.abs().max(1e-18),
                "{what}: dQ/dv != C at v={v}: fd={dq:e} c={c:e}"
            );
        }
    }

    /// The FC-knee continuation is C1 in the capacitance (C and dC/dv
    /// continuous), Q'(v) == C by finite difference on both sides, and the
    /// shared depletion helper reproduces the diode arithmetic bit-for-bit.
    #[test]
    fn junction_charge_is_the_c1_integral_of_the_cap() {
        let m = diode_charge_model();
        let eval = |vd: f64| {
            let (idc, gd) = diode_eval(&m, vd, 27.0, false);
            diode_charge(&m, vd, idc, gd)
        };
        let knee = DIODE_FC * m.vj;
        let eps = 1e-9;
        let ((q_lo, c_lo), (q_hi, c_hi)) = (eval(knee - eps), eval(knee + eps));
        assert!(
            (q_hi - q_lo).abs() < 1e-6 * q_lo.abs().max(1e-15),
            "Q jump at knee"
        );
        assert!((c_hi - c_lo).abs() < 1e-6 * c_lo, "C jump at knee");
        let d = 1e-6;
        let slope_lo = (eval(knee - eps).1 - eval(knee - eps - d).1) / d;
        let slope_hi = (eval(knee + eps + d).1 - eval(knee + eps).1) / d;
        assert!(
            (slope_hi - slope_lo).abs() < 1e-3 * slope_lo.abs(),
            "dC/dv kink: {slope_lo:e} vs {slope_hi:e}"
        );
        fd_matches_cap(
            eval,
            &[-5.0, -1.0, -0.2, 0.0, 0.2, 0.34, 0.4, 0.6, 0.9],
            "diode",
        );

        let m = DiodeModel {
            tt: 0.0,
            ..diode_charge_model()
        };
        for &vd in &[-5.0, -1.0, 0.0, 0.2, 0.34, 0.35, 0.4, 0.9] {
            let (q_h, c_h) = depletion_charge(m.cjo, m.vj, m.m, DIODE_FC, vd);
            let (idc, gd) = diode_eval(&m, vd, 27.0, false);
            let (q_d, c_d) = diode_charge(&m, vd, idc, gd);
            assert_eq!(
                q_d.to_bits(),
                (0.0f64 + q_h).to_bits(),
                "Q mismatch at {vd}"
            );
            assert_eq!(
                c_d.to_bits(),
                (0.0f64 + c_h).to_bits(),
                "C mismatch at {vd}"
            );
        }
    }

    /// Breakdown: REVERSE current growing exponentially below -bv, smooth across
    /// -bv, bounded by the exponent clamp; bv = ∞ leaves plain reverse leakage.
    #[test]
    fn breakdown_current_is_reverse_continuous_and_bounded() {
        let m = DiodeModel {
            bv: 6.2,
            ..DiodeModel::default()
        };
        let nvt = m.n * hauksbee_ir::thermal_voltage_c(27.0);
        let (i_at, _) = diode_eval(&m, -6.2, 27.0, false);
        assert!(
            2.0f64.mul_add(m.is, i_at).abs() < 1e-3 * m.is,
            "leakage plus a full-scale exponential at -bv: {i_at:e}"
        );
        let (i_below, _) = diode_eval(&m, -6.2 - 1e-6, 27.0, false);
        assert!((i_below - i_at).abs() < 1e-3 * m.is, "step at -bv");
        let (i_past, g_past) = diode_eval(&m, 5.0f64.mul_add(-nvt, -6.2), 27.0, false);
        assert!(i_past < 0.0 && g_past > 0.0);
        assert!(m.is.mul_add(1.0 + 5.0f64.exp(), i_past).abs() < 1e-6 * m.is * 5.0f64.exp());
        let (i_deep, g_deep) = diode_eval(&m, -1e6, 27.0, false);
        assert!(i_deep.is_finite() && g_deep.is_finite());
        assert!(m.is.mul_add(40.0f64.exp(), i_deep).abs() < 1e-6 * m.is * 40.0f64.exp());
        let m_inf = DiodeModel::default();
        let (i_rev, g_rev) = diode_eval(&m_inf, -100.0, 27.0, false);
        assert_eq!(i_rev.to_bits(), (-m_inf.is).to_bits());
        assert_eq!(g_rev.to_bits(), (m_inf.is / nvt * 1e-3).to_bits());
    }

    fn one_diode(model: DiodeModel) -> Circuit {
        let mut c = Circuit::new();
        let a = c.node("a");
        diode(&mut c, "D1", a, GND, model);
        c
    }

    fn one_bjt(model: BjtModel) -> (Circuit, NodeId, NodeId) {
        let mut c = Circuit::new();
        let (nb, nc) = (c.node("b"), c.node("c"));
        bjt(&mut c, "Q1", nc, nb, GND, &model);
        (c, nb, nc)
    }

    /// Flipping `junction_caps` changes the stamp (companion conductance and
    /// history RHS) for charge-carrying diode and BJT models, and leaves
    /// charge-free models bit-identical.
    #[test]
    #[allow(clippy::float_cmp)] // exact: the toggle must leave or change stamps bitwise
    fn junction_caps_toggle_changes_only_charge_carrying_stamps() {
        let diode_stamp = |model: DiodeModel, junction_caps: bool| {
            let c = one_diode(model);
            let mut opts = SolverOptions::default();
            opts.effects.junction_caps = junction_caps;
            let mut state = ReactiveState::new(1);
            state.x1[0] = 1e-12;
            let x = vec![0.62];
            let (_, m, rhs, _) = assemble(&c, &x, &x, &opts, &state, false, 0.0, 0.0);
            (at(&m, 0, 0), rhs[0])
        };
        let bjt_stamp = |model: BjtModel, junction_caps: bool| {
            let (c, nb, nc) = one_bjt(model);
            let mut opts = SolverOptions::default();
            opts.effects.junction_caps = junction_caps;
            let layout = Layout::new(&c);
            let mut x = vec![0.65; layout.size];
            x[layout.node(nc).unwrap()] = 3.0;
            let mut state = ReactiveState::new(1);
            state.x1[0] = 1e-12;
            state.xb[0].x1[0] = -2e-12;
            let (layout, m, rhs, _) = assemble(&c, &x, &x, &opts, &state, false, 0.0, 0.0);
            let b_row = layout.node(nb).unwrap();
            (at(&m, b_row, b_row), rhs[b_row])
        };
        for (on, off) in [
            (
                diode_stamp(diode_charge_model(), true),
                diode_stamp(diode_charge_model(), false),
            ),
            (
                bjt_stamp(bjt_charge_model(), true),
                bjt_stamp(bjt_charge_model(), false),
            ),
        ] {
            assert!(
                on.0 > off.0,
                "junction_caps on must add companion conductance"
            );
            assert!(on.1 != off.1, "junction_caps on must add history RHS terms");
        }
        for (on, off) in [
            (
                diode_stamp(DiodeModel::default(), true),
                diode_stamp(DiodeModel::default(), false),
            ),
            (
                bjt_stamp(BjtModel::default(), true),
                bjt_stamp(BjtModel::default(), false),
            ),
        ] {
            assert_eq!(on.0.to_bits(), off.0.to_bits());
            assert_eq!(on.1.to_bits(), off.1.to_bits());
        }
    }

    /// Diode RS moves the junction onto an intrinsic anode bridged by 1/rs; a
    /// diode without RS allocates no internal node.
    #[test]
    fn diode_series_resistance_is_stamped_on_an_intrinsic_node() {
        let c = one_diode(DiodeModel {
            rs: 0.5,
            ..DiodeModel::default()
        });
        let layout = Layout::new(&c);
        assert_eq!(layout.size, 2);
        let x = vec![0.6; layout.size];
        let state = ReactiveState::new(1);
        let (layout, m, _, _) = assemble(
            &c,
            &x,
            &x,
            &SolverOptions::default(),
            &state,
            false,
            0.0,
            0.0,
        );
        let int = layout.diode_internal(DeviceId(0)).expect("intrinsic anode");
        assert!(
            (at(&m, 0, int).abs() - 2.0).abs() < 1e-9,
            "off-diagonal carries 1/rs"
        );
        assert!(
            at(&m, int, int) > 2.0,
            "intrinsic diagonal carries the bridge plus the junction"
        );

        let layout = Layout::new(&one_diode(DiodeModel::default()));
        assert!(layout.diode_internal(DeviceId(0)).is_none());
        assert_eq!(layout.size, 1);
    }

    /// Flipping `series_resistance` moves the BJT core onto internal nodes
    /// behind rb/re/rc; a default model is bit-identical across the toggle.
    #[test]
    fn series_resistance_toggle_changes_bjt_stamp() {
        let run = |model: BjtModel, series: bool| {
            let (c, _, nc) = one_bjt(model);
            let mut opts = SolverOptions::default();
            opts.effects.series_resistance = series;
            let layout = Layout::new(&c);
            let mut x = vec![0.65; layout.size];
            x[layout.node(nc).unwrap()] = 3.0;
            let mut state = ReactiveState::new(1);
            state.x1[0] = 1e-12;
            state.xb[0].x1[0] = -2e-12;
            assemble(&c, &x, &x, &opts, &state, false, 0.0, 0.0)
        };
        let model = BjtModel {
            rb: 100.0,
            re: 1.0,
            rc: 10.0,
            ..BjtModel::default()
        };
        let (layout_on, m_on, _, _) = run(model, true);
        let (_, m_off, _, _) = run(model, false);
        assert_eq!(layout_on.n_nodes, 2 + 3);
        assert_eq!(
            at(&m_on, 0, 0).to_bits(),
            (1.0f64 / 100.0).to_bits(),
            "external base row carries only 1/rb"
        );
        assert_ne!(at(&m_off, 0, 0).to_bits(), at(&m_on, 0, 0).to_bits());
        assert_eq!(
            at(&m_off, 2, 2).to_bits(),
            1.0f64.to_bits(),
            "toggle OFF pins each internal unknown"
        );

        let (l_def, m_def_on, r_def_on, _) = run(BjtModel::default(), true);
        let (_, m_def_off, r_def_off, _) = run(BjtModel::default(), false);
        assert_eq!(l_def.n_nodes, 2);
        assert_eq!(
            at(&m_def_on, 0, 0).to_bits(),
            at(&m_def_off, 0, 0).to_bits()
        );
        assert_eq!(r_def_on[0].to_bits(), r_def_off[0].to_bits());
    }

    #[test]
    #[allow(clippy::similar_names, clippy::float_cmp)] // q_be/q_bc junction charges; exact cap identity
    fn bjt_charge_is_integral_of_cap() {
        let m = bjt_charge_model();
        let vt = hauksbee_ir::thermal_voltage_c(27.0);
        let q_be = |v: f64| {
            let nvf = m.nf * vt;
            let ef = (v / nvf).clamp(-40.0, 40.0).exp();
            bjt_charge_be(&m, v, m.is * (ef - 1.0), m.is * ef / nvf)
        };
        let q_bc = |v: f64| {
            let nvr = m.nr * vt;
            let er = (v / nvr).clamp(-40.0, 40.0).exp();
            bjt_charge_bc(&m, v, m.is * (er - 1.0), m.is * er / nvr)
        };
        fd_matches_cap(q_be, &[-5.0, -1.0, 0.0, 0.2, 0.37, 0.5, 0.65], "Qbe");
        fd_matches_cap(q_bc, &[-12.0, -3.0, 0.0, 0.37, 0.5], "Qbc");
        // MJE comes from the card, not a hardcoded 0.33 (a vendor 2N3904 card
        // states MJE=.2593, so its depletion cap must differ from the default).
        let cap = |b: &BjtModel| bjt_charge_be(b, -2.0, 0.0, 0.0).1;
        let g = BjtModel { mje: 0.2593, ..m };
        assert_eq!(
            cap(&g),
            depletion_charge(m.cje, m.vje, 0.2593, m.fc, -2.0).1
        );
        assert!(cap(&g) != cap(&m), "MJE was ignored");
    }

    /// Residual rows `(f_b, f_c)` of a one-BJT system at a pinned iterate,
    /// temperature effects off so the arithmetic matches by hand.
    fn bjt_residual(model: BjtModel, vb: f64, vc: f64, vb_prev: f64) -> (f64, f64) {
        let (c, nb, nc) = one_bjt(model);
        let layout = Layout::new(&c);
        let (b_row, c_row) = (layout.node(nb).unwrap(), layout.node(nc).unwrap());
        let mut x = vec![0.0; layout.size];
        x[b_row] = vb;
        x[c_row] = vc;
        let mut x_prev = x.clone();
        x_prev[b_row] = vb_prev;
        let mut opts = SolverOptions::default();
        opts.effects.temperature = false;
        let state = ReactiveState::new(1);
        let (_, _, _, f) = assemble(&c, &x, &x_prev, &opts, &state, true, 0.0, 0.0);
        (f[b_row], f[c_row])
    }

    /// A finite VAR scales the transport current by exactly the SGP factor
    /// `1 - vbc/VAF - vbe/VAR` (shrinking ic); the base current is untouched;
    /// VAR = ∞ reproduces the VAF-only analytic collector current.
    #[test]
    #[allow(clippy::similar_names)] // fb/fc residuals for the inf/fin models
    fn bjt_var_scales_transport_current() {
        let vt = hauksbee_ir::thermal_voltage_c(27.0);
        let (vb, vc) = (0.65, 3.0);
        let m_inf = BjtModel {
            vaf: 100.0,
            ..BjtModel::default()
        };
        let m_fin = BjtModel { var: 15.0, ..m_inf };
        let (fb_inf, fc_inf) = bjt_residual(m_inf, vb, vc, vb);
        let (fb_fin, fc_fin) = bjt_residual(m_fin, vb, vc, vb);
        assert_eq!(fb_inf.to_bits(), fb_fin.to_bits());
        let (vbe, vbc) = (vb, vb - vc);
        let q_inf = 1.0 - vbc / 100.0;
        let q_fin = q_inf - vbe / 15.0;
        assert!(fc_fin.abs() < fc_inf.abs(), "finite VAR must reduce ic");
        assert!((fc_fin / fc_inf - q_fin / q_inf).abs() < 1e-9);
        let cf = m_inf.is * (vbe / vt).clamp(-40.0, 40.0).exp_m1();
        let cr = m_inf.is * (vbc / vt).clamp(-40.0, 40.0).exp_m1();
        let ic = (cf - cr).mul_add(q_inf, -(cr / m_inf.br));
        assert!(
            (fc_inf.abs() - ic).abs() < 1e-9 * ic,
            "{} != {ic}",
            fc_inf.abs()
        );
    }

    /// The vbc limiter uses the REVERSE critical voltage (nr·Vt), so with
    /// nf != nr a step below the nr-built vcrit passes unlimited.
    #[test]
    #[allow(clippy::float_cmp)] // exact: pnjlim passes an unlimited step through untouched
    fn bjt_vbc_limiter_uses_reverse_vcrit() {
        let vt = hauksbee_ir::thermal_voltage_c(27.0);
        let m = BjtModel {
            nr: 1.5,
            ..BjtModel::default()
        };
        let vcrit_f = vcrit(m.is, m.nf * vt);
        let vcrit_r = vcrit(m.is, m.nr * vt);
        assert!(vcrit_f < 1.0 && 1.0 < vcrit_r);
        assert_eq!(pnjlim(1.0, 0.9, m.nr * vt, vcrit_r), 1.0);
        assert_ne!(pnjlim(1.0, 0.9, m.nr * vt, vcrit_f), 1.0);
        let (fb, _) = bjt_residual(m, 1.0, 0.0, 0.9);
        let (nvf, nvr) = (m.nf * vt, m.nr * vt);
        let vbe_lim = pnjlim(1.0, 0.9, nvf, vcrit_f);
        let ef = (vbe_lim / nvf).clamp(-40.0, 40.0).exp();
        let er = (1.0 / nvr).clamp(-40.0, 40.0).exp();
        let ib = m.is * (ef - 1.0) / m.bf + m.is * (er - 1.0) / m.br;
        let gpi = m.is * ef / (nvf * m.bf);
        let expected = ib + gpi * (1.0 - vbe_lim);
        assert!(
            (fb.abs() - expected).abs() < 1e-9 * expected,
            "{} != {expected}",
            fb.abs()
        );
    }

    /// The MOS channel gives id and gm with no jump across threshold, and
    /// gm == d id/d vgs and gds == d id/d vds by finite difference, for
    /// signal- and power-scale beta, with and without CLM, in saturation and
    /// triode.
    #[test]
    fn mos_channel_id_and_gm_continuous_across_vth() {
        let vt = hauksbee_ir::thermal_voltage_c(27.0);
        for &(beta, nsub, lambda, vds) in &[
            (2e-5, 1.0, 0.0, 2.0),
            (2e-5, 2.0, 0.02, 2.0),
            (20.0, 1.5, 0.05, 2.0),
            (20.0, 1.5, 0.05, 0.03),
        ] {
            let nvt: f64 = nsub * vt;
            let f = |vov: f64| mos_channel(beta, nvt, vov, vds, lambda, 0.0);
            let clm = lambda.mul_add(vds, 1.0);
            let h = 1e-4;
            let mut prev = f(-0.3);
            for k in 1..=6000 {
                let vov = f64::from(k).mul_add(h, -0.3);
                let cur = f(vov);
                assert!(
                    (cur.0 - prev.0).abs() <= (prev.1.max(cur.1) * h).mul_add(1.5, 1e-18),
                    "id jump at vov={vov} (beta={beta})"
                );
                assert!(
                    (cur.1 - prev.1).abs() <= (3.0 * beta * clm).mul_add(h, 1e-18),
                    "gm jump at vov={vov} (beta={beta})"
                );
                prev = cur;
            }
            let (below, at0, above) = (f(-1e-9), f(0.0), f(1e-9));
            assert!((above.0 - below.0).abs() <= 1e-6 * at0.0.abs().max(1e-30));
            assert!((above.1 - below.1).abs() <= 1e-6 * at0.1.abs().max(1e-30));
            for &v in &[-0.1, -0.02, 0.0, 0.02, 0.1] {
                let d = 1e-7;
                let fd = (f(v + d).0 - f(v - d).0) / (2.0 * d);
                let gm = f(v).1;
                assert!(
                    (fd - gm).abs() <= 1e-4 * gm.abs().max(1e-15),
                    "gm != d id/d vgs at vov={v}: fd={fd:e} gm={gm:e}"
                );
                let g = |x: f64| mos_channel(beta, nvt, v, x, lambda, 0.0);
                let (fdd, gds) = ((g(vds + d).0 - g(vds - d).0) / (2.0 * d), g(vds).2);
                assert!(
                    (fdd - gds).abs() <= 1e-4 * gds.abs().max(1e-6 * beta),
                    "gds != d id/d vds at vov={v}: fd={fdd:e} gds={gds:e}"
                );
            }
        }
    }

    /// Level 1 exactly in triode with both channel ends 2·n·Vt past threshold
    /// and in saturation well beyond pinch-off, an n·Vt subthreshold slope
    /// with the `1 - e^(-vds/(n·Vt))` drain dependence, zero current across a
    /// zero-bias channel, and never more than the documented
    /// `0.081·beta·(2·n·Vt)²` from Level 1 anywhere.
    #[test]
    fn mos_channel_regions_physically_sane() {
        let nvt = hauksbee_ir::thermal_voltage_c(27.0);
        let (beta, w) = (2e-5, 2.0 * nvt);
        let f = |vov: f64, vds: f64| mos_channel(beta, nvt, vov, vds, 0.0, 0.0).0;
        let level1 = |vov: f64, vds: f64| {
            let (a, b) = (vov.max(0.0), (vov - vds).max(0.0));
            0.5 * beta * (a - b) * (a + b)
        };
        let slope = f(nvt - 0.2, 2.0) / f(-0.2, 2.0) - std::f64::consts::E;
        let tail = f(-0.2, nvt) / f(-0.2, 2.0) + (-1.0f64).exp_m1();
        assert!(slope.abs().max(tail.abs()) < 1e-9, "{slope:e} {tail:e}");
        for (vov, vds) in [(0.5, 2.0), (w, 2.0), (0.5, 0.1), (3.0, 1.0)] {
            let err = f(vov, vds) / level1(vov, vds) - 1.0;
            assert!(err.abs() <= 1e-12, "{err:e} at {vov},{vds}");
        }
        for i in -30..=50 {
            for j in 0..=50 {
                let (vov, vds) = (f64::from(i) * 0.01, f64::from(j) * 0.02);
                let dev = (f(vov, vds) - level1(vov, vds)).abs();
                assert!(dev <= 0.081 * beta * w * w, "{dev:e} at {vov},{vds}");
            }
        }
        assert!(f(-0.1, 0.0) == 0.0);
        assert!(f(0.5, 0.0) == 0.0);
    }

    /// Forward-difference partials match analytic derivatives (exact on the
    /// linear term, within the reltol-driven truncation on the curved one),
    /// and NaN/INF/eval faults come back as Err with `vals` restored.
    #[test]
    #[allow(clippy::float_cmp)] // exact: the fault path restores the stored value
    fn fd_partials_match_analytic_and_report_faults() {
        let expr =
            CompiledExpr::compile("2.0*__d0 + 100.0*math::tanh(5.0*__d1) + 0.1*time").unwrap();
        let deps = [BDep::Volt(NodeId(1)), BDep::Branch(DeviceId(0))];
        let opts = SolverOptions::default();
        for &(v, i, t) in &[
            (0.0, 0.0, 0.0),
            (1.0, 0.05, 1e-3),
            (-3.0, -0.2, 0.5),
            (0.25, 0.4, 2.0),
            (10.0, -0.02, 10.0),
        ] {
            let mut vals = vec![v, i];
            let (f0, partials) =
                behavioral_eval_partials(&expr, &deps, &mut vals, t, &opts).unwrap();
            let f_true = 0.1f64.mul_add(t, 2.0f64.mul_add(v, 100.0 * (5.0 * i).tanh()));
            assert!((f0 - f_true).abs() < 1e-12 * f_true.abs().max(1.0));
            assert!(
                ((partials[0] - 2.0) / 2.0).abs() < 1e-9,
                "df/dv at ({v},{i},{t}): {}",
                partials[0]
            );
            let di_true = 500.0 / (5.0f64 * i).cosh().powi(2);
            let di_err = ((partials[1] - di_true) / di_true.abs().max(1e-12)).abs();
            assert!(
                di_err < 5e-3,
                "df/di at ({v},{i},{t}): fd={} analytic={di_true}",
                partials[1]
            );
        }

        let deps = [BDep::Volt(NodeId(1))];
        let ln = CompiledExpr::compile("math::ln(__d0)").unwrap();
        assert!(behavioral_eval_partials(&ln, &deps, &mut [-1.0], 0.0, &opts).is_err());
        let div = CompiledExpr::compile("1.0/__d0").unwrap();
        assert!(behavioral_eval_partials(&div, &deps, &mut [0.0], 0.0, &opts).is_err());
        let flip = CompiledExpr::compile("math::ln(0.0 - __d0)").unwrap();
        let mut vals = vec![-1e-15];
        assert!(behavioral_eval_partials(&flip, &deps, &mut vals, 0.0, &opts).is_err());
        assert_eq!(vals[0], -1e-15, "base value restored on the fault path");
    }

    /// Variable-step BDF2: bit-identical to the fixed-step constants on a
    /// uniform grid, exact on quadratics for any step ratio (where uniform
    /// coefficients are not), and second-order on an alternating grid.
    #[test]
    #[allow(clippy::while_float)] // marches t to the end time on a float grid
    fn gear2_variable_step_coefficients() {
        for &dt in &[
            1e-12,
            1e-9,
            3.7e-8,
            1e-6,
            1.0 / 3.0,
            0.1,
            1.0,
            7.3,
            1e3,
            1e9,
        ] {
            let c = IntegCoeffs::for_step(Integration::Gear2, dt, dt, false);
            assert_eq!(c.g.to_bits(), (1.5 / dt).to_bits(), "g at dt={dt}");
            assert_eq!(c.a1.to_bits(), (2.0 / dt).to_bits(), "a1 at dt={dt}");
            assert_eq!(c.a2.to_bits(), (0.5 / dt).to_bits(), "a2 at dt={dt}");
            let c0 = IntegCoeffs::for_step(Integration::Gear2, dt, 0.0, false);
            assert_eq!(
                (c0.g.to_bits(), c0.a1.to_bits(), c0.a2.to_bits()),
                (c.g.to_bits(), c.a1.to_bits(), c.a2.to_bits())
            );
        }

        let q = |t: f64| (5.0 * t).mul_add(t, 3.0f64.mul_add(t, 2.0));
        let dq = |t: f64| 10.0f64.mul_add(t, 3.0);
        for &(h, r) in &[
            (1e-3, 0.25),
            (1e-3, 0.5),
            (1e-3, 2.0),
            (1e-3, 4.0),
            (0.1, 1.7),
            (1e-6, 0.33),
        ] {
            let h_prev = h / r;
            let tn = 0.4_f64;
            let (t1, t2) = (tn - h, tn - h - h_prev);
            let c = IntegCoeffs::for_step(Integration::Gear2, h, h_prev, false);
            let got = c.g.mul_add(q(tn), -c.a1.mul_add(q(t1), -(c.a2 * q(t2))));
            assert!(
                (got - dq(tn)).abs() <= 1e-9 * dq(tn).abs().max(1.0),
                "h={h}, r={r}: got {got}, want {}",
                dq(tn)
            );
            let cu = IntegCoeffs::for_step(Integration::Gear2, h, h, false);
            let bad = cu.g.mul_add(q(tn), -cu.a1.mul_add(q(t1), -(cu.a2 * q(t2))));
            assert!(
                (bad - dq(tn)).abs() > 1e-6 * dq(tn).abs(),
                "uniform stencil unexpectedly exact at r={r}"
            );
        }

        // q' = -q on the grid h, h/2, h, h/2, ...: halving h must cut the error ~4x.
        let march = |base_h: f64| -> f64 {
            let mut t = base_h * 0.5;
            let mut q2 = 1.0_f64;
            let mut q1 = (-(base_h * 0.5)).exp();
            let mut h_prev = base_h * 0.5;
            let mut long = true;
            let mut worst = 0.0_f64;
            while t < 1.0 - 1e-12 {
                let h = if long { base_h } else { base_h * 0.5 }.min(1.0 - t);
                let c = IntegCoeffs::for_step(Integration::Gear2, h, h_prev, false);
                let qn = c.a1.mul_add(q1, -(c.a2 * q2)) / (c.g + 1.0);
                t += h;
                worst = worst.max((qn - (-t).exp()).abs());
                q2 = q1;
                q1 = qn;
                h_prev = h;
                long = !long;
            }
            worst
        };
        let (e1, e2, e3) = (march(0.02), march(0.01), march(0.005));
        assert!(
            e1 / e2 > 3.4 && e2 / e3 > 3.4,
            "not ~h^2: {e1:.3e}/{e2:.3e}/{e3:.3e}"
        );
    }

    /// The latch's inertial delay: a decision holds the output for half
    /// the delay, then slews it across the swing in one delay, so it passes
    /// mid-swing one delay after the decision; a decision reverting inside
    /// the hold never reaches the output.
    #[test]
    fn comparator_latch_follows_a_decision_one_delay_late() {
        let near = |a: f64, b: f64| (a - b).abs() < 1e-12;
        let tpd = 10e-9;
        let low = super::ComparatorLatch::settled(false, 0.0);
        assert_eq!(low.next_corner(tpd), None, "settled at its rail");
        // Input 1 V over a 0.2 V band: decides high.
        let (high, flipped) = low.advanced(tpd, 1e-9, 1.0, 0.2);
        assert!(flipped && high.high);
        assert!(near(high.age, 0.0));
        assert!(near(high.frac_after(tpd, 5e-9), 0.0), "held for tpd / 2");
        assert!(
            (high.frac_after(tpd, 10e-9) - 0.5).abs() < 1e-12,
            "mid-swing at tpd"
        );
        assert!(
            near(high.frac_after(tpd, 15e-9), 1.0),
            "at the rail tpd / 2 later"
        );
        assert!(near(high.frac_after(tpd, 30e-9), 1.0));
        assert!(
            (high.next_corner(tpd).unwrap() - 5e-9).abs() < 1e-18,
            "the hold ends"
        );
        // Inside the band the decision stands.
        let (held, flipped) = high.advanced(tpd, 2e-9, 0.05, 0.2);
        assert!(!flipped && held.high && held.frac == 0.0);
        // Back under the low trip inside the hold: the pulse is swallowed.
        let (swallowed, flipped) = held.advanced(tpd, 2e-9, -1.0, 0.2);
        assert!(flipped && !swallowed.high && swallowed.age.is_infinite());
        assert!(
            near(swallowed.frac_after(tpd, 1e-6), 0.0),
            "the output never moved"
        );
        // A decision older than the hold keeps going once reverted: the
        // output turns back from where it got to.
        let (moving, _) = high.advanced(tpd, 8e-9, 1.0, 0.2);
        assert!((moving.frac - 0.3).abs() < 1e-12);
        let (back, flipped) = moving.advanced(tpd, 1e-9, -1.0, 0.2);
        assert!(flipped && !back.high && back.age == 0.0);
        assert!(
            (back.frac - 0.4).abs() < 1e-12,
            "still slewing up through its hold"
        );
        // Up to 0.9 by the hold's end, then down at the same rate.
        assert!((back.frac_after(tpd, 10e-9) - 0.4).abs() < 1e-12);
        assert!(near(back.frac_after(tpd, 20e-9), 0.0));
        // Without a stated delay the output steps at the decision.
        let (step, _) = low.advanced(0.0, 1e-9, 1.0, 0.2);
        assert!(near(step.frac_after(0.0, 1e-12), 1.0));
        assert_eq!(step.next_corner(0.0), None);
    }
}
