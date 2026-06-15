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
fn stamp_cond(m: &mut SparseMatrix, layout: &Layout, a: NodeId, b: NodeId, g: f64) {
    let ai = layout.node(a);
    let bi = layout.node(b);
    if let Some(ai) = ai {
        m.add(ai, ai, g);
    }
    if let Some(bi) = bi {
        m.add(bi, bi, g);
    }
    if let (Some(ai), Some(bi)) = (ai, bi) {
        m.add(ai, bi, -g);
        m.add(bi, ai, -g);
    }
}

/// Stamp an equivalent current source pushing `i` from `a` to `b`.
#[inline]
fn stamp_current(rhs: &mut [f64], layout: &Layout, a: NodeId, b: NodeId, i: f64) {
    if let Some(ai) = layout.node(a) {
        rhs[ai] -= i;
    }
    if let Some(bi) = layout.node(b) {
        rhs[bi] += i;
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

/// Stamp every device for the current iterate into `(g, rhs)`.
pub fn stamp_all(ctx: &StampCtx, g: &mut SparseMatrix, rhs: &mut [f64]) {
    // gmin / homotopy conductance to ground.
    if ctx.gmin > 0.0 {
        for i in 0..ctx.layout.size {
            // Branch rows shouldn't get a shunt; only node rows (< n_nodes).
            if i < ctx.layout.n_nodes {
                g.add(i, i, ctx.gmin);
            }
        }
    }
    // Staged-DC branch regularization: a negligible series resistance on every
    // Vsource/Inductor branch diagonal, so the frozen ordering always has a
    // nonzero pivot there even when a conducting diode reshapes the elimination.
    if ctx.branch_reg > 0.0 {
        for i in ctx.layout.n_nodes..ctx.layout.size {
            g.add(i, i, -ctx.branch_reg);
        }
    }
    for (id, dev) in ctx.circuit.iter() {
        stamp_device(ctx, id, dev, g, rhs);
    }
}

fn stamp_device(ctx: &StampCtx, id: DeviceId, dev: &Device, g: &mut SparseMatrix, rhs: &mut [f64]) {
    match dev {
        Device::Resistor { a, b, ohms, tc1, .. } => {
            let r = resistor_value(*ohms, *tc1, ctx);
            if r > 0.0 {
                stamp_cond(g, ctx.layout, *a, *b, 1.0 / r);
            }
        }
        Device::Capacitor { a, b, farads, ic, .. } => {
            stamp_capacitor(ctx, id, *a, *b, *farads, *ic, g, rhs)
        }
        Device::Inductor { a, b, henries, ic, .. } => {
            stamp_inductor(ctx, id, *a, *b, *henries, *ic, g, rhs)
        }
        Device::Vsource { p, n, kind, .. } => stamp_vsource(ctx, id, *p, *n, kind, g, rhs),
        Device::Isource { p, n, kind, .. } => {
            let val = source_value(ctx, kind);
            stamp_current(rhs, ctx.layout, *p, *n, val * ctx.src_scale);
        }
        Device::Diode { a, k, model, .. } => stamp_diode(ctx, *a, *k, model, g, rhs),
        Device::Bjt { c, b, e, model, .. } => stamp_bjt(ctx, *c, *b, *e, model, g, rhs),
        Device::Mosfet { d, g: gate, s, model, .. } => {
            stamp_mosfet(ctx, *d, *gate, *s, model, g, rhs)
        }
        Device::VSwitch { a, b, ctrl_p, ctrl_n, von, voff, ron, roff, .. } => {
            stamp_vswitch(ctx, id, *a, *b, *ctrl_p, *ctrl_n, *von, *voff, *ron, *roff, g, rhs)
        }
        Device::OpAmp { out, inp, inn, gain, rail_lo, rail_hi, .. } => {
            stamp_opamp(ctx, *out, *inp, *inn, *gain, *rail_lo, *rail_hi, g, rhs)
        }
        Device::Comparator { out, inp, inn, out_lo, out_hi, hysteresis, .. } => {
            stamp_comparator(ctx, id, *out, *inp, *inn, *out_lo, *out_hi, *hysteresis, g, rhs)
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
fn stamp_capacitor(
    ctx: &StampCtx,
    id: DeviceId,
    a: NodeId,
    b: NodeId,
    c: f64,
    ic: Option<f64>,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
) {
    if ctx.dc {
        // For the initial-conditions operating point, pin the capacitor voltage
        // to its IC via a stiff penalty conductance (keeps the pattern fixed).
        if ctx.use_ic {
            if let Some(vic) = ic {
                let gpin = 1e9; // very stiff; drives v across the cap -> vic
                stamp_cond(g, ctx.layout, a, b, gpin);
                stamp_current(rhs, ctx.layout, a, b, -gpin * vic);
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
    stamp_cond(g, ctx.layout, a, b, geq);
    // Equivalent current ieq flows like the capacitor current a->b.
    stamp_current(rhs, ctx.layout, a, b, -ieq);
}

#[allow(clippy::too_many_arguments)]
fn stamp_inductor(
    ctx: &StampCtx,
    id: DeviceId,
    a: NodeId,
    b: NodeId,
    l: f64,
    ic: Option<f64>,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
) {
    let br = ctx.layout.branch(id).expect("inductor has a branch unknown");
    let ai = ctx.layout.node(a);
    let bi = ctx.layout.node(b);
    // KCL: the branch current enters the two node equations (matrix columns).
    if let Some(ai) = ai {
        g.add(ai, br, 1.0);
    }
    if let Some(bi) = bi {
        g.add(bi, br, -1.0);
    }

    if ctx.dc && ctx.use_ic {
        if let Some(iic) = ic {
            // Initial-conditions point: pin the branch current, i = iic. The
            // branch row carries no voltage terms in this mode.
            g.add(br, br, 1.0);
            rhs[br] += iic;
            return;
        }
    }

    // Branch-row voltage relation: v_a - v_b (- req*i) = veq.
    if let Some(ai) = ai {
        g.add(br, ai, 1.0);
    }
    if let Some(bi) = bi {
        g.add(br, bi, -1.0);
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
    g.add(br, br, -req);
    rhs[br] -= veq;
}

fn stamp_vsource(
    ctx: &StampCtx,
    id: DeviceId,
    p: NodeId,
    n: NodeId,
    kind: &SourceKind,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
) {
    let br = ctx.layout.branch(id).expect("vsource has a branch unknown");
    let pi = ctx.layout.node(p);
    let ni = ctx.layout.node(n);
    if let Some(pi) = pi {
        g.add(pi, br, 1.0);
        g.add(br, pi, 1.0);
    }
    if let Some(ni) = ni {
        g.add(ni, br, -1.0);
        g.add(br, ni, -1.0);
    }
    let val = source_value(ctx, kind) * ctx.src_scale;
    rhs[br] += val;
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

fn stamp_diode(
    ctx: &StampCtx,
    a: NodeId,
    k: NodeId,
    model: &DiodeModel,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
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
    stamp_cond(g, ctx.layout, a, k, gd);
    stamp_current(rhs, ctx.layout, a, k, ieq);
}

fn stamp_bjt(
    ctx: &StampCtx,
    c: NodeId,
    b: NodeId,
    e: NodeId,
    model: &BjtModel,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
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
    add_pair(g, bi, ei, gpi);
    add_pair(g, bi, ci, gmu);
    // output conductance go between c-e.
    add_pair(g, ci, ei, go);
    // transconductance: ic depends on vbe = v(b)-v(e).
    add_transconductance(g, ci, ei, bi, ei, gm);

    // Equivalent currents: residual = I_terminal - linearized part, both in
    // folded (NPN-reference) space, then mapped to real space by `sign`. The
    // matrix rows in real space equal sign * (folded linear part), so the
    // whole bracket folds together -- folding only some terms breaks PNP
    // convergence (sign = -1 flips gm/gpi/gmu contributions).
    let vce_f = sign * (ctx.v(c) - ctx.v(e));
    let ic_eq = sign * (ic - (gm * vbe + go * vce_f - gmu * vbc));
    let ib_eq = sign * (ib - (gpi * vbe + gmu * vbc));
    let ie_eq = sign * (ie + (gm + gpi) * vbe + go * vce_f);

    inject(rhs, ci, -ic_eq);
    inject(rhs, bi, -ib_eq);
    inject(rhs, ei, -ie_eq);
}

fn stamp_mosfet(
    ctx: &StampCtx,
    d: NodeId,
    gnode: NodeId,
    s: NodeId,
    model: &MosfetModel,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
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
        let lambda = if ctx.opts.effects.early_effect { model.lambda } else { 0.0 };
        let vov = vgs - vth;
        if vds < vov {
            // Triode.
            let id = beta * (vov * vds - 0.5 * vds * vds) * (1.0 + lambda * vds);
            let gm = beta * vds * (1.0 + lambda * vds);
            let gds = beta * ((vov - vds) * (1.0 + lambda * vds) + (vov * vds - 0.5 * vds * vds) * lambda);
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
    let (di, gi, si) = (ctx.layout.node(d), ctx.layout.node(gnode), ctx.layout.node(s));
    let (dn, sn) = if swap { (si, di) } else { (di, si) };

    // Conductances: gds between drain-source, gm couples drain current to vgs.
    add_pair(g, dn, sn, gds);
    add_transconductance(g, dn, sn, gi, sn, gm);

    // Equivalent current: id_eq = ids - gm*vgs - gds*vds, flowing d->s.
    let ieq = ids - gm * vgs - gds * vds;
    let ieq_signed = sign * ieq;
    inject(rhs, dn, -ieq_signed);
    inject(rhs, sn, ieq_signed);
}

#[allow(clippy::too_many_arguments)]
fn stamp_vswitch(
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
    g: &mut SparseMatrix,
    rhs: &mut [f64],
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
        stamp_cond(g, ctx.layout, a, b, gsw);
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
    let s = 0.5 * (1.0 + th);
    let gln = lgoff + s * (lgon - lgoff);
    let gsw = gln.exp();

    // Conductance stamp between the through nodes (a, b).
    stamp_cond(g, ctx.layout, a, b, gsw);

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
    let skip_ctrl_gm = std::env::var("HAUKSBEE_SW_NO_CTRL_GM").as_deref() == Ok("1");
    let dgsw_dvctrl = gsw * (lgon - lgoff) * 0.5 * (1.0 - th * th) * (3.0 / span);
    let vab = ctx.v(a) - ctx.v(b);
    let gm_ctrl = vab * dgsw_dvctrl;
    if gm_ctrl != 0.0 && !skip_ctrl_gm {
        let (ai, bi) = (ctx.layout.node(a), ctx.layout.node(b));
        let (cpi, cni) = (ctx.layout.node(cp), ctx.layout.node(cn));
        // i_a += gm_ctrl * (v_cp - v_cn), i_b -= ... .
        add_transconductance(g, ai, bi, cpi, cni, gm_ctrl);
        // Equivalent-current correction: the transconductance term contributes
        // gm_ctrl * vctrl to the linearized current, which is already implicit
        // in the conductance stamp at the operating point. Subtract it back via
        // the RHS so the residual equals the true device current there.
        let ieq = gm_ctrl * vctrl;
        inject(rhs, ai, ieq);
        inject(rhs, bi, -ieq);
    }
}

fn stamp_opamp(
    ctx: &StampCtx,
    out: NodeId,
    inp: NodeId,
    inn: NodeId,
    gain: f64,
    rail_lo: f64,
    rail_hi: f64,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
) {
    // Behavioral: drive `out` toward clamp(gain*(vp-vn)) through a stiff
    // conductance, modeling an ideal output stage with finite open-loop gain.
    let gout = 1.0; // output stage conductance (1 ohm), strong source
    let vp = ctx.v(inp);
    let vn = ctx.v(inn);
    let target = (gain * (vp - vn)).clamp(rail_lo, rail_hi);
    // Linearize: out = target + gain*(d vp - d vn) within rails.
    let in_rail = target > rail_lo && target < rail_hi;
    let oi = ctx.layout.node(out);
    if let Some(oi) = oi {
        g.add(oi, oi, gout);
        rhs[oi] += gout * target;
        if in_rail {
            // Couple output to inputs through the gain (tangent).
            if let Some(pi) = ctx.layout.node(inp) {
                g.add(oi, pi, -gout * gain);
                rhs[oi] -= gout * gain * vp;
            }
            if let Some(ni) = ctx.layout.node(inn) {
                g.add(oi, ni, gout * gain);
                rhs[oi] += gout * gain * vn;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stamp_comparator(
    ctx: &StampCtx,
    id: DeviceId,
    out: NodeId,
    inp: NodeId,
    inn: NodeId,
    out_lo: f64,
    out_hi: f64,
    hyst: f64,
    g: &mut SparseMatrix,
    rhs: &mut [f64],
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
            g.add(oi, oi, gout);
            rhs[oi] += gout * target;
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
        let k = std::env::var("HAUKSBEE_CMP_K").ok()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(2000.0); // 1/V; ~2 mV transition width
        let d = vp - vn - thresh;
        let kd = (k * d).clamp(-40.0, 40.0);
        let e = (-kd).exp();
        let sig = 1.0 / (1.0 + e); // logistic in (0,1)
        let target = out_lo + span * sig;
        let dsig = k * sig * (1.0 - sig);
        let dtarget_dvp = span * dsig; // d target / d vp  (= -d/d vn)
        if let Some(oi) = ctx.layout.node(out) {
            g.add(oi, oi, gout);
            rhs[oi] += gout * target;
            if let Some(pi) = ctx.layout.node(inp) {
                g.add(oi, pi, -gout * dtarget_dvp);
                rhs[oi] -= gout * dtarget_dvp * vp;
            }
            if let Some(ni) = ctx.layout.node(inn) {
                g.add(oi, ni, gout * dtarget_dvp);
                rhs[oi] += gout * dtarget_dvp * vn;
            }
        }
        return;
    }

    let target = if vp - vn > thresh { out_hi } else { out_lo };
    if let Some(oi) = ctx.layout.node(out) {
        g.add(oi, oi, gout);
        rhs[oi] += gout * target;
    }
}

// --- low-level stamp helpers ------------------------------------------------

#[inline]
fn add_pair(m: &mut SparseMatrix, a: Option<usize>, b: Option<usize>, gval: f64) {
    if let Some(a) = a {
        m.add(a, a, gval);
    }
    if let Some(b) = b {
        m.add(b, b, gval);
    }
    if let (Some(a), Some(b)) = (a, b) {
        m.add(a, b, -gval);
        m.add(b, a, -gval);
    }
}

/// Stamp a transconductance: current into `(ip, in)` proportional to the
/// voltage across `(cp, cn)`. `i = gm * (v_cp - v_cn)` added at ip, removed
/// at in.
#[inline]
fn add_transconductance(
    m: &mut SparseMatrix,
    ip: Option<usize>,
    in_: Option<usize>,
    cp: Option<usize>,
    cn: Option<usize>,
    gm: f64,
) {
    if let Some(ip) = ip {
        if let Some(cp) = cp {
            m.add(ip, cp, gm);
        }
        if let Some(cn) = cn {
            m.add(ip, cn, -gm);
        }
    }
    if let Some(in_) = in_ {
        if let Some(cp) = cp {
            m.add(in_, cp, -gm);
        }
        if let Some(cn) = cn {
            m.add(in_, cn, gm);
        }
    }
}

#[inline]
fn inject(rhs: &mut [f64], node: Option<usize>, val: f64) {
    if let Some(n) = node {
        rhs[n] += val;
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
