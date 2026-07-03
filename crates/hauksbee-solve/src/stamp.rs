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
    /// HAUKSBEE_SPDT_BBM=1; empty/ignored otherwise (path bit-identical).
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
        let ns: Vec<Option<usize>> = dev.nodes().iter().map(|&n| layout.node(n)).collect();
        // All-pairs structural coupling among a device's nodes covers every
        // companion/tangent stamp it can produce.
        for i in 0..ns.len() {
            for j in 0..ns.len() {
                touch(m, ns[i], ns[j]);
            }
        }
        if let Some(br) = layout.branch(id) {
            // Branch couples to its two primary nodes both ways, plus its own
            // diagonal slot.
            m.touch(br, br);
            for &n in &ns[..2.min(ns.len())] {
                if let Some(n) = n {
                    m.touch(br, n);
                    m.touch(n, br);
                }
            }
        }
    }
}

/// Device-model env knobs, read ONCE per assembly instead of once per device.
/// WHY: `stamp_all` runs once per Newton iteration AND once per line-search
/// residual eval (~120k assemblies on the flagship's joint capture march), and
/// every VSwitch/Comparator stamp used to consult `std::env::var` inline: on a
/// board with hundreds of switches that is hundreds of env-lock acquisitions,
/// environ scans, and String allocations per assembly, a measured double-digit
/// share of assembly cost. The values feed the SAME expressions, so the
/// stamped numbers are bit-identical; the snapshot granularity (per assembly,
/// i.e. per Newton iteration) is indistinguishable for every real caller
/// (tests set these knobs around whole solves, never mid-assembly).
struct EnvKnobs {
    /// HAUKSBEE_SPDT_BBM=1: SPDT break-before-make winner-take-all.
    spdt_bbm: bool,
    /// HAUKSBEE_SPDT_BBM_K: BBM transition sharpness (default 6).
    spdt_bbm_k: f64,
    /// HAUKSBEE_SW_NO_CTRL_GM=1: drop the switch control back-coupling.
    sw_no_ctrl_gm: bool,
    /// HAUKSBEE_CMP_K: staged smooth-comparator gain (default 2000 / V).
    cmp_k: f64,
}

impl EnvKnobs {
    fn read() -> EnvKnobs {
        EnvKnobs {
            spdt_bbm: std::env::var("HAUKSBEE_SPDT_BBM").as_deref() == Ok("1"),
            spdt_bbm_k: std::env::var("HAUKSBEE_SPDT_BBM_K")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(6.0),
            sw_no_ctrl_gm: std::env::var("HAUKSBEE_SW_NO_CTRL_GM").as_deref() == Ok("1"),
            cmp_k: std::env::var("HAUKSBEE_CMP_K")
                .ok()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(2000.0),
        }
    }
}

/// Stamp every device for the current iterate into `(g, rhs)`.
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
    let knobs = EnvKnobs::read();
    for (id, dev) in ctx.circuit.iter() {
        stamp_device(ctx, &knobs, id, dev, sink);
    }
}

fn stamp_device<S: StampSink>(
    ctx: &StampCtx,
    knobs: &EnvKnobs,
    id: DeviceId,
    dev: &Device,
    sink: &mut S,
) {
    match dev {
        Device::Resistor {
            a, b, ohms, tc1, ..
        } => {
            let r = resistor_value(*ohms, *tc1, ctx);
            if r > 0.0 {
                stamp_cond(sink, ctx.layout, *a, *b, 1.0 / r);
            }
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
        Device::Diode { a, k, model, .. } => stamp_diode(ctx, *a, *k, model, sink),
        Device::Bjt { c, b, e, model, .. } => stamp_bjt(ctx, *c, *b, *e, model, sink),
        Device::Mosfet { d, g: gate, s, model, .. } => {
            stamp_mosfet(ctx, *d, *gate, *s, model, sink)
        }
        Device::VSwitch { a, b, ctrl_p, ctrl_n, von, voff, ron, roff, .. } => stamp_vswitch(
            ctx, knobs, id, *a, *b, *ctrl_p, *ctrl_n, *von, *voff, *ron, *roff, sink,
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
            ctx, knobs, id, *out, *inp, *inn, *out_lo, *out_hi, *hysteresis, sink,
        ),
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
fn diode_eval(model: &DiodeModel, vd: f64, t_c: f64, temp_on: bool) -> (f64, f64) {
    let is = if temp_on { model.is_at(t_c) } else { model.is };
    let nvt = model.n * hauksbee_ir_thermal(t_c, temp_on);
    if vd >= -3.0 * nvt {
        let e = (vd / nvt).exp();
        let id = is * (e - 1.0);
        let gd = is * e / nvt;
        (id, gd)
    } else {
        // Reverse region: tiny linear leakage, conductance ~ gmin handled outside.
        let id = -is;
        let gd = is / nvt * 1e-3;
        (id, gd)
    }
}

/// Thermal voltage helper that respects the temperature toggle.
fn hauksbee_ir_thermal(t_c: f64, temp_on: bool) -> f64 {
    let t = if temp_on { t_c } else { 27.0 };
    hauksbee_ir::thermal_voltage_c(t)
}

fn stamp_diode<S: StampSink>(
    ctx: &StampCtx,
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

    let vd_raw = ctx.v(a) - ctx.v(k);
    // Use the last accepted junction voltage as the limiting anchor.
    let mut vd = pnjlim(vd_raw, ctx.last_vd(a, k), nvt, vc);

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

    let (id, gd) = diode_eval(model, vd, t_c, temp_on);
    let gd = gd.max(ctx.opts.gmin);
    // Newton equivalent current: ieq = id - gd*vd.
    let ieq = id - gd * vd;
    stamp_cond(sink, ctx.layout, a, k, gd);
    stamp_current(sink, ctx.layout, a, k, ieq);
}

fn stamp_bjt<S: StampSink>(
    ctx: &StampCtx,
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

    // Internal junction voltages with polarity folded in.
    let vbe = sign * (ctx.v(b) - ctx.v(e));
    let vbc = sign * (ctx.v(b) - ctx.v(c));
    let vcrit_be = vcrit(is, model.nf * vt);
    let vbe = pnjlim(vbe, sign * ctx.last_vbe(b, e), model.nf * vt, vcrit_be);
    let vbc = pnjlim(vbc, sign * ctx.last_vbc(b, c), model.nr * vt, vcrit_be);

    let nvf = model.nf * vt;
    let nvr = model.nr * vt;

    // Forward / reverse transport currents (clamped exponent for safety).
    let ef = (vbe / nvf).clamp(-40.0, 40.0).exp();
    let er = (vbc / nvr).clamp(-40.0, 40.0).exp();
    let cf = is * (ef - 1.0); // forward diffusion
    let cr = is * (er - 1.0); // reverse diffusion

    // Base-width modulation (Early effect).
    let early = if ctx.opts.effects.early_effect && model.vaf.is_finite() {
        1.0 - vbc / model.vaf
    } else {
        1.0
    };
    let inv_early = 1.0 / early.max(0.1);

    // Transport (collector) current and the two base components.
    let ict = (cf - cr) * inv_early;
    let ibe = cf / model.bf;
    let ibc = cr / model.br;

    // Conductances (derivatives wrt the junction voltages).
    let gpi = is * ef / (nvf * model.bf); // d ibe / d vbe
    let gmu = is * er / (nvr * model.br); // d ibc / d vbc
    let gif = is * ef / nvf; // d cf / d vbe
    let gir = is * er / nvr; // d cr / d vbc
    let gm = (gif * inv_early).max(ctx.opts.gmin); // forward transconductance
    let go = (gir * inv_early).max(ctx.opts.gmin); // reverse / output

    // Terminal currents into c, b, e (NPN reference, then sign-folded).
    let ic = ict - ibc;
    let ib = ibe + ibc;
    let ie = -(ic + ib);

    // Linearized stamp. We add conductances among (b,e) and (b,c) plus the
    // transconductance gm coupling c to (vbe). This is the standard GP small
    // companion: matrix entries follow the partials, RHS gets the residual.
    let (ci, bi, ei) = (ctx.layout.node(c), ctx.layout.node(b), ctx.layout.node(e));

    // gpi between b-e, gmu between b-c.
    add_pair(sink, bi, ei, gpi);
    add_pair(sink, bi, ci, gmu);
    // output conductance go between c-e.
    add_pair(sink, ci, ei, go);
    // transconductance: ic depends on vbe = v(b)-v(e).
    add_transconductance(sink, ci, ei, bi, ei, gm);

    // Equivalent currents: residual = I_terminal - linearized part, both in
    // folded (NPN-reference) space, then mapped to real space by `sign`. The
    // matrix rows in real space equal sign * (folded linear part), so the
    // whole bracket folds together -- folding only some terms breaks PNP
    // convergence (sign = -1 flips gm/gpi/gmu contributions).
    let vce_f = sign * (ctx.v(c) - ctx.v(e));
    let ic_eq = sign * (ic - (gm * vbe + go * vce_f - gmu * vbc));
    let ib_eq = sign * (ib - (gpi * vbe + gmu * vbc));
    let ie_eq = sign * (ie + (gm + gpi) * vbe + go * vce_f);

    inject(sink, ci, -ic_eq);
    inject(sink, bi, -ib_eq);
    inject(sink, ei, -ie_eq);
}

fn stamp_mosfet<S: StampSink>(
    ctx: &StampCtx,
    d: NodeId,
    gnode: NodeId,
    s: NodeId,
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
    let vth = model.vto;

    let nsub = model.n_sub.max(1.0);
    let (ids, gm, gds) = if vgs - vth < -nsub * vt * 10.0 {
        // Deep subthreshold: negligible current, tiny conductance.
        (0.0, ctx.opts.gmin, ctx.opts.gmin)
    } else if vgs <= vth {
        // Subthreshold exponential region (smooth turn-on).
        let i0 = beta * (nsub * vt) * (nsub * vt) * std::f64::consts::E; // continuity scale
        let id = i0 * ((vgs - vth) / (nsub * vt)).exp();
        let gm = id / (nsub * vt);
        (id.min(1e3), gm.max(ctx.opts.gmin), ctx.opts.gmin)
    } else {
        let lambda = if ctx.opts.effects.early_effect {
            model.lambda
        } else {
            0.0
        };
        let vov = vgs - vth;
        if vds < vov {
            // Triode.
            let id = beta * (vov * vds - 0.5 * vds * vds) * (1.0 + lambda * vds);
            let gm = beta * vds * (1.0 + lambda * vds);
            let gds = beta
                * ((vov - vds) * (1.0 + lambda * vds) + (vov * vds - 0.5 * vds * vds) * lambda);
            (id, gm.max(ctx.opts.gmin), gds.max(ctx.opts.gmin))
        } else {
            // Saturation.
            let id = 0.5 * beta * vov * vov * (1.0 + lambda * vds);
            let gm = beta * vov * (1.0 + lambda * vds);
            let gds = 0.5 * beta * vov * vov * lambda;
            (id, gm.max(ctx.opts.gmin), gds.max(ctx.opts.gmin))
        }
    };

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
    let ieq = ids - gm * vgs - gds * vds;
    let ieq_signed = sign * ieq;
    inject(sink, dn, -ieq_signed);
    inject(sink, sn, ieq_signed);
}

#[allow(clippy::too_many_arguments)]
fn stamp_vswitch<S: StampSink>(
    ctx: &StampCtx,
    knobs: &EnvKnobs,
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

    // BREAK-BEFORE-MAKE for SPDT legs (HAUKSBEE_SPDT_BBM=1, gated; default OFF ->
    // bit-identical). A real SN74LVC1G3157 SPDT is NEVER low-Z to both throws at
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
    // Newton. K sets the transition sharpness (HAUKSBEE_SPDT_BBM_K, default 6).
    let bbm = knobs.spdt_bbm;
    if bbm {
        if let Some(&sib) = ctx.spdt_sibling.get(&id) {
            if let Some(Device::VSwitch { ctrl_p: scp, ctrl_n: scn, von: svon, voff: svoff, .. }) =
                ctx.circuit.devices.get(sib.0 as usize)
            {
                let svmid = 0.5 * (svon + svoff);
                let sspan = (svon - svoff).abs().max(1e-9);
                let margin_self = (vctrl - vmid) / span;
                let margin_sib = ((ctx.v(*scp) - ctx.v(*scn)) - svmid) / sspan;
                let k_bbm = knobs.spdt_bbm_k;
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
    // slower than its R*C). HAUKSBEE_SW_NO_CTRL_GM=1 drops the back-coupling so
    // the control node is purely exogenous; the conductance stamp + the switch's
    // own a/b dynamics still track the flip. OFF by default -> bit-identical.
    let skip_ctrl_gm = knobs.sw_no_ctrl_gm;
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
    knobs: &EnvKnobs,
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
        let k = knobs.cmp_k; // 1/V; ~2 mV transition width
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
    fn last_vbe(&self, b: NodeId, e: NodeId) -> f64 {
        self.v_prev(b) - self.v_prev(e)
    }
    fn last_vbc(&self, b: NodeId, c: NodeId) -> f64 {
        self.v_prev(b) - self.v_prev(c)
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
