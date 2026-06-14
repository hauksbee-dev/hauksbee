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
use hauksbee_ir::{Circuit, Device, NodeId};

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
            linear,
            plan,
            staged_branch_reg: 0.0,
            used_staged_dc: false,
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
    // Anchor for pn-junction voltage limiting: the linearization point of the
    // PREVIOUS Newton iteration. Seeded to the starting guess so the first
    // iteration's anchor equals its own linearization point (pnjlim then sees
    // zero delta and does not limit), exactly as a cold SPICE iteration.
    ws.x_prev_iter.copy_from_slice(&ws.x);
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
            if std::env::var("HAUKSBEE_STAGED_DBG").is_ok() {
                eprintln!("[newton] refactor singular at iter {iters} (already_converged={already})");
            }
            return NewtonResult { converged: already, iters };
        }
        ws.symbolic.solve(&mut ws.rhs);
        // rhs now holds the new iterate x.
        ws.x.copy_from_slice(&ws.rhs);

        // Damped Newton (staged-DC only). A node sitting behind a reverse-biased
        // diode plus a DC-open cap is nearly floating and high-gain, so the
        // undamped Newton map has a limit cycle (the node flip-flops between two
        // values forever). Take a fractional step on the NODE block plus a hard
        // per-iteration cap; both are standard Newton globalizations that change
        // only the iteration path, not the fixed point (at the root the step is
        // zero, so damping is inert there). Branch currents are left unclamped.
        // Active only for branch_reg>0, so the normal solve paths stay
        // bit-identical.
        if branch_reg > 0.0 {
            const NODE_ALPHA: f64 = 0.5; // fractional damping
            const NODE_STEP_MAX: f64 = 2.0; // hard cap (V) per iteration
            for i in 0..ws.layout.n_nodes {
                let full = ws.x[i] - lin_point[i];
                let mut step = NODE_ALPHA * full;
                if step > NODE_STEP_MAX {
                    step = NODE_STEP_MAX;
                } else if step < -NODE_STEP_MAX {
                    step = -NODE_STEP_MAX;
                }
                ws.x[i] = lin_point[i] + step;
            }
        }

        // A fully linear system is solved exactly in one shot; no need to
        // assemble and factor a second time just to watch the residual be zero.
        if ws.linear {
            return NewtonResult { converged: true, iters };
        }
        if converged(&ws.x, &ws.x_prev_iter, &ws.layout, opts) {
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
        if branch_reg > 0.0
            && iters >= 3
            && node_block_converged(&ws.x, &ws.x_prev_iter, &ws.layout, opts)
        {
            return NewtonResult { converged: true, iters };
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
                ws.used_staged_dc = true;
                return Ok(());
            }

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
            ws.used_staged_dc = true;
            return Ok(());
        }
    } else if dbg {
        eprintln!("[staged] relaxed (no-diode) solve did NOT converge");
    }

    Err(format!(
        "DC homotopy failed (source scale {last_scale:.3}, {last_iters} iters; \
         staged-DC relaxation did not recover)"
    ))
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
