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
//! Long-form how-and-why (motivation, theory, rejected alternatives, the
//! buried bodies): docs/how-and-why/hauksbee-solve/stamp.md

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
}

impl IntegCoeffs {
    /// Coefficients for the chosen rule at step size `dt`. `first_step` forces
    /// backward Euler where a multi-step rule lacks history.
    pub fn for_step(method: Integration, dt: f64, first_step: bool) -> IntegCoeffs {
        match method {
            Integration::BackwardEuler => IntegCoeffs {
                g: 1.0 / dt,
                a1: 1.0 / dt,
                a2: 0.0,
            },
            Integration::Trapezoidal if !first_step => IntegCoeffs {
                g: 2.0 / dt,
                a1: 2.0 / dt,
                a2: 0.0,
            },
            Integration::Trapezoidal => IntegCoeffs {
                // First step: backward Euler to start cleanly.
                g: 1.0 / dt,
                a1: 1.0 / dt,
                a2: 0.0,
            },
            Integration::Gear2 if !first_step => IntegCoeffs {
                // BDF2 (uniform step): (3 x_n - 4 x_{n-1} + x_{n-2}) / (2 dt).
                g: 1.5 / dt,
                a1: 2.0 / dt,
                a2: 0.5 / dt,
            },
            Integration::Gear2 => IntegCoeffs {
                g: 1.0 / dt,
                a1: 1.0 / dt,
                a2: 0.0,
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
    /// supplied only by the staged-DC event-driven outer loop. The 4320
    /// SN74LVC1G3157 switches that fuse the Tarski synapse mesh each carry a
    /// tanh-blended conductance whose control node sits near the transition for
    /// a coupled core; left self-deciding, the inner Newton limit-cycles as the
    /// switch flips on/off every iteration. When present, each switch's
    /// conductance is PINNED to its frozen rail (full `ron` or `roff`, no
    /// control dependence) so the inner circuit is smooth and Newton converges;
    /// the outer Gauss-Seidel loop re-derives every switch's rail from the
    /// converged control voltages and re-solves until none flip. `None` (every
    /// normal solve) keeps the smooth tanh + control tangent bit-identical.
    pub switch_freeze: Option<&'a std::collections::HashMap<DeviceId, bool>>,
    /// SPDT leg sibling map (device id -> complementary throw's device id) for
    /// the smooth break-before-make coupling. Consulted only when
    /// `effects.spdt_bbm` (the device-model default); empty/ignored otherwise.
    pub spdt_sibling: &'a std::collections::HashMap<DeviceId, DeviceId>,
}

impl StampCtx<'_> {
    #[inline]
    fn v(&self, node: NodeId) -> f64 {
        match self.layout.node(node) {
            Some(i) => self.x[i],
            None => 0.0,
        }
    }
    /// Node voltage at the PREVIOUS Newton iterate (the limiting anchor).
    #[inline]
    fn v_prev(&self, node: NodeId) -> f64 {
        match self.layout.node(node) {
            Some(i) => self.x_prev[i],
            None => 0.0,
        }
    }
}

/// Stamp a conductance `g` between two nodes into the matrix.
#[inline]
fn stamp_cond<S: StampSink>(sink: &mut S, layout: &Layout, a: NodeId, b: NodeId, g: f64) {
    let ai = layout.node(a);
    let bi = layout.node(b);
    if let Some(ai) = ai {
        sink.g(ai, ai, g);
    }
    if let Some(bi) = bi {
        sink.g(bi, bi, g);
    }
    if let (Some(ai), Some(bi)) = (ai, bi) {
        sink.g(ai, bi, -g);
        sink.g(bi, ai, -g);
    }
}

/// Stamp an equivalent current source pushing `i` from `a` to `b`.
#[inline]
fn stamp_current<S: StampSink>(sink: &mut S, layout: &Layout, a: NodeId, b: NodeId, i: f64) {
    if let Some(ai) = layout.node(a) {
        sink.i(ai, -i);
    }
    if let Some(bi) = layout.node(b) {
        sink.i(bi, i);
    }
}


/// Where device stamps write. Two implementations: the real `(matrix, rhs)`
/// assembly, and the matrix-free residual accumulator (assembly-economy
/// sub-lever A). Generic and monomorphized, so the default assembly path
/// compiles to exactly the writes it always did (its bit-exactness contract
/// is pinned by the fixture waveform hash and the solve suites).
pub(crate) trait StampSink {
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

/// Matrix-free residual accumulation: `F = g*x - rhs` over the NODE block,
/// folded directly as each device stamps (`F[r] += v * x[c]` for a matrix
/// write, `F[r] -= v` for an rhs write; rows outside the node block are
/// dropped, they are not part of the residual norm). This skips the
/// clear/slot-search/store cost of a real assembly AND the separate row-product
/// pass, which is what makes the line-search residual eval cheap. The per-row
/// SUM ORDER differs from the assembled row product (device-stamp order,
/// interleaved with rhs terms, instead of slot order then rhs), so the norm can
/// differ from the assembled one by rounding: switching the residual eval
/// onto this sink was tried (assembly-economy sub-lever A), measured 2x on
/// the eval and -21% on the smoke march wall, and REVERTED: the flagship
/// joint march died where the assembled eval survived. The sink's F is
/// proven equal to the assembled row product within accumulation rounding
/// (`residual_sink_matches_assembled_row_product`), so the death was not a
/// bug but marginal Armijo decisions flipping under equally-valid rounding
/// on cancellation-heavy stiff rows: too fragile to ship. Kept (with the
/// bench and the equivalence gate) as the measurement estate and for any
/// future order-robust redesign.
#[cfg_attr(not(test), allow(dead_code))]
struct ResidualSink<'a> {
    f: &'a mut [f64],
    x: &'a [f64],
    n_nodes: usize,
}

impl StampSink for ResidualSink<'_> {
    #[inline]
    fn g(&mut self, row: usize, col: usize, v: f64) {
        if row < self.n_nodes {
            self.f[row] += v * self.x[col];
        }
    }
    #[inline]
    fn i(&mut self, row: usize, v: f64) {
        if row < self.n_nodes {
            self.f[row] -= v;
        }
    }
}

/// Reserve the structural slots a device will ever touch, so the symbolic
/// factorization sees the complete pattern even when a value is momentarily 0.
pub fn reserve_pattern(circuit: &Circuit, layout: &Layout, m: &mut SparseMatrix) {
    let touch = |m: &mut SparseMatrix, a: Option<usize>, b: Option<usize>| {
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
        // A series-resistance BJT's internal unknowns (dev-plan 04 §3.2) join
        // the all-pairs set: the ohmic stamps couple external<->internal and
        // the relocated Gummel-Poon core couples the internals among
        // themselves — the union's all-pairs covers both (and the toggle-off
        // unit diagonal is inside it too).
        if let Some(ints) = layout.bjt_internal(id) {
            ns.extend(ints.iter().copied());
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
        // reserve slots in the CONTROL source's branch COLUMN — a column
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
                    let br = layout
                        .branch(id)
                        .expect("ccvs owns a branch unknown");
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
}

pub fn stamp_all(ctx: &StampCtx, g: &mut SparseMatrix, rhs: &mut [f64]) {
    let mut sink = MatrixSink { g, rhs };
    stamp_into(ctx, &mut sink);
}

/// Accumulate the nonlinear residual `F = g*x - rhs` over the NODE block at
/// the iterate `ctx.x`, without assembling the matrix (see [`ResidualSink`]
/// for what that changes). `f[0..n_nodes]` must be zeroed by the caller.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn stamp_residual(ctx: &StampCtx, f: &mut [f64]) {
    let mut sink = ResidualSink {
        f,
        x: ctx.x,
        n_nodes: ctx.layout.n_nodes,
    };
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
pub(crate) fn stamp_device<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    dev: &Device,
    sink: &mut S,
) {
    match dev {
        Device::Resistor {
            a, b, ohms, tc1, ..
        } => {
            // A non-positive resistance is a SHORT, not an open: skipping the
            // stamp would leave the nodes coupled only through gmin (an open
            // with a ~1e12 Ω leak), silently breaking a 0-Ω jumper's net.
            // Clamp to the same 1e-6 Ω floor as the SPICE loader and the
            // engine's board binder (`bind_passive`) so a stiff conductance
            // couples the nodes. Also covers a tc1 derating driving r below 0.
            let r = resistor_value(*ohms, *tc1, ctx).max(1e-6);
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
        Device::Mosfet { d, g: gate, s, b, model, .. } => {
            stamp_mosfet(ctx, id, *d, *gate, *s, *b, model, sink)
        }
        Device::VSwitch { a, b, ctrl_p, ctrl_n, von, voff, ron, roff, .. } => stamp_vswitch(
            ctx, id, *a, *b, *ctrl_p, *ctrl_n, *von, *voff, *ron, *roff, sink,
        ),
        Device::OpAmp {
            out,
            inp,
            inn,
            reference,
            gain,
            rail_lo,
            rail_hi,
            ..
        } => stamp_opamp(
            ctx, *out, *inp, *inn, *reference, *gain, *rail_lo, *rail_hi, sink,
        ),
        Device::Comparator { out, inp, inn, out_lo, out_hi, hysteresis, .. } => stamp_comparator(
            ctx, id, *out, *inp, *inn, *out_lo, *out_hi, *hysteresis, sink,
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
            p, n, ctrl_src, gain, ..
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
            p, n, ctrl_src, transres, ..
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
        // not an element. Its physics lives in the WINDINGS' stamps —
        // `stamp_inductor` consults `layout.mutual_partners` (built from the
        // Coupling devices at layout time) for the −M·coeffs.g cross terms
        // and each winding's mutual history voltage. An arm that stamped the
        // cross terms HERE as well would double-count them, so the one home
        // is the inductor stamp and this arm is deliberately inert.
        Device::Coupling { .. } => {}
    }
}

// --- behavioral B-source (nonlinear expression device, dev-plan 04 §2.5) -----

thread_local! {
    /// First behavioral-expression fault of the current assembly (device-named,
    /// human-readable). The stamp interface is infallible by design (a sink of
    /// accumulations), so an expression that errors or produces a non-finite
    /// value at some Newton iterate cannot return an error — instead the
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

pub(crate) fn note_behavioral_fault(name: &str, detail: String) {
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
pub(crate) fn take_behavioral_fault() -> Option<String> {
    BEHAVIORAL_FAULT.with(|f| f.borrow_mut().take())
}

/// Evaluate a behavioral expression and its finite-difference partials at the
/// dependency values `vals` (slot-aligned with `deps`). Returns
/// `(f, partials)` or a human-readable fault description.
///
/// FD scheme (plan §2.5, shipped default; symbolic d/dv is the later
/// upgrade): forward difference per slot with step
/// `delta_k = reltol*|x_k| + floor`, where the floor is the per-quantity
/// convergence floor — `vntol` for a `V(...)` slot, `abstol` for an
/// `I(...)` slot — so the perturbation is always resolvable at the same
/// scale the Newton convergence test cares about. Any non-finite value
/// (IEEE semantics: `1/0 = inf`, `ln(-1) = NaN`, see `CompiledExpr::eval`)
/// or structural eval error is a fault: partial derivatives from a poisoned
/// point must never enter the matrix.
pub(crate) fn behavioral_eval_partials(
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
        let delta = opts.reltol * x0.abs() + floor;
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
///   `v_p - v_n - sum_k g_k*x_k = f0 - sum_k g_k*x_k0` — `-g_k` at
///   `(branch_row, dep_col)`, the constant on the branch RHS (a VCVS with
///   numeric gains plus a Newton-companion value).
///
/// A dependency column is a node unknown for `V(...)` slots (ground = no
/// column) or the control source's branch unknown for `I(...)` slots. Like
/// every dependent source, the value is a device property: never scaled by
/// source-stepping homotopy (`src_scale`), matching SPICE. The same stamp
/// serves DC (time = 0) and transient — the constraint has no memory. Sense
/// contract: dependency-node ROWS receive nothing (only their columns are
/// referenced), which the conduction cross-check enforces mechanically.
///
/// On an expression fault the device stamps NOTHING and notes a device-named
/// fault for the drivers (see [`BEHAVIORAL_FAULT`]) — never a NaN into the
/// matrix.
#[allow(clippy::too_many_arguments)]
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
    let (f0, partials) = match behavioral_eval_partials(expr, deps, &mut vals, ctx.time, ctx.opts)
    {
        Ok(r) => r,
        Err(detail) => {
            note_behavioral_fault(name, detail);
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
            if let Some(pi) = ctx.layout.node(p) {
                sink.g(pi, br, 1.0);
                sink.g(br, pi, 1.0);
            }
            if let Some(ni) = ctx.layout.node(n) {
                sink.g(ni, br, -1.0);
                sink.g(br, ni, -1.0);
            }
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
            ohms * (1.0 + tc * (ctx.opts.model_temp() - 27.0))
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

#[allow(clippy::too_many_arguments)]
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
            geq * v_prev + c * dv_prev
        }
        Integration::Gear2 => {
            let v_prev2 = ctx.state.x2[id.0 as usize];
            c * (ctx.coeffs.a1 * v_prev - ctx.coeffs.a2 * v_prev2)
        }
        Integration::BackwardEuler => c * ctx.coeffs.a1 * v_prev,
    };
    stamp_cond(sink, ctx.layout, a, b, geq);
    // Equivalent current ieq flows like the capacitor current a->b.
    stamp_current(sink, ctx.layout, a, b, -ieq);
}

#[allow(clippy::too_many_arguments)]
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
        Integration::Trapezoidal => req * i_prev + l * di_prev,
        Integration::Gear2 => {
            let i_prev2 = ctx.state.x2[id.0 as usize];
            l * (ctx.coeffs.a1 * i_prev - ctx.coeffs.a2 * i_prev2)
        }
        Integration::BackwardEuler => l * ctx.coeffs.a1 * i_prev,
    };
    // Branch row: v_a - v_b - req*i = -(req*i_prev + v_prev) = -veq.
    sink.g(br, br, -req);
    sink.i(br, -(veq));

    // Mutual inductance (dev-plan 04 §2.3): with couplings the branch relation
    // generalizes verbatim from `v = L·di/dt` to `v_j = Σ_k L_jk·di_k/dt`
    // (L_jj the self term already stamped above, L_jk = M = k·sqrt(Lj·Lk)).
    // Per partner k this adds one matrix cross term −M·coeffs.g at
    // (this branch row, partner branch column) — L stamped DIRECTLY, never
    // inverted, so a k=1 group's singular inductance matrix is harmless (the
    // node-incidence terms keep the MNA system regular) — plus the partner's
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
            Integration::Trapezoidal => {
                m * ctx.coeffs.g * ctx.state.x1[pid.0 as usize]
                    + m * ctx.state.dx1[pid.0 as usize]
            }
            Integration::Gear2 => {
                m * (ctx.coeffs.a1 * ctx.state.x1[pid.0 as usize]
                    - ctx.coeffs.a2 * ctx.state.x2[pid.0 as usize])
            }
            Integration::BackwardEuler => m * ctx.coeffs.a1 * ctx.state.x1[pid.0 as usize],
        };
        sink.i(br, -veq_m);
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
    let pi = ctx.layout.node(p);
    let ni = ctx.layout.node(n);
    if let Some(pi) = pi {
        sink.g(pi, br, 1.0);
        sink.g(br, pi, 1.0);
    }
    if let Some(ni) = ni {
        sink.g(ni, br, -1.0);
        sink.g(br, ni, -1.0);
    }
    let val = source_value(ctx, kind) * ctx.src_scale;
    sink.i(br, val);
}

/// VCVS: the branch-current unknown pattern of an ideal Vsource, with the
/// constraint row `v_p - v_n - gain*(v_cp - v_cn) = 0` in place of a fixed
/// value. The branch incidence (`±1` in the p/n KCL rows and the branch row)
/// is identical to `stamp_vsource`; the control terms land in the branch ROW
/// only (columns cp/cn), so the control pair's own rows stay untouched — that
/// is the sense-terminal contract `Device::sense_nodes` declares. The RHS is
/// zero (nothing time-varying, nothing to src_scale: the gain is a device
/// property, not an independent source value). The same stamp serves DC and
/// transient — the constraint has no memory.
#[allow(clippy::too_many_arguments)]
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
    if let Some(pi) = ctx.layout.node(p) {
        sink.g(pi, br, 1.0);
        sink.g(br, pi, 1.0);
    }
    if let Some(ni) = ctx.layout.node(n) {
        sink.g(ni, br, -1.0);
        sink.g(br, ni, -1.0);
    }
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
/// CONTROL source's branch COLUMN — reading another device's unknown, never
/// touching the control loop's node rows (the control coupling is declared via
/// `Device::controlling_source`, not `sense_nodes`, precisely because it is a
/// branch-current read). RHS is zero (the transresistance is a device
/// property, not an independent source value — no src_scale). Same stamp for
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
    if let Some(pi) = ctx.layout.node(p) {
        sink.g(pi, br, 1.0);
        sink.g(br, pi, 1.0);
    }
    if let Some(ni) = ctx.layout.node(n) {
        sink.g(ni, br, -1.0);
        sink.g(br, ni, -1.0);
    }
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
                vold + vt * arg.ln()
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
///   `i = -Is·exp(-(bv+vd)/(n·Vt))` — ngspice's `-IBV·exp(-(BV+v)/VT)` shape
///   with `Is` standing in for `IBV`. The SIGN is the point: the current is
///   REVERSE (negative, cathode->anode) and grows exponentially as `vd` drops
///   below `-bv`. Using `Is` as the scale makes the current CONTINUOUS at
///   `vd == -bv` (both branches give `-Is` there); decks that must match an
///   ngspice `IBV` reconcile by setting `IBV=IS` on the ngspice side. The
///   exponent is clamped like the BJT's (`exp(40)·Is` bounds the current to a
///   few kA at typical `Is`), and `stamp_diode` additionally junction-limits
///   the per-iteration move in MIRRORED coordinates (see there), so a Newton
///   iterate far past breakdown cannot poison the solve. `bv` defaults to
///   infinity, which makes the breakdown branch unreachable: existing decks
///   are bit-identical.
pub(crate) fn diode_eval(model: &DiodeModel, vd: f64, t_c: f64, temp_on: bool) -> (f64, f64) {
    let is = if temp_on { model.is_at(t_c) } else { model.is };
    let nvt = model.n * hauksbee_ir_thermal(t_c, temp_on);
    if vd >= -3.0 * nvt {
        let e = (vd / nvt).exp();
        let id = is * (e - 1.0);
        let gd = is * e / nvt;
        (id, gd)
    } else if !model.bv.is_finite() || vd >= -model.bv {
        // Reverse region: tiny linear leakage, conductance ~ gmin handled outside.
        let id = -is;
        let gd = is / nvt * 1e-3;
        (id, gd)
    } else {
        // Reverse breakdown: exponentially growing REVERSE current.
        let e = (-(model.bv + vd) / nvt).min(40.0).exp();
        let id = -is * e;
        let gd = is * e / nvt;
        (id, gd)
    }
}

/// SPICE's standard depletion-capacitance knee: below `FC·vj` the physical
/// `Cj(v) = cjo (1 - v/vj)^-m` applies; above it the singular expression is
/// replaced by its tangent-matched linear continuation.
pub(crate) const DIODE_FC: f64 = 0.5;

/// Whether a diode carries charge under the current effects toggles: the
/// `junction_caps` gate AND at least one charge-producing model field. A
/// default model (`cjo == 0`, `tt == 0`) never stores charge, so decks whose
/// diode models predate this physics are bit-identical whatever the toggle.
#[inline]
pub(crate) fn diode_has_charge(
    model: &DiodeModel,
    effects: &crate::options::DeviceEffects,
) -> bool {
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
pub(crate) fn diode_charge(model: &DiodeModel, vd: f64, id: f64, gd: f64) -> (f64, f64) {
    let mut q = 0.0;
    let mut c = 0.0;
    if model.cjo > 0.0 {
        let (qj, cj) = depletion_charge(model.cjo, model.vj, model.m, vd);
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
/// junction voltage `vd`, with the FC-knee linear continuation documented on
/// [`diode_charge`]. Shared by the diode (`cjo/vj/m` from its model card) and
/// the BJT's two junctions (`cje`/`cjc` with the SPICE default `vj`/`m`, see
/// [`BJT_VJ`]/[`BJT_MJ`]); the arithmetic is the diode's original,
/// expression-for-expression, so extracting it moved no diode bit.
pub(crate) fn depletion_charge(cjo: f64, vj: f64, m: f64, vd: f64) -> (f64, f64) {
    let fcv = DIODE_FC * vj;
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
        let f2 = (1.0 - DIODE_FC).powf(1.0 + m);
        let f3 = 1.0 - DIODE_FC * (1.0 + m);
        let f1 = if (1.0 - m).abs() < 1e-9 {
            -vj * (1.0 - DIODE_FC).ln()
        } else {
            vj * (1.0 - (1.0 - DIODE_FC).powf(1.0 - m)) / (1.0 - m)
        };
        let c = cjo * (f3 + m * vd / vj) / f2;
        let q = cjo * (f1 + (f3 * (vd - fcv) + (m / (2.0 * vj)) * (vd * vd - fcv * fcv)) / f2);
        (q, c)
    }
}

/// SPICE default junction built-in potential / grading coefficient for the
/// BJT's depletion capacitances (`VJE`/`VJC` = 0.75 V, `MJE`/`MJC` = 0.33).
/// The loader does not parse per-junction `vj`/`m` overrides yet, so both
/// junctions use the ngspice defaults — the value an ngspice model card
/// without VJE/MJE also gets, which keeps the differential decks honest.
pub(crate) const BJT_VJ: f64 = 0.75;
pub(crate) const BJT_MJ: f64 = 0.33;

/// Whether a BJT stores charge under the current effects toggles: the
/// `junction_caps` gate AND at least one charge-producing model field. A
/// default model (`cje == cjc == tf == tr == 0`) never stores charge, so
/// decks whose BJT models predate this physics are bit-identical whatever
/// the toggle — the same contract [`diode_has_charge`] pins for diodes.
#[inline]
pub(crate) fn bjt_has_charge(
    model: &BjtModel,
    effects: &crate::options::DeviceEffects,
) -> bool {
    effects.junction_caps
        && (model.cje > 0.0 || model.cjc > 0.0 || model.tf > 0.0 || model.tr > 0.0)
}

/// BJT base-emitter stored charge `Q(vbe)` and capacitance `dQ/dvbe`, in
/// POLARITY-FOLDED (NPN-reference) space: depletion (`cje`, SPICE-default
/// knee) plus forward diffusion `tf·i_cc`, where `i_cc = cf` is the forward
/// transport current the Gummel-Poon core already evaluates (`gif` its
/// tangent) — the transit-time charge rides the core's own transport
/// evaluation instead of recomputing it.
pub(crate) fn bjt_charge_be(model: &BjtModel, vbe: f64, cf: f64, gif: f64) -> (f64, f64) {
    let mut q = 0.0;
    let mut c = 0.0;
    if model.cje > 0.0 {
        let (qj, cj) = depletion_charge(model.cje, BJT_VJ, BJT_MJ, vbe);
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
pub(crate) fn bjt_charge_bc(model: &BjtModel, vbc: f64, cr: f64, gir: f64) -> (f64, f64) {
    let mut q = 0.0;
    let mut c = 0.0;
    if model.cjc > 0.0 {
        let (qj, cj) = depletion_charge(model.cjc, BJT_VJ, BJT_MJ, vbc);
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
/// the stamp uses — the seed/advance/LTE entry point (the stamp itself reuses
/// its already-evaluated `cf`/`gif`/`cr`/`gir` via the per-junction helpers).
pub(crate) fn bjt_charges_at(
    model: &BjtModel,
    vbe: f64,
    vbc: f64,
    t_c: f64,
    temp_on: bool,
) -> (f64, f64) {
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
/// stamped — the voltages its stored charges are functions of. This is the
/// single node-resolution rule the transient/partitioned seed-advance arms
/// and the AC stamp share with `stamp_bjt`: internal unknowns when the
/// `series_resistance` toggle is on AND the layout allocated them, the
/// external nodes otherwise.
#[allow(clippy::too_many_arguments)]
pub(crate) fn bjt_junction_voltages(
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
    let v = |i: Option<usize>| i.map(|i| x[i]).unwrap_or(0.0);
    let sign = model.polarity.sign();
    (sign * (v(bi) - v(ei)), sign * (v(bi) - v(ci)))
}

/// The unknown indices the Gummel-Poon core (and the charges) live on:
/// `[c, b, e]` as internal-node indices where the layout allocated them and
/// the `series_resistance` toggle honors them, external node indices
/// otherwise (per terminal — a zero-valued resistance allocated no internal
/// node and stays external).
pub(crate) fn bjt_effective_nodes(
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
    match layout.bjt_internal(id) {
        Some(ints) => [ints[0].or(ext[0]), ints[1].or(ext[1]), ints[2].or(ext[2])],
        None => ext,
    }
}

// --- MOSFET charge helpers (dev-plan 04 §3.3) --------------------------------

/// Meyer-limit intrinsic gate-drain capacitance fraction: `Cgd -> (1/2)·Cox`
/// in deep triode (Meyer's `vds -> 0` limit); zero in saturation/cutoff where
/// only the overlap remains — the Miller-charge asymmetry that sets switching
/// timing.
pub(crate) const MOS_CGD_MEYER: f64 = 0.5;

/// Whether a MOSFET stores charge under the current effects toggles: the
/// `junction_caps` gate AND at least one charge-producing model field (gate
/// overlap/oxide caps or bulk depletion caps). A default model never stores
/// charge, so decks whose MOS models predate this physics are bit-identical
/// whatever the toggle — the [`diode_has_charge`]/[`bjt_has_charge`] contract.
#[inline]
pub(crate) fn mos_has_charge(
    model: &MosfetModel,
    effects: &crate::options::DeviceEffects,
) -> bool {
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

/// Level-1 channel current and tangents `(id, gm, gds)` at FOLDED, post-swap
/// voltages (`vov = vgs - vth`, `vds >= 0`), shared by the transient stamp and
/// the AC tangent so the two linearizations CANNOT drift apart.
///
/// The subthreshold blend is the `softplus` overdrive (the gate-charge region
/// switch's discipline, applied to the channel itself):
///
///   vov_eff = 2·n·Vt · softplus(vov / (2·n·Vt))
///
/// fed into the plain Shichman-Hodges triode/saturation expressions. Above
/// threshold `vov_eff -> vov` (the square law, exponentially fast); below,
/// `vov_eff -> 2·n·Vt·e^(vov/(2·n·Vt))`, so the saturation current tends to
/// `2·beta·(n·Vt)²·e^(vov/(n·Vt))` — an exponential subthreshold tail with
/// slope n·Vt, which is what the old two-branch form MEANT its
/// "continuity scale" to buy. That form matched neither value nor slope at
/// `vgs == vth` (the exponential branch carried `beta·(n·Vt)²·e` where the
/// square law carries 0), so a gate sweep saw `id` drop by the full
/// `i0` and `gm` collapse to 0 crossing threshold — a Newton limit-cycle trap
/// and a lie against the model's "smooth subthreshold tail" doc. The blended
/// overdrive is C-infinity in `vgs`, so `id` and `gm` are continuous
/// everywhere by construction (`gm` picks up the `sigmoid` chain factor
/// d vov_eff/d vgs). Channel-length modulation multiplies every region
/// including the tail — the (1 + lambda·vds) factor would otherwise jump at
/// the region switch.
#[inline]
pub(crate) fn mos_channel(
    beta: f64,
    nvt: f64,
    vov: f64,
    vds: f64,
    lambda: f64,
    gmin: f64,
) -> (f64, f64, f64) {
    let two_nvt = 2.0 * nvt;
    let u = vov / two_nvt;
    let vov_eff = two_nvt * softplus(u);
    let dvov = sigmoid(u); // d vov_eff / d vgs, the C1 chain factor
    let clm = 1.0 + lambda * vds;
    if vds < vov_eff {
        // Triode (vov_eff > 0 always, so vds -> 0 lands here: id -> 0, no
        // phantom subthreshold current across a zero-bias channel).
        let id = beta * (vov_eff * vds - 0.5 * vds * vds) * clm;
        let gm = beta * vds * clm * dvov;
        let gds =
            beta * ((vov_eff - vds) * clm + (vov_eff * vds - 0.5 * vds * vds) * lambda);
        (id, gm.max(gmin), gds.max(gmin))
    } else {
        // Saturation; the subthreshold exponential is this branch's
        // small-vov_eff limit, not a separate region.
        let id = 0.5 * beta * vov_eff * vov_eff * clm;
        let gm = beta * vov_eff * clm * dvov;
        let gds = 0.5 * beta * vov_eff * vov_eff * lambda;
        (id, gm.max(gmin), gds.max(gmin))
    }
}

/// THE CHARGE-MODEL CHOICE (dev-plan 04 §3.3, decided here): the gate
/// charges are the "simpler-but-honest" per-junction alternative to true
/// Meyer capacitances, for a structural reason, not just a stability one.
/// Meyer's `Cgs(vgs, vds)`/`Cgd(vgs, vds)` depend on TWO junction voltages,
/// so they do not integrate to a per-junction two-terminal `Q(v)` at all
/// (the classic Meyer charge-non-conservation problem); the solver's
/// reactive machinery is charge-based `Q(v)` companions (§3.1) precisely so
/// nonlinear capacitance conserves charge. Instead each gate junction gets a
/// genuine `Q(v)` whose capacitance matches Meyer's REGION LIMITS, smoothly
/// interpolated over a `delta`-wide transition at the threshold (`softplus`
/// makes Q C-infinity-smooth, so Newton sees continuous C and dC/dv through
/// every switching edge — the FC-knee discipline applied to the region
/// switch):
///
/// * [`mos_charge_gs`]: `C = c_ov + c_ox` BELOW threshold falling to
///   `c_ov + (1/2)·c_ox` above. The below-threshold `c_ox` is Meyer's
///   CUTOFF GATE-BULK capacitance (`Cgb = Cox`) REFERENCED TO THE SOURCE:
///   in the target case (discrete MOSFET, bulk tied to source) that is
///   electrically exact, and dropping it instead (the first cut of this
///   model) made every gate charge ~`Cox` too fast below threshold — the
///   switching edges led ngspice by the whole subthreshold gate-RC
///   (measured on the §3.3 load-switch decks: worst pointwise errors 5.45
///   NMOS / 54.5 PMOS without it). The ON value is Meyer's DEEP-TRIODE
///   limit `Cox/2`, not the saturation `2/3·Cox`: a switch dwells in
///   triode whenever it is on and only transits saturation briefly during
///   the drain swing, and the choice is cross-check-driven — with `2/3`
///   the load-switch turn-off edge lagged ngspice by 5-8 ns (gate
///   discharging through an over-stated triode Cgs; worst pointwise error
///   0.76), with `1/2` the same edge aligns to 0.8 ns (worst 0.32,
///   mid-edge).
/// * [`mos_charge_gd`]: `C = c_ov` in cutoff/saturation rising to
///   `c_ov + (1/2)·c_ox` in triode (`vgd` crossing the threshold is the
///   drain end of the channel forming) — the Miller-charge asymmetry that
///   sets switching timing.
///
/// What is given up vs Meyer is the region interpolation only a
/// two-voltage C can express (Meyer's Cgs rises to `2/3·Cox` in
/// saturation; ours holds the triode `Cox/2` there), bounded and visible
/// as nanosecond-scale edge skew in the ngspice cross-check tolerances of
/// the §3.3 decks.
#[inline]
pub(crate) fn mos_charge_gs(c_ov: f64, c_ox: f64, vth: f64, delta: f64, v: f64) -> (f64, f64) {
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
pub(crate) fn mos_charge_gd(c_ov: f64, c_ox: f64, vth: f64, delta: f64, v: f64) -> (f64, f64) {
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
/// junction voltages — the seed/advance/LTE entry point, computing exactly
/// what the stamp's companions integrate. Gate charges are the
/// [`mos_gate_charge`] shape at the zero-bias threshold `vto` (the charge
/// region switch deliberately ignores the body effect: the common
/// bulk-tied-to-source switch has none, and a body-biased charge switch
/// would couple three terminals into what must stay a two-terminal `Q(v)`);
/// bulk charges are the §3.1 depletion helper with the model's `pb`/`mj`.
pub(crate) fn mos_charges_at(
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
    let q_bd = if model.cbd > 0.0 {
        depletion_charge(model.cbd, model.pb, model.mj, vbd).0
    } else {
        0.0
    };
    let q_bs = if model.cbs > 0.0 {
        depletion_charge(model.cbs, model.pb, model.mj, vbs).0
    } else {
        0.0
    };
    (q_gs, q_gd, q_bd, q_bs)
}

/// FOLDED junction voltages `(vgs, vgd, vbd, vbs)` of a MOSFET at the
/// solution vector `x`, at the PHYSICAL terminals (no drain/source symmetry
/// swap: the swap is a channel-evaluation device; the charges live on the
/// real gate/bulk junctions). Bulk defaults to source when absent, exactly
/// as the stamp resolves it.
pub(crate) fn mos_junction_voltages(
    layout: &Layout,
    x: &[f64],
    d: NodeId,
    g: NodeId,
    s: NodeId,
    b: Option<NodeId>,
    model: &MosfetModel,
) -> (f64, f64, f64, f64) {
    let v = |n: NodeId| layout.node(n).map(|i| x[i]).unwrap_or(0.0);
    let sign = model.polarity.sign();
    let bulk = b.unwrap_or(s);
    (
        sign * (v(g) - v(s)),
        sign * (v(g) - v(d)),
        sign * (v(bulk) - v(d)),
        sign * (v(bulk) - v(s)),
    )
}

/// §3.4 (`DeviceEffects` contract): a toggle the stamp cannot honor for a
/// device must LOG ONCE rather than silently ignore the model fields. Each
/// dishonored (effect, device) pair owns one flag; tests read the flag.
pub(crate) mod effect_log {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Diode `rs` is parsed but not stamped. The BJT's rb/re/rc landed the
    /// internal-node machinery (dev-plan 04 §3.2), but wiring the diode's RS
    /// through it is deliberately deferred: the diode's §3.1 numbers are a
    /// regression surface this arc must not move (its decks omit RS for
    /// exactly this reason).
    pub static DIODE_SERIES_R: AtomicBool = AtomicBool::new(false);

    /// Log `msg` the first time this flag fires; a no-op afterwards.
    ///
    /// This is an engine-internal dev note (it references a dev-plan section and
    /// an unimplemented effect), so it goes through the `HAUKSBEE_DEBUG`-gated
    /// debug channel rather than straight to stderr: on a user's CI run it was
    /// leaking `[effects] ... (dev-plan 04 §3.2)` into the output. The flag still
    /// flips exactly once (tests read it), the print is just now channel-gated.
    pub fn log_once(flag: &AtomicBool, msg: &str) {
        if !flag.swap(true, Ordering::Relaxed) {
            hauksbee_ir::debug::note("effects", msg);
        }
    }
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

    // §3.4: series resistance is promised by the toggle but not stamped yet
    // (series-node insertion lands with the BJT's rb/re/rc, dev-plan 04 §3.2).
    if ctx.opts.effects.series_resistance && model.rs > 0.0 {
        effect_log::log_once(
            &effect_log::DIODE_SERIES_R,
            "series_resistance: diode RS is not stamped yet (dev-plan 04 §3.2); \
             the model field is ignored",
        );
    }

    let vd_raw = ctx.v(a) - ctx.v(k);
    let vd_last = ctx.last_vd(a, k);
    // Junction limiting. The forward exponential is limited by pnjlim toward
    // vcrit; the breakdown exponential is the same curve MIRRORED about
    // vd = -bv, so in the breakdown region (vd below -bv, with a 10·nvt skirt,
    // ngspice's region test) the per-iteration move is limited in mirrored
    // coordinates vdtemp = -(vd + bv) — pnjlim toward the same vcrit — and
    // mapped back. Without this, one Newton iterate landing volts past -bv
    // evaluates exp() at full depth and poisons the step exactly the way the
    // unlimited forward branch used to. `bv == INFINITY` (the default) makes
    // the region test unsatisfiable, keeping every existing deck on the
    // byte-for-byte identical pnjlim path.
    let mut vd = if model.bv.is_finite() && vd_raw < (10.0 * nvt - model.bv).min(0.0) {
        -(pnjlim(-(vd_raw + model.bv), -(vd_last + model.bv), nvt, vc) + model.bv)
    } else {
        pnjlim(vd_raw, vd_last, nvt, vc)
    };

    // STAGED-DC diode pre-limit (the relaxed-seed overflow cure). On the staged
    // path only (branch_reg > 0), HARD-CLAMP the junction voltage so the very
    // FIRST stamp from the relaxed (diodes-off) seed cannot overflow exp(). The
    // relaxed seed leaves some stretcher-diode cathodes floating; their first
    // forward bias can be tens of volts, and on iteration 1 pnjlim sees a zero
    // delta (anchor == iterate) so it does not limit — diode_eval then evaluates
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

    let (idc, gd_raw) = diode_eval(model, vd, t_c, temp_on);
    let gd = gd_raw.max(ctx.opts.gmin);
    // Newton equivalent current: ieq = id - gd*vd.
    let ieq = idc - gd * vd;
    stamp_cond(sink, ctx.layout, a, k, gd);
    stamp_current(sink, ctx.layout, a, k, ieq);

    // Charge storage (dev-plan 04 §3.1): junction (depletion) + diffusion
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
    // this rides the identical integration machinery — the linear capacitor
    // IS this formula specialized to Q = C·v. Newton then linearizes Q(v) at
    // the iterate with dQ/dv = C(vd):
    //   i_lin(v) = geq·v + (i_hat(vd) - geq·vd),  geq = coeffs.g·C(vd).
    let sl = id.0 as usize;
    let q_prev = ctx.state.x1[sl];
    let ieq_hist = match ctx.opts.integration {
        Integration::Trapezoidal => ctx.coeffs.g * q_prev + ctx.state.dx1[sl],
        Integration::Gear2 => ctx.coeffs.a1 * q_prev - ctx.coeffs.a2 * ctx.state.x2[sl],
        Integration::BackwardEuler => ctx.coeffs.a1 * q_prev,
    };
    let geq = ctx.coeffs.g * c;
    let ieq_q = (ctx.coeffs.g * q - ieq_hist) - geq * vd;
    stamp_cond(sink, ctx.layout, a, k, geq);
    stamp_current(sink, ctx.layout, a, k, ieq_q);
}

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

    // Series resistances rb/re/rc (dev-plan 04 §3.2): the layout allocated an
    // internal unknown per NONZERO terminal resistance (see
    // `Layout::bjt_internal` — zero-valued resistances allocate nothing, so a
    // default model takes none of these branches). With the toggle on, stamp
    // the ohmic conductance between each external terminal and its internal
    // node and move the Gummel-Poon core (and the charges below) onto the
    // internal nodes. With the toggle off, the core stays on the external
    // nodes — exactly the no-series-R physics — and each allocated-but-unused
    // internal unknown is pinned to 0 by a unit diagonal (an isolated row
    // coupled to nothing, so it cannot influence the solution).
    let ext = [ctx.layout.node(c), ctx.layout.node(b), ctx.layout.node(e)];
    let mut eff = ext;
    if let Some(&ints) = ctx.layout.bjt_internal(id) {
        if ctx.opts.effects.series_resistance {
            let rs = [model.rc, model.rb, model.re];
            for t in 0..3 {
                if let Some(int_i) = ints[t] {
                    add_pair(sink, ext[t], Some(int_i), 1.0 / rs[t]);
                    eff[t] = Some(int_i);
                }
            }
        } else {
            for int_i in ints.into_iter().flatten() {
                sink.g(int_i, int_i, 1.0);
            }
        }
    }
    let (ci, bi, ei) = (eff[0], eff[1], eff[2]);
    let vx = |i: Option<usize>| i.map(|i| ctx.x[i]).unwrap_or(0.0);
    let vx_prev = |i: Option<usize>| i.map(|i| ctx.x_prev[i]).unwrap_or(0.0);

    // Intrinsic junction voltages with polarity folded in.
    let vbe = sign * (vx(bi) - vx(ei));
    let vbc = sign * (vx(bi) - vx(ci));
    // Separate critical voltages per junction (SPICE3's VCRITF/VCRITR):
    // pnjlim's gate (`vnew > vcrit`) and its `arg <= 0` fallback (return
    // vcrit) are only self-consistent when vcrit is built from the SAME n·Vt
    // the call limits with — feeding the vbc call the nf-scaled vcrit made
    // the reverse limiter fire at the wrong threshold whenever nf != nr.
    // Iteration-path only; the converged root is unchanged.
    let vcrit_be = vcrit(is, model.nf * vt);
    let vcrit_bc = vcrit(is, model.nr * vt);
    let vbe = pnjlim(vbe, sign * (vx_prev(bi) - vx_prev(ei)), model.nf * vt, vcrit_be);
    let vbc = pnjlim(vbc, sign * (vx_prev(bi) - vx_prev(ci)), model.nr * vt, vcrit_bc);

    let nvf = model.nf * vt;
    let nvr = model.nr * vt;

    // Forward / reverse transport currents (clamped exponent for safety).
    let ef = (vbe / nvf).clamp(-40.0, 40.0).exp();
    let er = (vbc / nvr).clamp(-40.0, 40.0).exp();
    let cf = is * (ef - 1.0); // forward diffusion
    let cr = is * (er - 1.0); // reverse diffusion

    // Base-width modulation (Early effect): SGP's base-charge factor with
    // IKF absent is qb = q1 = 1/(1 - vbc/VAF - vbe/VAR), and the transport
    // current is DIVIDED by qb — i.e. MULTIPLIED by (1 - vbc/VAF - vbe/VAR),
    // which is > 1 in forward-active (vbc < 0), so ic grows with vce and
    // ro = VAF/IC, the classic Early slope; the -vbe/VAR term is the REVERSE
    // Early voltage, shrinking ic as the forward junction charges the base
    // (it dominates in saturation/reverse-active, where VAR-carrying models
    // used to mis-simulate silently: the field was parsed and dropped here).
    // (This arc FIXED an inversion here: the previous code divided by
    // (1 - vbc/VAF), shrinking ic with vce — undetected because the bias
    // decks' collector points are bias-network-set, exposed by the §3.2
    // amplifier-gain cross-check against ngspice. For the default model both
    // `vaf` and `var` are infinite, `early == 1.0`, and `1/1.0 == 1.0`
    // exactly: default-model decks are bit-identical across both fixes.)
    let early = if ctx.opts.effects.early_effect {
        let mut e = 1.0;
        if model.vaf.is_finite() {
            e -= vbc / model.vaf;
        }
        if model.var.is_finite() {
            e -= vbe / model.var;
        }
        e
    } else {
        1.0
    };
    let q1_inv = early.max(0.1); // = 1/qb (SGP q1 reciprocal), clamped far from use

    // Transport (collector) current and the two base components.
    let ict = (cf - cr) * q1_inv;
    let ibe = cf / model.bf;
    let ibc = cr / model.br;

    // Conductances (derivatives wrt the junction voltages).
    let gpi = is * ef / (nvf * model.bf); // d ibe / d vbe
    let gmu = is * er / (nvr * model.br); // d ibc / d vbc
    let gif = is * ef / nvf; // d cf / d vbe
    let gir = is * er / nvr; // d cr / d vbc
    let gm = (gif * q1_inv).max(ctx.opts.gmin); // forward transconductance
    let go = (gir * q1_inv).max(ctx.opts.gmin); // reverse / output

    // Terminal currents into c, b, e (NPN reference, then sign-folded).
    let ic = ict - ibc;
    let ib = ibe + ibc;
    let ie = -(ic + ib);

    // Linearized stamp, two variants sharing the residual physics:
    //
    // LEGACY (external nodes — every pre-§3.2 deck): conductances among (b,e)
    // and (b,c) plus the transconductance gm coupling c to vbe, with vce as
    // the output coordinate. This Jacobian is APPROXIMATE in saturation
    // (mapping ∂ic/∂vbc through go-between-c-e plus gm-on-vbe mismatches the
    // true partial by gir), which board-level resistances damp fine — and it
    // is the byte-for-byte path the fixture hash pins, so it stays.
    //
    // EXACT (core relocated onto internal nodes — the §3.2 opt-in honored):
    // the true partials
    //   ∂ic/∂vbe = gif·q1_inv
    //   ∂ic/∂vbc = -gir·q1_inv - gmu - (cf-cr)/vaf  (Early term only when the
    //              effect is active and the clamp is not)
    //   ∂ib/∂vbe = gpi,  ∂ib/∂vbc = gmu
    // stamped directly per junction voltage. The legacy approximation
    // limit-cycles Newton on a hard-saturated switching edge (measured: a
    // persistent ~0.1 V two-cycle at the collector, failing the step at
    // dt_min); the exact tangent converges it. It applies exactly when the
    // core sits on the internal nodes: every pre-§3.2 deck (and any toggle-
    // off run) stays on the pinned legacy bytes, and the staged-DC relaxed
    // pass (which solves with `series_resistance` OFF precisely to get the
    // easier base-topology system) stays legacy too. Both Jacobians define
    // the same root — the RHS residual brackets are the terminal currents
    // either way.
    //
    // (ci, bi, ei) are the EFFECTIVE unknowns resolved above — the intrinsic
    // internal nodes when series resistance is stamped, the externals
    // otherwise. `sign` folds out of the matrix entries (sign² = 1) and folds
    // the RHS brackets whole; folding only some terms breaks PNP convergence.
    let exact_tangent = eff != ext;
    if exact_tangent {
        // The base-charge factor's own partials (both zero unless the effect
        // is on, the voltage is finite, and the clamp is not engaged):
        // ∂q1_inv/∂vbc = -1/vaf, ∂q1_inv/∂vbe = -1/var, each contributing
        // (cf-cr)·∂q1_inv to the transport-current row.
        let early_free = ctx.opts.effects.early_effect && early > 0.1;
        let early_deriv = if early_free && model.vaf.is_finite() {
            -(cf - cr) / model.vaf
        } else {
            0.0
        };
        let var_deriv = if early_free && model.var.is_finite() {
            -(cf - cr) / model.var
        } else {
            0.0
        };
        // gif·q1_inv (gmin-floored like the legacy path) plus the VAR term.
        let gc_be = gm + var_deriv;
        let gc_bc = -gir * q1_inv - gmu + early_deriv;
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
    } else {
        // gpi between b-e, gmu between b-c.
        add_pair(sink, bi, ei, gpi);
        add_pair(sink, bi, ci, gmu);
        // output conductance go between c-e.
        add_pair(sink, ci, ei, go);
        // transconductance: ic depends on vbe = v(b)-v(e).
        add_transconductance(sink, ci, ei, bi, ei, gm);

        // Equivalent currents: residual = I_terminal - linearized part, both
        // in folded (NPN-reference) space, then mapped to real space by
        // `sign` (matrix rows in real space equal sign * folded linear part).
        let vce_f = sign * (vx(ci) - vx(ei));
        let ic_eq = sign * (ic - (gm * vbe + go * vce_f - gmu * vbc));
        let ib_eq = sign * (ib - (gpi * vbe + gmu * vbc));
        let ie_eq = sign * (ie + (gm + gpi) * vbe + go * vce_f);

        inject(sink, ci, -ic_eq);
        inject(sink, bi, -ib_eq);
        inject(sink, ei, -ie_eq);
    }

    // Charge storage (dev-plan 04 §3.2): base-emitter and base-collector
    // charges as CHARGE-BASED companions, exactly the diode's §3.1 companion
    // applied per junction — bank A of the device's `ReactiveState` slots
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
    // (resp. b->c) in NPN-reference space; `sign` maps it to real space —
    // the conductance stamp is polarity-free (sign² = 1) while the RHS
    // residual folds, mirroring the DC bracket above.
    let hist = |x1: f64, dx1: f64, x2: f64| match ctx.opts.integration {
        Integration::Trapezoidal => ctx.coeffs.g * x1 + dx1,
        Integration::Gear2 => ctx.coeffs.a1 * x1 - ctx.coeffs.a2 * x2,
        Integration::BackwardEuler => ctx.coeffs.a1 * x1,
    };
    let geq_be = ctx.coeffs.g * c_be;
    let ieq_be = (ctx.coeffs.g * q_be - hist(ctx.state.x1[sl], ctx.state.dx1[sl], ctx.state.x2[sl]))
        - geq_be * vbe;
    add_pair(sink, bi, ei, geq_be);
    inject(sink, bi, -sign * ieq_be);
    inject(sink, ei, sign * ieq_be);

    let geq_bc = ctx.coeffs.g * c_bc;
    let ieq_bc = (ctx.coeffs.g * q_bc
        - hist(ctx.state.xb[0].x1[sl], ctx.state.xb[0].dx1[sl], ctx.state.xb[0].x2[sl]))
        - geq_bc * vbc;
    add_pair(sink, bi, ci, geq_bc);
    inject(sink, bi, -sign * ieq_bc);
    inject(sink, ci, sign * ieq_bc);
}

#[allow(clippy::too_many_arguments)]
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

    // Fold polarity: work in N-channel space.
    let mut vd = sign * ctx.v(d);
    let vg = sign * ctx.v(gnode);
    let mut vs = sign * ctx.v(s);
    // Ensure drain is the higher terminal (symmetry handled by swap).
    let swap = vd < vs;
    if swap {
        std::mem::swap(&mut vd, &mut vs);
    }
    let vgs = vg - vs;
    let vds = vd - vs;

    // Body-effect threshold shift (dev-plan 04 §3.3): `gamma` was parsed and
    // silently ignored before this arc. With a bulk terminal present and
    // `gamma > 0`, `vth = vto + gamma·(sqrt(phi - vbs) - sqrt(phi))` at the
    // folded bulk-to-EFFECTIVE-source voltage (the post-swap source — ngspice
    // measures the body effect from whichever terminal acts as the source).
    // `gamma == 0` (every pre-§3.3 model, every db entry) takes the plain
    // `vto` path bit-identically and never reads the bulk voltage.
    let bulk = b.unwrap_or(s);
    let mut vth = model.vto;
    // d vth / d vbs, for the gmb transconductance below (0 unless body effect).
    let mut dvth_dvbs = 0.0;
    let mut vbs_f = 0.0;
    let body_effect = model.gamma > 0.0 && b.is_some();
    if body_effect {
        let vb_f = sign * ctx.v(bulk);
        vbs_f = vb_f - vs;
        let phi = model.phi.max(1e-6);
        let arg = phi - vbs_f;
        if arg > 0.0 {
            let sq = arg.sqrt();
            vth = model.vto + model.gamma * (sq - phi.sqrt());
            // Guard the sqrt singularity at vbs -> phi: cap the derivative at
            // the value it has 1 mV short of the pole (vth itself stays exact).
            dvth_dvbs = -model.gamma / (2.0 * sq.max(1e-3 * phi.sqrt()));
        } else {
            // Forward-biased bulk clamped at phi: threshold floor, no slope.
            vth = model.vto + model.gamma * (0.0 - phi.sqrt());
        }
    }

    let nsub = model.n_sub.max(1.0);
    // Channel evaluation via the shared C1-continuous blend (see
    // [`mos_channel`]) — the AC tangent calls the SAME function at the OP.
    let lambda = if ctx.opts.effects.early_effect {
        model.lambda
    } else {
        0.0
    };
    let (ids, gm, gds) = mos_channel(beta, nsub * vt, vgs - vth, vds, lambda, ctx.opts.gmin);

    // Map back through the swap and polarity. The drain current flows d->s in
    // N-channel space; after a swap it flows the other way.
    let (di, gi, si) = (
        ctx.layout.node(d),
        ctx.layout.node(gnode),
        ctx.layout.node(s),
    );
    let (dn, sn) = if swap { (si, di) } else { (di, si) };

    // Conductances: gds between drain-source, gm couples drain current to vgs.
    add_pair(sink, dn, sn, gds);
    add_transconductance(sink, dn, sn, gi, sn, gm);

    // Equivalent current: id_eq = ids - gm*vgs - gds*vds, flowing d->s.
    let mut ieq = ids - gm * vgs - gds * vds;
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
    // below — the diode's §3.1 companion verbatim (slots hold CHARGE in
    // x1/x2, dQ/dt in dx1; the discrete current is coeffs.g·Q(v) - hist).
    let hist = |x1: f64, dx1: f64, x2: f64| match ctx.opts.integration {
        Integration::Trapezoidal => ctx.coeffs.g * x1 + dx1,
        Integration::Gear2 => ctx.coeffs.a1 * x1 - ctx.coeffs.a2 * x2,
        Integration::BackwardEuler => ctx.coeffs.a1 * x1,
    };
    let sl = id.0 as usize;

    // Body diode (dev-plan 04 §3.3): STRUCTURAL bulk-junction physics, the
    // BJT-junction discipline — the DC branches exist whenever the model
    // carries them (`body_is > 0`), un-toggled; the depletion charges ride
    // `junction_caps`. Each junction is bulk->drain / bulk->source in folded
    // space (the bulk is the anode of an N-channel body diode; `sign` folds
    // PMOS). A junction whose bulk IS its terminal (the discrete
    // bulk-tied-to-source case) is a short: skipped, it can carry no state.
    // This is the reverse-conduction path of every synchronous-rectifier /
    // flyback deck; `body_is` defaults to 0 (BIT-IDENTITY deviation from
    // ngspice's 1e-14 default, documented on the model field).
    if model.has_body_diode() {
        let bulk_i = ctx.layout.node(bulk);
        // (terminal, its unknown, depletion cap, secondary bank index)
        let junctions = [(d, di, model.cbd, 1usize), (s, si, model.cbs, 2usize)];
        for (term, term_i, cbx, bank) in junctions {
            if term == bulk {
                continue;
            }
            let v_raw = sign * (ctx.v(bulk) - ctx.v(term));
            // DC junction: Shockley branch with pn-junction limiting, exactly
            // the diode's convergence discipline.
            let vj = if model.body_is > 0.0 {
                let v_prev = sign * (ctx.v_prev(bulk) - ctx.v_prev(term));
                let vcr = vcrit(model.body_is, vt);
                let vj = pnjlim(v_raw, v_prev, vt, vcr);
                let e = (vj / vt).clamp(-40.0, 40.0).exp();
                let ij = model.body_is * (e - 1.0);
                let gj = (model.body_is * e / vt).max(ctx.opts.gmin);
                add_pair(sink, bulk_i, term_i, gj);
                let ieq_j = ij - gj * vj;
                inject(sink, bulk_i, -sign * ieq_j);
                inject(sink, term_i, sign * ieq_j);
                vj
            } else {
                v_raw
            };
            // Depletion charge companion (open at DC like every capacitor).
            if !ctx.dc && ctx.opts.effects.junction_caps && cbx > 0.0 {
                let (q, c) = depletion_charge(cbx, model.pb, model.mj, vj);
                let bk = &ctx.state.xb[bank];
                let geq = ctx.coeffs.g * c;
                let ieq_c =
                    (ctx.coeffs.g * q - hist(bk.x1[sl], bk.dx1[sl], bk.x2[sl])) - geq * vj;
                add_pair(sink, bulk_i, term_i, geq);
                inject(sink, bulk_i, -sign * ieq_c);
                inject(sink, term_i, sign * ieq_c);
            }
        }
    }

    // Gate charges (dev-plan 04 §3.3, gated by `junction_caps` — "the single
    // biggest reason a MOS switching deck disagrees with ngspice"): Q_gs on
    // bank A, Q_gd on secondary bank 0, each a two-terminal charge companion
    // at the PHYSICAL (unswapped) terminals — the charge sits on the real
    // gate junction whichever way the channel evaluation swapped d/s. The
    // charge-model choice (Meyer region limits on a smooth two-terminal
    // Q(v)) is documented on [`mos_gate_charge`].
    if !ctx.dc && ctx.opts.effects.junction_caps && model.has_gate_charge() {
        let delta = 2.0 * nsub * vt;
        let vgs_p = sign * (ctx.v(gnode) - ctx.v(s));
        let vgd_p = sign * (ctx.v(gnode) - ctx.v(d));
        let (q_gs, c_gs) = mos_charge_gs(model.cgs_ov, model.c_ox, model.vto, delta, vgs_p);
        let geq_gs = ctx.coeffs.g * c_gs;
        let ieq_gs = (ctx.coeffs.g * q_gs
            - hist(ctx.state.x1[sl], ctx.state.dx1[sl], ctx.state.x2[sl]))
            - geq_gs * vgs_p;
        add_pair(sink, gi, si, geq_gs);
        inject(sink, gi, -sign * ieq_gs);
        inject(sink, si, sign * ieq_gs);

        let (q_gd, c_gd) = mos_charge_gd(model.cgd_ov, model.c_ox, model.vto, delta, vgd_p);
        let bk = &ctx.state.xb[0];
        let geq_gd = ctx.coeffs.g * c_gd;
        let ieq_gd =
            (ctx.coeffs.g * q_gd - hist(bk.x1[sl], bk.dx1[sl], bk.x2[sl])) - geq_gd * vgd_p;
        add_pair(sink, gi, di, geq_gd);
        inject(sink, gi, -sign * ieq_gd);
        inject(sink, di, sign * ieq_gd);
    }
}

#[allow(clippy::too_many_arguments)]
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
    let vmid = 0.5 * (von + voff);
    let span = ((von - voff).abs()).max(1e-9);
    // CONTROL LIMITING (staged / dynamic path only, branch_reg > 0). The
    // log-interpolated conductance gsw = exp(lgoff + s*(lgon-lgoff)) spans many
    // decades (ron ~ ohms, roff ~ 1e9), so a single Newton iteration that moves
    // the control voltage across the transition makes gsw jump multiple decades
    // at once. On a switch carrying real current (e.g. a synapse spike-gate
    // whose control is a climbing neuron V_out) that snap destabilizes the
    // coupled solve and the per-step Newton fails right at the flip. Mirroring
    // pn-junction limiting (pnjlim) for diodes, bound the per-iteration change
    // in the tanh argument `u` to ~1 (about one transition-width / a few
    // decades of conductance) anchored on the previous iterate, so Newton tracks
    // the switch through its transition smoothly. branch_reg==0 (every normal
    // solve) keeps vctrl unlimited and the path bit-identical.
    let vctrl = if ctx.branch_reg > 0.0 {
        let vctrl_old = ctx.v_prev(cp) - ctx.v_prev(cn);
        let u_new = 3.0 * (vctrl_raw - vmid) / span;
        let u_old = 3.0 * (vctrl_old - vmid) / span;
        const MAX_DU: f64 = 1.0;
        let du = (u_new - u_old).clamp(-MAX_DU, MAX_DU);
        vmid + (u_old + du) * span / 3.0
    } else {
        vctrl_raw
    };
    // Smooth tanh transition between log-conductances. With u the tanh argument,
    //   s   = 0.5 * (1 + tanh(u)),      u = 3*(vctrl - vmid)/span
    //   gln = ln(goff) + s*(ln(gon) - ln(goff))
    //   gsw = exp(gln)
    let lgon = gon.ln();
    let lgoff = goff.ln();
    let u = 3.0 * (vctrl - vmid) / span;
    let th = u.tanh();
    let mut s = 0.5 * (1.0 + th);

    // BREAK-BEFORE-MAKE for SPDT legs (effects.spdt_bbm, the device-model
    // default; the typed compat field restores the old bridging model). A real
    // SN74LVC1G3157 SPDT is NEVER low-Z to both throws at
    // once: as the SELECT crosses its threshold the leaving throw opens before the
    // entering throw closes. The bare smooth-tanh model breaks this -- at the
    // SELECT transition MID-band BOTH complementary legs sit at the geometric-mean
    // conductance sqrt(ron*roff) (~70-200 kohm), so the common node `a` is
    // simultaneously low-Z to BOTH throws. In the Tarski synapse spike-gate the two
    // throws are the output membrane (s1) and a fixed rail (s0 = GND on the
    // excitatory leg, +5P on the inhibitory leg). Both legs half-on therefore wires
    // the rail straight onto the membrane through `a`, injecting a WEIGHT-INDEPENDENT
    // common-mode current (the +5P throw of the inhibitory gate pulls `a` to ~3 V
    // and the half-on s1 dumps ~20 uA into the membrane regardless of the latched
    // synapse weight) -- the over-firing pedestal. Enforcing break-before-make:
    // WINNER-TAKE-ALL between the two throws by their SELECT margin: the throw
    // whose control sits FURTHER past its own threshold is the selected one and
    // keeps (most of) its conductance; the LOSER is driven toward roff. With
    //   margin = (vctrl - vmid)/span        (>0: past the make threshold)
    //   s_eff  = s * sigmoid(K*(margin_self - margin_sib))
    // a cleanly selected throw (margin_self >> margin_sib) is unchanged, the
    // opposite throw collapses to roff, and AT the exact crossover both halve --
    // so the rail throw never bridges to the membrane throw, while the GENUINELY
    // selected throw (the spike-gate s1 once the climbing hidden V_out passes the
    // band centre) still conducts the weighted synapse current. This is the
    // analog realisation of the same on-margin tie-break the frozen event path
    // (`eval_switch_states`) uses, made smooth/differentiable for the per-step
    // Newton. K sets the transition sharpness (effects.spdt_bbm_k, default 6).
    let bbm = ctx.opts.effects.spdt_bbm;
    if bbm {
        if let Some(&sib) = ctx.spdt_sibling.get(&id) {
            if let Some(Device::VSwitch { ctrl_p: scp, ctrl_n: scn, von: svon, voff: svoff, .. }) =
                ctx.circuit.devices.get(sib.0 as usize)
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

    let gln = lgoff + s * (lgon - lgoff);
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
    //   ds/dvctrl       = 0.5 * (1 - tanh^2(u)) * (3/span)
    // and a matching equivalent-current correction so the linearization is
    // exact at the operating point (same root, faster convergence). Stamping
    // this makes each switch Newton-linearized instead of a Picard fixed point.
    // The control-node transconductance is a Newton TANGENT (it makes each switch
    // Newton-linearized rather than a Picard fixed point); it is not required for
    // correctness -- the same root is reached without it, just with more
    // iterations. For a torn feedforward column whose spike-gate control is a
    // HIGH-Z boundary node (the captured hidden V_out, driven through an external
    // RC, drawing zero conduction current), summing this term across the ~20
    // switch legs on that one control node with the large mirror voltage
    // `vab≈4.5 V` couples back into the iterative solution and throttles the
    // control's slew so the gate never closes (measured: V_out charges ~15x
    // slower than its R*C). effects.switch_ctrl_gm = false drops the
    // back-coupling so the control node is purely exogenous; the conductance
    // stamp + the switch's own a/b dynamics still track the flip. The default
    // (true) keeps the tangent, the classic behavior.
    let skip_ctrl_gm = !ctx.opts.effects.switch_ctrl_gm;
    let dgsw_dvctrl = gsw * (lgon - lgoff) * 0.5 * (1.0 - th * th) * (3.0 / span);
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

fn stamp_opamp<S: StampSink>(
    ctx: &StampCtx,
    out: NodeId,
    inp: NodeId,
    inn: NodeId,
    reference: Option<NodeId>,
    gain: f64,
    rail_lo: f64,
    rail_hi: f64,
    sink: &mut S,
) {
    // Behavioral: drive `out` toward clamp(gain*(vp-vn)) through a stiff
    // conductance, modeling an ideal output stage with finite open-loop gain.
    let gout = 1.0; // output stage conductance (1 ohm), strong source
    let vp = ctx.v(inp);
    let vn = ctx.v(inn);
    let vref = reference.map(|n| ctx.v(n)).unwrap_or(0.0);
    let target = (vref + gain * (vp - vn)).clamp(rail_lo, rail_hi);
    // Linearize: out = target + gain*(d vp - d vn) within rails.
    let in_rail = target > rail_lo && target < rail_hi;
    let oi = ctx.layout.node(out);
    if let Some(oi) = oi {
        sink.g(oi, oi, gout);
        sink.i(oi, gout * target);
        if in_rail {
            if let Some(ri) = reference.and_then(|n| ctx.layout.node(n)) {
                sink.g(oi, ri, -gout);
                sink.i(oi, -(gout * vref));
            }
            // Couple output to inputs through the gain (tangent).
            if let Some(pi) = ctx.layout.node(inp) {
                sink.g(oi, pi, -gout * gain);
                sink.i(oi, -(gout * gain * vp));
            }
            if let Some(ni) = ctx.layout.node(inn) {
                sink.g(oi, ni, gout * gain);
                sink.i(oi, gout * gain * vn);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stamp_comparator<S: StampSink>(
    ctx: &StampCtx,
    id: DeviceId,
    out: NodeId,
    inp: NodeId,
    inn: NodeId,
    out_lo: f64,
    out_hi: f64,
    hyst: f64,
    sink: &mut S,
) {
    let vp = ctx.v(inp);
    let vn = ctx.v(inn);
    let prev_out = ctx.v(out);
    let gout = 1.0;

    // Event-driven staged path: the outer loop supplies a FROZEN decision for
    // this comparator. Hold its output state fixed for the whole inner Newton
    // solve so it cannot chatter; the threshold is set by the frozen state's
    // hysteresis side. This is the limit-cycle cure: with every comparator's
    // state fixed, the inner circuit is smooth and Newton converges; the outer
    // loop re-evaluates and re-solves if any decision flipped.
    if let Some(freeze) = ctx.cmp_freeze {
        let _ = (vp, vn, hyst);
        let high = freeze.get(&id).copied().unwrap_or(prev_out > 0.5 * (out_lo + out_hi));
        // PIN the output to its frozen rail with a CONSTANT target (no input
        // dependence). A smooth input-dependent transfer still swings the output
        // whenever the comparator inputs pass near threshold, which makes the
        // output node chase the inputs and the inner Newton limit-cycle (the
        // output flip-flops at the step cap forever). Decoupling the output from
        // the inputs entirely — output := frozen rail — makes the inner circuit
        // strictly feed-FORWARD through the comparator, so Newton converges
        // cleanly. The outer event loop is what enforces input/output
        // consistency: it re-derives each comparator's rail from the converged
        // inputs and re-solves until no rail flips, so the fixed point is a true
        // root with every output consistent with its own inputs.
        let target = if high { out_hi } else { out_lo };
        if let Some(oi) = ctx.layout.node(out) {
            sink.g(oi, oi, gout);
            sink.i(oi, gout * target);
        }
        return;
    }

    // Hysteresis: threshold depends on current output state.
    let high = prev_out > 0.5 * (out_lo + out_hi);
    let thresh = if high { -hyst } else { hyst };

    // Staged-DC path (branch_reg > 0) without an event-freeze map: the bare
    // bang-bang transfer below has no tangent, so on a stiff board where a
    // comparator input sits near threshold the output chatters between rails
    // every Newton iteration. Replace it, for the staged solve only, with a
    // smooth high-gain logistic transfer that has a real derivative. A real
    // LMV7219 has finite (high) open-loop gain, so this is physically faithful;
    // the discrete model below is kept bit-identical for every normal solve
    // (branch_reg == 0 and no freeze map).
    if ctx.branch_reg > 0.0 {
        let span = out_hi - out_lo;
        let k = ctx.opts.effects.cmp_smooth_gain; // 1/V; ~2 mV transition width at the default
        let d = vp - vn - thresh;
        let kd = (k * d).clamp(-40.0, 40.0);
        let e = (-kd).exp();
        let sig = 1.0 / (1.0 + e); // logistic in (0,1)
        let target = out_lo + span * sig;
        let dsig = k * sig * (1.0 - sig);
        let dtarget_dvp = span * dsig; // d target / d vp  (= -d/d vn)
        if let Some(oi) = ctx.layout.node(out) {
            sink.g(oi, oi, gout);
            sink.i(oi, gout * target);
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

    let target = if vp - vn > thresh { out_hi } else { out_lo };
    if let Some(oi) = ctx.layout.node(out) {
        sink.g(oi, oi, gout);
        sink.i(oi, gout * target);
    }
}

// --- low-level stamp helpers ------------------------------------------------

#[inline]
fn add_pair<S: StampSink>(sink: &mut S, a: Option<usize>, b: Option<usize>, gval: f64) {
    if let Some(a) = a {
        sink.g(a, a, gval);
    }
    if let Some(b) = b {
        sink.g(b, b, gval);
    }
    if let (Some(a), Some(b)) = (a, b) {
        sink.g(a, b, -gval);
        sink.g(b, a, -gval);
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

// Junction-voltage memory for limiting is carried on the context via the
// previous Newton iterate; these helpers read it back.
impl StampCtx<'_> {
    fn last_vd(&self, a: NodeId, k: NodeId) -> f64 {
        self.v_prev(a) - self.v_prev(k)
    }
}


#[cfg(test)]
mod diode_physics_tests {
    use super::*;
    use crate::system::ReactiveState;
    use hauksbee_ir::{Circuit, Device};

    fn charge_model() -> DiodeModel {
        DiodeModel {
            cjo: 4e-12,
            vj: 0.7,
            m: 0.45,
            tt: 20e-9,
            ..DiodeModel::default()
        }
    }

    /// The FC-knee continuation must be C1 in the CAPACITANCE (C and dC/dv
    /// continuous), i.e. C2 in the charge — the property that keeps Newton
    /// from chattering when a switching edge crosses FC·vj.
    #[test]
    fn junction_cap_knee_is_c1_continuous() {
        let m = charge_model();
        let knee = DIODE_FC * m.vj;
        let eps = 1e-9;
        let eval = |vd: f64| {
            let (idc, gd) = diode_eval(&m, vd, 27.0, false);
            diode_charge(&m, vd, idc, gd)
        };
        let (q_lo, c_lo) = eval(knee - eps);
        let (q_hi, c_hi) = eval(knee + eps);
        // Q and C continuous across the knee.
        assert!((q_hi - q_lo).abs() < 1e-6 * q_lo.abs().max(1e-15), "Q jump at knee");
        assert!((c_hi - c_lo).abs() < 1e-6 * c_lo, "C jump at knee");
        // dC/dv continuous: one-sided slopes agree.
        let d = 1e-6;
        let slope_lo = (eval(knee - eps).1 - eval(knee - eps - d).1) / d;
        let slope_hi = (eval(knee + eps + d).1 - eval(knee + eps).1) / d;
        assert!(
            (slope_hi - slope_lo).abs() < 1e-3 * slope_lo.abs(),
            "dC/dv kink at knee: {slope_lo:e} vs {slope_hi:e}"
        );
    }

    /// Analytic sanity below the knee: C = cjo (1 - v/vj)^-m, and Q'(v) == C
    /// by finite difference (the charge really is the integral of the cap).
    #[test]
    fn junction_charge_is_integral_of_cap() {
        let m = charge_model();
        for &vd in &[-5.0, -1.0, -0.2, 0.0, 0.2, 0.34, 0.4, 0.6, 0.9] {
            let (idc, gd) = diode_eval(&m, vd, 27.0, false);
            let (_q, c) = diode_charge(&m, vd, idc, gd);
            let d = 1e-7;
            let q_of = |v: f64| {
                let (i2, g2) = diode_eval(&m, v, 27.0, false);
                diode_charge(&m, v, i2, g2).0
            };
            let dq = (q_of(vd + d) - q_of(vd - d)) / (2.0 * d);
            assert!(
                (dq - c).abs() < 1e-4 * c.abs().max(1e-18),
                "dQ/dv != C at vd={vd}: fd={dq:e} c={c:e}"
            );
        }
    }

    /// Breakdown branch: REVERSE (negative) current growing exponentially
    /// below -bv, continuous with the -Is leakage at -bv, and bounded by the
    /// exponent clamp far past breakdown.
    #[test]
    fn breakdown_current_is_reverse_continuous_and_bounded() {
        let m = DiodeModel { bv: 6.2, ..DiodeModel::default() };
        let nvt = m.n * hauksbee_ir::thermal_voltage_c(27.0);
        let (i_at, _) = diode_eval(&m, -6.2, 27.0, false);
        assert!((i_at + m.is).abs() < 1e-3 * m.is, "not continuous at -bv: {i_at:e}");
        let (i_past, g_past) = diode_eval(&m, -6.2 - 5.0 * nvt, 27.0, false);
        assert!(i_past < 0.0, "breakdown current must be REVERSE, got {i_past:e}");
        assert!(
            (i_past + m.is * 5.0f64.exp()).abs() < 1e-6 * m.is * 5.0f64.exp(),
            "wrong exponential shape: {i_past:e}"
        );
        assert!(g_past > 0.0);
        // Deep past breakdown: exponent clamped, current finite.
        let (i_deep, g_deep) = diode_eval(&m, -1e6, 27.0, false);
        assert!(i_deep.is_finite() && g_deep.is_finite());
        assert!((i_deep + m.is * 40.0f64.exp()).abs() < 1e-6 * m.is * 40.0f64.exp());
        // bv = INFINITY keeps the old reverse branch bit-identical.
        let m_inf = DiodeModel::default();
        let (i_rev, g_rev) = diode_eval(&m_inf, -100.0, 27.0, false);
        assert_eq!(i_rev, -m_inf.is);
        assert_eq!(g_rev, m_inf.is / nvt * 1e-3);
    }

    /// §3.4 contract, stamp level: flipping `junction_caps` on a model WITH
    /// cjo/tt changes the stamped system (charge terms appear); on a model
    /// WITHOUT charge fields the two stamps are bit-identical.
    #[test]
    fn junction_caps_toggle_changes_diode_stamp() {
        let stamp_with = |model: DiodeModel, junction_caps: bool| {
            let mut c = Circuit::new();
            let a = c.node("a");
            c.add(Device::Diode { name: "D1".into(), a, k: hauksbee_ir::NodeId::GROUND, model });
            let layout = Layout::new(&c);
            let mut m = SparseMatrix::new(layout.size);
            reserve_pattern(&c, &layout, &mut m);
            let x = vec![0.62];
            let mut state = ReactiveState::new(1);
            state.x1[0] = 1e-12; // nonzero history so RHS terms show too
            let mut opts = SolverOptions::default();
            opts.effects.junction_caps = junction_caps;
            let coeffs =
                IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-9, false);
            let spdt = std::collections::HashMap::new();
            let ctx = StampCtx {
                circuit: &c,
                layout: &layout,
                opts: &opts,
                x: &x,
                x_prev: &x,
                time: 0.0,
                coeffs,
                state: &state,
                dc: false,
                use_ic: false,
                gmin: 0.0,
                src_scale: 1.0,
                branch_reg: 0.0,
                cmp_freeze: None,
                switch_freeze: None,
                spdt_sibling: &spdt,
            };
            let mut rhs = vec![0.0; layout.size];
            let mut mat = m;
            stamp_all(&ctx, &mut mat, &mut rhs);
            let diag = mat.row(0).iter().find(|(col, _)| *col == 0).map(|&(_, v)| v).unwrap();
            (diag, rhs[0])
        };
        // Charge-carrying model: the toggle must CHANGE the stamp.
        let (g_on, r_on) = stamp_with(charge_model(), true);
        let (g_off, r_off) = stamp_with(charge_model(), false);
        assert!(g_on > g_off, "junction_caps on must add companion conductance");
        assert!(r_on != r_off, "junction_caps on must add history RHS terms");
        // Charge-free (default) model: bit-identical either way.
        let (g_on0, r_on0) = stamp_with(DiodeModel::default(), true);
        let (g_off0, r_off0) = stamp_with(DiodeModel::default(), false);
        assert_eq!(g_on0.to_bits(), g_off0.to_bits());
        assert_eq!(r_on0.to_bits(), r_off0.to_bits());
    }

    /// §3.4 contract: the one toggle the stamps still cannot honor (diode RS
    /// — deliberately deferred, see `effect_log::DIODE_SERIES_R`) LOGS ONCE
    /// instead of silently ignoring the parsed model field. The BJT flags
    /// this test used to read are GONE: cje/cjc/tf/tr and rb/re/rc are real
    /// stamps now (§3.2), asserted by the toggle tests below.
    #[test]
    fn dishonored_effects_log_once() {
        let mut c = Circuit::new();
        let a = c.node("a");
        c.add(Device::Diode {
            name: "D1".into(),
            a,
            k: hauksbee_ir::NodeId::GROUND,
            model: DiodeModel { rs: 0.5, ..DiodeModel::default() },
        });
        let layout = Layout::new(&c);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&c, &layout, &mut m);
        let x = vec![0.6];
        let state = ReactiveState::new(1);
        let opts = SolverOptions::default();
        let coeffs = IntegCoeffs::for_step(crate::options::Integration::BackwardEuler, 1e-9, true);
        let spdt = std::collections::HashMap::new();
        let ctx = StampCtx {
            circuit: &c,
            layout: &layout,
            opts: &opts,
            x: &x,
            x_prev: &x,
            time: 0.0,
            coeffs,
            state: &state,
            dc: false,
            use_ic: false,
            gmin: 0.0,
            src_scale: 1.0,
            branch_reg: 0.0,
            cmp_freeze: None,
            switch_freeze: None,
            spdt_sibling: &spdt,
        };
        let mut rhs = vec![0.0; layout.size];
        stamp_all(&ctx, &mut m, &mut rhs);
        use std::sync::atomic::Ordering;
        assert!(effect_log::DIODE_SERIES_R.load(Ordering::Relaxed));
    }
}

#[cfg(test)]
mod bjt_physics_tests {
    use super::*;
    use crate::system::ReactiveState;
    use hauksbee_ir::{Circuit, Device};

    fn charge_model() -> BjtModel {
        BjtModel {
            cje: 20e-12,
            cjc: 8e-12,
            tf: 400e-12,
            tr: 50e-9,
            ..BjtModel::default()
        }
    }

    /// Stamp a one-BJT circuit (b at 0.65 V, c at 3 V, e grounded) and return
    /// the base-row diagonal and base RHS entry.
    fn stamp_bjt_system(model: BjtModel, opts: SolverOptions) -> (Layout, SparseMatrix, Vec<f64>) {
        let mut cir = Circuit::new();
        let nb = cir.node("b");
        let nc = cir.node("c");
        cir.add(Device::Bjt {
            name: "Q1".into(),
            c: nc,
            b: nb,
            e: hauksbee_ir::NodeId::GROUND,
            model,
        });
        let layout = Layout::new(&cir);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&cir, &layout, &mut m);
        let mut x_full = vec![0.65; layout.size];
        if let Some(i) = layout.node(nc) {
            x_full[i] = 3.0;
        }
        let mut state = ReactiveState::new(1);
        state.x1[0] = 1e-12; // nonzero history so RHS terms show too
        state.xb[0].x1[0] = -2e-12;
        let coeffs = IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-9, false);
        let spdt = std::collections::HashMap::new();
        let ctx = StampCtx {
            circuit: &cir,
            layout: &layout,
            opts: &opts,
            x: &x_full,
            x_prev: &x_full,
            time: 0.0,
            coeffs,
            state: &state,
            dc: false,
            use_ic: false,
            gmin: 0.0,
            src_scale: 1.0,
            branch_reg: 0.0,
            cmp_freeze: None,
            switch_freeze: None,
            spdt_sibling: &spdt,
        };
        let mut rhs = vec![0.0; layout.size];
        stamp_all(&ctx, &mut m, &mut rhs);
        (layout, m, rhs)
    }

    /// Diagonal entry and RHS value at `row`.
    fn pair(m: &SparseMatrix, rhs: &[f64], row: usize) -> (f64, f64) {
        let d = m
            .row(row)
            .iter()
            .find(|(col, _)| *col == row)
            .map(|&(_, v)| v)
            .unwrap();
        (d, rhs[row])
    }

    /// §3.4 contract, stamp level: flipping `junction_caps` on a BJT model
    /// WITH cje/cjc/tf/tr changes the stamped system (two charge companions
    /// appear); on a default model the two stamps are bit-identical.
    #[test]
    fn junction_caps_toggle_changes_bjt_stamp() {
        let run = |model: BjtModel, junction_caps: bool| {
            let mut opts = SolverOptions::default();
            opts.effects.junction_caps = junction_caps;
            let (layout, m, rhs) = stamp_bjt_system(model, opts);
            let b_row = layout.node(hauksbee_ir::NodeId(1)).unwrap();
            pair(&m, &rhs, b_row)
        };
        let (g_on, r_on) = run(charge_model(), true);
        let (g_off, r_off) = run(charge_model(), false);
        assert!(g_on > g_off, "junction_caps on must add companion conductance");
        assert!(r_on != r_off, "junction_caps on must add history RHS terms");
        let (g_on0, r_on0) = run(BjtModel::default(), true);
        let (g_off0, r_off0) = run(BjtModel::default(), false);
        assert_eq!(g_on0.to_bits(), g_off0.to_bits());
        assert_eq!(r_on0.to_bits(), r_off0.to_bits());
    }

    /// §3.4 contract, stamp level: flipping `series_resistance` on a model
    /// with rb/re/rc changes the stamp (the core moves onto internal nodes
    /// behind ohmic resistors); on a default model (no internal nodes
    /// allocated) the two stamps are bit-identical.
    #[test]
    fn series_resistance_toggle_changes_bjt_stamp() {
        let model = BjtModel { rb: 100.0, re: 1.0, rc: 10.0, ..BjtModel::default() };
        let run = |model: BjtModel, series: bool| {
            let mut opts = SolverOptions::default();
            opts.effects.series_resistance = series;
            stamp_bjt_system(model, opts)
        };
        let (layout_on, m_on, _) = run(model, true);
        let (_, m_off, _) = run(model, false);
        // Three internal unknowns allocated either way (model-keyed).
        assert_eq!(layout_on.n_nodes, 2 + 3);
        let b_row = 0usize;
        let d_on = m_on.row(b_row).iter().find(|(c, _)| *c == b_row).map(|&(_, v)| v).unwrap();
        let d_off = m_off.row(b_row).iter().find(|(c, _)| *c == b_row).map(|&(_, v)| v).unwrap();
        // Toggle ON: the external base row carries ONLY the 1/rb series
        // conductance (the junction moved inside). Toggle OFF: it carries the
        // junction tangents and no series term.
        assert_eq!(d_on, 1.0 / 100.0);
        assert!(d_off != d_on);
        // Toggle OFF pins each internal unknown with a unit diagonal.
        let int_diag = m_off.row(2).iter().find(|(c, _)| *c == 2).map(|&(_, v)| v).unwrap();
        assert_eq!(int_diag, 1.0);
        // Default model: no internal nodes, bit-identical across the toggle.
        let (l_def_on, m_def_on, r_def_on) = run(BjtModel::default(), true);
        let (_, m_def_off, r_def_off) = run(BjtModel::default(), false);
        assert_eq!(l_def_on.n_nodes, 2);
        let (g1, r1) = pair(&m_def_on, &r_def_on, 0);
        let (g2, r2) = pair(&m_def_off, &r_def_off, 0);
        assert_eq!(g1.to_bits(), g2.to_bits());
        assert_eq!(r1.to_bits(), r2.to_bits());
    }

    /// The BJT junction charge is the integral of its capacitance (dQ/dv == C
    /// by finite difference) on both sides of the FC knee, for both junctions
    /// including the diffusion term.
    #[test]
    fn bjt_charge_is_integral_of_cap() {
        let m = charge_model();
        let q_be = |v: f64| {
            let is = m.is;
            let nvf = m.nf * hauksbee_ir::thermal_voltage_c(27.0);
            let ef = (v / nvf).clamp(-40.0, 40.0).exp();
            bjt_charge_be(&m, v, is * (ef - 1.0), is * ef / nvf)
        };
        for &v in &[-5.0, -1.0, 0.0, 0.2, 0.37, 0.5, 0.65] {
            let (_, c) = q_be(v);
            let d = 1e-7;
            let dq = (q_be(v + d).0 - q_be(v - d).0) / (2.0 * d);
            assert!(
                (dq - c).abs() < 1e-4 * c.abs().max(1e-18),
                "dQbe/dv != Cbe at v={v}: fd={dq:e} c={c:e}"
            );
        }
        let q_bc = |v: f64| {
            let is = m.is;
            let nvr = m.nr * hauksbee_ir::thermal_voltage_c(27.0);
            let er = (v / nvr).clamp(-40.0, 40.0).exp();
            bjt_charge_bc(&m, v, is * (er - 1.0), is * er / nvr)
        };
        for &v in &[-12.0, -3.0, 0.0, 0.37, 0.5] {
            let (_, c) = q_bc(v);
            let d = 1e-7;
            let dq = (q_bc(v + d).0 - q_bc(v - d).0) / (2.0 * d);
            assert!(
                (dq - c).abs() < 1e-4 * c.abs().max(1e-18),
                "dQbc/dv != Cbc at v={v}: fd={dq:e} c={c:e}"
            );
        }
    }

    /// Residual (terminal-current) rows `(f_b, f_c)` of a one-BJT system at a
    /// PINNED iterate: b at `vb` (previous iterate `vb_prev`), c at `vc`, e
    /// grounded — the pure DC physics through the same `stamp_residual` sink
    /// Newton brackets with. Temperature toggle OFF so `is` and `Vt` match
    /// hand arithmetic exactly.
    fn bjt_residual(model: BjtModel, vb: f64, vc: f64, vb_prev: f64) -> (f64, f64) {
        let mut cir = Circuit::new();
        let nb = cir.node("b");
        let nc = cir.node("c");
        cir.add(Device::Bjt {
            name: "Q1".into(),
            c: nc,
            b: nb,
            e: hauksbee_ir::NodeId::GROUND,
            model,
        });
        let layout = Layout::new(&cir);
        let b_row = layout.node(nb).unwrap();
        let c_row = layout.node(nc).unwrap();
        let mut x = vec![0.0; layout.size];
        x[b_row] = vb;
        x[c_row] = vc;
        let mut x_prev = x.clone();
        x_prev[b_row] = vb_prev;
        let state = ReactiveState::new(1);
        let mut opts = SolverOptions::default();
        opts.effects.temperature = false;
        let coeffs = IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-9, false);
        let spdt = std::collections::HashMap::new();
        let ctx = StampCtx {
            circuit: &cir,
            layout: &layout,
            opts: &opts,
            x: &x,
            x_prev: &x_prev,
            time: 0.0,
            coeffs,
            state: &state,
            dc: true,
            use_ic: false,
            gmin: 0.0,
            src_scale: 1.0,
            branch_reg: 0.0,
            cmp_freeze: None,
            switch_freeze: None,
            spdt_sibling: &spdt,
        };
        let mut f = vec![0.0; layout.size];
        stamp_residual(&ctx, &mut f);
        (f[b_row], f[c_row])
    }

    /// Bug-hunt r4 #11: `VAR` (reverse Early voltage) was parsed but never
    /// stamped — the base-charge factor carried only `-vbc/VAF`. The SGP
    /// factor is `q1_inv = 1 - vbc/VAF - vbe/VAR`, so at a forward vbe a
    /// finite VAR must scale the TRANSPORT current by exactly the corrected
    /// factor and in the physically-correct DIRECTION (forward base charge
    /// SHRINKS ic); the base current carries no q1 dependence and must be
    /// bit-identical, and VAR = ∞ must reproduce the VAF-only arithmetic.
    #[test]
    fn bjt_var_scales_transport_current() {
        let vt = hauksbee_ir::thermal_voltage_c(27.0);
        let (vb, vc) = (0.65, 3.0);
        let m_inf = BjtModel { vaf: 100.0, ..BjtModel::default() };
        let m_fin = BjtModel { vaf: 100.0, var: 15.0, ..BjtModel::default() };
        let (fb_inf, fc_inf) = bjt_residual(m_inf, vb, vc, vb);
        let (fb_fin, fc_fin) = bjt_residual(m_fin, vb, vc, vb);
        // Base current has no q1 factor: untouched to the bit.
        assert_eq!(fb_inf.to_bits(), fb_fin.to_bits());
        // Collector current scales by q1_inv(VAR)/q1_inv(∞) — smaller, not
        // larger (the ibc offset is ~1e-11 relative at this bias).
        let (vbe, vbc) = (vb, vb - vc);
        let q_inf = 1.0 - vbc / 100.0;
        let q_fin = q_inf - vbe / 15.0;
        assert!(fc_fin.abs() < fc_inf.abs(), "finite VAR must reduce ic");
        let ratio = fc_fin / fc_inf;
        assert!(
            (ratio - q_fin / q_inf).abs() < 1e-9,
            "ic ratio {ratio} != q1 ratio {}",
            q_fin / q_inf
        );
        // VAR = ∞ is the old VAF-only physics exactly: analytic collector
        // current at this iterate (br = 1, sign conventions folded out).
        let is = m_inf.is;
        let cf = is * ((vbe / vt).clamp(-40.0, 40.0).exp() - 1.0);
        let cr = is * ((vbc / vt).clamp(-40.0, 40.0).exp() - 1.0);
        let ic = (cf - cr) * q_inf - cr / m_inf.br;
        assert!(
            (fc_inf.abs() - ic).abs() < 1e-9 * ic,
            "VAR=inf collector current {} != VAF-only analytic {ic}",
            fc_inf.abs()
        );
    }

    /// Bug-hunt r4 #9: the vbc pnjlim call was fed the FORWARD critical
    /// voltage (built from nf·Vt) while limiting on the nr·Vt scale, so with
    /// nf != nr the reverse limiter gated and clamped at the wrong threshold
    /// (SPICE3 computes separate VCRITF/VCRITR for exactly this reason).
    /// Iteration-path only — but the wrong clamp bends Newton's path, so pin
    /// the vcrit values AND the stamped residual at a limited iterate.
    #[test]
    fn bjt_vbc_limiter_uses_reverse_vcrit() {
        let vt = hauksbee_ir::thermal_voltage_c(27.0);
        let m = BjtModel { nr: 1.5, ..BjtModel::default() };
        let vcrit_f = vcrit(m.is, m.nf * vt);
        let vcrit_r = vcrit(m.is, m.nr * vt);
        assert!(vcrit_r > vcrit_f, "nr-built vcrit must scale with nr");
        // The regression point: vbc stepping 0.9 -> 1.0 V sits BELOW the
        // nr-built vcrit (must pass unlimited) but ABOVE the nf-built one the
        // old wiring passed (which clamped it).
        assert!(vcrit_f < 1.0 && 1.0 < vcrit_r);
        assert_eq!(pnjlim(1.0, 0.9, m.nr * vt, vcrit_r), 1.0);
        assert_ne!(pnjlim(1.0, 0.9, m.nr * vt, vcrit_f), 1.0);
        // Stamp-level: b at 1.0 V (prev 0.9), c grounded — both junctions
        // forward. The base-row residual is F_b = A·x - rhs at the RAW x, so
        // a limited junction contributes its tangent extrapolated back to the
        // raw voltage: F_b = ib(limited) + gpi·(vbe_raw - vbe_lim)
        //                               + gmu·(vbc_raw - vbc_lim).
        // With the fix vbc passes UNLIMITED (vbc_lim == 1.0, its term is 0);
        // the old wiring clamped vbc near 0.95 V, shifting cr/gmu well past
        // this tolerance.
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
            "base residual {} != analytic {expected} at the nr-limited iterate",
            fb.abs()
        );
    }

    /// The extracted shared depletion helper reproduces the diode's charge
    /// arithmetic bit-for-bit (the §3.1 regression surface must not move).
    #[test]
    fn depletion_helper_is_bit_identical_to_diode_charge() {
        let m = DiodeModel { cjo: 4e-12, vj: 0.7, m: 0.45, ..DiodeModel::default() };
        for &vd in &[-5.0, -1.0, 0.0, 0.2, 0.34, 0.35, 0.4, 0.9] {
            let (q_h, c_h) = depletion_charge(m.cjo, m.vj, m.m, vd);
            let (idc, gd) = diode_eval(&m, vd, 27.0, false);
            let (q_d, c_d) = diode_charge(&m, vd, idc, gd);
            assert_eq!(q_d.to_bits(), (0.0f64 + q_h).to_bits(), "Q mismatch at {vd}");
            assert_eq!(c_d.to_bits(), (0.0f64 + c_h).to_bits(), "C mismatch at {vd}");
        }
    }
}

#[cfg(test)]
mod mos_channel_tests {
    use super::*;

    /// Bug-hunt r4 #10: the old two-branch channel had a genuine downward id
    /// jump of the full `i0 = beta·(n·Vt)²·e` at `vgs == vth` (and gm
    /// collapsed from `i0/(n·Vt)` to 0) — the "continuity scale" matched
    /// nothing. The blended overdrive must give id and gm with NO jump across
    /// threshold: consecutive fine-sweep deltas bounded by the local slope,
    /// one-sided limits at vth agreeing tightly, and gm equal to the true
    /// derivative d id/d vgs (C1 by finite difference). Swept for a
    /// signal-scale beta AND a power-scale kp, with and without CLM, and for
    /// a crossing that lands in triode (small vds) as well as saturation.
    /// stamp.rs and ac.rs share this ONE function, so the DC and AC tangents
    /// cannot disagree at the boundary by construction.
    #[test]
    fn mos_channel_id_and_gm_continuous_across_vth() {
        let vt = hauksbee_ir::thermal_voltage_c(27.0);
        for &(beta, nsub, lambda, vds) in &[
            (2e-5, 1.0, 0.0, 2.0),   // default-scale kp, saturation crossing
            (2e-5, 2.0, 0.02, 2.0),  // slope factor + CLM
            (20.0, 1.5, 0.05, 2.0),  // power-scale kp
            (20.0, 1.5, 0.05, 0.03), // crossing inside triode (small vds)
        ] {
            let nvt: f64 = nsub * vt;
            let f = |vov: f64| mos_channel(beta, nvt, vov, vds, lambda, 0.0);
            let clm = 1.0 + lambda * vds;
            // Fine sweep across the threshold: each id step bounded by the
            // local gm (no jump), each gm step bounded by the curvature scale
            // beta·clm (the square-law d²id/dvgs², which the blend never
            // exceeds by more than the sigmoid-chain factor).
            let h = 1e-4;
            let mut prev = f(-0.3);
            let mut k = 1;
            while -0.3 + (k as f64) * h <= 0.3 {
                let vov = -0.3 + (k as f64) * h;
                let cur = f(vov);
                let gmax = prev.1.max(cur.1);
                assert!(
                    (cur.0 - prev.0).abs() <= gmax * h * 1.5 + 1e-18,
                    "id jump at vov={vov} (beta={beta}): {} -> {}",
                    prev.0,
                    cur.0
                );
                assert!(
                    (cur.1 - prev.1).abs() <= 3.0 * beta * clm * h + 1e-18,
                    "gm jump at vov={vov} (beta={beta}): {} -> {}",
                    prev.1,
                    cur.1
                );
                prev = cur;
                k += 1;
            }
            // One-sided limits at exactly vth: value and slope agree tightly.
            let below = f(-1e-9);
            let at = f(0.0);
            let above = f(1e-9);
            assert!((above.0 - below.0).abs() <= 1e-6 * at.0.abs().max(1e-30));
            assert!((above.1 - below.1).abs() <= 1e-6 * at.1.abs().max(1e-30));
            // gm IS d id / d vgs — through the threshold, not just beside it.
            for &v in &[-0.1, -0.02, 0.0, 0.02, 0.1] {
                let d = 1e-7;
                let fd = (f(v + d).0 - f(v - d).0) / (2.0 * d);
                let gm = f(v).1;
                assert!(
                    (fd - gm).abs() <= 1e-4 * gm.abs().max(1e-15),
                    "gm != d id/d vgs at vov={v} (beta={beta}): fd={fd:e} gm={gm:e}"
                );
            }
        }
    }

    /// The blend keeps the promised physics on both sides: an exponential
    /// tail with slope n·Vt below threshold, the plain square law above, and
    /// zero current across a zero-bias channel.
    #[test]
    fn mos_channel_regions_physically_sane() {
        let vt = hauksbee_ir::thermal_voltage_c(27.0);
        let nvt = vt; // nsub = 1
        let beta = 2e-5;
        let f = |vov: f64, vds: f64| mos_channel(beta, nvt, vov, vds, 0.0, 0.0);
        // Deep subthreshold: one n·Vt of gate bias is one decade-of-e — the
        // exponential tail's defining ratio.
        let ratio = f(-0.2 + nvt, 2.0).0 / f(-0.2, 2.0).0;
        assert!(
            (ratio - std::f64::consts::E).abs() < 0.05 * std::f64::consts::E,
            "subthreshold slope: id ratio per n·Vt = {ratio}, want ~e"
        );
        // Strong inversion: the unblemished square law to well under 0.1%.
        let sq = 0.5 * beta * 0.5 * 0.5;
        assert!(((f(0.5, 2.0).0 - sq) / sq).abs() < 1e-3);
        // vds = 0: no phantom channel current at any gate bias.
        assert_eq!(f(-0.1, 0.0).0, 0.0);
        assert_eq!(f(0.5, 0.0).0, 0.0);
    }
}

#[cfg(test)]
mod bench {
    use super::*;
    use crate::system::ReactiveState;
    use hauksbee_ir::{Circuit, Device, SourceKind};

    /// A flagship-shaped synthetic board: repeated neuron-ish cells of
    /// resistors, caps, diode, BJT, comparator and an SPDT switch pair, sized
    /// to the joint capture march's scale (~5.8k devices, ~4k unknowns).
    fn big_board(cells: usize) -> Circuit {
        let mut c = Circuit::new();
        let vdd = c.node("vdd");
        c.add(Device::Vsource {
            name: "VDD".into(),
            p: vdd,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        for k in 0..cells {
            let m = c.node(&format!("m{k}"));
            let s = c.node(&format!("s{k}"));
            let o = c.node(&format!("o{k}"));
            let com = c.node(&format!("c{k}"));
            c.add(Device::Resistor { name: format!("R{k}a"), a: vdd, b: m, ohms: 10e3, tc1: None });
            c.add(Device::Resistor { name: format!("R{k}b"), a: m, b: NodeId::GROUND, ohms: 47e3, tc1: None });
            c.add(Device::Capacitor { name: format!("C{k}"), a: m, b: NodeId::GROUND, farads: 1e-9, ic: None });
            c.add(Device::Diode { name: format!("D{k}"), a: m, k: s, model: Default::default() });
            c.add(Device::Bjt { name: format!("Q{k}"), c: vdd, b: s, e: NodeId::GROUND, model: Default::default() });
            c.add(Device::Comparator {
                name: format!("K{k}"),
                out: o,
                inp: m,
                inn: s,
                out_lo: 0.0,
                out_hi: 5.0,
                hysteresis: 0.1,
            });
            c.add(Device::VSwitch {
                name: format!("G{k}_s1"),
                a: com, b: m, ctrl_p: o, ctrl_n: NodeId::GROUND,
                von: 3.0, voff: 2.0, ron: 10.0, roff: 1e9,
            });
            c.add(Device::VSwitch {
                name: format!("G{k}_s0"),
                a: com, b: vdd, ctrl_p: o, ctrl_n: NodeId::GROUND,
                von: 2.0, voff: 3.0, ron: 10.0, roff: 1e9,
            });
            c.add(Device::Resistor { name: format!("R{k}c"), a: com, b: NodeId::GROUND, ohms: 100.0, tc1: None });
        }
        c
    }

    /// Assembly-economy sub-lever B decision measurement (run with
    /// --ignored --nocapture): split one assembly's cost into device-model
    /// COMPUTE (the residual sink: same models, trivial writes) and
    /// ACCUMULATION (clear + slot-search stores + rhs), by timing `stamp_all`
    /// against `stamp_residual` on the same context. Whatever a deterministic
    /// parallel stamping scheme (per-thread buffers reduced in device order)
    /// can parallelize is bounded by the compute share; the accumulation is
    /// its serial section. Numbers feed the report; this is measurement
    /// estate, not a regression gate.
    #[test]
    #[ignore = "measurement harness, run explicitly"]
    fn bench_stamp_split() {
        let circuit = big_board(640); // ~5.8k devices
        let layout = Layout::new(&circuit);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&circuit, &layout, &mut m);
        let n = layout.size;
        let x: Vec<f64> = (0..n).map(|i| 0.5 + 0.001 * (i % 7) as f64).collect();
        let state = ReactiveState::new(circuit.devices.len());
        let opts = SolverOptions::default();
        let coeffs = IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, false);
        let spdt = std::collections::HashMap::new();
        let ctx = StampCtx {
            circuit: &circuit,
            layout: &layout,
            opts: &opts,
            x: &x,
            x_prev: &x,
            time: 1e-6,
            coeffs,
            state: &state,
            dc: false,
            use_ic: false,
            gmin: 1e-12,
            src_scale: 1.0,
            branch_reg: 1e-2,
            cmp_freeze: None,
            switch_freeze: None,
            spdt_sibling: &spdt,
        };
        let mut rhs = vec![0.0f64; n];
        let mut f = vec![0.0f64; n];
        const REPS: usize = 300;
        // Warm both paths.
        for _ in 0..10 {
            m.clear_values();
            for v in rhs.iter_mut() { *v = 0.0; }
            stamp_all(&ctx, &mut m, &mut rhs);
            for v in f.iter_mut() { *v = 0.0; }
            stamp_residual(&ctx, &mut f);
        }
        let t0 = std::time::Instant::now();
        for _ in 0..REPS {
            m.clear_values();
            for v in rhs.iter_mut() { *v = 0.0; }
            stamp_all(&ctx, &mut m, &mut rhs);
        }
        let full = t0.elapsed().as_secs_f64() / REPS as f64;
        let t1 = std::time::Instant::now();
        for _ in 0..REPS {
            for v in f.iter_mut() { *v = 0.0; }
            stamp_residual(&ctx, &mut f);
        }
        let resid = t1.elapsed().as_secs_f64() / REPS as f64;
        println!(
            "bench_stamp_split: devices={} unknowns={} full_assembly={:.3}ms residual_only={:.3}ms accumulation_share={:.0}%",
            circuit.devices.len(),
            n,
            full * 1e3,
            resid * 1e3,
            (1.0 - resid / full) * 100.0,
        );
        assert!(full > 0.0 && resid > 0.0);
    }

    /// ResidualSink correctness: on the same context, the matrix-free F must
    /// equal the assembled row product F within accumulation rounding (bounded
    /// RELATIVE TO EACH ROW'S TERM MAGNITUDES, because MNA rows cancel large
    /// terms and the two schemes sum in different orders). A systematic
    /// mismatch (sign error, dropped/duplicated contribution, wrong scope)
    /// shows up as an error far above the rounding floor.
    #[test]
    fn residual_sink_matches_assembled_row_product() {
        let circuit = big_board(64);
        let layout = Layout::new(&circuit);
        let mut m = SparseMatrix::new(layout.size);
        reserve_pattern(&circuit, &layout, &mut m);
        let n = layout.size;
        // A deliberately non-trivial iterate: mixed signs and magnitudes.
        let x: Vec<f64> = (0..n)
            .map(|i| ((i as f64 * 0.7391).sin()) * 3.0 + 0.1)
            .collect();
        let state = ReactiveState::new(circuit.devices.len());
        let opts = SolverOptions::default();
        let coeffs = IntegCoeffs::for_step(crate::options::Integration::Trapezoidal, 1e-7, false);
        let spdt = std::collections::HashMap::new();
        let ctx = StampCtx {
            circuit: &circuit,
            layout: &layout,
            opts: &opts,
            x: &x,
            x_prev: &x,
            time: 1e-6,
            coeffs,
            state: &state,
            dc: false,
            use_ic: false,
            gmin: 1e-12,
            src_scale: 1.0,
            branch_reg: 1e-2,
            cmp_freeze: None,
            switch_freeze: None,
            spdt_sibling: &spdt,
        };
        let mut rhs = vec![0.0f64; n];
        m.clear_values();
        stamp_all(&ctx, &mut m, &mut rhs);
        let mut f = vec![0.0f64; n];
        stamp_residual(&ctx, &mut f);
        for i in 0..layout.n_nodes {
            let mut acc = 0.0;
            let mut mag = rhs[i].abs();
            for &(col, val) in m.row(i) {
                acc += val * x[col];
                mag += (val * x[col]).abs();
            }
            let assembled = acc - rhs[i];
            let err = (f[i] - assembled).abs();
            let bound = 1e-12 * mag.max(1.0);
            assert!(
                err <= bound,
                "row {i}: sink F={} assembled F={} err={err:e} > bound={bound:e} (mag {mag:e})",
                f[i],
                assembled,
            );
        }
    }
}

#[cfg(test)]
mod behavioral_fd_tests {
    use super::*;
    use hauksbee_ir::{BDep, CompiledExpr, DeviceId};

    /// FD-Jacobian accuracy gate (plan §2.5): the forward-difference partials
    /// must match ANALYTIC derivatives on a known expression at representative
    /// operating points. With `delta = reltol*|x| + floor` the forward
    /// difference carries an O(delta * f''/2) truncation term, so the bar is
    /// set from reltol (1e-3) times the local curvature — a few 1e-3 relative
    /// on curved terms, tighter on linear ones.
    #[test]
    fn fd_partials_match_analytic() {
        // f(v, i) = 2 v + 100 tanh(5 i) + 0.1 t
        //   df/dv = 2 (exactly, linear)
        //   df/di = 500 sech^2(5 i)
        let expr = CompiledExpr::compile("2.0*__d0 + 100.0*math::tanh(5.0*__d1) + 0.1*time")
            .unwrap();
        let deps = [BDep::Volt(hauksbee_ir::NodeId(1)), BDep::Branch(DeviceId(0))];
        let opts = SolverOptions::default();
        let mut worst_lin = 0.0f64;
        let mut worst_curved = 0.0f64;
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
            let f_true = 2.0 * v + 100.0 * (5.0 * i).tanh() + 0.1 * t;
            assert!((f0 - f_true).abs() < 1e-12 * f_true.abs().max(1.0), "f0 at ({v},{i},{t})");
            // Linear partial: FD is exact to rounding for a linear term...
            // except for the tanh term's contribution? No: partials are per
            // SLOT — slot 0 perturbs v only, and f is linear in v, so the
            // difference quotient is exactly 2 up to cancellation rounding.
            let dv_err = ((partials[0] - 2.0) / 2.0).abs();
            worst_lin = worst_lin.max(dv_err);
            assert!(dv_err < 1e-9, "df/dv at ({v},{i},{t}): {} (rel {dv_err:e})", partials[0]);
            // Curved partial: truncation O(delta/2 * f''), delta ~ 1e-3|i|+1e-12.
            let sech2 = 1.0 / (5.0f64 * i).cosh().powi(2);
            let di_true = 500.0 * sech2;
            let di_err = ((partials[1] - di_true) / di_true.abs().max(1e-12)).abs();
            worst_curved = worst_curved.max(di_err);
            assert!(
                di_err < 5e-3,
                "df/di at ({v},{i},{t}): fd={} analytic={di_true} rel={di_err:e}",
                partials[1]
            );
        }
        // Print the measured accuracy so the gate report carries real numbers
        // (run with --nocapture).
        println!(
            "behavioral FD accuracy: worst linear rel err {worst_lin:.3e}, \
             worst curved rel err {worst_curved:.3e}"
        );
    }

    /// The fault contract of the partials helper: NaN values, INF values, and
    /// eval errors all come back as Err — never as poisoned numbers.
    #[test]
    fn fd_partials_report_faults() {
        let opts = SolverOptions::default();
        let ln = CompiledExpr::compile("math::ln(__d0)").unwrap();
        let deps = [BDep::Volt(hauksbee_ir::NodeId(1))];
        // NaN at the base point.
        let mut vals = vec![-1.0];
        assert!(behavioral_eval_partials(&ln, &deps, &mut vals, 0.0, &opts).is_err());
        // INF from division by zero.
        let div = CompiledExpr::compile("1.0/__d0").unwrap();
        let mut vals = vec![0.0];
        assert!(behavioral_eval_partials(&div, &deps, &mut vals, 0.0, &opts).is_err());
        // A fault while PROBING a partial (base point fine, perturbed point
        // NaN): ln(x) at x just below 0 after the +delta probe crosses it...
        // construct via ln(-__d0) with x = -1e-15: base -x = 1e-15 > 0 ok,
        // probe x+delta makes -x negative -> NaN.
        let flip = CompiledExpr::compile("math::ln(0.0 - __d0)").unwrap();
        let mut vals = vec![-1e-15];
        assert!(behavioral_eval_partials(&flip, &deps, &mut vals, 0.0, &opts).is_err());
        // And vals is restored even on the fault path? The base value is
        // written back before the error returns, so callers can reuse it.
        assert_eq!(vals[0], -1e-15);
    }
}
