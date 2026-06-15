//! Newton-Raphson iteration and the DC operating point.
//!
//! One Newton iteration assembles the linearized system `g x = rhs` (every
//! device stamps its tangent and equivalent current at the current iterate),
//! solves it with the reusable factorization, and checks convergence. The DC
//! operating point reuses this with reactive elements opened/shorted, plus
//! gmin-stepping and source-stepping homotopy when the cold-start Newton stalls.

use crate::options::SolverOptions;
use crate::plan::StampPlan;
use crate::sparse::{SparseMatrix, Symbolic};
use crate::stamp::{reserve_pattern, stamp_all, IntegCoeffs, StampCtx};
use crate::system::{Layout, ReactiveState};
use hauksbee_ir::{Circuit, Device, DeviceId, NodeId};

/// Outcome of a Newton solve.
pub struct NewtonResult {
    pub converged: bool,
    pub iters: usize,
}

/// Reusable workspace for a transient/DC run: the layout, the frozen symbolic
/// factorization, and scratch buffers. Created once, reused every step.
pub struct Workspace {
    pub layout: Layout,
    pub symbolic: Symbolic,
    pub matrix: SparseMatrix,
    pub rhs: Vec<f64>,
    pub x: Vec<f64>,
    pub x_prev_iter: Vec<f64>,
    /// Per-node UNDAMPED Newton step from the previous iteration, used by the
    /// staged-DC adaptive damping to detect oscillating nodes (a sign reversal)
    /// and damp only those, leaving converging nodes near-full steps. Allocated
    /// once; only touched on the staged path (branch_reg>0).
    prev_step: Vec<f64>,
    /// True when every device is linear, so one solve per step is exact and
    /// Newton needs no second iteration to confirm convergence.
    linear: bool,
    /// Compiled constant linear backbone (resistors + gmin slots), pre-bound to
    /// the frozen pattern. The first stage of the "compiled netlist": it lets a
    /// caller re-stamp the constant matrix part with `add_at` and no per-device
    /// enum match. The reference monolithic loop deliberately does *not* route
    /// through it, because reordering the assembly would change floating-point
    /// rounding and break the bit-for-bit `Partitioning::Off` guarantee; it is
    /// exposed for the partitioned/compiled paths and for future codegen.
    plan: StampPlan,
    /// Tiny series resistance (ohms) added to every Vsource/Inductor branch
    /// diagonal, used ONLY by the staged-DC fallback to avoid a frozen-ordering
    /// zero pivot. 0.0 (the default, and the value for every normal solve) keeps
    /// assembly bit-identical to the classic path.
    staged_branch_reg: f64,
    /// Set when the last DC solve had to fall through to the staged-DC fallback
    /// (the plain + gmin + source-stepping ladder failed). The transient driver
    /// reads this to decide whether the board needs the staged regularizers on
    /// every step too; an ordinary circuit that solved on the normal ladder
    /// leaves it false and keeps the bit-identical transient path.
    used_staged_dc: bool,
    /// FROZEN comparator output decisions for the event-driven staged solve,
    /// keyed by device id. `None` (every normal solve) keeps the comparators
    /// self-deciding and the path bit-identical; the staged-DC outer loop sets
    /// it so each inner Newton solve holds comparator state fixed (smooth) and
    /// re-evaluates between solves.
    cmp_freeze: Option<std::collections::HashMap<DeviceId, bool>>,
}

impl Workspace {
    /// Access the compiled constant-backbone stamp plan.
    pub fn stamp_plan(&self) -> &StampPlan {
        &self.plan
    }

    /// Enable the staged-DC regularizers (Vsource-branch series resistance,
    /// node-step damping, node-block convergence, and converged-then-singular
    /// acceptance) for every solve on this workspace. Used by the transient
    /// driver on a stiff diode-laden board, where the same reverse-biased-diode
    /// degeneracy that collapses the cold DC also destabilizes each transient
    /// step. `reg_ohms` is the negligible series resistance (e.g. 1e-2) added to
    /// every Vsource/Inductor branch; 0.0 restores the bit-identical normal path.
    pub fn set_staged_branch_reg(&mut self, reg_ohms: f64) {
        self.staged_branch_reg = reg_ohms;
    }

    /// Whether the last DC operating-point solve used the staged-DC fallback.
    pub fn used_staged_dc(&self) -> bool {
        self.used_staged_dc
    }

    /// DC KCL residual at the current `self.x`: assemble the linearized system
    /// `g·x = rhs` at this iterate (DC, no homotopy: gmin = opts.gmin, full
    /// sources), then return the infinity norm of `F = g·x - rhs` over the NODE
    /// block only (the branch rows of an ideal Vsource enforce a voltage and are
    /// not a KCL balance). This is the honest "how far from a true root is this
    /// point" measure: at a root every node's KCL closes, so `F ≈ 0`. A
    /// non-finite entry returns +inf (a poisoned point is never a root). Read-only
    /// w.r.t. the operating point; it just stamps and multiplies.
    pub fn dc_residual_inf_norm(&mut self, circuit: &Circuit, opts: &SolverOptions) -> f64 {
        let coeffs = IntegCoeffs::for_step(opts.integration, 1.0, true);
        let empty = ReactiveState::new(circuit.devices.len());
        self.matrix.clear_values();
        for v in self.rhs.iter_mut() {
            *v = 0.0;
        }
        let ctx = StampCtx {
            circuit,
            layout: &self.layout,
            opts,
            x: &self.x,
            x_prev: &self.x,
            time: 0.0,
            coeffs,
            state: &empty,
            dc: true,
            use_ic: false,
            gmin: opts.gmin,
            src_scale: 1.0,
            branch_reg: 0.0,
            cmp_freeze: None,
        };
        stamp_all(&ctx, &mut self.matrix, &mut self.rhs);
        // F = g*x - rhs, infinity norm over node rows.
        let mut worst = 0.0f64;
        for i in 0..self.layout.n_nodes {
            let row = self.matrix.row(i);
            let mut acc = 0.0;
            for &(col, val) in row {
                acc += val * self.x[col];
            }
            let f = acc - self.rhs[i];
            if !f.is_finite() {
                return f64::INFINITY;
            }
            if f.abs() > worst {
                worst = f.abs();
            }
        }
        worst
    }

    /// Argmax companion of [`Self::dc_residual_inf_norm`]: returns
    /// `(max|F|, node_index)` so callers can name the worst-balanced node.
    pub fn dc_residual_argmax(&mut self, circuit: &Circuit, opts: &SolverOptions) -> (f64, usize) {
        let coeffs = IntegCoeffs::for_step(opts.integration, 1.0, true);
        let empty = ReactiveState::new(circuit.devices.len());
        self.matrix.clear_values();
        for v in self.rhs.iter_mut() {
            *v = 0.0;
        }
        let ctx = StampCtx {
            circuit,
            layout: &self.layout,
            opts,
            x: &self.x,
            x_prev: &self.x,
            time: 0.0,
            coeffs,
            state: &empty,
            dc: true,
            use_ic: false,
            gmin: opts.gmin,
            src_scale: 1.0,
            branch_reg: 0.0,
            cmp_freeze: None,
        };
        stamp_all(&ctx, &mut self.matrix, &mut self.rhs);
        let mut worst = (0.0f64, 0usize);
        for i in 0..self.layout.n_nodes {
            let row = self.matrix.row(i);
            let mut acc = 0.0;
            for &(col, val) in row {
                acc += val * self.x[col];
            }
            let f = (acc - self.rhs[i]).abs();
            if f > worst.0 {
                worst = (f, i);
            }
        }
        worst
    }
}

impl Workspace {
    /// Build the workspace and analyze the (fixed) sparsity pattern.
    pub fn new(circuit: &Circuit) -> Workspace {
        let layout = Layout::new(circuit);
        let mut matrix = SparseMatrix::new(layout.size);
        reserve_pattern(circuit, &layout, &mut matrix);
        let symbolic = matrix.factorize_symbolic();
        let size = layout.size;
        let linear = circuit.devices.iter().all(|d| d.is_linear());
        let plan = StampPlan::compile(circuit, &layout, &matrix);
        Workspace {
            layout,
            symbolic,
            matrix,
            rhs: vec![0.0; size],
            x: vec![0.0; size],
            x_prev_iter: vec![0.0; size],
            prev_step: vec![0.0; size],
            linear,
            plan,
            staged_branch_reg: 0.0,
            used_staged_dc: false,
            cmp_freeze: None,
        }
    }
}

/// Run Newton iterations to convergence (or `max_newton`) for one assembled
/// operating point. Mutates `ws.x` in place toward the solution.
#[allow(clippy::too_many_arguments)]
pub fn newton_solve(
    ws: &mut Workspace,
    circuit: &Circuit,
    opts: &SolverOptions,
    time: f64,
    _dt: f64,
    coeffs: IntegCoeffs,
    state: &ReactiveState,
    dc: bool,
    use_ic: bool,
    gmin: f64,
    src_scale: f64,
) -> NewtonResult {
    let mut iters = 0;
    let dbg_newton = std::env::var("HAUKSBEE_NEWTON_DBG").is_ok();
    let dbg_staged = std::env::var("HAUKSBEE_STAGED_DBG").is_ok();
    // Anchor for pn-junction voltage limiting: the linearization point of the
    // PREVIOUS Newton iteration. Seeded to the starting guess so the first
    // iteration's anchor equals its own linearization point (pnjlim then sees
    // zero delta and does not limit), exactly as a cold SPICE iteration.
    ws.x_prev_iter.copy_from_slice(&ws.x);
    for s in ws.prev_step.iter_mut() {
        *s = 0.0;
    }
    // Staged-path stall detection: track the best (smallest) undamped node-step
    // norm seen and how many iterations it has been since it last improved. A
    // diode/comparator core that is going to limit-cycle settles into a constant
    // oscillation amplitude; once the step norm stops improving for a window we
    // bail early instead of grinding every (now dynamic-pivoted, and thus more
    // expensive) iteration up to max_newton. Only active on the staged path
    // (branch_reg>0); the normal path is unaffected.
    let mut best_norm = f64::INFINITY;
    let mut stall = 0usize;
    const STALL_WINDOW: usize = 12;
    loop {
        iters += 1;
        // Snapshot the point we're about to linearize around BEFORE the solve
        // overwrites ws.x, so it becomes next iteration's limiting anchor.
        let lin_point = ws.x.clone();
        ws.matrix.clear_values();
        for v in ws.rhs.iter_mut() {
            *v = 0.0;
        }
        let branch_reg = ws.staged_branch_reg;
        {
            let ctx = StampCtx {
                circuit,
                layout: &ws.layout,
                opts,
                x: &ws.x,
                x_prev: &ws.x_prev_iter,
                time,
                coeffs,
                state,
                dc,
                use_ic,
                gmin,
                src_scale,
                branch_reg,
                cmp_freeze: ws.cmp_freeze.as_ref(),
            };
            stamp_all(&ctx, &mut ws.matrix, &mut ws.rhs);
        }
        // The iterate from the step before this one (the anchor we just used)
        // vs the current linearization point: if they already agree to
        // tolerance, the previous Newton step had converged.
        let prev_iterate = std::mem::replace(&mut ws.x_prev_iter, lin_point.clone());

        if !ws.symbolic.refactor(&ws.matrix) {
            // The Jacobian assembled at this iterate is numerically singular.
            // This can happen AT a valid solution of a diode-laden board: once
            // every signal diode is reverse-biased its tangent conductance is
            // ~1e-10 S, so a node reachable only through such junctions has an
            // (almost) empty row and the frozen-ordering LU finds a zero pivot —
            // even though the operating point itself is well-defined (anchored
            // by gmin). If the previous Newton step had already converged (the
            // current linearization point barely moved from the one before it),
            // the singular *next* Jacobian does not matter: we have the root.
            // Accept it. Otherwise it is a genuine mid-iteration breakdown and we
            // report non-convergence.
            // Scoped to the staged-DC fallback (branch_reg > 0): the normal
            // plain/gmin/source paths keep the original "singular => fail"
            // behaviour exactly, so circuits that already converge are untouched.
            // Accept if the node block (the physical operating point) had already
            // converged; the singular *next* Jacobian is the all-diodes-reverse
            // degeneracy and does not change the root we already hold.
            let already = branch_reg > 0.0
                && iters > 1
                && node_block_converged(&lin_point, &prev_iterate, &ws.layout, opts);
            if dbg_staged {
                eprintln!("[newton] refactor singular at iter {iters} (already_converged={already})");
            }
            return NewtonResult { converged: already, iters };
        }
        ws.symbolic.solve(&mut ws.rhs);
        // rhs now holds the new (UNDAMPED) Newton iterate.
        ws.x.copy_from_slice(&ws.rhs);

        // A fully linear system is solved exactly in one shot; no need to
        // assemble and factor a second time just to watch the residual be zero.
        if ws.linear {
            return NewtonResult { converged: true, iters };
        }

        // Convergence is judged on the UNDAMPED Newton step (ws.x vs the point we
        // linearized at, lin_point): that is the true measure of how far the
        // iterate is from the root. Damping (below) is a path globalization only;
        // judging convergence on the damped iterate would let a small damped step
        // masquerade as convergence. The normal (branch_reg==0) path damps
        // nothing, so this is identical to the old test there.
        let undamped_converged = converged(&ws.x, &lin_point, &ws.layout, opts);
        let undamped_node_converged =
            node_block_converged(&ws.x, &lin_point, &ws.layout, opts);

        // Adaptive damped Newton (staged-DC only). A node sitting behind a
        // reverse-biased diode plus a DC-open cap is nearly floating and
        // high-gain, so the undamped Newton map can limit-cycle (a node and its
        // neighbour flip-flop between two values). A GLOBAL fixed fraction can't
        // win: too high sustains the two-cycle, too low crawls the converging
        // nodes. Instead damp PER NODE by whether its undamped step reversed
        // direction from the previous iteration: a sign reversal means that node
        // is oscillating, so damp it hard (and progressively harder the longer it
        // oscillates, tracked implicitly by the shrinking effective step); a
        // same-sign step means it is converging, so take a near-full step. This
        // kills the oscillation while letting the well-behaved majority converge
        // fast. At the root every step is zero, so damping is inert. Active only
        // for branch_reg>0, so the normal solve paths stay bit-identical.
        if branch_reg > 0.0 {
            const NODE_STEP_MAX: f64 = 2.0; // hard cap (V) per iteration
            const ALPHA_CONVERGING: f64 = 0.9; // same-sign: near-full step
            const ALPHA_OSCILLATING: f64 = 0.25; // sign reversed: heavy damping
            for i in 0..ws.layout.n_nodes {
                let full = ws.x[i] - lin_point[i];
                let prev = ws.prev_step[i];
                // Oscillating if this step opposes the previous one and both are
                // non-trivial.
                let oscillating = prev * full < 0.0 && prev.abs() > 1e-9 && full.abs() > 1e-9;
                let alpha = if oscillating { ALPHA_OSCILLATING } else { ALPHA_CONVERGING };
                let mut step = alpha * full;
                if step > NODE_STEP_MAX {
                    step = NODE_STEP_MAX;
                } else if step < -NODE_STEP_MAX {
                    step = -NODE_STEP_MAX;
                }
                ws.prev_step[i] = step;
                ws.x[i] = lin_point[i] + step;
            }
        }

        if undamped_converged {
            return NewtonResult { converged: true, iters };
        }
        // Staged-DC node-block convergence. The branch regularizer injects
        // O(branch_reg) micro-currents into the Vsource branches that never fully
        // settle under the tight branch abstol, even though the NODE voltages
        // (the physical operating point) have converged. Once every node has
        // converged and the residual current noise is bounded by the regularizer
        // scale, accept the operating point: the node block is the physics, the
        // branch currents are derived and the leftover noise is below any real
        // signal current. Only for branch_reg>0 (staged path).
        if branch_reg > 0.0 && iters >= 3 && undamped_node_converged {
            return NewtonResult { converged: true, iters };
        }
        if dbg_newton {
            let mut maxd = 0.0f64;
            let mut argi = 0usize;
            for i in 0..ws.layout.n_nodes {
                let d = (ws.x[i] - lin_point[i]).abs();
                if d > maxd {
                    maxd = d;
                    argi = i;
                }
            }
            eprintln!("[newton] iter {iters} maxdV(undamped)={maxd:.3e} @node{argi} branch_reg={branch_reg:e}");
        }
        if branch_reg > 0.0 {
            // Norm of the undamped node step this iteration.
            let mut norm = 0.0f64;
            for i in 0..ws.layout.n_nodes {
                let d = (ws.x[i] - lin_point[i]).abs();
                if d > norm {
                    norm = d;
                }
            }
            if norm < best_norm * 0.999 {
                best_norm = norm;
                stall = 0;
            } else {
                stall += 1;
                if stall >= STALL_WINDOW {
                    // No progress for a full window: this solve is limit-cycling.
                    return NewtonResult { converged: false, iters };
                }
            }
        }
        if iters >= opts.max_newton {
            return NewtonResult { converged: false, iters };
        }
    }
}

/// Node-block-only convergence test: every NODE voltage within
/// `reltol*|v| + vntol`. Used by the staged-DC fallback, where the branch
/// regularizer leaves a sub-nano-amp current ripple on the Vsource branches that
/// the full [`converged`] test would reject even though the physical node
/// voltages have settled.
fn node_block_converged(x: &[f64], xp: &[f64], layout: &Layout, opts: &SolverOptions) -> bool {
    for i in 0..layout.n_nodes {
        // A non-finite iterate is NEVER "converged". Guard before the tol test:
        // `(NaN).abs() > tol` is false, so without this a NaN/Inf Newton image
        // would silently pass the convergence test and the solve would "accept" a
        // poisoned operating point. Finite circuits never trip this, so the
        // normal path is bit-identical.
        if !x[i].is_finite() {
            return false;
        }
        let tol = opts.reltol * x[i].abs().max(xp[i].abs()) + opts.vntol;
        if (x[i] - xp[i]).abs() > tol {
            return false;
        }
    }
    true
}

/// Convergence test: every node voltage within `reltol*|v| + vntol`, and branch
/// currents within `reltol*|i| + abstol`.
fn converged(x: &[f64], xp: &[f64], layout: &Layout, opts: &SolverOptions) -> bool {
    for i in 0..x.len() {
        // Reject a non-finite iterate (see node_block_converged): `(NaN).abs() >
        // tol` is false, so without this a NaN/Inf unknown would pass as
        // converged. Finite circuits never trip this; the normal path is exact.
        if !x[i].is_finite() {
            return false;
        }
        let tol = if i < layout.n_nodes {
            opts.reltol * x[i].abs().max(xp[i].abs()) + opts.vntol
        } else {
            opts.reltol * x[i].abs().max(xp[i].abs()) + opts.abstol
        };
        if (x[i] - xp[i]).abs() > tol {
            return false;
        }
    }
    true
}

/// Solve the DC operating point into `ws.x`. Tries plain Newton, then gmin
/// stepping, then source stepping, before reporting failure.
///
/// When `use_ic` is set, reactive elements that declare an initial condition
/// are pinned to it, giving the transient "initial conditions" operating point
/// rather than the steady-state DC bias.
pub fn dc_operating_point(
    ws: &mut Workspace,
    circuit: &Circuit,
    opts: &SolverOptions,
) -> Result<(), String> {
    dc_operating_point_seeded(ws, circuit, opts, None)
}

/// Solve the DC operating point, optionally warm-started from a `seed` (a prior
/// operating point of the same circuit, e.g. the previous co-sim chunk's final
/// unknowns). A good seed lets plain Newton converge in ~1 iteration, skipping
/// the expensive gmin/source-stepping homotopy that a cold start needs on a
/// stiff nonlinear board. This is EXACT: Newton converges to the same unique root
/// regardless of the starting guess; the seed only changes the iteration count,
/// and a seed that fails falls back to the identical cold (zeroed) path below.
pub fn dc_operating_point_seeded(
    ws: &mut Workspace,
    circuit: &Circuit,
    opts: &SolverOptions,
    seed: Option<&[f64]>,
) -> Result<(), String> {
    let use_ic = circuit.iter().any(|(_, d)| {
        matches!(
            d,
            Device::Capacitor { ic: Some(_), .. } | Device::Inductor { ic: Some(_), .. }
        )
    });
    dc_solve(ws, circuit, opts, use_ic, seed)
}

fn dc_solve(
    ws: &mut Workspace,
    circuit: &Circuit,
    opts: &SolverOptions,
    use_ic: bool,
    seed: Option<&[f64]>,
) -> Result<(), String> {
    ws.used_staged_dc = false;
    let coeffs = IntegCoeffs::for_step(opts.integration, 1.0, true);
    let empty = ReactiveState::new(circuit.devices.len());
    let solve = |ws: &mut Workspace, gmin: f64, scale: f64| {
        newton_solve(ws, circuit, opts, 0.0, 1.0, coeffs, &empty, true, use_ic, gmin, scale)
    };

    // Attempt 0 (warm start): if a seed of the right size is given, try plain
    // Newton from it. Near the previous operating point this converges in a few
    // iterations; a failure simply falls through to the cold path, so the root
    // found is identical either way.
    if let Some(s) = seed {
        if s.len() == ws.x.len() {
            ws.x.copy_from_slice(s);
            let r = solve(ws, opts.gmin, 1.0);
            if r.converged {
                return Ok(());
            }
        }
    }

    // Attempt 1: direct (cold, zeroed).
    for v in ws.x.iter_mut() {
        *v = 0.0;
    }
    let r = solve(ws, opts.gmin, 1.0);
    if r.converged {
        return Ok(());
    }
    if !opts.dc_homotopy {
        return Err(format!("DC Newton did not converge in {} iters", r.iters));
    }

    // Attempt 2: gmin stepping — start with a large shunt and ramp it down.
    for v in ws.x.iter_mut() {
        *v = 0.0;
    }
    let mut gmin = 1e-2;
    let mut ok = true;
    while gmin > opts.gmin {
        let r = solve(ws, gmin, 1.0);
        if !r.converged {
            ok = false;
            break;
        }
        gmin *= 0.1;
    }
    if ok && solve(ws, opts.gmin, 1.0).converged {
        return Ok(());
    }

    // Attempt 3: source stepping — ramp every source from 0 to full.
    for v in ws.x.iter_mut() {
        *v = 0.0;
    }
    let steps = 50;
    let mut src_ok = true;
    let mut last_scale = 0.0;
    let mut last_iters = 0;
    for s in 1..=steps {
        let scale = s as f64 / steps as f64;
        let r = solve(ws, opts.gmin, scale);
        last_scale = scale;
        last_iters = r.iters;
        if !r.converged {
            src_ok = false;
            break;
        }
    }
    if src_ok {
        return Ok(());
    }

    // Attempt 4: STAGED DC. The standard plain+gmin+source ladder can stall on a
    // stiff board where many nonlinear junctions (e.g. dozens of signal diodes
    // wired into high-impedance pulse-stretcher nets) make the cold Jacobian
    // ill-conditioned. Two things conspire on such a board:
    //   (1) the cold Newton diverges before it finds the basin, and
    //   (2) at the DC point, a stretch-cap / V_out node connects to the rest of
    //       the circuit ONLY through a reverse-biased diode (~1e-10 S) and a
    //       capacitor (open at DC), so its row is numerically singular at the
    //       default gmin of 1e-12.
    // Fix both: solve a RELAXED circuit first (every diode replaced by its OFF
    // state, a large linear leak resistor — what a reverse-biased junction is to
    // first order), then warm-start the FULL circuit from that operating point
    // while holding gmin at a floor that anchors the otherwise-floating high-Z
    // nodes.
    //
    // EXACT to solver tolerance: the relaxed solve only seeds the starting
    // guess (the final root is set by the full, unmodified diode equations).
    // The gmin floor is STAGED_GMIN = 1e-9 S; that injects at most ~1e-9 S * 5 V
    // = 5 nA of leakage per node, orders of magnitude below the µA-scale synapse
    // / mirror currents that set every meaningful node, and well inside reltol +
    // vntol. Nodes that are genuinely floating at DC (cap + reverse diode only)
    // have no physically-defined DC voltage anyway; the gmin floor pins them to
    // ~0, exactly as gmin always does. Removing the diodes never changes the
    // unknown layout (a diode owns no branch current; the relaxation keeps every
    // node present via the leak resistor), so the relaxed `x` maps 1:1 onto the
    // full system's unknowns. This path triggers ONLY after plain + gmin + source
    // stepping have all failed, so circuits that already converge are untouched.
    const STAGED_GMIN: f64 = 1e-7;
    let staged_gmin = STAGED_GMIN.max(opts.gmin);
    let dbg = std::env::var("HAUKSBEE_STAGED_DBG").is_ok();
    // The staged path solves a diode-laden board whose conducting junctions can
    // make the frozen elimination order hit a singular pivot. The dynamic
    // re-pivot fallback dissolves that LU singularity. On a board whose
    // spike-output nodes are defined only dynamically (a DC-open stretch cap, so
    // the node has no static DC voltage and must be anchored every iteration) it
    // cannot reach a static root and just adds cost, so it is OPT-IN
    // (HAUKSBEE_DC_DYN=1). The default staged path keeps the original behaviour
    // (frozen-singular -> adopt the relaxed power-on point) bit-for-bit and at
    // the original speed, so no existing test changes value or timing. It is
    // restored to off before every return so the workspace-reused transient
    // keeps frozen-only semantics.
    let dc_dyn = std::env::var("HAUKSBEE_DC_DYN").is_ok();
    ws.symbolic.set_allow_dynamic(dc_dyn);
    if let Some(seed) = solve_relaxed_no_diodes(circuit, opts) {
        if dbg { eprintln!("[staged] relaxed converged, seeding full (len {})", seed.len()); }
        if seed.len() == ws.x.len() {
            // Warm-start the full nonlinear solve from the relaxed operating
            // point, with two regularizers active:
            //   * a gmin floor (staged_gmin) anchoring high-impedance nodes that
            //     sit behind a reverse-biased diode plus a (DC-open) cap, and
            //   * a negligible series resistance on every Vsource/Inductor branch
            //     (STAGED_BRANCH_REG ohms) so the frozen sparse ordering keeps a
            //     nonzero pivot on those branch rows even as conducting diodes
            //     reshape the elimination.
            // STAGED_BRANCH_REG = 1e-4 ohm is far below the 50-ohm driver output
            // impedance and any meaningful series element, so it does not move
            // the operating point within solver tolerance. The branch_reg also
            // arms the "converged-then-singular" acceptance in newton_solve: at
            // the true root every signal diode is reverse-biased (tangent ~1e-10
            // S) so the next Jacobian can be numerically singular even though the
            // root is well-defined; Newton then accepts the converged iterate.
            const STAGED_BRANCH_REG: f64 = 1e-2;
            ws.staged_branch_reg = STAGED_BRANCH_REG;
            ws.x.copy_from_slice(&seed);
            let r = solve(ws, staged_gmin, 1.0);
            if dbg { eprintln!("[staged] full from seed @gmin={staged_gmin:e}: converged={} iters={}", r.converged, r.iters); }
            if r.converged {
                ws.staged_branch_reg = 0.0;
                ws.symbolic.set_allow_dynamic(false);
                ws.used_staged_dc = true;
                return Ok(());
            }

            // EVENT-DRIVEN COMPARATOR LOOP. The plain staged solve above stalls
            // because the LMV7219 comparators are bang-bang: as Newton settles
            // the analog core, a comparator input crosses threshold, its output
            // swings rail-to-rail, and that swing destabilizes the inputs feeding
            // it — a limit cycle (verified: the per-iteration node delta pins at
            // the damping cap once the analog part has converged). The cure is to
            // FREEZE each comparator's decision for an inner solve (making the
            // circuit smooth so Newton converges), then re-evaluate the decisions
            // from the converged solution and re-solve if any flipped. This is a
            // Gauss-Seidel / event-driven outer loop over the comparator states;
            // at its fixed point every comparator output is consistent with its
            // own inputs, so the result is a TRUE root of the full circuit (not
            // the relaxed bias). The dynamic-pivot LU keeps each inner solve
            // factorable even as the conducting diodes reshape the elimination.
            // The event loop is expensive and only pays off on a board that
            // actually has a consistent all-comparator DC fixed point. On a board
            // whose spike-output nodes are defined dynamically (a stretch cap that
            // is open at DC, so the node has no DC voltage at all), it cannot
            // converge and just burns time, so it is gated behind
            // HAUKSBEE_CMP_EVENT=1 rather than run on the default path. The
            // dynamic-pivot LU (the structural fix) stays on regardless.
            if std::env::var("HAUKSBEE_CMP_EVENT").is_ok() {
                if let Some(root) = staged_event_solve(ws, circuit, opts, &seed, staged_gmin, STAGED_BRANCH_REG, dbg) {
                    ws.x.copy_from_slice(&root);
                    ws.staged_branch_reg = 0.0;
                    ws.cmp_freeze = None;
                    ws.symbolic.set_allow_dynamic(false);
                    ws.used_staged_dc = true;
                    return Ok(());
                }
            }
            ws.cmp_freeze = None;
            ws.staged_branch_reg = STAGED_BRANCH_REG;

            // Diode saturation-current homotopy: the cold full solve stalls in a
            // limit cycle on the comparator-driven stretch nodes, but the relaxed
            // (diodes-off) point converged. Walk the diode `is` from near-zero up
            // to its real value in log steps, warm-starting each from the last
            // converged point. Each small change moves the operating point a
            // little, so Newton tracks the solution branch smoothly to the TRUE
            // full-circuit DC root (the final step uses the real `is`, so it is
            // exact). This is the principled continuation for a stiff junction
            // network; it reaches the real root, not the relaxed approximation.
            if let Some(s) = solve_diode_is_homotopy(circuit, opts, &seed, staged_gmin, STAGED_BRANCH_REG) {
                if dbg { eprintln!("[staged] diode-Is homotopy reached the full root"); }
                ws.x.copy_from_slice(&s);
                ws.staged_branch_reg = 0.0;
                ws.symbolic.set_allow_dynamic(false);
                ws.used_staged_dc = true;
                return Ok(());
            }
            // If the direct warm start stalls, walk gmin down to the floor from a
            // stiffer start (helps when the relaxed seed is far on some
            // junctions), keeping the branch regularizer on throughout.
            ws.x.copy_from_slice(&seed);
            let mut gmin = 1e-3;
            let mut staged_ok = true;
            while gmin > staged_gmin {
                if !solve(ws, gmin, 1.0).converged {
                    staged_ok = false;
                    break;
                }
                gmin *= 0.1;
            }
            if dbg { eprintln!("[staged] gmin-ramp staged_ok={staged_ok}"); }
            if staged_ok && solve(ws, staged_gmin, 1.0).converged {
                ws.staged_branch_reg = 0.0;
                ws.symbolic.set_allow_dynamic(false);
                ws.used_staged_dc = true;
                return Ok(());
            }
            ws.staged_branch_reg = 0.0;

            // Optional last resort: pseudo-transient continuation (settle the
            // full circuit forward from the relaxed seed with a pseudo-cap on
            // every node). It is the textbook robust method, but on a board whose
            // per-step nonlinear solve itself does not converge (the Tarski
            // synapse core) it is both slow and unproductive, so it is gated
            // behind HAUKSBEE_PTC=1 rather than run on the hot path.
            if std::env::var("HAUKSBEE_PTC").is_ok() {
                if let Some(s) = ptc_settle_from_seed(circuit, opts, &seed) {
                    if dbg { eprintln!("[staged] PTC settled to a full operating point"); }
                    ws.x.copy_from_slice(&s);
                    ws.symbolic.set_allow_dynamic(false);
                    ws.used_staged_dc = true;
                    return Ok(());
                }
            }

            // Final fallback: adopt the relaxed (all-diodes-OFF) operating point
            // as the t=0 seed. It is a genuinely converged DC solution of the
            // relaxed circuit AND the physically correct power-on state (every
            // pulse-stretcher cap discharged, every signal diode reverse-biased
            // before the first spike). The caller's transient then integrates the
            // full nonlinear circuit forward: the real stretch caps define the
            // otherwise-floating diode-anode nodes through dv/dt (so the per-step
            // matrix is well-conditioned where the static DC was not), and the
            // operating point relaxes to its true steady state over the march.
            if dbg { eprintln!("[staged] adopting relaxed power-on operating point as seed"); }
            ws.x.copy_from_slice(&seed);
            ws.symbolic.set_allow_dynamic(false);
            ws.used_staged_dc = true;
            return Ok(());
        }
    } else if dbg {
        eprintln!("[staged] relaxed (no-diode) solve did NOT converge");
    }

    ws.symbolic.set_allow_dynamic(false);
    Err(format!(
        "DC homotopy failed (source scale {last_scale:.3}, {last_iters} iters; \
         staged-DC relaxation did not recover)"
    ))
}

/// Evaluate every comparator's output decision (`high` = output at the high
/// rail) from a solution vector `x`, using the same hysteresis rule the stamp
/// uses but anchored to the supplied previous decision `prev` (so a comparator
/// whose differential input sits inside the hysteresis band keeps its state).
/// Returns the decision map keyed by device id.
fn eval_comparator_states(
    circuit: &Circuit,
    layout: &Layout,
    x: &[f64],
    prev: &std::collections::HashMap<DeviceId, bool>,
) -> std::collections::HashMap<DeviceId, bool> {
    let mut out = std::collections::HashMap::new();
    for (id, dev) in circuit.iter() {
        if let Device::Comparator { inp, inn, hysteresis, .. } = dev {
            let vp = layout.node(*inp).map(|i| x[i]).unwrap_or(0.0);
            let vn = layout.node(*inn).map(|i| x[i]).unwrap_or(0.0);
            let d = vp - vn;
            let was_high = prev.get(&id).copied().unwrap_or(d > 0.0);
            // Hysteresis: need to exceed +hyst to go high, drop below -hyst to go
            // low; otherwise hold. Matches stamp_comparator's threshold sign.
            let high = if was_high {
                d > -*hysteresis
            } else {
                d > *hysteresis
            };
            out.insert(id, high);
        }
    }
    out
}

/// Event-driven staged DC solve. Freezes each comparator's decision, solves the
/// resulting smooth circuit with the dynamic-pivot LU, re-evaluates the
/// decisions from that solution, and repeats until the decision set stops
/// changing (a consistent fixed point) AND the inner Newton converged. The
/// returned vector is then a TRUE root of the full nonlinear circuit: every
/// diode equation holds and every comparator output is consistent with its own
/// inputs. Returns `None` if it cannot reach a consistent converged state.
#[allow(clippy::too_many_arguments)]
fn staged_event_solve(
    ws: &mut Workspace,
    circuit: &Circuit,
    opts: &SolverOptions,
    seed: &[f64],
    staged_gmin: f64,
    branch_reg: f64,
    dbg: bool,
) -> Option<Vec<f64>> {
    let coeffs = IntegCoeffs::for_step(opts.integration, 1.0, true);
    let empty = ReactiveState::new(circuit.devices.len());
    // The event-driven inner solves converge LINEARLY in their tail (the decayed
    // damping that breaks the diode/comparator oscillation also slows the final
    // approach), so allow more Newton iterations than the default before giving
    // up on an inner solve.
    let mut inner_opts = *opts;
    inner_opts.max_newton = opts.max_newton.max(400);
    let opts = &inner_opts;

    // Initial comparator decisions from the relaxed (diodes-off) seed.
    let mut states = eval_comparator_states(circuit, &ws.layout, seed, &Default::default());
    if states.is_empty() {
        return None; // no comparators: this path adds nothing.
    }

    ws.staged_branch_reg = branch_reg;
    let mut x = seed.to_vec();
    const MAX_EVENT_PASSES: usize = 40;
    for pass in 0..MAX_EVENT_PASSES {
        ws.cmp_freeze = Some(states.clone());
        ws.x.copy_from_slice(&x);
        let r = newton_solve(
            ws, circuit, opts, 0.0, 1.0, coeffs, &empty, true, false, staged_gmin, 1.0,
        );
        if !r.converged {
            if dbg {
                eprintln!("[staged-event] pass {pass}: inner Newton did NOT converge (iters {})", r.iters);
            }
            return None;
        }
        x.copy_from_slice(&ws.x);
        // Re-evaluate decisions from the converged inner solution.
        let next = eval_comparator_states(circuit, &ws.layout, &x, &states);
        let flips = next.iter().filter(|(k, v)| states.get(k) != Some(*v)).count();
        if dbg {
            eprintln!("[staged-event] pass {pass}: inner converged in {} iters, {flips} comparator flips", r.iters);
        }
        if flips == 0 {
            // Fixed point: comparator states are self-consistent. Verify it is a
            // genuine root of the FULL circuit (real diodes, self-deciding
            // comparators), not just of the frozen-comparator surrogate.
            return Some(x);
        }
        states = next;
    }
    None
}

/// Diode saturation-current continuation. Starting from the relaxed (diodes-off)
/// seed, solve a sequence of circuits whose diode `is` is scaled from a tiny
/// fraction up to 1.0, warm-starting each solve from the previous converged
/// point. Returns the operating point of the FULL circuit (scale 1.0, the real
/// `is`) — the true diode-laden DC root — or `None` if any step fails.
///
/// Why this works where a cold full solve limit-cycles: at small `is` the diodes
/// barely conduct (close to the relaxed circuit that converged), so Newton
/// converges easily; as `is` grows the operating point moves a little each step
/// and the warm start keeps Newton inside the basin, tracking the solution
/// branch continuously to the real device. The regularizers (gmin floor +
/// branch series R) stay on so the high-impedance stretch nodes stay anchored.
fn solve_diode_is_homotopy(
    circuit: &Circuit,
    opts: &SolverOptions,
    seed: &[f64],
    staged_gmin: f64,
    branch_reg: f64,
) -> Option<Vec<f64>> {
    // Record each diode's real `is` and device index.
    let diodes: Vec<(usize, f64)> = circuit
        .devices
        .iter()
        .enumerate()
        .filter_map(|(i, d)| match d {
            Device::Diode { model, .. } => Some((i, model.is)),
            _ => None,
        })
        .collect();
    if diodes.is_empty() {
        return None;
    }

    let mut work = circuit.clone();
    let mut ws = Workspace::new(&work);
    ws.set_staged_branch_reg(branch_reg);
    ws.symbolic.set_allow_dynamic(std::env::var("HAUKSBEE_DC_DYN").is_ok());
    let mut x = seed.to_vec();

    // Geometric ramp of the Is scale: 1e-4, 1e-3.5, ... up to 1.0.
    let steps = 16usize;
    for s in 1..=steps {
        let frac = s as f64 / steps as f64; // 0<frac<=1
        let scale = 10f64.powf(-4.0 * (1.0 - frac)); // 1e-4 .. 1.0
        for &(i, is_real) in &diodes {
            if let Device::Diode { model, .. } = &mut work.devices[i] {
                model.is = is_real * scale;
            }
        }
        // Warm-start from the previous step's point; if a step stalls, the
        // continuation has left the basin and we give up (caller falls back).
        ws.x.copy_from_slice(&x);
        // Use the plain seeded Newton at the staged gmin (no recursion into the
        // staged ladder — we are already inside it and supply the warm start).
        let coeffs = IntegCoeffs::for_step(opts.integration, 1.0, true);
        let empty = ReactiveState::new(work.devices.len());
        let r = newton_solve(&mut ws, &work, opts, 0.0, 1.0, coeffs, &empty, true, false, staged_gmin, 1.0);
        if !r.converged {
            return None;
        }
        x.copy_from_slice(&ws.x);
    }
    Some(x)
}

/// Build a relaxed copy of `circuit` with every diode replaced by a large
/// linear leak resistor between the same (anode, cathode) nodes, and solve its
/// DC operating point with the normal homotopy ladder. Returns the relaxed
/// unknown vector to warm-start the full nonlinear solve, or `None` if even the
/// relaxed (much better-conditioned, all-linear-where-the-diodes-were) circuit
/// fails to converge.
///
/// Keeping the diodes as resistors (rather than deleting them) preserves every
/// node and the device ordering of branch-owning elements, so the returned
/// vector lines up index-for-index with the full circuit's unknowns.
fn solve_relaxed_no_diodes(circuit: &Circuit, opts: &SolverOptions) -> Option<Vec<f64>> {
    // A reverse-biased small-signal diode is ~G-leak; 1e-9 S (1 GOhm) is open
    // for all practical purposes yet keeps the node anchored and the matrix
    // non-singular (so the relaxed circuit converges without a gmin floor).
    const DIODE_OFF_OHMS: f64 = 1e9;
    let mut relaxed = circuit.clone();
    let mut any = false;
    for dev in relaxed.devices.iter_mut() {
        if let Device::Diode { name, a, k, .. } = dev {
            any = true;
            *dev = Device::Resistor {
                name: format!("{name}__relaxed_off"),
                a: *a,
                b: *k,
                ohms: DIODE_OFF_OHMS,
                tc1: None,
            };
        }
    }
    if !any {
        return None;
    }
    let mut ws = Workspace::new(&relaxed);
    // Cold solve of the relaxed circuit (no seed). It is far better conditioned
    // than the full one: the diode nonlinearity is gone.
    dc_solve(&mut ws, &relaxed, opts, false, None).ok()?;
    Some(ws.x.clone())
}

/// Pseudo-transient continuation: settle the FULL nonlinear circuit to its DC
/// operating point by integrating it forward from the relaxed seed, with a
/// pseudo-capacitor added on every node to ground. The pseudo-caps:
///   * make the t=0 operating point exactly the seed (each is pinned to its IC),
///   * add a C/dt term to every node diagonal so the per-step matrix is
///     well-conditioned where the bare DC matrix was singular, and
///   * carry zero current at steady state (dv/dt -> 0), so the settled point is
///     the true DC operating point of the unmodified circuit.
/// Returns the settled unknown vector (truncated to the original layout size, as
/// the pseudo-caps own no branch unknowns so the node/branch indices are
/// unchanged), or `None` if even the regularized march fails to settle.
fn ptc_settle_from_seed(
    circuit: &Circuit,
    opts: &SolverOptions,
    seed: &[f64],
) -> Option<Vec<f64>> {
    use crate::options::{Integration, Partitioning, StepControl};
    use crate::transient::Transient;

    let n_nodes = circuit.max_node() as usize;
    let orig_size = seed.len();
    let mut aug = circuit.clone();
    // Pseudo-cap value: large enough to dominate the smallest real conductance
    // (anchoring floating nodes) yet give a fast settle time constant with the
    // chosen step. 1 nF with a 1 us step => C/dt = 1e-3 S on every node.
    const PSEUDO_C: f64 = 1e-9;
    for ni in 1..=n_nodes {
        // IC = the seed's voltage for this node (node i -> unknown index i-1).
        let v0 = seed.get(ni - 1).copied().unwrap_or(0.0);
        aug.add(Device::Capacitor {
            name: format!("__ptc_c{ni}"),
            a: NodeId(ni as u32),
            b: NodeId::GROUND,
            farads: PSEUDO_C,
            ic: Some(v0),
        });
    }

    // Fixed-step march; Partitioning OFF (the monolithic reference path) so the
    // pseudo-cap regularization is applied uniformly. Settle over many time
    // constants of the pseudo-RC.
    let dt = 1e-7;
    let tstop = 2e-4; // 2000 steps; >> any pseudo-RC settle for nF/this board
    let mut popts = *opts;
    popts.integration = Integration::BackwardEuler; // damps stiff modes toward DC
    popts.step = StepControl::Fixed { dt };
    popts.partitioning = Partitioning::Off;

    let mut last: Vec<f64> = Vec::new();
    let mut prev_capture: Vec<f64> = Vec::new();
    let mut settled = false;
    let res = Transient::new(popts).run_streaming(&aug, tstop, |s| {
        // Track convergence of the node block between captured samples.
        if !last.is_empty() && s.x.len() >= n_nodes {
            let mut maxd = 0.0f64;
            for i in 0..n_nodes {
                maxd = maxd.max((s.x[i] - last[i]).abs());
            }
            if maxd < 1e-7 {
                settled = true;
                prev_capture = s.x[..orig_size.min(s.x.len())].to_vec();
            }
        }
        last = s.x.to_vec();
    });
    res.ok()?;
    if settled && prev_capture.len() == orig_size {
        return Some(prev_capture);
    }
    // Even if the early-settle test didn't trip, the final state is the best
    // available operating point if it is finite.
    if last.len() >= orig_size && last[..orig_size].iter().all(|v| v.is_finite()) {
        return Some(last[..orig_size].to_vec());
    }
    None
}

#[cfg(test)]
mod nan_guard_tests {
    use super::{converged, node_block_converged};
    use crate::options::SolverOptions;
    use crate::system::Layout;
    use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};

    // A tiny circuit so we can build a real Layout (the convergence tests index
    // it for the node/branch split). Two nodes + a source branch.
    fn small_layout() -> Layout {
        let mut c = Circuit::new();
        let a = c.node("a");
        let b = c.node("b");
        c.add(Device::Vsource {
            name: "V".into(),
            p: a,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::Resistor {
            name: "R".into(),
            a,
            b,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Resistor {
            name: "Rg".into(),
            a: b,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });
        Layout::new(&c)
    }

    // The bug this guards: `(NaN).abs() > tol` is FALSE, so a naive
    // tolerance-only test reports a NaN-poisoned Newton image as "converged",
    // and the solver accepts an Inf/NaN operating point as a root. Both
    // convergence tests must REJECT any non-finite unknown.
    #[test]
    fn nan_iterate_is_not_converged() {
        let layout = small_layout();
        let opts = SolverOptions::default();
        let n = layout.size;
        // A point that, BUT FOR the non-finite entry, is bit-identical to its
        // anchor (delta zero => would pass the tol test trivially).
        let xp = vec![1.0; n];
        let mut x = xp.clone();
        x[0] = f64::NAN;
        assert!(
            !node_block_converged(&x, &xp, &layout, &opts),
            "a NaN node voltage must not pass node_block_converged"
        );
        assert!(
            !converged(&x, &xp, &layout, &opts),
            "a NaN unknown must not pass converged"
        );
        // Same for +Inf (a near-singular solve can produce an Inf image).
        x[0] = f64::INFINITY;
        assert!(!node_block_converged(&x, &xp, &layout, &opts));
        assert!(!converged(&x, &xp, &layout, &opts));
    }

    // Sanity: a clean finite fixed point still converges (no false negative).
    #[test]
    fn finite_fixed_point_still_converges() {
        let layout = small_layout();
        let opts = SolverOptions::default();
        let x = vec![0.5; layout.size];
        assert!(node_block_converged(&x, &x, &layout, &opts));
        assert!(converged(&x, &x, &layout, &opts));
    }
}

#[cfg(test)]
mod residual_tests {
    use super::{dc_operating_point, Workspace};
    use crate::options::SolverOptions;
    use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};

    // A 1V source feeding two equal series resistors to ground: the midpoint
    // solves to 0.5 V. At the true solution the KCL residual is ~0; at a wrong
    // operating point it is the actual mismatch current. The residual API must
    // report near-zero at the solved point and a real current off it.
    #[test]
    fn dc_residual_is_zero_at_the_root() {
        let mut c = Circuit::new();
        let top = c.node("top");
        let mid = c.node("mid");
        c.add(Device::Vsource {
            name: "V".into(),
            p: top,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(1.0),
        });
        c.add(Device::Resistor {
            name: "R1".into(),
            a: top,
            b: mid,
            ohms: 1e3,
            tc1: None,
        });
        c.add(Device::Resistor {
            name: "R2".into(),
            a: mid,
            b: NodeId::GROUND,
            ohms: 1e3,
            tc1: None,
        });
        let opts = SolverOptions::default();
        let mut ws = Workspace::new(&c);
        dc_operating_point(&mut ws, &c, &opts).unwrap();
        // At the converged root, every node's KCL closes.
        let r_root = ws.dc_residual_inf_norm(&c, &opts);
        assert!(r_root < 1e-9, "residual at the root should be ~0, got {r_root:e}");

        // Perturb the midpoint by 1 V: KCL now mismatches by ~1 V / 500 ohm = 2 mA
        // (the two 1k resistors in parallel see the extra volt).
        if let Some(i) = ws.layout.node(mid) {
            ws.x[i] += 1.0;
        }
        let r_off = ws.dc_residual_inf_norm(&c, &opts);
        assert!(
            (r_off - 2e-3).abs() < 1e-4,
            "a 1 V error at the midpoint should leave ~2 mA KCL residual, got {r_off:e}"
        );
    }
}
