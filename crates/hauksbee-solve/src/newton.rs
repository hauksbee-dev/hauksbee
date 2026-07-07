//! Newton-Raphson iteration and the DC operating point.
//!
//! One Newton iteration assembles the linearized system `g x = rhs` (every
//! device stamps its tangent and equivalent current at the current iterate),
//! solves it with the reusable factorization, and checks convergence. The DC
//! operating point reuses this with reactive elements opened/shorted, plus
//! gmin-stepping and source-stepping homotopy when the cold-start Newton stalls.

use crate::options::{SolverOptions, Strategy};
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
    /// Persistent work buffer for `Symbolic::solve`'s permuted RHS. Owned by the
    /// workspace and passed into every solve so the factorization stays `&self`
    /// and no `vec![0.0; n]` is allocated per Newton iteration (plan §4.1).
    solve_scratch: Vec<f64>,
    /// Persistent linearization-point buffer: a snapshot of `x` taken at the top
    /// of each Newton iteration (the point the Jacobian is stamped at), replacing
    /// the per-iteration `x.clone()`. Fully overwritten each iteration before any
    /// read, so its lifetime — not its value history — is all that changed (plan
    /// §4.3).
    lin_point: Vec<f64>,
    /// Persistent buffer holding the PREVIOUS iteration's linearization point,
    /// replacing the per-iteration `lin_point.clone()` that fed the singular-
    /// refactor node-block convergence check. Copied from `x_prev_iter` before it
    /// is refreshed, so the values are identical to the old clone (plan §4.3).
    prev_iterate: Vec<f64>,
    /// Per-node UNDAMPED Newton step from the previous iteration, used by the
    /// staged-DC adaptive damping to detect oscillating nodes (a sign reversal)
    /// and damp only those, leaving converging nodes near-full steps. Allocated
    /// once; only touched on the staged path (branch_reg>0).
    prev_step: Vec<f64>,
    /// Per-node consecutive-oscillation counter: how many iterations in a row a
    /// node's undamped step has reversed sign. Used by the staged-DC adaptive
    /// damping to damp a persistently-oscillating node PROGRESSIVELY harder (a
    /// fixed fraction can sustain a stubborn 2-cycle, the measured refractory-reset
    /// follow-on limit cycle on the synapse-mesh / BJT-mirror nodes). Allocated
    /// once; only touched on the staged path (branch_reg>0).
    osc_count: Vec<u32>,
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
    /// FROZEN analog-switch on/off decisions for the event-driven staged solve,
    /// keyed by device id. `None` (every normal solve) keeps the switches'
    /// smooth tanh conductance + control tangent and the path bit-identical; the
    /// staged-DC outer loop sets it so each inner Newton solve holds switch state
    /// fixed (smooth resistor network) and re-evaluates between solves.
    switch_freeze: Option<std::collections::HashMap<DeviceId, bool>>,
    /// Armed by the transient driver on a stiff comparator/switch board: when a
    /// bare per-step Newton fails, retry the step through the event-freeze loop
    /// (`newton_solve_event`) before cutting the timestep. Off by default, so an
    /// ordinary circuit's per-step path is unchanged.
    tran_event: bool,
    /// Armed by the transient driver on the same stiff-board condition as
    /// `tran_event`: enable the GLOBAL Armijo line-search globalization inside the
    /// per-step (`dc==false`) Newton, to defeat the traveling fused-mesh limit
    /// cycle (the post-reset overshoot whose maxdV rotates across mesh nodes, which
    /// per-node damping cannot catch). Off by default so an ordinary circuit's
    /// per-step path is bit-identical (no residual evaluation). Also force-on via
    /// Strategy::LineSearch for direct grants.
    tran_line_search: bool,
    /// SPDT leg sibling map (device id -> the device id of its complementary
    /// throw), recovered once at construction. Two `VSwitch` legs are siblings
    /// when they share the common node `a` and their names differ only in the
    /// `_s0`/`_s1` suffix the binder assigns the two throws. Threaded into the
    /// stamp so the smooth-tanh path can enforce BREAK-BEFORE-MAKE (a real SPDT
    /// is never low-Z to both throws at once): in the select transition band the
    /// loser leg is driven toward `roff`, instead of both legs sitting at the
    /// geometric-mean conductance and bridging the two throws. Consulted when
    /// `effects.spdt_bbm` is on (the device-model default; the typed compat
    /// field restores the bridging model).
    spdt_sibling: std::collections::HashMap<DeviceId, DeviceId>,
}

impl Workspace {
    /// Access the compiled constant-backbone stamp plan.
    pub fn stamp_plan(&self) -> &StampPlan {
        &self.plan
    }

    /// Arm/disarm the per-step transient event-freeze retry. Set by the transient
    /// driver only on a board that needed the staged DC (or via TRANSIENT_DYN);
    /// the default-false keeps ordinary circuits on the plain per-step path.
    pub fn set_tran_event(&mut self, on: bool) {
        self.tran_event = on;
    }

    /// Whether the per-step transient event-freeze retry is armed.
    pub fn tran_event(&self) -> bool {
        self.tran_event
    }

    /// Arm/disarm the per-step transient Armijo line-search globalization. Set by
    /// the transient driver on the stiff-board path (same condition as
    /// `set_tran_event`); the default-false keeps ordinary per-step Newton
    /// bit-identical (and free of the extra residual evaluation).
    pub fn set_tran_line_search(&mut self, on: bool) {
        self.tran_line_search = on;
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
            switch_freeze: None,
            spdt_sibling: &self.spdt_sibling,
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
            switch_freeze: None,
            spdt_sibling: &self.spdt_sibling,
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
        let spdt_sibling = SpdtPairs::analyze(circuit).sibling;
        Workspace {
            layout,
            symbolic,
            matrix,
            rhs: vec![0.0; size],
            x: vec![0.0; size],
            x_prev_iter: vec![0.0; size],
            solve_scratch: vec![0.0; size],
            lin_point: vec![0.0; size],
            prev_iterate: vec![0.0; size],
            prev_step: vec![0.0; size],
            osc_count: vec![0; size],
            linear,
            plan,
            staged_branch_reg: 0.0,
            used_staged_dc: false,
            cmp_freeze: None,
            switch_freeze: None,
            tran_event: false,
            tran_line_search: false,
            spdt_sibling,
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
    // GLOBAL damped-Newton line-search (Armijo backtracking on the full residual
    // infinity-norm). Gated behind Strategy::LineSearch; OFF by default so
    // every existing per-step path is bit-identical. This is the textbook
    // globalization for a traveling overshoot in a stiff mesh (the measured
    // refractory-reset follow-on limit cycle: a huge Newton correction that pins
    // maxdV on a DIFFERENT synapse-mesh/BJT-mirror node each iteration, which the
    // per-node oscillation damping cannot catch because the offending node moves).
    // On each Newton step compute the full step dx, then backtrack alpha in
    // {1, 0.5, 0.25, ...} until ||F(x + alpha*dx)||_inf < (1 - c*alpha)||F(x)||_inf
    // with c = ARMIJO_C and a floor on alpha; the accepted step decreases the true
    // nonlinear residual monotonically, so the iteration cannot travel.
    //
    // SCOPE: the TRANSIENT per-step Newton only (`dc == false`), never the DC
    // operating-point / staged-event solves (`dc == true`). The DC staged path has
    // its own carefully-tuned homotopy (relaxed seed + diode-Is continuation +
    // event-freeze Gauss-Seidel); folding an Armijo residual-decrease onto those
    // inner solves perturbs their convergence (measured: it broke the milestone's
    // rest-DC). The brief's globalization target is the post-reset traveling
    // mesh limit cycle, which lives in the transient per-step Newton, so the
    // line-search belongs there. With the flag off it is a no-op everywhere.
    //
    // Armed either by the transient driver on the stiff-board path
    // (ws.tran_line_search, TransientDyn's arming) or granted directly via
    // Strategy::LineSearch. Never on the DC path (dc==true).
    let line_search = !dc && (ws.tran_line_search || opts.ladder.has(Strategy::LineSearch));
    if line_search && !ws.tran_line_search {
        // Granted directly by the ladder (not via TransientDyn's arming):
        // record the activation.
        crate::diagnostics::note(Strategy::LineSearch);
    }
    const ARMIJO_C: f64 = 1e-4;
    const ARMIJO_ALPHA_FLOOR: f64 = 1.0 / 64.0;
    // Anchor for pn-junction voltage limiting: the linearization point of the
    // PREVIOUS Newton iteration. Seeded to the starting guess so the first
    // iteration's anchor equals its own linearization point (pnjlim then sees
    // zero delta and does not limit), exactly as a cold SPICE iteration.
    ws.x_prev_iter.copy_from_slice(&ws.x);
    for s in ws.prev_step.iter_mut() {
        *s = 0.0;
    }
    for c in ws.osc_count.iter_mut() {
        *c = 0;
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
    // Undamped step inf-norm of the PREVIOUS line-search iteration, plus the
    // hysteresis arm state, the lazy-arming predictor's history (see the
    // line_search block). Armed at entry: the first iteration always searches.
    let mut prev_ls_step_norm = f64::INFINITY;
    let mut sim_ls_armed = true;
    // Census (lever-3 attribution): the previous iteration's post-line-search
    // node-step norm, for the contraction-ratio histogram. NaN = none yet.
    // Pure readout, only computed when the census is live.
    let census_on = crate::census::enabled();
    let mut census_prev_norm = f64::NAN;
    loop {
        iters += 1;
        // Snapshot the point we're about to linearize around BEFORE the solve
        // overwrites ws.x, so it becomes next iteration's limiting anchor. Copied
        // into the persistent `lin_point` buffer rather than a fresh clone; the
        // buffer is fully rewritten here every iteration, so this is a pure
        // lifetime change (plan §4.3).
        ws.lin_point.copy_from_slice(&ws.x);
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
                switch_freeze: ws.switch_freeze.as_ref(),
                spdt_sibling: &ws.spdt_sibling,
            };
            // Census hooks (HAUKSBEE_STEP_CENSUS): the march-cost attribution
            // needs the stamp/factor/backsolve split and these three calls are
            // the only place the phases exist. A cached-bool branch when off.
            //
            // AssemblyMode routing (03-solver-performance.md §5): `Planned`
            // (explicit opt-in) replays the compiled constant backbone and
            // re-stamps only the nonlinear/time-varying tier through
            // pre-resolved slots; the default `Interpreted` keeps the classic
            // walk bit-identical. `stamp_all_planned` itself falls back to the
            // interpreted walk on contexts the plan does not model (DC solves,
            // staged regularizers, event-frozen states).
            crate::census::timed(crate::census::Phase::Stamp, || {
                if opts.assembly == crate::options::AssemblyMode::Planned {
                    crate::plan::stamp_all_planned(&ctx, &ws.plan, &mut ws.matrix, &mut ws.rhs)
                } else {
                    stamp_all(&ctx, &mut ws.matrix, &mut ws.rhs)
                }
            });
        }
        // The iterate from the step before this one (the anchor we just used)
        // vs the current linearization point: if they already agree to
        // tolerance, the previous Newton step had converged. Persistent-buffer
        // equivalent of `prev_iterate = mem::replace(&mut x_prev_iter,
        // lin_point.clone())`: stash the OLD x_prev_iter into `prev_iterate`
        // before overwriting x_prev_iter with this iteration's lin_point. Same
        // values, no per-iteration clone (plan §4.3).
        ws.prev_iterate.copy_from_slice(&ws.x_prev_iter);
        ws.x_prev_iter.copy_from_slice(&ws.lin_point);

        let factored = crate::census::timed(crate::census::Phase::Factor, || {
            ws.symbolic.refactor(&ws.matrix)
        });
        if !factored {
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
                && node_block_converged(&ws.lin_point, &ws.prev_iterate, &ws.layout, opts);
            if dbg_staged {
                eprintln!("[newton] refactor singular at iter {iters} (already_converged={already})");
            }
            if census_on {
                let reason = if already {
                    crate::census::NewtonExit::SingularAccept
                } else {
                    crate::census::NewtonExit::SingularFail
                };
                crate::census::newton_exit(reason, iters);
            }
            return NewtonResult { converged: already, iters };
        }
        // Residual of the NONLINEAR system at the current linearization point,
        // F(lin_point) = g*lin_point - rhs over the node block. Computed from the
        // just-stamped (g, rhs) BEFORE the solve clobbers rhs, so the line-search
        // can compare the trial residual against it. Only built when the
        // line-search is armed (otherwise it is dead work on the bit-identical
        // default path).
        let f_norm_lin = if line_search && !ws.linear {
            let mut worst = 0.0f64;
            for i in 0..ws.layout.n_nodes {
                let row = ws.matrix.row(i);
                let mut acc = 0.0;
                for &(col, val) in row {
                    acc += val * ws.lin_point[col];
                }
                let f = (acc - ws.rhs[i]).abs();
                if f > worst {
                    worst = f;
                }
            }
            worst
        } else {
            0.0
        };

        crate::census::timed(crate::census::Phase::Backsolve, || {
            ws.symbolic.solve(&mut ws.rhs, &mut ws.solve_scratch)
        });
        // rhs now holds the new (UNDAMPED) Newton iterate.
        ws.x.copy_from_slice(&ws.rhs);

        // A fully linear system is solved exactly in one shot; no need to
        // assemble and factor a second time just to watch the residual be zero.
        if ws.linear {
            if census_on {
                crate::census::newton_exit(crate::census::NewtonExit::Linear, iters);
            }
            return NewtonResult {
                converged: true,
                iters,
            };
        }

        // GLOBAL Armijo line-search (opt-in). Backtrack the full Newton step dx =
        // (ws.x - lin_point) until the trial point's nonlinear residual decreases
        // sufficiently. This REPLACES the undamped iterate with lin_point+alpha*dx;
        // the per-node staged damping below then operates on the globalized point.
        // Convergence is still judged on the resulting step (lin_point -> ws.x), so
        // a heavily backtracked step reads as "not yet converged" and the loop
        // continues -- exactly what stops the traveling overshoot.
        if line_search {
            let dx: Vec<f64> = (0..ws.x.len()).map(|i| ws.x[i] - ws.lin_point[i]).collect();
            // Lazy-arming predictor SHADOW (census only, no behaviour): a
            // two-state hysteresis machine. ARMED runs the search every
            // iteration and only DISARMS when the search itself accepts
            // alpha=1 (proof the full step currently satisfies Armijo);
            // DISARMED skips the search and RE-ARMS the moment the proposed
            // step norm stops shrinking (the traveling-overshoot signature the
            // search exists for). Floor-grinding iterations never see an
            // alpha=1 accept, so they structurally keep their full search. The
            // first cut of this predictor (skip on any monotone shrink,
            // stateless) was measured near-uninformative on the flagship
            // march: it skipped 93% of iterations with P(pass|skip)=57%
            // against a 58% base rate, and it disarmed the floor-grinders.
            // Cross-tabbed against the ground truth of the alpha=1 trial
            // below to measure the hit rate BEFORE any skip is wired in.
            let step_norm = dx.iter().fold(0.0f64, |m, &d| m.max(d.abs()));
            let would_skip = !sim_ls_armed && step_norm < prev_ls_step_norm;
            if !sim_ls_armed && step_norm >= prev_ls_step_norm {
                sim_ls_armed = true;
            }
            prev_ls_step_norm = step_norm;
            let mut alpha = 1.0;
            let mut best_alpha = 1.0;
            let mut best_norm = f64::INFINITY;
            // Census readout: whether this iteration's search ended by Armijo
            // sufficient decrease (vs hitting the floor and falling back to
            // the best trial). Written below, read only when the census is on.
            let mut armijo_ok = false;
            // Ground truth for the predictor: did the FIRST (alpha=1) trial
            // pass Armijo?
            let mut trials = 0u32;
            let mut first_trial_pass = false;
            loop {
                // Trial point x = lin_point + alpha*dx, evaluated into ws.x scratch.
                for i in 0..ws.x.len() {
                    ws.x[i] = ws.lin_point[i] + alpha * dx[i];
                }
                let trial_norm = crate::census::timed(crate::census::Phase::LineSearch, || {
                    residual_inf_norm_at(
                        ws, circuit, opts, time, coeffs, state, dc, use_ic, gmin, src_scale,
                        branch_reg,
                    )
                });
                trials += 1;
                if trial_norm < best_norm {
                    best_norm = trial_norm;
                    best_alpha = alpha;
                }
                // Armijo sufficient-decrease on the residual inf-norm. The first
                // Newton iterate (f_norm_lin==0 because lin_point is the cold
                // start) accepts the full step.
                let accept = trial_norm.is_finite()
                    && (f_norm_lin <= 0.0 || trial_norm < (1.0 - ARMIJO_C * alpha) * f_norm_lin);
                if trials == 1 && accept {
                    first_trial_pass = true;
                }
                if accept || alpha <= ARMIJO_ALPHA_FLOOR + 1e-15 {
                    armijo_ok = accept;
                    break;
                }
                alpha *= 0.5;
            }
            crate::census::ls_predictor(would_skip, first_trial_pass);
            // Shadow state update from the ground truth (in the real design
            // the DISARM transition comes from the search's own outcome, so
            // it is only ever taken on iterations that actually searched).
            if sim_ls_armed && first_trial_pass {
                sim_ls_armed = false;
            }
            // If even the floor step did not satisfy Armijo, take the best alpha
            // tried (the largest residual decrease seen) rather than a step that
            // increases the residual.
            let use_alpha = if best_norm.is_finite() && best_norm < f_norm_lin && f_norm_lin > 0.0 {
                best_alpha
            } else {
                alpha
            };
            for i in 0..ws.x.len() {
                ws.x[i] = ws.lin_point[i] + use_alpha * dx[i];
            }
            // Census: the alpha this iteration actually stepped with, and how
            // the search ended (the lever-1c lazy-arming decision data).
            crate::census::ls_alpha(use_alpha, armijo_ok);
            if dbg_newton {
                eprintln!(
                    "[newton-ls] iter {iters} alpha={use_alpha:.4} ||F||: {f_norm_lin:.3e} -> {best_norm:.3e}"
                );
            }
            // After globalizing, re-stamp must happen for the next iteration's
            // matrix anyway, so nothing else to restore here. The scratch ws.matrix
            // / ws.rhs were overwritten by residual_inf_norm_at and will be
            // re-stamped at the top of the next loop.
        }

        // Convergence is judged on the UNDAMPED Newton step (ws.x vs the point we
        // linearized at, lin_point): that is the true measure of how far the
        // iterate is from the root. Damping (below) is a path globalization only;
        // judging convergence on the damped iterate would let a small damped step
        // masquerade as convergence. The normal (branch_reg==0) path damps
        // nothing, so this is identical to the old test there.
        let undamped_converged = converged(&ws.x, &ws.lin_point, &ws.layout, opts);
        let undamped_node_converged =
            node_block_converged(&ws.x, &ws.lin_point, &ws.layout, opts);

        // Census (lever-3 attribution): the node-block step norm this
        // iteration will be judged on, captured BEFORE the damping rewrites
        // ws.x. Pure readout behind the cached bool.
        let census_step_norm = if census_on {
            let mut n = 0.0f64;
            for i in 0..ws.layout.n_nodes {
                let d = (ws.x[i] - ws.lin_point[i]).abs();
                if d > n {
                    n = d;
                }
            }
            n
        } else {
            0.0
        };
        // How many nodes the staged damping classifies as oscillating this
        // iteration (integer side-channel out of the damping loop below; no
        // float behaviour touched).
        let mut census_osc_nodes = 0u64;

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
            const ALPHA_OSCILLATING: f64 = 0.25; // sign reversed: base damping
            for i in 0..ws.layout.n_nodes {
                let full = ws.x[i] - ws.lin_point[i];
                let prev = ws.prev_step[i];
                // Oscillating if this step opposes the previous one and both are
                // non-trivial.
                let oscillating = prev * full < 0.0 && prev.abs() > 1e-9 && full.abs() > 1e-9;
                // PROGRESSIVE damping: a fixed fraction (0.25) breaks a transient
                // 2-cycle but can SUSTAIN a stubborn one (the measured
                // refractory-reset follow-on limit cycle, where a synapse-mesh /
                // BJT-mirror node and its neighbour flip-flop at the step cap for
                // hundreds of iterations and never decay). Each consecutive
                // oscillation halves the node's damping again (0.25, 0.125, ...),
                // so a node that keeps reversing is driven toward a near-frozen
                // step and the cycle collapses; a node that stops reversing resets
                // its counter and resumes near-full steps. A converging node never
                // oscillates, so this is inert at the root and bit-identical on the
                // normal path (branch_reg==0 skips this block entirely).
                let alpha = if oscillating {
                    census_osc_nodes += 1;
                    ws.osc_count[i] = ws.osc_count[i].saturating_add(1);
                    // 0.25 / 2^(min(osc_count-1, 8)) -> floors at ~1e-3.
                    let shrink = ws.osc_count[i].saturating_sub(1).min(8);
                    ALPHA_OSCILLATING / (1u32 << shrink) as f64
                } else {
                    ws.osc_count[i] = 0;
                    ALPHA_CONVERGING
                };
                let mut step = alpha * full;
                if step > NODE_STEP_MAX {
                    step = NODE_STEP_MAX;
                } else if step < -NODE_STEP_MAX {
                    step = -NODE_STEP_MAX;
                }
                ws.prev_step[i] = step;
                ws.x[i] = ws.lin_point[i] + step;
            }
        }

        if census_on {
            crate::census::newton_iter_norm(
                census_step_norm,
                census_prev_norm,
                branch_reg > 0.0 && iters < 3 && undamped_node_converged && !undamped_converged,
                census_osc_nodes,
            );
            census_prev_norm = census_step_norm;
        }

        if undamped_converged {
            if census_on {
                crate::census::newton_exit(crate::census::NewtonExit::Full, iters);
            }
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
            if census_on {
                crate::census::newton_exit(crate::census::NewtonExit::NodeBlock, iters);
            }
            return NewtonResult { converged: true, iters };
        }
        if dbg_newton {
            let mut maxd = 0.0f64;
            let mut argi = 0usize;
            for i in 0..ws.layout.n_nodes {
                let d = (ws.x[i] - ws.lin_point[i]).abs();
                if d > maxd {
                    maxd = d;
                    argi = i;
                }
            }
            eprintln!("[newton] iter {iters} maxdV(undamped)={maxd:.3e} @node{argi} branch_reg={branch_reg:e}");
        }
        // Stall detection bails a limit-cycling solve early. It must NOT fire when
        // the discrete states are FROZEN (cmp_freeze/switch_freeze set): the inner
        // circuit is then smooth and converges monotonically-but-slowly (the
        // diode/BJT-mirror tail is linear), so a flat window is "still working",
        // not a limit cycle. Bailing there wrongly fails the event-freeze inner
        // solve right at the spike-gate flip. With the states self-deciding
        // (no freeze) the early bail is the correct limit-cycle guard.
        let frozen = ws.cmp_freeze.is_some() || ws.switch_freeze.is_some();
        if branch_reg > 0.0 && !frozen {
            // Norm of the undamped node step this iteration.
            let mut norm = 0.0f64;
            for i in 0..ws.layout.n_nodes {
                let d = (ws.x[i] - ws.lin_point[i]).abs();
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
                    if census_on {
                        crate::census::newton_exit(crate::census::NewtonExit::Stall, iters);
                    }
                    return NewtonResult { converged: false, iters };
                }
            }
        }
        if iters >= opts.max_newton {
            if census_on {
                crate::census::newton_exit(crate::census::NewtonExit::MaxIters, iters);
            }
            return NewtonResult {
                converged: false,
                iters,
            };
        }
    }
}

/// Stamp the full nonlinear system at `ws.x` (a trial point) with the SAME
/// integration/freeze/regularizer context as the live Newton iteration, and
/// return the infinity-norm of the residual `F = g*x - rhs` over the NODE block.
/// Used by the opt-in Armijo line-search in [`newton_solve`] to evaluate a
/// backtracked trial point. Clobbers `ws.matrix` and `ws.rhs` (scratch that the
/// next Newton iteration re-stamps anyway). A non-finite entry returns +inf so a
/// poisoned trial point is never accepted.
#[allow(clippy::too_many_arguments)]
fn residual_inf_norm_at(
    ws: &mut Workspace,
    circuit: &Circuit,
    opts: &SolverOptions,
    time: f64,
    coeffs: IntegCoeffs,
    state: &ReactiveState,
    dc: bool,
    use_ic: bool,
    gmin: f64,
    src_scale: f64,
    branch_reg: f64,
) -> f64 {
    ws.matrix.clear_values();
    for v in ws.rhs.iter_mut() {
        *v = 0.0;
    }
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
            switch_freeze: ws.switch_freeze.as_ref(),
            spdt_sibling: &ws.spdt_sibling,
        };
        stamp_all(&ctx, &mut ws.matrix, &mut ws.rhs);
    }
    let mut worst = 0.0f64;
    for i in 0..ws.layout.n_nodes {
        let row = ws.matrix.row(i);
        let mut acc = 0.0;
        for &(col, val) in row {
            acc += val * ws.x[col];
        }
        let f = acc - ws.rhs[i];
        if !f.is_finite() {
            return f64::INFINITY;
        }
        if f.abs() > worst {
            worst = f.abs();
        }
    }
    worst
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

/// Copy each series-resistance BJT's EXTERNAL terminal voltages onto its
/// device-private internal unknowns (dev-plan 04 §3.2). `.nodeset` can only
/// name netlist nodes, so a cold start would otherwise leave the intrinsic
/// nodes at zero volts behind a seeded external — putting the intrinsic
/// junction back into the limiting-walk trap the nodeset was written to
/// avoid. Through a small ohmic resistance "same voltage as the terminal" is
/// the physically sensible guess. On the all-zero cold start this is a no-op
/// (0 -> 0), and circuits without internal nodes take one empty-vec check.
fn seed_bjt_internal_nodes(ws: &mut Workspace, circuit: &Circuit) {
    for (id, dev) in circuit.iter() {
        if let Device::Bjt { c, b, e, .. } = dev {
            if let Some(&ints) = ws.layout.bjt_internal(id) {
                let ext = [*c, *b, *e];
                for t in 0..3 {
                    if let Some(ii) = ints[t] {
                        ws.x[ii] = ws.layout.node(ext[t]).map(|i| ws.x[i]).unwrap_or(0.0);
                    }
                }
            }
        }
    }
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
        newton_solve(
            ws, circuit, opts, 0.0, 1.0, coeffs, &empty, true, use_ic, gmin, scale,
        )
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

    // Attempt 1: direct cold start. Normally the zero vector; but `.nodeset`
    // cards (SPICE-compat §4.1) seed the START VECTOR for named nodes. This is a
    // convergence GUESS only — Newton is free to walk away from it (nothing is
    // pinned), so on a well-posed circuit the root is unchanged, while on a
    // multi-stable one the seed selects which root is found. Every other node
    // stays zero, exactly as before.
    for v in ws.x.iter_mut() {
        *v = 0.0;
    }
    for &(nid, val) in &circuit.nodesets {
        if let Some(i) = ws.layout.node(nid) {
            ws.x[i] = val;
        }
    }
    seed_bjt_internal_nodes(ws, circuit);
    let dbg = std::env::var("HAUKSBEE_STAGED_DBG").is_ok();
    let r = solve(ws, opts.gmin, 1.0);
    if dbg {
        eprintln!("[dc] plain cold: converged={} iters={}", r.converged, r.iters);
    }
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
            if dbg {
                eprintln!("[dc] gmin-step stalled at gmin={gmin:e} iters={}", r.iters);
            }
            ok = false;
            break;
        }
        gmin *= 0.1;
    }
    if ok && solve(ws, opts.gmin, 1.0).converged {
        return Ok(());
    }
    if dbg {
        eprintln!("[dc] gmin ladder failed (ok={ok})");
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
            if dbg {
                eprintln!("[dc] source-step stalled at scale={scale:.3} iters={}", r.iters);
            }
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
    // (Strategy::DynamicPivot). The default staged path keeps the original behaviour
    // (frozen-singular -> adopt the relaxed power-on point) bit-for-bit and at
    // the original speed, so no existing test changes value or timing. It is
    // restored to off before every return so the workspace-reused transient
    // keeps frozen-only semantics.
    let dc_dyn = opts.ladder.has(Strategy::DynamicPivot);
    if dc_dyn {
        crate::diagnostics::note(Strategy::DynamicPivot);
    }
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
            // Strategy::EventFreeze rather than run on the default path. The
            // dynamic-pivot LU (the structural fix) stays on regardless.
            if opts.ladder.has(Strategy::EventFreeze) {
                crate::diagnostics::note(Strategy::EventFreeze);
                if let Some(root) = staged_event_solve(ws, circuit, opts, &seed, staged_gmin, STAGED_BRANCH_REG, dbg) {
                    ws.x.copy_from_slice(&root);
                    ws.staged_branch_reg = 0.0;
                    ws.cmp_freeze = None;
                    ws.switch_freeze = None;
                    ws.symbolic.set_allow_dynamic(false);
                    ws.used_staged_dc = true;
                    return Ok(());
                }
            }
            ws.cmp_freeze = None;
            ws.switch_freeze = None;
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
            // behind Strategy::Ptc rather than run on the hot path.
            if opts.ladder.has(Strategy::Ptc) {
                crate::diagnostics::note(Strategy::Ptc);
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

    // Residual-acceptance backstop (opt-in, gated; default path unchanged).
    //
    // The homotopy/staged ladder accepts on a Newton *step-norm* (increment)
    // test, never on the KCL residual. A high-Z node behind a reverse-biased
    // diode / DC-open cap can make the Newton map limit-cycle: successive
    // iterates flip-flop by a small but >vntol amount forever, so the increment
    // test never passes even though the actual KCL imbalance F = g*x - rhs is at
    // the gmin floor (a genuine root). For a torn feedforward column (one output
    // neuron of the spiking net: BJT mirrors off a stiff rail + SPDT switches +
    // one comparator/RC-stretcher) that limit cycle parks at a sub-nA residual.
    // Under Strategy::ResidualAccept, check the real residual on the current
    // iterate and accept it as a root if it is below opts.residual_accept_tol
    // (default 1e-9 A). This is physically honest: a sub-nA KCL residual *is*
    // a DC operating point. It is OFF by default so the production DC path is
    // bit-identical.
    if opts.ladder.has(Strategy::ResidualAccept) {
        let tol = opts.residual_accept_tol;
        let res = ws.dc_residual_inf_norm(circuit, opts);
        if res.is_finite() && res < tol {
            crate::diagnostics::note(Strategy::ResidualAccept);
            if dbg {
                eprintln!("[staged] residual-accept: KCL residual {res:e} A < {tol:e} A, accepting iterate as DC root");
            }
            ws.symbolic.set_allow_dynamic(false);
            ws.used_staged_dc = true;
            return Ok(());
        }
    }

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

/// The two complementary legs of one physical SPDT analog switch, paired by the
/// shared common node `a` (the binder emits `{ref}_s0` / `{ref}_s1` with the same
/// `a` = the summing/common node, opposite control senses). The summing bus must
/// never be simultaneously low-Z to BOTH throws, so the break-before-make logic
/// in [`eval_switch_states`] consults these pairs.
struct SpdtPairs {
    /// device id -> the device id of its sibling leg (only present for paired legs).
    sibling: std::collections::HashMap<DeviceId, DeviceId>,
}

impl SpdtPairs {
    /// Recover SPDT leg pairs from the circuit. A pair is two `VSwitch` devices
    /// that share the same common node `a` and whose names differ only in the
    /// `_s0`/`_s1` suffix the binder assigns to the two throws. Built once per
    /// event solve; empty for boards with no SPDTs (then the break-before-make is
    /// inert and the path is unchanged).
    /// No SPDT pairs (the break-before-make is inert). Used by unit tests on
    /// circuits with no complementary-leg SPDTs.
    #[cfg(test)]
    fn empty() -> SpdtPairs {
        SpdtPairs { sibling: std::collections::HashMap::new() }
    }

    fn analyze(circuit: &Circuit) -> SpdtPairs {
        // Group VSwitch legs by (common node a, base name without the _sN suffix).
        let mut groups: std::collections::HashMap<(u32, String), Vec<DeviceId>> =
            std::collections::HashMap::new();
        for (id, dev) in circuit.iter() {
            if let Device::VSwitch { name, a, .. } = dev {
                let base = name
                    .strip_suffix("_s0")
                    .or_else(|| name.strip_suffix("_s1"))
                    .map(|s| s.to_string());
                if let Some(base) = base {
                    groups.entry((a.0, base)).or_default().push(id);
                }
            }
        }
        let mut sibling = std::collections::HashMap::new();
        for (_, ids) in groups {
            if ids.len() == 2 {
                sibling.insert(ids[0], ids[1]);
                sibling.insert(ids[1], ids[0]);
            }
        }
        SpdtPairs { sibling }
    }
}

/// Derive each analog switch's frozen on/off decision from a solution vector
/// `x`, using the switch's own (von, voff) thresholds with hysteresis so a
/// control voltage inside the [voff, von] band holds the switch's previous
/// state. Returns the decision map keyed by device id, empty when the circuit
/// has no switches (so the event loop can skip the freeze for plain boards).
///
/// BREAK-BEFORE-MAKE: for the two complementary legs of one SPDT (paired in
/// `pairs`), both legs are NEVER allowed ON in the same frozen state. During the
/// flip the smooth tanh conductance of each leg would otherwise make the common
/// node simultaneously low-Z to BOTH throws (GND and the output membrane) — the
/// multi-decade conductance snap that makes the per-step Newton matrix singular.
/// When both legs of a pair evaluate ON, the leg whose control sits LESS firmly
/// in its on-region is forced OFF (break) so the other can make. The
/// Gauss-Seidel outer loop re-derives between inner solves, so as the control
/// fully crosses, the make leg takes over cleanly after the break leg has opened.
fn eval_switch_states(
    circuit: &Circuit,
    layout: &Layout,
    x: &[f64],
    prev: &std::collections::HashMap<DeviceId, bool>,
    pairs: &SpdtPairs,
) -> std::collections::HashMap<DeviceId, bool> {
    // First pass: each switch's raw hysteretic decision and its "on-margin" (how
    // far the control sits past the on threshold; higher = more firmly on).
    let mut on: std::collections::HashMap<DeviceId, bool> = std::collections::HashMap::new();
    let mut margin: std::collections::HashMap<DeviceId, f64> = std::collections::HashMap::new();
    for (id, dev) in circuit.iter() {
        if let Device::VSwitch { ctrl_p, ctrl_n, von, voff, .. } = dev {
            let vp = layout.node(*ctrl_p).map(|i| x[i]).unwrap_or(0.0);
            let vn = layout.node(*ctrl_n).map(|i| x[i]).unwrap_or(0.0);
            let vctrl = vp - vn;
            let vmid = 0.5 * (von + voff);
            let was_on = prev.get(&id).copied().unwrap_or(vctrl > vmid);
            // Hysteresis at the switch's own band: turn ON above von, OFF below
            // voff, otherwise hold. (For von==voff this is the mid crossing.)
            let st = if was_on { vctrl > *voff } else { vctrl > *von };
            on.insert(id, st);
            // On-margin relative to vmid (sign-agnostic to which leg): used only
            // to choose which leg breaks when both would make.
            margin.insert(id, vctrl - vmid);
        }
    }
    // Break-before-make: resolve every SPDT pair so at most one leg is ON.
    for (&id, &sib) in pairs.sibling.iter() {
        // Process each unordered pair once (id < sib).
        if id.0 >= sib.0 {
            continue;
        }
        let a_on = on.get(&id).copied().unwrap_or(false);
        let b_on = on.get(&sib).copied().unwrap_or(false);
        if a_on && b_on {
            // Both legs want to conduct (the transition overlap). Keep the leg
            // whose control sits more firmly in its on-region; break the other.
            let ma = margin.get(&id).copied().unwrap_or(0.0);
            let mb = margin.get(&sib).copied().unwrap_or(0.0);
            if ma >= mb {
                on.insert(sib, false);
            } else {
                on.insert(id, false);
            }
        }
    }
    on
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

    // Initial comparator AND analog-switch decisions from the relaxed
    // (diodes-off) seed. The discrete state of BOTH device classes is frozen in
    // the inner solve: the comparators are bang-bang and the 4320 analog
    // switches each carry a tanh conductance whose control sits near transition
    // for the coupled core, so both must be held fixed for the inner circuit to
    // be smooth (otherwise a flipping switch flips the synapse current that
    // flips a comparator that flips the switch — the measured limit cycle on the
    // switch-control / BJT-base nodes).
    let spdt = SpdtPairs::analyze(circuit);
    let mut cmp_states = eval_comparator_states(circuit, &ws.layout, seed, &Default::default());
    let mut sw_states = eval_switch_states(circuit, &ws.layout, seed, &Default::default(), &spdt);
    if cmp_states.is_empty() && sw_states.is_empty() {
        return None; // nothing discrete to freeze: this path adds nothing.
    }

    ws.staged_branch_reg = branch_reg;
    let mut x = seed.to_vec();
    const MAX_EVENT_PASSES: usize = 40;
    for pass in 0..MAX_EVENT_PASSES {
        ws.cmp_freeze = Some(cmp_states.clone());
        ws.switch_freeze = Some(sw_states.clone());
        ws.x.copy_from_slice(&x);
        let r = newton_solve(
            ws, circuit, opts, 0.0, 1.0, coeffs, &empty, true, false, staged_gmin, 1.0,
        );
        if !r.converged {
            if dbg {
                eprintln!("[staged-event] pass {pass}: inner Newton did NOT converge (iters {})", r.iters);
            }
            ws.switch_freeze = None;
            return None;
        }
        x.copy_from_slice(&ws.x);
        // Re-evaluate BOTH decision sets from the converged inner solution
        // (Gauss-Seidel over the discrete comparator + switch states).
        let next_cmp = eval_comparator_states(circuit, &ws.layout, &x, &cmp_states);
        let next_sw = eval_switch_states(circuit, &ws.layout, &x, &sw_states, &spdt);
        let cmp_flips = next_cmp.iter().filter(|(k, v)| cmp_states.get(k) != Some(*v)).count();
        let sw_flips = next_sw.iter().filter(|(k, v)| sw_states.get(k) != Some(*v)).count();
        if dbg {
            eprintln!(
                "[staged-event] pass {pass}: inner converged in {} iters, {cmp_flips} comparator flips, {sw_flips} switch flips",
                r.iters
            );
        }
        if cmp_flips == 0 && sw_flips == 0 {
            // Fixed point: every comparator AND every switch state is consistent
            // with its own (control) inputs at the converged smooth solution.
            // The returned vector is then a genuine root of the FULL circuit
            // (real diodes, self-deciding comparators, self-deciding switches),
            // not just of the frozen surrogate.
            ws.switch_freeze = None;
            return Some(x);
        }
        cmp_states = next_cmp;
        sw_states = next_sw;
    }
    ws.switch_freeze = None;
    None
}

/// Event-driven TRANSIENT step solve: the per-step analogue of
/// [`staged_event_solve`]. Where the bare per-step [`newton_solve`] limit-cycles
/// at a comparator/switch flip (the synapse spike-gate SPDT snapping multiple
/// conductance decades while it carries real mirror current — "Newton failed at
/// t~133us"), this freezes BOTH the comparator and the analog-switch discrete
/// states for each inner solve (making the step a smooth circuit Newton can
/// converge), re-derives every state from the converged inner solution
/// (Gauss-Seidel), and re-solves until no state flips. The break-before-make in
/// [`eval_switch_states`] keeps the summing bus from ever being low-Z to both
/// SPDT throws at once.
///
/// Unlike the DC event loop, the transient step has the reactive companion
/// (`state`, `coeffs`, `dt`) of the real integration step, so the converged
/// fixed point is a genuine root of the FULL nonlinear circuit AT t+dt: every
/// diode/BJT equation holds, every cap/inductor companion holds, and every
/// comparator+switch state is consistent with its own control. Returns the
/// converged `ws.x` in place + `true` on success; `false` (and `ws.x` left at
/// the last inner iterate) when it cannot reach a consistent converged state, so
/// the caller falls back to a step cut.
///
/// DAMPED PARTIAL FLIP: the bare DC event loop flips every disagreeing state at
/// once and can cycle (the brief's "fails at pass 1, 369 flips don't settle").
/// Here the re-derivation is bounded — at most `MAX_FLIPS_PER_PASS` switches are
/// allowed to change state per Gauss-Seidel pass, chosen by largest control
/// over/under-drive — so the discrete state walks toward consistency instead of
/// thrashing. Comparators (few, and the spike driver) are not throttled.
#[allow(clippy::too_many_arguments)]
pub fn newton_solve_event(
    ws: &mut Workspace,
    circuit: &Circuit,
    opts: &SolverOptions,
    time: f64,
    dt: f64,
    coeffs: IntegCoeffs,
    state: &ReactiveState,
    gmin: f64,
    cmp_smooth: bool,
) -> bool {
    let dbg = std::env::var("HAUKSBEE_STAGED_DBG").is_ok();
    let spdt = SpdtPairs::analyze(circuit);
    // Seed the discrete states from the entry iterate (the accepted previous
    // step, already in ws.x), anchored on the previous decisions if any (so a
    // control sitting in a hysteresis band holds, not chatters).
    let seed = ws.x.clone();
    let mut cmp_states = eval_comparator_states(circuit, &ws.layout, &seed, &Default::default());
    let mut sw_states = eval_switch_states(circuit, &ws.layout, &seed, &Default::default(), &spdt);
    if cmp_states.is_empty() && sw_states.is_empty() {
        // Nothing discrete to freeze: a plain step is already smooth. Caller
        // should not have routed here, but stay correct: one ordinary solve.
        let r = newton_solve(ws, circuit, opts, time, dt, coeffs, state, false, false, gmin, 1.0);
        return r.converged;
    }

    // Allow more inner Newton iterations: the frozen inner circuit converges
    // linearly in its tail (the damping that breaks the diode chatter slows the
    // final approach), as in the DC event loop.
    let mut inner_opts = *opts;
    inner_opts.max_newton = opts
        .event_retry
        .inner_max_newton
        .unwrap_or_else(|| opts.max_newton.max(400));
    let inner = &inner_opts;

    const MAX_EVENT_PASSES: usize = 60;
    // Switch flip budget per Gauss-Seidel pass. A throttle keeps the discrete
    // state from thrashing, but too small a budget splits a single fired
    // neuron's ganged spike-gates (one V_out drives ~10 output gates) across
    // passes, leaving an inconsistent intermediate the inner solve fights. The
    // budget is typed tuning (event_retry.flip_budget); the default is high
    // enough to flip a neuron's whole gate fan-out in one pass.
    let max_flips_per_pass: usize = opts.event_retry.flip_budget;
    // Comparator handling in the inner solve. By default the comparators are
    // FROZEN per inner solve (like the DC event loop), which converges the hidden
    // spike-gate flip. But the OUTPUT neuron has an adaptation feedback (C_adapt
    // couples its comparator OUT back to its own -IN threshold node): with the
    // output comparator pinned to a constant rail the inner Newton cannot resolve
    // that feedback and stalls right at the membrane-crosses-threshold instant.
    // CMP_SMOOTH leaves comparators SELF-DECIDING via the smooth high-gain
    // logistic transfer (the branch_reg>0 path, which has a real tangent), so the
    // comparator tracks its flip continuously while the switch mesh stays frozen.
    //
    // The OUTPUT refractory reset is the opposite regime: once the output spike
    // SPIKE1 climbs through the refractory NEURON_SWITCH's control threshold the
    // switch shorts the output membrane to GND in a fast positive-feedback
    // discharge (membrane -> comparator -> spike -> switch -> membrane). With the
    // output comparator left SMOOTH its ~2000 V^-1 logistic gain couples the
    // collapsing membrane straight back into the spike/switch loop and the
    // per-step Newton diverges. There the comparator must be FROZEN like every
    // other state so the inner circuit is a smooth resistor network. The caller
    // therefore tries cmp_smooth=true first (resolves the membrane-crosses-up
    // FIRE, the C_adapt feedback) and, if that fails, cmp_smooth=false (resolves
    // the reset discharge). `cmp_smooth` is the per-call mode, not the env, so the
    // two-mode retry can pick the regime that converges this step.
    //
    // GMIN FLOOR for the inner solves. The DC staged path anchors the
    // otherwise-floating high-impedance synapse-mesh nodes (BJT mirror bases /
    // off-switch internal nodes between the magnitude switches and the spike
    // gate, DC-coupled only through reverse-biased junctions and DC-open caps)
    // with a STAGED_GMIN=1e-7 floor; without it those nodes have no defined
    // operating point and the inner Newton limit-cycles on them (the measured
    // "maxdV pinned at the 2 V damping cap, rotating across mesh nodes" at the
    // refractory-reset follow-on step). The per-step event-freeze is called with
    // opts.gmin (1e-12), far too small to anchor them, so floor it to the same
    // staged value here. Pure-resistor inner conductances dwarf 1e-7 S, so the
    // physical nodes are unaffected (a 1e-7 S leak across 5 V is 0.5 uA, below the
    // signal currents); it only pins the genuinely-floating ones.
    //
    // Applied ONLY in the frozen-comparator (reset) regime: the smooth-comparator
    // FIRE step converges on the bare opts.gmin, and a 1e-7 floor there perturbs
    // the membrane-near-threshold operating point enough to BREAK the earlier
    // hidden spike-gate flip (measured: the floor moved the failure from the
    // refractory reset back to the fire). So only the reset pass raises the floor.
    let inner_gmin = if cmp_smooth { gmin } else { gmin.max(1e-7) };
    let mut x = seed.clone();
    for pass in 0..MAX_EVENT_PASSES {
        ws.cmp_freeze = if cmp_smooth { None } else { Some(cmp_states.clone()) };
        ws.switch_freeze = Some(sw_states.clone());
        ws.x.copy_from_slice(&x);
        let r = newton_solve(ws, circuit, inner, time, dt, coeffs, state, false, false, inner_gmin, 1.0);
        if !r.converged {
            if dbg {
                eprintln!("[tran-event] t={time:.6e} pass {pass}: inner Newton did NOT converge (iters {})", r.iters);
            }
            ws.cmp_freeze = None;
            ws.switch_freeze = None;
            return false;
        }
        x.copy_from_slice(&ws.x);

        // Re-derive both discrete state sets from the converged inner solution.
        let next_cmp = eval_comparator_states(circuit, &ws.layout, &x, &cmp_states);
        let want_sw = eval_switch_states(circuit, &ws.layout, &x, &sw_states, &spdt);

        // Comparators flip freely (few, and they DRIVE the event). When the
        // comparators are self-deciding (cmp_smooth), their state isn't frozen so
        // there is no frozen-vs-derived flip to count — consistency is automatic
        // (the smooth transfer always agrees with its own inputs), so the loop's
        // fixed point is governed by the switch set alone.
        let cmp_flips = if cmp_smooth {
            0
        } else {
            next_cmp.iter().filter(|(k, v)| cmp_states.get(k) != Some(*v)).count()
        };

        // Switches: bound how many flip this pass. Rank the disagreeing switches
        // by how far their control has moved past the relevant threshold and flip
        // only the most-committed handful — a damped Gauss-Seidel that walks
        // toward the consistent state instead of flipping all 369 at once (which
        // cycles). The break-before-make in want_sw already guarantees no SPDT
        // pair is both-on, so flipping a subset never shorts the summing bus.
        let mut disagree: Vec<(DeviceId, bool, f64)> = Vec::new();
        for (id, dev) in circuit.iter() {
            if let Device::VSwitch { ctrl_p, ctrl_n, von, voff, .. } = dev {
                let want = want_sw.get(&id).copied().unwrap_or(false);
                if sw_states.get(&id).copied() != Some(want) {
                    let vp = ws.layout.node(*ctrl_p).map(|i| x[i]).unwrap_or(0.0);
                    let vn = ws.layout.node(*ctrl_n).map(|i| x[i]).unwrap_or(0.0);
                    let vmid = 0.5 * (von + voff);
                    // Over/under-drive magnitude past the band centre = commitment.
                    disagree.push((id, want, (vp - vn - vmid).abs()));
                }
            }
        }
        let sw_flips = disagree.len();
        disagree.sort_by(|a, b| b.2.total_cmp(&a.2));
        for (id, want, _) in disagree.iter().take(max_flips_per_pass) {
            sw_states.insert(*id, *want);
        }
        cmp_states = next_cmp;

        if dbg {
            eprintln!(
                "[tran-event] t={time:.6e} pass {pass}: inner converged in {} iters, {cmp_flips} cmp flips, {sw_flips} sw disagreements (flipped {})",
                r.iters,
                sw_flips.min(max_flips_per_pass)
            );
        }

        if cmp_flips == 0 && sw_flips == 0 {
            // Fixed point at t+dt: every comparator + switch state is consistent
            // with its own control at the converged smooth solution, so ws.x is a
            // true root of the full self-deciding circuit at this step.
            ws.cmp_freeze = None;
            ws.switch_freeze = None;
            ws.x.copy_from_slice(&x);
            return true;
        }
    }
    ws.cmp_freeze = None;
    ws.switch_freeze = None;
    false
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
    ws.symbolic.set_allow_dynamic(opts.ladder.has(Strategy::DynamicPivot));
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
    // than the full one: the diode nonlinearity is gone — and so is the BJT
    // intrinsic mesh (dev-plan 04 §3.2): `series_resistance` is relaxed OFF,
    // which pins each internal unknown behind a unit diagonal and puts the
    // Gummel-Poon cores back on the external nodes, the base-topology system
    // the cold ladder is known to converge (measured on the flagship board:
    // with the intrinsic rb/rc/re mesh live, cold source-stepping limit-cycles
    // on the mirror pairs' internal nodes at scale ~0.2 and the whole staged
    // DC dies). The full-toggle solve then warm-starts from this converged
    // point with the internal unknowns seeded from their external terminals —
    // the same relax-then-continue discipline the diode OFF-conductance swap
    // embodies. For a circuit with no series-R BJT both changes are no-ops.
    let mut relaxed_opts = *opts;
    relaxed_opts.effects.series_resistance = false;
    dc_solve(&mut ws, &relaxed, &relaxed_opts, false, None).ok()?;
    seed_bjt_internal_nodes(&mut ws, &relaxed);
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

#[cfg(test)]
mod vswitch_jacobian_tests {
    use super::{dc_operating_point, newton_solve, Workspace};
    use crate::options::SolverOptions;
    use crate::stamp::IntegCoeffs;
    use crate::system::ReactiveState;
    use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};

    // A NEGATIVE-feedback analog switch (unique root, control in the tanh knee).
    // A 5 V source drives `out` through the switch; the switch's control is
    // vctrl = vbias - v(out), so as `out` rises the conductance FALLS. That
    // negative feedback gives a single self-consistent operating point sitting
    // right on the tanh transition, where the conductance's dependence on the
    // control voltage is strongest. A no-tangent (Picard) stamp iterates this
    // fixed point slowly / oscillates across the knee; the control-node Jacobian
    // makes it Newton-linearized and convergent in a few iterations to the same
    // unique root.
    fn feedback_switch_circuit() -> (Circuit, NodeId) {
        let mut c = Circuit::new();
        let src = c.node("src");
        let out = c.node("out");
        let bias = c.node("bias");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: src,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        c.add(Device::Vsource {
            name: "VB".into(),
            p: bias,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(3.5),
        });
        c.add(Device::VSwitch {
            name: "S1".into(),
            a: src,
            b: out,
            ctrl_p: bias, // vctrl = vbias - v(out): negative feedback through out
            ctrl_n: out,
            von: 2.0,
            voff: 1.0,
            ron: 1.0,
            roff: 1e3,
        });
        c.add(Device::Resistor {
            name: "RL".into(),
            a: out,
            b: NodeId::GROUND,
            ohms: 1.0,
            tc1: None,
        });
        (c, out)
    }

    #[test]
    fn control_jacobian_converges_to_the_true_root() {
        let (c, out) = feedback_switch_circuit();
        let opts = SolverOptions::default();
        let mut ws = Workspace::new(&c);
        dc_operating_point(&mut ws, &c, &opts).expect("switch DC solve converges");

        let vout = ws.layout.node(out).map(|i| ws.x[i]).unwrap();
        // The negative-feedback loop settles on the tanh knee between the fully-on
        // divider (2.5 V) and the fully-off divider (5/1001 ≈ 0 V). Confirm it is
        // a real interior operating point, not pinned to either rail.
        assert!(
            (0.2..=2.5).contains(&vout),
            "negative-feedback switch should settle in the tanh knee, got {vout}"
        );

        // It must be a TRUE root: the KCL residual at the solved point is ~0.
        let r = ws.dc_residual_inf_norm(&c, &opts);
        assert!(r < 1e-7, "residual at the switch root should be ~0, got {r:e}");
    }

    // A gently-coupled negative-feedback switch (wide tanh transition, moderate
    // impedances) on which plain undamped Newton converges. With the control-node
    // Jacobian the conductance is Newton-linearized, so the loop closes in a
    // handful of iterations. Without a control tangent (the old `let _ = rhs`) the
    // conductance lags the control voltage by one iteration (Picard), which on
    // this feedback loop needs many more sweeps to settle. Bound the iteration
    // count below what a tangent-free stamp needs.
    fn gentle_feedback_switch_circuit() -> (Circuit, NodeId) {
        let mut c = Circuit::new();
        let src = c.node("src");
        let out = c.node("out");
        let bias = c.node("bias");
        c.add(Device::Vsource {
            name: "V1".into(),
            p: src,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(5.0),
        });
        c.add(Device::Vsource {
            name: "VB".into(),
            p: bias,
            n: NodeId::GROUND,
            kind: SourceKind::Dc(4.0),
        });
        c.add(Device::VSwitch {
            name: "S1".into(),
            a: src,
            b: out,
            ctrl_p: bias, // vctrl = 4 - v(out): gentle negative feedback
            ctrl_n: out,
            von: 5.0, // wide transition (span = 5 V): smooth, non-stiff tanh
            voff: 0.0,
            ron: 100.0,
            roff: 1e4,
        });
        c.add(Device::Resistor {
            name: "RL".into(),
            a: out,
            b: NodeId::GROUND,
            ohms: 100.0,
            tc1: None,
        });
        (c, out)
    }

    #[test]
    fn control_jacobian_converges_in_few_iterations() {
        let (c, out) = gentle_feedback_switch_circuit();
        let opts = SolverOptions::default();
        let mut ws = Workspace::new(&c);
        let coeffs = IntegCoeffs::for_step(opts.integration, 1.0, true);
        let empty = ReactiveState::new(c.devices.len());
        // Cold start from zero (the default x), full sources, DC. Plain Newton,
        // no homotopy: this is the bare per-step behaviour the tangent improves.
        let r = newton_solve(
            &mut ws, &c, &opts, 0.0, 1.0, coeffs, &empty, true, false, opts.gmin, 1.0,
        );
        assert!(r.converged, "switch Newton should converge, iters={}", r.iters);
        assert!(
            r.iters <= 8,
            "the control Jacobian should converge the feedback switch quickly, took {} iters",
            r.iters
        );
        // And to a real interior operating point on the transition (not pinned to
        // either rail), with a near-zero KCL residual (a true root).
        let vout = ws.layout.node(out).map(|i| ws.x[i]).unwrap();
        assert!(
            (0.3..=2.5).contains(&vout),
            "gentle feedback switch should settle on the transition, got {vout}"
        );
        let res = ws.dc_residual_inf_norm(&c, &opts);
        assert!(res < 1e-7, "residual at the gentle-switch root should be ~0, got {res:e}");
    }
}

#[cfg(test)]
mod switch_freeze_tests {
    use super::{eval_switch_states, solve_relaxed_no_diodes, staged_event_solve, SpdtPairs, Workspace};
    use crate::options::SolverOptions;
    use hauksbee_ir::{Circuit, Device, DiodeModel, NodeId, SourceKind};
    use std::collections::HashMap;

    // eval_switch_states must classify each switch on/off from its control voltage
    // with hysteresis at the switch's own (von, voff) band.
    #[test]
    fn switch_states_track_control_with_hysteresis() {
        let mut c = Circuit::new();
        let ctrl = c.node("ctrl");
        let out = c.node("out");
        let id = c.add(Device::VSwitch {
            name: "S".into(),
            a: ctrl,
            b: out,
            ctrl_p: ctrl,
            ctrl_n: NodeId::GROUND,
            von: 2.0,
            voff: 1.0,
            ron: 1.0,
            roff: 1e6,
        });
        let layout = crate::system::Layout::new(&c);
        let ci = layout.node(ctrl).unwrap();
        let mut x = vec![0.0; layout.size];

        // Control above von -> ON regardless of prior state.
        x[ci] = 3.0;
        let s = eval_switch_states(&c, &layout, &x, &HashMap::new(), &SpdtPairs::empty());
        assert_eq!(s.get(&id), Some(&true), "vctrl=3 > von=2 should be ON");

        // Control inside the band holds the prior state (hysteresis).
        x[ci] = 1.5;
        let mut prev = HashMap::new();
        prev.insert(id, true);
        let held_on = eval_switch_states(&c, &layout, &x, &prev, &SpdtPairs::empty());
        assert_eq!(held_on.get(&id), Some(&true), "in-band should hold prior ON");
        prev.insert(id, false);
        let held_off = eval_switch_states(&c, &layout, &x, &prev, &SpdtPairs::empty());
        assert_eq!(held_off.get(&id), Some(&false), "in-band should hold prior OFF");

        // Control below voff -> OFF.
        x[ci] = 0.5;
        let s = eval_switch_states(&c, &layout, &x, &HashMap::new(), &SpdtPairs::empty());
        assert_eq!(s.get(&id), Some(&false), "vctrl=0.5 < voff=1 should be OFF");
    }

    // The event-freeze outer loop, driven directly, on a switch + diode core with
    // a SATURATED consistent root (the realistic Tarski case: the SN74LVC1G3157
    // switches are driven by digital control nodes pulled hard to a rail, so at
    // the true root every switch is fully ON or fully OFF, not mid-transition).
    // Freezing pins each switch to ron/roff per inner solve and re-derives the
    // state between solves; the loop reaches the consistent saturated fixed point.
    // This exercises the switch half of the Gauss-Seidel loop end-to-end and
    // confirms the returned vector is a self-consistent root.
    //
    // (The freeze is correct precisely when the root is saturated — pinning a
    // switch to a rail cannot represent a switch whose true solution is partial
    // conduction at its own knee; that case is handled by the smooth tanh path
    // with the new control tangent, not the freeze. The limit-cycle cure's
    // load-bearing proof on a real switch mesh is the Tarski board.)
    #[test]
    fn staged_event_solve_settles_switch_core() {
        let mut c = Circuit::new();
        let rail = c.node("RAIL");
        let ctrl_on = c.node("CON"); // pulled to the rail: switch saturated ON
        let ctrl_off = c.node("COFF"); // pulled to ground: switch saturated OFF
        let out_on = c.node("OUTON");
        let out_off = c.node("OUTOFF");
        let flt = c.node("FLT");
        c.add(Device::Vsource { name: "VR".into(), p: rail, n: NodeId::GROUND, kind: SourceKind::Dc(5.0) });
        // Control nodes pulled hard to definite rails (a 595 output, saturated).
        c.add(Device::Resistor { name: "Ron".into(), a: rail, b: ctrl_on, ohms: 100.0, tc1: None });
        c.add(Device::Resistor { name: "Roff".into(), a: ctrl_off, b: NodeId::GROUND, ohms: 100.0, tc1: None });
        // Switch driven ON: routes the rail to out_on.
        c.add(Device::VSwitch {
            name: "Son".into(), a: rail, b: out_on, ctrl_p: ctrl_on, ctrl_n: NodeId::GROUND,
            von: 2.5, voff: 1.5, ron: 1.0, roff: 1e6,
        });
        c.add(Device::Resistor { name: "RLon".into(), a: out_on, b: NodeId::GROUND, ohms: 1.0, tc1: None });
        // Switch driven OFF: leaves out_off near ground.
        c.add(Device::VSwitch {
            name: "Soff".into(), a: rail, b: out_off, ctrl_p: ctrl_off, ctrl_n: NodeId::GROUND,
            von: 2.5, voff: 1.5, ron: 1.0, roff: 1e6,
        });
        c.add(Device::Resistor { name: "RLoff".into(), a: out_off, b: NodeId::GROUND, ohms: 1e3, tc1: None });
        // Floating reverse-diode cap node, so the relaxed-seed staged machinery is
        // engaged (the diode pathology the staged path exists for).
        let model = DiodeModel { is: 4.352e-9, n: 1.9, rs: 0.65, ..DiodeModel::default() };
        c.add(Device::Diode { name: "Dr".into(), a: NodeId::GROUND, k: flt, model });
        c.add(Device::Capacitor { name: "Cf".into(), a: flt, b: NodeId::GROUND, farads: 5.8e-9, ic: None });

        let opts = SolverOptions::default();
        let mut ws = Workspace::new(&c);
        ws.symbolic.set_allow_dynamic(true);
        let seed = solve_relaxed_no_diodes(&c, &opts).expect("relaxed seed converges");

        let root = staged_event_solve(&mut ws, &c, &opts, &seed, 1e-9, 1e-2, false)
            .expect("event-freeze settles the saturated switch core to a consistent root");

        let v_on = root[ws.layout.node(out_on).unwrap()];
        let v_off = root[ws.layout.node(out_off).unwrap()];
        // ON switch: 5 V divided 1:1 -> ~2.5 V. OFF switch: leaks 5 V through 1e6
        // to a 1k load -> ~5 mV. The states are saturated and distinct.
        assert!((2.0..=2.5).contains(&v_on), "ON switch should conduct (~2.5 V), got {v_on}");
        assert!(v_off < 0.1, "OFF switch should block (~0 V), got {v_off}");

        // The returned vector is a consistent fixed point: re-deriving the switch
        // states from it produces no flip.
        let states = eval_switch_states(&c, &ws.layout, &root, &HashMap::new(), &SpdtPairs::empty());
        let states2 = eval_switch_states(&c, &ws.layout, &root, &states, &SpdtPairs::empty());
        assert_eq!(states, states2, "switch states at the root must be self-consistent");
    }
}
